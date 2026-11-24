//! HTTP: the sync API, the local API apps use, and the core's own small dashboard.
//! No JavaScript, no CDN, nothing fetched from the internet.

use std::fmt::Write as _;
use std::sync::Arc;

use std::net::SocketAddr;

use axum::Extension;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use ridgewarden_client::core::{FEED_PAGE, FeedItem};
use ridgewarden_client::ui::{Color, Page, pill};

use crate::app::App;
use crate::diff;
use crate::event::Event;
use crate::log::MAX_EVENT_BYTES;
use crate::sync::PAGE;
use crate::util::{esc, human_duration, now_ms, short_time};

#[derive(Clone)]
struct Ctx {
    app: Arc<App>,
    token: Option<Arc<String>>,
}

type S = State<Ctx>;

/// `token` enables `POST /local/v1/append`; without it, apps can read but not write.
pub fn router(app: Arc<App>, token: Option<String>) -> Router {
    Router::new()
        .route("/", get(overview))
        .route("/changes", get(changes_page))
        .route("/invite", get(invite_page))
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/heads", get(heads))
        .route("/v1/events", get(events))
        .route("/v1/identity", get(identity))
        .route("/v1/digests", get(digests))
        .route("/v1/feed", get(feed))
        .route("/v1/whoami", get(whoami))
        .route("/v1/peers", get(peers))
        .route("/local/v1/append", post(append))
        .route("/local/v1/blob/want", post(blob_want))
        .route("/local/v1/blob/export", post(blob_export))
        .route("/local/v1/blob/add", post(blob_add))
        .route("/local/v1/blob/unwant", post(blob_unwant))
        .route("/local/v1/metrics", get(metrics))
        .route("/v1/chunk/{hash}", get(chunk))
        .route("/v1/blobs", get(blob_list))
        .with_state(Ctx {
            app,
            token: token.map(Arc::new),
        })
}

struct Fail(StatusCode, String);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            tracing::error!("request failed: {}", self.1);
            return (self.0, "internal error").into_response();
        }
        (self.0, self.1).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e.into()))
    }
}

type R<T> = Result<T, Fail>;

async fn heads(State(c): S) -> R<Json<Value>> {
    Ok(Json(json!({"heads": c.app.log.heads()?})))
}

#[derive(Deserialize)]
struct EventsQ {
    author: String,
    #[serde(default = "minus_one")]
    after: i64,
    after_id: Option<String>,
    /// every event at exactly this seq (fork branches), instead of a page
    seq: Option<i64>,
    limit: Option<usize>,
}

fn minus_one() -> i64 {
    -1
}

async fn events(State(c): S, Query(q): Query<EventsQ>) -> R<Json<Vec<Event>>> {
    if let Some(seq) = q.seq {
        return Ok(Json(c.app.log.events_at(&q.author, seq)?));
    }
    let limit = q.limit.unwrap_or(PAGE).clamp(1, PAGE);
    Ok(Json(c.app.log.events_after(
        &q.author,
        q.after,
        q.after_id.as_deref(),
        limit,
    )?))
}

#[derive(Deserialize)]
struct DigestsQ {
    author: String,
}

async fn digests(State(c): S, Query(q): Query<DigestsQ>) -> R<Json<Value>> {
    let d = c.app.log.bucket_digests(&q.author, i64::MAX)?;
    Ok(Json(
        json!({"bucket": crate::log::DIGEST_BUCKET, "digests": d}),
    ))
}

async fn identity(State(c): S) -> R<Json<Event>> {
    Ok(Json(c.app.log.bind_event()?))
}

#[derive(Deserialize)]
struct FeedQ {
    #[serde(default = "minus_one")]
    after: i64,
    limit: Option<usize>,
}

async fn feed(State(c): S, Query(q): Query<FeedQ>) -> R<Json<Value>> {
    let limit = q.limit.unwrap_or(FEED_PAGE).clamp(1, FEED_PAGE);
    let events: Vec<FeedItem> = c.app.log.feed(q.after, limit)?;
    Ok(Json(
        // the trust epoch is part of the instance, so apps rebuild their views when the
        // set of trusted stations changes instead of having skipped those events for good
        json!({"instance": format!("{}-{}", c.app.log.store.instance()?, c.app.log.trust_epoch()), "events": events}),
    ))
}

