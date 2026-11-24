//! Spec 00, M1: "event log + 2-agent sync + forced partition test passes with zero lost
//! events". Agents run real HTTP servers; a partition makes a server refuse every request.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use ridgewarden::app::App;
use ridgewarden::config::Config;
use ridgewarden::identity::Identity;
use ridgewarden::log::Log;
use ridgewarden::store::Store;
use ridgewarden::{sync, web};

const TOKEN: &str = "test-token-0123456789abcdef0123456789abcdef";

struct Agent {
    app: Arc<App>,
    url: String,
    cut: Arc<AtomicBool>,
    /// chunk requests this agent's server has seen
    chunks: Arc<AtomicU64>,
    _dir: tempfile::TempDir,
}

async fn count_chunks(State(n): State<Arc<AtomicU64>>, req: Request, next: Next) -> Response {
    if req.uri().path().starts_with("/v1/chunk/") {
        n.fetch_add(1, Ordering::SeqCst);
    }
    next.run(req).await
}

async fn refuse_when_cut(State(cut): State<Arc<AtomicBool>>, req: Request, next: Next) -> Response {
    if cut.load(Ordering::SeqCst) {
        return (StatusCode::SERVICE_UNAVAILABLE, "partitioned").into_response();
    }
    next.run(req).await
}

async fn agent(call: &str, seed: u8) -> Agent {
    agent_with(call, seed, |_| {}).await
}

async fn agent_with(call: &str, seed: u8, set: impl FnOnce(&mut Config)) -> Agent {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str(&Config::example(call, dir.path())).unwrap();
    set(&mut cfg);
    let store = Store::open(&cfg.db_path()).unwrap();
    let log = Log::new(store, Identity::from_seed(call, [seed; 32])).unwrap();
    let app = Arc::new(App::new(cfg, log).unwrap());
    let cut = Arc::new(AtomicBool::new(false));
    let chunks = Arc::new(AtomicU64::new(0));
    let router = web::router(app.clone(), Some(TOKEN.into()))
        .layer(middleware::from_fn_with_state(cut.clone(), refuse_when_cut))
        .layer(middleware::from_fn_with_state(chunks.clone(), count_chunks));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    Agent {
        app,
        url,
        cut,
        chunks,
        _dir: dir,
    }
}

/// `app` served without connection info, the way the local API's gate sees a caller it
/// can't place as this machine.
async fn unplaced(app: &Arc<App>) -> String {
    let router = web::router(app.clone(), Some(TOKEN.into()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router.into_make_service())
            .await
            .unwrap()
    });
    url
}

impl Agent {
    async fn pull(&self, from: &Agent) -> anyhow::Result<ridgewarden::log::IngestReport> {
        sync::pull_from(&self.app.log, &self.app.http, &from.url, true).await
    }
    fn write(&self, n: usize, tag: &str) {
        for i in 0..n {
            self.app
                .log
                .append("test.note", json!({"tag": tag, "i": i}))
                .unwrap();
        }
    }
    fn ids(&self) -> BTreeSet<String> {
        self.app.log.event_ids().unwrap().into_iter().collect()
    }
}

