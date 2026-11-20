//! Content-addressed file transfer over the mesh.
//!
//! A file is split into 1 MiB chunks, each named by its BLAKE3 hash. A signed
//! `blob.manifest` event lists the chunks and gives the blob its id (`root`): the BLAKE3 of
//! the chunk hashes in order. Big files keep their chunk list in an index blob so the event
//! stays small. A site fetches only blobs it wants, chunk by chunk, from any peer that has
//! them, checking every chunk against its hash; an interrupted transfer picks up where it
//! stopped. Apps get a finished blob out as a file on disk.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::event::Event;
use crate::log::Log;
use crate::util::now_ms;

pub const CHUNK: usize = 1 << 20;
/// Chunk lists longer than this (2 GiB of file) go into an index blob.
pub const MAX_INLINE: usize = 2048;
/// Index blob chunks, at most: 64 MiB of hashes covers a 2 TiB file.
pub const MAX_INDEX: usize = 64;
/// Largest blob, what a full index covers.
pub const MAX_SIZE: u64 = (MAX_INDEX * CHUNK / 32 * CHUNK) as u64;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS blobs (
    root     TEXT PRIMARY KEY,
    event_id TEXT NOT NULL,
    author   TEXT NOT NULL,
    name     TEXT NOT NULL,
    size     INTEGER NOT NULL,
    json     TEXT NOT NULL,
    wanted   INTEGER NOT NULL DEFAULT 0,
    complete INTEGER NOT NULL DEFAULT 0,
    priority INTEGER NOT NULL DEFAULT 5,
    added_ms INTEGER NOT NULL,
    -- unwanted on purpose: auto_want leaves it alone
    dropped  INTEGER NOT NULL DEFAULT 0,
    -- JSON list of peers to ask for chunks first, in order
    sources  TEXT NOT NULL DEFAULT '[]',
    -- 1: ask only those peers
    only_sources INTEGER NOT NULL DEFAULT 0
);";

/// Add columns that databases made by earlier builds lack.
pub fn migrate(conn: &Connection) -> Result<()> {
    for (col, def) in [
        ("dropped", "INTEGER NOT NULL DEFAULT 0"),
        ("sources", "TEXT NOT NULL DEFAULT '[]'"),
        ("only_sources", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        let has: bool = conn.query_row(
            "SELECT count(*) > 0 FROM pragma_table_info('blobs') WHERE name = ?1",
            params![col],
            |r| r.get(0),
        )?;
        if !has {
            conn.execute_batch(&format!("ALTER TABLE blobs ADD COLUMN {col} {def}"))?;
        }
    }
    Ok(())
}

/// Preferred peers kept per blob, at most.
pub const MAX_SOURCES: usize = 16;

/// `u` as a peer's base URL in one canonical spelling ("http://host:port", host lower
/// case, no default port), or None if it is anything else: other schemes, a path, a
/// query, a fragment or a user name.
pub fn peer_url(u: &str) -> Option<String> {
    let u = u.trim();
    if u.len() >= 200 {
        return None;
    }
    let p = reqwest::Url::parse(u).ok()?;
    let plain = p.scheme() == "http"
        && p.username().is_empty()
        && p.password().is_none()
        && matches!(p.path(), "" | "/")
        && p.query().is_none()
        && p.fragment().is_none();
    let host = p.host_str().filter(|h| plain && !h.is_empty())?;
    Some(match p.port() {
        Some(port) => format!("http://{host}:{port}"),
        None => format!("http://{host}"),
    })
}

/// An app's list of preferred peers, checked, in canonical form and without repeats.
pub fn sources_from(peers: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = vec![];
    for p in peers {
        let Some(u) = peer_url(p) else {
            bail!("{p:?} is not a peer address like http://10.1.2.3:8790");
        };
        if !out.contains(&u) {
            out.push(u);
        }
    }
    ensure!(
        out.len() <= MAX_SOURCES,
        "at most {MAX_SOURCES} preferred peers, not {}",
        out.len()
    );
    Ok(out)
}

/// How fast blobs may move: overall, and slower over links that need it.
#[derive(Debug, Clone, PartialEq)]
pub struct Pace {
    pub bytes_per_s: usize,
    /// by peer base URL
    pub per_peer: std::collections::BTreeMap<String, usize>,
}

impl Pace {
    pub fn new(cfg: &crate::config::BlobsCfg) -> Self {
        let bytes = |kbps: u64| (kbps * 1000 / 8).max(1) as usize;
        Self {
            bytes_per_s: bytes(cfg.rate_kbps),
            per_peer: cfg
                .peer_rate_kbps
                .iter()
                .filter_map(|(u, k)| Some((peer_url(u)?, bytes(*k))))
                .collect(),
        }
    }

    /// The rate for chunks from `peer`: its own cap if it has a lower one.
    pub fn for_peer(&self, peer: &str) -> usize {
        self.per_peer
            .get(peer)
            .map_or(self.bytes_per_s, |r| (*r).min(self.bytes_per_s))
            .max(1)
    }
}

/// Blobs fetch in priority order, 0 first; most files are 5.
pub const DEFAULT_PRIORITY: u8 = 5;
pub const MAX_PRIORITY: u8 = 9;

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub root: String,
    pub name: String,
    pub size: u64,
    /// chunk hashes, when the list fits in the event
    pub chunks: Option<Vec<String>>,
    /// chunk hashes of the index blob (32 raw bytes per chunk hash), for big files
    pub index: Option<Vec<String>>,
    /// 0 fetches first; an urgent message's attachment goes before bulk content
    pub priority: u8,
}