async fn whoami(State(c): S) -> Json<Value> {
    let a = &c.app.log.id.author;
    // empty until a poll has told us the node's real name, so apps don't log under a guess
    let node = c.app.node_name.lock().unwrap().clone().unwrap_or_default();
    Json(json!({"call": a.call, "key": a.key, "node": node}))
}

#[derive(Deserialize)]
struct AppendReq {
    kind: String,
    payload: Value,
    /// write to the station's telemetry log, which is pruned after a while
    #[serde(default)]
    telemetry: bool,
}

type Peer = Option<Extension<ConnectInfo<SocketAddr>>>;

fn from_loopback(peer: &Peer) -> bool {
    peer.as_ref()
        .is_some_and(|Extension(ConnectInfo(a))| a.ip().to_canonical().is_loopback())
}

/// The local API's gate: loopback only, with the token. The token travels in clear HTTP,
/// so it is only accepted from this machine; on the mesh anyone could sniff it.
fn local_ok(c: &Ctx, peer: &Peer, headers: &HeaderMap) -> R<()> {
    if !from_loopback(peer) {
        return Err(Fail(
            StatusCode::FORBIDDEN,
            "the local API only answers on loopback".into(),
        ));
    }
    let Some(token) = &c.token else {
        return Err(Fail(
            StatusCode::FORBIDDEN,
            "writing is disabled on this agent".into(),
        ));
    };
    if !token_ok(token, headers) {
        return Err(Fail(
            StatusCode::UNAUTHORIZED,
            "bad or missing token".into(),
        ));
    }
    Ok(())
}

async fn chunk(State(c): S, Path(hash): Path<String>) -> Response {
    match c.app.blobs.get(&hash) {
        Ok(Some(data)) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            data,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such chunk").into_response(),
        Err(e) => Fail::from(e).into_response(),
    }
}

#[derive(Deserialize)]
struct BlobsQ {
    root: Option<String>,
}

/// One blob with `?root=`, else up to 1000: complete and wanted ones first, so a flood of
/// manifests nobody asked for can't push them out of the list.
async fn blob_list(State(c): S, Query(q): Query<BlobsQ>) -> R<Response> {
    let conn = c.app.log.store.conn();
    if let Some(root) = q.root {
        return Ok(
            match crate::blobs::status(&conn, &root, &c.app.log.trusted())? {
                Some(v) => Json(v).into_response(),
                None => (StatusCode::NOT_FOUND, "no manifest for that blob").into_response(),
            },
        );
    }
    let mut st = conn.prepare(
        "SELECT root, name, size, complete, wanted FROM blobs
         ORDER BY complete DESC, wanted DESC, added_ms DESC LIMIT 1000",
    )?;
    let rows: Vec<Value> = st
        .query_map([], |r| {
            Ok(
                json!({"root": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                      "size": r.get::<_, i64>(2)?, "complete": r.get::<_, bool>(3)?,
                      "wanted": r.get::<_, bool>(4)?}),
            )
        })?
        .collect::<Result<_, _>>()?;
    Ok(Json(json!({"blobs": rows})).into_response())
}

async fn blob_unwant(
    State(c): S,
    peer: Peer,
    headers: HeaderMap,
    Json(req): Json<BlobReq>,
) -> R<Json<Value>> {
    local_ok(&c, &peer, &headers)?;
    let app = c.app.clone();
    let root = req.root.clone();
    // deleting chunks and reading big chunk lists is disk work, not for the request threads
    let freed = tokio::task::spawn_blocking(move || {
        crate::blobs::unwant(&app.log.store.conn(), &app.blobs, &root)
    })
    .await
    .map_err(|e| {
        Fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("unwant stopped: {e}"),
        )
    })??;
    match freed {
        Some(n) => Ok(Json(json!({"unwanted": req.root, "freed": n}))),
        None => Err(Fail(
            StatusCode::NOT_FOUND,
            "no manifest for that blob".into(),
        )),
    }
}