#[tokio::test]
async fn two_agents_survive_a_partition_with_zero_lost_events() {
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;

    a.write(20, "before");
    b.write(20, "before");
    a.pull(&b).await.unwrap();
    b.pull(&a).await.unwrap();
    assert_eq!(a.ids(), b.ids());
    assert_eq!(a.ids().len(), 2 + 40);

    // cut the link: both sides keep working and keep writing
    a.cut.store(true, Ordering::SeqCst);
    b.cut.store(true, Ordering::SeqCst);
    for round in 0..5 {
        a.write(10, &format!("cut-{round}"));
        b.write(10, &format!("cut-{round}"));
        assert!(a.pull(&b).await.is_err(), "pull across the cut must fail");
        assert!(b.pull(&a).await.is_err());
    }
    assert_ne!(a.ids(), b.ids());
    let union: BTreeSet<String> = a.ids().union(&b.ids()).cloned().collect();

    // heal
    a.cut.store(false, Ordering::SeqCst);
    b.cut.store(false, Ordering::SeqCst);
    let ra = a.pull(&b).await.unwrap();
    let rb = b.pull(&a).await.unwrap();
    assert_eq!(ra.accepted, 50);
    assert_eq!(rb.accepted, 50);
    assert_eq!(ra.rejected + rb.rejected, 0);

    assert_eq!(a.ids(), b.ids(), "both sides converge on the same set");
    assert_eq!(a.ids(), union, "nothing written on either side was lost");
    assert_eq!(a.ids().len(), 2 + 40 + 100);
    for x in [&a, &b] {
        assert_eq!(x.app.log.fork_count().unwrap(), 0);
        for h in x.app.log.heads().unwrap() {
            assert_eq!(h.seq, 70, "{} has a gap-free chain", h.call);
        }
    }

    // a second pull after convergence moves nothing
    assert_eq!(a.pull(&b).await.unwrap().accepted, 0);
}

#[tokio::test]
async fn events_relay_through_a_middle_agent() {
    // a - b - c, with a and c never talking directly
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;
    let c = agent("N0CCC", 3).await;
    a.write(5, "a");
    c.write(7, "c");
    b.pull(&a).await.unwrap();
    b.pull(&c).await.unwrap();
    c.pull(&b).await.unwrap();
    a.pull(&b).await.unwrap();
    assert_eq!(a.ids(), c.ids());
    assert_eq!(a.ids(), b.ids());
    assert_eq!(a.ids().len(), 3 + 12);
}

#[tokio::test]
async fn large_backlog_pulls_in_pages() {
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;
    a.write(sync::PAGE * 2 + 37, "bulk");
    let r = b.pull(&a).await.unwrap();
    assert_eq!(r.accepted, sync::PAGE * 2 + 37 + 1);
    assert_eq!(a.ids(), b.ids().intersection(&a.ids()).cloned().collect());
}

#[tokio::test]
async fn partition_during_a_pull_resumes_where_it_left_off() {
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;
    a.write(sync::PAGE + 10, "bulk");
    // hand b a partial copy, as if the link died mid-transfer
    let first = a
        .app
        .log
        .events_after(&a.app.log.id.author.key, -1, None, 100)
        .unwrap();
    b.app.log.ingest(&first).unwrap();
    let r = b.pull(&a).await.unwrap();
    assert_eq!(r.accepted, sync::PAGE + 11 - 100);
    assert_eq!(
        r.duplicate, 0,
        "resumed after the last event it had, no refetching"
    );
    assert!(a.ids().is_subset(&b.ids()));
}

#[tokio::test]
async fn forks_converge_over_http() {
    // one key on two boxes (a cloned SD card): x and y each write their own seq 1 and 2
    let x = agent("N0AAA", 1).await;
    let y = agent("N0AAA", 1).await;
    let obs = agent("N0BBB", 2).await;
    x.write(2, "x");
    y.write(2, "y");
    // everyone pulls from everyone, a couple of rounds
    for _ in 0..2 {
        for (to, from) in [
            (&obs, &x),
            (&obs, &y),
            (&x, &y),
            (&y, &x),
            (&x, &obs),
            (&y, &obs),
        ] {
            to.pull(from).await.unwrap();
        }
    }
    assert_eq!(x.ids(), y.ids(), "both branches reach both boxes");
    assert_eq!(x.ids(), obs.ids());
    // a fresh agent that only ever talks to the observer still gets both branches
    let late = agent("N0CCC", 3).await;
    late.pull(&obs).await.unwrap();
    assert!(obs.ids().is_subset(&late.ids()));
    assert!(late.app.log.fork_count().unwrap() >= 1);
}

