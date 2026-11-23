//! The node poller against fake 3.x and 4.x nodes over HTTP, and what it keeps of each poll.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use ridgewarden::app::App;
use ridgewarden::config::Config;
use ridgewarden::identity::Identity;
use ridgewarden::log::Log;
use ridgewarden::store::Store;
use ridgewarden::util::now_ms;
use ridgewarden::{diff, node};

const AREDN_3_25: &str =
    include_str!("../crates/ridgewarden-client/tests/fixtures/sysinfo-3.25.8.0.json");

/// A node that answers like 3.25 firmware until `upgraded`, then like 4.x, which
/// redirects the old path unless `old_gone`.
#[derive(Default)]
struct Node {
    upgraded: AtomicBool,
    old_gone: AtomicBool,
    new_path: AtomicUsize,
    old_path: AtomicUsize,
}

fn after_upgrade() -> Value {
    json!({
        "api_version": "2.0",
        "node": "TST-PASO",
        "node_details": {"firmware_version": "4.26.10.0", "model": "Ubiquiti Rocket M5 XW"},
        "link_info": {"10.54.1.4": {"hostname": "TST-NORTH", "linkType": "RF", "interface": "wlan0", "signal": -70, "noise": -95}},
        "lqm": {"info": {"now": 100, "trackers": {"02:00:0a:36:01:04": {
            "hostname": "tst-north", "type": "RF", "ip": "10.54.1.4", "lastseen": 100,
            "signal": -70, "snr": 25, "quality": 98, "rev_quality": 97}}}},
        "hosts": [{"name": "TST-NORTH", "ip": "10.54.1.4"}],
        "services": [],
    })
}

async fn new_path(State(n): State<Arc<Node>>) -> Response {
    n.new_path.fetch_add(1, SeqCst);
    if n.upgraded.load(SeqCst) {
        Json(after_upgrade()).into_response()
    } else {
        // what 3.x's web UI shows for a page it doesn't have
        Html("<!DOCTYPE html><html><body>status</body></html>").into_response()
    }
}

async fn old_path(State(n): State<Arc<Node>>, q: RawQuery) -> Response {
    n.old_path.fetch_add(1, SeqCst);
    if n.old_gone.load(SeqCst) {
        StatusCode::NOT_FOUND.into_response()
    } else if n.upgraded.load(SeqCst) {
        let q = q.0.unwrap_or_default();
        Redirect::temporary(&format!("/a/sysinfo?{q}")).into_response()
    } else {
        ([(CONTENT_TYPE, "application/json")], AREDN_3_25).into_response()
    }
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}

async fn fake_node() -> (Arc<Node>, String) {
    let node = Arc::new(Node::default());
    let router = Router::new()
        .route("/a/sysinfo", get(new_path))
        .route("/cgi-bin/sysinfo.json", get(old_path))
        .with_state(node.clone());
    (node, serve(router).await)
}