/// The peers this agent pulls from and how the last pull went (for `ridgewarden status`).
async fn peers(State(c): S) -> Json<Value> {
    let peers: Vec<Value> = c
        .app
        .peers
        .lock()
        .unwrap()
        .values()
        .map(
            |p| json!({"url": p.url, "call": p.call, "last_ok": p.last_ok, "last_err": p.last_err}),
        )
        .collect();
    Json(json!({"peers": peers}))
}

#[derive(Deserialize)]
struct BlobReq {
    root: String,
    /// for want: peers to fetch chunks from first, in order; omitted keeps the current list
    peers: Option<Vec<String>>,
    /// for want: ask only `peers`
    #[serde(default)]
    only: bool,
    /// for export: a file name; the file goes in the agent's export folder
    name: Option<String>,
}

/// A plain file name: no directories, no dot-dot, nothing an app could use to write
/// somewhere else (like over the station's key).
fn plain_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 100
        && !n.starts_with('.')
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

async fn blob_want(
    State(c): S,
    peer: Peer,
    headers: HeaderMap,
    Json(req): Json<BlobReq>,
) -> R<Json<Value>> {
    local_ok(&c, &peer, &headers)?;
    let sources = match &req.peers {
        Some(p) => Some(
            crate::blobs::sources_from(p)
                .map_err(|e| Fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?,
        ),
        None if req.only => {
            return Err(Fail(
                StatusCode::BAD_REQUEST,
                "only needs a list of peers".into(),
            ));
        }
        None => None,
    };
    let conn = c.app.log.store.conn();
    crate::blobs::prefer_trusted(&conn, &req.root, &c.app.log.trusted())?;
    let found = match &sources {
        Some(s) => crate::blobs::want_from(&conn, &req.root, s, req.only)?,
        None => crate::blobs::want(&conn, &req.root, true)?,
    };
    if !found {
        return Err(Fail(
            StatusCode::NOT_FOUND,
            "no manifest for that blob yet".into(),
        ));
    }
    Ok(Json(json!({"wanted": req.root})))
}

async fn blob_export(
    State(c): S,
    peer: Peer,
    headers: HeaderMap,
    Json(req): Json<BlobReq>,
) -> R<Json<Value>> {
    local_ok(&c, &peer, &headers)?;
    let Some(name) = req.name.filter(|n| plain_name(n)) else {
        return Err(Fail(
            StatusCode::BAD_REQUEST,
            "export needs a plain file name".into(),
        ));
    };
    let Some(m) = crate::blobs::manifest(&c.app.log.store.conn(), &req.root)? else {
        return Err(Fail(
            StatusCode::NOT_FOUND,
            "no manifest for that blob".into(),
        ));
    };
    let dir = c.app.cfg.data_dir.join("exports");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(&name);
    // re-hashes every chunk: minutes for a big file, so not on the request threads
    let app = c.app.clone();
    let out = path.clone();
    let size = m.size;
    tokio::task::spawn_blocking(move || {
        let r = app.blobs.export(&m, &out);
        if r.is_err() {
            crate::blobs::recheck(&app.log.store.conn(), &app.blobs, &m)?;
        }
        r
    })
    .await
    .map_err(|e| {
        Fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("export stopped: {e}"),
        )
    })?
    .map_err(|e| Fail(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(Json(json!({"exported": path, "size": size})))
}

#[derive(Deserialize)]
struct AddReq {
    /// a plain file name in the agent's import folder
    file: String,
    name: String,
    #[serde(default)]
    priority: Option<u8>,
}

/// Publish a file an app has put in the import folder. Only that folder, so an app can't
/// publish the station's key or anything else the core can read.
async fn blob_add(
    State(c): S,
    peer: Peer,
    headers: HeaderMap,
    Json(req): Json<AddReq>,
) -> R<Json<Value>> {
    local_ok(&c, &peer, &headers)?;
    if !plain_name(&req.file) {
        return Err(Fail(
            StatusCode::BAD_REQUEST,
            "add needs a plain file name in the import folder".into(),
        ));
    }
    let name = req.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(Fail(
            StatusCode::BAD_REQUEST,
            "a blob name is 1 to 200 characters".into(),
        ));
    }
    let path = c.app.cfg.imports_dir().join(&req.file);
    if !path.is_file() {
        return Err(Fail(
            StatusCode::NOT_FOUND,
            format!("no file {} in the import folder", req.file),
        ));
    }
    let priority = req.priority.unwrap_or(crate::blobs::DEFAULT_PRIORITY);
    let app = c.app.clone();
    let root = tokio::task::spawn_blocking(move || {
        crate::blobs::add_file(&app.log, &app.blobs, &path, &name, priority)
    })
    .await
    .map_err(|e| {
        Fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("add stopped: {e}"),
        )
    })?
    .map_err(|e| Fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({"root": root})))
}

