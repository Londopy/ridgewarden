//! Parsing AREDN's sysinfo, as 4.x serves it at `/a/sysinfo` (api_version 2.x) and 3.x at
//! `/cgi-bin/sysinfo.json` (api_version 1.x).
//!
//! The api_version decides how fields are read, since the two firmware lines use some of the
//! same names for different things: 1.x `link_info` carries OLSR link qualities as 0-1
//! fractions and each service's `ip` is the advertising node's address, while 2.x keeps link
//! data only in LQM trackers and puts the node's name in `ip`. A missing or unknown version
//! is read as 2.x, taking 1.x fields where 2.x ones are missing. Nothing assumes a field
//! exists: mixed fleets are normal.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mesh::MAX_LINKS_PER_POLL;
use crate::util::{norm_node, valid_node};

/// The query flags every poll asks for.
pub const SYSINFO_QUERY: &str = "hosts=1&services=1&services_local=1&link_info=1&lqm=1";
/// A service with a longer name, link, protocol or host than this is left out: no real one
/// comes close, and the rest of the suite stores and shows them.
pub const MAX_SERVICE_FIELD: usize = 256;
/// Services and hosts taken from one reply, far more than a regional mesh has, so a broken
/// node can't swamp the log or an app's pages.
pub const MAX_SERVICES: usize = 4096;
pub const MAX_HOSTS: usize = 8192;

/// Which sysinfo layout a response has, from its `api_version`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Api {
    /// "1.x": 3.x firmware's Lua `sysinfo.json`
    V1,
    /// "2.x": 4.x firmware's `sysinfo.ut`
    V2,
    /// no `api_version`, or one newer than this parser knows
    #[default]
    Unknown,
}

