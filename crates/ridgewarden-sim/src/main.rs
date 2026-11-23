//! ridgewarden-sim: a fake AREDN mesh on one port.
//!
//! every simulated node answers `GET /n/<node>/a/sysinfo` with JSON shaped like the
//! firmware's `files/app/main/sysinfo.ut` (api_version 2.0), and redirects the old
//! `/cgi-bin/sysinfo.json` there, as 4.x does. nodes with a 3.x firmware string answer the
//! way 3.25.8.0 does instead: `/cgi-bin/sysinfo.json` (api_version 1.14, OLSR `link_info`,
//! services by address) and an HTML page at `/a/sysinfo`. services live at
//! `/svc/<node>/<name>`.
//!
//! reachability is computed from the live link graph: a probe from a vantage only
//! succeeds if the vantage's node can still reach the service's node. the vantage is
//! picked from the probe's source address (`[[agent]] source_ip`), the same way a real
//! probe's reachability depends on which site it leaves from.
//!
//! control endpoints under `/_ctl/` let tests and demos cut links, kill nodes, stop
//! services and unpublish adverts while the agents run.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Parser)]
#[command(version, about = "Fake AREDN mesh for testing ridgewarden")]
struct Args {
    /// scenario file (TOML)
    scenario: PathBuf,
    /// address to listen on
    #[arg(long, default_value = "127.0.0.1:9100")]
    listen: SocketAddr,
    /// base URL used in advertised service links (defaults to http://<listen>)
    #[arg(long)]
    public_url: Option<String>,
}

#[derive(Deserialize, Clone)]
struct Scenario {
    #[serde(default = "default_seed")]
    seed: u64,
    #[serde(default, rename = "node")]
    nodes: Vec<NodeSpec>,
    #[serde(default, rename = "link")]
    links: Vec<LinkSpec>,
    #[serde(default, rename = "agent")]
    agents: Vec<AgentSpec>,
}

fn default_seed() -> u64 {
    1
}

#[derive(Deserialize, Clone)]
struct NodeSpec {
    name: String,
    ip: String,
    #[serde(default)]
    lat: Option<f64>,
    #[serde(default)]
    lon: Option<f64>,
    #[serde(default = "default_fw")]
    firmware: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, rename = "service")]
    services: Vec<ServiceSpec>,
}

fn default_fw() -> String {
    "4.26.7.0".into()
}

#[derive(Deserialize, Clone)]
struct ServiceSpec {
    name: String,
    /// "http" (default) or "tcp"; tcp services are advertised with a non-http link
    #[serde(default = "default_kind")]
    kind: String,
}

fn default_kind() -> String {
    "http".into()
}

#[derive(Deserialize, Clone)]
struct LinkSpec {
    a: String,
    b: String,
    #[serde(default = "default_link_type", rename = "type")]
    link_type: String,
    #[serde(default = "default_snr")]
    snr: f64,
    /// dB lost per real day since the sim started; models a drifting dish
    #[serde(default)]
    drift_db_per_day: f64,
    #[serde(default = "default_quality")]
    quality: f64,
}

fn default_link_type() -> String {
    "RF".into()
}
fn default_snr() -> f64 {
    25.0
}
fn default_quality() -> f64 {
    100.0
}

#[derive(Deserialize, Clone)]
struct AgentSpec {
    node: String,
    source_ip: String,
}

struct Sim {
    sc: Scenario,
    public_url: String,
    started: Instant,
    rng: u64,
    node_down: BTreeSet<String>,
    link_down: BTreeSet<(String, String)>,
    svc_down: BTreeSet<(String, String)>,
    svc_hidden: BTreeSet<(String, String)>,
    vantage_by_ip: HashMap<String, String>,
    /// when each link stopped working (sim seconds). Real 4.x firmware keeps a dead
    /// neighbor's tracker for 24 h with its old `lastseen`, and so do we.
    down_at: HashMap<(String, String), f64>,
}

type Shared = Arc<Mutex<Sim>>;

fn key(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.into(), b.into())
    } else {
        (b.into(), a.into())
    }
}

