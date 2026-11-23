//! The running agent: shared state plus the node poller and peer discovery.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::params;
use serde_json::{Value, json};

use crate::aredn::{self, NodeObs, SYSINFO_QUERY, Service};
use crate::config::Config;
use crate::event::Event;
use crate::event::list_hash;
use crate::log::{Log, MAX_EVENT_BYTES};
use crate::node;
use crate::util::{norm_node, now_ms, valid_node};

/// Service names that mark another ridgewarden agent.
pub const SERVICE_PREFIX: &str = "ridgewarden";
/// Adverts checked per poll, and how long a failed one is left alone.
const MAX_ADVERTS_PER_POLL: usize = 8;
const ADVERT_RETRY_MS: i64 = 3_600_000;
/// Peers learned from adverts, at most; configured and joined peers don't count.
const MAX_DISCOVERED: usize = 32;
/// Where nodes serve sysinfo: 4.x at /a/sysinfo, 3.x only at /cgi-bin/sysinfo.json (which
/// 4.x redirects to /a/sysinfo).
const SYSINFO_PATHS: [&str; 2] = ["/a/sysinfo", "/cgi-bin/sysinfo.json"];
/// Biggest sysinfo reply read. A mesh of 3,000 nodes answers in about 1 MB.
const MAX_SYSINFO: usize = 8 << 20;

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
    /// which of SYSINFO_PATHS last answered, per node base URL
    sysinfo_path: Mutex<BTreeMap<String, usize>>,
    /// said once already that the node's sysinfo version is one we don't know
    warned_api: AtomicBool,
    /// said once already that a list is too big to share
    warned_list: AtomicBool,
}