/// Kinds only the core itself may write.
fn reserved(kind: &str) -> bool {
    kind.starts_with("identity.")
        || matches!(
            kind,
            "node.poll" | "mesh.list" | "trust.list" | "log.checkpoint" | "blob.manifest"
        )
}

// compare the hashes, so the time taken doesn't leak how much of the token matched
fn token_ok(expected: &str, headers: &HeaderMap) -> bool {
    let Some(got) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    blake3::hash(got.trim().as_bytes()) == blake3::hash(expected.as_bytes())
}

async fn append(
    State(c): S,
    peer: Peer,
    headers: HeaderMap,
    Json(req): Json<AppendReq>,
) -> R<Json<Event>> {
    local_ok(&c, &peer, &headers)?;
    let kind = req.kind.trim();
    if kind.is_empty() || kind.len() > 64 || reserved(kind) {
        return Err(Fail(
            StatusCode::BAD_REQUEST,
            format!("kind {kind:?} is not allowed"),
        ));
    }
    if req.payload.to_string().len() > MAX_EVENT_BYTES / 2 {
        return Err(Fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload too large".into(),
        ));
    }
    let ev = if req.telemetry {
        c.app.log.append_telemetry(kind, req.payload)?
    } else {
        c.app.log.append(kind, req.payload)?
    };
    Ok(Json(ev))
}

async fn metrics(State(c): S, peer: Peer, headers: HeaderMap) -> R<Response> {
    if !c.app.cfg.metrics.enabled {
        return Err(Fail(
            StatusCode::NOT_FOUND,
            "metrics are off on this agent ([metrics] enabled = true turns them on)".into(),
        ));
    }
    local_ok(&c, &peer, &headers)?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        crate::metrics::render(&c.app)?,
    )
        .into_response())
}

/// `refresh`: reload every minute, for pages people leave open.
fn page(app: &App, title: &str, active: &str, body: &str, refresh: bool) -> Html<String> {
    let mut nav: Vec<(&str, &str)> = vec![
        ("/", "Overview"),
        ("/changes", "Changes"),
        ("/invite", "Invite"),
    ];
    for a in &app.cfg.apps {
        nav.push((a.url.as_str(), a.name.as_str()));
    }
    let who = format!("{} ({})", app.node_name(), app.log.id.author.call);
    Html(
        Page {
            app: "ridgewarden",
            nav: &nav,
            active,
            who: &who,
            refresh,
        }
        .render(title, body),
    )
}