impl Sim {
    // xorshift is plenty for jittering fake signal numbers and keeps us off the rand crate
    fn next(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Seconds on a made-up epoch clock, like the firmware's `time()`.
    fn now_s(&self) -> f64 {
        (1_800_000_000.0 + self.started.elapsed().as_secs_f64()).floor()
    }

    /// Note when links stop or start working, for the trackers' `lastseen`.
    fn mark_links(&mut self) {
        let now = self.now_s();
        let links = self.sc.links.clone();
        for l in &links {
            let k = key(&l.a, &l.b);
            if self.link_up(l) {
                self.down_at.remove(&k);
            } else {
                self.down_at.entry(k).or_insert(now);
            }
        }
    }

    fn node(&self, name: &str) -> Option<&NodeSpec> {
        self.sc
            .nodes
            .iter()
            .find(|n| n.name.eq_ignore_ascii_case(name))
    }

    fn link_up(&self, l: &LinkSpec) -> bool {
        !self.link_down.contains(&key(&l.a, &l.b))
            && !self.node_down.contains(&l.a)
            && !self.node_down.contains(&l.b)
    }

    /// every node reachable from `start` over live links (including itself)
    fn component(&self, start: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        if self.node_down.contains(start) {
            return seen;
        }
        let mut q = VecDeque::from([start.to_string()]);
        seen.insert(start.to_string());
        while let Some(n) = q.pop_front() {
            for l in self.sc.links.iter().filter(|l| self.link_up(l)) {
                let other = if l.a == n {
                    &l.b
                } else if l.b == n {
                    &l.a
                } else {
                    continue;
                };
                if seen.insert(other.clone()) {
                    q.push_back(other.clone());
                }
            }
        }
        seen
    }

    fn service_link(&self, node: &str, s: &ServiceSpec) -> String {
        let base = self.public_url.trim_end_matches('/');
        let path = format!("/svc/{}/{}", node, s.name.replace(' ', "_"));
        if s.kind == "http" {
            format!("{base}{path}")
        } else {
            // non-http services advertise host:port style links, like a winlink or chat port
            let hostport = base.trim_start_matches("http://");
            format!("tcp://{hostport}")
        }
    }

    fn sysinfo(&mut self, name: &str, q: &str) -> Value {
        let spec = self.node(name).cloned().expect("checked by caller");
        let legacy = legacy(&spec);
        let comp = self.component(&spec.name);
        let mut info = if legacy {
            json!({
                "api_version": "1.14",
                "node": spec.name,
                "node_details": {
                    "model": spec.model.clone().unwrap_or_else(|| "Ubiquiti Rocket M5".into()),
                    "board_id": "0xe6b5",
                    "firmware_mfg": "AREDN",
                    "firmware_version": spec.firmware,
                    "mesh_gateway": "0",
                },
                "meshrf": {"status": "on", "mode": "adhoc", "ssid": "AREDN-10-v3", "channel": "177", "chanbw": "10", "freq": "5885"},
                "tunnels": {"active_tunnel_count": 0, "legacy_tunnel_count": 0, "wireguard_tunnel_count": 0},
                "sysinfo": {"uptime": "3 days, 4:05:22", "loads": [0.1, 0.05, 0.01]},
                "interfaces": [{"name": "wlan0", "ip": spec.ip}],
            })
        } else {
            json!({
                "api_version": "2.0",
                "node": spec.name,
                "ip": spec.ip,
                "node_details": {
                    "model": spec.model.clone().unwrap_or_else(|| "Ubiquiti Rocket 5AC Lite".into()),
                    "firmware_mfg": "AREDN",
                    "firmware_version": spec.firmware,
                    "mesh_gateway": false,
                    "mesh_supernode": false,
                },
                "meshrf": {"status": "on", "mode": "adhoc", "channel": 140, "chanbw": "10", "freq": "5.700"},
                "sysinfo": {"uptime": "3 days, 4:05", "loads": [0.1, 0.05, 0.01], "freememory": "40000"},
                "interfaces": [{"name": "br-lan", "ip": spec.ip}],
            })
        };
        if let (Some(lat), Some(lon)) = (spec.lat, spec.lon) {
            // 3.x reads them from uci and sends strings
            info["lat"] = if legacy {
                json!(lat.to_string())
            } else {
                json!(lat)
            };
            info["lon"] = if legacy {
                json!(lon.to_string())
            } else {
                json!(lon)
            };
        }
        let (link_info, trackers) = self.links(&spec, legacy);
        if q.contains("link_info=1") {
            info["link_info"] = Value::Object(link_info);
        }
        if q.contains("hosts=1") {
            let hosts: Vec<Value> = self
                .sc
                .nodes
                .iter()
                .filter(|n| comp.contains(&n.name))
                .map(|n| json!({"name": n.name.to_lowercase(), "ip": n.ip}))
                .collect();
            info["hosts"] = json!(hosts);
        }
        if q.contains("services=1") {
            let mut out = vec![];
            for n in self.sc.nodes.iter().filter(|n| comp.contains(&n.name)) {
                for s in &n.services {
                    if self.svc_hidden.contains(&(n.name.clone(), s.name.clone())) {
                        continue;
                    }
                    // 3.x names the advertising node by address, 4.x by name
                    let ip = if legacy { &n.ip } else { &n.name };
                    out.push(json!({"name": s.name, "ip": ip, "link": self.service_link(&n.name, s), "protocol": "tcp"}));
                }
            }
            info["services"] = json!(out);
        }
        if q.contains("services_local=1") {
            let out: Vec<Value> = spec
                .services
                .iter()
                .map(|s| json!({"name": s.name, "protocol": "tcp", "link": self.service_link(&spec.name, s)}))
                .collect();
            info["services_local"] = json!(out);
        }
        if q.contains("lqm=1") {
            info["lqm"] = json!({
                "enabled": true,
                "config": {"user_blocks": ""},
                "info": {"trackers": Value::Object(trackers), "hidden_nodes": [], "now": self.now_s()},
            });
        }
        info
    }

    /// This node's `link_info` and LQM trackers. A dead neighbor's tracker stays listed
    /// for a day with its old `lastseen` on both firmware lines; 3.x's OLSR drops the link
    /// itself at once, and `link_info` then shows only the tracker's copy.
    fn links(&mut self, spec: &NodeSpec, legacy: bool) -> (Map<String, Value>, Map<String, Value>) {
        let days = self.started.elapsed().as_secs_f64() / 86_400.0;
        let now_s = self.now_s();
        let mine: Vec<(LinkSpec, f64, bool)> = self
            .sc
            .links
            .iter()
            .filter(|l| l.a == spec.name || l.b == spec.name)
            .filter_map(|l| {
                if self.link_up(l) {
                    return Some((l.clone(), now_s, true));
                }
                let since = *self.down_at.get(&key(&l.a, &l.b))?;
                (now_s - since < 86_400.0).then(|| (l.clone(), since, false))
            })
            .collect();
        let mut link_info = Map::new();
        let mut trackers = Map::new();
        for (i, (l, lastseen, up)) in mine.iter().enumerate() {
            let other = if l.a == spec.name { &l.b } else { &l.a };
            let Some(o) = self.node(other).cloned() else {
                continue;
            };
            let jitter = (self.next() - 0.5) * 2.0;
            let snr = (l.snr - l.drift_db_per_day * days + jitter)
                .max(0.0)
                .round();
            let noise = -95.0;
            let signal = noise + snr;
            let q = (l.quality + (self.next() - 0.5) * 4.0)
                .clamp(0.0, 100.0)
                .round();
            let rtt = (2.0 + self.next() * 3.0).round();
            let rf = l.link_type == "RF";
            let mac = format!("02:00:00:00:{:02x}:{:02x}", i / 256, i % 256);
            if legacy {
                // mgr/lqm.lua's names for the link types, and the devices they run on
                let (lqm_type, device) = match l.link_type.as_str() {
                    "RF" => ("RF", "wlan0"),
                    "TUN" => ("Tunnel", "tun50"),
                    _ => ("DtD", "br-dtdlink"),
                };
                if *up {
                    // aredn/olsr.lua getCurrentNeighbors: OLSR's link, plus iwinfo for RF
                    let mut d = json!({
                        "hostname": o.name,
                        "olsrInterface": device,
                        "linkType": l.link_type,
                        "linkQuality": q / 100.0,
                        "neighborLinkQuality": q / 100.0,
                        "currentLinkStatus": "SYMMETRIC",
                        "linkCost": (100.0 / q.max(1.0) * 100.0).round() / 100.0,
                    });
                    if rf {
                        d["signal"] = json!(signal);
                        d["noise"] = json!(noise);
                        d["tx_rate"] = json!(26);
                        d["rx_rate"] = json!(19.5);
                    }
                    link_info.insert(o.ip.clone(), d);
                } else {
                    // sysinfo.json adds every tracker OLSR doesn't list
                    link_info.insert(
                        o.ip.clone(),
                        json!({"linkType": lqm_type.to_uppercase(), "hostname": o.name.to_lowercase(), "interface": device}),
                    );
                }
                // mgr/lqm.lua keeps no signal, only its running SNR
                let mut t = json!({
                    "type": lqm_type, "device": device,
                    "mac": mac, "ip": o.ip, "canonical_ip": o.ip, "hostname": o.name.to_lowercase(),
                    "user_blocks": false, "ping_quality": 100, "quality": q, "rev_quality": q,
                    "node_route_count": 3, "babel_route_count": 0, "routable": *up,
                    "lastseen": lastseen,
                });
                if rf {
                    t["snr"] = json!(snr);
                    t["tx_quality"] = json!(q);
                    t["tx_bitrate"] = json!(26.0);
                    t["rx_bitrate"] = json!(19.5);
                }
                trackers.insert(mac, t);
                continue;
            }
            let mut d = json!({"hostname": o.name, "linkType": l.link_type, "interface": "wlan0"});
            if rf {
                d["signal"] = json!(signal);
                d["noise"] = json!(noise);
            }
            link_info.insert(o.ip.clone(), d);
            let mut t = json!({
                "mac": mac, "ip": o.ip, "canonical_ip": o.ip, "hostname": o.name,
                "type": l.link_type, "device": "wlan0",
                "lq": q, "avg_lq": q, "quality": q, "rev_quality": q,
                "rtt": rtt, "tx_quality": q, "ping_quality": 100,
                "tx_bitrate": 130.0, "rx_bitrate": 117.0,
                "tx_packets": 100000, "tx_fail": ((100.0 - q) * 10.0) as i64, "tx_retries": 500,
                "metric": 256, "routable": true, "route_count": 3,
                "lastseen": lastseen,
            });
            if rf {
                t["signal"] = json!(signal);
                t["snr"] = json!(snr);
                t["rev_snr"] = json!(snr);
            }
            trackers.insert(mac, t);
        }
        (link_info, trackers)
    }
}

fn legacy(spec: &NodeSpec) -> bool {
    spec.firmware.starts_with('3')
}

/// `/a/sysinfo`: 4.x's sysinfo. 3.x has no such page, and its web UI answers with the HTML
/// app page it shows for any page it doesn't know.
async fn sysinfo(
    State(s): State<Shared>,
    Path(node): Path<String>,
    raw: axum::extract::RawQuery,
) -> Response {
    let mut sim = s.lock().unwrap();
    let spec = match answering(&sim, &node) {
        Ok(spec) => spec,
        Err(r) => return r.into_response(),
    };
    if legacy(&spec) {
        return Html(
            "<!DOCTYPE html><html><head><title>sysinfo</title></head><body></body></html>",
        )
        .into_response();
    }
    Json(sim.sysinfo(&spec.name, &raw.0.unwrap_or_default())).into_response()
}

/// `/cgi-bin/sysinfo.json`: 3.x's sysinfo; 4.x redirects it to `/a/sysinfo`.
async fn sysinfo_json(
    State(s): State<Shared>,
    Path(node): Path<String>,
    raw: axum::extract::RawQuery,
) -> Response {
    let mut sim = s.lock().unwrap();
    let spec = match answering(&sim, &node) {
        Ok(spec) => spec,
        Err(r) => return r.into_response(),
    };
    let q = raw.0.unwrap_or_default();
    if !legacy(&spec) {
        return Redirect::temporary(&format!("/n/{node}/a/sysinfo?{q}")).into_response();
    }
    Json(sim.sysinfo(&spec.name, &q)).into_response()
}

fn answering(sim: &Sim, node: &str) -> Result<NodeSpec, (StatusCode, &'static str)> {
    let Some(spec) = sim.node(node).cloned() else {
        return Err((StatusCode::NOT_FOUND, "no such node"));
    };
    if sim.node_down.contains(&spec.name) {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "node down"));
    }
    Ok(spec)
}