#[tokio::test]
async fn every_page_and_api_answers() {
    let a = agent("N0AAA", 1).await;
    a.write(3, "x");
    let http = reqwest::Client::new();
    let key = a.app.log.id.author.key.clone();
    for path in [
        "/".to_string(),
        "/changes".into(),
        "/changes?hours=1".into(),
        "/healthz".into(),
        "/v1/heads".into(),
        format!("/v1/events?author={key}&after=0"),
        format!("/v1/events?author={key}&seq=1"),
        "/v1/identity".into(),
        "/v1/feed?after=-1".into(),
        "/v1/whoami".into(),
        "/v1/peers".into(),
        "/v1/blobs".into(),
        "/invite".into(),
    ] {
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            http.get(format!("{}{path}", a.url)).send(),
        )
        .await
        .unwrap_or_else(|_| panic!("{path} hung"))
        .unwrap();
        assert!(r.status().is_success(), "{path}: {}", r.status());
    }
}

#[tokio::test]
async fn the_dashboard_counts_stations_not_their_chains() {
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;
    for x in [&a, &b] {
        x.app
            .log
            .append_telemetry("svc.probe", json!({"results": []}))
            .unwrap();
    }
    a.pull(&b).await.unwrap();
    let page = reqwest::get(&a.url).await.unwrap().text().await.unwrap();
    assert!(
        page.contains("<div class=n>2</div>stations writing"),
        "two stations, each with a main and a telemetry chain: {page}"
    );
}

#[tokio::test]
async fn apps_can_append_through_the_local_api_with_the_token() {
    let a = agent("N0AAA", 1).await;
    let core = |tok: Option<&str>| {
        let dir = a._dir.path().join(format!("tok{}", tok.is_some()));
        if let Some(t) = tok {
            std::fs::write(&dir, t).unwrap();
            ridgewarden_client::CoreClient::new(&a.url, Some(&dir)).unwrap()
        } else {
            ridgewarden_client::CoreClient::new(&a.url, None).unwrap()
        }
    };
    let ok = core(Some(TOKEN))
        .append("svc.probe", json!({"vantage": "x", "results": []}))
        .await
        .unwrap();
    assert_eq!(
        ok.author.key, a.app.log.id.author.key,
        "signed by the core's key"
    );
    assert!(
        core(Some("wrong"))
            .append("svc.probe", json!({}))
            .await
            .is_err()
    );
    assert!(core(None).append("svc.probe", json!({})).await.is_err());
    assert!(
        core(Some(TOKEN))
            .append("node.poll", json!({}))
            .await
            .is_err(),
        "reserved kind"
    );
    let feed = core(None).feed(-1, 100).await.unwrap();
    assert!(feed.events.iter().any(|i| i.event.id == ok.id && i.trusted));
}

#[tokio::test]
async fn a_mule_can_leave_telemetry_behind() {
    let a = agent("N0AAA", 1).await;
    let car = agent("N0CAR", 3).await;
    a.write(3, "msg");
    a.app
        .log
        .append_telemetry("node.poll", json!({"node": "x", "ok": true}))
        .unwrap();
    sync::pull_from(&car.app.log, &car.app.http, &a.url, false)
        .await
        .unwrap();
    let kinds: Vec<String> = car
        .app
        .log
        .store
        .conn()
        .prepare("SELECT kind FROM events")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        kinds.iter().filter(|k| *k == "test.note").count(),
        3,
        "{kinds:?}"
    );
    assert!(!kinds.iter().any(|k| k == "node.poll"), "{kinds:?}");
}

