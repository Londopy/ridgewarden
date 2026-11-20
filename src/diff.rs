//! "What changed" (spec 00, M0): compare this node's sysinfo now against a day ago.

use std::collections::BTreeSet;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::aredn::{self, NodeObs};

pub struct Changes {
    pub from_ts: i64,
    pub to_ts: i64,
    pub node: String,
    pub lines: Vec<String>,
}

fn snapshot(conn: &Connection, sql: &str, p: i64) -> Result<Option<(i64, NodeObs)>> {
    let row: Option<(i64, String)> = conn
        .query_row(sql, params![p], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(match row {
        Some((ts, raw)) => serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| aredn::parse(&v).ok())
            .map(|o| (ts, o)),
        None => None,
    })
}

fn list(xs: &BTreeSet<String>) -> String {
    xs.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// Changes over the last `hours`, or None if there's no older snapshot to compare with.
pub fn changes(conn: &Connection, now: i64, hours: i64) -> Result<Option<Changes>> {
    let Some((to_ts, new)) = snapshot(
        conn,
        "SELECT ts, raw FROM snapshots WHERE ok = 1 AND ts <= ?1 ORDER BY ts DESC LIMIT 1",
        now,
    )?
    else {
        return Ok(None);
    };
    let cutoff = now - hours * 3_600_000;
    let old = match snapshot(
        conn,
        "SELECT ts, raw FROM snapshots WHERE ok = 1 AND ts <= ?1 ORDER BY ts DESC LIMIT 1",
        cutoff,
    )? {
        Some(o) => Some(o),
        // not a full day of history yet: compare with the oldest we have
        None => snapshot(
            conn,
            "SELECT ts, raw FROM snapshots WHERE ok = 1 AND ts > ?1 ORDER BY ts LIMIT 1",
            i64::MIN,
        )?,
    };
    let Some((from_ts, old)) = old.filter(|(ts, _)| *ts < to_ts) else {
        return Ok(None);
    };
    Ok(Some(Changes {
        from_ts,
        to_ts,
        node: new.node.clone(),
        lines: compare(&old, &new),
    }))
}

pub fn compare(old: &NodeObs, new: &NodeObs) -> Vec<String> {
    let mut lines = vec![];
    let set = |xs: Vec<String>| xs.into_iter().collect::<BTreeSet<String>>();
    let mut emit = |what: &str, a: BTreeSet<String>, b: BTreeSet<String>| {
        let added: BTreeSet<String> = b.difference(&a).cloned().collect();
        let gone: BTreeSet<String> = a.difference(&b).cloned().collect();
        if !added.is_empty() {
            lines.push(format!("New {what}: {}", list(&added)));
        }
        if !gone.is_empty() {
            lines.push(format!("Gone {what}: {}", list(&gone)));
        }
    };
    emit(
        "nodes on the mesh",
        set(old.hosts.clone()),
        set(new.hosts.clone()),
    );
    let svc = |o: &NodeObs| {
        set(o
            .services
            .iter()
            .map(|s| format!("{} ({})", s.name, s.host))
            .collect())
    };
    emit("services", svc(old), svc(new));
    let links = |o: &NodeObs| set(o.links.iter().map(|l| l.neighbor.clone()).collect());
    emit(&format!("links from {}", new.node), links(old), links(new));
    if old.firmware != new.firmware {
        lines.push(format!(
            "Firmware on {} changed: {} to {}",
            new.node,
            old.firmware.as_deref().unwrap_or("?"),
            new.firmware.as_deref().unwrap_or("?")
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aredn::{LinkObs, Service};

    fn obs(hosts: &[&str], links: &[&str], fw: &str) -> NodeObs {
        NodeObs {
            node: "me".into(),
            firmware: Some(fw.into()),
            hosts: hosts.iter().map(|s| s.to_string()).collect(),
            links: links
                .iter()
                .map(|n| LinkObs {
                    neighbor: n.to_string(),
                    ..Default::default()
                })
                .collect(),
            services: vec![Service {
                host: "a".into(),
                name: "Chat".into(),
                link: "http://a/".into(),
                protocol: "tcp".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn lists_what_changed() {
        let lines = compare(
            &obs(&["a", "b"], &["a"], "3.25.8.0"),
            &obs(&["a", "c"], &["a", "c"], "4.26.7.0"),
        );
        assert_eq!(
            lines,
            vec![
                "New nodes on the mesh: c",
                "Gone nodes on the mesh: b",
                "New links from me: c",
                "Firmware on me changed: 3.25.8.0 to 4.26.7.0",
            ]
        );
        assert!(compare(&obs(&["a"], &[], "x"), &obs(&["a"], &[], "x")).is_empty());
    }
}