async fn service(
    State(s): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((node, name)): Path<(String, String)>,
) -> Response {
    let sim = s.lock().unwrap();
    let name = name.replace('_', " ");
    let Some(spec) = sim.node(&node).cloned() else {
        return (StatusCode::NOT_FOUND, "no such node").into_response();
    };
    if !spec.services.iter().any(|x| x.name == name) {
        return (StatusCode::NOT_FOUND, "no such service").into_response();
    }
    // unknown source addresses count as "on the target's own LAN"
    let vantage = sim.vantage_by_ip.get(&peer.ip().to_string()).cloned();
    let reachable = match &vantage {
        Some(v) => sim.component(v).contains(&spec.name),
        None => !sim.node_down.contains(&spec.name),
    };
    if !reachable {
        // a real unreachable host times out; a 504 keeps tests fast and is still a failure
        return (StatusCode::GATEWAY_TIMEOUT, "unreachable from this vantage").into_response();
    }
    if sim.svc_down.contains(&(spec.name.clone(), name)) {
        return (StatusCode::SERVICE_UNAVAILABLE, "service down").into_response();
    }
    (StatusCode::OK, "ok").into_response()
}

#[derive(Deserialize)]
struct Ctl {
    a: Option<String>,
    b: Option<String>,
    node: Option<String>,
    name: Option<String>,
    up: Option<bool>,
}