async fn overview(State(c): S) -> R<Html<String>> {
    let app = &c.app;
    let now = now_ms();
    let (events, last_poll) = {
        let conn = app.log.store.conn();
        let events: i64 = conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?;
        (events, crate::node::last_poll(&conn)?)
    };
    let heads = app.log.heads()?;
    let forks = app.log.fork_count()?;
    let trusted = app.log.trusted();
    let mut b = String::from("<h1>ridgewarden core</h1>");

    let (color, line) = match last_poll {
        Some((t, true)) if now - t < 3 * app.cfg.node.poll_interval_s as i64 * 1000 => (
            Color::Green,
            format!(
                "Polling {}; last answered {} ago.",
                app.cfg.node.url,
                human_duration(now - t)
            ),
        ),
        Some((t, ok)) => (
            Color::Red,
            format!(
                "Node {} {} at {}.",
                app.cfg.node.url,
                if ok {
                    "last answered"
                } else {
                    "did not answer"
                },
                short_time(t)
            ),
        ),
        None => (Color::Unknown, "No poll yet.".into()),
    };
    let _ = write!(
        b,
        "<div class=\"headline {}\">{}</div>",
        color.css(),
        esc(&line)
    );

    let _ = write!(
        b,
        "<div class=cards><div class=card><div class=n>{events}</div>events in the log</div>\
         <div class=card><div class=n>{}</div>stations writing</div>\
         <div class=card><div class=n>{forks}</div>forks seen</div></div>",
        heads.iter().filter(|h| !h.key.contains(':')).count()
    );

    b.push_str("<h2>Apps</h2>");
    if app.cfg.apps.is_empty() {
        b.push_str("<p class=mute>No apps linked. Install one (ridgewarden-health, ridgewarden-linkdoctor, ridgewarden-status) and add an <code>[[app]]</code> entry.</p>");
    } else {
        b.push_str("<ul class=plain>");
        for a in &app.cfg.apps {
            let _ = write!(
                b,
                "<li><a href=\"{}\">{}</a></li>",
                esc(&a.url),
                esc(&a.name)
            );
        }
        b.push_str("</ul>");
    }

    b.push_str("<h2>What changed in the last 24 hours</h2>");
    let changes = diff::changes(&app.log.store.conn(), now, 24)?;
    match changes {
        Some(ch) if !ch.lines.is_empty() => {
            b.push_str("<ul class=plain>");
            for l in &ch.lines {
                let _ = write!(b, "<li>{}</li>", esc(l));
            }
            let _ = write!(
                b,
                "</ul><p class=\"mute small\">Comparing {} with {}.</p>",
                short_time(ch.from_ts),
                short_time(ch.to_ts)
            );
        }
        Some(_) => b.push_str("<p class=mute>No changes.</p>"),
        None => b.push_str("<p class=mute>Not enough history yet.</p>"),
    }

    b.push_str("<h2>Peers</h2>");
    let peers: Vec<_> = app.peers.lock().unwrap().values().cloned().collect();
    if peers.is_empty() {
        b.push_str("<p class=mute>No other agents found yet. Add <code>[sync] peers</code> or advertise a service named <code>ridgewarden</code> on your node.</p>");
    } else {
        b.push_str(
            "<div class=wrap><table><tr><th>Agent</th><th>Address</th><th>Last sync</th></tr>",
        );
        for p in peers {
            let last = match (&p.last_err, p.last_ok) {
                (Some(e), _) => format!("{} {}", pill(Color::Red, "failing"), esc(e)),
                (None, Some(t)) => format!("{} ago", human_duration(now - t)),
                (None, None) => "not yet".into(),
            };
            let _ = write!(
                b,
                "<tr><td>{}</td><td>{}</td><td>{last}</td></tr>",
                esc(p.call.as_deref().unwrap_or("?")),
                esc(&p.url)
            );
        }
        b.push_str("</table></div>");
    }

    match crate::usage::measure(app) {
        Ok(u) => disk_section(&mut b, &u),
        Err(e) => {
            let _ = write!(
                b,
                "<h2>Disk use</h2><p class=mute>Couldn't measure disk use: {}</p>",
                esc(&format!("{e:#}"))
            );
        }
    }

    if trusted.len() <= 1 {
        b.push_str("<p class=mute>This site trusts only itself, so apps show only its own data. Join the other sites with <code>ridgewarden join</code> and an invite from one of them, or make one for a new site on the <a href=\"/invite\">invite page</a>.</p>");
    }

    b.push_str("<h2>Stations in the log</h2><div class=wrap><table><tr><th>Callsign</th><th>Key</th><th>Trusted</th><th>Events</th><th>Telemetry kept</th></tr>");
    let telemetry: std::collections::BTreeMap<&str, i64> = heads
        .iter()
        .filter_map(|h| h.key.strip_suffix(":telemetry").map(|k| (k, h.count)))
        .collect();
    for h in heads.iter().filter(|h| !h.key.contains(':')) {
        let gap = if h.max_seq > h.seq {
            format!(" ({} missing so far)", h.max_seq - h.seq)
        } else {
            String::new()
        };
        let trust = if trusted.contains(&h.key) {
            pill(Color::Green, "yes")
        } else {
            pill(Color::Unknown, "no")
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td class=\"mute small\">{}</td><td>{trust}</td><td>{}{gap}</td><td>{}</td></tr>",
            esc(&h.call),
            esc(&h.key[..16.min(h.key.len())]),
            h.count,
            telemetry.get(h.key.as_str()).copied().unwrap_or(0)
        );
    }
    b.push_str("</table></div>");
    Ok(page(app, "Overview", "/", &b, true))
}

