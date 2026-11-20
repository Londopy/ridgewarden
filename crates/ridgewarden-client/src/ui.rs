//! Shared page layout and status colors, so every app's pages look alike and work the
//! same way: server-rendered, no JavaScript, nothing fetched from the internet.

use std::fmt::Write as _;

use crate::util::esc;

pub const CSS: &str = r#"
:root{--bg:#fff;--fg:#1a1a1a;--mute:#666;--line:#ddd;--card:#f6f6f4;
--green:#1e7b34;--green-bg:#e3f4e6;--yellow:#8a5a00;--yellow-bg:#fff2cc;--red:#b3261e;--red-bg:#fde4e1;--unknown:#555;--unknown-bg:#ececec;--link:#1558a6;--visited:#3e5f86}
@media (prefers-color-scheme:dark){:root{--bg:#141414;--fg:#e8e8e8;--mute:#9a9a9a;--line:#333;--card:#1e1e1e;
--green:#7fd38f;--green-bg:#18301d;--yellow:#f0c75e;--yellow-bg:#352b10;--red:#ff8a80;--red-bg:#3a1714;--unknown:#bbb;--unknown-bg:#2a2a2a;--link:#8ec5ff;--visited:#b4c8e8}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.45 system-ui,-apple-system,"Segoe UI",Roboto,sans-serif}
main{max-width:1000px;margin:0 auto;padding:16px}
a{color:var(--link)}a:visited{color:var(--visited)}
:focus-visible{outline:2px solid var(--link);outline-offset:2px}
nav{border-bottom:1px solid var(--line);padding:8px 16px;display:flex;flex-wrap:wrap;gap:4px 16px;align-items:baseline}
nav b{margin-right:8px}nav a,nav a:visited{color:var(--fg)}nav a.on{font-weight:700;text-decoration:none}
h1{font-size:1.5em;margin:.2em 0 .4em}h2{font-size:1.15em;margin:1.4em 0 .5em}
.mute{color:var(--mute)}.small{font-size:.875em}
table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:6px 8px;border-bottom:1px solid var(--line);vertical-align:top}
th{font-size:.85em;color:var(--mute);font-weight:600}
.wrap{overflow-x:auto}
.pill{display:inline-block;padding:1px 8px;border-radius:10px;font-size:.8em;font-weight:700;white-space:nowrap}
.green{color:var(--green);background:var(--green-bg)}.yellow{color:var(--yellow);background:var(--yellow-bg)}
.red{color:var(--red);background:var(--red-bg)}.unknown{color:var(--unknown);background:var(--unknown-bg)}
.headline{padding:14px 16px;border-radius:8px;font-size:1.15em;font-weight:600;margin:8px 0 16px}
.banner{border:2px solid var(--yellow);background:var(--yellow-bg);color:var(--fg);padding:12px 16px;border-radius:8px;margin:8px 0}
.tiles{display:flex;flex-wrap:wrap;gap:10px}
.tile{flex:1 1 220px;border-radius:8px;padding:10px 12px;border-left:6px solid}
.tile.green{border-color:var(--green)}.tile.yellow{border-color:var(--yellow)}.tile.red{border-color:var(--red)}.tile.unknown{border-color:var(--unknown)}
.tile .name{font-weight:700;color:var(--fg)}.tile .why{color:var(--fg);font-size:.9em}
.cards{display:flex;flex-wrap:wrap;gap:10px}.card{flex:1 1 150px;background:var(--card);border-radius:8px;padding:10px 12px}
.card .n{font-size:1.6em;font-weight:700}
ul.plain{padding-left:1.1em}
svg.chart{width:100%;height:auto;display:block;margin:4px 0 12px}svg.strip{width:100%;height:20px;display:block}
.chart .frame{fill:none;stroke:var(--line)}.chart .axis,.chart .key{fill:var(--mute);font-size:11px}
.chart path{fill:none;stroke-width:1.6}.chart .s0{stroke:var(--fg)}.chart .s1{stroke:#2a7ab8}.chart .s2{stroke:#c26a1b}
.chart .k0{fill:var(--fg)}.chart .k1{fill:#2a7ab8}.chart .k2{fill:#c26a1b}.chart .d0{fill:var(--fg)}.chart .d1{fill:#2a7ab8}.chart .d2{fill:#c26a1b}
.chart .rule{stroke:var(--red);stroke-dasharray:4 3}.chart .marker{stroke:var(--yellow);stroke-width:1.5}
.strip .bgreen{fill:var(--green)}.strip .byellow{fill:var(--yellow)}.strip .bred{fill:var(--red)}.strip .bunknown{fill:var(--line)}
@media print{nav,.noprint{display:none}body{font-size:12pt;background:#fff;color:#000}main{max-width:none;padding:0}
.tile,.headline,.pill{border:1px solid #000!important;background:#fff!important;color:#000!important}
a,a:visited{color:#000;text-decoration:none}}
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Color {
    Red,
    Yellow,
    Unknown,
    Green,
}

impl Color {
    pub fn word(self) -> &'static str {
        match self {
            Color::Green => "OK",
            Color::Yellow => "DEGRADED",
            Color::Red => "DOWN",
            Color::Unknown => "UNKNOWN",
        }
    }

    pub fn css(self) -> &'static str {
        match self {
            Color::Green => "green",
            Color::Yellow => "yellow",
            Color::Red => "red",
            Color::Unknown => "unknown",
        }
    }
}

/// A colored label. The text always carries the meaning; color is only a second cue.
pub fn pill(c: Color, text: &str) -> String {
    format!("<span class=\"pill {}\">{}</span>", c.css(), esc(text))
}

pub struct Page<'a> {
    /// shown in the nav bar, e.g. "ridgewarden-health"
    pub app: &'a str,
    /// (href, label) pairs for the nav bar
    pub nav: &'a [(&'a str, &'a str)],
    pub active: &'a str,
    /// right-hand nav text, e.g. the node and callsign
    pub who: &'a str,
    /// reload every minute (for pages people leave open)
    pub refresh: bool,
}

impl Page<'_> {
    pub fn render(&self, title: &str, body: &str) -> String {
        let mut h = String::with_capacity(body.len() + 2500);
        h.push_str("<!doctype html><html lang=en><head><meta charset=utf-8>");
        h.push_str("<meta name=viewport content=\"width=device-width,initial-scale=1\">");
        if self.refresh {
            h.push_str("<meta http-equiv=refresh content=60>");
        }
        let _ = write!(
            h,
            "<title>{} - {}</title><style>{CSS}</style></head><body><nav><b>{}</b>",
            esc(title),
            esc(self.app),
            esc(self.app)
        );
        for (href, name) in self.nav {
            let on = if *href == self.active {
                " class=on"
            } else {
                ""
            };
            let _ = write!(h, "<a href=\"{}\"{on}>{}</a>", esc(href), esc(name));
        }
        let _ = write!(
            h,
            "<span class=\"mute small\">{}</span></nav><main>{body}</main></body></html>",
            esc(self.who)
        );
        h
    }
}
