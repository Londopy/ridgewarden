//! Crashes and power cuts: a writer killed mid-append, and a write-ahead log whose tail was
//! torn off part way through a commit. Either way the log must reopen whole and go on
//! numbering where it left off, or this site would fork its own chain.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use serde_json::json;

use ridgewarden::event::chain_id;
use ridgewarden::identity::Identity;
use ridgewarden::log::{Log, TELEMETRY, xor_into};
use ridgewarden::store::Store;

const DB: &str = "ridgewarden.db";

fn id() -> Identity {
    Identity::from_seed("N0AAA", [1; 32])
}

/// Reopen the log after a crash and check everything a peer could have relied on: a sound
/// database, gap-free chains whose links hold, and heads that match the events.
fn reopen(dir: &Path) -> Log {
    let store = Store::open(&dir.join(DB)).unwrap();
    let sound: String = store
        .conn()
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sound, "ok");
    let log = Log::new(store, id()).unwrap();
    let heads = log.heads().unwrap();
    for chain in [
        log.id.author.key.clone(),
        chain_id(&log.id.author.key, Some(TELEMETRY)),
    ] {
        let evs = log.events_after(&chain, -1, None, 1_000_000).unwrap();
        for (i, e) in evs.iter().enumerate() {
            assert_eq!(e.seq, i as u64, "no holes and nothing twice");
            e.verify().unwrap();
            if i > 0 {
                assert_eq!(e.prev.as_deref(), Some(evs[i - 1].id.as_str()));
            }
        }
        let Some(h) = heads.iter().find(|h| h.key == chain) else {
            assert!(evs.is_empty());
            continue;
        };
        assert_eq!(
            (h.seq, h.max_seq),
            (evs.len() as i64 - 1, evs.len() as i64 - 1)
        );
        let digest = evs.iter().fold(String::new(), |d, e| xor_into(&d, &e.id));
        assert_eq!(
            h.digest, digest,
            "the digest peers compare matches the events"
        );
    }
    assert_eq!(log.fork_count().unwrap(), 0);
    log
}

/// The next append follows the last event held, so no sequence number is used twice.
fn carries_on(log: &Log) -> usize {
    let key = log.id.author.key.clone();
    let held = log.events_after(&key, -1, None, 1_000_000).unwrap();
    let last = held.last().unwrap();
    let next = log.append("test.note", json!({"after": "crash"})).unwrap();
    assert_eq!(next.seq, last.seq + 1);
    assert_eq!(next.prev.as_deref(), Some(last.id.as_str()));
    assert!(next.mono_time > last.mono_time);
    log.event_ids().unwrap().len()
}

fn events_in(dir: &Path) -> usize {
    let Ok(c) = Connection::open_with_flags(dir.join(DB), OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return 0;
    };
    c.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
        .map_or(0, |n| n as usize)
}

#[test]
#[ignore = "the writer killed_mid_append_the_log_reopens_whole starts and kills"]
fn writer_until_killed() {
    let Ok(dir) = std::env::var("RIDGEWARDEN_CRASH_DIR") else {
        return;
    };
    let log = Log::new(Store::open(&Path::new(&dir).join(DB)).unwrap(), id()).unwrap();
    for i in (0..50).cycle() {
        log.append("test.note", json!({"pad": "x".repeat(i * 40)}))
            .unwrap();
        log.append_telemetry("node.poll", json!({"i": i})).unwrap();
    }
}

/// The writer process. It writes until it is killed, so a test that fails part way must
/// still kill it.
struct Writer(Child);

impl Drop for Writer {
    fn drop(&mut self) {
        // usually it is dead already
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn killed_mid_append_the_log_reopens_whole() {
    let dir = tempfile::tempdir().unwrap();
    let mut held = 0;
    for _ in 0..3 {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "writer_until_killed",
                "--exact",
                "--ignored",
                "--test-threads",
                "1",
            ])
            .env("RIDGEWARDEN_CRASH_DIR", dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut writer = Writer(child);
        let start = Instant::now();
        while events_in(dir.path()) < held + 150 {
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "the writer stalled"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // no warning and no clean shutdown: it dies wherever it is in its next append
        writer.0.kill().unwrap();
        writer.0.wait().unwrap();
        let log = reopen(dir.path());
        held = carries_on(&log);
    }
}

#[test]
fn a_power_cut_mid_write_loses_only_the_unfinished_event() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join(format!("{DB}-wal"));
    let log = Log::new(Store::open(&dir.path().join(DB)).unwrap(), id()).unwrap();
    for i in 0..20 {
        log.append("test.note", json!({"i": i})).unwrap();
    }
    let durable = std::fs::metadata(&wal_path).unwrap().len() as usize;
    let unfinished = log.append("test.note", json!({"i": "last"})).unwrap();
    // with synchronous = FULL, append returned only after the log reached the disk; a cut
    // before that leaves part of its commit there, which reopening must throw away
    let main = std::fs::read(dir.path().join(DB)).unwrap();
    let wal = std::fs::read(&wal_path).unwrap();
    assert!(wal.len() > durable);
    for cut in [
        durable + 1,
        (durable + wal.len()) / 2,
        wal.len() - 1,
        wal.len(),
    ] {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(DB), &main).unwrap();
        std::fs::write(d.path().join(format!("{DB}-wal")), &wal[..cut]).unwrap();
        let log = reopen(d.path());
        let ids = log.event_ids().unwrap();
        assert_eq!(
            ids.len(),
            21 + usize::from(cut == wal.len()),
            "cut at {cut}"
        );
        assert_eq!(ids.contains(&unfinished.id), cut == wal.len());
        carries_on(&log);
    }
}