fn priority_of(p: &Value) -> u8 {
    p.get("priority")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_PRIORITY, |v| v.min(MAX_PRIORITY as u64) as u8)
}

fn is_hash(h: &str) -> bool {
    crate::invite::valid_key(h)
}

fn hashes(v: Option<&Value>) -> Option<Vec<String>> {
    let a = v?.as_array()?;
    let out: Vec<String> = a
        .iter()
        .filter_map(|h| h.as_str().map(str::to_string))
        .collect();
    (out.len() == a.len() && out.iter().all(|h| is_hash(h))).then_some(out)
}

pub fn root_of(chunks: &[String]) -> Result<String> {
    let mut h = blake3::Hasher::new();
    for c in chunks {
        h.update(&hex::decode(c)?);
    }
    Ok(h.finalize().to_hex().to_string())
}

pub fn parse_manifest(e: &Event) -> Option<Manifest> {
    if e.kind != "blob.manifest" || e.log.is_some() {
        return None;
    }
    let p = &e.payload;
    let m = Manifest {
        root: p.get("root")?.as_str()?.to_string(),
        name: p.get("name")?.as_str()?.chars().take(200).collect(),
        size: p.get("size")?.as_u64()?,
        chunks: hashes(p.get("chunks")),
        index: hashes(p.get("index")),
        priority: priority_of(p),
    };
    let n = m.size.div_ceil(CHUNK as u64) as usize;
    // every chunk hash takes 32 bytes of index
    let index_len = (n * 32).div_ceil(CHUNK);
    let ok = is_hash(&m.root)
        && m.size <= MAX_SIZE
        && match (&m.chunks, &m.index) {
            (Some(c), None) => c.len() == n && c.len() <= MAX_INLINE && root_of(c).ok()? == m.root,
            (None, Some(i)) => i.len() == index_len && n > MAX_INLINE,
            _ => false,
        };
    ok.then_some(m)
}

/// Record a manifest that arrived in the log (wanted or not).
pub fn note_manifest(conn: &Connection, e: &Event) -> Result<()> {
    if let Some(m) = parse_manifest(e) {
        conn.execute(
            "INSERT OR IGNORE INTO blobs (root, event_id, author, name, size, json, priority, added_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                m.root,
                e.id,
                e.author.key,
                m.name,
                m.size as i64,
                e.payload.to_string(),
                m.priority,
                now_ms()
            ],
        )?;
    }
    Ok(())
}

/// Chunk files on disk.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, hash: &str) -> PathBuf {
        self.dir.join(&hash[..2]).join(hash)
    }

    pub fn has(&self, hash: &str) -> bool {
        is_hash(hash) && self.path(hash).exists()
    }

    /// Store a chunk if it really has this hash.
    pub fn put(&self, hash: &str, data: &[u8]) -> Result<()> {
        ensure!(is_hash(hash), "not a chunk hash");
        ensure!(data.len() <= CHUNK, "chunk too large");
        ensure!(
            blake3::hash(data).to_hex().as_str() == hash,
            "chunk does not match its hash"
        );
        let p = self.path(hash);
        if p.exists() {
            return Ok(());
        }
        fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("part");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        // on disk before the name says it's complete, or a power cut leaves a torn chunk
        f.sync_all()?;
        fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// Delete a chunk; returns the bytes freed.
    fn remove(&self, hash: &str) -> Result<u64> {
        if !self.has(hash) {
            return Ok(0);
        }
        let p = self.path(hash);
        let len = fs::metadata(&p)?.len();
        fs::remove_file(&p)?;
        Ok(len)
    }

    /// Chunk files and their total size. Files that vanish mid-count (freed by unwant, or
    /// the folder cleared by hand) just don't count.
    pub fn disk_use(&self) -> Result<(u64, u64)> {
        let (mut files, mut bytes) = (0, 0);
        let subs = match fs::read_dir(&self.dir) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
            Err(e) => return Err(e.into()),
        };
        for sub in subs.flatten() {
            let Ok(chunks) = fs::read_dir(sub.path()) else {
                continue;
            };
            for f in chunks.flatten() {
                if !is_hash(&f.file_name().to_string_lossy()) {
                    continue;
                }
                if let Ok(m) = f.metadata() {
                    files += 1;
                    bytes += m.len();
                }
            }
        }
        Ok((files, bytes))
    }

    pub fn get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        if !self.has(hash) {
            return Ok(None);
        }
        Ok(Some(fs::read(self.path(hash))?))
    }

    /// The full chunk list of a blob, if we have what's needed to know it.
    pub fn chunk_list(&self, m: &Manifest) -> Result<Option<Vec<String>>> {
        if let Some(c) = &m.chunks {
            return Ok(Some(c.clone()));
        }
        let mut raw = vec![];
        for h in m.index.as_deref().unwrap_or_default() {
            match self.get(h)? {
                Some(d) => raw.extend(d),
                None => return Ok(None),
            }
        }
        ensure!(raw.len() % 32 == 0, "index blob is not a list of hashes");
        let list: Vec<String> = raw.chunks(32).map(hex::encode).collect();
        ensure!(
            root_of(&list)? == m.root,
            "index blob does not match the blob id"
        );
        ensure!(
            list.len() as u64 == m.size.div_ceil(CHUNK as u64),
            "index blob has the wrong length"
        );
        Ok(Some(list))
    }

    /// Chunks still needed for this blob: the index first, then the content.
    pub fn missing(&self, m: &Manifest) -> Result<Vec<String>> {
        let index_missing: Vec<String> = m
            .index
            .iter()
            .flatten()
            .filter(|h| !self.has(h))
            .cloned()
            .collect();
        if !index_missing.is_empty() {
            return Ok(index_missing);
        }
        let list = self.chunk_list(m)?.context("index blob unavailable")?;
        let mut seen = std::collections::HashSet::new();
        Ok(list
            .into_iter()
            .filter(|h| !self.has(h) && seen.insert(h.clone()))
            .collect())
    }

    /// Write a complete blob out to `dest` (atomically).
    pub fn export(&self, m: &Manifest, dest: &Path) -> Result<()> {
        let list = self.chunk_list(m)?.context("blob is not complete")?;
        let tmp = dest.with_extension("part");
        // apps can write in the export folder, so a link planted at the temp name must
        // never be followed: remove whatever is there and insist on a new file
        let _ = fs::remove_file(&tmp);
        let mut f = new_file(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        for h in &list {
            let d = self.get(h)?.with_context(|| format!("missing chunk {h}"))?;
            if blake3::hash(&d).to_hex().as_str() != h {
                // damaged on disk: drop it, and `recheck` sends the blob back to fetching
                let _ = fs::remove_file(self.path(h));
                bail!("chunk {h} was damaged on disk and has been removed");
            }
            f.write_all(&d)?;
        }
        f.sync_all()?;
        drop(f);
        ensure!(
            fs::metadata(&tmp)?.len() == m.size,
            "exported size does not match the manifest"
        );
        fs::rename(&tmp, dest)?;
        Ok(())
    }
}

