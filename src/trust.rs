//! Who this site trusts, and the commands that change it: invites, joining, and the
//! club's signed trust list.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::json;

use crate::invite::{self, Station, valid_key};
use crate::log::{Log, club_keys};
use crate::util::now_ms;

/// Last known callsign for a key, from the heads it has written.
pub fn call_of(log: &Log, key: &str) -> Result<Option<String>> {
    Ok(log
        .store
        .conn()
        .query_row(
            "SELECT call FROM heads WHERE chain = ?1
             UNION ALL SELECT call FROM local_trust WHERE key = ?1 LIMIT 1",
            params![key],
            |r| r.get(0),
        )
        .optional()?)
}

/// Every trusted station with why: "this station", "config", "invite from X", "club list".
pub fn explain(log: &Log, pinned: &[String]) -> Result<Vec<(String, String, String)>> {
    let mut why: BTreeMap<String, String> = BTreeMap::new();
    if let Some(list) = log.club_list()? {
        for k in club_keys(&list) {
            why.insert(
                k,
                format!(
                    "club list (v{}, by {})",
                    list.payload["version"], list.author.call
                ),
            );
        }
    }
    {
        let c = log.store.conn();
        let mut st = c.prepare("SELECT key, via, provisional FROM local_trust")?;
        for r in st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })? {
            let (k, via, provisional) = r?;
            let w = if provisional {
                format!("listed in {via}'s invite (until the club list arrives)")
            } else {
                format!("invite from {via}")
            };
            why.entry(k).or_insert(w);
        }
    }
    for k in pinned {
        why.insert(k.to_ascii_lowercase(), "config".into());
    }
    why.insert(log.id.author.key.clone(), "this station".into());
    let trusted = log.trusted();
    let mut out = vec![];
    for (k, w) in why.into_iter().filter(|(k, _)| trusted.contains(k)) {
        let call = call_of(log, &k)?.unwrap_or_else(|| "?".into());
        out.push((call, k, w));
    }
    out.sort();
    Ok(out)
}

pub fn make_invite(log: &Log, url: &str) -> Result<String> {
    if !url.starts_with("http://") || url.len() >= 200 {
        bail!(
            "the invite URL must be this site's http:// address on the mesh, under 200 characters"
        );
    }
    let mut stations = vec![];
    for k in log.trusted() {
        if k == log.id.author.key {
            continue;
        }
        let call = call_of(log, &k)?.unwrap_or_else(|| "NOCALL".into());
        stations.push(Station { key: k, call });
    }
    invite::make(
        &log.id,
        vec![url.trim_end_matches('/').to_string()],
        stations,
        now_ms(),
    )
}

/// Accept an invite: trust its signer and everyone it lists, and remember its peers.
pub fn join(log: &Log, line: &str) -> Result<invite::Invite> {
    let inv = invite::read(line)?;
    if inv.by.key == log.id.author.key {
        bail!("that invite is from this station");
    }
    {
        let mut c = log.store.conn();
        let tx = c.transaction()?;
        // only the inviter is trusted to change the club list; the stations it listed are
        // trusted provisionally, until its club list syncs and takes over
        tx.execute(
            "INSERT INTO local_trust (key, call, via, added_ms, provisional) VALUES (?1, ?2, ?2, ?3, 0)
             ON CONFLICT(key) DO UPDATE SET call = excluded.call, provisional = 0",
            params![inv.by.key, inv.by.call, now_ms()],
        )?;
        for s in &inv.stations {
            if s.key == log.id.author.key || s.key == inv.by.key {
                continue;
            }
            tx.execute(
                "INSERT INTO local_trust (key, call, via, added_ms, provisional) VALUES (?1, ?2, ?3, ?4, 1)
                 ON CONFLICT(key) DO NOTHING",
                params![s.key, s.call, inv.by.call, now_ms()],
            )?;
        }
        for p in &inv.peers {
            tx.execute(
                "INSERT OR IGNORE INTO local_peers (url, added_ms) VALUES (?1, ?2)",
                params![p, now_ms()],
            )?;
        }
        tx.commit()?;
    }
    log.refresh_trust()?;
    Ok(inv)
}

