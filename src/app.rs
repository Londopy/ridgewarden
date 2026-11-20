//! The running agent: shared state plus the node poller and peer discovery.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::aredn::{self, NodeObs, SYSINFO_QUERY, Service};
use crate::config::Config;
use crate::event::Event;
use crate::event::list_hash;
use crate::log::Log;
use crate::util::{norm_node, now_ms, valid_node};

/// Snapshots are kept long enough for the daily "what changed" diff and a bit of history.
const SNAPSHOT_KEEP_MS: i64 = 8 * 86_400_000;

/// Service names that mark another ridgewarden agent.
pub const SERVICE_PREFIX: &str = "ridgewarden";
/// Adverts checked per poll, and how long a failed one is left alone.
const MAX_ADVERTS_PER_POLL: usize = 8;
const ADVERT_RETRY_MS: i64 = 3_600_000;
/// Peers learned from adverts, at most; configured and joined peers don't count.
const MAX_DISCOVERED: usize = 32;

#[derive(Clone, Debug)]
pub struct Peer {
    pub url: String,
    pub call: Option<String>,
    pub key: Option<String>,
    pub last_ok: Option<i64>,
    pub last_err: Option<String>,
    pub from_config: bool,
}

pub struct App {
    pub cfg: Config,
    pub log: Log,
    pub http: reqwest::Client,
    /// for blob chunks: a slow RF link needs longer than the API timeout for 1 MiB
    pub blob_http: reqwest::Client,
    pub blobs: crate::blobs::Store,
    pub peers: Mutex<BTreeMap<String, Peer>>,
    /// the node's name as it last reported it
    pub node_name: Mutex<Option<String>>,
    /// adverts that didn't verify, and when; retried after a while, not every poll
    failed_adverts: Mutex<BTreeMap<String, i64>>,
}