fn disk_section(b: &mut String, u: &crate::usage::Usage) {
    use crate::usage::human_bytes as hb;
    let size = |n: i64| hb(n.max(0) as u64);
    let _ = write!(
        b,
        "<h2>Disk use</h2><div class=wrap><table>\
         <tr><th>Class</th><th>Events</th><th>Size now</th><th>Outlook</th></tr>\
         <tr><td>Main log (kept)</td><td>{}</td><td>{}</td><td>{}</td></tr>\
         <tr><td>Telemetry ({} days)</td><td>{}</td><td>{}</td><td>{}</td></tr>\
         <tr><td>Blobs</td><td>{} chunks</td><td>{}</td><td>whatever this site fetches</td></tr>\
         <tr><td>Database file</td><td></td><td>{}</td><td>events, indexes, the node's polls (8 days parsed, 2 raw)</td></tr>\
         <tr><td>Exports, imports</td><td></td><td>{}</td><td></td></tr>\
         <tr><th>Total on disk</th><td></td><th>{}</th><td></td></tr></table></div>\
         <p class=\"mute small\">Event sizes are their JSON, as synced. Rates come from the last \
         week of events; telemetry levels off once {} days have passed, if other stations keep \
         theirs as long as this one.</p>",
        u.main.events,
        size(u.main.bytes),
        u.main_outlook(),
        u.telemetry_days,
        u.telemetry.events,
        size(u.telemetry.bytes),
        u.telemetry_outlook(),
        u.blob_chunks,
        hb(u.blob_bytes),
        hb(u.db_bytes),
        hb(u.files_bytes),
        hb(u.on_disk()),
        u.telemetry_days,
    );
}

#[derive(Deserialize)]
struct InviteQ {
    url: Option<String>,
}

