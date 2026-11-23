//! Alerts for apps: each condition opens once, is repeated while nobody has acknowledged
//! it, and sends a "resolved" notice when it clears. Delivered to ntfy, Discord and plain
//! webhooks. Quiet hours hold back everything except critical alerts until they end.
//! Maintenance windows suppress alerts for what they cover.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::util::{now_ms, rfc3339_to_ms};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

/// A condition that is true right now. `key` must be stable for as long as the condition
/// lasts (e.g. "svc-down:http://node:8080/"), so it opens once rather than every round.
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    pub key: String,
    pub severity: Severity,
    pub title: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct AlertCfg {
    /// e.g. "https://ntfy.sh/my-mesh-alerts" or a self-hosted "http://10.1.2.3/mesh"
    #[serde(default)]
    pub ntfy: Option<String>,
    #[serde(default)]
    pub discord: Option<String>,
    /// plain JSON POSTs, one per notice
    #[serde(default)]
    pub webhooks: Vec<String>,
    /// mail through the box's own mail setup (sendmail, Postfix, msmtp...)
    #[serde(default)]
    pub email: Option<EmailCfg>,
    /// UTC hours when only critical alerts go out, e.g. "22-6"; empty means never quiet
    #[serde(default)]
    pub quiet_hours: String,
    /// repeat an unacknowledged alert this often; 0 means never
    #[serde(default)]
    pub repeat_hours: u64,
    /// this app's address, for acknowledge links, e.g. "http://n0call-pi.local.mesh:8791"
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default)]
    pub maintenance: Vec<Maintenance>,
    #[serde(default)]
    pub silence: Vec<Silence>,
}

/// Known and accepted: no alerts for what it covers until the review date, so a silence
/// can't quietly last forever.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Silence {
    pub matches: String,
    pub reason: String,
    /// "2027-01-31": alerts come back after this day (UTC)
    pub review: String,
}

impl Silence {
    fn ends(&self) -> Option<i64> {
        rfc3339_to_ms(&format!("{}T00:00:00Z", self.review)).map(|t| t + 86_400_000)
    }

    pub fn active(&self, now: i64) -> bool {
        self.ends().is_some_and(|e| now < e)
    }

    pub fn covers(&self, text: &str) -> bool {
        covers(&self.matches, text)
    }
}

fn covers(matches: &str, text: &str) -> bool {
    !matches.is_empty()
        && text
            .to_ascii_lowercase()
            .contains(&matches.to_ascii_lowercase())
}

/// Mail is handed to a local sendmail-compatible program, so whatever the box already
/// uses to send mail (a smarthost, msmtp, Postfix) works, and no password lives here.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EmailCfg {
    pub to: Vec<String>,
    pub from: String,
    #[serde(default = "d_sendmail")]
    pub sendmail: String,
}

fn d_sendmail() -> String {
    "/usr/sbin/sendmail".into()
}

fn plain_address(a: &str) -> bool {
    let a = a.trim();
    a.contains('@')
        && !a.contains(|c: char| c.is_whitespace() || c.is_control() || "<>,;\"".contains(c))
}

/// The whole message for `sendmail -t`. Mesh text never reaches a header raw: titles lose
/// line breaks, so nobody can add a Bcc by naming a service cleverly.
pub fn email_message(
    e: &EmailCfg,
    app: &str,
    title: &str,
    text: &str,
    ack: Option<&str>,
) -> String {
    let subject: String = format!("{app}: {title}")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect();
    let mut m = format!(
        "From: {}\r\nTo: {}\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n",
        e.from.trim(),
        e.to.iter().map(|t| t.trim()).collect::<Vec<_>>().join(", ")
    );
    for line in text.lines() {
        // a lone "." would end the message for some senders
        m.push_str(if line == "." { ".." } else { line });
        m.push_str("\r\n");
    }
    if let Some(a) = ack {
        m.push_str(&format!("\r\nAcknowledge: {a}\r\n"));
    }
    m
}