#[tokio::test]
async fn apps_publish_files_from_the_import_folder_only() {
    let a = agent("N0AAA", 1).await;
    let tok = a._dir.path().join("token");
    std::fs::write(&tok, TOKEN).unwrap();
    let core = ridgewarden_client::CoreClient::new(&a.url, Some(&tok)).unwrap();
    let imports = a.app.cfg.imports_dir();
    std::fs::create_dir_all(&imports).unwrap();
    std::fs::write(imports.join("bulk.bin"), vec![7u8; 3000]).unwrap();
    std::fs::write(imports.join("urgent.txt"), b"water at the shelter").unwrap();
    let bulk = core.blob_add("bulk.bin", "lib/bulk.bin", 7).await.unwrap();
    let urgent = core
        .blob_add("urgent.txt", "mule/urgent.txt", 0)
        .await
        .unwrap();
    assert_ne!(bulk, urgent);
    // the key sits in the data dir, one level up from the import folder
    let key = a.app.cfg.key_path();
    let name = key.file_name().unwrap().to_string_lossy().into_owned();
    assert!(core.blob_add(&name, "stolen", 5).await.is_err());
    assert!(core.blob_add("../identity.key", "stolen", 5).await.is_err());
    let err = core.blob_add("missing.bin", "x", 5).await.unwrap_err();
    assert!(format!("{err:#}").contains("import folder"), "{err:#}");
    // an app could plant a link to the key in the shared folders
    #[cfg(unix)]
    {
        // this test agent keeps its key in memory; put a stand-in secret where the key goes
        std::fs::write(&key, "a".repeat(64)).unwrap();
        std::os::unix::fs::symlink(&key, imports.join("k")).unwrap();
        let err = core.blob_add("k", "stolen", 5).await.unwrap_err();
        assert!(format!("{err:#}").contains("not a plain file"), "{err:#}");
        let before = std::fs::read(&key).unwrap();
        let exports = a.app.cfg.data_dir.join("exports");
        std::fs::create_dir_all(&exports).unwrap();
        std::os::unix::fs::symlink(&key, exports.join("out.part")).unwrap();
        core.blob_export(&urgent, "out.bin").await.unwrap();
        assert_eq!(
            std::fs::read(&key).unwrap(),
            before,
            "the key was not overwritten"
        );
        assert_eq!(
            std::fs::read(exports.join("out.bin")).unwrap(),
            b"water at the shelter"
        );
    }

    // another site fetches the urgent one first, whatever order they were wanted in
    let b = agent("N0BBB", 2).await;
    b.pull(&a).await.unwrap();
    let conn = b.app.log.store.conn();
    ridgewarden::blobs::want(&conn, &bulk, true).unwrap();
    ridgewarden::blobs::want(&conn, &urgent, true).unwrap();
    let order: Vec<String> = ridgewarden::blobs::pending(&conn)
        .unwrap()
        .into_iter()
        .map(|m| m.root)
        .collect();
    assert_eq!(order, vec![urgent, bulk]);
}

#[tokio::test]
async fn pruned_telemetry_converges_and_late_joiners_sync_cleanly() {
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;
    let hour = 3_600_000;
    let now = ridgewarden::util::now_ms();
    for i in 0..20 {
        a.app
            .log
            .append_in(
                "node.poll",
                json!({"i": i}),
                Some("telemetry"),
                now - (20 - i) * hour,
            )
            .unwrap();
    }
    b.pull(&a).await.unwrap();
    // keep the last ~5 hours of telemetry
    a.app
        .log
        .checkpoint_telemetry(5 * hour + hour / 2)
        .unwrap()
        .unwrap();
    b.pull(&a).await.unwrap();
    a.pull(&b).await.unwrap();
    assert_eq!(a.ids(), b.ids(), "both pruned the same events");

    // a site joining after the prune gets the main chain and the surviving telemetry
    let c = agent("N0CCC", 3).await;
    c.pull(&b).await.unwrap();
    assert!(a.ids().is_subset(&c.ids()));
    let chain = format!("{}:telemetry", a.app.log.id.author.key);
    assert_eq!(c.app.log.contiguous(&chain).unwrap(), 19);
    assert_eq!(
        c.app.log.digest(&chain).unwrap(),
        a.app.log.digest(&chain).unwrap()
    );
    let again = c.pull(&b).await.unwrap();
    assert_eq!(
        again.accepted + again.duplicate,
        0,
        "nothing left to fetch, nothing refetched"
    );
}