impl App {
    pub fn new(cfg: Config, log: Log) -> Result<Self> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(cfg.node.timeout_s))
            .user_agent(concat!("ridgewarden/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::custom(same_host))
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
        let node_name = node::last_name(&log.store.conn())?;
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
            sysinfo_path: Mutex::new(BTreeMap::new()),
            warned_api: AtomicBool::new(false),
            warned_list: AtomicBool::new(false),
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
        crate::sync::get_json(self.http.get(url), MAX_SYSINFO).await
    }

    /// A node's sysinfo, from whichever path its firmware serves it on.
    async fn sysinfo(&self, base: &str, query: &str) -> Result<Value> {
        let base = base.trim_end_matches('/');
        let url = |i: usize| format!("{base}{}?{query}", SYSINFO_PATHS[i]);
        let first = self
            .sysinfo_path
            .lock()
            .unwrap()
            .get(base)
            .copied()
            .unwrap_or(0);
        match self.fetch(&url(first)).await {
            Ok(v) => return Ok(v),
            Err(e) if !wrong_path(&e) => return Err(e),
            Err(_) => {}
        }
        let v = self.fetch(&url(1 - first)).await?;
        self.sysinfo_path
            .lock()
            .unwrap()
            .insert(base.to_string(), 1 - first);
        Ok(v)
    }

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
        if known {
            return Ok(hash);
        }
        let payload = json!({"hash": hash, "list": v});
        // the rest of the event needs well under 4 KB
        let size = payload.to_string().len();
        if size <= MAX_EVENT_BYTES - 4096 {
            self.log.append("mesh.list", payload)?;
            return Ok(hash);
        }
        // too big for one event: keep it for this site's own pages rather than fail the poll
        if !self.warned_list.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "the node's service or host list is {size} bytes, more than one event holds; this site keeps it, but other sites won't get it"
            );
        }
        let mut c = self.log.store.conn();
        let tx = c.transaction()?;
        ridgewarden_client::mesh::store_list(&tx, &payload)?;
        tx.commit()?;
        Ok(hash)
    }

    /// A `node.poll` payload; `lists` only for this site's own node, whose lists are shared.
    fn poll_payload(obs: &NodeObs, via: &str, lists: Option<&node::ListHashes>) -> Value {
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
        if let Some(l) = lists {
            payload["ads_hash"] = json!(l.services);
            payload["hosts_hash"] = json!(l.hosts);
            payload["local_services"] = json!(
                obs.services_local
                    .iter()
                    .map(|s| &s.name)
                    .collect::<Vec<_>>()
            );
        }
        payload
    }

    /// Run `f` in one transaction on the core's database.
    fn in_tx(&self, f: impl FnOnce(&rusqlite::Transaction) -> Result<()>) -> Result<()> {
        let mut c = self.log.store.conn();
        let tx = c.transaction()?;
        f(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Fetch sysinfo from our node, keep it raw and parsed, and log a `node.poll` event. A
    /// poll that goes wrong anywhere is logged as one the node didn't answer, with why.
    pub async fn poll_once(&self) -> Result<Event> {
        let now = now_ms();
        let e = match self.poll_node(now).await {
            Ok(ev) => return Ok(ev),
            Err(e) => e,
        };
        let node = self.node_name();
        let error = short_err(&e);
        self.in_tx(|tx| {
            node::record_failed(tx, now, &node, &error)?;
            node::prune(tx, now)
        })?;
        tracing::warn!("poll of {} failed: {e:#}", self.cfg.node.url);
        self.log.append_telemetry(
            "node.poll",
            json!({"node": node, "ok": false, "via": "lan", "error": error}),
        )
    }

    async fn poll_node(&self, now: i64) -> Result<Event> {
        let v = self.sysinfo(&self.cfg.node.url, SYSINFO_QUERY).await?;
        let obs = aredn::parse(&v).context("parsing sysinfo")?;
        if obs.api == aredn::Api::Unknown && !self.warned_api.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "{} reports sysinfo api_version {:?}, which this version of ridgewarden doesn't know; reading the fields of both 3.x and 4.x",
                obs.node,
                obs.api_version.as_deref().unwrap_or("(none)")
            );
        }
        *self.node_name.lock().unwrap() = Some(obs.node.clone());
        let lists = node::ListHashes {
            services: self.list_ref(&obs.services)?,
            hosts: self.list_ref(&node::host_list(&obs))?,
        };
        self.in_tx(|tx| {
            node::record_ok(tx, now, &v, &obs, &lists)?;
            node::prune(tx, now)
        })?;
        let ev = self
            .log
            .append_telemetry("node.poll", Self::poll_payload(&obs, "lan", Some(&lists)))?;
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
        let polled = async {
            let v = self.sysinfo(&base, "link_info=1&lqm=1").await?;
            aredn::parse(&v).context("parsing sysinfo")
        };
        match polled.await {
            Ok(obs) => self
                .log
                .append_telemetry("node.poll", Self::poll_payload(&obs, "crawl", None)),
            Err(e) => self.log.append_telemetry(
                "node.poll",
                json!({"node": node, "ok": false, "via": "crawl", "error": short_err(&e)}),
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

/// 4.x answers the old sysinfo path with a redirect to the new one. Polls, crawls and
/// sync share this client: it follows a redirect at most twice, and never to another host.
fn same_host(a: reqwest::redirect::Attempt) -> reqwest::redirect::Action {
    let same = a.previous().first().is_some_and(|u| {
        u.host_str() == a.url().host_str()
            && u.port_or_known_default() == a.url().port_or_known_default()
    });
    if same && a.previous().len() < 3 {
        a.follow()
    } else {
        a.stop()
    }
}

/// The node answered, but not with sysinfo: 404, or (3.x asked for /a/sysinfo) the HTML
/// page its web UI shows for any page it doesn't have.
fn wrong_path(e: &anyhow::Error) -> bool {
    e.downcast_ref::<serde_json::Error>().is_some()
        || e.downcast_ref::<reqwest::Error>()
            .is_some_and(|r| r.status() == Some(reqwest::StatusCode::NOT_FOUND))
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
