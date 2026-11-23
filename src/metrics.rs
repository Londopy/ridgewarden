//! The agent's numbers in Prometheus' text format, for a scraper on this machine: what
//! `ridgewarden status` and the dashboard show, as gauges.

use std::fmt::Write as _;

use anyhow::Result;

use crate::app::App;
use crate::util::now_ms;
use crate::{node, usage};

/// Label values escape backslash, double quote and newline; nothing else needs it.
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn labels(pairs: &[(&str, &str)]) -> String {
    let inner: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
        .collect();
    format!("{{{}}}", inner.join(","))
}

/// One family of gauges, left out when there is nothing to say yet.
fn gauge(out: &mut String, name: &str, help: &str, samples: &[(String, f64)]) {
    if samples.is_empty() {
        return;
    }
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge");
    for (labels, v) in samples {
        let _ = writeln!(out, "{name}{labels} {v}");
    }
}

fn one(v: impl Into<f64>) -> Vec<(String, f64)> {
    vec![(String::new(), v.into())]
}

fn seconds(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

/// What `GET /local/v1/metrics` answers.
pub fn render(app: &App) -> Result<String> {
    let now = now_ms();
    let u = usage::measure(app)?;
    let (last_poll, seen, blobs) = {
        let c = app.log.store.conn();
        let blobs: (i64, i64) = c.query_row(
            "SELECT coalesce(sum(complete), 0), coalesce(sum(wanted AND NOT complete), 0) FROM blobs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        (node::last_poll(&c)?, node::seen_by(&c, now)?, blobs)
    };
    let chains = app.log.heads()?.len();
    let forks = app.log.fork_count()?;
    let trusted = app.log.trusted().len();
    let peers: Vec<_> = app.peers.lock().unwrap().values().cloned().collect();

    let mut out = String::new();
    let a = &app.log.id.author;
    gauge(
        &mut out,
        "ridgewarden_info",
        "This agent's version and callsign.",
        &[(
            labels(&[("version", env!("CARGO_PKG_VERSION")), ("call", &a.call)]),
            1.0,
        )],
    );
    gauge(
        &mut out,
        "ridgewarden_events",
        "Events held, by log.",
        &[
            (labels(&[("log", "main")]), u.main.events as f64),
            (labels(&[("log", "telemetry")]), u.telemetry.events as f64),
        ],
    );
    gauge(
        &mut out,
        "ridgewarden_event_bytes",
        "Size of the events' JSON as synced, by log.",
        &[
            (labels(&[("log", "main")]), u.main.bytes as f64),
            (labels(&[("log", "telemetry")]), u.telemetry.bytes as f64),
        ],
    );
    gauge(
        &mut out,
        "ridgewarden_disk_bytes",
        "Bytes on disk: the database, blob chunks, and exported and imported files.",
        &[
            (labels(&[("what", "database")]), u.db_bytes as f64),
            (labels(&[("what", "blobs")]), u.blob_bytes as f64),
            (labels(&[("what", "files")]), u.files_bytes as f64),
        ],
    );
    gauge(
        &mut out,
        "ridgewarden_chains",
        "Chains in the log: each station's main log and telemetry log.",
        &one(chains as f64),
    );
    gauge(
        &mut out,
        "ridgewarden_forks",
        "Forks seen: two events at one place in a chain, as when two boxes share a key.",
        &one(forks as f64),
    );
    gauge(
        &mut out,
        "ridgewarden_trusted_stations",
        "Stations whose events the apps believe, this one included.",
        &one(trusted as f64),
    );
    if let Some((ts, ok)) = last_poll {
        gauge(
            &mut out,
            "ridgewarden_node_up",
            "1 if the node answered the last poll.",
            &one(u8::from(ok)),
        );
        gauge(
            &mut out,
            "ridgewarden_node_last_poll_seconds",
            "When the node was last polled, as a Unix time.",
            &one(seconds(ts)),
        );
    }
    if let Some((_, obs)) = seen {
        gauge(
            &mut out,
            "ridgewarden_node_links",
            "Links in the node's last answer.",
            &one(obs.links.len() as f64),
        );
        gauge(
            &mut out,
            "ridgewarden_node_services",
            "Services on the mesh in the node's last answer.",
            &one(obs.services.len() as f64),
        );
    }
    let peer_labels = |p: &crate::app::Peer| {
        labels(&[("peer", &p.url), ("call", p.call.as_deref().unwrap_or(""))])
    };
    let up: Vec<_> = peers
        .iter()
        .filter(|p| p.last_ok.is_some() || p.last_err.is_some())
        .map(|p| (peer_labels(p), f64::from(u8::from(p.last_err.is_none()))))
        .collect();
    gauge(
        &mut out,
        "ridgewarden_peer_up",
        "1 if the last pull from the peer worked, 0 if it failed.",
        &up,
    );
    let synced: Vec<_> = peers
        .iter()
        .filter_map(|p| Some((peer_labels(p), seconds(p.last_ok?))))
        .collect();
    gauge(
        &mut out,
        "ridgewarden_peer_last_sync_seconds",
        "When a pull from the peer last worked, as a Unix time.",
        &synced,
    );
    gauge(
        &mut out,
        "ridgewarden_blobs",
        "Blobs held complete, and wanted ones still being fetched.",
        &[
            (labels(&[("state", "complete")]), blobs.0 as f64),
            (labels(&[("state", "fetching")]), blobs.1 as f64),
        ],
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_values_cannot_break_out_of_their_quotes() {
        assert_eq!(
            labels(&[("peer", "http://x/\"} 1\n"), ("call", "a\\b")]),
            r#"{peer="http://x/\"} 1\n",call="a\\b"}"#
        );
    }

    #[test]
    fn empty_families_are_left_out() {
        let mut out = String::new();
        gauge(&mut out, "ridgewarden_peer_up", "help", &[]);
        assert!(out.is_empty());
        gauge(&mut out, "ridgewarden_chains", "Chains.", &one(3.0));
        assert_eq!(
            out,
            "# HELP ridgewarden_chains Chains.\n# TYPE ridgewarden_chains gauge\nridgewarden_chains 3\n"
        );
    }
}