fn need(v: &Option<String>, what: &str) -> Result<String, (StatusCode, String)> {
    v.clone()
        .ok_or((StatusCode::BAD_REQUEST, format!("missing {what}")))
}

async fn ctl(
    State(s): State<Shared>,
    Path(what): Path<String>,
    Query(c): Query<Ctl>,
) -> Result<&'static str, (StatusCode, String)> {
    let mut sim = s.lock().unwrap();
    let up = c.up.unwrap_or(true);
    let toggle = |set: &mut BTreeSet<(String, String)>, k: (String, String)| {
        if up {
            set.remove(&k);
        } else {
            set.insert(k);
        }
    };
    match what.as_str() {
        "link" => toggle(
            &mut sim.link_down,
            key(&need(&c.a, "a")?, &need(&c.b, "b")?),
        ),
        "node" => {
            let n = need(&c.node, "node")?;
            if up {
                sim.node_down.remove(&n);
            } else {
                sim.node_down.insert(n);
            }
        }
        "service" => toggle(
            &mut sim.svc_down,
            (need(&c.node, "node")?, need(&c.name, "name")?),
        ),
        // up=false unpublishes the advert but leaves the service running
        "advertise" => toggle(
            &mut sim.svc_hidden,
            (need(&c.node, "node")?, need(&c.name, "name")?),
        ),
        _ => return Err((StatusCode::NOT_FOUND, "unknown control".into())),
    }
    sim.mark_links();
    Ok("ok")
}

