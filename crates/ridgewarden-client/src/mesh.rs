//! The shared view of the mesh built from `node.poll` events: who was polled when and the
//! link samples each poll carried, plus the service and host lists from `mesh.list` events
//! (published once per distinct list and referenced by hash, since they rarely change).
//!
//! Polls also say `"trackers"` (the node sent 4.x `lqm.info.trackers`) and
//! `"stale_skipped"` (trackers left out because the neighbor went quiet); they are in the
//! event payload only, not in these tables. Older polls lack both.

use anyhow::Result;
use rusqlite::{Transaction, params};
use serde_json::Value;

use crate::core::Projection;
use crate::event::Event;
use crate::event::list_hash;
use crate::util::{norm_node, valid_node};

/// Neighbors taken from one poll. A real node has a few dozen at most; this keeps one
/// bogus poll from making every analysis page slow.
pub const MAX_LINKS_PER_POLL: usize = 256;

pub struct Mesh;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS polls (
    event_id    TEXT PRIMARY KEY,
    ts          INTEGER NOT NULL,
    author_key  TEXT NOT NULL,
    node        TEXT NOT NULL,
    ok          INTEGER NOT NULL,
    firmware    TEXT,
    lat         REAL,
    lon         REAL,
    ads_hash    TEXT,
    hosts_hash  TEXT,
    via         TEXT,
    error       TEXT
);
CREATE INDEX IF NOT EXISTS polls_node_ts ON polls(node, ts);
CREATE INDEX IF NOT EXISTS polls_author_ts ON polls(author_key, ts);

CREATE TABLE IF NOT EXISTS lists (
    hash TEXT PRIMARY KEY,
    json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS link_samples (
    event_id   TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    node       TEXT NOT NULL,
    neighbor   TEXT NOT NULL,
    link_type  TEXT,
    signal     REAL,
    noise      REAL,
    snr        REAL,
    quality    REAL,
    nquality   REAL,
    rtt        REAL,
    tx_rate    REAL,
    rx_rate    REAL,
    tx_fail    REAL,
    metric     REAL
);
CREATE INDEX IF NOT EXISTS link_samples_pair_ts ON link_samples(node, neighbor, ts);
CREATE INDEX IF NOT EXISTS link_samples_ts ON link_samples(ts);
CREATE INDEX IF NOT EXISTS link_samples_event ON link_samples(event_id);
"#;

pub fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}

pub fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// Store a list from a `mesh.list` event, but only under the hash it really has, so
/// nobody can put their list under someone else's hash. The core uses this too, to know
/// which lists it has already published.
pub fn store_list(tx: &Transaction, p: &Value) -> Result<()> {
    if let (Some(hash), Some(list)) = (s(p, "hash"), p.get("list"))
        && list_hash(list) == hash
    {
        tx.execute(
            "INSERT OR IGNORE INTO lists (hash, json) VALUES (?1, ?2)",
            params![hash, list.to_string()],
        )?;
    }
    Ok(())
}

impl Projection for Mesh {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch("DELETE FROM polls; DELETE FROM lists; DELETE FROM link_samples;")?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind == "mesh.list" {
            return store_list(tx, &e.payload);
        }
        if e.kind != "node.poll" {
            return Ok(());
        }
        let p = &e.payload;
        let node = norm_node(s(p, "node").unwrap_or(""));
        if !valid_node(&node) {
            return Ok(());
        }
        tx.execute(
            "INSERT OR IGNORE INTO polls (event_id, ts, author_key, node, ok, firmware, lat, lon, ads_hash, hosts_hash, via, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                e.id,
                ts,
                e.author.key,
                node,
                p.get("ok").and_then(Value::as_bool).unwrap_or(false),
                s(p, "firmware"),
                f(p, "lat"),
                f(p, "lon"),
                s(p, "ads_hash"),
                s(p, "hosts_hash"),
                s(p, "via"),
                s(p, "error"),
            ],
        )?;
        for l in p
            .get("links")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(MAX_LINKS_PER_POLL)
        {
            let nb = norm_node(s(l, "neighbor").unwrap_or(""));
            if !valid_node(&nb) {
                continue;
            }
            tx.execute(
                "INSERT INTO link_samples (event_id, ts, node, neighbor, link_type, signal, noise, snr,
                     quality, nquality, rtt, tx_rate, rx_rate, tx_fail, metric)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    e.id,
                    ts,
                    node,
                    nb,
                    s(l, "type"),
                    f(l, "signal"),
                    f(l, "noise"),
                    f(l, "snr"),
                    f(l, "quality"),
                    f(l, "nquality"),
                    f(l, "rtt"),
                    f(l, "tx_rate"),
                    f(l, "rx_rate"),
                    f(l, "tx_fail"),
                    f(l, "metric"),
                ],
            )?;
        }
        Ok(())
    }
}

/// Test helpers apps use to fake a history of polls without a core.
pub mod testing {
    use super::*;
    use crate::core::init_db;
    use crate::event::{Author, Draft};
    use crate::util::ms_to_rfc3339;
    use ed25519_dalek::SigningKey;
    use rusqlite::Connection;
    use std::cell::Cell;

    /// An in-memory database plus a fake author that signs events into it.
    pub struct Fake {
        pub conn: Connection,
        key: SigningKey,
        seq: Cell<u64>,
        projections: Vec<Box<dyn Projection>>,
    }

    impl Fake {
        pub fn new(projections: Vec<Box<dyn Projection>>) -> Self {
            let conn = Connection::open_in_memory().unwrap();
            let refs: Vec<&dyn Projection> = projections.iter().map(|p| p.as_ref()).collect();
            init_db(&conn, &refs).unwrap();
            Self {
                conn,
                key: SigningKey::from_bytes(&[9; 32]),
                seq: Cell::new(0),
                projections,
            }
        }

        /// Sign an event at wall time `ts` and run it through every projection.
        pub fn add(&mut self, kind: &str, payload: Value, ts: i64) -> Event {
            let seq = self.seq.get();
            self.seq.set(seq + 1);
            let e = Draft {
                author: Author {
                    call: "N0TEST".into(),
                    key: hex::encode(self.key.verifying_key().to_bytes()),
                },
                seq,
                prev: None,
                lamport: seq,
                wall_time: ms_to_rfc3339(ts),
                mono_time: 0,
                kind: kind.into(),
                payload,
                log: None,
            }
            .sign(&self.key);
            let tx = self.conn.transaction().unwrap();
            for p in &self.projections {
                p.apply(&tx, &e, ts).unwrap();
            }
            tx.commit().unwrap();
            e
        }
    }
}
