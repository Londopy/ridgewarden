//! Wiring: open everything and run the poll, crawl and sync loops next to the web server.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::app::App;
use crate::config::Config;
use crate::identity::Identity;
use crate::log::Log;
use crate::store::Store;
use crate::{sync, web};

/// The token apps present to `/local/v1/append`. Created once, readable by the agent's
/// user and group: each app runs as its own user in that group, so it can sign events
/// through the core without being able to read the station key.
pub fn ensure_token(cfg: &Config) -> Result<String> {
    let p = cfg.token_path();
    if let Ok(t) = std::fs::read_to_string(&p)
        && t.trim().len() >= 32
    {
        // tokens made before apps had their own users were owner-only
        if let Err(e) = crate::identity::set_mode(&p, 0o640) {
            tracing::warn!("could not let apps read {}: {e:#}", p.display());
        }
        return Ok(t.trim().to_string());
    }
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    let t = hex::encode(b);
    std::fs::create_dir_all(&cfg.data_dir)?;
    let tmp = p.with_extension("tmp");
    crate::identity::write_group(&tmp, &t)?;
    std::fs::rename(&tmp, &p)?;
    Ok(t)
}

pub fn open(cfg: Config) -> Result<App> {
    let id = Identity::load_or_create(&cfg.key_path(), &cfg.callsign)?;
    let store = Store::open(&cfg.db_path())
        .with_context(|| format!("opening {}", cfg.db_path().display()))?;
    let log = Log::new(store, id)?;
    log.set_trusted(cfg.trust.keys.iter().cloned());
    log.set_blocked(cfg.trust.blocked.iter().cloned())?;
    // apps (in the agent's group) drop files into imports and take blobs out of exports;
    // setgid keeps files in both in the agent's group whoever writes them
    for dir in [cfg.imports_dir(), cfg.data_dir.join("exports")] {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        // a folder made by a root-run command may refuse this; apps then can't use it,
        // which they'll report, but the agent itself keeps running
        if let Err(e) = crate::identity::set_mode(&dir, 0o2770) {
            tracing::warn!("could not open {} to apps: {e:#}", dir.display());
        }
    }
    App::new(cfg, log)
}

/// `base` seconds, give or take `pct` percent, so agents that started together drift apart
/// instead of polling and syncing in lockstep.
fn jittered(base: u64, pct: u64) -> Duration {
    let spread = base * 1000 * pct / 100;
    let r = getrandom::u64().unwrap_or(0) % (2 * spread + 1);
    Duration::from_millis(base * 1000 - spread + r)
}

/// One blob round: pick up auto-wanted blobs, then fetch for a minute's worth of the
/// configured rate, if we're inside the fetch window.
async fn blob_round(app: &App) -> Result<()> {
    let n = crate::blobs::auto_want(
        &app.log.store.conn(),
        &app.log.trusted(),
        &app.cfg.blobs.auto_want,
    )?;
    if n > 0 {
        tracing::info!("{n} new blob(s) queued for fetching");
    }
    let hour = (crate::util::now_ms() / 3_600_000).rem_euclid(24) as u32;
    if !crate::blobs::in_window(app.cfg.blobs.window()?, hour) {
        return Ok(());
    }
    let peers: Vec<String> = app.peers.lock().unwrap().keys().cloned().collect();
    let pace = crate::blobs::Pace::new(&app.cfg.blobs);
    let got = crate::blobs::fetch_round(
        &app.log,
        &app.blobs,
        &app.blob_http,
        &peers,
        pace.bytes_per_s * 60,
        &pace,
    )
    .await?;
    if got > 0 {
        tracing::debug!("fetched {got} bytes of blobs");
    }
    Ok(())
}

pub fn spawn_loops(app: Arc<App>) {
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = blob_round(&a).await {
                tracing::warn!("blob round failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        let keep_ms = a.cfg.retention.telemetry_days as i64 * 86_400_000;
        loop {
            match a.log.checkpoint_telemetry(keep_ms) {
                Ok(Some(ev)) => {
                    tracing::info!("telemetry checkpoint: before #{}", ev.payload["before"])
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("telemetry checkpoint failed: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = a.poll_once().await {
                tracing::warn!("poll failed: {e:#}");
            }
            tokio::time::sleep(jittered(a.cfg.node.poll_interval_s, 10)).await;
        }
    });
    if app.cfg.crawl.enabled {
        let a = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(20)).await;
            loop {
                crawl_round(&a).await;
                tokio::time::sleep(jittered(a.cfg.crawl.interval_s, 10)).await;
            }
        });
    }
    let a = app;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(jittered(a.cfg.sync.interval_s, 20)).await;
            sync::round(&a).await;
        }
    });
}

/// Which nodes to crawl this round: the configured list, plus (optionally) every node
/// reported as a neighbor in the last hour, minus our own node.
pub fn crawl_targets(app: &App) -> Result<Vec<String>> {
    let me = app.node_name();
    let mut set: std::collections::BTreeSet<String> = app
        .cfg
        .crawl
        .nodes
        .iter()
        .map(|n| crate::util::norm_node(n))
        .collect();
    if app.cfg.crawl.follow_links {
        // neighbors named in our own recent polls (LAN or crawl), read from the log itself
        let c = app.log.store.conn();
        let mut st = c.prepare(
            "SELECT json FROM events WHERE kind = 'node.poll' AND author_key = ?1 AND seen_ms > ?2",
        )?;
        let rows = st.query_map(
            rusqlite::params![app.log.id.author.key, crate::util::now_ms() - 3_600_000],
            |r| r.get::<_, String>(0),
        )?;
        for j in rows {
            let e: crate::event::Event = serde_json::from_str(&j?)?;
            for l in e
                .payload
                .get("links")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(n) = l.get("neighbor").and_then(|v| v.as_str()) {
                    set.insert(crate::util::norm_node(n));
                }
            }
        }
    }
    set.remove(&me);
    set.retain(|n| crate::util::valid_node(n));
    Ok(set.into_iter().take(app.cfg.crawl.max_nodes).collect())
}

async fn crawl_round(app: &Arc<App>) {
    let targets = match crawl_targets(app) {
        Ok(t) => t,
        Err(e) => return tracing::warn!("crawl: {e:#}"),
    };
    // a few at a time: this is other people's hardware
    for chunk in targets.chunks(4) {
        let mut set = tokio::task::JoinSet::new();
        for n in chunk {
            let (a, n) = (app.clone(), n.clone());
            set.spawn(async move { (a.crawl_node(&n).await, n) });
        }
        while let Some(Ok((r, n))) = set.join_next().await {
            if let Err(e) = r {
                tracing::warn!("crawl of {n} failed: {e:#}");
            }
        }
    }
}

pub async fn run(cfg: Config) -> Result<()> {
    let listen = cfg.listen;
    let token = ensure_token(&cfg)?;
    let app = Arc::new(open(cfg)?);
    tracing::info!(
        "ridgewarden {} as {} ({}), node {}, dashboard on http://{listen}",
        env!("CARGO_PKG_VERSION"),
        app.log.id.author.call,
        &app.log.id.author.key[..16],
        app.cfg.node.url
    );
    spawn_loops(app.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    axum::serve(
        listener,
        web::router(app, Some(token)).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..200 {
            let d = jittered(100, 20).as_millis();
            assert!((80_000..=120_000).contains(&d), "{d}");
        }
    }
}