async fn state(State(s): State<Shared>) -> Json<Value> {
    let sim = s.lock().unwrap();
    Json(json!({
        "nodes_down": sim.node_down,
        "links_down": sim.link_down,
        "services_down": sim.svc_down,
        "services_unpublished": sim.svc_hidden,
    }))
}

fn validate(sc: &Scenario) -> Result<()> {
    let names: BTreeSet<_> = sc.nodes.iter().map(|n| n.name.as_str()).collect();
    for l in &sc.links {
        for end in [&l.a, &l.b] {
            if !names.contains(end.as_str()) {
                bail!("link {}-{} names unknown node {end}", l.a, l.b);
            }
        }
    }
    for a in &sc.agents {
        if !names.contains(a.node.as_str()) {
            bail!("agent names unknown node {}", a.node);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let text = std::fs::read_to_string(&args.scenario)
        .with_context(|| format!("reading {}", args.scenario.display()))?;
    let sc: Scenario = toml::from_str(&text).context("parsing scenario")?;
    validate(&sc)?;
    let public_url = args
        .public_url
        .unwrap_or_else(|| format!("http://{}", args.listen));
    let vantage_by_ip = sc
        .agents
        .iter()
        .map(|a| (a.source_ip.clone(), a.node.clone()))
        .collect();
    let sim = Sim {
        rng: sc.seed.max(1),
        sc,
        public_url,
        started: Instant::now(),
        node_down: BTreeSet::new(),
        link_down: BTreeSet::new(),
        svc_down: BTreeSet::new(),
        svc_hidden: BTreeSet::new(),
        vantage_by_ip,
        down_at: HashMap::new(),
    };
    let names: Vec<String> = sim.sc.nodes.iter().map(|n| n.name.clone()).collect();
    let shared: Shared = Arc::new(Mutex::new(sim));
    let app = Router::new()
        .route("/n/{node}/a/sysinfo", get(sysinfo))
        .route("/n/{node}/cgi-bin/sysinfo.json", get(sysinfo_json))
        .route("/svc/{node}/{name}", get(service))
        .route("/_ctl/state", get(state))
        .route("/_ctl/{what}", post(ctl))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    println!(
        "ridgewarden-sim on http://{} with {} nodes:",
        args.listen,
        names.len()
    );
    for n in &names {
        println!("  http://{}/n/{}/a/sysinfo", args.listen, n);
    }
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