/// Hand a whole message (headers and body, as from `email_message`) to the configured
/// sendmail. Apps use it for mail that isn't an alert, like an SMS gateway's one-liner.
pub async fn send_email(e: &EmailCfg, message: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(&e.sendmail)
        .args(["-t", "-oi"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", e.sendmail))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(message.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "{} failed ({}): {}",
            e.sendmail,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// A planned outage: alerts whose key or title contains `matches` are held while it runs.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Maintenance {
    pub matches: String,
    /// RFC 3339, e.g. "2026-11-22T15:00:00Z"
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub note: String,
}

impl Maintenance {
    pub fn active(&self, now: i64) -> bool {
        match (rfc3339_to_ms(&self.from), rfc3339_to_ms(&self.to)) {
            (Some(a), Some(b)) => a <= now && now < b,
            _ => false,
        }
    }

    pub fn covers(&self, text: &str) -> bool {
        covers(&self.matches, text)
    }
}

impl AlertCfg {
    pub fn validate(&self) -> Result<()> {
        quiet(&self.quiet_hours)?;
        for m in &self.maintenance {
            match (rfc3339_to_ms(&m.from), rfc3339_to_ms(&m.to)) {
                (Some(a), Some(b)) if a < b => {}
                (Some(_), Some(_)) => {
                    bail!("maintenance for {:?} ends before it starts", m.matches)
                }
                _ => bail!("maintenance times must look like 2026-11-22T15:00:00Z"),
            }
        }
        if self.repeat_hours > 24 * 30 {
            bail!("repeat_hours can be at most 720 (30 days)");
        }
        for x in &self.silence {
            if x.ends().is_none() {
                bail!(
                    "silence review dates look like 2027-01-31, not {:?}",
                    x.review
                );
            }
            if x.reason.trim().is_empty() {
                bail!("silence for {:?} needs a reason", x.matches);
            }
        }
        for u in self.ntfy.iter().chain(&self.discord).chain(&self.webhooks) {
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                bail!("alert destination {u:?} is not an http(s) URL");
            }
        }
        if let Some(e) = &self.email {
            if e.to.is_empty() {
                bail!("[alerts.email] needs at least one address in `to`");
            }
            for a in e.to.iter().chain(std::iter::once(&e.from)) {
                if !plain_address(a) {
                    bail!("{a:?} is not a plain email address like ops@example.org");
                }
            }
        }
        Ok(())
    }

    /// Why alerts about `text` are held right now, if they are: a maintenance window or a
    /// silence that covers it.
    pub fn hold(&self, text: &str, now: i64) -> Option<String> {
        if let Some(m) = self
            .maintenance
            .iter()
            .find(|m| m.active(now) && m.covers(text))
        {
            let until = rfc3339_to_ms(&m.to)
                .map(crate::util::short_time)
                .unwrap_or_default();
            return Some(if m.note.is_empty() {
                format!("Maintenance until {until}")
            } else {
                format!("Maintenance until {until}: {}", m.note)
            });
        }
        self.silence
            .iter()
            .find(|x| x.active(now) && x.covers(text))
            .map(|x| format!("Silenced: {} (review by {})", x.reason, x.review))
    }

    /// Silences whose review date has passed, so pages can ask someone to look again.
    pub fn lapsed(&self, now: i64) -> impl Iterator<Item = &Silence> {
        self.silence.iter().filter(move |x| !x.active(now))
    }
}

fn quiet(spec: &str) -> Result<Option<(u32, u32)>> {
    if spec.trim().is_empty() {
        return Ok(None);
    }
    let (a, b) = spec
        .split_once('-')
        .context("quiet_hours looks like \"22-6\" (UTC hours)")?;
    let (a, b): (u32, u32) = (a.trim().parse()?, b.trim().parse()?);
    if a > 23 || b > 24 || a == b {
        bail!("quiet_hours must be two different hours from 0 to 24");
    }
    Ok(Some((a, b)))
}

fn is_quiet(spec: &str, now: i64) -> bool {
    let hour = (now / 3_600_000).rem_euclid(24) as u32;
    match quiet(spec).ok().flatten() {
        None => false,
        Some((a, b)) if a < b => (a..b).contains(&hour),
        Some((a, b)) => hour >= a || hour < b,
    }
}

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS alerts (
    key           TEXT PRIMARY KEY,
    severity      TEXT NOT NULL,
    title         TEXT NOT NULL,
    text          TEXT NOT NULL,
    opened_ms     INTEGER NOT NULL,
    last_sent_ms  INTEGER,
    acked_ms      INTEGER,
    resolved_ms   INTEGER,
    resolved_sent INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS alert_secret (
    id     INTEGER PRIMARY KEY CHECK (id = 1),
    secret TEXT NOT NULL
);";

/// An open alert as stored, for pages that list them.
#[derive(Clone, Debug)]
pub struct Open {
    pub key: String,
    pub severity: String,
    pub title: String,
    pub text: String,
    pub opened_ms: i64,
    pub acked: bool,
}

/// Record which conditions are true now: open new ones, resolve ones that cleared. Open
/// alerts for which `keep` is true stay open even when missing from `active`: the
/// producer couldn't judge them this round (no fresh data), which isn't the same as fixed.
pub fn update(
    conn: &Connection,
    active: &[Alert],
    keep: &(dyn Fn(&str) -> bool + Sync),
    now: i64,
) -> Result<()> {
    for a in active {
        let sev = serde_json::to_value(a.severity)?
            .as_str()
            .unwrap_or("warning")
            .to_string();
        conn.execute(
            "INSERT INTO alerts (key, severity, title, text, opened_ms) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET severity = excluded.severity, title = excluded.title,
                 text = excluded.text,
                 opened_ms = CASE WHEN resolved_ms IS NULL THEN opened_ms ELSE excluded.opened_ms END,
                 last_sent_ms = CASE WHEN resolved_ms IS NULL THEN last_sent_ms ELSE NULL END,
                 acked_ms = CASE WHEN resolved_ms IS NULL THEN acked_ms ELSE NULL END,
                 resolved_sent = 0, resolved_ms = NULL",
            params![a.key, sev, a.title, a.text, now],
        )?;
    }
    let keys: Vec<&str> = active.iter().map(|a| a.key.as_str()).collect();
    let open: Vec<String> = conn
        .prepare("SELECT key FROM alerts WHERE resolved_ms IS NULL")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for k in open
        .iter()
        .filter(|k| !keys.contains(&k.as_str()) && !keep(k))
    {
        conn.execute(
            "UPDATE alerts SET resolved_ms = ?2 WHERE key = ?1",
            params![k, now],
        )?;
    }
    // forget resolved alerts after a week
    conn.execute(
        "DELETE FROM alerts WHERE resolved_ms IS NOT NULL AND resolved_ms < ?1
         AND (resolved_sent = 1 OR last_sent_ms IS NULL)",
        params![now - 7 * 86_400_000],
    )?;
    Ok(())
}

pub fn open_alerts(conn: &Connection) -> Result<Vec<Open>> {
    let mut st = conn.prepare(
        "SELECT key, severity, title, text, opened_ms, acked_ms FROM alerts
         WHERE resolved_ms IS NULL
         ORDER BY CASE severity WHEN 'critical' THEN 2 WHEN 'warning' THEN 1 ELSE 0 END DESC, opened_ms",
    )?;
    let rows = st.query_map([], |r| {
        Ok(Open {
            key: r.get(0)?,
            severity: r.get(1)?,
            title: r.get(2)?,
            text: r.get(3)?,
            opened_ms: r.get(4)?,
            acked: r.get::<_, Option<i64>>(5)?.is_some(),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn secret(conn: &Connection) -> Result<String> {
    if let Some(s) = conn
        .query_row("SELECT secret FROM alert_secret WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()?
    {
        return Ok(s);
    }
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    let s = hex::encode(b);
    conn.execute(
        "INSERT OR IGNORE INTO alert_secret (id, secret) VALUES (1, ?1)",
        params![s],
    )?;
    secret(conn)
}

/// The token in an alert's acknowledge link: unguessable without this app's secret, so
/// nobody else on the mesh can silence an alert, and tied to when it opened, so an old
/// link can't acknowledge the next time the same thing breaks.
pub fn ack_token(conn: &Connection, key: &str, opened_ms: i64) -> Result<String> {
    let s = secret(conn)?;
    Ok(blake3::hash(format!("{s}:{key}:{opened_ms}").as_bytes()).to_hex()[..24].to_string())
}

/// Acknowledge the alert this token belongs to. Returns its title if one matched.
pub fn ack(conn: &Connection, token: &str, now: i64) -> Result<Option<String>> {
    let open = open_alerts(conn)?;
    for a in open {
        if ack_token(conn, &a.key, a.opened_ms)? == token {
            conn.execute(
                "UPDATE alerts SET acked_ms = ?2 WHERE key = ?1",
                params![a.key, now],
            )?;
            return Ok(Some(a.title));
        }
    }
    Ok(None)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub key: String,
    pub state: &'static str,
    pub severity: Severity,
    pub title: String,
    pub text: String,
    pub opened_ms: i64,
}

/// What should go out now: new alerts, repeats that are due, and resolutions.
pub fn due(conn: &Connection, cfg: &AlertCfg, now: i64) -> Result<Vec<Notice>> {
    let quiet_now = is_quiet(&cfg.quiet_hours, now);
    let mut out = vec![];
    let mut st = conn.prepare(
        "SELECT key, severity, title, text, last_sent_ms, acked_ms, resolved_ms, resolved_sent, opened_ms
         FROM alerts",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, bool>(7)?,
            r.get::<_, i64>(8)?,
        ))
    })?;
    for r in rows {
        let (key, sev, title, text, last_sent, acked, resolved, resolved_sent, opened_ms) = r?;
        let severity: Severity = serde_json::from_value(json!(sev)).unwrap_or(Severity::Warning);
        if quiet_now && severity != Severity::Critical {
            continue;
        }
        let held = cfg
            .hold(&key, now)
            .or_else(|| cfg.hold(&title, now))
            .is_some();
        let state = match (resolved, last_sent) {
            // only say "resolved" for something we said was wrong
            (Some(_), Some(_)) if !resolved_sent => "resolved",
            (Some(_), _) => continue,
            (None, None) if !held => "open",
            (None, Some(t))
                if !held
                    && acked.is_none()
                    && cfg.repeat_hours > 0
                    && now - t >= cfg.repeat_hours as i64 * 3_600_000 =>
            {
                "repeat"
            }
            _ => continue,
        };
        out.push(Notice {
            key,
            state,
            severity,
            title,
            text,
            opened_ms,
        });
    }
    Ok(out)
}

pub fn mark_sent(conn: &Connection, n: &Notice, now: i64) -> Result<()> {
    if n.state == "resolved" {
        conn.execute(
            "UPDATE alerts SET resolved_sent = 1 WHERE key = ?1",
            params![n.key],
        )?;
    } else {
        conn.execute(
            "UPDATE alerts SET last_sent_ms = ?2 WHERE key = ?1",
            params![n.key, now],
        )?;
    }
    Ok(())
}

fn ack_url(cfg: &AlertCfg, token: &str) -> Option<String> {
    cfg.public_url
        .as_ref()
        .map(|u| format!("{}/ack/{token}", u.trim_end_matches('/')))
}

/// Send one notice to every configured channel. One channel that works is enough to call
/// it sent: retrying all of them would repeat it on the ones that worked. Fails only if
/// every channel failed, so it is tried again next round.
pub async fn send(
    http: &reqwest::Client,
    cfg: &AlertCfg,
    app: &str,
    n: &Notice,
    token: &str,
) -> Result<()> {
    let prefix = match n.state {
        "resolved" => "Resolved: ",
        "repeat" => "Still: ",
        _ => "",
    };
    // titles come from the mesh; any control character makes the ntfy Title header
    // invalid, and the notice would then fail on every retry
    let title: String = format!("{prefix}{}", n.title)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let ack = (n.state != "resolved")
        .then(|| ack_url(cfg, token))
        .flatten();
    let mut tries: Vec<reqwest::RequestBuilder> = vec![];
    if let Some(url) = &cfg.ntfy {
        let priority = match (n.state, n.severity) {
            ("resolved", _) => "2",
            (_, Severity::Critical) => "5",
            (_, Severity::Warning) => "4",
            _ => "3",
        };
        let tags = if n.state == "resolved" {
            "white_check_mark"
        } else {
            "warning"
        };
        let mut req = http
            .post(url)
            .header("Title", format!("{app}: {title}"))
            .header("Priority", priority)
            .header("Tags", tags)
            .body(n.text.clone());
        if let Some(a) = &ack {
            req = req.header("Actions", format!("view, Acknowledge, {a}"));
        }
        tries.push(req);
    }
    if let Some(url) = &cfg.discord {
        let mut content = format!("**{app}: {title}**\n{}", n.text);
        if let Some(a) = &ack {
            content.push_str(&format!("\n[Acknowledge]({a})"));
        }
        // Discord's limit; no pings, since a service named "@everyone" could otherwise ping
        let content: String = content.chars().take(2000).collect();
        tries.push(
            http.post(url)
                .json(&json!({"content": content, "allowed_mentions": {"parse": []}})),
        );
    }
    for url in &cfg.webhooks {
        tries.push(http.post(url).json(&json!({
            "app": app, "key": n.key, "state": n.state, "severity": n.severity,
            "title": n.title, "text": n.text, "ack_url": ack,
        })));
    }
    let mut delivered = false;
    let mut last: Option<anyhow::Error> = None;
    if let Some(e) = &cfg.email {
        match send_email(e, &email_message(e, app, &title, &n.text, ack.as_deref())).await {
            Ok(()) => delivered = true,
            Err(err) => {
                tracing::warn!("alert {:?}: email failed: {err:#}", n.title);
                last = Some(err);
            }
        }
    }
    for req in tries {
        match req.send().await.and_then(|r| r.error_for_status()) {
            Ok(_) => delivered = true,
            // webhook URLs carry their secret, so they stay out of the log
            Err(e) => {
                let e = e.without_url();
                tracing::warn!("alert {:?}: a channel failed: {e}", n.title);
                last = Some(e.into());
            }
        }
    }
    match last {
        Some(e) if !delivered => Err(e),
        _ => Ok(()),
    }
}

/// The whole round: record what's true, then deliver what's due. Returns how many
/// notices went out.
pub async fn run(
    http: &reqwest::Client,
    db: &std::sync::Mutex<Connection>,
    cfg: &AlertCfg,
    app: &str,
    active: Vec<Alert>,
    keep: &(dyn Fn(&str) -> bool + Sync),
) -> Result<usize> {
    let now = now_ms();
    let (notices, tokens) = {
        let c = db.lock().unwrap_or_else(|e| e.into_inner());
        update(&c, &active, keep, now)?;
        let notices = due(&c, cfg, now)?;
        let tokens = notices
            .iter()
            .map(|n| ack_token(&c, &n.key, n.opened_ms))
            .collect::<Result<Vec<_>>>()?;
        (notices, tokens)
    };
    let nothing_configured = cfg.ntfy.is_none()
        && cfg.discord.is_none()
        && cfg.webhooks.is_empty()
        && cfg.email.is_none();
    let mut sent = 0;
    for (n, token) in notices.iter().zip(tokens) {
        if !nothing_configured {
            // a hung alert endpoint must not hold up the app's other work
            match tokio::time::timeout(Duration::from_secs(30), send(http, cfg, app, n, &token))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!("alert {:?} not delivered: {e:#}", n.title);
                    continue;
                }
                Err(_) => {
                    tracing::warn!("alert {:?} not delivered: timed out", n.title);
                    continue;
                }
            }
        }
        // with nowhere to send, notices are still marked so the page's open list is right
        mark_sent(&db.lock().unwrap_or_else(|e| e.into_inner()), n, now)?;
        if !nothing_configured {
            sent += 1;
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c
    }

    fn alert(key: &str, sev: Severity) -> Alert {
        Alert {
            key: key.into(),
            severity: sev,
            title: format!("{key} broke"),
            text: "details".into(),
        }
    }

    const H: i64 = 3_600_000;
    // a fixed time at 12:00 UTC
    const NOON: i64 = 1_800_000_000_000 - 1_800_000_000_000 % (24 * H) + 12 * H;

    #[test]
    fn opens_once_repeats_when_due_and_resolves() {
        let c = db();
        let cfg = AlertCfg {
            repeat_hours: 2,
            ..Default::default()
        };
        update(&c, &[alert("a", Severity::Warning)], &|_| false, NOON).unwrap();
        let n = due(&c, &cfg, NOON).unwrap();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].state, "open");
        mark_sent(&c, &n[0], NOON).unwrap();
        // still true an hour later: nothing new
        update(&c, &[alert("a", Severity::Warning)], &|_| false, NOON + H).unwrap();
        assert!(due(&c, &cfg, NOON + H).unwrap().is_empty());
        // two hours after sending: repeat
        assert_eq!(due(&c, &cfg, NOON + 2 * H).unwrap()[0].state, "repeat");
        // cleared: resolved, once
        update(&c, &[], &|_| false, NOON + 3 * H).unwrap();
        let n = due(&c, &cfg, NOON + 3 * H).unwrap();
        assert_eq!(n[0].state, "resolved");
        mark_sent(&c, &n[0], NOON + 3 * H).unwrap();
        assert!(due(&c, &cfg, NOON + 4 * H).unwrap().is_empty());
    }

    #[test]
    fn acknowledged_alerts_stop_repeating() {
        let c = db();
        let cfg = AlertCfg {
            repeat_hours: 1,
            ..Default::default()
        };
        update(&c, &[alert("a", Severity::Warning)], &|_| false, NOON).unwrap();
        let n = due(&c, &cfg, NOON).unwrap();
        mark_sent(&c, &n[0], NOON).unwrap();
        let token = ack_token(&c, "a", NOON).unwrap();
        assert!(ack(&c, "not-the-token", NOON).unwrap().is_none());
        assert_eq!(ack(&c, &token, NOON).unwrap().as_deref(), Some("a broke"));
        assert!(due(&c, &cfg, NOON + 5 * H).unwrap().is_empty());
    }

    #[test]
    fn quiet_hours_hold_all_but_critical() {
        let c = db();
        let cfg = AlertCfg {
            quiet_hours: "11-13".into(),
            ..Default::default()
        };
        update(
            &c,
            &[
                alert("w", Severity::Warning),
                alert("c", Severity::Critical),
            ],
            &|_| false,
            NOON,
        )
        .unwrap();
        let n = due(&c, &cfg, NOON).unwrap();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].key, "c");
        // after quiet hours the held warning goes out
        assert!(
            due(&c, &cfg, NOON + 2 * H)
                .unwrap()
                .iter()
                .any(|n| n.key == "w")
        );
    }

    #[test]
    fn maintenance_holds_matching_alerts() {
        let c = db();
        let cfg = AlertCfg {
            maintenance: vec![Maintenance {
                matches: "cuesta".into(),
                from: crate::util::ms_to_rfc3339(NOON - H),
                to: crate::util::ms_to_rfc3339(NOON + H),
                note: "tower work".into(),
            }],
            ..Default::default()
        };
        update(
            &c,
            &[
                alert("link:tst-cuesta|tst-lopez", Severity::Critical),
                alert("other", Severity::Warning),
            ],
            &|_| false,
            NOON,
        )
        .unwrap();
        let keys: Vec<String> = due(&c, &cfg, NOON)
            .unwrap()
            .into_iter()
            .map(|n| n.key)
            .collect();
        assert_eq!(keys, vec!["other".to_string()]);
        assert_eq!(due(&c, &cfg, NOON + 2 * H).unwrap().len(), 2, "window over");
    }

    #[test]
    fn silences_last_until_their_review_date() {
        let c = db();
        let review = crate::util::ms_to_rfc3339(NOON)[..10].to_string();
        let cfg = AlertCfg {
            silence: vec![Silence {
                matches: "camera".into(),
                reason: "owner knows".into(),
                review,
            }],
            ..Default::default()
        };
        cfg.validate().unwrap();
        update(
            &c,
            &[alert("svc:camera", Severity::Warning)],
            &|_| false,
            NOON,
        )
        .unwrap();
        assert!(due(&c, &cfg, NOON).unwrap().is_empty());
        assert!(cfg.hold("Camera 3", NOON).unwrap().contains("owner knows"));
        assert_eq!(cfg.lapsed(NOON).count(), 0);
        // the day after the review date the alert goes out
        assert_eq!(due(&c, &cfg, NOON + 13 * H).unwrap().len(), 1);
        assert_eq!(cfg.lapsed(NOON + 13 * H).count(), 1);
    }

    #[test]
    fn alerts_the_producer_cannot_judge_stay_open() {
        let c = db();
        let cfg = AlertCfg::default();
        update(
            &c,
            &[alert("svc x DOWN", Severity::Critical)],
            &|_| false,
            NOON,
        )
        .unwrap();
        let n = due(&c, &cfg, NOON).unwrap();
        mark_sent(&c, &n[0], NOON).unwrap();
        // no fresh data: not active, but not fixed either
        update(&c, &[], &|k| k.starts_with("svc x "), NOON + H).unwrap();
        assert!(due(&c, &cfg, NOON + H).unwrap().is_empty());
        assert_eq!(open_alerts(&c).unwrap().len(), 1);
        update(&c, &[], &|_| false, NOON + 2 * H).unwrap();
        assert_eq!(due(&c, &cfg, NOON + 2 * H).unwrap()[0].state, "resolved");
    }

    #[test]
    fn an_old_ack_link_does_not_ack_the_next_outage() {
        let c = db();
        let cfg = AlertCfg::default();
        update(&c, &[alert("a", Severity::Warning)], &|_| false, NOON).unwrap();
        let old = ack_token(&c, "a", NOON).unwrap();
        let n = due(&c, &cfg, NOON).unwrap();
        mark_sent(&c, &n[0], NOON).unwrap();
        update(&c, &[], &|_| false, NOON + H).unwrap();
        update(
            &c,
            &[alert("a", Severity::Warning)],
            &|_| false,
            NOON + 2 * H,
        )
        .unwrap();
        assert!(ack(&c, &old, NOON + 2 * H).unwrap().is_none());
    }

    #[test]
    fn criticals_list_first() {
        let c = db();
        update(
            &c,
            &[
                alert("w", Severity::Warning),
                alert("i", Severity::Info),
                alert("c", Severity::Critical),
            ],
            &|_| false,
            NOON,
        )
        .unwrap();
        let keys: Vec<String> = open_alerts(&c)
            .unwrap()
            .into_iter()
            .map(|a| a.key)
            .collect();
        assert_eq!(keys, ["c", "w", "i"]);
    }

    /// A one-request HTTP server that answers with `status`, and the URL to reach it.
    fn server(status: u16) -> String {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut buf = [0u8; 8192];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X
Content-Length: 0
Connection: close

"
                );
            }
        });
        url
    }

    #[tokio::test]
    async fn one_broken_channel_does_not_block_or_repeat_the_others() {
        let db = std::sync::Mutex::new(db());
        let http = reqwest::Client::new();
        let cfg = AlertCfg {
            ntfy: Some(server(200)),
            discord: Some(server(404)),
            ..Default::default()
        };
        let a = vec![alert("a", Severity::Warning)];
        assert_eq!(
            run(&http, &db, &cfg, "t", a.clone(), &|_| false)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            run(&http, &db, &cfg, "t", a, &|_| false).await.unwrap(),
            0,
            "sent once"
        );
        assert_eq!(
            run(&http, &db, &cfg, "t", vec![], &|_| false)
                .await
                .unwrap(),
            1,
            "resolved once"
        );
        assert_eq!(
            run(&http, &db, &cfg, "t", vec![], &|_| false)
                .await
                .unwrap(),
            0
        );
        let dead = AlertCfg {
            discord: Some(server(500)),
            ..Default::default()
        };
        let r = run(
            &http,
            &db,
            &dead,
            "t",
            vec![alert("b", Severity::Warning)],
            &|_| false,
        )
        .await;
        assert_eq!(
            r.unwrap(),
            0,
            "every channel failed, so it waits for next round"
        );
    }

    #[test]
    fn a_condition_that_returns_opens_again() {
        let c = db();
        let cfg = AlertCfg::default();
        update(&c, &[alert("a", Severity::Warning)], &|_| false, NOON).unwrap();
        let n = due(&c, &cfg, NOON).unwrap();
        mark_sent(&c, &n[0], NOON).unwrap();
        update(&c, &[], &|_| false, NOON + H).unwrap();
        let n = due(&c, &cfg, NOON + H).unwrap();
        mark_sent(&c, &n[0], NOON + H).unwrap();
        update(
            &c,
            &[alert("a", Severity::Warning)],
            &|_| false,
            NOON + 2 * H,
        )
        .unwrap();
        assert_eq!(due(&c, &cfg, NOON + 2 * H).unwrap()[0].state, "open");
    }

    #[test]
    fn email_keeps_mesh_text_out_of_the_headers() {
        let e = EmailCfg {
            to: vec!["ops@example.org".into()],
            from: "mesh@example.org".into(),
            sendmail: d_sendmail(),
        };
        let m = email_message(
            &e,
            "app",
            "Chat\r\nBcc: x@evil.example",
            "line one\n.\nline three",
            Some("http://a/ack/t"),
        );
        let (head, body) = m.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            head.lines().filter(|l| l.starts_with("Bcc")).count(),
            0,
            "{head}"
        );
        assert!(
            head.contains("Subject: app: Chat  Bcc: x@evil.example"),
            "{head}"
        );
        assert!(
            body.contains("\r\n..\r\n") && body.contains("Acknowledge: http://a/ack/t"),
            "{body}"
        );
        let bad = AlertCfg {
            email: Some(EmailCfg {
                to: vec!["a@b.org, c@d.org".into()],
                ..e.clone()
            }),
            ..Default::default()
        };
        assert!(bad.validate().is_err());
        assert!(
            AlertCfg {
                email: Some(e),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[tokio::test]
    async fn control_characters_in_a_title_dont_block_delivery() {
        let db = std::sync::Mutex::new(db());
        let http = reqwest::Client::new();
        let cfg = AlertCfg {
            ntfy: Some(server(200)),
            ..Default::default()
        };
        let a = Alert {
            key: "k".into(),
            severity: Severity::Warning,
            title: "Chat \u{1b}[31m\u{7}\0 down".into(),
            text: "x".into(),
        };
        assert_eq!(
            run(&http, &db, &cfg, "t", vec![a], &|_| false)
                .await
                .unwrap(),
            1
        );
    }

    #[test]
    fn config_is_checked() {
        assert!(
            AlertCfg {
                quiet_hours: "25-3".into(),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            AlertCfg {
                ntfy: Some("ftp://x".into()),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            AlertCfg {
                quiet_hours: "22-6".into(),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }
}
