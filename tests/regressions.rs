//! Regression tests from the second code review: each failed before its fix.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use ridgewarden::app::App;
use ridgewarden::config::Config;
use ridgewarden::event::{Draft, Event};
use ridgewarden::identity::Identity;
use ridgewarden::log::Log;
use ridgewarden::store::Store;
use ridgewarden::util::{ms_to_rfc3339, now_ms};
use ridgewarden::{sync, web};

fn sign(
    id: &Identity,
    seq: u64,
    lamport: u64,
    kind: &str,
    payload: Value,
    log: Option<&str>,
) -> Event {
    Draft {
        author: id.author.clone(),
        seq,
        prev: None,
        lamport,
        wall_time: ms_to_rfc3339(now_ms()),
        mono_time: 0,
        kind: kind.into(),
        payload,
        log: log.map(str::to_string),
    }
    .sign(&id.key)
}

fn mem_log(seed: u8, call: &str) -> Log {
    Log::new(
        Store::open_in_memory().unwrap(),
        Identity::from_seed(call, [seed; 32]),
    )
    .unwrap()
}

struct Agent {
    app: Arc<App>,
    url: String,
    _dir: tempfile::TempDir,
}

async fn agent(call: &str, seed: u8) -> Agent {
    let dir = tempfile::tempdir().unwrap();
    let cfg: Config = toml::from_str(&Config::example(call, dir.path())).unwrap();
    let log = Log::new(
        Store::open(&cfg.db_path()).unwrap(),
        Identity::from_seed(call, [seed; 32]),
    )
    .unwrap();
    let app = Arc::new(App::new(cfg, log).unwrap());
    let router = web::router(app.clone(), None);
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
        _dir: dir,
    }
}

impl Agent {
    async fn pull(&self, from: &Agent) -> ridgewarden::log::IngestReport {
        sync::pull_from(&self.app.log, &self.app.http, &from.url, true)
            .await
            .unwrap()
    }
}

fn two_keys() -> (Identity, Identity) {
    // a spammer key that sorts before the honest one
    let mut c: Vec<Identity> = (50u8..80)
        .map(|s| Identity::from_seed("N0KEY", [s; 32]))
        .collect();
    c.sort_by(|x, y| x.author.key.cmp(&y.author.key));
    let honest = c.pop().unwrap();
    (c.remove(0), honest)
}

fn ingest_returns(order_checkpoint_first: bool) -> bool {
    let s = Identity::from_seed("N0BAD", [9; 32]);
    let l = mem_log(1, "N0AAA");
    let cp = sign(
        &s,
        0,
        1,
        "log.checkpoint",
        json!({"log": "telemetry", "before": i64::MAX}),
        None,
    );
    let ev = sign(
        &s,
        i64::MAX as u64,
        2,
        "node.poll",
        json!({}),
        Some("telemetry"),
    );
    let evs = if order_checkpoint_first {
        vec![cp, ev]
    } else {
        vec![ev, cp]
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(l.ingest(&evs).is_ok());
    });
    rx.recv_timeout(Duration::from_secs(5)) == Ok(true)
}

#[test]
fn checkpoint_and_event_at_the_largest_seq_do_not_hang_ingest() {
    assert!(ingest_returns(true), "checkpoint first");
    assert!(ingest_returns(false), "event first");
}

#[tokio::test]
async fn a_hole_nobody_has_is_not_paid_for_on_every_pull() {
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;
    let s = Identity::from_seed("N0BAD", [9; 32]);
    let evs: Vec<Event> = (0..300u64)
        .filter(|i| *i != 5)
        .map(|i| sign(&s, i, i + 1, "x", json!({"i": i}), None))
        .collect();
    a.app.log.ingest(&evs).unwrap();
    b.pull(&a).await;
    assert_eq!(
        b.pull(&a).await.duplicate,
        0,
        "tail above the hole sent again"
    );
}

#[tokio::test]
async fn a_refused_event_one_agent_saw_does_not_split_digests() {
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;
    let s = Identity::from_seed("N0BAD", [9; 32]);
    let evs: Vec<Event> = (0..40u64)
        .map(|i| {
            let lamport = if i == 5 { (1u64 << 53) + 1 } else { i + 1 };
            sign(&s, i, lamport, "x", json!({"i": i}), None)
        })
        .collect();
    assert_eq!(a.app.log.ingest(&evs).unwrap().rejected, 1);
    for _ in 0..3 {
        b.pull(&a).await;
        a.pull(&b).await;
    }
    let k = &s.author.key;
    assert_eq!(a.app.log.digest(k).unwrap(), b.app.log.digest(k).unwrap());
    assert!(
        b.pull(&a).await.duplicate <= 1,
        "at most one probe for the hole, not the whole bucket"
    );
}

#[test]
fn a_box_behind_a_checkpoint_numbers_its_next_event_past_it() {
    let x = mem_log(5, "N0TWO");
    let y = mem_log(5, "N0TWO");
    let hour = 3_600_000;
    let now = now_ms();
    for i in 0..10 {
        x.append_in(
            "node.poll",
            json!({"x": i}),
            Some("telemetry"),
            now - (10 - i) * hour,
        )
        .unwrap();
    }
    let cp = x
        .checkpoint_telemetry(2 * hour + hour / 2)
        .unwrap()
        .unwrap();
    for i in 0..3 {
        y.append_in(
            "node.poll",
            json!({"y": i}),
            Some("telemetry"),
            now - (10 - i) * hour,
        )
        .unwrap();
    }
    y.ingest(&x.events_after(&x.id.author.key, -1, None, 100).unwrap())
        .unwrap();
    let next = y.append_telemetry("node.poll", json!({})).unwrap();
    assert!(next.seq as i64 >= cp.payload["before"].as_i64().unwrap());
    let z = mem_log(6, "N0ZZZ");
    z.ingest(&x.events_after(&x.id.author.key, -1, None, 100).unwrap())
        .unwrap();
    assert_eq!(z.ingest(&[next]).unwrap().accepted, 1);
}

#[tokio::test]
async fn thousands_of_chains_under_one_key_do_not_starve_an_honest_chain() {
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;
    let (spam, honest) = two_keys();
    let letters = b"abcdefghijklmnopqrstuvwxyz";
    let spam_events: Vec<Event> = (0..2100usize)
        .map(|i| {
            let name: String = [letters[i / 676], letters[(i / 26) % 26], letters[i % 26]]
                .iter()
                .map(|c| *c as char)
                .collect();
            sign(&spam, 0, 1, "x", json!({}), Some(&name))
        })
        .collect();
    a.app.log.ingest(&spam_events).unwrap();
    let real: Vec<Event> = (0..5u64)
        .map(|i| sign(&honest, i, i + 1, "x", json!({"real": i}), None))
        .collect();
    a.app.log.ingest(&real).unwrap();
    for _ in 0..6 {
        b.pull(&a).await;
    }
    assert_eq!(b.app.log.contiguous(&honest.author.key).unwrap(), 4);
}

#[tokio::test]
async fn being_one_event_ahead_of_a_peer_does_not_refetch_its_tip_bucket() {
    let a = agent("N0AAA", 1).await;
    let b = agent("N0BBB", 2).await;
    for i in 0..300 {
        b.app
            .log
            .append_telemetry("node.poll", json!({"i": i}))
            .unwrap();
    }
    a.pull(&b).await;
    b.pull(&a).await;
    b.app
        .log
        .append_telemetry("node.poll", json!({"i": 300}))
        .unwrap();
    assert_eq!(b.pull(&a).await.duplicate, 0);
}
