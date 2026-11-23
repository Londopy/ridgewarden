//! The signed, append-only event log and its merge rule.
//!
//! Merge is set union by event id: every correctly signed event is kept, whatever order it
//! arrived in, so any two agents that have pulled from each other hold the same set.
//!
//! Each author writes one or more chains: the main chain (identity, trust, checkpoints,
//! app data) and sub-logs such as `telemetry` (polls, probes). Within a chain, events are
//! numbered and point back to the one before (`prev`), which is how gaps are found: an
//! agent asks a peer for everything after the last number it holds without a hole. Two
//! events with the same chain and number (a fork: one key on two boxes) are both kept.
//!
//! Sub-logs can be pruned: an author's signed `log.checkpoint` says "my telemetry before
//! #N may go", and every agent deletes those events and counts them as present. Because
//! every agent applies the same author-signed checkpoints, they still converge.
//!
//! Policy (who to believe, revoked keys) is applied when events are *read*, never by
//! refusing to store them. Refusing would make what an agent holds depend on arrival order.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use anyhow::{Result, bail};
use ridgewarden_client::core::FeedItem;
use ridgewarden_client::event::chain_id;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::event::{Draft, Event};
use crate::identity::Identity;
use crate::store::{Store, checkpoint_of, insert_event};
use crate::util::{ms_to_rfc3339, now_ms};

/// Largest event accepted from a peer. A poll with a long service list is a few KB;
/// this only exists so a broken or hostile peer can't hand us something enormous.
pub const MAX_EVENT_BYTES: usize = 256 * 1024;
/// A Lamport clock above this is nonsense: nobody writes 2^53 events.
pub const MAX_LAMPORT: u64 = 1 << 53;
/// Same for a sequence number, and for how far a checkpoint may reach.
pub const MAX_SEQ: u64 = 1 << 53;
/// How far one received event may push our own clock forward.
pub const MAX_LAMPORT_JUMP: u64 = 1_000_000;
/// Upper bound on one `/v1/events` response body, however small the events.
pub const PAGE_BYTES: usize = 4 * 1024 * 1024;
/// Seqs per digest bucket when two agents' digests differ and they look for where.
pub const DIGEST_BUCKET: i64 = 1024;

pub const TELEMETRY: &str = "telemetry";

pub struct Log {
    pub store: Store,
    pub id: Identity,
    lamport: Mutex<u64>,
    /// `mono_time` is this database's clock: where it had got to when the log was opened,
    /// plus how long this process has run
    mono_base: u64,
    started: Instant,
    /// keys pinned in config
    pinned: RwLock<BTreeSet<String>>,
    /// keys whose events apps may build views from (see `refresh_trust`)
    trusted: RwLock<BTreeSet<String>>,
    /// keys we refuse to store or relay at all
    blocked: RwLock<BTreeSet<String>>,
}

/// What an agent holds for one chain.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Head {
    /// the chain: an author's key, or `key:log` for a sub-log
    pub key: String,
    pub call: String,
    /// highest seq with every seq from 0 up to it present (or pruned); -1 when seq 0 is missing
    pub seq: i64,
    pub max_seq: i64,
    /// events held for this chain (more than seq + 1 when there are forks)
    #[serde(default)]
    pub count: i64,
    /// sequence numbers held more than once (fork points)
    #[serde(default)]
    pub forks: Vec<i64>,
    /// XOR of every event id held for this chain (and of refused ones): two agents with
    /// the same digest hold the same events, even when their chains look alike
    #[serde(default)]
    pub digest: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IngestReport {
    pub accepted: usize,
    pub duplicate: usize,
    pub rejected: usize,
}

const HEADS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS heads (
    chain      TEXT PRIMARY KEY,
    call       TEXT NOT NULL,
    contiguous INTEGER NOT NULL,
    max_seq    INTEGER NOT NULL,
    digest     TEXT NOT NULL DEFAULT ''
);";