/// A new file readable by the agent's group (apps), never an existing one or a link.
fn new_file(p: &Path) -> std::io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o640);
    o.open(p)
}

/// Open a file to publish, refusing anything but a plain file. Apps share the import
/// folder with the agent, so a link there could point at the station key.
fn open_plain(path: &Path) -> Result<fs::File> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        meta.file_type().is_file(),
        "{} is not a plain file (links are refused)",
        path.display()
    );
    let f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    // and it must still be that file: swapped for a link between the check and the open
    // would mean opening something else
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = f.metadata()?;
        ensure!(
            opened.dev() == meta.dev() && opened.ino() == meta.ino(),
            "{} changed while it was being opened",
            path.display()
        );
    }
    Ok(f)
}

/// Chunk a local file into the store and publish its manifest. Returns the blob id.
pub fn add_file(log: &Log, store: &Store, path: &Path, name: &str, priority: u8) -> Result<String> {
    let mut f = open_plain(path)?;
    let mut chunks = vec![];
    let mut size = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let mut n = 0;
        while n < CHUNK {
            let k = f.read(&mut buf[n..])?;
            if k == 0 {
                break;
            }
            n += k;
        }
        if n == 0 {
            break;
        }
        let h = blake3::hash(&buf[..n]).to_hex().to_string();
        store.put(&h, &buf[..n])?;
        chunks.push(h);
        size += n as u64;
        if n < CHUNK {
            break;
        }
    }
    if chunks.is_empty() {
        bail!("{} is empty", path.display());
    }
    let root = root_of(&chunks)?;
    let mut payload = json!({"root": root, "name": name, "size": size});
    if priority != DEFAULT_PRIORITY {
        payload["priority"] = json!(priority.min(MAX_PRIORITY));
    }
    if chunks.len() <= MAX_INLINE {
        payload["chunks"] = json!(chunks);
    } else {
        let mut index = vec![];
        for part in index_parts(&chunks) {
            let h = blake3::hash(&part).to_hex().to_string();
            store.put(&h, &part)?;
            index.push(h);
        }
        payload["index"] = json!(index);
    }
    let ev = log.append("blob.manifest", payload)?;
    {
        let c = log.store.conn();
        note_manifest(&c, &ev)?;
        c.execute(
            "UPDATE blobs SET wanted = 1, dropped = 0 WHERE root = ?1",
            params![root],
        )?;
    }
    // an unwant of a blob sharing chunks with this one may have freed some while the row
    // didn't list them yet; now that it does, put back whatever went
    for part in index_parts(&chunks) {
        let h = blake3::hash(&part).to_hex().to_string();
        if !store.has(&h) {
            store.put(&h, &part)?;
        }
    }
    for (i, h) in chunks.iter().enumerate() {
        if !store.has(h) {
            f.seek(SeekFrom::Start((i * CHUNK) as u64))?;
            let mut part = Vec::with_capacity(CHUNK);
            (&mut f).take(CHUNK as u64).read_to_end(&mut part)?;
            store.put(h, &part)?;
        }
    }
    log.store.conn().execute(
        "UPDATE blobs SET complete = 1 WHERE root = ?1 AND wanted = 1",
        params![root],
    )?;
    Ok(root)
}

/// A big blob's chunk list as raw hashes, split into index chunks; empty when the list
/// fits in the manifest.
fn index_parts(chunks: &[String]) -> Vec<Vec<u8>> {
    if chunks.len() <= MAX_INLINE {
        return vec![];
    }
    let raw: Vec<u8> = chunks
        .iter()
        .flat_map(|h| hex::decode(h).unwrap_or_default())
        .collect();
    raw.chunks(CHUNK).map(<[u8]>::to_vec).collect()
}