#[tokio::test]
async fn blobs_move_between_sites_and_resume_after_a_cut() {
    use ridgewarden::blobs;
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;
    let src = a._dir.path().join("map.pmtiles");
    let data: Vec<u8> = (0..(blobs::CHUNK * 3 + blobs::CHUNK / 2))
        .map(|i| (i * 7 % 253) as u8)
        .collect();
    std::fs::write(&src, &data).unwrap();
    let root = blobs::add_file(
        &a.app.log,
        &a.app.blobs,
        &src,
        "map.pmtiles",
        blobs::DEFAULT_PRIORITY,
    )
    .unwrap();

    b.pull(&a).await.unwrap();
    assert!(blobs::want(&b.app.log.store.conn(), &root, true).unwrap());
    let peers = vec![a.url.clone()];
    // a budget of 2 chunks: the transfer stops partway, as if the link dropped
    let got = blobs::fetch_round(
        &b.app.log,
        &b.app.blobs,
        &b.app.http,
        &peers,
        2 * blobs::CHUNK,
        &unlimited(),
    )
    .await
    .unwrap();
    assert_eq!(got, 2 * blobs::CHUNK);
    let m = blobs::manifest(&b.app.log.store.conn(), &root)
        .unwrap()
        .unwrap();
    assert_eq!(b.app.blobs.missing(&m).unwrap().len(), 2);
    // the next round picks up where it stopped
    blobs::fetch_round(
        &b.app.log,
        &b.app.blobs,
        &b.app.http,
        &peers,
        1 << 30,
        &unlimited(),
    )
    .await
    .unwrap();
    assert!(b.app.blobs.missing(&m).unwrap().is_empty());
    let out = b._dir.path().join("copy.pmtiles");
    b.app.blobs.export(&m, &out).unwrap();
    assert_eq!(std::fs::read(out).unwrap(), data);
    assert!(blobs::pending(&b.app.log.store.conn()).unwrap().is_empty());
}