fn app(node_url: &str, dir: &Path) -> App {
    let mut cfg: Config = toml::from_str(&Config::example("N0AAA", dir)).unwrap();
    cfg.node.url = node_url.into();
    cfg.sync.discover = false;
    let store = Store::open(&cfg.db_path()).unwrap();
    App::new(
        cfg,
        Log::new(store, Identity::from_seed("N0AAA", [1; 32])).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn a_3_25_node_is_polled_at_its_old_path_and_kept_parsed() {
    let (node, url) = fake_node().await;
    let dir = tempfile::tempdir().unwrap();
    let a = app(&url, dir.path());

    let ev = a.poll_once().await.unwrap();
    assert_eq!(ev.payload["ok"], true, "{}", ev.payload);
    assert_eq!(ev.payload["api_version"], "1.14");
    assert_eq!(ev.payload["links"].as_array().unwrap().len(), 4);
    assert_eq!(a.node_name(), "tst-paso");
    a.poll_once().await.unwrap();
    assert_eq!(
        (node.new_path.load(SeqCst), node.old_path.load(SeqCst)),
        (1, 2),
        "the new path is tried once, then the one that answered is remembered"
    );

    let conn = a.log.store.conn();
    assert_eq!(node::last_poll(&conn).unwrap().map(|p| p.1), Some(true));
    let (_, seen) = node::seen_by(&conn, now_ms()).unwrap().unwrap();
    assert_eq!(seen.firmware.as_deref(), Some("3.25.8.0"));
    let north = seen
        .links
        .iter()
        .find(|l| l.neighbor == "tst-north")
        .unwrap();
    assert_eq!((north.snr, north.quality), (Some(24.0), Some(99.0)));
    let services: Vec<(&str, &str)> = seen
        .services
        .iter()
        .map(|s| (s.name.as_str(), s.host.as_str()))
        .collect();
    assert!(
        services.contains(&("Mesh Chat", "tst-shack")),
        "{services:?}"
    );
    assert_eq!(seen.hosts.len(), 7);
    let raw: i64 = conn
        .query_row("SELECT count(*) FROM snapshots WHERE ok = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(raw, 2, "the raw replies are kept too, for a while");
}

#[tokio::test]
async fn an_upgraded_node_is_followed_through_its_redirect() {
    let (node, url) = fake_node().await;
    let dir = tempfile::tempdir().unwrap();
    let a = app(&url, dir.path());
    a.poll_once().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    node.upgraded.store(true, SeqCst);
    let ev = a.poll_once().await.unwrap();
    assert_eq!(ev.payload["api_version"], "2.0", "{}", ev.payload);
    assert_eq!(
        node.old_path.load(SeqCst),
        2,
        "asked the remembered path, which now redirects"
    );
    let ch = diff::changes(&a.log.store.conn(), now_ms(), 24)
        .unwrap()
        .unwrap();
    assert!(
        ch.lines
            .contains(&"Firmware on tst-paso changed: 3.25.8.0 to 4.26.10.0".to_string()),
        "{:?}",
        ch.lines
    );
    assert!(
        ch.lines
            .iter()
            .any(|l| l.starts_with("Gone links from tst-paso:") && l.contains("tst-hilltop")),
        "{:?}",
        ch.lines
    );
}

#[tokio::test]
async fn a_redirect_to_another_host_is_not_followed() {
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let h = hits.clone();
    let router = Router::new()
        .route("/a/sysinfo", get(|| async { StatusCode::NOT_FOUND }))
        .route(
            "/cgi-bin/sysinfo.json",
            get(move || async move {
                Redirect::temporary(&format!("http://localhost:{port}/elsewhere"))
            }),
        )
        .route(
            "/elsewhere",
            get(move || async move {
                h.fetch_add(1, SeqCst);
                Json(after_upgrade())
            }),
        );
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let a = app(&format!("http://127.0.0.1:{port}"), dir.path());
    let ev = a.poll_once().await.unwrap();
    assert_eq!(ev.payload["ok"], false, "{}", ev.payload);
    assert_eq!(hits.load(SeqCst), 0);
    assert_eq!(
        node::last_poll(&a.log.store.conn()).unwrap().map(|p| p.1),
        Some(false)
    );
}

#[tokio::test]
async fn a_remembered_old_path_that_is_gone_falls_back_to_the_new_one() {
    let (node, url) = fake_node().await;
    let dir = tempfile::tempdir().unwrap();
    let a = app(&url, dir.path());
    a.poll_once().await.unwrap();
    node.upgraded.store(true, SeqCst);
    node.old_gone.store(true, SeqCst);
    let ev = a.poll_once().await.unwrap();
    assert_eq!(ev.payload["api_version"], "2.0", "{}", ev.payload);
    a.poll_once().await.unwrap();
    assert_eq!(
        (node.new_path.load(SeqCst), node.old_path.load(SeqCst)),
        (3, 2),
        "the old path is asked once more, then the new one is remembered"
    );
}

/// A node at `/a/sysinfo` that answers with `body`.
async fn answering(body: impl IntoResponse + Clone + Send + Sync + 'static) -> String {
    serve(Router::new().route(
        "/a/sysinfo",
        get(move || {
            let body = body.clone();
            async move { body.into_response() }
        }),
    ))
    .await
}

#[tokio::test]
async fn a_poll_that_goes_wrong_after_the_node_answered_is_still_logged() {
    let dir = tempfile::tempdir().unwrap();
    let a = app(
        &answering(Json(json!({"hello": "world"}))).await,
        dir.path(),
    );
    let ev = a.poll_once().await.unwrap();
    assert_eq!(ev.payload["ok"], false);
    assert!(
        ev.payload["error"]
            .as_str()
            .unwrap()
            .contains("no node name"),
        "{}",
        ev.payload
    );
    assert_eq!(
        node::last_poll(&a.log.store.conn()).unwrap().map(|p| p.1),
        Some(false)
    );

    let huge = answering(" ".repeat(9 << 20)).await;
    let dir = tempfile::tempdir().unwrap();
    let a = app(&huge, dir.path());
    let ev = a.poll_once().await.unwrap();
    assert_eq!(ev.payload["ok"], false);
    assert!(
        ev.payload["error"].as_str().unwrap().contains("over"),
        "{}",
        ev.payload
    );
}

#[tokio::test]
async fn a_service_list_too_big_for_an_event_stays_here_and_polls_go_on() {
    let services: Vec<Value> = (0..3000)
        .map(|i| {
            json!({
                "name": format!("Service {i:04} of the big mesh"),
                "ip": "tst-big",
                "link": format!("http://tst-big.local.mesh:8080/service/{i:04}/index.html"),
                "protocol": "tcp",
            })
        })
        .collect();
    let reply = json!({
        "api_version": "2.0",
        "node": "TST-BIG",
        "services": services,
        "hosts": [{"name": "tst-big", "ip": "10.0.0.1"}],
    });
    let dir = tempfile::tempdir().unwrap();
    let a = app(&answering(Json(reply)).await, dir.path());
    for _ in 0..2 {
        let ev = a.poll_once().await.unwrap();
        assert_eq!(ev.payload["ok"], true, "{}", ev.payload["error"]);
    }
    let conn = a.log.store.conn();
    let lists: Vec<usize> = conn
        .prepare("SELECT json FROM events WHERE kind = 'mesh.list'")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|j| {
            serde_json::from_str::<Value>(&j.unwrap()).unwrap()["payload"]["list"]
                .as_array()
                .unwrap()
                .len()
        })
        .collect();
    assert_eq!(lists, [1], "only the host list went out");
    let (_, seen) = node::seen_by(&conn, now_ms()).unwrap().unwrap();
    assert_eq!(
        seen.services.len(),
        3000,
        "this site's own pages still have it"
    );
}

#[test]
fn raw_replies_from_before_the_tables_are_parsed_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ridgewarden.db");
    let now = now_ms();
    {
        let s = Store::open(&db).unwrap();
        let c = s.conn();
        c.execute(
            "INSERT INTO snapshots (node, ts, ok, raw) VALUES ('tst-paso', ?1, 1, ?2)",
            rusqlite::params![now - 3_600_000, AREDN_3_25],
        )
        .unwrap();
        c.execute(
            "INSERT INTO snapshots (node, ts, ok, raw) VALUES ('tst-paso', ?1, 0, NULL)",
            rusqlite::params![now - 60_000],
        )
        .unwrap();
    }
    let s = Store::open(&db).unwrap();
    let c = s.conn();
    assert_eq!(node::last_poll(&c).unwrap(), Some((now - 60_000, false)));
    let (ts, seen) = node::seen_by(&c, now).unwrap().unwrap();
    assert_eq!(ts, now - 3_600_000);
    assert_eq!(seen.links.len(), 4);
    assert_eq!(seen.services.len(), 3);
    drop(c);
    drop(s);
    let again = Store::open(&db).unwrap();
    let polls: i64 = again
        .conn()
        .query_row("SELECT count(*) FROM node_polls", [], |r| r.get(0))
        .unwrap();
    assert_eq!(polls, 2, "parsed once, not again on every open");
}
