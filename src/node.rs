//! This site's node: each poll, parsed into tables, with the raw reply kept for a while.
//!
//! The parsed tables are what the core itself reads (what changed, status, metrics); the
//! raw replies stay only long enough to check what the node actually sent. Polls of other
//! nodes (the crawl) are only in the log.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::aredn::{self, LinkObs, NodeObs, Service};
use crate::event::list_hash;

const DAY_MS: i64 = 86_400_000;
/// Raw replies, for checking what the node sent.
pub const RAW_KEEP_MS: i64 = 2 * DAY_MS;
/// Parsed polls: a week of "what changed" and a day to spare.
pub const PARSED_KEEP_MS: i64 = 8 * DAY_MS;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS node_polls (
    id          INTEGER PRIMARY KEY,
    ts          INTEGER NOT NULL,
    node        TEXT NOT NULL,
    ok          INTEGER NOT NULL,
    api_version TEXT,
    firmware    TEXT,
    model       TEXT,
    lat         REAL,
    lon         REAL,
    -- hashes of the service and host lists, which are in `lists`
    services    TEXT,
    hosts       TEXT,
    error       TEXT
);
CREATE INDEX IF NOT EXISTS node_polls_ts ON node_polls(ts);

CREATE TABLE IF NOT EXISTS node_links (
    poll      INTEGER NOT NULL REFERENCES node_polls(id) ON DELETE CASCADE,
    neighbor  TEXT NOT NULL,
    link_type TEXT,
    signal    REAL,
    noise     REAL,
    snr       REAL,
    quality   REAL,
    nquality  REAL,
    rtt       REAL,
    tx_rate   REAL,
    rx_rate   REAL,
    tx_fail   REAL,
    metric    REAL
);
CREATE INDEX IF NOT EXISTS node_links_poll ON node_links(poll);
"#;

/// The hashes a poll's lists are stored under (`lists`, shared with `mesh.list` events).
pub struct ListHashes {
    pub services: String,
    pub hosts: String,
}

/// The hosts list the way polls publish it: sorted, no repeats.
pub fn host_list(obs: &NodeObs) -> Vec<String> {
    let mut hosts = obs.hosts.clone();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Keep a list under its own hash; returns the hash.
fn put_list(tx: &Transaction, list: Value) -> Result<String> {
    let hash = list_hash(&list);
    ridgewarden_client::mesh::store_list(tx, &json!({"hash": hash, "list": list}))?;
    Ok(hash)
}

/// Record a poll that answered: the raw reply and what was parsed from it.
pub fn record_ok(
    tx: &Transaction,
    ts: i64,
    raw: &Value,
    obs: &NodeObs,
    lists: &ListHashes,
) -> Result<()> {
    tx.execute(
        "INSERT INTO snapshots (node, ts, ok, raw) VALUES (?1, ?2, 1, ?3)",
        params![obs.node, ts, raw.to_string()],
    )?;
    insert_parsed(tx, ts, obs, lists)
}

fn insert_parsed(tx: &Transaction, ts: i64, obs: &NodeObs, lists: &ListHashes) -> Result<()> {
    tx.execute(
        "INSERT INTO node_polls (ts, node, ok, api_version, firmware, model, lat, lon, services, hosts)
         VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            ts,
            obs.node,
            obs.api_version,
            obs.firmware,
            obs.model,
            obs.lat,
            obs.lon,
            lists.services,
            lists.hosts
        ],
    )?;
    let poll = tx.last_insert_rowid();
    for l in &obs.links {
        tx.execute(
            "INSERT INTO node_links (poll, neighbor, link_type, signal, noise, snr, quality,
                 nquality, rtt, tx_rate, rx_rate, tx_fail, metric)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                poll,
                l.neighbor,
                l.link_type,
                l.signal,
                l.noise,
                l.snr,
                l.quality,
                l.nquality,
                l.rtt,
                l.tx_rate,
                l.rx_rate,
                l.tx_fail,
                l.metric
            ],
        )?;
    }
    Ok(())
}

/// Record a poll the node didn't answer.
pub fn record_failed(tx: &Transaction, ts: i64, node: &str, error: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO snapshots (node, ts, ok, raw) VALUES (?1, ?2, 0, NULL)",
        params![node, ts],
    )?;
    tx.execute(
        "INSERT INTO node_polls (ts, node, ok, error) VALUES (?1, ?2, 0, ?3)",
        params![ts, node, error],
    )?;
    Ok(())
}

/// Drop raw replies and parsed polls past their keep time.
pub fn prune(tx: &Transaction, now: i64) -> Result<()> {
    tx.execute(
        "DELETE FROM snapshots WHERE ts < ?1",
        params![now - RAW_KEEP_MS],
    )?;
    tx.execute(
        "DELETE FROM node_polls WHERE ts < ?1",
        params![now - PARSED_KEEP_MS],
    )?;
    Ok(())
}

