//! `ridgewarden top`: a live view of this agent in a terminal, for operators on a headless
//! Pi over SSH. It reads what `status` reads, the same way: the agent's database directly,
//! and the peers from the running agent's local API.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::Result;
use rusqlite::params;
use serde::Deserialize;
use serde_json::Value;

use ridgewarden::app::{App, short_err};
use ridgewarden::config::{Config, SyncCfg};
use ridgewarden::event::{Event, chain_id};
use ridgewarden::log::{Head, TELEMETRY, author_of};
use ridgewarden::usage::{self, Usage, human_bytes};
use ridgewarden::util::{human_duration, ms_to_rfc3339, now_ms, short_time};
use ridgewarden::{agent, blobs, node};
use ridgewarden_client::tui::ratatui::Frame;
use ridgewarden_client::tui::ratatui::crossterm::event::{KeyCode, KeyEvent};
use ridgewarden_client::tui::ratatui::layout::{Constraint, Layout, Rect};
use ridgewarden_client::tui::ratatui::style::Style;
use ridgewarden_client::tui::ratatui::text::{Line, Span};
use ridgewarden_client::tui::ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use ridgewarden_client::tui::{
    self, Chrome, Fetch, Notice, Pill, Refresh, Theme, View, move_key, pill_table, tab_key,
};

const TABS: [&str; 6] = ["Overview", "Peers", "Log", "Node", "Blobs", "Events"];
const HELP: &str = "q quit  1-6 Tab pages  Up Down PgUp PgDn move  r reload";
pub const START: &str = "start it with: sudo systemctl start ridgewarden";
/// Counting forks and disk use reads every event, so those are read this often, not every
/// round.
const SLOW_MS: i64 = 30_000;
/// Events kept for the Events page.
const RECENT: i64 = 200;
/// Our own polls read back each round: enough for the last failures and a round of crawling,
/// without parsing a day of polls every couple of seconds.
const POLLS: i64 = 300;
/// Longest line kept about one event; payloads come from other stations and can be big.
const LINE: usize = 200;

/// Where this agent's API answers on this machine.
pub fn local_base(cfg: &Config) -> String {
    let mut local = cfg.listen;
    if local.ip().is_unspecified() {
        local.set_ip(Ipv4Addr::LOCALHOST.into());
    }
    format!("http://{local}")
}

/// A peer as the running agent reports it.
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct PeerState {
    pub url: String,
    pub call: Option<String>,
    pub last_ok: Option<i64>,
    pub last_err: Option<String>,
}

impl PeerState {
    pub fn who(&self) -> &str {
        self.call.as_deref().unwrap_or("?")
    }

    /// How the last pull from it went.
    pub fn state(&self, now: i64) -> String {
        match (&self.last_err, self.last_ok) {
            (Some(e), _) => format!("failing: {e}"),
            (None, Some(t)) => format!("synced {} ago", human_duration(now - t)),
            (None, None) => "not synced yet".into(),
        }
    }
}

#[derive(Deserialize)]
struct Peers {
    #[serde(default)]
    peers: Vec<PeerState>,
}

/// The running agent's peers, which only it knows about; an error means it isn't answering.
pub async fn running_peers(fetch: &Fetch, base: &str) -> Result<Vec<PeerState>> {
    Ok(fetch
        .json::<Peers>(&format!("{base}/v1/peers"))
        .await?
        .peers)
}

/// Run `top` until the user quits, reading everything again every `every`.
pub fn run(cfg: Config, every: Duration) -> Result<()> {
    refuse_root(&cfg)?;
    let mut loader = Loader {
        cfg: cfg.clone(),
        app: None,
        fetch: Fetch::new(Duration::from_secs(3))?,
        slow: None,
    };
    let mut top = Top {
        refresh: Refresh::spawn(every, async move || loader.load().await)?,
        cfg,
        theme: Theme::from_env(),
        snap: None,
        err: None,
        tab: 0,
        rows: Default::default(),
        page: 0,
    };
    tui::run(&mut top, Duration::from_millis(250))?;
    Ok(())
}