impl App {
    pub fn new(cfg: Config, log: Log) -> Result<Self> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(cfg.node.timeout_s))
            .user_agent(concat!("ridgewarden/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let joined = crate::trust::local_peers(&log)?;
        let peers = cfg
            .sync
            .peers
            .iter()
            .chain(&joined)
            .map(|u| {
                let url = crate::blobs::peer_url(u)
                    .unwrap_or_else(|| u.trim_end_matches('/').to_string());
                (
                    url.clone(),
                    Peer {
                        url,
                        call: None,
                        key: None,
                        last_ok: None,
                        last_err: None,
                        from_config: true,
                    },
                )
            })
            .collect();
        let node_name = {
            let c = log.store.conn();
            c.query_row(
                "SELECT node FROM snapshots WHERE ok = 1 ORDER BY ts DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?
        };
        Ok(Self {
            blobs: crate::blobs::Store::new(cfg.blob_dir())?,
            blob_http: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(20))
                .timeout(Duration::from_secs(300))
                // a peer's chunk route never redirects; following one would let a peer
                // aim this core at any address it can reach
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            cfg,
            log,
            http,
            peers: Mutex::new(peers),
            node_name: Mutex::new(node_name),
            failed_adverts: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn node_name(&self) -> String {
        self.node_name.lock().unwrap().clone().unwrap_or_else(|| {
            // before the first good poll, fall back to the host part of the node URL
            let u = self
                .cfg
                .node
                .url
                .trim_start_matches("http://")
                .trim_start_matches("https://");
            norm_node(u.split(['/', ':']).next().unwrap_or("unknown"))
        })
    }

    async fn fetch(&self, url: &str) -> Result<Value> {
        let r = self.http.get(url).send().await?.error_for_status()?;
        Ok(r.json::<Value>().await?)
    }

    /// Hash a list and say whether peers already have it. Lists (the mesh's services and
    /// hosts) rarely change, so they ride along in an event only when they do; every poll
    /// carries the hash so readers can look the list up.
    /// The hash of a service or host list, publishing the list itself (once, in the main
    /// chain, which is never pruned) the first time it is seen. Polls carry only the hash.
    fn list_ref<T: serde::Serialize>(&self, list: &T) -> Result<String> {
        let v = serde_json::to_value(list)?;
        let hash = list_hash(&v);
        let known: bool = self.log.store.conn().query_row(
            "SELECT count(*) > 0 FROM events WHERE kind = 'mesh.list' AND author_key = ?1
             AND json_extract(json, '$.payload.hash') = ?2",
            params![self.log.id.author.key, hash],
            |r| r.get(0),
        )?;
        if !known {
            self.log
                .append("mesh.list", json!({"hash": hash, "list": v}))?;
        }
        Ok(hash)
    }

    fn poll_payload(&self, obs: &NodeObs, via: &str, full: bool) -> Result<Value> {
        let mut payload = json!({
            "node": obs.node,
            "ok": true,
            "via": via,
            "api_version": obs.api_version,
            "firmware": obs.firmware,
            "model": obs.model,
            "lat": obs.lat,
            "lon": obs.lon,
            "links": obs.links,
            // with no trackers, or with some skipped as stale, the link list is partial
            "trackers": obs.trackers,
            "stale_skipped": obs.stale_skipped,
        });
        if full {
            payload["ads_hash"] = json!(self.list_ref(&obs.services)?);
            let mut hosts = obs.hosts.clone();
            hosts.sort();
            hosts.dedup();
            payload["hosts_hash"] = json!(self.list_ref(&hosts)?);
            payload["local_services"] = json!(
                obs.services_local
                    .iter()
                    .map(|s| &s.name)
                    .collect::<Vec<_>>()
            );
        }
        Ok(payload)
    }

    /// Fetch sysinfo from our node, keep the raw snapshot, and log a `node.poll` event.
    pub async fn poll_once(&self) -> Result<Event> {
        let url = format!(
            "{}/a/sysinfo?{}",
            self.cfg.node.url.trim_end_matches('/'),
            SYSINFO_QUERY
        );
        let now = now_ms();
        let v = match self.fetch(&url).await {
            Ok(v) => v,
            Err(e) => {
                let node = self.node_name();
                self.log.store.conn().execute(
                    "INSERT INTO snapshots (node, ts, ok, raw) VALUES (?1, ?2, 0, NULL)",
                    params![node, now],
                )?;
                tracing::warn!("poll of {url} failed: {e:#}");
                return self.log.append_telemetry(
                    "node.poll",
                    json!({"node": node, "ok": false, "via": "lan", "error": short_err(&e)}),
                );
            }
        };
        let obs = aredn::parse(&v).context("parsing sysinfo")?;
        *self.node_name.lock().unwrap() = Some(obs.node.clone());
        {
            let c = self.log.store.conn();
            c.execute(
                "INSERT INTO snapshots (node, ts, ok, raw) VALUES (?1, ?2, 1, ?3)",
                params![obs.node, now, v.to_string()],
            )?;
            c.execute(
                "DELETE FROM snapshots WHERE ts < ?1",
                params![now - SNAPSHOT_KEEP_MS],
            )?;
        }
        let ev = self
            .log
            .append_telemetry("node.poll", self.poll_payload(&obs, "lan", true)?)?;
        if self.cfg.sync.discover {
            self.discover(&obs.services).await;
        }
        Ok(ev)
    }

    /// Poll another node across the mesh for its links (spec 03/07 crawl). Only what the
    /// link analysis needs is asked for, to keep the load on other owners' nodes small.
    pub async fn crawl_node(&self, node: &str) -> Result<Event> {
        // names come from other nodes' neighbor lists; only a plain node name may go into a URL
        let node = norm_node(node);
        anyhow::ensure!(valid_node(&node), "not a node name: {node:?}");
        let node = node.as_str();
        let base = self.cfg.crawl.url_template.replace("{node}", node);
        let url = format!("{}/a/sysinfo?link_info=1&lqm=1", base.trim_end_matches('/'));
        match self.fetch(&url).await {
            Ok(v) => {
                let obs = aredn::parse(&v).context("parsing sysinfo")?;
                self.log.append_telemetry("node.poll", self.poll_payload(&obs, "crawl", false)?)
            }
            Err(e) => self.log.append_telemetry("node.poll",
                json!({"node": norm_node(node), "ok": false, "via": "crawl", "error": short_err(&e)}),
            ),
        }
    }

    /// Find other agents from their service adverts, then check each one's signed identity.
    /// An advert alone proves nothing (AREDN service lists can be stale or wrong).
    async fn discover(&self, services: &[Service]) {
        let me = self.log.id.author.key.clone();
        let now = now_ms();
        let mut candidates: Vec<String> = services
            .iter()
            .filter(|s| s.name.to_ascii_lowercase().starts_with(SERVICE_PREFIX))
            .filter_map(|s| base_url(&s.link).and_then(|u| crate::blobs::peer_url(&u)))
            .collect();
        candidates.sort();
        candidates.dedup();
        {
            let peers = self.peers.lock().unwrap();
            let failed = self.failed_adverts.lock().unwrap();
            candidates.retain(|b| {
                !peers.get(b).is_some_and(|p| p.key.is_some())
                    && !failed.get(b).is_some_and(|t| now - t < ADVERT_RETRY_MS)
            });
        }
        // a few at a time, briefly, all at once: a bad advert must not stall the poller
        let mut set = tokio::task::JoinSet::new();
        for base in candidates.into_iter().take(MAX_ADVERTS_PER_POLL) {
            let http = self.http.clone();
            set.spawn(async move {
                let r =
                    tokio::time::timeout(Duration::from_secs(3), verify_peer(&http, &base)).await;
                (
                    base,
                    r.map_err(|_| anyhow::anyhow!("timed out")).and_then(|r| r),
                )
            });
        }
        while let Some(Ok((base, r))) = set.join_next().await {
            match r {
                Ok(bind) if bind.author.key == me => {}
                Ok(bind) => {
                    let discovered = self
                        .peers
                        .lock()
                        .unwrap()
                        .values()
                        .filter(|p| !p.from_config)
                        .count();
                    if discovered >= MAX_DISCOVERED {
                        tracing::debug!("not adding {base}: already {discovered} discovered peers");
                        continue;
                    }
                    tracing::info!("discovered peer {} at {base}", bind.author.call);
                    self.peers.lock().unwrap().insert(
                        base.clone(),
                        Peer {
                            url: base,
                            call: Some(bind.author.call),
                            key: Some(bind.author.key),
                            last_ok: None,
                            last_err: None,
                            from_config: false,
                        },
                    );
                }
                Err(e) => {
                    tracing::debug!("advert {base} did not verify: {e:#}");
                    self.failed_adverts.lock().unwrap().insert(base, now);
                }
            }
        }
    }
}

/// Fetch a peer's `identity.bind` event and check it is genuine.
pub async fn verify_peer(http: &reqwest::Client, base: &str) -> Result<Event> {
    let bind: Event = http
        .get(format!("{base}/v1/identity"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    bind.verify()?;
    anyhow::ensure!(
        bind.kind == "identity.bind" && bind.seq == 0,
        "not an identity event"
    );
    Ok(bind)
}

/// "http://host:port/anything" -> "http://host:port"
pub fn base_url(link: &str) -> Option<String> {
    let (scheme, rest) = link.split_once("://")?;
    if scheme != "http" {
        return None;
    }
    let host = rest.split('/').next()?;
    (!host.is_empty()).then(|| format!("http://{host}"))
}

pub fn short_err(e: &anyhow::Error) -> String {
    if let Some(r) = e.downcast_ref::<reqwest::Error>() {
        if r.is_timeout() {
            return "timed out".into();
        }
        if r.is_connect() {
            return "could not connect".into();
        }
        if let Some(s) = r.status() {
            return format!("HTTP {}", s.as_u16());
        }
    }
    let s = format!("{e:#}");
    s.chars().take(160).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls() {
        assert_eq!(
            base_url("http://10.1.2.3:8790/x/y").as_deref(),
            Some("http://10.1.2.3:8790")
        );
        assert_eq!(base_url("https://a/"), None);
        assert_eq!(base_url(""), None);
    }
}
