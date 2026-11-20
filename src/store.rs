//! SQLite storage for the core.
//!
//! Events are the source of truth. The core only keeps what it needs itself (the log,
//! revocations, its node's raw snapshots, and which lists it has shipped); apps build
//! their own views from the core's feed.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;
use ridgewarden_client::mesh::store_list;
use rusqlite::{Connection, Transaction, params};

use crate::event::Event;
use crate::util::rfc3339_to_ms;

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    -- local arrival order: the apps' feed cursor. AUTOINCREMENT so it never renumbers
    n           INTEGER PRIMARY KEY AUTOINCREMENT,
    id          TEXT NOT NULL UNIQUE,
    author_key  TEXT NOT NULL,
    -- the author's key, or key:log for a sub-log; sequence numbers count per chain
    chain       TEXT NOT NULL,
    author_call TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    lamport     INTEGER NOT NULL,
    wall_ms     INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    json        TEXT NOT NULL,
    -- when this agent first got it, by its own clock
    seen_ms     INTEGER NOT NULL
);
-- not unique: two different events with the same chain+seq is a fork, and a grow-only
-- set has to keep both or two agents could end up with different sets
CREATE INDEX IF NOT EXISTS events_chain_seq ON events(chain, seq);
CREATE INDEX IF NOT EXISTS events_author ON events(author_key);
CREATE INDEX IF NOT EXISTS events_kind ON events(kind, wall_ms);

-- correctly signed events we refused (too big, absurd clock). Remembered so the author's
-- chain still counts as gap-free past them and peers aren't asked for them forever.
CREATE TABLE IF NOT EXISTS rejected (
    id     TEXT PRIMARY KEY,
    chain  TEXT NOT NULL,
    seq    INTEGER NOT NULL,
    reason TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS rejected_chain_seq ON rejected(chain, seq);

-- a key's own revocation: its main-chain seq, and its lamport clock for its other chains
CREATE TABLE IF NOT EXISTS revoked (
    author_key TEXT PRIMARY KEY,
    after_seq  INTEGER NOT NULL,
    lamport    INTEGER NOT NULL
);

-- an author's own word that a sub-log's events below `before` may be deleted everywhere
CREATE TABLE IF NOT EXISTS checkpoints (
    chain  TEXT PRIMARY KEY,
    before INTEGER NOT NULL
);

-- raw sysinfo responses from this agent's own node; local only, never synced
CREATE TABLE IF NOT EXISTS snapshots (
    id      INTEGER PRIMARY KEY,
    node    TEXT NOT NULL,
    ts      INTEGER NOT NULL,
    ok      INTEGER NOT NULL,
    raw     TEXT
);
CREATE INDEX IF NOT EXISTS snapshots_ts ON snapshots(node, ts);

-- service and host lists this core has seen, so it only ships each one once
CREATE TABLE IF NOT EXISTS lists (
    hash TEXT PRIMARY KEY,
    json TEXT NOT NULL
);

-- stations this site trusts because an invite it accepted listed them
CREATE TABLE IF NOT EXISTS local_trust (
    key         TEXT PRIMARY KEY,
    call        TEXT NOT NULL,
    via         TEXT NOT NULL,
    added_ms    INTEGER NOT NULL,
    -- 1: listed in an invite, trusted only until the club list arrives; can't change it
    provisional INTEGER NOT NULL DEFAULT 0
);

-- peers learned from accepted invites
CREATE TABLE IF NOT EXISTS local_peers (
    url      TEXT PRIMARY KEY,
    added_ms INTEGER NOT NULL
);

-- a random id for this database; apps following the feed rebuild their views if it changes
CREATE TABLE IF NOT EXISTS meta (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
"#;

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL + FULL: every commit is on disk before we hand the event to anyone. NORMAL
        // could lose the last commits in a power cut, and then this agent would reuse
        // sequence numbers its peers already hold (a self-inflicted fork). One fsync per
        // poll is cheap even on an SD card.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(crate::blobs::SCHEMA)?;
        crate::blobs::migrate(&conn)?;
        let have: bool = conn.query_row(
            "SELECT count(*) > 0 FROM meta WHERE k = 'instance'",
            [],
            |r| r.get(0),
        )?;
        if !have {
            let mut b = [0u8; 16];
            getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
            conn.execute(
                "INSERT INTO meta (k, v) VALUES ('instance', ?1)",
                [hex::encode(b)],
            )?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// This database's random id (see the `meta` table).
    pub fn instance(&self) -> Result<String> {
        Ok(self
            .conn()
            .query_row("SELECT v FROM meta WHERE k = 'instance'", [], |r| r.get(0))?)
    }

    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Insert one already-verified event and project it. Returns false if it was already there.
pub fn insert_event(tx: &Transaction, e: &Event) -> Result<bool> {
    let wall_ms = rfc3339_to_ms(&e.wall_time).unwrap_or(0);
    let n = tx.execute(
        "INSERT OR IGNORE INTO events (id, author_key, chain, author_call, seq, lamport, wall_ms, kind, json, seen_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            e.id,
            e.author.key,
            e.chain(),
            e.author.call,
            e.seq as i64,
            e.lamport.min(i64::MAX as u64) as i64,
            wall_ms,
            e.kind,
            serde_json::to_string(e)?,
            crate::util::now_ms(),
        ],
    )?;
    if n == 0 {
        return Ok(false);
    }
    project(tx, e)?;
    Ok(true)
}

fn project(tx: &Transaction, e: &Event) -> Result<()> {
    match e.kind.as_str() {
        "mesh.list" => store_list(tx, &e.payload)?,
        "identity.revoke" if e.log.is_none() => {
            tx.execute(
                "INSERT INTO revoked (author_key, after_seq, lamport) VALUES (?1, ?2, ?3)
                 ON CONFLICT(author_key) DO UPDATE SET after_seq = min(after_seq, excluded.after_seq),
                     lamport = min(lamport, excluded.lamport)",
                params![e.author.key, e.seq as i64, e.lamport.min(i64::MAX as u64) as i64],
            )?;
        }
        "blob.manifest" => crate::blobs::note_manifest(tx, e)?,
        "log.checkpoint" if e.log.is_none() => {
            if let Some((chain, before)) = checkpoint_of(e) {
                tx.execute(
                    "INSERT INTO checkpoints (chain, before) VALUES (?1, ?2)
                     ON CONFLICT(chain) DO UPDATE SET before = max(before, excluded.before)",
                    params![chain, before],
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The sub-log chain a checkpoint covers and its prune point, if it is well formed.
/// Only an author's main chain can checkpoint, and only its own sub-logs.
pub fn checkpoint_of(e: &Event) -> Option<(String, i64)> {
    let log = e.payload.get("log")?.as_str()?;
    let before = e.payload.get("before")?.as_i64()?;
    (ridgewarden_client::event::valid_log(log)
        && before > 0
        && before <= crate::log::MAX_SEQ as i64)
        .then(|| {
            (
                ridgewarden_client::event::chain_id(&e.author.key, Some(log)),
                before,
            )
        })
}
