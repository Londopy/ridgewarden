//! Pull-only gossip sync over plain HTTP.
//!
//! An agent only ever asks; it never pushes. So no peer can flood another, and a slow
//! RF link just means a slower pull, not a backlog on the far side. Everything a peer
//! sends back is bounded (bytes, events, authors) and checked as it arrives, so a broken
//! or hostile peer costs at most one capped pull per round.
//!
//!   GET /v1/heads                                  what the peer holds, per author
//!   GET /v1/events?author=<key>&after=<n>&after_id=<id>   that author's events past a cursor
//!   GET /v1/events?author=<key>&seq=<n>            every event at one seq (fork branches)

use anyhow::{Result, bail, ensure};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::app::App;
use crate::event::Event;
use crate::log::{DIGEST_BUCKET, Head, IngestReport, Log, PAGE_BYTES};
use crate::util::now_ms;

pub const PAGE: usize = 500;
/// Events taken from one peer in one pull; the rest waits for the next round.
pub const MAX_EVENTS_PER_PULL: usize = 20_000;
/// Authors looked at in one heads response.
pub const MAX_AUTHORS: usize = 2_000;
/// Events taken from one chain in one pull, so no single chain can use the whole budget.
pub const MAX_EVENTS_PER_CHAIN: usize = 2_000;
/// Longest a single pull from one peer may run.
pub const PULL_TIME: std::time::Duration = std::time::Duration::from_secs(120);
/// Fork points followed per author per pull.
pub const MAX_FORKS: usize = 64;
/// Runs of missing seqs asked about per chain per pull.
pub const MAX_HOLES: usize = 8;
/// Response bodies larger than this are refused (a full page is at most PAGE_BYTES plus one event).
const MAX_BODY: usize = PAGE_BYTES + 512 * 1024;

#[derive(Deserialize)]
struct Heads {
    heads: Vec<Head>,
}

#[derive(Deserialize)]
struct Digests {
    bucket: i64,
    digests: std::collections::BTreeMap<i64, String>,
}

/// Which part of a chain to fetch: seqs in (after, last], `page` events per request, at
/// most `limit` events in all.
struct Span {
    after: i64,
    last: i64,
    page: usize,
    limit: usize,
}

/// Page through `key`'s events in `span`, ingesting as we go.
/// Returns how many events were received.
async fn page_range(
    log: &Log,
    http: &reqwest::Client,
    base: &str,
    key: &str,
    span: Span,
    total: &mut IngestReport,
) -> Result<usize> {
    let Span {
        after,
        last,
        page,
        limit,
    } = span;
    let mut cursor: (i64, Option<String>) = (after, None);
    let mut taken = 0;
    loop {
        let mut q = vec![
            ("author", key.to_string()),
            ("after", cursor.0.to_string()),
            ("limit", page.to_string()),
        ];
        if let Some(id) = &cursor.1 {
            q.push(("after_id", id.clone()));
        }
        let evs: Vec<Event> =
            get_json(http.get(format!("{base}/v1/events")).query(&q), MAX_BODY).await?;
        let Some(end) = evs.last() else { break };
        // a page must be this author's, in strictly increasing (seq, id) order past the
        // cursor; anything else is a broken peer and we stop asking it.
        // "g" sorts after every hex id: with no id, the cursor is past the whole seq
        let mut prev = (cursor.0, cursor.1.clone().unwrap_or_else(|| "g".into()));
        for e in &evs {
            ensure!(e.chain() == key, "peer sent an event from another chain");
            let here = (e.seq as i64, e.id.clone());
            ensure!(here > prev, "peer page is out of order");
            prev = here;
        }
        cursor = (end.seq as i64, Some(end.id.clone()));
        taken += evs.len();
        add(total, log.ingest(&evs)?);
        if evs.len() < page || cursor.0 >= last || taken >= limit {
            break;
        }
    }
    Ok(taken)
}

/// GET JSON whose body may be at most `max` bytes.
pub async fn get_json<T: DeserializeOwned>(req: reqwest::RequestBuilder, max: usize) -> Result<T> {
    let mut resp = req.send().await?.error_for_status()?;
    if resp.content_length().is_some_and(|n| n > max as u64) {
        bail!("the reply is over {max} bytes");
    }
    // read in chunks so a lying or missing Content-Length can't make us buffer forever
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        ensure!(body.len() <= max, "the reply is over {max} bytes");
    }
    Ok(serde_json::from_slice(&body)?)
}

fn add(total: &mut IngestReport, r: IngestReport) {
    total.accepted += r.accepted;
    total.duplicate += r.duplicate;
    total.rejected += r.rejected;
}