pub fn local_peers(log: &Log) -> Result<Vec<String>> {
    let c = log.store.conn();
    let mut st = c.prepare("SELECT url FROM local_peers ORDER BY url")?;
    let rows = st.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Publish a new version of the club list with `key` added (or removed).
pub fn change_club_list(log: &Log, key: &str, call: Option<&str>, add: bool) -> Result<u64> {
    let key = key.trim().to_ascii_lowercase();
    if !valid_key(&key) {
        bail!(
            "{key:?} is not a public key (64 hex digits; see `ridgewarden whoami` on that station)"
        );
    }
    let current = log.club_list()?;
    let version = current
        .as_ref()
        .and_then(|e| e.payload["version"].as_u64())
        .unwrap_or(0)
        + 1;
    let mut stations: BTreeMap<String, String> = BTreeMap::new();
    if let Some(e) = &current {
        for s in e.payload["stations"].as_array().into_iter().flatten() {
            if let (Some(k), Some(c)) = (s["key"].as_str(), s["call"].as_str()) {
                stations.insert(k.to_string(), c.to_string());
            }
        }
    }
    stations.insert(log.id.author.key.clone(), log.id.author.call.clone());
    if add {
        let call = match call {
            Some(c) => c.trim().to_ascii_uppercase(),
            None => call_of(log, &key)?.ok_or_else(|| {
                anyhow::anyhow!("no callsign known for that key yet; give it with --call")
            })?,
        };
        if !crate::event::valid_call(&call) {
            bail!("{call:?} is not a callsign");
        }
        stations.insert(key, call);
    } else if stations.remove(&key).is_none() {
        bail!("that key is not on the club list");
    }
    let list: Vec<_> = stations
        .into_iter()
        .map(|(key, call)| json!({"key": key, "call": call}))
        .collect();
    log.append("trust.list", json!({"version": version, "stations": list}))?;
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use crate::store::Store;

    fn log(seed: u8, call: &str) -> Log {
        Log::new(
            Store::open_in_memory().unwrap(),
            Identity::from_seed(call, [seed; 32]),
        )
        .unwrap()
    }

    #[test]
    fn invite_then_club_list_spreads_trust() {
        let a = log(1, "N0AAA");
        let b = log(2, "N0BBB");
        let c = log(3, "N0CCC");
        // a puts b on the club list; c joins with a's invite and syncs a's log
        b.ingest(&a.events_after(&a.id.author.key, -1, None, 100).unwrap())
            .unwrap();
        a.ingest(&b.events_after(&b.id.author.key, -1, None, 100).unwrap())
            .unwrap();
        change_club_list(&a, &b.id.author.key, None, true).unwrap();
        let line = make_invite(&a, "http://10.0.0.1:8790").unwrap();
        join(&c, &line).unwrap();
        assert!(c.trusted().contains(&a.id.author.key), "the inviter");
        assert!(
            c.trusted().contains(&b.id.author.key),
            "listed in the invite"
        );
        assert_eq!(local_peers(&c).unwrap(), vec!["http://10.0.0.1:8790"]);

        // a later club-list change by a trusted station reaches c through sync
        let d = log(4, "N0DDD");
        change_club_list(&a, &d.id.author.key, Some("N0DDD"), true).unwrap();
        c.ingest(&a.events_after(&a.id.author.key, -1, None, 100).unwrap())
            .unwrap();
        assert!(c.trusted().contains(&d.id.author.key));
    }

    #[test]
    fn stations_listed_in_an_invite_cannot_change_the_club_list() {
        let a = log(1, "N0AAA");
        let b = log(2, "N0BBB");
        let c = log(3, "N0CCC");
        b.ingest(&a.events_after(&a.id.author.key, -1, None, 100).unwrap())
            .unwrap();
        a.ingest(&b.events_after(&b.id.author.key, -1, None, 100).unwrap())
            .unwrap();
        change_club_list(&a, &b.id.author.key, None, true).unwrap();
        join(&c, &make_invite(&a, "http://10.0.0.1:8790").unwrap()).unwrap();
        // a drops b; b, still holding its key, publishes a list adding a stranger
        change_club_list(&a, &b.id.author.key, None, false).unwrap();
        let stranger = "ee".repeat(32);
        change_club_list(&b, &stranger, Some("N0EVIL"), true).unwrap();
        c.ingest(&a.events_after(&a.id.author.key, -1, None, 100).unwrap())
            .unwrap();
        c.ingest(&b.events_after(&b.id.author.key, -1, None, 100).unwrap())
            .unwrap();
        assert!(
            !c.trusted().contains(&b.id.author.key),
            "removal took effect"
        );
        assert!(!c.trusted().contains(&stranger));
        assert!(c.trusted().contains(&a.id.author.key));
    }

    #[test]
    fn a_stranger_cannot_rewrite_the_club_list() {
        let a = log(1, "N0AAA");
        let stranger = log(9, "N0BAD");
        let victim = log(3, "N0CCC");
        victim.set_trusted([a.id.author.key.clone()]);
        change_club_list(&stranger, &"ee".repeat(32), Some("N0EVIL"), true).unwrap();
        victim
            .ingest(
                &stranger
                    .events_after(&stranger.id.author.key, -1, None, 100)
                    .unwrap(),
            )
            .unwrap();
        assert!(!victim.trusted().contains(&"ee".repeat(32)));
        assert!(!victim.trusted().contains(&stranger.id.author.key));
    }

    #[test]
    fn removing_from_the_club_list_takes_effect() {
        let a = log(1, "N0AAA");
        let k = "cd".repeat(32);
        change_club_list(&a, &k, Some("N0XYZ"), true).unwrap();
        assert!(a.trusted().contains(&k));
        change_club_list(&a, &k, None, false).unwrap();
        assert!(!a.trusted().contains(&k));
        assert!(change_club_list(&a, &k, None, false).is_err());
    }
}