/// Opening the agent's database as root can leave files in its data dir that the agent's
/// own user can't write, which locks the agent out. Root is fine where the agent runs as
/// root too, as in the Docker image.
#[cfg(unix)]
fn refuse_root(cfg: &Config) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // /proc/self belongs to the user this process runs as
    let me = std::fs::metadata("/proc/self").map(|m| m.uid());
    let owner = std::fs::metadata(&cfg.data_dir).map(|m| m.uid());
    if let (Ok(0), Ok(owner)) = (me, owner)
        && owner != 0
    {
        anyhow::bail!(
            "top doesn't run as root here: files root opens in {} could lock the agent out. Run it as the agent's user: sudo -u ridgewarden ridgewarden top",
            cfg.data_dir.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_root(_: &Config) -> Result<()> {
    Ok(())
}

/// Everything top shows, read in one go.
struct Snap {
    /// when it was read
    at: i64,
    call: String,
    node: String,
    base: String,
    /// the running agent's peers, or why it didn't answer
    agent: Result<Vec<PeerState>, String>,
    last_poll: Option<(i64, bool)>,
    day: (i64, i64),
    lan: Option<Lan>,
    /// our own polls, newest first
    polls: Vec<Poll>,
    nodes: Vec<NodeRow>,
    stations: Vec<Station>,
    refused: i64,
    /// the latest few refusals: whose, which one, and why
    last_refused: Vec<String>,
    blobs: Vec<BlobRow>,
    events: Vec<Recent>,
    slow: Slow,
}

/// What it costs a read of every event to know: fork points and disk use.
#[derive(Clone)]
struct Slow {
    at: i64,
    forks: i64,
    usage: Usage,
}

struct Loader {
    cfg: Config,
    /// opened by the first read that works
    app: Option<App>,
    fetch: Fetch,
    slow: Option<Slow>,
}

impl Loader {
    async fn load(&mut self) -> Result<Snap, String> {
        let app = match &mut self.app {
            Some(app) => app,
            none => none.insert(agent::open(self.cfg.clone()).map_err(|e| {
                format!(
                    "Can't open this agent's data in {}: {e:#}. Run top as the agent's user: sudo -u ridgewarden ridgewarden top",
                    self.cfg.data_dir.display()
                )
            })?),
        };
        let base = local_base(&app.cfg);
        let agent = running_peers(&self.fetch, &base)
            .await
            .map_err(|e| short_err(&e));
        let now = now_ms();
        let slow = match self.slow.take() {
            Some(s) if now - s.at < SLOW_MS => s,
            _ => Slow {
                at: now,
                forks: app.log.fork_count().map_err(fail)?,
                usage: usage::measure(app).map_err(fail)?,
            },
        };
        self.slow = Some(slow.clone());
        read(app, base, agent, slow).map_err(fail)
    }
}

fn fail(e: anyhow::Error) -> String {
    format!("{e:#}")
}

fn read(
    app: &App,
    base: String,
    agent: Result<Vec<PeerState>, String>,
    slow: Slow,
) -> Result<Snap> {
    let now = now_ms();
    // trust lists arrive with syncs; the running agent reads them again every round
    app.log.refresh_trust()?;
    let polls = polls(app)?;
    let heads = app.log.heads()?;
    let (refused, last_refused) = refused(app)?;
    let (name, last_poll, lan) = {
        let c = app.log.store.conn();
        let lan = node::seen_by(&c, now)?.map(|(_, obs)| Lan {
            model: obs.model,
            firmware: obs.firmware,
            links: obs.links.len(),
        });
        (node::last_name(&c)?, node::last_poll(&c)?, lan)
    };
    Ok(Snap {
        at: now,
        call: app.log.id.author.call.clone(),
        node: name.unwrap_or_else(|| app.node_name()),
        base,
        agent,
        last_poll,
        day: day_polls(app, now)?,
        lan,
        nodes: nodes(&polls),
        polls,
        stations: stations(
            &heads,
            &app.log.trusted(),
            &revoked(app)?,
            &app.log.id.author.key,
        ),
        refused,
        last_refused,
        blobs: blob_rows(app)?,
        events: recent(app)?,
        slow,
    })
}

/// Polls of our node in the last day: (answered, tried).
fn day_polls(app: &App, now: i64) -> Result<(i64, i64)> {
    Ok(app.log.store.conn().query_row(
        "SELECT coalesce(sum(ok), 0), count(*) FROM node_polls WHERE ts > ?1",
        params![now - 86_400_000],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// What our node said about itself when it last answered.
struct Lan {
    model: Option<String>,
    firmware: Option<String>,
    links: usize,
}

/// What `status` says about the last poll of the node.
pub fn poll_state(last: Option<(i64, bool)>, now: i64) -> String {
    match last {
        Some((t, true)) => format!("answered {} ago", human_duration(now - t)),
        Some((t, false)) => format!("did not answer at {}", short_time(t)),
        None => "not polled yet".into(),
    }
}

/// A poll this agent made, of its own node or of a crawled one.
struct Poll {
    at: i64,
    node: String,
    ok: bool,
    crawl: bool,
    error: String,
    links: usize,
}

impl Poll {
    fn of(at: i64, p: &Value) -> Self {
        let s = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Self {
            at,
            node: s("node"),
            ok: p.get("ok").and_then(Value::as_bool).unwrap_or(false),
            crawl: p.get("via").and_then(Value::as_str) == Some("crawl"),
            error: s("error"),
            links: p.get("links").and_then(Value::as_array).map_or(0, Vec::len),
        }
    }
}

/// Our own polls, newest first.
fn polls(app: &App) -> Result<Vec<Poll>> {
    let chain = chain_id(&app.log.id.author.key, Some(TELEMETRY));
    let c = app.log.store.conn();
    let mut st = c.prepare(
        "SELECT wall_ms, json FROM events WHERE chain = ?1 AND kind = 'node.poll'
         ORDER BY seq DESC LIMIT ?2",
    )?;
    let rows = st.query_map(params![chain, POLLS], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = vec![];
    for r in rows {
        let (at, json) = r?;
        let e: Event = serde_json::from_str(&json)?;
        out.push(Poll::of(at, &e.payload));
    }
    Ok(out)
}

/// The newest poll of one node.
struct NodeRow {
    node: String,
    crawl: bool,
    at: i64,
    ok: bool,
    /// when it last answered, among the polls read
    last_ok: Option<i64>,
    links: usize,
    error: String,
}

/// One row per node from polls newest first: ours first, then the crawled ones by name.
fn nodes(polls: &[Poll]) -> Vec<NodeRow> {
    let mut out: Vec<NodeRow> = vec![];
    for p in polls {
        match out
            .iter_mut()
            .find(|r| r.node == p.node && r.crawl == p.crawl)
        {
            Some(r) if p.ok && r.last_ok.is_none() => r.last_ok = Some(p.at),
            Some(_) => {}
            None => out.push(NodeRow {
                node: p.node.clone(),
                crawl: p.crawl,
                at: p.at,
                ok: p.ok,
                last_ok: p.ok.then_some(p.at),
                links: p.links,
                error: p.error.clone(),
            }),
        }
    }
    out.sort_by(|a, b| (a.crawl, &a.node).cmp(&(b.crawl, &b.node)));
    out
}

/// What this agent holds of one station's chains, and anything wrong with them.
struct Station {
    call: String,
    key: String,
    trusted: bool,
    revoked: bool,
    /// main log events held, and the highest number among them: the chain's tail
    events: i64,
    latest: i64,
    telemetry: i64,
    /// numbers below the tail that haven't arrived, in any of its chains
    missing: i64,
    forks: usize,
}

impl Station {
    fn pill(&self) -> Pill {
        if self.revoked {
            Pill::Down
        } else if self.missing > 0 || self.forks > 0 {
            Pill::Partial
        } else {
            Pill::Ok
        }
    }
}

/// One row per station from the chain heads, this station (`me`) first.
fn stations(
    heads: &[Head],
    trusted: &BTreeSet<String>,
    revoked: &BTreeSet<String>,
    me: &str,
) -> Vec<Station> {
    let mut by: BTreeMap<&str, Station> = BTreeMap::new();
    for h in heads {
        let key = author_of(&h.key);
        let s = by.entry(key).or_insert_with(|| Station {
            call: h.call.clone(),
            key: key.to_string(),
            trusted: trusted.contains(key),
            revoked: revoked.contains(key),
            events: 0,
            latest: -1,
            telemetry: 0,
            missing: 0,
            forks: 0,
        });
        if h.key == key {
            s.events = h.count;
            s.latest = h.max_seq;
        } else {
            s.telemetry += h.count;
        }
        s.missing += (h.max_seq - h.seq).max(0);
        s.forks += h.forks.len();
    }
    let mut out: Vec<Station> = by.into_values().collect();
    out.sort_by(|a, b| (a.key != me, &a.call).cmp(&(b.key != me, &b.call)));
    out
}

fn revoked(app: &App) -> Result<BTreeSet<String>> {
    let c = app.log.store.conn();
    let mut st = c.prepare("SELECT author_key FROM revoked")?;
    let keys = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(keys)
}

/// Correctly signed events this agent refused to keep (too big, an impossible clock): how
/// many, and the latest few.
fn refused(app: &App) -> Result<(i64, Vec<String>)> {
    let c = app.log.store.conn();
    let n = c.query_row("SELECT count(*) FROM rejected", [], |r| r.get(0))?;
    let mut st = c.prepare(
        "SELECT coalesce(h.call, '?'), r.seq, r.reason FROM rejected r
         LEFT JOIN heads h ON h.chain = r.chain ORDER BY r.rowid DESC LIMIT 3",
    )?;
    let last = st
        .query_map([], |r| {
            Ok(format!(
                "{} #{}: {}",
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?
            ))
        })?
        .collect::<Result<_, _>>()?;
    Ok((n, last))
}

struct BlobRow {
    name: String,
    size: u64,
    priority: u8,
    state: BlobState,
    /// peers to ask first, and whether only those
    sources: usize,
    only: bool,
}

#[derive(Debug, PartialEq)]
enum BlobState {
    Here,
    /// chunks held of those needed; `index` while its chunk list is still coming
    Fetching {
        have: usize,
        total: usize,
        index: bool,
    },
    /// why it can't go on, e.g. a damaged chunk list
    Stuck(String),
    /// known, not wanted
    Available,
}

impl BlobState {
    fn pill(&self) -> Option<Pill> {
        match self {
            BlobState::Here => Some(Pill::Ok),
            BlobState::Fetching { .. } => Some(Pill::Partial),
            BlobState::Stuck(_) => Some(Pill::Down),
            BlobState::Available => None,
        }
    }

    fn text(&self) -> String {
        match self {
            BlobState::Here => "complete".into(),
            BlobState::Fetching {
                have,
                total,
                index: false,
            } => format!("{}% ({have} of {total} chunks)", percent(*have, *total)),
            BlobState::Fetching { have, total, .. } => format!("chunk list {have} of {total}"),
            BlobState::Stuck(why) => why.clone(),
            BlobState::Available => "not wanted".into(),
        }
    }
}

/// Whole percent, rounded down so it says 100 only when it's done.
fn percent(have: usize, total: usize) -> usize {
    (have * 100).checked_div(total).unwrap_or(100)
}

/// Blobs this agent knows of: the ones it's fetching first, then the ones it has, then the
/// rest.
fn blob_rows(app: &App) -> Result<Vec<BlobRow>> {
    let rows: Vec<(String, bool, BlobRow)> = {
        let c = app.log.store.conn();
        let mut st = c.prepare(
            "SELECT root, wanted, complete, name, size, priority, sources, only_sources FROM blobs
             ORDER BY (wanted AND NOT complete) DESC, complete DESC, priority, added_ms DESC
             LIMIT 500",
        )?;
        st.query_map([], |r| {
            let complete: bool = r.get(2)?;
            let sources: String = r.get(6)?;
            let row = BlobRow {
                name: r.get(3)?,
                size: r.get::<_, i64>(4)?.max(0) as u64,
                priority: r.get(5)?,
                state: if complete {
                    BlobState::Here
                } else {
                    BlobState::Available
                },
                sources: serde_json::from_str::<Vec<String>>(&sources).map_or(0, |v| v.len()),
                only: r.get(7)?,
            };
            Ok((r.get(0)?, r.get::<_, bool>(1)? && !complete, row))
        })?
        .collect::<Result<_, _>>()?
    };
    rows.into_iter()
        .map(|(root, fetching, mut row)| {
            if fetching {
                row.state = progress(app, &root)?;
            }
            Ok(row)
        })
        .collect()
}

/// How far a wanted blob has got. A big blob's chunk list is in chunks of its own; until
/// those have arrived, they are what's counted.
fn progress(app: &App, root: &str) -> Result<BlobState> {
    let Some(m) = blobs::manifest(&app.log.store.conn(), root)? else {
        return Ok(BlobState::Available);
    };
    Ok(match app.blobs.chunk_list(&m) {
        Ok(Some(list)) => {
            let need: HashSet<&String> = list.iter().collect();
            BlobState::Fetching {
                have: need.iter().filter(|h| app.blobs.has(h)).count(),
                total: need.len(),
                index: false,
            }
        }
        Ok(None) => {
            let index = m.index.unwrap_or_default();
            BlobState::Fetching {
                have: index.iter().filter(|h| app.blobs.has(h)).count(),
                total: index.len(),
                index: true,
            }
        }
        Err(e) => BlobState::Stuck(format!("{e:#}")),
    })
}

/// An event as the Events page lists it.
struct Recent {
    seen: i64,
    call: String,
    kind: String,
    trusted: bool,
    what: String,
    detail: String,
}

/// The events this agent got last, newest first.
fn recent(app: &App) -> Result<Vec<Recent>> {
    let last: i64 =
        app.log
            .store
            .conn()
            .query_row("SELECT coalesce(max(n), 0) FROM events", [], |r| r.get(0))?;
    let items = app.log.feed(last - RECENT, RECENT as usize)?;
    Ok(items
        .iter()
        .rev()
        .map(|i| Recent {
            seen: i.seen_ms.unwrap_or_default(),
            call: i.event.author.call.clone(),
            kind: i.event.kind.clone(),
            trusted: i.trusted,
            what: what(&i.event),
            detail: detail(&i.event),
        })
        .collect())
}

/// One line about an event, from its payload. Every field is optional here: payloads come
/// from other stations.
fn what(e: &Event) -> String {
    let p = &e.payload;
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("?");
    let n = |k: &str| {
        p.get(k)
            .and_then(Value::as_u64)
            .map_or("?".into(), |v| v.to_string())
    };
    let len = |k: &str| p.get(k).and_then(Value::as_array).map_or(0, Vec::len);
    let line = match e.kind.as_str() {
        "node.poll" if p.get("ok").and_then(Value::as_bool) == Some(true) => {
            format!(
                "{} answered ({}), {} link(s)",
                s("node"),
                s("via"),
                len("links")
            )
        }
        "node.poll" => format!(
            "{} did not answer ({}): {}",
            s("node"),
            s("via"),
            s("error")
        ),
        "mesh.list" => format!("a list of {} item(s)", len("list")),
        "trust.list" => format!(
            "trust list v{}, {} station(s)",
            n("version"),
            len("stations")
        ),
        "blob.manifest" => format!(
            "shares {} ({})",
            s("name"),
            p.get("size")
                .and_then(Value::as_u64)
                .map_or("?".into(), human_bytes)
        ),
        "log.checkpoint" => format!("{} before #{} may be pruned", s("log"), n("before")),
        "identity.bind" => format!("{} signs as {}", s("callsign"), s("key")),
        "identity.revoke" => format!("revoked its key: {}", s("reason")),
        _ => p.to_string(),
    };
    line.chars().take(LINE).collect()
}

/// The rest of an event, for the Events page.
fn detail(e: &Event) -> String {
    let text = format!(
        "{} #{} in {}'s {} log, written {}, lamport {}\nid {}\nkey {}\n{}",
        e.kind,
        e.seq,
        e.author.call,
        e.log.as_deref().unwrap_or("main"),
        e.wall_time,
        e.lamport,
        e.id,
        e.author.key,
        e.payload
    );
    text.chars().take(5 * LINE).collect()
}

/// "11-20 14:02:33", in UTC.
fn clock(ms: i64) -> String {
    let t = ms_to_rfc3339(ms);
    format!("{} {}", &t[5..10], &t[11..19])
}

/// About how long one peer waits between pulls: each round pulls from `fanout` peers picked
/// at random.
fn pull_gap_ms(sync: &SyncCfg, peers: usize) -> i64 {
    sync.interval_s as i64 * 1000 * peers.div_ceil(sync.fanout.max(1)).max(1) as i64
}

fn peer_pill(p: &PeerState, now: i64, gap_ms: i64) -> Option<Pill> {
    Pill::freshness(p.last_err.is_some(), p.last_ok, now, gap_ms)
}

fn poll_pill(last: Option<(i64, bool)>, now: i64, every_ms: i64) -> Option<Pill> {
    Pill::freshness(
        matches!(last, Some((_, false))),
        last.map(|l| l.0),
        now,
        every_ms,
    )
}

/// One pill for a group: theirs when they all agree, partial when they don't, none when
/// nothing in it has a state yet.
fn overall(pills: &[Option<Pill>]) -> Option<Pill> {
    let mut known = pills.iter().flatten();
    let first = *known.next()?;
    Some(if known.all(|p| *p == first) {
        first
    } else {
        Pill::Partial
    })
}

/// "2 synced lately, 1 failing": the counts that aren't zero.
fn counts(parts: &[(usize, &str)]) -> String {
    let parts: Vec<String> = parts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    parts.join(", ")
}

/// What needs a look in the log, as "2 forks, 5 missing"; empty when nothing does.
fn problems(s: &Snap) -> String {
    let missing: i64 = s.stations.iter().map(|x| x.missing).sum();
    counts(&[
        (s.slow.forks.max(0) as usize, "forks"),
        (missing.max(0) as usize, "missing"),
        (s.refused.max(0) as usize, "refused"),
    ])
}

struct Top {
    cfg: Config,
    theme: Theme,
    refresh: Refresh<Result<Snap, String>>,
    snap: Option<Snap>,
    /// why the latest read failed, if it did; the last good one stays on screen
    err: Option<String>,
    tab: usize,
    /// each page's selected row
    rows: [TableState; TABS.len()],
    /// rows a page had room for at the last draw, for PgUp and PgDn
    page: usize,
}

impl Top {
    fn take(&mut self, read: Result<Snap, String>) {
        match read {
            Ok(s) => {
                self.snap = Some(s);
                self.err = None;
            }
            Err(e) => self.err = Some(e),
        }
    }

    /// Rows in a page's table.
    fn len(&self, tab: usize) -> usize {
        let Some(s) = &self.snap else { return 0 };
        match tab {
            1 => s.agent.as_ref().map_or(0, Vec::len),
            2 => s.stations.len(),
            3 => s.nodes.len(),
            4 => s.blobs.len(),
            5 => s.events.len(),
            _ => 0,
        }
    }

    fn status(&self, now: i64) -> Span<'static> {
        let Some(s) = &self.snap else {
            return Span::styled("loading", self.theme.mute());
        };
        let age = format!("updated {} ago", human_duration(now - s.at));
        match &s.agent {
            Ok(_) => Span::styled(age, self.theme.mute()),
            Err(_) => Span::styled(format!("agent not answering, {age}"), self.theme.bad()),
        }
    }
}

impl View for Top {
    fn tick(&mut self) -> io::Result<()> {
        if let Some(read) = self.refresh.latest()? {
            self.take(read);
        }
        Ok(())
    }

    fn key(&mut self, k: KeyEvent) -> bool {
        if let Some(t) = tab_key(&k, self.tab, TABS.len()) {
            self.tab = t;
        } else if k.code == KeyCode::Char('r') {
            self.refresh.again();
        } else {
            let len = self.len(self.tab);
            move_key(&mut self.rows[self.tab], &k, len, self.page);
        }
        true
    }

    fn draw(&mut self, f: &mut Frame) {
        let now = now_ms();
        let title = match &self.snap {
            Some(s) => format!("ridgewarden top  {} on {}", s.call, s.node),
            None => "ridgewarden top".into(),
        };
        let chrome = Chrome {
            title: &title,
            status: self.status(now),
            tabs: &TABS,
            tab: self.tab,
            help: HELP,
        };
        let Some(mut area) = chrome.draw(f, &self.theme) else {
            return;
        };
        let theme = &self.theme;
        if let (Some(e), Some(_)) = (&self.err, &self.snap) {
            let [banner, rest] =
                Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
            let msg = format!("Reading failed, so this is from earlier: {e}");
            f.render_widget(Line::styled(msg, theme.bad()), banner);
            area = rest;
        }
        // the header and the lines above most tables
        self.page = area.height.saturating_sub(4).into();
        let Some(s) = &self.snap else {
            let n = match &self.err {
                Some(e) => Notice::Error(e),
                None => Notice::Loading,
            };
            f.render_widget(n.paragraph(theme), area);
            return;
        };
        let c = Ctx {
            theme,
            cfg: &self.cfg,
            s,
            now,
        };
        let rows = &mut self.rows[self.tab];
        match self.tab {
            0 => overview(&c, f, area),
            1 => peers(&c, f, area, rows),
            2 => log(&c, f, area, rows),
            3 => node(&c, f, area, rows),
            4 => files(&c, f, area, rows),
            _ => events(&c, f, area, rows),
        }
    }
}

/// What every page draws from.
struct Ctx<'a> {
    theme: &'a Theme,
    cfg: &'a Config,
    s: &'a Snap,
    now: i64,
}

type Item = (Option<Pill>, Vec<Cell<'static>>);

fn item(p: Option<Pill>, name: &'static str, text: String) -> Item {
    (p, vec![Cell::from(name), Cell::from(text)])
}

fn overview(c: &Ctx, f: &mut Frame, area: Rect) {
    let rows = vec![
        agent_row(c),
        node_row(c),
        peers_row(c),
        log_row(c),
        blobs_row(c),
        disk_row(c),
    ];
    let [top, _, head, list] = Layout::vertical([
        Constraint::Length(rows.len() as u16),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    let widths = [Constraint::Length(6), Constraint::Fill(1)];
    f.render_widget(pill_table(c.theme, &[], &widths, rows), top);
    f.render_widget(Line::styled("Recent events", c.theme.heading()), head);
    f.render_widget(events_table(c), list);
}

fn agent_row(c: &Ctx) -> Item {
    match &c.s.agent {
        Ok(_) => item(
            Some(Pill::Ok),
            "Agent",
            format!("running, dashboard on {}/", c.s.base),
        ),
        Err(e) => item(
            Some(Pill::Down),
            "Agent",
            format!("not answering on {} ({e})", c.s.base),
        ),
    }
}

fn node_row(c: &Ctx) -> Item {
    let s = c.s;
    let every = c.cfg.node.poll_interval_s as i64 * 1000;
    let (ok, tried) = s.day;
    item(
        poll_pill(s.last_poll, c.now, every),
        "Node",
        format!(
            "{} {}; {ok} of {tried} polls answered in a day",
            s.node,
            poll_state(s.last_poll, c.now)
        ),
    )
}

fn peers_row(c: &Ctx) -> Item {
    let list = match &c.s.agent {
        Err(_) => {
            return item(
                None,
                "Peers",
                "unknown while the agent isn't answering".into(),
            );
        }
        Ok(list) if list.is_empty() => return item(None, "Peers", "none yet".into()),
        Ok(list) => list,
    };
    let gap = pull_gap_ms(&c.cfg.sync, list.len());
    let pills: Vec<Option<Pill>> = list.iter().map(|p| peer_pill(p, c.now, gap)).collect();
    let n = |want: Option<Pill>| pills.iter().filter(|p| **p == want).count();
    let text = counts(&[
        (n(Some(Pill::Ok)), "synced lately"),
        (n(Some(Pill::Down)), "failing"),
        (n(Some(Pill::Stale)), "quiet"),
        (n(None), "not synced yet"),
    ]);
    item(
        overall(&pills),
        "Peers",
        format!("{} peer(s): {text}", list.len()),
    )
}

fn log_row(c: &Ctx) -> Item {
    let s = c.s;
    let events: i64 = s.stations.iter().map(|x| x.events + x.telemetry).sum();
    let trusted = s.stations.iter().filter(|x| x.trusted).count();
    let problems = problems(s);
    let (pill, problems) = if problems.is_empty() {
        (Pill::Ok, "nothing missing".to_string())
    } else {
        (Pill::Partial, problems)
    };
    item(
        Some(pill),
        "Log",
        format!(
            "{events} events from {} station(s) ({trusted} trusted); {problems}",
            s.stations.len()
        ),
    )
}

fn blobs_row(c: &Ctx) -> Item {
    let b = &c.s.blobs;
    let n = |want: fn(&BlobState) -> bool| b.iter().filter(|x| want(&x.state)).count();
    let text = counts(&[
        (n(|s| matches!(s, BlobState::Fetching { .. })), "fetching"),
        (n(|s| matches!(s, BlobState::Stuck(_))), "stuck"),
        (n(|s| matches!(s, BlobState::Here)), "here"),
        (n(|s| matches!(s, BlobState::Available)), "not wanted"),
    ]);
    let pills: Vec<Option<Pill>> = b.iter().map(|x| x.state.pill()).collect();
    let text = if text.is_empty() {
        "none shared yet".into()
    } else {
        text
    };
    item(overall(&pills), "Blobs", text)
}

fn disk_row(c: &Ctx) -> Item {
    let u = &c.s.slow.usage;
    let size = |n: i64| human_bytes(n.max(0) as u64);
    item(
        None,
        "Disk",
        format!(
            "{} in all: main log {}, telemetry {}, blobs {}",
            human_bytes(u.on_disk()),
            size(u.main.bytes),
            size(u.telemetry.bytes),
            human_bytes(u.blob_bytes)
        ),
    )
}

/// A line of text over a page's table, and the area left for the table.
fn above(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) -> Rect {
    let [head, rest] = Layout::vertical([
        Constraint::Length(lines.len() as u16 + 1),
        Constraint::Fill(1),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(lines), head);
    rest
}

fn peers(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let s = c.s;
    let list = match &s.agent {
        Ok(list) => list,
        Err(e) => {
            let msg = format!(
                "The agent is not answering on {} ({e}), and only the running agent knows how its pulls went; {START}.",
                s.base
            );
            f.render_widget(Notice::Error(&msg).paragraph(c.theme), area);
            return;
        }
    };
    if list.is_empty() {
        let hint = "No peers yet: add [sync] peers to the config, join with an invite, or advertise a ridgewarden service on the node.";
        f.render_widget(Notice::Empty(hint).paragraph(c.theme), area);
        return;
    }
    let gap = pull_gap_ms(&c.cfg.sync, list.len());
    let sync = &c.cfg.sync;
    let area = above(
        f,
        area,
        vec![Line::styled(
            format!(
                "Pulls from {} peer(s) every {} s, so from each one about every {}.",
                sync.fanout,
                sync.interval_s,
                human_duration(gap)
            ),
            c.theme.mute(),
        )],
    );
    let items = list.iter().map(|p| {
        (
            peer_pill(p, c.now, gap),
            vec![
                Cell::from(p.who().to_string()),
                Cell::from(p.url.clone()),
                Cell::from(p.state(c.now)),
            ],
        )
    });
    let widths = [
        Constraint::Length(10),
        Constraint::Length(26),
        Constraint::Fill(1),
    ];
    let table = pill_table(
        c.theme,
        &["Station", "Address", "Last pull"],
        &widths,
        items,
    );
    f.render_stateful_widget(table, area, rows);
}

fn log(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let s = c.s;
    let u = &s.slow.usage;
    let main: i64 = s.stations.iter().map(|x| x.events).sum();
    let telemetry: i64 = s.stations.iter().map(|x| x.telemetry).sum();
    let trusted = s.stations.iter().filter(|x| x.trusted).count();
    let problems = problems(s);
    let mut lines = vec![
        Line::from(format!(
            "{} events: {main} in main logs, {telemetry} telemetry, from {} station(s) ({trusted} trusted).",
            main + telemetry,
            s.stations.len()
        )),
        if problems.is_empty() {
            Line::from("No forks, nothing missing, nothing refused.")
        } else {
            Line::styled(format!("Needs a look: {problems}."), c.theme.bad())
        },
        Line::styled(
            format!(
                "Main log: {}; telemetry: {}. Forks and disk use as of {} ago.",
                u.main_outlook(),
                u.telemetry_outlook(),
                human_duration(c.now - s.slow.at)
            ),
            c.theme.mute(),
        ),
    ];
    for r in &s.last_refused {
        lines.push(Line::styled(format!("Refused {r}"), c.theme.bad()));
    }
    let area = above(f, area, lines);
    let items = s.stations.iter().map(|x| {
        let trust = match (x.revoked, x.trusted) {
            (true, _) => "revoked",
            (false, true) => "yes",
            (false, false) => "no",
        };
        let latest = if x.latest < 0 {
            "-".to_string()
        } else {
            format!("#{}", x.latest)
        };
        (
            Some(x.pill()),
            vec![
                Cell::from(x.call.clone()),
                Cell::from(x.key.chars().take(8).collect::<String>()),
                Cell::from(trust),
                Cell::from(x.events.to_string()),
                Cell::from(latest),
                Cell::from(x.telemetry.to_string()),
                Cell::from(x.missing.to_string()),
                Cell::from(x.forks.to_string()),
            ],
        )
    });
    let header = [
        "Station",
        "Key",
        "Trusted",
        "Events",
        "Latest",
        "Telemetry",
        "Missing",
        "Forks",
    ];
    let widths = [10, 8, 7, 7, 7, 9, 7, 5].map(Constraint::Length);
    f.render_stateful_widget(pill_table(c.theme, &header, &widths, items), area, rows);
}

fn node(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let s = c.s;
    let every = c.cfg.node.poll_interval_s as i64 * 1000;
    let (ok, tried) = s.day;
    let mut first = vec![];
    if let Some(p) = poll_pill(s.last_poll, c.now, every) {
        first.extend([tui::pill(c.theme, p), Span::raw(" ")]);
    }
    first.push(Span::raw(format!(
        "{} at {} {}, polled every {}.",
        s.node,
        c.cfg.node.url,
        poll_state(s.last_poll, c.now),
        human_duration(every)
    )));
    let mut lines = vec![
        Line::from(first),
        Line::from(format!("{ok} of {tried} polls answered in the last day.")),
    ];
    if let Some(lan) = &s.lan {
        lines.push(Line::from(format!(
            "{}, firmware {}, {} link(s) at its last answer.",
            lan.model.as_deref().unwrap_or("Unknown model"),
            lan.firmware.as_deref().unwrap_or("unknown"),
            lan.links
        )));
    }
    for p in s.polls.iter().filter(|p| !p.crawl && !p.ok).take(3) {
        lines.push(Line::styled(
            format!("Failed at {}: {}", short_time(p.at), p.error),
            c.theme.bad(),
        ));
    }
    let crawl = &c.cfg.crawl;
    lines.push(Line::styled(
        if crawl.enabled {
            format!(
                "Also crawls other nodes every {}.",
                human_duration(crawl.interval_s as i64 * 1000)
            )
        } else {
            "Crawling other nodes is off ([crawl] in the config).".into()
        },
        c.theme.mute(),
    ));
    let area = above(f, area, lines);
    if s.nodes.is_empty() {
        f.render_widget(Notice::Empty("No polls yet.").paragraph(c.theme), area);
        return;
    }
    let items = s.nodes.iter().map(|n| {
        let gap = if n.crawl {
            crawl.interval_s as i64 * 1000
        } else {
            every
        };
        let answered = n.last_ok.map_or("not lately".into(), |t| {
            format!("{} ago", human_duration(c.now - t))
        });
        (
            Pill::freshness(!n.ok, n.last_ok, c.now, gap),
            vec![
                Cell::from(n.node.clone()),
                Cell::from(if n.crawl { "crawl" } else { "lan" }),
                Cell::from(format!("{} ago", human_duration(c.now - n.at))),
                Cell::from(answered),
                Cell::from(n.links.to_string()),
                Cell::from(n.error.clone()),
            ],
        )
    });
    let header = ["Node", "Via", "Polled", "Answered", "Links", "Error"];
    let widths = [
        Constraint::Length(18),
        Constraint::Length(5),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Fill(1),
    ];
    f.render_stateful_widget(pill_table(c.theme, &header, &widths, items), area, rows);
}

fn files(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let b = &c.cfg.blobs;
    let when = match b.window() {
        Ok(Some((from, to))) => {
            let hour = (c.now / 3_600_000).rem_euclid(24) as u32;
            let now = if blobs::in_window(Some((from, to)), hour) {
                "open now"
            } else {
                "closed now"
            };
            format!("between {from:02}:00 and {to:02}:00 UTC ({now})")
        }
        _ => "at any hour".into(),
    };
    let area = above(
        f,
        area,
        vec![Line::styled(
            format!(
                "Fetches wanted blobs at up to {} kbps, {when}.",
                b.rate_kbps
            ),
            c.theme.mute(),
        )],
    );
    if c.s.blobs.is_empty() {
        let hint = "No blobs yet. Share a file with: ridgewarden blob add <file>";
        f.render_widget(Notice::Empty(hint).paragraph(c.theme), area);
        return;
    }
    let items = c.s.blobs.iter().map(|x| {
        let from = match (x.sources, x.only) {
            (0, _) => "any peer".to_string(),
            (n, true) => format!("only {n} peer(s)"),
            (n, false) => format!("{n} peer(s) first"),
        };
        (
            x.state.pill(),
            vec![
                Cell::from(x.name.clone()),
                Cell::from(human_bytes(x.size)),
                Cell::from(x.state.text()),
                Cell::from(x.priority.to_string()),
                Cell::from(from),
            ],
        )
    });
    let header = ["Name", "Size", "Progress", "Pri", "From"];
    let widths = [
        Constraint::Fill(1),
        Constraint::Length(9),
        Constraint::Length(24),
        Constraint::Length(3),
        Constraint::Length(15),
    ];
    f.render_stateful_widget(pill_table(c.theme, &header, &widths, items), area, rows);
}

fn events_table<'a>(c: &Ctx<'a>) -> Table<'a> {
    let rows = c.s.events.iter().map(|e| {
        let style = if e.trusted {
            Style::new()
        } else {
            c.theme.mute()
        };
        Row::new([
            Cell::from(clock(e.seen)),
            Cell::from(e.call.as_str()),
            Cell::from(e.kind.as_str()),
            Cell::from(if e.trusted { "yes" } else { "no" }),
            Cell::from(e.what.as_str()),
        ])
        .style(style)
    });
    let widths = [
        Constraint::Length(14),
        Constraint::Length(9),
        Constraint::Length(14),
        Constraint::Length(7),
        Constraint::Fill(1),
    ];
    Table::new(rows, widths)
        .header(
            Row::new(["Seen (UTC)", "Station", "Kind", "Trusted", "What"]).style(c.theme.heading()),
        )
        .row_highlight_style(c.theme.selected())
}

fn events(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    if c.s.events.is_empty() {
        f.render_widget(Notice::Empty("No events yet.").paragraph(c.theme), area);
        return;
    }
    let [list, _, more] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(5),
    ])
    .areas(area);
    f.render_stateful_widget(events_table(c), list, rows);
    let text = match rows.selected().and_then(|i| c.s.events.get(i)) {
        Some(e) => Paragraph::new(e.detail.as_str()),
        None => Paragraph::new(Line::styled(
            "Pick an event with the arrow keys to see all of it here.",
            c.theme.mute(),
        )),
    };
    f.render_widget(text.wrap(Wrap { trim: false }), more);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::{Json, Router};
    use ridgewarden::identity::Identity;
    use ridgewarden::log::Log;
    use ridgewarden::store::Store;
    use ridgewarden::util::esc;
    use ridgewarden_client::core::FeedItem;
    use ridgewarden_client::tui::MIN_SIZE;
    use ridgewarden_client::tui::ratatui::Terminal;
    use ridgewarden_client::tui::ratatui::backend::TestBackend;
    use ridgewarden_client::tui::ratatui::buffer::{self, Buffer};
    use ridgewarden_client::tui::ratatui::style::{Color, Modifier};
    use serde_json::json;

    fn log() -> Log {
        Log::new(
            Store::open_in_memory().unwrap(),
            Identity::from_seed("N0AAA", [1; 32]),
        )
        .unwrap()
    }

    /// A node that answers its polls with `replies` in turn, then with a 503.
    async fn node(replies: Vec<Value>) -> String {
        let replies = Arc::new(replies);
        let polls = Arc::new(AtomicUsize::new(0));
        let sysinfo = move || {
            let (replies, polls) = (replies.clone(), polls.clone());
            async move {
                match replies.get(polls.fetch_add(1, SeqCst)) {
                    Some(r) => Json(r.clone()).into_response(),
                    None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                }
            }
        };
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let router = Router::new().route("/a/sysinfo", get(sysinfo));
        tokio::spawn(async move { axum::serve(l, router).await });
        url
    }

    /// `top` showing `s`, with a loader that never runs.
    fn showing(cfg: Config, s: Snap, theme: Theme) -> Top {
        Top {
            cfg,
            theme,
            refresh: Refresh::spawn(Duration::from_secs(3600), async || {
                Err("not read in this test".to_string())
            })
            .unwrap(),
            snap: Some(s),
            err: None,
            tab: 0,
            rows: Default::default(),
            page: 0,
        }
    }

    /// One draw on a `w` by `h` terminal.
    fn frame(top: &mut Top, w: u16, h: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| top.draw(f)).unwrap();
        t.backend().buffer().clone()
    }

    /// A frame's text, row by row.
    fn text(b: &Buffer) -> String {
        let a = b.area;
        let rows =
            (0..a.height).map(|y| (0..a.width).map(|x| b[(x, y)].symbol()).collect::<String>());
        rows.collect::<Vec<_>>().join("\n")
    }

    /// The text on a `w` by `h` terminal after one draw.
    fn screen(top: &mut Top, w: u16, h: u16) -> String {
        text(&frame(top, w, h))
    }

    #[tokio::test]
    async fn top_shows_what_the_agent_keeps() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: Config = toml::from_str(&Config::example("N0AAA", dir.path())).unwrap();
        // answers the first poll like 4.x firmware, and every later one with a 503
        cfg.node.url = node(vec![json!({
            "api_version": "2.0",
            "node": "TST-PASO",
            "node_details": {"firmware_version": "4.26.10.0", "model": "Ubiquiti Rocket M5 XW"},
            "link_info": {"10.54.1.4": {"hostname": "TST-NORTH", "linkType": "RF", "interface": "wlan0", "signal": -70, "noise": -95}},
            "lqm": {"info": {"now": 100, "trackers": {"02:00:0a:36:01:04": {
                "hostname": "tst-north", "type": "RF", "ip": "10.54.1.4", "lastseen": 100,
                "signal": -70, "snr": 25, "quality": 98, "rev_quality": 97}}}},
            "hosts": [{"name": "TST-NORTH", "ip": "10.54.1.4"}],
            "services": [],
        })])
        .await;
        cfg.sync.discover = false;
        let app = agent::open(cfg.clone()).unwrap();
        app.poll_once().await.unwrap();
        app.poll_once().await.unwrap();
        let file = dir.path().join("map.pmtiles");
        std::fs::write(&file, b"tiles").unwrap();
        blobs::add_file(
            &app.log,
            &app.blobs,
            &file,
            "map.pmtiles",
            blobs::DEFAULT_PRIORITY,
        )
        .unwrap();

        let slow = Slow {
            at: now_ms(),
            forks: 0,
            usage: usage::measure(&app).unwrap(),
        };
        let base = local_base(&cfg);
        let s = read(&app, base.clone(), Err("could not connect".into()), slow).unwrap();
        assert_eq!((s.node.as_str(), s.day), ("tst-paso", (1, 2)));
        assert!(matches!(s.last_poll, Some((_, false))));
        assert_eq!(s.nodes.len(), 1, "both polls were of our own node");
        let n = &s.nodes[0];
        assert!(!n.ok && n.last_ok.is_some(), "down now, answered before");
        assert_eq!(n.error, "HTTP 503");
        assert_eq!(s.blobs.len(), 1);
        assert_eq!(s.blobs[0].state, BlobState::Here);
        let whats: Vec<&str> = s.events[..2].iter().map(|e| e.what.as_str()).collect();
        assert_eq!(
            whats,
            [
                "shares map.pmtiles (5 B)",
                "tst-paso did not answer (lan): HTTP 503"
            ],
            "newest first"
        );
        assert_eq!(
            (s.stations[0].call.as_str(), s.stations[0].telemetry),
            ("N0AAA", 2)
        );

        let mut top = showing(cfg, s, Theme { color: false });
        let overview = screen(&mut top, 100, 30);
        assert!(
            overview.contains("ridgewarden top  N0AAA on tst-paso"),
            "{overview}"
        );
        assert!(
            overview.contains(&format!("not answering on {base} (could not connect)")),
            "{overview}"
        );
        assert!(
            overview.contains("\n          Peers  unknown while the agent isn't answering"),
            "a row without a pill keeps its columns: {overview}"
        );
        top.tab = 3;
        let node = screen(&mut top, 100, 30);
        assert!(
            node.contains(
                "Ubiquiti Rocket M5 XW, firmware 4.26.10.0, 1 link(s) at its last answer."
            ),
            "{node}"
        );
        assert!(node.contains("1 of 2 polls answered in the last day."));
        top.tab = 4;
        assert!(screen(&mut top, 100, 30).contains("map.pmtiles"));
        for tab in 0..TABS.len() {
            top.tab = tab;
            assert!(!screen(&mut top, MIN_SIZE.0, MIN_SIZE.1).contains("Make the terminal"));
        }
        assert!(
            screen(&mut top, MIN_SIZE.0 - 1, MIN_SIZE.1).contains("Make the terminal at least")
        );
    }

    /// Site 1 of three on the simulator, with site 2 cut off: the README's picture of `top`.
    /// The fixture is what site 1's core and node answered during that partition drill: its
    /// peers and blobs, the other two stations' events, and the node's sysinfo. With
    /// README_SHOTS=1 the frame is also written to ridgewarden-top.html in the temp folder,
    /// to be screenshotted.
    #[tokio::test]
    async fn the_overview_during_a_partition_drill() {
        let fx: Value =
            serde_json::from_str(include_str!("../tests/fixtures/top-drill.json")).unwrap();
        let feed: Vec<FeedItem> = serde_json::from_value(fx["feed"].clone()).unwrap();
        let events: Vec<Event> = feed.into_iter().map(|i| i.event).collect();
        let dir = tempfile::tempdir().unwrap();
        // site 1's key in the drill
        std::fs::write(dir.path().join("identity.key"), hex::encode([1u8; 32])).unwrap();
        let mut cfg: Config = toml::from_str(&Config::example("N0CALL-1", dir.path())).unwrap();
        cfg.node.url = node(vec![fx["sysinfo"].clone()]).await;
        cfg.sync.discover = false;
        let others: BTreeSet<String> = events.iter().map(|e| e.author.key.clone()).collect();
        cfg.trust.keys = others.into_iter().collect();
        let app = agent::open(cfg.clone()).unwrap();
        assert_eq!(app.log.ingest(&events).unwrap().accepted, events.len());
        app.poll_once().await.unwrap();
        for b in fx["blobs"]["blobs"].as_array().unwrap() {
            if b["wanted"] == true && b["complete"] == false {
                blobs::want(&app.log.store.conn(), b["root"].as_str().unwrap(), true).unwrap();
            }
        }
        // each peer last heard from as long ago as it was then
        let shift = now_ms() - fx["at"].as_i64().unwrap();
        let mut peers = serde_json::from_value::<Peers>(fx["peers"].clone())
            .unwrap()
            .peers;
        for p in &mut peers {
            p.last_ok = p.last_ok.map(|t| t + shift);
        }
        let slow = Slow {
            at: now_ms(),
            forks: app.log.fork_count().unwrap(),
            usage: usage::measure(&app).unwrap(),
        };
        let s = read(&app, local_base(&cfg), Ok(peers), slow).unwrap();
        let mut top = showing(cfg, s, Theme { color: true });
        let b = frame(&mut top, 80, 24);
        let shown = text(&b);
        for line in [
            "ridgewarden top  N0CALL-1 on tst-shack",
            " OK       Agent  running, dashboard on http://127.0.0.1:8790/",
            " PARTIAL  Peers  2 peer(s): 1 synced lately, 1 failing",
            "from 3 station(s) (3 trusted); nothing missing",
            " PARTIAL  Blobs  1 fetching",
        ] {
            assert!(shown.contains(line), "{line:?} in\n{shown}");
        }
        if std::env::var_os("README_SHOTS").is_some() {
            std::fs::write(std::env::temp_dir().join("ridgewarden-top.html"), html(&b)).unwrap();
        }
    }

    /// A frame as a page of colored monospace cells, light or dark with the system.
    fn html(b: &Buffer) -> String {
        let mut page = String::from(
            "<!doctype html><meta charset=utf-8><title>ridgewarden top</title><style>\
             :root{--fg:#1f2328;--bg:#fff;--edge:#d0d7de}\
             @media (prefers-color-scheme:dark){:root{--fg:#d1d7e0;--bg:#0d1117;--edge:#30363d}}\
             body{margin:0;background:var(--bg)}\
             pre{display:inline-block;margin:0;padding:10px 14px;border:1px solid var(--edge);\
             font:16px/1.3 Consolas,monospace;color:var(--fg);background:var(--bg)}\
             </style><pre>",
        );
        for y in 0..b.area.height {
            let mut run = (String::new(), String::new());
            for x in 0..b.area.width {
                let c = &b[(x, y)];
                let style = css(c);
                if style != run.0 {
                    span(&mut page, &run);
                    run = (style, String::new());
                }
                run.1.push_str(c.symbol());
            }
            span(&mut page, &run);
            page.push('\n');
        }
        page + "</pre>"
    }

    fn span(page: &mut String, (style, text): &(String, String)) {
        if !text.is_empty() {
            let _ = write!(page, "<span style=\"{style}\">{}</span>", esc(text));
        }
    }

    /// A cell as CSS: Tango's palette for the pills, and the page's own colors for the
    /// terminal's default ones.
    fn css(c: &buffer::Cell) -> String {
        let color = |c: Color, default: &'static str| match c {
            Color::Reset => default,
            Color::Black => "#2e3436",
            Color::Red => "#cc0000",
            Color::Green => "#4e9a06",
            Color::Yellow => "#c4a000",
            Color::Gray => "#d3d7cf",
            Color::White => "#eeeeec",
            other => panic!("no CSS for {other:?}"),
        };
        let (mut fg, mut bg) = (color(c.fg, "var(--fg)"), color(c.bg, "var(--bg)"));
        if c.modifier.contains(Modifier::REVERSED) {
            std::mem::swap(&mut fg, &mut bg);
        }
        let mut s = format!("color:{fg};background:{bg}");
        if c.modifier.contains(Modifier::BOLD) {
            s.push_str(";font-weight:bold");
        }
        if c.modifier.contains(Modifier::DIM) {
            s.push_str(";opacity:.6");
        }
        s
    }

    #[test]
    fn each_kind_of_event_reads_as_one_short_line() {
        let l = log();
        let ev = |kind: &str, p: Value| l.append(kind, p).unwrap();
        let lines = [
            (
                ev(
                    "node.poll",
                    json!({"node": "tst-cuesta", "ok": true, "via": "lan", "links": [{}, {}]}),
                ),
                "tst-cuesta answered (lan), 2 link(s)",
            ),
            (
                ev(
                    "node.poll",
                    json!({"node": "tst-paso", "ok": false, "via": "crawl", "error": "timed out"}),
                ),
                "tst-paso did not answer (crawl): timed out",
            ),
            (
                ev("trust.list", json!({"version": 3, "stations": [{}, {}]})),
                "trust list v3, 2 station(s)",
            ),
            (
                ev(
                    "blob.manifest",
                    json!({"name": "map.pmtiles", "size": 3400}),
                ),
                "shares map.pmtiles (3.4 KB)",
            ),
            (ev("node.poll", json!({})), "? did not answer (?): ?"),
        ];
        for (e, want) in lines {
            assert_eq!(what(&e), want);
        }
        let flood = ev("x.note", json!({"text": "a".repeat(10_000)}));
        assert_eq!(what(&flood).chars().count(), LINE);
        assert!(detail(&flood).chars().count() <= 5 * LINE);
    }

    fn head(key: &str, call: &str, seq: i64, max_seq: i64, count: i64, forks: Vec<i64>) -> Head {
        Head {
            key: key.into(),
            call: call.into(),
            seq,
            max_seq,
            count,
            forks,
            digest: String::new(),
        }
    }

    #[test]
    fn stations_add_up_their_chains_and_show_what_is_wrong() {
        let heads = [
            head("bb", "N0BBB", 3, 7, 6, vec![2]),
            head("bb:telemetry", "N0BBB", 5, 5, 6, vec![]),
            head("aa", "N0AAA", 9, 9, 10, vec![]),
            head("aa:telemetry", "N0AAA", 99, 99, 100, vec![]),
            head("cc", "N0CCC", 1, 1, 2, vec![]),
        ];
        let trusted = ["aa".to_string(), "cc".to_string()].into();
        let revoked = ["cc".to_string()].into();
        let st = stations(&heads, &trusted, &revoked, "aa");
        let calls: Vec<&str> = st.iter().map(|s| s.call.as_str()).collect();
        assert_eq!(calls, ["N0AAA", "N0BBB", "N0CCC"], "this station first");
        assert_eq!(
            (st[0].events, st[0].latest, st[0].telemetry, st[0].pill()),
            (10, 9, 100, Pill::Ok)
        );
        assert_eq!((st[1].missing, st[1].forks, st[1].trusted), (4, 1, false));
        assert_eq!(st[1].pill(), Pill::Partial);
        assert_eq!(st[2].pill(), Pill::Down, "revoked");
    }

    fn poll(at: i64, node: &str, ok: bool, crawl: bool) -> Poll {
        Poll::of(
            at,
            &json!({"node": node, "ok": ok, "via": if crawl { "crawl" } else { "lan" },
                    "error": if ok { "" } else { "timed out" }}),
        )
    }

    #[test]
    fn each_node_shows_its_latest_poll_and_last_answer() {
        let polls = [
            poll(100, "tst-shack", true, false),
            poll(90, "tst-paso", false, true),
            poll(80, "tst-north", true, true),
            poll(50, "tst-paso", true, true),
            poll(40, "tst-shack", false, false),
        ];
        let rows = nodes(&polls);
        let got: Vec<(&str, bool, i64, Option<i64>)> = rows
            .iter()
            .map(|r| (r.node.as_str(), r.ok, r.at, r.last_ok))
            .collect();
        assert_eq!(
            got,
            [
                ("tst-shack", true, 100, Some(100)),
                ("tst-north", true, 80, Some(80)),
                ("tst-paso", false, 90, Some(50)),
            ]
        );
        assert_eq!(rows[2].error, "timed out");
    }

    #[test]
    fn peers_go_quiet_after_three_of_their_pulls() {
        let sync = SyncCfg {
            interval_s: 120,
            fanout: 2,
            ..Default::default()
        };
        let gap = pull_gap_ms(&sync, 5);
        assert_eq!(gap, 360_000, "5 peers, 2 a round: each every third round");
        assert_eq!(pull_gap_ms(&sync, 0), 120_000);
        let peer = |last_ok, last_err: Option<&str>| PeerState {
            url: "http://10.1.2.3:8790".into(),
            call: None,
            last_ok,
            last_err: last_err.map(str::to_string),
        };
        let now = 10_000_000;
        assert_eq!(
            peer_pill(&peer(Some(now - 3 * gap), None), now, gap),
            Some(Pill::Ok)
        );
        assert_eq!(
            peer_pill(&peer(Some(now - 3 * gap - 1), None), now, gap),
            Some(Pill::Stale)
        );
        assert_eq!(
            peer_pill(&peer(Some(now), Some("timed out")), now, gap),
            Some(Pill::Down)
        );
        assert_eq!(peer_pill(&peer(None, None), now, gap), None);
        assert_eq!(peer(None, None).state(now), "not synced yet");
        assert_eq!(peer(None, Some("HTTP 503")).state(now), "failing: HTTP 503");
    }

    #[test]
    fn a_group_is_partial_when_its_members_disagree() {
        let (ok, down) = (Some(Pill::Ok), Some(Pill::Down));
        assert_eq!(overall(&[ok, ok, None]), ok);
        assert_eq!(overall(&[ok, down]), Some(Pill::Partial));
        assert_eq!(overall(&[down, None]), down);
        assert_eq!(overall(&[None, None]), None);
        assert_eq!(overall(&[]), None);
        assert_eq!(
            counts(&[(2, "synced lately"), (0, "failing"), (1, "quiet")]),
            "2 synced lately, 1 quiet"
        );
    }

    #[test]
    fn blob_progress_reads_plainly() {
        assert_eq!(percent(1, 3), 33);
        assert_eq!(percent(2, 3), 66, "rounded down: 100 only when done");
        assert_eq!(percent(0, 0), 100);
        let fetching = |have, total, index| BlobState::Fetching { have, total, index };
        assert_eq!(
            fetching(412, 1180, false).text(),
            "34% (412 of 1180 chunks)"
        );
        assert_eq!(fetching(1, 2, true).text(), "chunk list 1 of 2");
        assert_eq!(BlobState::Here.pill(), Some(Pill::Ok));
        assert_eq!(BlobState::Available.pill(), None);
    }

    #[test]
    fn times_and_polls_read_like_status() {
        assert_eq!(clock(1_795_000_000_123), "11-18 11:06:40");
        let now = 1_795_000_000_000;
        assert_eq!(
            poll_state(Some((now - 180_000, true)), now),
            "answered 3 min ago"
        );
        assert_eq!(poll_state(None, now), "not polled yet");
        assert_eq!(
            poll_pill(Some((now - 1000, false)), now, 60_000),
            Some(Pill::Down)
        );
        assert_eq!(
            poll_pill(Some((now - 200_000, true)), now, 60_000),
            Some(Pill::Stale)
        );
        assert_eq!(poll_pill(None, now, 60_000), None);
    }
}