/// Pull everything `base` has that we don't, author by author.
/// Pull what `base` has that we don't. With `telemetry` off, stations' telemetry logs
/// (polls, probes) are skipped whole, which keeps a data-mule car's load to what matters.
pub async fn pull_from(
    log: &Log,
    http: &reqwest::Client,
    base: &str,
    telemetry: bool,
) -> Result<IngestReport> {
    let base = base.trim_end_matches('/');
    let heads: Heads = get_json(http.get(format!("{base}/v1/heads")), MAX_BODY).await?;
    let mut total = IngestReport::default();
    let mut taken = 0usize;
    // a random subset each time, so chains that sort late are not starved by early ones
    let all = heads.heads;
    for i in pick(all.len(), MAX_AUTHORS) {
        let h = &all[i];
        if taken >= MAX_EVENTS_PER_PULL {
            break;
        }
        if log.is_blocked(&h.key) || (!telemetry && h.key.contains(':')) {
            continue;
        }
        // new events: page from the highest seq we hold, so a hole we can't fill is not
        // paid for again with the whole tail above it
        let held = log.max_seq(&h.key)?;
        if h.max_seq > held {
            let span = Span {
                after: held,
                last: i64::MAX,
                page: PAGE,
                limit: MAX_EVENTS_PER_CHAIN,
            };
            taken += page_range(log, http, base, &h.key, span, &mut total).await?;
        }
        // holes below that, if the peer's own gap-free run reaches them
        for (first, len) in log.holes(&h.key, MAX_HOLES)? {
            if h.seq < first {
                continue;
            }
            let page = (len as usize).min(PAGE);
            let span = Span {
                after: first - 1,
                last: first + len - 1,
                page,
                limit: page,
            };
            taken += page_range(log, http, base, &h.key, span, &mut total).await?;
        }
        // the other branch of any fork the peer holds and we don't
        for seq in h.forks.iter().copied().take(MAX_FORKS) {
            if log.count_at(&h.key, seq)? >= 2 {
                continue;
            }
            let q = [("author", h.key.clone()), ("seq", seq.to_string())];
            let evs: Vec<Event> =
                get_json(http.get(format!("{base}/v1/events")).query(&q), MAX_BODY).await?;
            ensure!(
                evs.iter()
                    .all(|e| e.chain() == h.key && e.seq as i64 == seq),
                "peer sent the wrong events for a fork"
            );
            taken += evs.len();
            add(&mut total, log.ingest(&evs)?);
        }
        // same chain length, different events: one key on two boxes. Compare digests per
        // range of seqs and fetch only the ranges that differ.
        if !h.digest.is_empty() && log.digest(&h.key)? != h.digest && taken < MAX_EVENTS_PER_PULL {
            let theirs: Digests = get_json(
                http.get(format!("{base}/v1/digests"))
                    .query(&[("author", h.key.as_str())]),
                MAX_BODY,
            )
            .await?;
            ensure!(
                theirs.bucket == DIGEST_BUCKET,
                "peer uses a different digest bucket size"
            );
            let ours = log.bucket_digests(&h.key, h.max_seq)?;
            for (start, d) in theirs.digests.iter().take(MAX_AUTHORS) {
                if ours.get(start) == Some(d) || start.rem_euclid(DIGEST_BUCKET) != 0 {
                    continue;
                }
                let span = Span {
                    after: start - 1,
                    last: start + DIGEST_BUCKET - 1,
                    page: PAGE,
                    limit: MAX_EVENTS_PER_CHAIN,
                };
                taken += page_range(log, http, base, &h.key, span, &mut total).await?;
                if taken >= MAX_EVENTS_PER_PULL {
                    break;
                }
            }
        }
    }
    Ok(total)
}

/// Pick up to `k` of `n` indexes at random.
pub fn pick(n: usize, k: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        let r = getrandom::u64().unwrap_or(i as u64) as usize % (i + 1);
        idx.swap(i, r);
    }
    idx.truncate(k);
    idx
}

/// One gossip round: pull from `fanout` random known peers.
pub async fn round(app: &App) {
    // `ridgewarden trust` and `join` change the database from another process
    if let Err(e) = app.log.refresh_trust() {
        tracing::warn!("working out trusted stations: {e:#}");
    }
    let urls: Vec<String> = app.peers.lock().unwrap().keys().cloned().collect();
    for i in pick(urls.len(), app.cfg.sync.fanout.max(1)) {
        let url = &urls[i];
        // a slow or hostile peer gets a bounded slice of time, not the whole sync loop
        let res = match tokio::time::timeout(
            PULL_TIME,
            pull_from(&app.log, &app.http, url, app.cfg.sync.telemetry),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "pull took longer than {}s",
                PULL_TIME.as_secs()
            )),
        };
        let named = app
            .peers
            .lock()
            .unwrap()
            .get(url)
            .is_some_and(|p| p.key.is_some());
        // the first good pull also learns who the peer is, from the signed identity.bind
        // that starts its own chain
        let who = match &res {
            Ok(_) if !named => crate::app::verify_peer(&app.http, url)
                .await
                .inspect_err(|e| tracing::debug!("{url} did not say who it is: {e:#}"))
                .ok(),
            _ => None,
        };
        let mut peers = app.peers.lock().unwrap();
        let Some(p) = peers.get_mut(url) else {
            continue;
        };
        if let Some(bind) = who {
            p.call = Some(bind.author.call);
            p.key = Some(bind.author.key);
        }
        match res {
            Ok(rep) => {
                p.last_ok = Some(now_ms());
                p.last_err = None;
                if rep.accepted > 0 || rep.rejected > 0 {
                    tracing::info!(
                        "pulled from {url}: {} new, {} rejected",
                        rep.accepted,
                        rep.rejected
                    );
                }
            }
            Err(e) => {
                p.last_err = Some(crate::app::short_err(&e));
                tracing::debug!("pull from {url} failed: {e:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_is_a_subset_without_repeats() {
        let mut p = pick(10, 4);
        assert_eq!(p.len(), 4);
        p.sort();
        p.dedup();
        assert_eq!(p.len(), 4);
        assert!(p.iter().all(|&i| i < 10));
        assert_eq!(pick(2, 5).len(), 2);
        assert!(pick(0, 3).is_empty());
    }
}
