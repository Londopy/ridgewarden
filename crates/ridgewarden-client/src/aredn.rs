//! Parsing AREDN's `/a/sysinfo` (api_version 2.0) tolerantly.
//!
//! Mixed fleets are normal, so nothing here assumes a field exists. 4.x nodes carry rich
//! per-link data in `lqm.info.trackers`; older nodes only have `link_info`, sometimes with
//! the OLSR-era `linkQuality`/`neighborLinkQuality` names. Both are read, trackers first.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mesh::MAX_LINKS_PER_POLL;
use crate::util::{norm_node, valid_node};

/// The query flags every poll asks for.
pub const SYSINFO_QUERY: &str = "hosts=1&services=1&services_local=1&link_info=1&lqm=1";

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
    pub firmware: Option<String>,
    pub model: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub links: Vec<LinkObs>,
    /// every service this node can see on the mesh
    pub services: Vec<Service>,
    pub services_local: Vec<Service>,
    pub hosts: Vec<String>,
    /// the response had `lqm.info.trackers` (4.x link quality data)
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

/// 4.x trackers are always 0-100; a quality of 1 means 1%, not 100%.
fn pct100(x: Option<f64>) -> Option<f64> {
    x.map(|q| q.clamp(0.0, 100.0))
}

/// Some builds report 0-1, others 0-100.
fn pct(x: Option<f64>) -> Option<f64> {
    x.map(|q| if q <= 1.0 { q * 100.0 } else { q })
        .map(|q| q.clamp(0.0, 100.0))
}

fn parse_services(v: Option<&Value>, default_host: &str) -> Vec<Service> {
    let mut out: Vec<Service> = v
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let name = st(s, "name")?;
            Some(Service {
                host: norm_node(&st(s, "ip").unwrap_or_else(|| default_host.to_string())),
                name,
                link: st(s, "link").unwrap_or_default(),
                protocol: st(s, "protocol").unwrap_or_else(|| "tcp".into()),
            })
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

pub fn parse(v: &Value) -> Result<NodeObs> {
    let Some(node) = st(v, "node") else {
        bail!("sysinfo has no node name")
    };
    let node = norm_node(&node);
    if !valid_node(&node) {
        bail!("sysinfo node name {node:?} is not a valid node name");
    }
    let details = v.get("node_details").cloned().unwrap_or(Value::Null);

    let mut links: BTreeMap<String, LinkObs> = BTreeMap::new();

    // 4.x: lqm trackers, keyed by MAC. LQM keeps a tracker for 24 h after its neighbor was
    // last heard (lqm.uc lastseen_timeout), so "listed" is not "up": the firmware's own UI
    // calls a tracker disconnected once lastseen + 120 s is behind lqm.info.now.
    let trackers = v.pointer("/lqm/info/trackers").and_then(Value::as_object);
    let lqm_now = v.pointer("/lqm/info/now").and_then(Value::as_f64);
    let mut stale_ips: std::collections::HashSet<String> = Default::default();
    let mut stale_skipped = 0;
    for t in trackers.into_iter().flat_map(|m| m.values()) {
        let Some(host) = st(t, "hostname") else {
            continue;
        };
        if t.get("user_blocks").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let (Some(now), Some(seen)) = (lqm_now, num(t, "lastseen"))
            && now - seen > LQM_LIVE_MARGIN_S
        {
            if let Some(ip) = st(t, "ip").or_else(|| st(t, "canonical_ip")) {
                stale_ips.insert(ip);
            }
            stale_skipped += 1;
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

    // link_info, keyed by IP: fills in anything trackers didn't have, and is the only
    // source on older nodes
    for (ip, d) in v
        .get("link_info")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let Some(host) = st(d, "hostname") else {
            continue;
        };
        if stale_ips.contains(ip) {
            continue; // the same stale tracker, listed again by link_info
        }
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
        e.quality = e.quality.or_else(|| pct(num(d, "linkQuality")));
        e.nquality = e.nquality.or_else(|| pct(num(d, "neighborLinkQuality")));
        e.tx_rate = e.tx_rate.or_else(|| num(d, "tx_rate"));
        e.rx_rate = e.rx_rate.or_else(|| num(d, "rx_rate"));
    }

    let hosts = v
        .get("hosts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| st(h, "name").map(|n| norm_node(&n)))
        .filter(|n| valid_node(n))
        .collect();

    Ok(NodeObs {
        api_version: st(v, "api_version"),
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
        services: parse_services(v.get("services"), &node),
        services_local: parse_services(v.get("services_local"), &node),
        hosts,
        node,
        trackers: trackers.is_some(),
        stale_skipped,
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
                {"name": "Club Chat", "ip": "tst-shack", "link": "http://tst-shack.local.mesh:8080/", "protocol": "tcp"},
                {"name": "Club Chat", "ip": "tst-shack", "link": "http://tst-shack.local.mesh:8080/", "protocol": "tcp"}
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

    #[test]
    fn tolerates_missing_everything_but_the_name() {
        let n = parse(&json!({"node": "x"})).unwrap();
        assert!(n.links.is_empty() && n.services.is_empty());
        assert!(parse(&json!({})).is_err());
    }
}
