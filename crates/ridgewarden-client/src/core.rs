//! Talking to the core agent's local API, and following its event feed.
//!
//!   GET  /v1/feed?after=<n>&limit=<m>  every event the core holds, in the order it got them
//!   GET  /v1/whoami                    the core's callsign, key and node
//!   POST /local/v1/append              ask the core to sign and log an event (needs the token)
//!   GET  /v1/blobs?root=<id>           what the core knows about one blob
//!   POST /local/v1/blob/...            want, unwant, export and add blobs (needs the token)

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::event::Event;
use crate::util::rfc3339_to_ms;

pub const FEED_PAGE: usize = 500;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FeedItem {
    /// the core's local arrival order; only meaningful together with `instance`
    pub seq: i64,
    pub event: Event,
    /// whether the core trusts this event's author (its own key, or one pinned in its
    /// config) and the key hasn't been revoked; apps only build views from trusted events
    #[serde(default)]
    pub trusted: bool,
    /// when the core first saw the event, by its own clock
    #[serde(default)]
    pub seen_ms: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Feed {
    /// random id of the core's database; if it changes, the core was reset and
    /// projections have to be rebuilt from scratch
    pub instance: String,
    pub events: Vec<FeedItem>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Whoami {
    pub call: String,
    pub key: String,
    pub node: String,
}

/// What the core knows about one blob (`GET /v1/blobs?root=`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BlobStatus {
    pub root: String,
    pub name: String,
    pub size: u64,
    /// every chunk is on the core's disk
    pub complete: bool,
    /// the core is fetching it, or keeping it once complete
    pub wanted: bool,
    pub priority: u8,
    /// the station whose manifest the core fetches by
    pub author: String,
    /// a station the core trusts published a manifest for it
    pub trusted: bool,
}

#[derive(Clone)]
pub struct CoreClient {
    base: String,
    http: reqwest::Client,
    token: Option<String>,
}

impl CoreClient {
    /// `token_file` is the core's `local.token`; only needed by apps that write events.
    pub fn new(base: &str, token_file: Option<&Path>) -> Result<Self> {
        let token = match token_file {
            Some(p) => Some(
                std::fs::read_to_string(p)
                    .with_context(|| format!("reading core token {}", p.display()))?
                    .trim()
                    .to_string(),
            ),
            None => None,
        };
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            token,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub async fn feed(&self, after: i64, limit: usize) -> Result<Feed> {
        Ok(self
            .http
            .get(format!("{}/v1/feed", self.base))
            .query(&[("after", after.to_string()), ("limit", limit.to_string())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn whoami(&self) -> Result<Whoami> {
        Ok(self
            .http
            .get(format!("{}/v1/whoami", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Ask the core to fetch a blob from other sites.
    pub async fn blob_want(&self, root: &str) -> Result<()> {
        self.local_post("/local/v1/blob/want", json!({"root": root}))
            .await?;
        Ok(())
    }

    /// Like `blob_want`, but ask these cores (base URLs like "http://10.1.2.3:8790") for
    /// the chunks first, in this order. Other peers are asked only for chunks these don't
    /// have, or after one fails; a failed peer sits out the rest of that fetch round (about
    /// a minute). The list replaces any earlier one; an empty list clears it.
    pub async fn blob_want_from(&self, root: &str, peers: &[String]) -> Result<()> {
        self.local_post("/local/v1/blob/want", json!({"root": root, "peers": peers}))
            .await?;
        Ok(())
    }

    /// Like `blob_want_from`, but never ask any other peer: chunks these don't have wait
    /// for a later round. For fan-out where each link should carry a chunk once.
    pub async fn blob_want_only_from(&self, root: &str, peers: &[String]) -> Result<()> {
        self.local_post(
            "/local/v1/blob/want",
            json!({"root": root, "peers": peers, "only": true}),
        )
        .await?;
        Ok(())
    }

    /// The core's state for one blob; None until its manifest has reached the core.
    pub async fn blob_status(&self, root: &str) -> Result<Option<BlobStatus>> {
        let r = self
            .http
            .get(format!("{}/v1/blobs", self.base))
            .query(&[("root", root)])
            .send()
            .await?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(r.error_for_status()?.json().await?))
    }

    /// Stop fetching or keeping a blob. The core deletes its chunks, except any another
    /// blob it still wants or holds shares. Returns the bytes freed.
    pub async fn blob_unwant(&self, root: &str) -> Result<u64> {
        let v = self
            .local_post_slow("/local/v1/blob/unwant", json!({"root": root}))
            .await?;
        Ok(v["freed"].as_u64().unwrap_or(0))
    }

    /// Have the core write a complete blob into its export folder as `name`; returns the
    /// file's path. A big file takes minutes, since every chunk is checked again.
    pub async fn blob_export(&self, root: &str, name: &str) -> Result<String> {
        let v = self
            .local_post_slow("/local/v1/blob/export", json!({"root": root, "name": name}))
            .await?;
        Ok(v["exported"].as_str().unwrap_or_default().to_string())
    }

    /// Publish `file`, a plain file name in the core's import folder (`<data_dir>/imports`),
    /// to the mesh as blob `name`. Sites fetch priority 0 first; most files are 5. Returns
    /// the blob's root.
    pub async fn blob_add(&self, file: &str, name: &str, priority: u8) -> Result<String> {
        let v = self
            .local_post_slow(
                "/local/v1/blob/add",
                json!({"file": file, "name": name, "priority": priority}),
            )
            .await?;
        Ok(v["root"].as_str().unwrap_or_default().to_string())
    }

    /// For work proportional to a file's size: hashing gigabytes on a Pi takes a while.
    async fn local_post_slow(&self, path: &str, body: Value) -> Result<Value> {
        self.post(path, body, Duration::from_secs(3 * 3600)).await
    }

    async fn local_post(&self, path: &str, body: Value) -> Result<Value> {
        self.post(path, body, Duration::from_secs(30)).await
    }

    async fn post(&self, path: &str, body: Value, timeout: Duration) -> Result<Value> {
        let Some(token) = &self.token else {
            bail!("no core token configured")
        };
        let r = self
            .http
            .post(format!("{}{path}", self.base))
            .timeout(timeout)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?;
        // the core says why it refused; pass that on rather than just the status
        if !r.status().is_success() {
            let status = r.status();
            let why = r.text().await.unwrap_or_default();
            bail!("core refused {path} ({status}): {}", why.trim());
        }
        Ok(r.json().await?)
    }

    /// Have the core sign `payload` under its key and add it to its main log.
    pub async fn append(&self, kind: &str, payload: Value) -> Result<Event> {
        self.append_to(kind, payload, false).await
    }

    /// Like `append`, but into the station's telemetry log, which is pruned after a while.
    /// Use it for frequent measurements (probe rounds), not for anything people need later.
    pub async fn append_telemetry(&self, kind: &str, payload: Value) -> Result<Event> {
        self.append_to(kind, payload, true).await
    }

    async fn append_to(&self, kind: &str, payload: Value, telemetry: bool) -> Result<Event> {
        let Some(token) = &self.token else {
            bail!("no core token configured; this app can't write events")
        };
        Ok(self
            .http
            .post(format!("{}/local/v1/append", self.base))
            .bearer_auth(token)
            .json(&json!({"kind": kind, "payload": payload, "telemetry": telemetry}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}

/// A view an app builds from the event feed. `apply` sees every event once, in order,
/// inside a transaction; `reset` clears the view when the core's database changes.
pub trait Projection: Send + Sync {
    fn schema(&self) -> &'static str;
    fn reset(&self, tx: &Transaction) -> Result<()>;
    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()>;
}

const CURSOR_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS feed_cursor (
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    instance TEXT NOT NULL,
    seq      INTEGER NOT NULL
);";

/// Open an app's own database (WAL, power-cut safe) and create the projections' tables.
pub fn open_db(path: &Path, projections: &[&dyn Projection]) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    init_db(&conn, projections)?;
    Ok(conn)
}

pub fn init_db(conn: &Connection, projections: &[&dyn Projection]) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(CURSOR_SCHEMA)?;
    for p in projections {
        conn.execute_batch(p.schema())?;
    }
    Ok(())
}

/// Apply one page of feed items; returns how many were new (trusted or not). Split out
/// from `follow` so it can be tested without HTTP.
pub fn apply_page(
    conn: &mut Connection,
    feed: &Feed,
    projections: &[&dyn Projection],
) -> Result<usize> {
    let tx = conn.transaction()?;
    let cur: Option<(String, i64)> = tx
        .query_row(
            "SELECT instance, seq FROM feed_cursor WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if cur.as_ref().is_some_and(|(i, _)| i != &feed.instance) {
        for p in projections {
            p.reset(&tx)?;
        }
    }
    let mut last = match &cur {
        Some((i, s)) if i == &feed.instance => *s,
        _ => -1,
    };
    let mut n = 0;
    for item in &feed.events {
        if item.seq <= last {
            continue;
        }
        last = item.seq;
        n += 1;
        if !item.trusted {
            continue;
        }
        // an author's clock can be wrong; never let an event claim to be from later than
        // when our core actually received it
        let claimed = rfc3339_to_ms(&item.event.wall_time).unwrap_or(0);
        let ts = item.seen_ms.map_or(claimed, |seen| claimed.min(seen));
        for p in projections {
            p.apply(&tx, &item.event, ts)?;
        }
    }
    tx.execute(
        "INSERT INTO feed_cursor (id, instance, seq) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET instance = excluded.instance, seq = excluded.seq",
        params![feed.instance, last],
    )?;
    tx.commit()?;
    Ok(n)
}

fn cursor(conn: &Connection) -> Result<(Option<String>, i64)> {
    Ok(conn
        .query_row(
            "SELECT instance, seq FROM feed_cursor WHERE id = 1",
            [],
            |r| Ok((Some(r.get(0)?), r.get(1)?)),
        )
        .optional()?
        .unwrap_or((None, -1)))
}

/// Pull everything new from the core's feed into the app's projections.
/// Returns how many events were applied.
pub async fn follow(
    core: &CoreClient,
    db: &Mutex<Connection>,
    projections: &[&dyn Projection],
) -> Result<usize> {
    let mut total = 0;
    loop {
        let (instance, after) = cursor(&db.lock().unwrap_or_else(|e| e.into_inner()))?;
        let mut feed = core.feed(after, FEED_PAGE).await?;
        if instance.as_deref().is_some_and(|i| i != feed.instance) {
            // the core was reset: start again from its first event
            feed = core.feed(-1, FEED_PAGE).await?;
        }
        let full = feed.events.len() >= FEED_PAGE;
        let n = apply_page(
            &mut db.lock().unwrap_or_else(|e| e.into_inner()),
            &feed,
            projections,
        )?;
        total += n;
        if !full || n == 0 {
            return Ok(total);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Author, Draft};
    use ed25519_dalek::SigningKey;

    struct Count;
    impl Projection for Count {
        fn schema(&self) -> &'static str {
            "CREATE TABLE IF NOT EXISTS seen (id TEXT)"
        }
        fn reset(&self, tx: &Transaction) -> Result<()> {
            tx.execute("DELETE FROM seen", [])?;
            Ok(())
        }
        fn apply(&self, tx: &Transaction, e: &Event, _ts: i64) -> Result<()> {
            tx.execute("INSERT INTO seen (id) VALUES (?1)", [&e.id])?;
            Ok(())
        }
    }

    fn ev(seq: u64) -> Event {
        let k = SigningKey::from_bytes(&[3; 32]);
        Draft {
            author: Author {
                call: "N0CALL".into(),
                key: hex::encode(k.verifying_key().to_bytes()),
            },
            seq,
            prev: None,
            lamport: seq,
            wall_time: "2026-11-19T00:00:00Z".into(),
            mono_time: 0,
            kind: "x".into(),
            payload: json!({}),
            log: None,
        }
        .sign(&k)
    }

    fn feed(instance: &str, seqs: &[i64]) -> Feed {
        Feed {
            instance: instance.into(),
            events: seqs
                .iter()
                .map(|s| FeedItem {
                    seq: *s,
                    event: ev(*s as u64),
                    trusted: true,
                    seen_ms: None,
                })
                .collect(),
        }
    }

    #[test]
    fn untrusted_events_are_skipped_but_advance_the_cursor() {
        let mut c = Connection::open_in_memory().unwrap();
        init_db(&c, &[&Count]).unwrap();
        let mut f = feed("a", &[1, 2]);
        f.events[0].trusted = false;
        assert_eq!(apply_page(&mut c, &f, &[&Count]).unwrap(), 2);
        assert_eq!(seen(&c), 1);
        assert_eq!(cursor(&c).unwrap(), (Some("a".into()), 2));
    }

    fn seen(c: &Connection) -> i64 {
        c.query_row("SELECT count(*) FROM seen", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn applies_each_event_once_and_resets_on_new_instance() {
        let mut c = Connection::open_in_memory().unwrap();
        init_db(&c, &[&Count]).unwrap();
        assert_eq!(
            apply_page(&mut c, &feed("a", &[1, 2, 3]), &[&Count]).unwrap(),
            3
        );
        assert_eq!(
            apply_page(&mut c, &feed("a", &[2, 3, 4]), &[&Count]).unwrap(),
            1
        );
        assert_eq!(seen(&c), 4);
        assert_eq!(cursor(&c).unwrap(), (Some("a".into()), 4));
        // core database replaced: the view starts over
        assert_eq!(apply_page(&mut c, &feed("b", &[1]), &[&Count]).unwrap(), 1);
        assert_eq!(seen(&c), 1);
    }
}
