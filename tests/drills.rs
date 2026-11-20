//! Spec 00 success measure: "20 forced partition drills, zero lost events, every site
//! identical afterwards", with telemetry pruning under the same partitions (spec risk:
//! pruning bugs). Each drill is random but seeded; a failure names its seed.

use std::collections::{BTreeMap, BTreeSet};
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
use ridgewarden::log::{Log, TELEMETRY};
use ridgewarden::store::Store;
use ridgewarden::util::now_ms;
use ridgewarden::{sync, web};

const DRILLS: u64 = 20;
const HOUR: i64 = 3_600_000;

/// splitmix64: small, and the same sequence on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// How a site's server treats requests: refuse all, or fail every `flaky`th one.
struct Link {
    down: AtomicBool,
    flaky: AtomicU64,
    seen: AtomicU64,
}

async fn faults(State(link): State<Arc<Link>>, req: Request, next: Next) -> Response {
    let n = link.seen.fetch_add(1, Ordering::SeqCst) + 1;
    let every = link.flaky.load(Ordering::SeqCst);
    if link.down.load(Ordering::SeqCst) || (every > 0 && n % every == 0) {
        return (StatusCode::SERVICE_UNAVAILABLE, "partitioned").into_response();
    }
    next.run(req).await
}

struct Site {
    app: Arc<App>,
    url: String,
    link: Arc<Link>,
    _dir: tempfile::TempDir,
}

async fn site(call: &str, seed: u8) -> Site {
    let dir = tempfile::tempdir().unwrap();
    let cfg: Config = toml::from_str(&Config::example(call, dir.path())).unwrap();
    // in memory: the drills test sync and pruning, not fsync, and must stay quick
    let log = Log::new(
        Store::open_in_memory().unwrap(),
        Identity::from_seed(call, [seed; 32]),
    )
    .unwrap();
    let app = Arc::new(App::new(cfg, log).unwrap());
    let link = Arc::new(Link {
        down: AtomicBool::new(false),
        flaky: AtomicU64::new(0),
        seen: AtomicU64::new(0),
    });
    let router =
        web::router(app.clone(), None).layer(middleware::from_fn_with_state(link.clone(), faults));
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
    Site {
        app,
        url,
        link,
        _dir: dir,
    }
}

impl Site {
    fn ids(&self) -> BTreeSet<String> {
        self.app.log.event_ids().unwrap().into_iter().collect()
    }
}

/// Everything written during a drill, and the prune points its checkpoints set.
#[derive(Default)]
struct Ledger {
    written: BTreeMap<String, (String, i64)>,
    prune: BTreeMap<String, i64>,
}

impl Ledger {
    fn note(&mut self, e: &ridgewarden::event::Event) {
        self.written.insert(e.id.clone(), (e.chain(), e.seq as i64));
        if e.kind == "log.checkpoint" {
            let chain = format!("{}:{TELEMETRY}", e.author.key);
            let before = e.payload["before"].as_i64().unwrap();
            let p = self.prune.entry(chain).or_default();
            *p = (*p).max(before);
        }
    }

    /// What every site must hold at the end: all of it, less what checkpoints pruned.
    fn expected(&self) -> BTreeSet<String> {
        self.written
            .iter()
            .filter(|(_, (chain, seq))| self.prune.get(chain).is_none_or(|p| seq >= p))
            .map(|(id, _)| id.clone())
            .collect()
    }
}

async fn pull(to: &Site, from: &Site) -> bool {
    sync::pull_from(&to.app.log, &to.app.http, &from.url, true)
        .await
        .is_ok()
}