#[tokio::test]
async fn invite_page_shows_a_qr_code_to_this_machine() {
    let a = agent("N0AAA", 1).await;
    let page = reqwest::get(format!("{}/invite?url=http://10.1.2.3:8790", a.url))
        .await
        .unwrap();
    assert!(page.status().is_success());
    let body = page.text().await.unwrap();
    assert!(body.contains("<svg") && body.contains("rw1."), "{body}");
    let line = body
        .split("rw1.")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .map(|rest| format!("rw1.{rest}"))
        .unwrap();
    let inv = ridgewarden::invite::read(&line).unwrap();
    assert_eq!(inv.peers, vec!["http://10.1.2.3:8790"]);
    let bad = reqwest::get(format!("{}/invite?url=ftp://x", a.url))
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn apps_can_check_on_a_blob_and_let_it_go() {
    let a = agent("N0AAA", 1).await;
    let tok = a._dir.path().join("token");
    std::fs::write(&tok, TOKEN).unwrap();
    let core = ridgewarden_client::CoreClient::new(&a.url, Some(&tok)).unwrap();
    let imports = a.app.cfg.imports_dir();
    std::fs::create_dir_all(&imports).unwrap();
    std::fs::write(imports.join("v1.txt"), b"first version").unwrap();
    let root = core.blob_add("v1.txt", "lib/doc", 5).await.unwrap();
    let st = core.blob_status(&root).await.unwrap().unwrap();
    assert!(st.complete && st.wanted && st.trusted);
    assert_eq!((st.name.as_str(), st.size), ("lib/doc", 13));
    assert_eq!(st.author, a.app.log.id.author.key);
    assert!(core.blob_status(&"ab".repeat(32)).await.unwrap().is_none());

    let reader = ridgewarden_client::CoreClient::new(&a.url, None).unwrap();
    assert!(reader.blob_unwant(&root).await.is_err(), "needs the token");
    assert_eq!(core.blob_unwant(&root).await.unwrap(), 13);
    let st = core.blob_status(&root).await.unwrap().unwrap();
    assert!(!st.complete && !st.wanted);
    assert_eq!(a.app.blobs.disk_use().unwrap(), (0, 0), "its chunk is gone");
    assert!(core.blob_unwant(&"ab".repeat(32)).await.is_err());
}

#[tokio::test]
async fn disk_use_is_split_by_class() {
    let a = agent("N0AAA", 1).await;
    let now = ridgewarden::util::now_ms();
    let hour = 3_600_000;
    for i in 0..48 {
        a.app
            .log
            .append_in(
                "node.poll",
                json!({"i": i}),
                Some("telemetry"),
                now - i * hour,
            )
            .unwrap();
    }
    a.write(5, "kept");
    let u = ridgewarden::usage::measure(&a.app).unwrap();
    assert_eq!(u.telemetry.events, 48);
    assert_eq!(u.main.events, 6);
    assert!(u.telemetry.bytes > u.main.bytes);
    // 48 polls over 47 hours: about 24 a day, so about 30 days' worth at default retention
    let per_day = u.telemetry.per_day.unwrap();
    let one = u.telemetry.bytes / 48;
    assert!(
        (23 * one..=26 * one).contains(&per_day),
        "{per_day} vs {one}"
    );
    assert_eq!(u.telemetry_steady(), Some(per_day * 30));
    assert!(
        u.main.per_day.is_none(),
        "main log has under a day of history"
    );
    assert!(u.db_bytes > 0);
}

#[tokio::test]
async fn local_actions_refuse_without_the_token_or_off_this_machine() {
    let a = agent("N0AAA", 1).await;
    let http = reqwest::Client::new();
    let unwant = |base: &str| {
        http.post(format!("{base}/local/v1/blob/unwant"))
            .json(&json!({"root": "ab".repeat(32)}))
    };
    assert_eq!(unwant(&a.url).send().await.unwrap().status(), 401);
    let wrong = unwant(&a.url).bearer_auth("wrong").send().await.unwrap();
    assert_eq!(wrong.status(), 401);
    let right = unwant(&a.url).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(right.status(), 404, "past the gate: no such blob");

    let remote = unplaced(&a.app).await;
    let page = reqwest::get(format!("{remote}/invite?url=http://10.1.2.3:8790"))
        .await
        .unwrap();
    assert_eq!(page.status(), 403);
    assert!(
        !page.text().await.unwrap().contains("rw1."),
        "no invite leaks"
    );
    let r = unwant(&remote).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(r.status(), 403);
    for base in [&a.url, &remote] {
        let want = http
            .post(format!("{base}/local/v1/blob/want"))
            .json(&json!({"root": "ab".repeat(32), "peers": ["http://10.1.2.3:8790"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(want.status(), if base == &a.url { 401 } else { 403 });
    }
}

#[tokio::test]
async fn metrics_are_off_unless_enabled_and_need_the_token() {
    let a = agent("N0AAA", 1).await;
    a.write(3, "x");
    let gone = agent("N0CCC", 3).await;
    gone.cut.store(true, Ordering::SeqCst);
    let b = agent_with("N0BBB", 2, |cfg| {
        cfg.metrics.enabled = true;
        cfg.sync.peers = vec![a.url.clone(), gone.url.clone()];
    })
    .await;
    sync::round(&b.app).await;

    let http = reqwest::Client::new();
    let get = |base: &str| http.get(format!("{base}/local/v1/metrics"));
    let off = get(&a.url).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(off.status(), 404, "off by default");
    assert_eq!(get(&b.url).send().await.unwrap().status(), 401);
    let r = get(&b.url).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain; version=0.0.4")
    );
    let body = r.text().await.unwrap();

    // every sample belongs to a family announced with HELP and TYPE, and has a number
    let mut families = BTreeSet::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            assert!(rest.ends_with(" gauge"), "{line}");
            families.insert(rest.trim_end_matches(" gauge").to_string());
            continue;
        }
        if line.starts_with("# HELP ") {
            continue;
        }
        let (series, value) = line.rsplit_once(' ').unwrap();
        let name = series.split('{').next().unwrap();
        assert!(families.contains(name), "{line}");
        value.parse::<f64>().unwrap();
    }
    for want in [
        "ridgewarden_events{log=\"main\"} 5".to_string(),
        "ridgewarden_trusted_stations 1".into(),
        format!("ridgewarden_peer_up{{peer=\"{}\",call=\"\"}} 1", a.url),
        format!("ridgewarden_peer_up{{peer=\"{}\",call=\"\"}} 0", gone.url),
        "ridgewarden_blobs{state=\"complete\"} 0".into(),
    ] {
        assert!(body.lines().any(|l| l == want), "{want} in\n{body}");
    }
    let synced = format!("ridgewarden_peer_last_sync_seconds{{peer=\"{}\"", a.url);
    assert!(body.contains(&synced));
    assert!(!body.contains(&format!(
        "ridgewarden_peer_last_sync_seconds{{peer=\"{}\"",
        gone.url
    )));
    assert!(
        !body.contains("ridgewarden_node_up"),
        "no poll yet, so no figure"
    );
    let remote = unplaced(&b.app).await;
    let away = get(&remote).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(away.status(), 403, "only for a scraper on this machine");
}

fn unlimited() -> ridgewarden::blobs::Pace {
    ridgewarden::blobs::Pace {
        bytes_per_s: 1 << 30,
        per_peer: Default::default(),
    }
}

/// Publish a file of `len` bytes on `a`; returns its root.
fn publish(a: &Agent, len: usize) -> String {
    let src = a._dir.path().join(format!("f{len}.bin"));
    let data: Vec<u8> = (0..len).map(|i| (i * 13 % 251) as u8).collect();
    std::fs::write(&src, data).unwrap();
    ridgewarden::blobs::add_file(&a.app.log, &a.app.blobs, &src, "f.bin", 5).unwrap()
}

impl Agent {
    async fn fetch(&self, peers: &[&Agent], pace: &ridgewarden::blobs::Pace) {
        let urls: Vec<String> = peers.iter().map(|p| p.url.clone()).collect();
        ridgewarden::blobs::fetch_round(
            &self.app.log,
            &self.app.blobs,
            &self.app.http,
            &urls,
            1 << 30,
            pace,
        )
        .await
        .unwrap();
    }
    fn chunk_hits(&self) -> u64 {
        self.chunks.load(Ordering::SeqCst)
    }
    fn complete(&self, root: &str) -> bool {
        let m = ridgewarden::blobs::manifest(&self.app.log.store.conn(), root)
            .unwrap()
            .unwrap();
        self.app.blobs.missing(&m).unwrap().is_empty()
    }
}

#[tokio::test]
async fn preferred_peers_serve_first_and_others_cover_for_them() {
    use ridgewarden::blobs;
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;
    let root = publish(&a, 2 * blobs::CHUNK + 10);
    b.pull(&a).await.unwrap();
    blobs::want(&b.app.log.store.conn(), &root, true).unwrap();
    b.fetch(&[&a], &unlimited()).await;
    assert!(b.complete(&root));

    // d knows only a, but its app asks for b first: every chunk comes from b
    let d = agent("N0DDD", 4).await;
    d.pull(&a).await.unwrap();
    let tok = d._dir.path().join("token");
    std::fs::write(&tok, TOKEN).unwrap();
    let core = ridgewarden_client::CoreClient::new(&d.url, Some(&tok)).unwrap();
    let err = core
        .blob_want_from(&root, &["10.1.2.3".into()])
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("peer address"), "{err:#}");
    core.blob_want_from(&root, &[format!("{}/", b.url)])
        .await
        .unwrap();
    let (a0, b0) = (a.chunk_hits(), b.chunk_hits());
    d.fetch(&[&a], &unlimited()).await;
    assert!(d.complete(&root));
    assert_eq!((a.chunk_hits() - a0, b.chunk_hits() - b0), (0, 3));

    // e prefers b, which is cut off, then c, which doesn't have them: b is asked once and
    // sits out the rest of the round, c is asked for each chunk, and a serves them all
    let second = publish(&a, blobs::CHUNK + 5);
    let c = agent("N0CCC", 3).await;
    let e = agent("N0EEE", 5).await;
    e.pull(&a).await.unwrap();
    b.cut.store(true, Ordering::SeqCst);
    for r in [&root, &second] {
        blobs::want_from(
            &e.app.log.store.conn(),
            r,
            &[b.url.clone(), c.url.clone()],
            false,
        )
        .unwrap();
    }
    let (a0, b0, c0) = (a.chunk_hits(), b.chunk_hits(), c.chunk_hits());
    e.fetch(&[&a], &unlimited()).await;
    assert!(e.complete(&root) && e.complete(&second));
    assert_eq!(
        (
            a.chunk_hits() - a0,
            b.chunk_hits() - b0,
            c.chunk_hits() - c0
        ),
        (4, 1, 4),
        "three chunks and one more: the second blob starts like the first"
    );

    // asked to use only c, f waits rather than going the long way to a
    let f = agent("N0FFF", 6).await;
    f.pull(&a).await.unwrap();
    let tok = f._dir.path().join("token");
    std::fs::write(&tok, TOKEN).unwrap();
    let core = ridgewarden_client::CoreClient::new(&f.url, Some(&tok)).unwrap();
    core.blob_want_only_from(&root, std::slice::from_ref(&c.url))
        .await
        .unwrap();
    let (a0, c0) = (a.chunk_hits(), c.chunk_hits());
    f.fetch(&[&a], &unlimited()).await;
    assert!(!f.complete(&root));
    assert_eq!((a.chunk_hits() - a0, c.chunk_hits() - c0), (0, 3));
    // an empty list lifts the preference, and a serves it
    core.blob_want_from(&root, &[]).await.unwrap();
    f.fetch(&[&a], &unlimited()).await;
    assert!(f.complete(&root));
}

/// How long `b` takes to fetch `root` from `a` at `pace`.
async fn timed_fetch(a: &Agent, b: &Agent, root: &str, pace: &ridgewarden::blobs::Pace) -> f64 {
    b.pull(a).await.unwrap();
    ridgewarden::blobs::want(&b.app.log.store.conn(), root, true).unwrap();
    let started = std::time::Instant::now();
    b.fetch(&[a], pace).await;
    assert!(b.complete(root));
    started.elapsed().as_secs_f64()
}

#[tokio::test]
async fn a_slow_link_gets_its_own_rate() {
    use ridgewarden::blobs;
    let a = agent("N0AAA", 1).await;
    // two chunks, 1.5 MiB: two seconds at 768 KiB/s
    let root = publish(&a, blobs::CHUNK + blobs::CHUNK / 2);
    let fast = timed_fetch(&a, &agent("N0BBB", 2).await, &root, &unlimited()).await;
    let mut capped = unlimited();
    capped.per_peer.insert(a.url.clone(), 786_432);
    let slow = timed_fetch(&a, &agent("N0CCC", 3).await, &root, &capped).await;
    let overall = blobs::Pace {
        bytes_per_s: 786_432,
        per_peer: Default::default(),
    };
    let slow_overall = timed_fetch(&a, &agent("N0DDD", 4).await, &root, &overall).await;
    assert!(fast < 1.0, "{fast}");
    for t in [slow, slow_overall] {
        assert!((1.9..3.5).contains(&t), "{t}");
    }
}