pub fn manifest(conn: &Connection, root: &str) -> Result<Option<Manifest>> {
    let row: Option<(String, String, i64, String)> = conn
        .query_row(
            "SELECT root, name, size, json FROM blobs WHERE root = ?1",
            params![root],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    Ok(row.map(|(root, name, size, j)| {
        let p: Value = serde_json::from_str(&j).unwrap_or(Value::Null);
        Manifest {
            root,
            name,
            size: size as u64,
            chunks: hashes(p.get("chunks")),
            index: hashes(p.get("index")),
            priority: priority_of(&p),
        }
    }))
}

/// Want a blob and ask these peers for its chunks first, in order.
/// With `only`, no other peer is asked: a chunk they don't have waits for a later round.
/// An empty list clears the preference. `sources` comes from `sources_from`.
pub fn want_from(conn: &Connection, root: &str, sources: &[String], only: bool) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE blobs SET wanted = 1, dropped = 0, sources = ?2, only_sources = ?3 WHERE root = ?1",
        params![
            root,
            serde_json::to_string(sources)?,
            only && !sources.is_empty()
        ],
    )? > 0)
}

/// A blob's preferred peers, and whether they are the only ones to ask.
pub fn sources(conn: &Connection, root: &str) -> Result<(Vec<String>, bool)> {
    let row: Option<(String, bool)> = conn
        .query_row(
            "SELECT sources, only_sources FROM blobs WHERE root = ?1",
            params![root],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((j, only)) = row else {
        return Ok((vec![], false));
    };
    match serde_json::from_str(&j) {
        Ok(list) => Ok((list, only)),
        Err(e) => {
            tracing::warn!("blob {root}: preferred peers unreadable ({e}); asking any peer");
            Ok((vec![], false))
        }
    }
}

pub fn want(conn: &Connection, root: &str, yes: bool) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE blobs SET wanted = ?2, dropped = 0 WHERE root = ?1",
        params![root, yes],
    )? > 0)
}

/// Fetch `root` by a trusted station's manifest when one exists, whichever manifest
/// arrived first: a stranger's copy could carry a bad index or size.
pub fn prefer_trusted(
    conn: &Connection,
    root: &str,
    trusted: &std::collections::BTreeSet<String>,
) -> Result<()> {
    let rows: Vec<String> = conn
        .prepare(
            "SELECT json FROM events WHERE kind = 'blob.manifest'
             AND json_extract(json, '$.payload.root') = ?1",
        )?
        .query_map(params![root], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for j in rows {
        let e: Event = serde_json::from_str(&j)?;
        if trusted.contains(&e.author.key)
            && let Some(m) = parse_manifest(&e)
        {
            take_manifest(conn, &e, &m)?;
            break;
        }
    }
    Ok(())
}

fn take_manifest(conn: &Connection, e: &Event, m: &Manifest) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE blobs SET event_id = ?2, author = ?3, name = ?4, size = ?5, json = ?6, priority = ?7
         WHERE root = ?1 AND complete = 0",
        params![
            m.root,
            e.id,
            e.author.key,
            m.name,
            m.size as i64,
            e.payload.to_string(),
            m.priority
        ],
    )?)
}

/// Stop wanting a blob and delete its chunks, keeping any chunk that another blob still
/// wanted or held lists (chunks are shared by content). Returns the bytes freed, or None
/// if there is no manifest for `root`.
pub fn unwant(conn: &Connection, store: &Store, root: &str) -> Result<Option<u64>> {
    let Some(m) = manifest(conn, root)? else {
        return Ok(None);
    };
    // read the chunk list before anything goes: for a big blob it lives in chunks too.
    // If its index is damaged only the index can go; that must not block unwanting it.
    let mut mine: Vec<String> = m.index.clone().unwrap_or_default();
    mine.extend(store.chunk_list(&m).ok().flatten().unwrap_or_default());
    conn.execute(
        "UPDATE blobs SET wanted = 0, complete = 0, dropped = 1, sources = '[]', only_sources = 0
         WHERE root = ?1",
        params![root],
    )?;
    let others: Vec<String> = conn
        .prepare("SELECT root FROM blobs WHERE (wanted = 1 OR complete = 1) AND root != ?1")?
        .query_map(params![root], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut keep = std::collections::HashSet::new();
    for r in others {
        let Some(o) = manifest(conn, &r)? else {
            continue;
        };
        keep.extend(o.index.iter().flatten().cloned());
        // a held blob whose list we can't read might share any chunk: free nothing
        match store.chunk_list(&o) {
            Ok(Some(list)) => keep.extend(list),
            Ok(None) if !held(conn, &r)? => {}
            Ok(None) => bail!(
                "blob {} has lost its index chunks; not freeing anything",
                o.name
            ),
            Err(e) => bail!(
                "reading blob {}'s chunk list: {e:#}; not freeing anything",
                o.name
            ),
        }
    }
    let mut freed = 0;
    for h in mine.iter().filter(|h| !keep.contains(*h)) {
        freed += store.remove(h)?;
    }
    Ok(Some(freed))
}

fn held(conn: &Connection, root: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT complete FROM blobs WHERE root = ?1",
        params![root],
        |r| r.get(0),
    )?)
}

/// After a failed export: if chunks are missing (a damaged one was just removed), mark the
/// blob incomplete so the next fetch round gets them again.
pub fn recheck(conn: &Connection, store: &Store, m: &Manifest) -> Result<()> {
    if !store.missing(m)?.is_empty() {
        conn.execute(
            "UPDATE blobs SET complete = 0 WHERE root = ?1",
            params![m.root],
        )?;
    }
    Ok(())
}