/// Returns how many events checkpoints pruned and how many fork points the drill made.
async fn drill(seed: u64) -> (usize, i64) {
    let mut rng = Rng(seed);
    let n = rng.range(3, 5);
    // sometimes the last site is a cloned SD card: the first site's key on a second box
    let cloned = rng.chance(30);
    let mut sites = vec![];
    for i in 0..n {
        let k = if cloned && i == n - 1 { 0 } else { i };
        sites.push(site(&format!("N0D{}", (b'A' + k as u8) as char), k as u8 + 1).await);
    }
    let mut ledger = Ledger::default();
    for s in &sites {
        for e in s.app.log.events_at(&s.app.log.id.author.key, 0).unwrap() {
            ledger.note(&e);
        }
    }
    let pruning = rng.chance(60);
    let phases = rng.range(3, 5);
    for phase in 0..phases {
        // a partition: each site lands in one of up to three groups
        let k = rng.range(1, 3);
        let groups: Vec<usize> = (0..n).map(|_| rng.below(k)).collect();
        for s in &sites {
            s.link.flaky.store(
                if rng.chance(30) {
                    rng.range(3, 7) as u64
                } else {
                    0
                },
                Ordering::SeqCst,
            );
        }
        for s in &sites {
            let log = &s.app.log;
            for i in 0..rng.range(0, 12) {
                ledger.note(
                    &log.append("drill.note", json!({"seed": seed, "phase": phase, "i": i}))
                        .unwrap(),
                );
            }
            // telemetry from older phases is older, so checkpoints have something to prune
            let base = now_ms() - (phases - phase) as i64 * HOUR;
            for i in 0..rng.range(0, 20) {
                let wall = base + rng.below(HOUR as usize / 2) as i64;
                ledger.note(
                    &log.append_in(
                        "node.poll",
                        json!({"ok": true, "i": i}),
                        Some(TELEMETRY),
                        wall,
                    )
                    .unwrap(),
                );
            }
            if pruning && rng.chance(40) {
                let keep = rng.range(1, phases) as i64 * HOUR;
                if let Some(cp) = log.checkpoint_telemetry(keep).unwrap() {
                    ledger.note(&cp);
                }
            }
        }
        for _ in 0..rng.range(1, 3) {
            for i in 0..n {
                let mates: Vec<usize> = (0..n)
                    .filter(|&j| j != i && groups[j] == groups[i])
                    .collect();
                if mates.is_empty() {
                    continue;
                }
                let j = mates[rng.below(mates.len())];
                pull(&sites[i], &sites[j]).await;
            }
        }
        // across the cut nothing gets through
        if let Some((i, j)) = (0..n)
            .flat_map(|i| (0..n).map(move |j| (i, j)))
            .find(|&(i, j)| groups[i] != groups[j])
        {
            sites[j].link.down.store(true, Ordering::SeqCst);
            assert!(
                !pull(&sites[i], &sites[j]).await,
                "seed {seed}: pull crossed the cut"
            );
            sites[j].link.down.store(false, Ordering::SeqCst);
        }
    }

    // heal: links clean, everyone pulls from everyone until nothing moves
    for s in &sites {
        s.link.flaky.store(0, Ordering::SeqCst);
    }
    let mut rounds = 0;
    while sites.iter().any(|s| s.ids() != sites[0].ids()) || rounds == 0 {
        rounds += 1;
        assert!(
            rounds <= 4,
            "seed {seed}: no convergence after 4 full rounds"
        );
        for i in 0..n {
            for j in 0..n {
                if i != j {
                    assert!(
                        pull(&sites[i], &sites[j]).await,
                        "seed {seed}: pull failed after heal"
                    );
                }
            }
        }
    }

    let want = ledger.expected();
    let heads = sites[0].app.log.heads().unwrap();
    for (i, s) in sites.iter().enumerate() {
        let have = s.ids();
        let lost: Vec<_> = want.difference(&have).collect();
        assert!(
            lost.is_empty(),
            "seed {seed}: site {i} lost {} event(s)",
            lost.len()
        );
        let extra: Vec<_> = have.difference(&want).collect();
        assert!(
            extra.is_empty(),
            "seed {seed}: site {i} kept {} pruned event(s)",
            extra.len()
        );
        assert_eq!(
            s.app.log.heads().unwrap(),
            heads,
            "seed {seed}: site {i} heads differ"
        );
    }
    let forks = sites[0].app.log.fork_count().unwrap();
    if cloned {
        assert!(forks > 0, "seed {seed}: the clone forked");
    }
    (ledger.written.len() - want.len(), forks)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_random_partition_drills_lose_nothing() {
    let mut set = tokio::task::JoinSet::new();
    for seed in 0..DRILLS {
        set.spawn(drill(0x5eed_0000 + seed));
    }
    let all = async {
        let (mut pruned, mut forks) = (0, 0);
        while let Some(r) = set.join_next().await {
            let (p, f) = r.unwrap();
            pruned += p;
            forks += f;
        }
        (pruned, forks)
    };
    let (pruned, forks) = tokio::time::timeout(std::time::Duration::from_secs(120), all)
        .await
        .expect("drills finish within two minutes");
    // the drills only mean something if they prune and fork somewhere
    assert!(pruned > 0, "no drill pruned anything");
    assert!(forks > 0, "no drill had a cloned key");
}