impl Api {
    pub fn of(version: Option<&str>) -> Self {
        match version.and_then(|v| v.trim().split('.').next()?.parse::<u32>().ok()) {
            Some(1) => Api::V1,
            Some(2) => Api::V2,
            _ => Api::Unknown,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LinkObs {
    pub neighbor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub link_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noise: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr: Option<f64>,
    /// 0-100, this side's view of the link
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<f64>,
    /// 0-100, the neighbor's view of the link
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nquality: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_rate: Option<f64>,
    /// percent of transmissions failing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_fail: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Service {
    pub host: String,
    pub name: String,
    pub link: String,
    pub protocol: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeObs {
    pub node: String,
    pub api_version: Option<String>,
    /// how the response was read, from `api_version`
    pub api: Api,
    pub firmware: Option<String>,
    pub model: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub links: Vec<LinkObs>,
    /// every service this node can see on the mesh
    pub services: Vec<Service>,
    pub services_local: Vec<Service>,
    pub hosts: Vec<String>,
    /// the response had `lqm.info.trackers` (LQM link quality data, 3.22 and later)
    pub trackers: bool,
    /// trackers left out because their neighbor hasn't been heard for a while
    pub stale_skipped: usize,
}

fn num(v: &Value, k: &str) -> Option<f64> {
    match v.get(k)? {
        Value::Number(n) => n.as_f64(),
        // uci values come through as strings on some firmware
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn st(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Seconds since a tracker was last heard before it counts as disconnected (the firmware UI's own margin).
const LQM_LIVE_MARGIN_S: f64 = 120.0;

/// LQM trackers are always 0-100; a quality of 1 means 1%, not 100%.
fn pct100(x: Option<f64>) -> Option<f64> {
    x.map(|q| q.clamp(0.0, 100.0))
}

/// With no version to go by, a quality of at most 1 is taken as a 0-1 fraction.
fn pct_guess(x: Option<f64>) -> Option<f64> {
    x.map(|q| if q <= 1.0 { q * 100.0 } else { q })
        .map(|q| q.clamp(0.0, 100.0))
}

/// `host` is the service's `ip` field: a node name from 2.x, an address from 1.x, which
/// `names` (address to name, from the hosts list) turns back into a name where it can.
fn parse_services(
    v: Option<&Value>,
    default_host: &str,
    names: &HashMap<String, String>,
) -> Vec<Service> {
    let mut out: Vec<Service> = v
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let name = st(s, "name")?;
            let host = st(s, "ip")
                .map(|ip| names.get(&ip).cloned().unwrap_or(ip))
                .unwrap_or_else(|| default_host.to_string());
            let svc = Service {
                host: norm_node(&host),
                name,
                link: st(s, "link").unwrap_or_default(),
                protocol: st(s, "protocol").unwrap_or_else(|| "tcp".into()),
            };
            [&svc.host, &svc.name, &svc.link, &svc.protocol]
                .iter()
                .all(|f| f.len() <= MAX_SERVICE_FIELD)
                .then_some(svc)
        })
        .take(MAX_SERVICES)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What `tracker_links` found besides the links themselves.
struct Trackers {
    present: bool,
    /// addresses of trackers left out (blocked or stale): both firmware lines list every
    /// tracker in link_info too, and those copies must not bring them back
    left_out: HashSet<String>,
    stale_skipped: usize,
}

/// Links from `lqm.info.trackers`, which both firmware lines have.
fn tracker_links(v: &Value, links: &mut BTreeMap<String, LinkObs>) -> Trackers {
    // LQM keeps a tracker for 24 h after its neighbor was last heard (lastseen_timeout in
    // lqm.uc and lqm.lua), so "listed" is not "up": the firmware's own UI calls a tracker
    // disconnected once lastseen + 120 s is behind lqm.info.now.
    let trackers = v.pointer("/lqm/info/trackers").and_then(Value::as_object);
    let lqm_now = v.pointer("/lqm/info/now").and_then(Value::as_f64);
    let mut out = Trackers {
        present: trackers.is_some(),
        left_out: HashSet::new(),
        stale_skipped: 0,
    };
    for t in trackers.into_iter().flat_map(|m| m.values()) {
        let Some(host) = st(t, "hostname") else {
            continue;
        };
        let blocked = t.get("user_blocks").and_then(Value::as_bool) == Some(true);
        let stale = matches!((lqm_now, num(t, "lastseen")),
            (Some(now), Some(seen)) if now - seen > LQM_LIVE_MARGIN_S);
        if blocked || stale {
            // 1.x lists a tracker in link_info under its canonical_ip, 2.x under its ip
            out.left_out.extend(st(t, "ip"));
            out.left_out.extend(st(t, "canonical_ip"));
            out.stale_skipped += usize::from(stale && !blocked);
            continue;
        }
        let signal = num(t, "signal");
        let snr = num(t, "snr");
        let l = LinkObs {
            neighbor: norm_node(&host),
            ip: st(t, "ip").or_else(|| st(t, "canonical_ip")),
            link_type: st(t, "type").map(|s| s.to_ascii_uppercase()),
            signal,
            noise: match (signal, snr) {
                (Some(s), Some(n)) => Some(s - n),
                _ => None,
            },
            snr,
            quality: pct100(num(t, "quality").or_else(|| num(t, "lq"))),
            nquality: pct100(num(t, "rev_quality").or_else(|| num(t, "rev_lq"))),
            rtt: num(t, "rtt"),
            tx_rate: num(t, "tx_bitrate").or_else(|| num(t, "avg_tx_bitrate")),
            rx_rate: num(t, "rx_bitrate").or_else(|| num(t, "avg_rx_bitrate")),
            tx_fail: num(t, "tx_quality").map(|q| (100.0 - q).clamp(0.0, 100.0)),
            metric: num(t, "metric"),
        };
        links.insert(l.neighbor.clone(), l);
    }
    out
}

/// `link_info`, keyed by address: fills in what trackers didn't have, and is the only
/// source on 3.x nodes from before LQM.
fn link_info_links(
    v: &Value,
    api: Api,
    left_out: &HashSet<String>,
    links: &mut BTreeMap<String, LinkObs>,
) {
    let fraction = |x: Option<f64>| x.map(|q| (q * 100.0).clamp(0.0, 100.0));
    for (ip, d) in v
        .get("link_info")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let Some(host) = st(d, "hostname") else {
            continue;
        };
        if left_out.contains(ip) {
            continue;
        }
        // only 1.x has OLSR's link qualities (0-1) and the radio's rates here
        let (quality, nquality, tx_rate, rx_rate) = match api {
            Api::V1 => (
                fraction(num(d, "linkQuality")),
                fraction(num(d, "neighborLinkQuality")),
                num(d, "tx_rate"),
                num(d, "rx_rate"),
            ),
            Api::V2 => (None, None, None, None),
            Api::Unknown => (
                pct_guess(num(d, "linkQuality")),
                pct_guess(num(d, "neighborLinkQuality")),
                num(d, "tx_rate"),
                num(d, "rx_rate"),
            ),
        };
        let key = norm_node(&host);
        let signal = num(d, "signal");
        let noise = num(d, "noise");
        let e = links.entry(key.clone()).or_insert_with(|| LinkObs {
            neighbor: key,
            ..Default::default()
        });
        e.ip = e.ip.take().or_else(|| Some(ip.clone()));
        e.link_type = e
            .link_type
            .take()
            .or_else(|| st(d, "linkType").map(|s| s.to_uppercase()));
        e.signal = e.signal.or(signal);
        e.noise = e.noise.or(noise);
        if e.snr.is_none()
            && let (Some(s), Some(n)) = (signal, noise)
        {
            e.snr = Some(s - n);
        }
        e.quality = e.quality.or(quality);
        e.nquality = e.nquality.or(nquality);
        e.tx_rate = e.tx_rate.or(tx_rate);
        e.rx_rate = e.rx_rate.or(rx_rate);
    }
}

pub fn parse(v: &Value) -> Result<NodeObs> {
    let Some(node) = st(v, "node") else {
        bail!("sysinfo has no node name")
    };
    let node = norm_node(&node);
    if !valid_node(&node) {
        bail!("sysinfo node name {node:?} is not a valid node name");
    }
    let api_version = st(v, "api_version");
    let api = Api::of(api_version.as_deref());
    let details = v.get("node_details").cloned().unwrap_or(Value::Null);

    let mut links: BTreeMap<String, LinkObs> = BTreeMap::new();
    let trackers = tracker_links(v, &mut links);
    link_info_links(v, api, &trackers.left_out, &mut links);

    let hosts: Vec<(String, Option<String>)> = v
        .get("hosts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| Some((norm_node(&st(h, "name")?), st(h, "ip"))))
        .filter(|(n, _)| valid_node(n))
        .take(MAX_HOSTS)
        .collect();
    // 2.x names the advertising node itself; anything else may give an address
    let names: HashMap<String, String> = match api {
        Api::V2 => HashMap::new(),
        Api::V1 | Api::Unknown => hosts
            .iter()
            .filter_map(|(n, ip)| Some((ip.clone()?, n.clone())))
            .collect(),
    };

    Ok(NodeObs {
        api_version,
        api,
        firmware: st(&details, "firmware_version"),
        model: st(&details, "model"),
        lat: num(v, "lat"),
        lon: num(v, "lon"),
        // invalid neighbor names (they end up in crawl URLs) and absurd counts are dropped
        links: links
            .into_values()
            .filter(|l| valid_node(&l.neighbor))
            .take(MAX_LINKS_PER_POLL)
            .collect(),
        services: parse_services(v.get("services"), &node, &names),
        services_local: parse_services(v.get("services_local"), &node, &names),
        hosts: hosts.into_iter().map(|(n, _)| n).collect(),
        node,
        trackers: trackers.present,
        stale_skipped: trackers.stale_skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // shaped like sysinfo.ut output from a 4.26 node
    fn modern() -> Value {
        json!({
            "api_version": "2.0",
            "node": "TST-Cuesta",
            "node_details": {"firmware_version": "4.26.7.0", "model": "Rocket 5AC"},
            "lat": "35.33", "lon": -120.62,
            "link_info": {
                "10.1.0.2": {"hostname": "tst-madonna.local.mesh", "linkType": "RF", "signal": -70, "noise": -95},
                "10.1.0.9": {"hostname": "tst-shack", "linkType": "DTD"}
            },
            "lqm": {"enabled": true, "info": {"trackers": {
                "aa:bb": {"hostname": "TST-Madonna", "type": "RF", "signal": -70, "snr": 25,
                          "quality": 96, "rev_quality": 94, "rtt": 3, "tx_bitrate": 130.0,
                          "tx_quality": 98, "metric": 256, "ip": "10.1.0.2"},
                "cc:dd": {"hostname": "blocked-node", "type": "RF", "user_blocks": true}
            }}},
            "services": [
                {"name": "Mesh Chat", "ip": "tst-shack", "link": "http://tst-shack.local.mesh:8080/", "protocol": "tcp"},
                {"name": "Mesh Chat", "ip": "tst-shack", "link": "http://tst-shack.local.mesh:8080/", "protocol": "tcp"}
            ],
            "hosts": [{"name": "tst-shack", "ip": "10.1.0.9"}]
        })
    }

    #[test]
    fn parses_modern_node() {
        let n = parse(&modern()).unwrap();
        assert_eq!(n.node, "tst-cuesta");
        assert_eq!(n.firmware.as_deref(), Some("4.26.7.0"));
        assert_eq!(n.lat, Some(35.33));
        assert_eq!(
            n.links.len(),
            2,
            "blocked tracker skipped, DTD from link_info kept"
        );
        let m = n
            .links
            .iter()
            .find(|l| l.neighbor == "tst-madonna")
            .unwrap();
        assert_eq!(m.snr, Some(25.0));
        assert_eq!(m.noise, Some(-95.0));
        assert_eq!(m.quality, Some(96.0));
        assert_eq!(m.tx_fail, Some(2.0));
        let d = n.links.iter().find(|l| l.neighbor == "tst-shack").unwrap();
        assert_eq!(d.link_type.as_deref(), Some("DTD"));
        assert_eq!(n.services.len(), 1, "duplicate adverts collapse");
        assert_eq!(n.hosts, vec!["tst-shack"]);
    }

    #[test]
    fn says_whether_trackers_came_and_how_many_were_stale() {
        let v = json!({
            "node": "TST-Cuesta",
            "lqm": {"info": {"now": 10_000, "trackers": {
                "aa": {"hostname": "tst-a", "lastseen": 9_990},
                "bb": {"hostname": "tst-b", "lastseen": 1_000, "ip": "10.0.0.2"},
                "cc": {"hostname": "tst-c", "lastseen": 2_000},
            }}},
        });
        let n = parse(&v).unwrap();
        assert!(n.trackers);
        assert_eq!(n.stale_skipped, 2);
        assert_eq!(n.links.len(), 1);
        let old = parse(&json!({"node": "TST-Old", "link_info": {}})).unwrap();
        assert!(!old.trackers);
        assert_eq!(old.stale_skipped, 0);
    }

    #[test]
    fn parses_legacy_link_info() {
        let v = json!({
            "api_version": "1.5", "node": "OLD-NODE",
            "link_info": {"10.0.0.2": {"hostname": "peer", "linkType": "RF",
                "linkQuality": 0.82, "neighborLinkQuality": 0.75, "signal": -80, "noise": -96,
                "tx_rate": 54, "rx_rate": 48}}
        });
        let n = parse(&v).unwrap();
        let l = &n.links[0];
        assert_eq!(l.quality, Some(82.0));
        assert_eq!(l.nquality, Some(75.0));
        assert_eq!(l.snr, Some(16.0));
        assert_eq!(l.tx_rate, Some(54.0));
    }

    // field for field what AREDN 3.25.8.0's files/www/cgi-bin/sysinfo.json writes (with
    // aredn/olsr.lua, aredn/info.lua and mgr/lqm.lua from that tag); the names and numbers
    // are made up
    fn aredn_3_25() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/sysinfo-3.25.8.0.json")).unwrap()
    }

    fn link<'a>(n: &'a NodeObs, name: &str) -> &'a LinkObs {
        n.links.iter().find(|l| l.neighbor == name).unwrap()
    }

    #[test]
    fn reads_an_aredn_3_25_node() {
        let n = parse(&aredn_3_25()).unwrap();
        assert_eq!(n.api, Api::V1);
        assert_eq!(n.node, "tst-paso");
        assert_eq!(n.firmware.as_deref(), Some("3.25.8.0"));
        assert_eq!(
            (n.lat, n.lon),
            (Some(35.6227), Some(-120.69)),
            "uci strings"
        );
        let names: Vec<&str> = n.links.iter().map(|l| l.neighbor.as_str()).collect();
        assert_eq!(
            names,
            ["tst-cuesta", "tst-hilltop", "tst-north", "tst-pasohub"],
            "the stale and the blocked neighbor stay out though link_info lists them"
        );
        assert!(n.trackers);
        assert_eq!(n.stale_skipped, 1);

        let north = link(&n, "tst-north");
        assert_eq!(north.link_type.as_deref(), Some("RF"));
        assert_eq!(north.snr, Some(24.0), "LQM's running SNR");
        assert_eq!((north.signal, north.noise), (Some(-71.0), Some(-95.0)));
        assert_eq!((north.quality, north.nquality), (Some(99.0), Some(96.0)));
        assert_eq!(north.tx_rate, Some(26.0));
        assert_eq!(north.tx_fail, Some(2.5));
        // a fresh tunnel tracker has no quality yet; OLSR's 0-1 figures fill in
        let tun = link(&n, "tst-cuesta");
        assert_eq!(tun.link_type.as_deref(), Some("TUNNEL"));
        assert_eq!((tun.quality, tun.nquality), (Some(94.0), Some(88.0)));
        assert_eq!(link(&n, "tst-pasohub").link_type.as_deref(), Some("DTD"));

        let svc: Vec<(&str, &str)> = n
            .services
            .iter()
            .map(|s| (s.name.as_str(), s.host.as_str()))
            .collect();
        assert_eq!(
            svc,
            [
                ("Winlink Gateway", "tst-cuesta"),
                ("Paso Camera", "tst-paso"),
                ("Mesh Chat", "tst-shack")
            ],
            "1.x gives addresses; the hosts list names them"
        );
        assert_eq!(n.services_local[0].host, "tst-paso");
        assert!(n.hosts.contains(&"tst-paso-pi".to_string()));
    }

    #[test]
    fn api_version_decides_how_fields_are_read() {
        let with = |version: Option<&str>| {
            let mut v = json!({
                "node": "TST-PASO",
                "hosts": [{"name": "TST-SHACK", "ip": "10.54.1.7"}],
                "services": [{"name": "Chat", "ip": "10.54.1.7", "link": "", "protocol": "tcp"}],
                "link_info": {"10.54.1.4": {"hostname": "TST-NORTH", "linkType": "RF",
                    "linkQuality": 0.82, "neighborLinkQuality": 0.75, "tx_rate": 26}},
            });
            if let Some(ver) = version {
                v["api_version"] = json!(ver);
            }
            parse(&v).unwrap()
        };
        let v1 = with(Some("1.14"));
        assert_eq!(v1.api, Api::V1);
        assert_eq!(
            (v1.links[0].quality, v1.links[0].nquality),
            (Some(82.0), Some(75.0))
        );
        assert_eq!(v1.links[0].tx_rate, Some(26.0));
        assert_eq!(v1.services[0].host, "tst-shack");

        // 2.x has no OLSR fields and names the node in `ip`; nothing is guessed
        let v2 = with(Some("2.0"));
        assert_eq!(v2.api, Api::V2);
        assert_eq!((v2.links[0].quality, v2.links[0].tx_rate), (None, None));
        assert_eq!(v2.services[0].host, "10.54.1.7");

        // no version, or one from the future: read both layouts
        for unknown in [None, Some("3.0"), Some("x")] {
            let u = with(unknown);
            assert_eq!(u.api, Api::Unknown, "{unknown:?}");
            assert_eq!(u.links[0].quality, Some(82.0));
            assert_eq!(u.services[0].host, "tst-shack");
        }
    }

    #[test]
    fn oversized_services_and_lists_are_cut() {
        let long = json!("x".repeat(MAX_SERVICE_FIELD + 1));
        let mut services: Vec<Value> = ["name", "link", "protocol", "ip"]
            .into_iter()
            .map(|field| {
                let mut s =
                    json!({"name": "Chat", "ip": "tst-shack", "link": "", "protocol": "tcp"});
                s[field] = long.clone();
                s
            })
            .collect();
        services.extend((0..MAX_SERVICES + 10).map(|i| {
            json!({"name": format!("svc {i}"), "ip": "tst-shack", "link": "", "protocol": "tcp"})
        }));
        let hosts: Vec<Value> = (0..MAX_HOSTS + 10)
            .map(|i| json!({"name": format!("tst-{i}"), "ip": "10.0.0.1"}))
            .collect();
        let n = parse(&json!({
            "node": "tst-paso", "api_version": "2.0", "services": services, "hosts": hosts
        }))
        .unwrap();
        assert_eq!(n.services.len(), MAX_SERVICES);
        assert!(
            n.services.iter().all(|s| s.name.starts_with("svc ")),
            "a service with any field too long is left out"
        );
        assert_eq!(n.hosts.len(), MAX_HOSTS);
    }

    #[test]
    fn tolerates_missing_everything_but_the_name() {
        let n = parse(&json!({"node": "x"})).unwrap();
        assert!(n.links.is_empty() && n.services.is_empty());
        assert!(parse(&json!({})).is_err());
    }
}