/// What a site knows about one blob, for apps waiting on it.
pub fn status(
    conn: &Connection,
    root: &str,
    trusted: &std::collections::BTreeSet<String>,
) -> Result<Option<Value>> {
    let row = conn
        .query_row(
            "SELECT root, name, size, complete, wanted, priority, author FROM blobs WHERE root = ?1",
            params![root],
            |r| {
                Ok(json!({"root": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                    "size": r.get::<_, i64>(2)?, "complete": r.get::<_, bool>(3)?,
                    "wanted": r.get::<_, bool>(4)?, "priority": r.get::<_, i64>(5)?,
                    "author": r.get::<_, String>(6)?}))
            },
        )
        .optional()?;
    let Some(mut v) = row else {
        return Ok(None);
    };
    let authors: Vec<String> = conn
        .prepare(
            "SELECT author_key FROM events WHERE kind = 'blob.manifest'
             AND json_extract(json, '$.payload.root') = ?1",
        )?
        .query_map(params![root], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    v["trusted"] = json!(authors.iter().any(|a| trusted.contains(a)));
    Ok(Some(v))
}

/// Mark as wanted any blob a trusted station published under a name starting with one of
/// `prefixes`. Returns how many were newly wanted. Looks at every manifest for a blob,
/// not just the first to arrive, so a stranger publishing the same file first can't hide
/// a trusted release.
pub fn auto_want(
    conn: &Connection,
    trusted: &std::collections::BTreeSet<String>,
    prefixes: &[String],
) -> Result<usize> {
    if prefixes.is_empty() {
        return Ok(0);
    }
    let revoked: std::collections::BTreeMap<String, i64> = conn
        .prepare("SELECT author_key, after_seq FROM revoked")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let manifests: Vec<(String, i64, String)> = conn
        .prepare("SELECT author_key, seq, json FROM events WHERE kind = 'blob.manifest'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut n = 0;
    for (author, seq, j) in manifests {
        if !trusted.contains(&author) || revoked.get(&author).is_some_and(|after| seq > *after) {
            continue;
        }
        let e: Event = serde_json::from_str(&j)?;
        let Some(m) = parse_manifest(&e) else {
            continue;
        };
        if !prefixes.iter().any(|p| m.name.starts_with(p.as_str())) {
            continue;
        }
        let fresh = conn.execute(
            "UPDATE blobs SET wanted = 1 WHERE root = ?1 AND wanted = 0 AND dropped = 0",
            params![m.root],
        )?;
        if fresh > 0 {
            // fetch by the trusted manifest, whichever arrived first
            take_manifest(conn, &e, &m)?;
            n += fresh;
        }
    }
    Ok(n)
}

/// Blobs this site wants but doesn't have yet.
pub fn pending(conn: &Connection) -> Result<Vec<Manifest>> {
    let roots: Vec<String> = conn
        .prepare(
            "SELECT root FROM blobs WHERE wanted = 1 AND complete = 0 ORDER BY priority, added_ms",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut out = vec![];
    for r in roots {
        if let Some(m) = manifest(conn, &r)? {
            out.push(m);
        }
    }
    Ok(out)
}

fn still_wanted(conn: &Connection, root: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT wanted FROM blobs WHERE root = ?1",
            params![root],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// Fetch one chunk from a peer, checking it on arrival. None if the peer doesn't have it.
pub async fn fetch_chunk(
    http: &reqwest::Client,
    base: &str,
    hash: &str,
) -> Result<Option<Vec<u8>>> {
    let mut resp = http
        .get(format!("{}/v1/chunk/{hash}", base.trim_end_matches('/')))
        .send()
        .await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    resp = resp.error_for_status()?;
    let mut body = Vec::with_capacity(CHUNK);
    while let Some(part) = resp.chunk().await? {
        body.extend_from_slice(&part);
        ensure!(body.len() <= CHUNK, "peer sent more than a chunk");
    }
    ensure!(
        blake3::hash(&body).to_hex().as_str() == hash,
        "peer sent a chunk that doesn't match its hash"
    );
    Ok(Some(body))
}

/// Is `hour` (UTC) inside a "start-end" window like "1-6" (wrapping past midnight allowed)?
pub fn in_window(window: Option<(u32, u32)>, hour: u32) -> bool {
    match window {
        None => true,
        Some((a, b)) if a <= b => (a..b).contains(&hour),
        Some((a, b)) => hour >= a || hour < b,
    }
}

/// The order to ask peers for a chunk: the blob's preferred sources as given, then (unless
/// `only`) the other peers starting after the one the last chunk came from. Peers that
/// failed this round are left out until the next one.
fn ask_order(
    sources: &[String],
    only: bool,
    peers: &[String],
    next: usize,
    down: &HashSet<String>,
) -> Vec<String> {
    let mut order: Vec<String> = vec![];
    for s in sources {
        if !down.contains(s) && !order.contains(s) {
            order.push(s.clone());
        }
    }
    if only {
        return order;
    }
    for i in 0..peers.len() {
        let p = &peers[(next + i) % peers.len()];
        if !down.contains(p) && !order.contains(p) {
            order.push(p.clone());
        }
    }
    order
}

/// Longest a fetch round runs; one chunk's pause can carry it a little past this.
pub const ROUND: Duration = Duration::from_secs(60);

/// Fetch pending blobs, at most `budget` bytes and about `ROUND` long, pacing each chunk to
/// its peer's rate. Returns bytes fetched.
///
/// A peer that doesn't have a chunk (404) is passed over for that chunk. One that fails
/// (no connection within 20 s, no whole chunk within 5 minutes, an error status, or data
/// that doesn't match the hash) is not asked again until the next round.
pub async fn fetch_round(
    log: &Log,
    store: &Store,
    http: &reqwest::Client,
    peers: &[String],
    budget: usize,
    pace: &Pace,
) -> Result<usize> {
    let pending = pending(&log.store.conn())?;
    let mut fetched = 0;
    let mut next_peer = 0;
    let mut down = HashSet::new();
    let started = std::time::Instant::now();
    for m in pending {
        let (preferred, only) = sources(&log.store.conn(), &m.root)?;
        loop {
            // unwanted since this round started: stop, or its chunks would come back
            if !still_wanted(&log.store.conn(), &m.root)? {
                break;
            }
            // one broken blob must not stop the others from fetching
            let missing = match store.missing(&m) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("blob {} ({}): {e:#}", &m.root[..12], m.name);
                    break;
                }
            };
            if missing.is_empty() {
                log.store.conn().execute(
                    "UPDATE blobs SET complete = 1 WHERE root = ?1 AND wanted = 1",
                    params![m.root],
                )?;
                tracing::info!("blob {} ({}) complete", &m.root[..12], m.name);
                break;
            }
            let mut progressed = false;
            for h in missing.iter().take(64) {
                if fetched >= budget || started.elapsed() >= ROUND {
                    return Ok(fetched);
                }
                for peer in ask_order(&preferred, only, peers, next_peer, &down) {
                    let asked = std::time::Instant::now();
                    match fetch_chunk(http, &peer, h).await {
                        Ok(Some(data)) => {
                            {
                                // checked under the lock unwant holds while it deletes
                                let c = log.store.conn();
                                if !still_wanted(&c, &m.root)? {
                                    return Ok(fetched);
                                }
                                store.put(h, &data)?;
                            }
                            fetched += data.len();
                            progressed = true;
                            if let Some(i) = peers.iter().position(|p| *p == peer) {
                                next_peer = (i + 1) % peers.len();
                            }
                            // the transfer itself counts toward the chunk's time at this rate
                            let at_rate = Duration::from_secs_f64(
                                data.len() as f64 / pace.for_peer(&peer) as f64,
                            );
                            tokio::time::sleep(at_rate.saturating_sub(asked.elapsed())).await;
                            break;
                        }
                        Ok(None) => continue,
                        Err(e) => {
                            if preferred.contains(&peer) {
                                tracing::info!(
                                    "preferred peer {peer} failed for blob {}, skipping it until the next round: {e:#}",
                                    m.name
                                );
                            } else {
                                tracing::debug!("chunk {} from {peer}: {e:#}", &h[..12]);
                            }
                            down.insert(peer);
                        }
                    }
                }
            }
            if !progressed {
                break; // nobody has the next chunks right now; try again next round
            }
        }
    }
    Ok(fetched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn log() -> Log {
        Log::new(
            crate::store::Store::open_in_memory().unwrap(),
            Identity::from_seed("N0AAA", [1; 32]),
        )
        .unwrap()
    }

    fn file(dir: &Path, name: &str, len: usize) -> PathBuf {
        let p = dir.join(name);
        let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn add_then_export_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let l = log();
        let s = Store::new(d.path().join("blobs")).unwrap();
        let src = file(d.path(), "a.bin", CHUNK * 2 + 123);
        let root = add_file(&l, &s, &src, "a.bin", DEFAULT_PRIORITY).unwrap();
        let m = manifest(&l.store.conn(), &root).unwrap().unwrap();
        assert_eq!(m.chunks.as_ref().unwrap().len(), 3);
        assert!(s.missing(&m).unwrap().is_empty());
        let out = d.path().join("out.bin");
        s.export(&m, &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), fs::read(&src).unwrap());
    }

    #[test]
    fn chunks_must_match_their_hash() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path().to_path_buf()).unwrap();
        let h = blake3::hash(b"hello").to_hex().to_string();
        assert!(s.put(&h, b"hellO").is_err());
        s.put(&h, b"hello").unwrap();
        assert_eq!(s.get(&h).unwrap().unwrap(), b"hello");
    }

    #[test]
    fn manifests_are_checked() {
        let l = log();
        let good = vec![blake3::hash(b"x").to_hex().to_string()];
        let root = root_of(&good).unwrap();
        let ev = l
            .append(
                "blob.manifest",
                json!({"root": root, "name": "x", "size": 1, "chunks": good}),
            )
            .unwrap();
        assert!(parse_manifest(&ev).is_some());
        let ev = l
            .append(
                "blob.manifest",
                json!({"root": "aa".repeat(32), "name": "x", "size": 1, "chunks": good}),
            )
            .unwrap();
        assert!(
            parse_manifest(&ev).is_none(),
            "root must match the chunk list"
        );
        let ev = l
            .append(
                "blob.manifest",
                json!({"root": root, "name": "x", "size": 5_000_000, "chunks": good}),
            )
            .unwrap();
        assert!(
            parse_manifest(&ev).is_none(),
            "size must match the chunk count"
        );
    }

    #[test]
    fn big_files_use_an_index_blob() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path().join("blobs")).unwrap();
        // fake a 3000-chunk file without writing 3 GB: build the index by hand
        let chunks: Vec<String> = (0..3000u32)
            .map(|i| blake3::hash(&i.to_le_bytes()).to_hex().to_string())
            .collect();
        let raw: Vec<u8> = chunks
            .iter()
            .flat_map(|h| hex::decode(h).unwrap())
            .collect();
        let index: Vec<String> = raw
            .chunks(CHUNK)
            .map(|p| {
                let h = blake3::hash(p).to_hex().to_string();
                s.put(&h, p).unwrap();
                h
            })
            .collect();
        let m = Manifest {
            root: root_of(&chunks).unwrap(),
            name: "big".into(),
            size: 3000 * CHUNK as u64,
            chunks: None,
            index: Some(index),
            priority: DEFAULT_PRIORITY,
        };
        assert_eq!(s.chunk_list(&m).unwrap().unwrap(), chunks);
        assert_eq!(s.missing(&m).unwrap().len(), 3000);
    }

    #[test]
    fn unwant_frees_only_chunks_nothing_else_lists() {
        let d = tempfile::tempdir().unwrap();
        let l = log();
        let s = Store::new(d.path().join("blobs")).unwrap();
        // b starts with the same chunk as a, so that chunk is shared
        let a_src = file(d.path(), "a.bin", CHUNK * 2 + 10);
        let mut b_data = fs::read(&a_src).unwrap()[..CHUNK].to_vec();
        b_data.extend(b"different tail");
        let b_src = d.path().join("b.bin");
        fs::write(&b_src, &b_data).unwrap();
        let a = add_file(&l, &s, &a_src, "a.bin", DEFAULT_PRIORITY).unwrap();
        let b = add_file(&l, &s, &b_src, "b.bin", DEFAULT_PRIORITY).unwrap();
        let conn = l.store.conn();
        let ma = manifest(&conn, &a).unwrap().unwrap();
        let shared = ma.chunks.as_ref().unwrap()[0].clone();
        assert_eq!(s.disk_use().unwrap().0, 4);

        want_from(&conn, &a, &["http://10.1.2.3:8790".into()], true).unwrap();
        let freed = unwant(&conn, &s, &a).unwrap().unwrap();
        assert_eq!(
            sources(&conn, &a).unwrap(),
            (vec![], false),
            "unwant forgets the peers"
        );
        assert_eq!(
            freed,
            (CHUNK + 10) as u64,
            "a's own two chunks, not the shared one"
        );
        assert!(s.has(&shared));
        assert!(!s.has(&ma.chunks.as_ref().unwrap()[2]));
        let out = d.path().join("b.out");
        s.export(&manifest(&conn, &b).unwrap().unwrap(), &out)
            .unwrap();
        assert_eq!(fs::read(out).unwrap(), b_data);
        let st = status(&conn, &a, &[l.id.author.key.clone()].into())
            .unwrap()
            .unwrap();
        assert_eq!(
            (st["wanted"].as_bool(), st["complete"].as_bool()),
            (Some(false), Some(false))
        );
        assert_eq!(st["trusted"], json!(true));

        // dropping b too takes the shared chunk with it
        unwant(&conn, &s, &b).unwrap().unwrap();
        assert_eq!(s.disk_use().unwrap(), (0, 0));
        assert!(unwant(&conn, &s, &"ab".repeat(32)).unwrap().is_none());
        // an unwanted blob stays unwanted even if its name matches auto_want
        let trusted = [l.id.author.key.clone()].into();
        assert_eq!(auto_want(&conn, &trusted, &["a.".into()]).unwrap(), 0);
        assert!(want(&conn, &a, true).unwrap());
        assert!(pending(&conn).unwrap().iter().any(|m| m.root == a));
    }

    #[test]
    fn a_strangers_copy_cannot_hide_a_trusted_release() {
        let d = tempfile::tempdir().unwrap();
        let ours = log();
        let stranger = Log::new(
            crate::store::Store::open_in_memory().unwrap(),
            Identity::from_seed("N0BAD", [9; 32]),
        )
        .unwrap();
        let site = Log::new(
            crate::store::Store::open_in_memory().unwrap(),
            Identity::from_seed("N0CCC", [3; 32]),
        )
        .unwrap();
        let src = file(d.path(), "rel.tar.gz", 5000);
        let s1 = Store::new(d.path().join("s1")).unwrap();
        let s2 = Store::new(d.path().join("s2")).unwrap();
        add_file(&stranger, &s1, &src, "junk", 0).unwrap();
        let root = add_file(&ours, &s2, &src, "ridgewarden-release/v9", 5).unwrap();
        // the stranger's manifest for the same file reaches the site first
        for l in [&stranger, &ours] {
            site.ingest(&l.events_after(&l.id.author.key, -1, None, 100).unwrap())
                .unwrap();
        }
        let conn = site.store.conn();
        let prefix = ["ridgewarden-release/".to_string()];
        let only_stranger = [stranger.id.author.key.clone()].into();
        assert_eq!(auto_want(&conn, &only_stranger, &prefix).unwrap(), 0);
        let trusted = [ours.id.author.key.clone()].into();
        assert_eq!(auto_want(&conn, &trusted, &prefix).unwrap(), 1);
        let st = status(&conn, &root, &trusted).unwrap().unwrap();
        assert_eq!(st["author"], json!(ours.id.author.key));
        assert_eq!(st["name"], json!("ridgewarden-release/v9"));
        assert_eq!(st["wanted"], json!(true));

        // an app wanting it by hand also gets the trusted manifest
        let other = Log::new(
            crate::store::Store::open_in_memory().unwrap(),
            Identity::from_seed("N0DDD", [4; 32]),
        )
        .unwrap();
        for l in [&stranger, &ours] {
            other
                .ingest(&l.events_after(&l.id.author.key, -1, None, 100).unwrap())
                .unwrap();
        }
        let c2 = other.store.conn();
        prefer_trusted(&c2, &root, &trusted).unwrap();
        assert!(want(&c2, &root, true).unwrap());
        let st = status(&c2, &root, &trusted).unwrap().unwrap();
        assert_eq!(st["author"], json!(ours.id.author.key));
    }

    #[test]
    fn manifests_with_impossible_sizes_are_ignored() {
        let l = log();
        let index = vec![blake3::hash(b"i").to_hex().to_string()];
        for size in [
            MAX_SIZE + 1,
            u64::MAX,
            (MAX_INLINE as u64 + 1) * CHUNK as u64 * 1000,
        ] {
            let ev = l
                .append(
                    "blob.manifest",
                    json!({"root": "aa".repeat(32), "name": "x", "size": size, "index": index}),
                )
                .unwrap();
            assert!(parse_manifest(&ev).is_none(), "{size}");
        }
        // a 3000-chunk file needs exactly one index chunk
        let ev = l
            .append(
                "blob.manifest",
                json!({"root": "aa".repeat(32), "name": "x", "size": 3000 * CHUNK, "index": index}),
            )
            .unwrap();
        assert!(parse_manifest(&ev).is_some());
    }

    #[test]
    fn preferred_sources_go_first_and_failed_peers_drop_out() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let peers = s(&["http://a", "http://b", "http://c"]);
        let mut down = HashSet::new();
        assert_eq!(
            ask_order(
                &s(&["http://x", "http://b", "http://x"]),
                false,
                &peers,
                2,
                &down
            ),
            s(&["http://x", "http://b", "http://c", "http://a"])
        );
        assert_eq!(
            ask_order(&s(&["http://x", "http://b"]), true, &peers, 2, &down),
            s(&["http://x", "http://b"])
        );
        down.insert("http://x".to_string());
        down.insert("http://c".to_string());
        assert_eq!(
            ask_order(&s(&["http://x"]), false, &peers, 0, &down),
            s(&["http://a", "http://b"])
        );
        assert!(ask_order(&s(&["http://x"]), true, &peers, 0, &down).is_empty());
        assert!(ask_order(&[], false, &[], 0, &down).is_empty());
    }

    #[test]
    fn slow_links_get_their_own_rate() {
        let cfg: crate::config::BlobsCfg = toml::from_str(
            "rate_kbps = 2000\n[peer_rate_kbps]\n\"http://10.1.2.3:8790/\" = 500\n\"http://10.9.9.9:8790\" = 9000",
        )
        .unwrap();
        let p = Pace::new(&cfg);
        assert_eq!(p.for_peer("http://10.1.2.3:8790"), 62_500);
        assert_eq!(
            p.for_peer("http://10.9.9.9:8790"),
            250_000,
            "never above the overall rate"
        );
        assert_eq!(p.for_peer("http://10.5.5.5:8790"), 250_000);
    }

    #[test]
    fn peer_urls_are_base_urls() {
        assert_eq!(
            peer_url(" http://10.1.2.3:8790/ ").as_deref(),
            Some("http://10.1.2.3:8790")
        );
        assert_eq!(
            peer_url("http://N0CALL-Pi.local.mesh:80").as_deref(),
            Some("http://n0call-pi.local.mesh")
        );
        for bad in [
            "https://x",
            "http://x/path",
            "10.1.2.3",
            "",
            "http://",
            "http://u:p@h",
            "http://h?x",
            "http://h#f",
            "http://a b",
            "http://h:99999",
        ] {
            assert!(peer_url(bad).is_none(), "{bad}");
        }
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            sources_from(&s(&["http://a:1", "http://a:1/", "http://b"])).unwrap(),
            s(&["http://a:1", "http://b"])
        );
        assert!(sources_from(&s(&["ftp://a"])).is_err());
        let many: Vec<String> = (0..17).map(|i| format!("http://10.0.0.{i}")).collect();
        assert!(sources_from(&many).is_err());
    }

    #[test]
    fn older_databases_gain_the_new_columns() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE blobs (root TEXT PRIMARY KEY, event_id TEXT NOT NULL, author TEXT NOT NULL,
             name TEXT NOT NULL, size INTEGER NOT NULL, json TEXT NOT NULL,
             wanted INTEGER NOT NULL DEFAULT 0, complete INTEGER NOT NULL DEFAULT 0,
             priority INTEGER NOT NULL DEFAULT 5, added_ms INTEGER NOT NULL);
             INSERT INTO blobs (root, event_id, author, name, size, json, added_ms)
             VALUES ('r', 'e', 'a', 'n', 1, '{}', 0);",
        )
        .unwrap();
        migrate(&c).unwrap();
        migrate(&c).unwrap();
        assert_eq!(sources(&c, "r").unwrap(), (vec![], false));
        assert!(want_from(&c, "r", &["http://a".into()], true).unwrap());
        assert_eq!(
            sources(&c, "r").unwrap(),
            (vec!["http://a".to_string()], true)
        );
    }

    #[test]
    fn windows_wrap_midnight() {
        assert!(in_window(None, 12));
        assert!(in_window(Some((1, 6)), 3) && !in_window(Some((1, 6)), 6));
        assert!(
            in_window(Some((22, 4)), 23)
                && in_window(Some((22, 4)), 2)
                && !in_window(Some((22, 4)), 12)
        );
    }
}
