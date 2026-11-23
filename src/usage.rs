//! Disk use per class (main log, telemetry, blobs) and where it is heading, so a site can
//! check the spec's "under 1 GB per site at default retention".

use std::path::Path;

use anyhow::Result;
use rusqlite::params;

use crate::app::App;
use crate::util::now_ms;

const DAY_MS: i64 = 86_400_000;
/// Growth rates are taken from events written in this window.
const RATE_WINDOW_MS: i64 = 7 * DAY_MS;
const NO_RATE: &str = "not enough history for a rate yet";

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Class {
    pub events: i64,
    /// size of the events' JSON, which is what syncs; SQLite adds indexes on top
    pub bytes: i64,
    /// bytes per day written lately, by the authors' clocks; None with under a day of history
    pub per_day: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub main: Class,
    pub telemetry: Class,
    pub blob_chunks: u64,
    pub blob_bytes: u64,
    /// the database file with its write-ahead log
    pub db_bytes: u64,
    /// exported blobs and files apps put up for publishing
    pub files_bytes: u64,
    pub telemetry_days: u64,
}

impl Usage {
    /// Telemetry once retention has levelled it off, if every station keeps what this
    /// site keeps.
    pub fn telemetry_steady(&self) -> Option<i64> {
        self.telemetry
            .per_day
            .map(|d| d * self.telemetry_days as i64)
    }

    /// What the main log adds in a year at the current rate. It is never pruned.
    pub fn main_per_year(&self) -> Option<i64> {
        self.main.per_day.map(|d| d * 365)
    }

    pub fn on_disk(&self) -> u64 {
        self.db_bytes + self.blob_bytes + self.files_bytes
    }

    /// "grows about 12.0 MB a year", or why there is no figure yet.
    pub fn main_outlook(&self) -> String {
        self.main_per_year().map_or(NO_RATE.into(), |n| {
            format!("grows about {} a year", human_bytes(n.max(0) as u64))
        })
    }

    /// "levels off near 40.0 MB", or why there is no figure yet.
    pub fn telemetry_outlook(&self) -> String {
        self.telemetry_steady().map_or(NO_RATE.into(), |n| {
            format!("levels off near {}", human_bytes(n.max(0) as u64))
        })
    }
}

fn class(app: &App, telemetry: bool, now: i64) -> Result<Class> {
    let c = app.log.store.conn();
    // sub-log chains are "key:log"; main chains are the bare key
    let filter = if telemetry {
        "instr(chain, ':') > 0"
    } else {
        "instr(chain, ':') = 0"
    };
    let (events, bytes): (i64, i64) = c.query_row(
        &format!(
            "SELECT count(*), coalesce(sum(octet_length(json)), 0) FROM events WHERE {filter}"
        ),
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (recent, oldest): (i64, Option<i64>) = c.query_row(
        &format!(
            "SELECT coalesce(sum(octet_length(json)), 0), min(wall_ms) FROM events
             WHERE {filter} AND wall_ms >= ?1 AND wall_ms <= ?2"
        ),
        params![now - RATE_WINDOW_MS, now],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let span = oldest.map_or(0, |o| now - o);
    let per_day = (span >= DAY_MS).then(|| recent * DAY_MS / span);
    Ok(Class {
        events,
        bytes,
        per_day,
    })
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map_or(0, |m| m.len())
}

/// Bytes in the files under `dir`; a folder that isn't there holds nothing.
fn tree_len(dir: &Path) -> Result<u64> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut n = 0;
    for e in entries.flatten() {
        let Ok(t) = e.file_type() else { continue };
        if t.is_dir() {
            n += tree_len(&e.path())?;
        } else if t.is_file() {
            n += e.metadata().map_or(0, |m| m.len());
        }
    }
    Ok(n)
}

pub fn measure(app: &App) -> Result<Usage> {
    let now = now_ms();
    let db = app.cfg.db_path();
    let wal = db.with_extension("db-wal");
    let (blob_chunks, blob_bytes) = app.blobs.disk_use()?;
    let mut files_bytes = 0;
    for d in ["exports", "imports"] {
        files_bytes += tree_len(&app.cfg.data_dir.join(d))?;
    }
    Ok(Usage {
        main: class(app, false, now)?,
        telemetry: class(app, true, now)?,
        blob_chunks,
        blob_bytes,
        db_bytes: file_len(&db) + file_len(&wal),
        files_bytes,
        telemetry_days: app.cfg.retention.telemetry_days,
    })
}

/// "512 B", "3.4 KB", "12.0 MB", "1.20 GB".
pub fn human_bytes(n: u64) -> String {
    match n {
        0..1_000 => format!("{n} B"),
        1_000..1_000_000 => format!("{:.1} KB", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", n as f64 / 1e6),
        _ => format!("{:.2} GB", n as f64 / 1e9),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_read_plainly() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(3_400), "3.4 KB");
        assert_eq!(human_bytes(12_000_000), "12.0 MB");
        assert_eq!(human_bytes(1_200_000_000), "1.20 GB");
    }
}