/// How far this database's monotonic clock has got: what the last append saved, or the
/// newest of our own events if that is further on, as after restoring an old copy of the
/// database and syncing our newer events back from peers, or in a database from before
/// the clock was saved.
fn saved_mono(c: &Connection, key: &str) -> Result<u64> {
    let mut newest: u64 = c
        .query_row("SELECT v FROM meta WHERE k = 'mono'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    for chain in [key.to_string(), chain_id(key, Some(TELEMETRY))] {
        let m: Option<i64> = c
            .query_row(
                "SELECT json_extract(json, '$.mono_time') FROM events WHERE chain = ?1
                 ORDER BY seq DESC LIMIT 1",
                params![chain],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        newest = newest.max(m.unwrap_or(0).max(0) as u64);
    }
    Ok(newest)
}

/// The author key a chain belongs to.
pub fn author_of(chain: &str) -> &str {
    chain.split(':').next().unwrap_or(chain)
}

/// XOR one id into a hex digest. Order doesn't matter, so arrival order can't either, and
/// XORing the same id again takes it back out (which is how pruning keeps digests equal).
pub fn xor_into(digest: &str, id: &str) -> String {
    let mut a = hex::decode(digest).unwrap_or_default();
    a.resize(32, 0);
    let b = hex::decode(id).unwrap_or_default();
    for (x, y) in a.iter_mut().zip(b.iter()) {
        *x ^= y;
    }
    hex::encode(a)
}

fn pruned_before(tx: &Transaction, chain: &str) -> Result<i64> {
    Ok(tx
        .query_row(
            "SELECT before FROM checkpoints WHERE chain = ?1",
            params![chain],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Is (chain, seq) covered: held, refused with a valid signature, or pruned?
fn present(tx: &Transaction, chain: &str, seq: i64) -> Result<bool> {
    if seq < pruned_before(tx, chain)? {
        return Ok(true);
    }
    Ok(tx
        .query_row(
            "SELECT 1 FROM events WHERE chain = ?1 AND seq = ?2
             UNION ALL SELECT 1 FROM rejected WHERE chain = ?1 AND seq = ?2 LIMIT 1",
            params![chain, seq],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn mix_digest(tx: &Transaction, chain: &str, id: &str) -> Result<()> {
    let d: String = tx.query_row(
        "SELECT digest FROM heads WHERE chain = ?1",
        params![chain],
        |r| r.get(0),
    )?;
    tx.execute(
        "UPDATE heads SET digest = ?2 WHERE chain = ?1",
        params![chain, xor_into(&d, id)],
    )?;
    Ok(())
}

fn ensure_head(tx: &Transaction, chain: &str, call: &str) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO heads (chain, call, contiguous, max_seq) VALUES (?1, ?2, -1, -1)",
        params![chain, call],
    )?;
    Ok(())
}

/// Record `seq` in the chain's head and walk the gap-free mark forward as far as it goes.
fn bump_head(tx: &Transaction, chain: &str, call: &str, seq: i64) -> Result<()> {
    ensure_head(tx, chain, call)?;
    tx.execute(
        "UPDATE heads SET max_seq = max(max_seq, ?2) WHERE chain = ?1",
        params![chain, seq],
    )?;
    walk_head(tx, chain)
}

fn walk_head(tx: &Transaction, chain: &str) -> Result<()> {
    let mut contiguous: i64 = tx.query_row(
        "SELECT contiguous FROM heads WHERE chain = ?1",
        params![chain],
        |r| r.get(0),
    )?;
    let start = contiguous;
    contiguous = contiguous.max(pruned_before(tx, chain)? - 1);
    while let Some(next) = contiguous.checked_add(1)
        && present(tx, chain, next)?
    {
        contiguous = next;
    }
    if contiguous != start {
        tx.execute(
            "UPDATE heads SET contiguous = ?2 WHERE chain = ?1",
            params![chain, contiguous],
        )?;
    }
    Ok(())
}

/// Apply a checkpoint: delete the chain's events (and refusals) below `before`, take their
/// ids out of the digest, and count everything below as present.
fn prune(tx: &Transaction, chain: &str, call: &str) -> Result<usize> {
    let before = pruned_before(tx, chain)?;
    ensure_head(tx, chain, call)?;
    let ids: Vec<String> = tx
        .prepare("SELECT id FROM events WHERE chain = ?1 AND seq < ?2")?
        .query_map(params![chain, before], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for id in &ids {
        mix_digest(tx, chain, id)?;
    }
    tx.execute(
        "DELETE FROM events WHERE chain = ?1 AND seq < ?2",
        params![chain, before],
    )?;
    tx.execute(
        "DELETE FROM rejected WHERE chain = ?1 AND seq < ?2",
        params![chain, before],
    )?;
    walk_head(tx, chain)?;
    Ok(ids.len())
}

/// Store an accepted event and do the bookkeeping that goes with it.
fn record(tx: &Transaction, e: &Event) -> Result<()> {
    insert_event(tx, e)?;
    let chain = e.chain();
    bump_head(tx, &chain, &e.author.call, e.seq as i64)?;
    mix_digest(tx, &chain, &e.id)?;
    if e.kind == "log.checkpoint"
        && e.log.is_none()
        && let Some((sub, _)) = checkpoint_of(e)
    {
        let n = prune(tx, &sub, &e.author.call)?;
        if n > 0 {
            tracing::debug!(author = %e.author.call, "pruned {n} events below checkpoint");
        }
    }
    Ok(())
}

/// The station keys a `trust.list` event names.
pub fn list_keys(e: &Event) -> Vec<String> {
    e.payload
        .get("stations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("key")?.as_str())
        .filter(|k| crate::invite::valid_key(k))
        .map(str::to_string)
        .collect()
}

/// Checks on a correctly signed event that we still won't hold.
fn policy(e: &Event) -> Result<()> {
    if serde_json::to_vec(e)?.len() > MAX_EVENT_BYTES {
        bail!("event too large");
    }
    if e.lamport > MAX_LAMPORT {
        bail!("lamport clock {} is not believable", e.lamport);
    }
    Ok(())
}

impl Log {
    /// Open the log for this identity, writing its `identity.bind` event on first run.
    pub fn new(store: Store, id: Identity) -> Result<Self> {
        let (lamport, mono_base): (i64, u64) = {
            let c = store.conn();
            c.execute_batch(HEADS_SCHEMA)?;
            let lamport = c.query_row("SELECT coalesce(max(lamport), 0) FROM events", [], |r| {
                r.get(0)
            })?;
            (lamport, saved_mono(&c, &id.author.key)? + 1)
        };
        let trusted = RwLock::new([id.author.key.clone()].into_iter().collect());
        let log = Self {
            store,
            id,
            lamport: Mutex::new((lamport.max(0) as u64).min(MAX_LAMPORT)),
            mono_base,
            started: Instant::now(),
            trusted,
            pinned: RwLock::new(BTreeSet::new()),
            blocked: RwLock::new(BTreeSet::new()),
        };
        let has_bind: bool = log.store.conn().query_row(
            "SELECT count(*) > 0 FROM events WHERE chain = ?1 AND seq = 0",
            params![log.id.author.key],
            |r| r.get(0),
        )?;
        if !has_bind {
            log.append(
                "identity.bind",
                json!({"callsign": log.id.author.call, "key": log.id.author.key}),
            )?;
        }
        Ok(log)
    }

    /// Pin these keys (from config) as trusted, on top of our own and any joined ones.
    pub fn set_trusted<I: IntoIterator<Item = String>>(&self, keys: I) {
        *self.pinned.write().unwrap_or_else(|e| e.into_inner()) = keys
            .into_iter()
            .map(|k| k.trim().to_ascii_lowercase())
            .collect();
        if let Err(e) = self.refresh_trust() {
            tracing::warn!("working out trusted stations: {e:#}");
        }
    }

    pub fn trusted(&self) -> BTreeSet<String> {
        self.trusted
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Stations we trust directly: ourselves, keys pinned in config, and keys from
    /// accepted invites. Only these can change the trust list.
    fn base_trust(&self) -> Result<BTreeSet<String>> {
        let mut base = self
            .pinned
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        base.insert(self.id.author.key.clone());
        let c = self.store.conn();
        let mut st = c.prepare("SELECT key FROM local_trust WHERE provisional = 0")?;
        for k in st.query_map([], |r| r.get::<_, String>(0))? {
            base.insert(k?);
        }
        Ok(base)
    }

    /// The trust list: the newest `trust.list` event written by a station we trust
    /// directly and whose key hasn't been revoked.
    pub fn trust_list(&self) -> Result<Option<Event>> {
        let base = self.base_trust()?;
        let c = self.store.conn();
        let mut st = c.prepare(
            "SELECT e.json FROM events e
             WHERE e.kind = 'trust.list' AND e.chain = e.author_key
               AND NOT EXISTS (SELECT 1 FROM revoked r WHERE r.author_key = e.author_key)
             ORDER BY e.lamport DESC, e.id DESC",
        )?;
        for j in st.query_map([], |r| r.get::<_, String>(0))? {
            let e: Event = serde_json::from_str(&j?)?;
            if base.contains(&e.author.key) {
                return Ok(Some(e));
            }
        }
        Ok(None)
    }

    /// Recompute who apps believe: everyone we trust directly, plus the trust list. Until a
    /// trust list has arrived, stations an accepted invite listed count too.
    pub fn refresh_trust(&self) -> Result<()> {
        let mut trusted = self.base_trust()?;
        match self.trust_list()? {
            Some(list) => trusted.extend(list_keys(&list)),
            None => {
                let c = self.store.conn();
                let mut st = c.prepare("SELECT key FROM local_trust WHERE provisional = 1")?;
                for k in st.query_map([], |r| r.get::<_, String>(0))? {
                    trusted.insert(k?);
                }
            }
        }
        let blocked = self
            .blocked
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        trusted.retain(|k| !blocked.contains(k));
        *self.trusted.write().unwrap_or_else(|e| e.into_inner()) = trusted;
        Ok(())
    }

    /// A short fingerprint of the trusted set; changes whenever it does.
    pub fn trust_epoch(&self) -> String {
        let t = self.trusted();
        let joined: Vec<&str> = t.iter().map(String::as_str).collect();
        blake3::hash(joined.join(",").as_bytes()).to_hex()[..12].to_string()
    }

    /// Refuse to store or relay these keys' events, and delete what we already hold.
    pub fn set_blocked<I: IntoIterator<Item = String>>(&self, keys: I) -> Result<()> {
        let keys: BTreeSet<String> = keys
            .into_iter()
            .map(|k| k.trim().to_ascii_lowercase())
            .filter(|k| *k != self.id.author.key)
            .collect();
        {
            let mut conn = self.store.conn();
            let tx = conn.transaction()?;
            for k in &keys {
                tx.execute("DELETE FROM events WHERE author_key = ?1", params![k])?;
                tx.execute(
                    "DELETE FROM rejected WHERE chain = ?1 OR chain LIKE ?1 || ':%'",
                    params![k],
                )?;
                tx.execute(
                    "DELETE FROM heads WHERE chain = ?1 OR chain LIKE ?1 || ':%'",
                    params![k],
                )?;
            }
            tx.commit()?;
        }
        *self.blocked.write().unwrap_or_else(|e| e.into_inner()) = keys;
        self.refresh_trust()
    }

    pub fn is_blocked(&self, chain: &str) -> bool {
        self.blocked
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(author_of(chain))
    }

    pub fn append(&self, kind: &str, payload: Value) -> Result<Event> {
        self.append_in(kind, payload, None, now_ms())
    }

    /// Append to this station's telemetry sub-log, which is pruned after a while.
    pub fn append_telemetry(&self, kind: &str, payload: Value) -> Result<Event> {
        self.append_in(kind, payload, Some(TELEMETRY), now_ms())
    }

    /// Append with an explicit wall time. Tests use this to build history quickly.
    pub fn append_at(&self, kind: &str, payload: Value, wall_ms: i64) -> Result<Event> {
        self.append_in(kind, payload, None, wall_ms)
    }

    pub fn append_in(
        &self,
        kind: &str,
        payload: Value,
        log: Option<&str>,
        wall_ms: i64,
    ) -> Result<Event> {
        let chain = chain_id(&self.id.author.key, log);
        let mut conn = self.store.conn();
        // take the write lock before reading the last seq: a CLI command and the running
        // agent appending at once must not both pick the same next number
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let last: Option<(i64, String)> = tx
            .query_row(
                "SELECT seq, id FROM events WHERE chain = ?1 ORDER BY seq DESC, id LIMIT 1",
                params![chain],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // after pruning, our own chain may be empty below the checkpoint: continue from it
        let floor: i64 = tx
            .query_row(
                "SELECT max_seq FROM heads WHERE chain = ?1",
                params![chain],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(-1)
            .max(pruned_before(&tx, &chain)? - 1);
        let (seq, prev) = match last {
            Some((s, id)) if s >= floor => (s as u64 + 1, Some(id)),
            _ => ((floor + 1) as u64, None),
        };
        let lamport = {
            let mut l = self.lamport.lock().unwrap_or_else(|e| e.into_inner());
            let Some(next) = l.checked_add(1).filter(|n| *n <= MAX_LAMPORT) else {
                bail!("lamport clock exhausted");
            };
            *l = next;
            next
        };
        // past whatever another process (a CLI command) last wrote, too
        let mono = (self.mono_base + self.started.elapsed().as_millis() as u64)
            .max(saved_mono(&tx, &self.id.author.key)? + 1);
        tx.execute(
            "INSERT INTO meta (k, v) VALUES ('mono', ?1) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![mono.to_string()],
        )?;
        let ev = Draft {
            author: self.id.author.clone(),
            seq,
            prev,
            lamport,
            wall_time: ms_to_rfc3339(wall_ms),
            mono_time: mono,
            kind: kind.to_string(),
            payload,
            log: log.map(str::to_string),
        }
        .sign(&self.id.key);
        policy(&ev)?;
        record(&tx, &ev)?;
        tx.commit()?;
        drop(conn);
        if kind == "trust.list" {
            self.refresh_trust()?;
        }
        Ok(ev)
    }

    /// Verify and merge events from a peer, in one transaction.
    pub fn ingest(&self, events: &[Event]) -> Result<IngestReport> {
        let mut rep = IngestReport::default();
        let mut conn = self.store.conn();
        let tx = conn.transaction()?;
        let mut clock = *self.lamport.lock().unwrap_or_else(|e| e.into_inner());
        let mut trust_changed = false;
        for e in events {
            if self.is_blocked(&e.author.key) || e.seq > MAX_SEQ {
                rep.rejected += 1;
                continue;
            }
            // cheap duplicate check first: a converged peer resends mostly known events
            let known: bool = tx
                .query_row(
                    "SELECT 1 FROM events WHERE id = ?1 UNION ALL SELECT 1 FROM rejected WHERE id = ?1 LIMIT 1",
                    params![e.id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            let chain = e.chain();
            if known || (e.seq as i64) < pruned_before(&tx, &chain)? {
                rep.duplicate += 1;
                continue;
            }
            if let Err(err) = e.verify() {
                // not tombstoned: a forged event must not stand in for the real one
                tracing::warn!(id = %e.id.escape_debug(), "rejected unverifiable event: {err}");
                rep.rejected += 1;
                continue;
            }
            if let Err(err) = policy(e) {
                tracing::warn!(id = %e.id, author = %e.author.call, "refused event: {err}");
                tx.execute(
                    "INSERT OR IGNORE INTO rejected (id, chain, seq, reason) VALUES (?1, ?2, ?3, ?4)",
                    params![e.id, chain, e.seq as i64, err.to_string()],
                )?;
                bump_head(&tx, &chain, &e.author.call, e.seq as i64)?;
                rep.rejected += 1;
                continue;
            }
            record(&tx, e)?;
            trust_changed |= e.kind == "trust.list";
            rep.accepted += 1;
            // a far-ahead clock can only drag ours forward so far
            clock = clock.max(e.lamport.min(clock.saturating_add(MAX_LAMPORT_JUMP)));
        }
        tx.commit()?;
        drop(conn);
        {
            let mut l = self.lamport.lock().unwrap_or_else(|e| e.into_inner());
            *l = (*l).max(clock).min(MAX_LAMPORT);
        }
        if trust_changed {
            self.refresh_trust()?;
        }
        Ok(rep)
    }

    /// Checkpoint our own telemetry: everything written before `keep_ms` ago may be pruned
    /// everywhere. Returns the checkpoint event if the prune point moved.
    pub fn checkpoint_telemetry(&self, keep_ms: i64) -> Result<Option<Event>> {
        let chain = chain_id(&self.id.author.key, Some(TELEMETRY));
        let (candidate, current): (Option<i64>, i64) = {
            let c = self.store.conn();
            let candidate = c.query_row(
                "SELECT min(seq) FROM events WHERE chain = ?1 AND wall_ms >= ?2",
                params![chain, now_ms() - keep_ms],
                |r| r.get(0),
            )?;
            let current = c
                .query_row(
                    "SELECT before FROM checkpoints WHERE chain = ?1",
                    params![chain],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            (candidate, current)
        };
        match candidate {
            Some(before) if before > current => Ok(Some(self.append(
                "log.checkpoint",
                json!({"log": TELEMETRY, "before": before}),
            )?)),
            _ => Ok(None),
        }
    }

    pub fn heads(&self) -> Result<Vec<Head>> {
        let c = self.store.conn();
        let mut counts: BTreeMap<String, i64> = BTreeMap::new();
        for r in c
            .prepare("SELECT chain, count(*) FROM events GROUP BY chain")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        {
            let (k, n) = r?;
            counts.insert(k, n);
        }
        let mut forks: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for r in c
            .prepare("SELECT chain, seq FROM events GROUP BY chain, seq HAVING count(*) > 1 ORDER BY seq")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        {
            let (k, s) = r?;
            forks.entry(k).or_default().push(s);
        }
        // main chains sort before their sub-logs, so a puller sees checkpoints first
        let mut st =
            c.prepare("SELECT chain, call, contiguous, max_seq, digest FROM heads ORDER BY chain")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = vec![];
        for r in rows {
            let (key, call, seq, max_seq, digest) = r?;
            out.push(Head {
                digest,
                count: counts.get(&key).copied().unwrap_or(0),
                forks: forks.remove(&key).unwrap_or_default(),
                key,
                call,
                seq,
                max_seq,
            });
        }
        Ok(out)
    }

    /// Highest seq we hold or have refused for `chain`, -1 if none.
    pub fn max_seq(&self, chain: &str) -> Result<i64> {
        Ok(self
            .store
            .conn()
            .query_row(
                "SELECT max_seq FROM heads WHERE chain = ?1",
                params![chain],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(-1))
    }

    /// Runs of missing seqs between the gap-free mark and the highest seq held, as
    /// (first, length), lowest first.
    pub fn holes(&self, chain: &str, max_runs: usize) -> Result<Vec<(i64, i64)>> {
        let from = self.contiguous(chain)?;
        let c = self.store.conn();
        let mut st = c.prepare(
            "SELECT seq FROM events WHERE chain = ?1 AND seq > ?2
             UNION SELECT seq FROM rejected WHERE chain = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let mut out = vec![];
        let mut next = from + 1;
        for r in st.query_map(params![chain, from], |r| r.get::<_, i64>(0))? {
            let s = r?;
            if s > next {
                out.push((next, s - next));
                if out.len() >= max_runs {
                    break;
                }
            }
            next = s + 1;
        }
        Ok(out)
    }

    pub fn contiguous(&self, chain: &str) -> Result<i64> {
        Ok(self
            .store
            .conn()
            .query_row(
                "SELECT contiguous FROM heads WHERE chain = ?1",
                params![chain],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(-1))
    }

    /// How many events we hold for `chain` at `seq` (2+ means a fork).
    pub fn count_at(&self, chain: &str, seq: i64) -> Result<i64> {
        Ok(self.store.conn().query_row(
            "SELECT count(*) FROM events WHERE chain = ?1 AND seq = ?2",
            params![chain, seq],
            |r| r.get(0),
        )?)
    }

    /// Events in `chain` after the cursor (seq, id), in (seq, id) order. With no `after_id`,
    /// everything with seq > `after`. Stops early at `PAGE_BYTES`.
    pub fn events_after(
        &self,
        chain: &str,
        after: i64,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let c = self.store.conn();
        // "g" sorts after every hex digit, so (after, "g") means "past every event at after"
        let after_id = after_id.unwrap_or("g");
        let mut st = c.prepare(
            "SELECT json FROM events WHERE chain = ?1 AND (seq > ?2 OR (seq = ?2 AND id > ?3))
             ORDER BY seq, id LIMIT ?4",
        )?;
        let rows = st.query_map(params![chain, after, after_id, limit as i64], |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = vec![];
        let mut bytes = 0;
        for j in rows {
            let j = j?;
            bytes += j.len();
            if bytes > PAGE_BYTES && !out.is_empty() {
                break;
            }
            out.push(serde_json::from_str(&j)?);
        }
        Ok(out)
    }

    /// This chain's digest.
    pub fn digest(&self, chain: &str) -> Result<String> {
        Ok(self
            .store
            .conn()
            .query_row(
                "SELECT digest FROM heads WHERE chain = ?1",
                params![chain],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default())
    }

    /// Digest per bucket of `DIGEST_BUCKET` seqs, for finding where two agents differ.
    pub fn bucket_digests(&self, chain: &str, upto: i64) -> Result<BTreeMap<i64, String>> {
        let c = self.store.conn();
        let mut st = c.prepare("SELECT seq, id FROM events WHERE chain = ?1 AND seq <= ?2")?;
        let mut out: BTreeMap<i64, String> = BTreeMap::new();
        for r in st.query_map(params![chain, upto], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })? {
            let (seq, id) = r?;
            let b = seq - seq.rem_euclid(DIGEST_BUCKET);
            let d = out.entry(b).or_default();
            *d = xor_into(d, &id);
        }
        Ok(out)
    }

    /// Every event in `chain` at exactly `seq` (to copy the other branch of a fork).
    pub fn events_at(&self, chain: &str, seq: i64) -> Result<Vec<Event>> {
        let c = self.store.conn();
        let mut st = c.prepare(
            "SELECT json FROM events WHERE chain = ?1 AND seq = ?2 ORDER BY id LIMIT 16",
        )?;
        let rows = st.query_map(params![chain, seq], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for j in rows {
            out.push(serde_json::from_str(&j?)?);
        }
        Ok(out)
    }

    /// Every event in local arrival order after `after` (the apps' feed), each marked with
    /// whether apps should believe it: its author is trusted and its key wasn't revoked
    /// before it was written.
    pub fn feed(&self, after: i64, limit: usize) -> Result<Vec<FeedItem>> {
        let trusted = self.trusted();
        let c = self.store.conn();
        let revoked: BTreeMap<String, (i64, i64)> = c
            .prepare("SELECT author_key, after_seq, lamport FROM revoked")?
            .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
            .collect::<Result<_, _>>()?;
        let mut st = c.prepare(
            "SELECT n, json, author_key, chain, seq, lamport, seen_ms FROM events WHERE n > ?1 ORDER BY n LIMIT ?2",
        )?;
        let rows = st.query_map(params![after, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        let mut out = vec![];
        let mut bytes = 0;
        for r in rows {
            let (n, j, key, chain, seq, lamport, seen) = r?;
            bytes += j.len();
            if bytes > PAGE_BYTES && !out.is_empty() {
                break;
            }
            // main chains revoke by position; other chains by the clock at revocation
            let before_revoke = revoked.get(&key).is_none_or(|(after_seq, at)| {
                if chain == key {
                    seq <= *after_seq
                } else {
                    lamport < *at
                }
            });
            out.push(FeedItem {
                seq: n,
                event: serde_json::from_str(&j)?,
                trusted: trusted.contains(&key) && before_revoke,
                seen_ms: Some(seen),
            });
        }
        Ok(out)
    }

    pub fn event_ids(&self) -> Result<Vec<String>> {
        let c = self.store.conn();
        let mut st = c.prepare("SELECT id FROM events ORDER BY id")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Fork points, worked out from what we hold (so arrival order doesn't matter): sequence
    /// numbers held twice, plus events whose `prev` matches none of the events before them.
    pub fn fork_count(&self) -> Result<i64> {
        let c = self.store.conn();
        let doubles: i64 = c.query_row(
            "SELECT count(*) FROM (SELECT 1 FROM events GROUP BY chain, seq HAVING count(*) > 1)",
            [],
            |r| r.get(0),
        )?;
        let broken: i64 = c.query_row(
            "SELECT count(*) FROM events e
             WHERE e.seq > 0
               AND EXISTS (SELECT 1 FROM events p WHERE p.chain = e.chain AND p.seq = e.seq - 1)
               AND NOT EXISTS (SELECT 1 FROM events p WHERE p.chain = e.chain AND p.seq = e.seq - 1
                               AND p.id = json_extract(e.json, '$.prev'))",
            [],
            |r| r.get(0),
        )?;
        Ok(doubles + broken)
    }

    /// This agent's own `identity.bind` event, which peers use to check who it is.
    pub fn bind_event(&self) -> Result<Event> {
        self.events_at(&self.id.author.key, 0)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no bind event"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(seed: u8, call: &str) -> Log {
        Log::new(
            Store::open_in_memory().unwrap(),
            Identity::from_seed(call, [seed; 32]),
        )
        .unwrap()
    }

    fn all(l: &Log, chain: &str) -> Vec<Event> {
        l.events_after(chain, -1, None, 10_000).unwrap()
    }

    fn telemetry(l: &Log) -> String {
        chain_id(&l.id.author.key, Some(TELEMETRY))
    }

    #[test]
    fn append_chains_events() {
        let l = log(1, "N0AAA");
        let a = l.append("x", json!({})).unwrap();
        let b = l.append("x", json!({})).unwrap();
        assert_eq!(a.seq, 1);
        assert_eq!(b.prev.as_deref(), Some(a.id.as_str()));
        assert!(b.lamport > a.lamport);
        assert_eq!(l.contiguous(&l.id.author.key).unwrap(), 2);
    }

    #[test]
    fn mono_time_keeps_counting_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("ridgewarden.db");
        let open = || {
            Log::new(
                Store::open(&db).unwrap(),
                Identity::from_seed("N0AAA", [1; 32]),
            )
            .unwrap()
        };
        let before = {
            let l = open();
            std::thread::sleep(std::time::Duration::from_millis(30));
            l.append_telemetry("node.poll", json!({})).unwrap()
        };
        let l = open();
        let after = l.append("x", json!({})).unwrap();
        assert!(after.mono_time > before.mono_time, "{after:?} {before:?}");

        // a second writer on the same database, like a CLI command, never goes backwards
        let cli = open();
        let a = l.append("x", json!({})).unwrap();
        let b = cli.append("x", json!({})).unwrap();
        let c = l.append("x", json!({})).unwrap();
        assert!(a.mono_time < b.mono_time && b.mono_time < c.mono_time);

        // a database from before the clock was saved, or whose saved clock is behind its
        // newest event, goes on from that event
        for saved in [None, Some("5")] {
            let c = l.append("x", json!({})).unwrap();
            let conn = l.store.conn();
            conn.execute("DELETE FROM meta WHERE k = 'mono'", [])
                .unwrap();
            if let Some(v) = saved {
                conn.execute("INSERT INTO meta (k, v) VALUES ('mono', ?1)", [v])
                    .unwrap();
            }
            drop(conn);
            let d = open().append("x", json!({})).unwrap();
            assert!(d.mono_time > c.mono_time, "saved {saved:?}");
        }
    }

    #[test]
    fn telemetry_is_its_own_chain() {
        let l = log(1, "N0AAA");
        let t0 = l.append_telemetry("node.poll", json!({})).unwrap();
        let m = l.append("x", json!({})).unwrap();
        let t1 = l.append_telemetry("node.poll", json!({})).unwrap();
        assert_eq!((t0.seq, t1.seq), (0, 1));
        assert_eq!(t1.prev.as_deref(), Some(t0.id.as_str()));
        assert_eq!(m.seq, 1, "main chain numbering is unaffected");
        assert_eq!(l.contiguous(&telemetry(&l)).unwrap(), 1);
    }

    #[test]
    fn out_of_order_ingest_fills_gaps() {
        let a = log(1, "N0AAA");
        for _ in 0..5 {
            a.append("x", json!({})).unwrap();
        }
        let evs = all(&a, &a.id.author.key);
        let b = log(2, "N0BBB");
        let mut rev = evs.clone();
        rev.reverse();
        b.ingest(&rev[..3]).unwrap(); // seqs 5,4,3: a gap below
        assert_eq!(b.contiguous(&a.id.author.key).unwrap(), -1);
        b.ingest(&rev[3..]).unwrap();
        assert_eq!(b.contiguous(&a.id.author.key).unwrap(), 5);
        assert_eq!(b.fork_count().unwrap(), 0);
    }

    #[test]
    fn duplicates_and_tampering() {
        let a = log(1, "N0AAA");
        a.append("x", json!({"v": 1})).unwrap();
        let mut evs = all(&a, &a.id.author.key);
        let b = log(2, "N0BBB");
        assert_eq!(b.ingest(&evs).unwrap().accepted, 2);
        assert_eq!(b.ingest(&evs).unwrap().duplicate, 2);
        // a tampered copy reaching someone who doesn't have the real one is refused
        evs[1].payload = json!({"v": 2});
        let c = log(3, "N0CCC");
        assert_eq!(c.ingest(&evs[1..]).unwrap().rejected, 1);
        assert_eq!(
            c.contiguous(&a.id.author.key).unwrap(),
            -1,
            "and leaves no tombstone"
        );
    }

    #[test]
    fn revocation_is_applied_on_read_not_on_arrival() {
        let a = log(1, "N0AAA");
        a.append("identity.revoke", json!({"reason": "test"}))
            .unwrap();
        let later = a.append("x", json!({})).unwrap();
        let later_t = a.append_telemetry("node.poll", json!({})).unwrap();
        let mut evs = all(&a, &a.id.author.key);
        evs.extend(all(&a, &telemetry(&a)));
        // two observers get the same events in opposite orders and end up the same
        let b = log(2, "N0BBB");
        let c = log(3, "N0CCC");
        for o in [&b, &c] {
            o.set_trusted([a.id.author.key.clone()]);
        }
        b.ingest(&evs).unwrap();
        let mut rev = evs.clone();
        rev.reverse();
        c.ingest(&rev).unwrap();
        let of_a = |o: &Log| {
            all(o, &a.id.author.key)
                .into_iter()
                .map(|e| e.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(of_a(&b), of_a(&c));
        assert_eq!(of_a(&b).len(), 3, "the post-revocation event is stored too");
        for o in [&b, &c] {
            let feed = o.feed(-1, 100).unwrap();
            for id in [&later.id, &later_t.id] {
                let item = feed.iter().find(|i| &i.event.id == id).unwrap();
                assert!(!item.trusted, "events after a revocation never reach apps");
            }
            assert!(
                feed.iter()
                    .filter(|i| i.event.author.key == a.id.author.key)
                    .filter(|i| i.event.id != later.id && i.event.id != later_t.id)
                    .all(|i| i.trusted)
            );
        }
    }

    #[test]
    fn untrusted_authors_are_kept_but_flagged() {
        let a = log(1, "N0AAA");
        let b = log(2, "N0BBB");
        b.ingest(&all(&a, &a.id.author.key)).unwrap();
        let feed = b.feed(-1, 100).unwrap();
        assert!(
            feed.iter()
                .all(|i| i.trusted == (i.event.author.key == b.id.author.key))
        );
        assert!(feed.iter().all(|i| i.seen_ms.is_some()));
    }

    #[test]
    fn fork_is_kept_and_flagged_in_any_order() {
        let a = log(1, "N0AAA");
        let e1 = a.append("x", json!({"v": 1})).unwrap();
        // a second log with the same key, as if the key were copied to another box
        let clone = log(1, "N0AAA");
        let e1b = clone.append("x", json!({"v": 2})).unwrap();
        assert_eq!(e1.seq, e1b.seq);
        let mut evs = all(&a, &a.id.author.key);
        evs.extend(all(&clone, &a.id.author.key));
        for order in [false, true] {
            let b = log(2, "N0BBB");
            let mut v = evs.clone();
            if order {
                v.reverse();
            }
            b.ingest(&v).unwrap();
            let ids = b.event_ids().unwrap();
            assert!(ids.contains(&e1.id) && ids.contains(&e1b.id));
            let forks = b.fork_count().unwrap();
            assert!(forks >= 2, "seq 0 and seq 1 both doubled; got {forks}");
            let h = b
                .heads()
                .unwrap()
                .into_iter()
                .find(|h| h.key == a.id.author.key)
                .unwrap();
            assert_eq!(h.forks, vec![0, 1]);
            assert_eq!(h.count, 4);
        }
    }

    #[test]
    fn refused_event_does_not_stall_the_chain() {
        let a = log(1, "N0AAA");
        let draft = |seq, payload| {
            Draft {
                author: a.id.author.clone(),
                seq,
                prev: None,
                lamport: seq + 1,
                wall_time: "2026-11-19T00:00:00Z".into(),
                mono_time: 0,
                kind: "x".into(),
                payload,
                log: None,
            }
            .sign(&a.id.key)
        };
        assert!(
            a.append("x", json!({"blob": "x".repeat(MAX_EVENT_BYTES)}))
                .is_err(),
            "an agent does not write what the mesh would refuse"
        );
        let evs = vec![
            all(&a, &a.id.author.key).remove(0),
            draft(1, json!({"blob": "x".repeat(MAX_EVENT_BYTES)})),
            draft(2, json!({})),
        ];
        let b = log(2, "N0BBB");
        let rep = b.ingest(&evs).unwrap();
        assert_eq!((rep.accepted, rep.rejected), (2, 1));
        assert_eq!(
            b.contiguous(&a.id.author.key).unwrap(),
            2,
            "tombstone counts for the gap check"
        );
        assert_eq!(b.ingest(&evs).unwrap().duplicate, 3);
    }

    #[test]
    fn checkpoint_prunes_everywhere_and_digests_still_agree() {
        let a = log(1, "N0AAA");
        let hour = 3_600_000;
        for i in 0..10 {
            a.append_in(
                "node.poll",
                json!({"i": i}),
                Some(TELEMETRY),
                now_ms() - (10 - i) * hour,
            )
            .unwrap();
        }
        let b = log(2, "N0BBB");
        b.ingest(&all(&a, &a.id.author.key)).unwrap();
        b.ingest(&all(&a, &telemetry(&a))).unwrap();
        // keep the last 4.5 hours: the 6 oldest go
        let cp = a
            .checkpoint_telemetry(4 * hour + hour / 2)
            .unwrap()
            .expect("prune point moved");
        assert_eq!(all(&a, &telemetry(&a)).len(), 4);
        b.ingest(&[cp]).unwrap();
        let t = telemetry(&a);
        assert_eq!(all(&b, &t).len(), 4);
        assert_eq!(a.digest(&t).unwrap(), b.digest(&t).unwrap());
        assert_eq!(b.contiguous(&t).unwrap(), 9);
        // a stale peer resending pruned events changes nothing
        let pruned_copy = {
            let old = log(1, "N0AAA");
            for i in 0..3 {
                old.append_in(
                    "node.poll",
                    json!({"i": i}),
                    Some(TELEMETRY),
                    now_ms() - (10 - i) * hour,
                )
                .unwrap();
            }
            all(&old, &t)
        };
        b.ingest(&pruned_copy).unwrap();
        assert_eq!(a.digest(&t).unwrap(), b.digest(&t).unwrap());
        // nothing new to prune: no new checkpoint
        assert!(
            a.checkpoint_telemetry(4 * hour + hour / 2)
                .unwrap()
                .is_none()
        );
        // appends continue the chain after pruning
        let next = a.append_telemetry("node.poll", json!({})).unwrap();
        assert_eq!(next.seq, 10);
    }

    #[test]
    fn blocked_keys_are_dropped_and_refused() {
        let a = log(1, "N0AAA");
        a.append("x", json!({})).unwrap();
        let b = log(2, "N0BBB");
        b.ingest(&all(&a, &a.id.author.key)).unwrap();
        b.set_blocked([a.id.author.key.clone()]).unwrap();
        assert!(all(&b, &a.id.author.key).is_empty());
        assert!(b.heads().unwrap().iter().all(|h| h.key != a.id.author.key));
        assert_eq!(b.ingest(&all(&a, &a.id.author.key)).unwrap().rejected, 2);
        assert!(b.is_blocked(&telemetry(&a)));
    }

    #[test]
    fn absurd_clock_cannot_wreck_ours() {
        let a = log(1, "N0AAA");
        let b = log(2, "N0BBB");
        let draft = |seq, lamport| Draft {
            author: a.id.author.clone(),
            seq,
            prev: None,
            lamport,
            wall_time: "2026-11-19T00:00:00Z".into(),
            mono_time: 0,
            kind: "x".into(),
            payload: json!({}),
            log: None,
        };
        assert_eq!(
            b.ingest(&[draft(0, u64::MAX).sign(&a.id.key)])
                .unwrap()
                .rejected,
            1
        );
        b.ingest(&[draft(1, MAX_LAMPORT).sign(&a.id.key)]).unwrap();
        let next = b.append("x", json!({})).unwrap();
        assert!(
            next.lamport <= MAX_LAMPORT_JUMP + 10,
            "clock only jumped a bounded amount"
        );
    }
}