/// When the node was last polled and whether it answered.
pub fn last_poll(conn: &Connection) -> Result<Option<(i64, bool)>> {
    Ok(conn
        .query_row(
            "SELECT ts, ok FROM node_polls ORDER BY ts DESC, id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// The node's name as it last reported it.
pub fn last_name(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT node FROM node_polls WHERE ok = 1 ORDER BY ts DESC, id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

/// The newest answered poll at or before `ts`, rebuilt from the tables.
pub fn seen_by(conn: &Connection, ts: i64) -> Result<Option<(i64, NodeObs)>> {
    load(
        conn,
        "SELECT id FROM node_polls WHERE ok = 1 AND ts <= ?1 ORDER BY ts DESC, id DESC LIMIT 1",
        ts,
    )
}

/// The oldest answered poll still kept.
pub fn first_seen(conn: &Connection) -> Result<Option<(i64, NodeObs)>> {
    load(
        conn,
        "SELECT id FROM node_polls WHERE ok = 1 AND ts > ?1 ORDER BY ts, id LIMIT 1",
        i64::MIN,
    )
}

fn load(conn: &Connection, pick: &str, p: i64) -> Result<Option<(i64, NodeObs)>> {
    let Some(id) = conn
        .query_row(pick, params![p], |r| r.get::<_, i64>(0))
        .optional()?
    else {
        return Ok(None);
    };
    let (ts, mut obs, services, hosts) = conn.query_row(
        "SELECT ts, node, api_version, firmware, model, lat, lon, services, hosts
         FROM node_polls WHERE id = ?1",
        params![id],
        |r| {
            let api_version: Option<String> = r.get(2)?;
            Ok((
                r.get::<_, i64>(0)?,
                NodeObs {
                    node: r.get(1)?,
                    api: aredn::Api::of(api_version.as_deref()),
                    api_version,
                    firmware: r.get(3)?,
                    model: r.get(4)?,
                    lat: r.get(5)?,
                    lon: r.get(6)?,
                    ..Default::default()
                },
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        },
    )?;
    obs.links = links(conn, id)?;
    obs.services = list::<Vec<Service>>(conn, services.as_deref())?.unwrap_or_default();
    obs.hosts = list::<Vec<String>>(conn, hosts.as_deref())?.unwrap_or_default();
    Ok(Some((ts, obs)))
}

fn list<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    hash: Option<&str>,
) -> Result<Option<T>> {
    let Some(hash) = hash else { return Ok(None) };
    let json: Option<String> = conn
        .query_row(
            "SELECT json FROM lists WHERE hash = ?1",
            params![hash],
            |r| r.get(0),
        )
        .optional()?;
    Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
}

/// The links a poll found.
pub fn links(conn: &Connection, poll: i64) -> Result<Vec<LinkObs>> {
    let mut st = conn.prepare(
        "SELECT neighbor, link_type, signal, noise, snr, quality, nquality, rtt, tx_rate,
                rx_rate, tx_fail, metric
         FROM node_links WHERE poll = ?1 ORDER BY neighbor",
    )?;
    let rows = st.query_map(params![poll], |r| {
        Ok(LinkObs {
            neighbor: r.get(0)?,
            ip: None,
            link_type: r.get(1)?,
            signal: r.get(2)?,
            noise: r.get(3)?,
            snr: r.get(4)?,
            quality: r.get(5)?,
            nquality: r.get(6)?,
            rtt: r.get(7)?,
            tx_rate: r.get(8)?,
            rx_rate: r.get(9)?,
            tx_fail: r.get(10)?,
            metric: r.get(11)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Parse raw replies kept from before the parsed tables existed, so "what changed" keeps
/// its history across the upgrade.
pub fn backfill(conn: &mut Connection) -> Result<usize> {
    // the write lock first, so a CLI command opening the database meanwhile waits and then
    // finds the work done, instead of doing it a second time
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let parsed: bool = tx.query_row("SELECT count(*) > 0 FROM node_polls", [], |r| r.get(0))?;
    if parsed {
        return Ok(0);
    }
    let mut n = 0;
    // a week of replies from a big mesh is too much to hold at once: one row at a time,
    // in id order, which needs no sort
    let mut st = tx.prepare("SELECT ts, node, ok, raw FROM snapshots ORDER BY id")?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let (ts, node, ok, raw): (i64, String, bool, Option<String>) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        if !ok {
            tx.execute(
                "INSERT INTO node_polls (ts, node, ok, error) VALUES (?1, ?2, 0, 'did not answer')",
                params![ts, node],
            )?;
            n += 1;
            continue;
        }
        let Some(obs) = raw
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| aredn::parse(&v).ok())
        else {
            continue;
        };
        let lists = ListHashes {
            services: put_list(&tx, serde_json::to_value(&obs.services)?)?,
            hosts: put_list(&tx, serde_json::to_value(host_list(&obs))?)?,
        };
        insert_parsed(&tx, ts, &obs, &lists)?;
        n += 1;
    }
    drop(rows);
    drop(st);
    tx.commit()?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn poll(tx: &Transaction, ts: i64) {
        let obs = NodeObs {
            node: "tst-paso".into(),
            links: vec![LinkObs {
                neighbor: "tst-north".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let lists = ListHashes {
            services: "s".into(),
            hosts: "h".into(),
        };
        record_ok(tx, ts, &json!({"node": "TST-PASO"}), &obs, &lists).unwrap();
    }

    #[test]
    fn raw_replies_go_before_parsed_polls() {
        let s = Store::open_in_memory().unwrap();
        let mut c = s.conn();
        let now = 100 * DAY_MS;
        let tx = c.transaction().unwrap();
        for days in [9, 3, 1] {
            poll(&tx, now - days * DAY_MS);
        }
        prune(&tx, now).unwrap();
        tx.commit().unwrap();
        let count = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count("SELECT count(*) FROM snapshots"), 1, "raw: 2 days");
        assert_eq!(
            count("SELECT count(*) FROM node_polls"),
            2,
            "parsed: 8 days"
        );
        assert_eq!(
            count("SELECT count(*) FROM node_links"),
            2,
            "a dropped poll takes its links with it"
        );
    }
}