/// An invite with its QR code. Only shown to someone on this machine, like the rest of
/// the local actions: an invite is signed by the station's key.
async fn invite_page(State(c): S, peer: Peer, Query(q): Query<InviteQ>) -> R<Response> {
    let app = &c.app;
    let mut b = String::from("<h1>Invite a new site</h1>");
    if !from_loopback(&peer) {
        b.push_str("<p>Invites are only shown on this machine itself, because they are signed with this station's key. Open this page on the Pi (or through <code>ssh -L 8790:127.0.0.1:8790</code> and <code>http://127.0.0.1:8790/invite</code>), or run <code>ridgewarden invite</code> there.</p>");
        return Ok((
            StatusCode::FORBIDDEN,
            page(app, "Invite", "/invite", &b, false),
        )
            .into_response());
    }
    let url = q
        .url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .or_else(|| app.cfg.public_url.clone());
    let form = |b: &mut String, value: &str| {
        let _ = write!(
            b,
            "<form method=get action=\"/invite\"><p><label>This site's address on the mesh<br> <input name=url size=40 value=\"{}\" placeholder=\"http://n0call-pi.local.mesh:8790\"></label> <button type=submit>Make invite</button></p></form> <p class=\"mute small\">Set <code>public_url</code> in the config to skip this step.</p>",
            esc(value)
        );
    };
    let Some(url) = url else {
        b.push_str("<p>The invite tells the new site where to reach this one.</p>");
        form(&mut b, "");
        return Ok(page(app, "Invite", "/invite", &b, false).into_response());
    };
    let line = match crate::trust::make_invite(&app.log, &url) {
        Ok(l) => l,
        Err(e) => {
            let _ = write!(b, "<p class=\"banner\">{}</p>", esc(&format!("{e:#}")));
            form(&mut b, &url);
            return Ok((
                StatusCode::BAD_REQUEST,
                page(app, "Invite", "/invite", &b, false),
            )
                .into_response());
        }
    };
    let others = app.log.trusted().len().saturating_sub(1);
    let _ = write!(
        b,
        "<p>Scan this on the new site's phone, or copy the line under it. On the new site run <code>ridgewarden join &lt;line&gt;</code>. Accepting it means the new site trusts {} to say who is on the trust list, trusts the {others} other station(s) this site trusts until the trust list arrives, and syncs with {}.</p>",
        esc(&app.log.id.author.call),
        esc(&url)
    );
    match crate::qr::encode(line.as_bytes()) {
        Ok(q) => {
            let _ = write!(
                b,
                "<div style=\"max-width:420px\">{}</div>",
                q.to_svg("ridgewarden invite")
            );
        }
        Err(e) => {
            let _ = write!(
                b,
                "<p class=\"banner\">No QR code: {}. Copy the line instead.</p>",
                esc(&format!("{e:#}"))
            );
        }
    }
    let _ = write!(
        b,
        "<p class=small><code style=\"overflow-wrap:anywhere\">{}</code></p>",
        esc(&line)
    );
    form(&mut b, &url);
    Ok(page(app, "Invite", "/invite", &b, false).into_response())
}

#[derive(Deserialize)]
struct ChangesQ {
    hours: Option<i64>,
}

async fn changes_page(State(c): S, Query(q): Query<ChangesQ>) -> R<Html<String>> {
    let hours = q.hours.unwrap_or(24).clamp(1, 24 * 7);
    let ch = diff::changes(&c.app.log.store.conn(), now_ms(), hours)?;
    let mut b = format!("<h1>What changed in the last {hours} hours</h1>");
    b.push_str("<p class=\"small noprint\"><a href=\"?hours=1\">1 h</a> &middot; <a href=\"?hours=24\">24 h</a> &middot; <a href=\"?hours=168\">7 days</a></p>");
    match ch {
        Some(ch) if !ch.lines.is_empty() => {
            let _ = write!(
                b,
                "<p class=mute>As seen from {}: {} compared with {}.</p><ul class=plain>",
                esc(&ch.node),
                short_time(ch.to_ts),
                short_time(ch.from_ts)
            );
            for l in &ch.lines {
                let _ = write!(b, "<li>{}</li>", esc(l));
            }
            b.push_str("</ul>");
        }
        Some(_) => b.push_str("<p>No changes.</p>"),
        None => b.push_str(
            "<p class=mute>Not enough history yet; the agent needs at least two polls.</p>",
        ),
    }
    Ok(page(&c.app, "Changes", "/changes", &b, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_kinds() {
        assert!(reserved("identity.bind") && reserved("identity.revoke") && reserved("node.poll"));
        assert!(reserved("trust.list") && reserved("log.checkpoint") && reserved("blob.manifest"));
        assert!(!reserved("svc.probe") && !reserved("status.banner"));
    }

    #[test]
    fn export_names_stay_in_the_folder() {
        assert!(plain_name("wikipedia_en_mini.zim"));
        for bad in ["", "../identity.key", "a/b", ".hidden", "x\\y", "con:"] {
            assert!(!plain_name(bad), "{bad}");
        }
    }

    #[test]
    fn token_check() {
        let mut h = HeaderMap::new();
        assert!(!token_ok("abc", &h));
        h.insert("authorization", "Bearer abd".parse().unwrap());
        assert!(!token_ok("abc", &h));
        h.insert("authorization", "Bearer abc".parse().unwrap());
        assert!(token_ok("abc", &h));
    }
}
