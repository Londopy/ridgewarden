//! A terminal view for operators on a headless Pi over SSH, shared by the core's `top` and
//! the apps (the `tui` feature): a screen guard, an event loop, the suite's status colors, a
//! frame with tabs and a help line, placeholder states, tables with state pills, a loader
//! that keeps slow reads off the screen's thread, and a JSON fetch helper.
//!
//! Text from the mesh needs no escaping here: ratatui drops control characters, so nothing a
//! station writes can drive the terminal.

use std::ffi::OsStr;
use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color as Ansi, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde::de::DeserializeOwned;

pub use ratatui;

use crate::ui::Color;

/// The terminal in raw mode on the alternate screen. Dropping it puts the terminal back, and
/// so does a panic on any thread. Ctrl-C is a key press in raw mode, and `run` quits on it.
pub struct Screen {
    term: DefaultTerminal,
}

/// Set when any thread panics while a `Screen` is open. The terminal is back to normal by
/// then, so `run` must not draw again.
static PANICKED: AtomicBool = AtomicBool::new(false);

/// Note panics in `PANICKED`, then let the hook before (ratatui's, which puts the terminal
/// back) do the rest.
fn watch_panics() {
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PANICKED.store(true, Ordering::SeqCst);
        before(info);
    }));
}

impl Screen {
    /// Take over the terminal; fails when stdout isn't one.
    pub fn open() -> io::Result<Self> {
        if !io::stdout().is_terminal() {
            return Err(io::Error::other("stdout is not a terminal"));
        }
        ratatui::try_init()
            .map(|term| {
                watch_panics();
                Self { term }
            })
            .inspect_err(|_| {
                // raw mode is already on if only the alternate screen failed
                ratatui::restore();
            })
    }

    pub fn draw(&mut self, f: impl FnOnce(&mut Frame)) -> io::Result<()> {
        self.term.draw(f).map(drop)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// A screen that `run` drives: tick, draw, then wait for a key or the next tick.
pub trait View {
    /// Pick up new data, e.g. from a `Refresh`. An error ends `run`.
    fn tick(&mut self) -> io::Result<()>;
    /// Draw the whole screen.
    fn draw(&mut self, frame: &mut Frame);
    /// A key press other than the quit keys (see `quits`). Return false to quit as well.
    fn key(&mut self, key: KeyEvent) -> bool;
}

/// Take over the terminal and run `view` until the user quits, ticking every `tick`. The
/// terminal is put back however this ends.
pub fn run(view: &mut impl View, tick: Duration) -> io::Result<()> {
    let mut screen = Screen::open()?;
    let mut due = Instant::now();
    loop {
        if Instant::now() >= due {
            view.tick()?;
            due = Instant::now() + tick;
        }
        if PANICKED.load(Ordering::SeqCst) {
            return Err(io::Error::other("a thread panicked (see above)"));
        }
        screen.draw(|f| view.draw(f))?;
        // anything else, a resize included, just leads to the next draw
        if event::poll(due.saturating_duration_since(Instant::now()))?
            && let Event::Key(k) = event::read()?
            && k.kind != KeyEventKind::Release
            && (quits(&k) || !view.key(k))
        {
            return Ok(());
        }
    }
}

/// q and Ctrl-C quit every view.
pub fn quits(k: &KeyEvent) -> bool {
    match k.code {
        KeyCode::Char('c' | 'C') => k.modifiers.contains(KeyModifiers::CONTROL),
        KeyCode::Char('q' | 'Q') => true,
        _ => false,
    }
}

/// Colors for terminal views. The pills are the web pages' green, yellow, red and gray, each
/// with its own background so it reads the same on light and dark terminals; everything else
/// keeps the terminal's colors and uses bold, dim and reverse video.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// false when NO_COLOR is set
    pub color: bool,
}

impl Theme {
    /// Colors unless NO_COLOR is set to anything but an empty string (see no-color.org).
    pub fn from_env() -> Self {
        Self::new(std::env::var_os("NO_COLOR").as_deref())
    }

    fn new(no_color: Option<&OsStr>) -> Self {
        Self {
            color: no_color.is_none_or(OsStr::is_empty),
        }
    }

    /// A pill in this color, the terminal's version of `ui::pill`. Without color all pills
    /// look alike, so their words carry the meaning.
    pub fn pill(&self, c: Color) -> Style {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        if !self.color {
            return bold.add_modifier(Modifier::REVERSED);
        }
        let (fg, bg) = match c {
            Color::Green => (Ansi::Black, Ansi::Green),
            Color::Yellow => (Ansi::Black, Ansi::Yellow),
            Color::Red => (Ansi::White, Ansi::Red),
            Color::Unknown => (Ansi::Black, Ansi::Gray),
        };
        bold.fg(fg).bg(bg)
    }

    /// Titles, headings and table headers.
    pub fn heading(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }

    /// Secondary text: hints, the help line, times.
    pub fn mute(&self) -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    /// The selected tab or row.
    pub fn selected(&self) -> Style {
        Style::new().add_modifier(Modifier::REVERSED)
    }

    /// Text that reports a problem.
    pub fn bad(&self) -> Style {
        let s = Style::new().add_modifier(Modifier::BOLD);
        if self.color { s.fg(Ansi::Red) } else { s }
    }
}

/// A row's state, in the words of the suite's pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pill {
    Ok,
    /// working, with something missing or still under way
    Partial,
    Down,
    /// the last success is too old to say anything about now
    Stale,
}

impl Pill {
    pub fn word(self) -> &'static str {
        match self {
            Pill::Ok => "OK",
            Pill::Partial => "PARTIAL",
            Pill::Down => "DOWN",
            Pill::Stale => "STALE",
        }
    }

    pub fn color(self) -> Color {
        match self {
            Pill::Ok => Color::Green,
            Pill::Partial => Color::Yellow,
            Pill::Down => Color::Red,
            Pill::Stale => Color::Unknown,
        }
    }

    /// Down if the last try failed; OK if the last success is within three of the gaps
    /// expected between tries, stale if older; None before anything has been tried.
    pub fn freshness(failed: bool, last_ok: Option<i64>, now: i64, gap_ms: i64) -> Option<Pill> {
        match (failed, last_ok) {
            (true, _) => Some(Pill::Down),
            (false, Some(t)) if now - t <= 3 * gap_ms => Some(Pill::Ok),
            (false, Some(_)) => Some(Pill::Stale),
            (false, None) => None,
        }
    }
}

/// A pill as text: its word with a space either side, in its color.
pub fn pill(theme: &Theme, p: Pill) -> Span<'static> {
    Span::styled(format!(" {} ", p.word()), theme.pill(p.color()))
}

/// Columns the pill column takes: " PARTIAL ".
const PILL_WIDTH: u16 = 9;

/// A table whose first column holds each row's pill (blank for a row with no state), the
/// selected row in reverse video. `header` and `widths` are for the columns after the pill;
/// an empty header leaves the header row out. Render it with a `TableState` kept between
/// frames, so the view doesn't jump as the selection moves.
pub fn pill_table<'a>(
    theme: &Theme,
    header: &[&'a str],
    widths: &[Constraint],
    rows: impl IntoIterator<Item = (Option<Pill>, Vec<Cell<'a>>)>,
) -> Table<'a> {
    let rows = rows.into_iter().map(|(p, cells)| {
        // not Cell::default(): it spans no columns, so the row's other cells would shift left
        let first = p.map_or_else(|| Cell::from(""), |p| Cell::from(pill(theme, p)));
        Row::new(std::iter::once(first).chain(cells))
    });
    let widths = std::iter::once(Constraint::Length(PILL_WIDTH)).chain(widths.iter().copied());
    let table = Table::new(rows, widths).row_highlight_style(theme.selected());
    if header.is_empty() {
        return table;
    }
    table.header(
        Row::new(std::iter::once("State").chain(header.iter().copied())).style(theme.heading()),
    )
}

/// The tab a key picks: 1 to 9 directly, Tab and Right the next one, Shift-Tab and Left the
/// one before, wrapping around. None for any other key.
pub fn tab_key(k: &KeyEvent, at: usize, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    match k.code {
        KeyCode::Char(c @ '1'..='9') => {
            let i = c as usize - '1' as usize;
            (i < n).then_some(i)
        }
        KeyCode::Tab | KeyCode::Right => Some((at + 1) % n),
        KeyCode::BackTab | KeyCode::Left => Some((at + n - 1) % n),
        _ => None,
    }
}

/// Move a table's selection for a key: arrows or j/k by one row, PgUp/PgDn by `page` rows,
/// Home/End or g/G to either end, always within the `len` rows there are now. Returns false
/// for any other key.
pub fn move_key(state: &mut TableState, k: &KeyEvent, len: usize, page: usize) -> bool {
    let last = len.saturating_sub(1);
    let page = page.max(1);
    let at = state.selected().map(|s| s.min(last));
    let to = match k.code {
        KeyCode::Down | KeyCode::Char('j') => at.map_or(0, |s| s + 1),
        KeyCode::Up | KeyCode::Char('k') => at.map_or(0, |s| s.saturating_sub(1)),
        KeyCode::PageDown => at.map_or(page - 1, |s| s + page),
        KeyCode::PageUp => at.map_or(0, |s| s.saturating_sub(page)),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => last,
        _ => return false,
    };
    state.select((len > 0).then_some(to.min(last)));
    true
}

/// What every page of a view shares: a title line with a status on its right, a row of tabs,
/// and a help line at the bottom.
pub struct Chrome<'a> {
    pub title: &'a str,
    /// e.g. "updated 2 s ago", styled by the view
    pub status: Span<'a>,
    pub tabs: &'a [&'a str],
    pub tab: usize,
    /// e.g. "q quit  1-6 pages  arrows move"
    pub help: &'a str,
}

/// Below this many columns or rows a view only says how big to make the terminal.
pub const MIN_SIZE: (u16, u16) = (40, 10);

impl Chrome<'_> {
    /// Draw the frame and return the area left for the page. When the terminal is too small
    /// to be any use, say how big it needs to be instead and return None.
    pub fn draw(&self, f: &mut Frame, theme: &Theme) -> Option<Rect> {
        let area = f.area();
        if area.width < MIN_SIZE.0 || area.height < MIN_SIZE.1 {
            let msg = format!(
                "Make the terminal at least {}x{} (it is {}x{}).",
                MIN_SIZE.0, MIN_SIZE.1, area.width, area.height
            );
            f.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }), area);
            return None;
        }
        let [top, tabs, body, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let [title, status] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(self.status.width() as u16),
        ])
        .spacing(1)
        .areas(top);
        f.render_widget(Span::styled(self.title, theme.heading()), title);
        f.render_widget(self.status.clone(), status);
        let labels = tab_labels(self.tabs, tabs.width);
        f.render_widget(
            Tabs::new(labels)
                .select(self.tab)
                .highlight_style(theme.selected())
                .padding("", "")
                .divider(""),
            tabs,
        );
        f.render_widget(Span::styled(self.help, theme.mute()), help);
        Some(body)
    }
}

/// Tab labels with their number keys, the names cut short if that's what it takes to fit
/// `width` columns.
fn tab_labels(tabs: &[&str], width: u16) -> Vec<String> {
    let labels = |n: usize| -> Vec<String> {
        tabs.iter()
            .enumerate()
            .map(|(i, t)| match t.chars().take(n).collect::<String>() {
                name if name.is_empty() => format!(" {} ", i + 1),
                name => format!(" {} {name} ", i + 1),
            })
            .collect()
    };
    let longest = tabs.iter().map(|t| t.chars().count()).max().unwrap_or(0);
    (1..=longest)
        .rev()
        .map(labels)
        .find(|l| l.iter().map(|s| s.chars().count()).sum::<usize>() <= width as usize)
        .unwrap_or_else(|| labels(0))
}

/// What a page shows in place of its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice<'a> {
    /// waiting for the first data
    Loading,
    /// nothing to list; says why, or what would make something appear
    Empty(&'a str),
    /// the data couldn't be read; says what went wrong
    Error(&'a str),
}

impl<'a> Notice<'a> {
    /// The notice as a paragraph that wraps to its area.
    pub fn paragraph(self, theme: &Theme) -> Paragraph<'a> {
        let line = match self {
            Notice::Loading => Line::styled("Loading...", theme.mute()),
            Notice::Empty(why) => Line::styled(why, theme.mute()),
            Notice::Error(what) => Line::from(vec![
                Span::styled(" ERROR ", theme.pill(Color::Red)),
                Span::raw(" "),
                Span::raw(what),
            ]),
        };
        Paragraph::new(line).wrap(Wrap { trim: true })
    }
}

/// Loads a view's data on a thread of its own, so a slow or hung source never freezes the
/// screen. The loader may be async (it runs on its own single-threaded runtime), e.g. for a
/// `Fetch`. Only the newest result waits to be picked up.
pub struct Refresh<T> {
    newest: Arc<Mutex<Option<T>>>,
    wake: mpsc::Sender<()>,
    loader: JoinHandle<()>,
}

impl<T: Send + 'static> Refresh<T> {
    /// Run `load` now, then every `every`, or sooner after `again`.
    pub fn spawn(
        every: Duration,
        mut load: impl AsyncFnMut() -> T + Send + 'static,
    ) -> io::Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let newest = Arc::new(Mutex::new(None));
        let slot = newest.clone();
        let (wake, woken) = mpsc::channel::<()>();
        let loader = std::thread::Builder::new()
            .name("refresh".into())
            .spawn(move || {
                loop {
                    let v = rt.block_on(load());
                    *slot.lock().unwrap() = Some(v);
                    // the view dropping its Refresh is what ends this thread
                    if let Err(RecvTimeoutError::Disconnected) = woken.recv_timeout(every) {
                        return;
                    }
                    // several presses of reload while loading mean one reload
                    while woken.try_recv().is_ok() {}
                }
            })?;
        Ok(Self {
            newest,
            wake,
            loader,
        })
    }

    /// The newest result since the last call, if there is one. Fails once the loader thread
    /// has died, which only a panic in the loader does.
    pub fn latest(&self) -> io::Result<Option<T>> {
        let v = self.newest.lock().unwrap().take();
        if v.is_none() && self.loader.is_finished() {
            return Err(io::Error::other("the loader thread stopped"));
        }
        Ok(v)
    }

    /// Load again now instead of at the next round.
    pub fn again(&self) {
        // a dead loader thread is reported by `latest`
        let _ = self.wake.send(());
    }
}

/// Reads JSON from an HTTP API on this machine (an app's own, or the core's), never through
/// a proxy, each request giving up after a timeout.
#[derive(Clone, Debug)]
pub struct Fetch {
    http: reqwest::Client,
}

impl Fetch {
    pub fn new(timeout: Duration) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(timeout)
                .build()?,
        })
    }

    /// GET `url` and parse its JSON. An error's message says what went wrong in a few words
    /// (timed out, could not connect, HTTP 503, unexpected JSON); its source has the details.
    pub async fn json<T: DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        let got = async {
            self.http
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .json::<T>()
                .await
        }
        .await;
        got.map_err(|e| {
            let why = reason(&e);
            anyhow::Error::new(e).context(format!("{url}: {why}"))
        })
    }
}

fn reason(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        "could not connect".into()
    } else if let Some(s) = e.status() {
        format!("HTTP {}", s.as_u16())
    } else if e.is_decode() {
        "unexpected JSON".into()
    } else {
        "request failed".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn q_and_ctrl_c_quit_and_nothing_else_does() {
        assert!(quits(&key(KeyCode::Char('q'))));
        assert!(quits(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!quits(&key(KeyCode::Char('c'))));
        assert!(!quits(&key(KeyCode::Esc)));
    }

    #[test]
    fn tabs_follow_their_numbers_and_wrap() {
        assert_eq!(tab_key(&key(KeyCode::Char('3')), 0, 6), Some(2));
        assert_eq!(tab_key(&key(KeyCode::Char('7')), 0, 6), None);
        assert_eq!(tab_key(&key(KeyCode::Tab), 5, 6), Some(0));
        assert_eq!(tab_key(&key(KeyCode::BackTab), 0, 6), Some(5));
        assert_eq!(tab_key(&key(KeyCode::Left), 2, 6), Some(1));
        assert_eq!(tab_key(&key(KeyCode::Char('x')), 2, 6), None);
        assert_eq!(tab_key(&key(KeyCode::Tab), 0, 0), None);
    }

    #[test]
    fn the_selection_stays_on_the_rows() {
        let mut s = TableState::default();
        let mut go = |code, len| {
            assert!(move_key(&mut s, &key(code), len, 4));
            s.selected()
        };
        assert_eq!(go(KeyCode::Down, 10), Some(0));
        assert_eq!(go(KeyCode::Up, 10), Some(0));
        assert_eq!(go(KeyCode::PageDown, 10), Some(4));
        assert_eq!(go(KeyCode::End, 10), Some(9));
        assert_eq!(go(KeyCode::Down, 10), Some(9));
        assert_eq!(go(KeyCode::PageUp, 10), Some(5));
        assert_eq!(go(KeyCode::Char('g'), 10), Some(0));
        // the list shrank under the selection
        assert_eq!(go(KeyCode::End, 10), Some(9));
        assert_eq!(go(KeyCode::Up, 3), Some(1));
        assert_eq!(go(KeyCode::Down, 0), None);
        assert!(!move_key(&mut s, &key(KeyCode::Char('x')), 10, 4));
        let mut fresh = TableState::default();
        assert!(move_key(&mut fresh, &key(KeyCode::PageDown), 10, 4));
        assert_eq!(
            fresh.selected(),
            Some(3),
            "a first PgDn shows the end of the first page"
        );
    }

    #[test]
    fn rows_without_a_pill_keep_their_columns() {
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;
        let rows = [
            (
                Some(Pill::Ok),
                vec![Cell::from("Agent"), Cell::from("running")],
            ),
            (None, vec![Cell::from("Peers"), Cell::from("none yet")]),
        ];
        let widths = [Constraint::Length(6), Constraint::Fill(1)];
        let area = Rect::new(0, 0, 40, 3);
        let mut buf = Buffer::empty(area);
        pill_table(&Theme::new(None), &["Name", "What"], &widths, rows).render(area, &mut buf);
        let line = |y| {
            let s: String = (0..40).map(|x| buf[(x, y)].symbol()).collect();
            s.trim_end().to_string()
        };
        assert_eq!(line(0), "State     Name   What");
        assert_eq!(line(1), " OK       Agent  running");
        assert_eq!(line(2), "          Peers  none yet");
    }

    #[test]
    fn tab_labels_shrink_to_fit() {
        let tabs = ["Overview", "Peers", "Log"];
        assert_eq!(
            tab_labels(&tabs, 80),
            [" 1 Overview ", " 2 Peers ", " 3 Log "]
        );
        let narrow = tab_labels(&tabs, 20);
        assert_eq!(narrow, [" 1 Ov ", " 2 Pe ", " 3 Lo "]);
        assert_eq!(tab_labels(&tabs, 5), [" 1 ", " 2 ", " 3 "]);
    }

    #[test]
    fn no_color_drops_colors_but_keeps_the_words() {
        assert!(Theme::new(None).color);
        assert!(Theme::new(Some(OsStr::new(""))).color);
        let plain = Theme::new(Some(OsStr::new("1")));
        assert!(!plain.color);
        let all = [Color::Green, Color::Yellow, Color::Red, Color::Unknown];
        for c in all {
            let s = plain.pill(c);
            assert_eq!((s.fg, s.bg), (None, None), "{c:?}");
        }
        assert_eq!(plain.bad().fg, None);
        let colored: Vec<Style> = all.iter().map(|c| Theme::new(None).pill(*c)).collect();
        for (i, a) in colored.iter().enumerate() {
            assert!(a.bg.is_some());
            assert!(
                colored[i + 1..].iter().all(|b| b != a),
                "each color differs"
            );
        }
        let words: Vec<&str> = [Pill::Ok, Pill::Partial, Pill::Down, Pill::Stale]
            .iter()
            .map(|p| p.word())
            .collect();
        assert_eq!(words, ["OK", "PARTIAL", "DOWN", "STALE"]);
    }

    #[test]
    fn freshness_goes_stale_after_three_gaps() {
        let min = 60_000;
        assert_eq!(Pill::freshness(false, None, 0, min), None);
        assert_eq!(Pill::freshness(true, Some(0), 1, min), Some(Pill::Down));
        assert_eq!(Pill::freshness(true, None, 1, min), Some(Pill::Down));
        assert_eq!(
            Pill::freshness(false, Some(0), 3 * min, min),
            Some(Pill::Ok)
        );
        assert_eq!(
            Pill::freshness(false, Some(0), 3 * min + 1, min),
            Some(Pill::Stale)
        );
    }

    fn wait<T: Send + 'static>(r: &Refresh<T>) -> T {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if let Some(v) = r.latest().unwrap() {
                return v;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the loader never answered");
    }

    #[test]
    fn refresh_loads_off_the_screen_thread_and_again_on_demand() {
        let mut n = 0;
        let r = Refresh::spawn(Duration::from_secs(3600), async move || {
            n += 1;
            n
        })
        .unwrap();
        assert_eq!(wait(&r), 1);
        r.again();
        assert_eq!(wait(&r), 2);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            r.latest().unwrap(),
            None,
            "nothing more until the hour is up"
        );
    }

    #[test]
    fn a_panic_on_another_thread_stops_the_drawing() {
        watch_panics();
        std::thread::spawn(|| panic!("a loader bug"))
            .join()
            .unwrap_err();
        assert!(PANICKED.load(Ordering::SeqCst));
    }

    #[test]
    fn a_dead_loader_is_reported() {
        let r: Refresh<u8> =
            Refresh::spawn(Duration::from_secs(3600), async || panic!("loader bug")).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while r.latest().is_ok() {
            assert!(Instant::now() < until, "the dead loader was never noticed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A one-request-at-a-time HTTP server that answers every request with `answer`, or
    /// never answers when it's None.
    fn serve(answer: Option<String>) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/x", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for s in l.incoming() {
                let mut s = s.unwrap();
                let _ = s.read(&mut [0u8; 4096]);
                match &answer {
                    Some(a) => s.write_all(a.as_bytes()).unwrap(),
                    None => std::thread::sleep(Duration::from_secs(30)),
                }
            }
        });
        url
    }

    fn reply(status: &str, body: &str) -> Option<String> {
        Some(format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ))
    }

    #[tokio::test]
    async fn fetch_reads_json_and_says_what_went_wrong() {
        #[derive(serde::Deserialize)]
        struct Answer {
            n: u32,
        }
        let f = Fetch::new(Duration::from_millis(500)).unwrap();
        let ok: Answer = f.json(&serve(reply("200 OK", r#"{"n":7}"#))).await.unwrap();
        assert_eq!(ok.n, 7);
        let err = |r: anyhow::Result<Answer>| r.err().unwrap().to_string();
        let failing = serve(reply("503 Service Unavailable", ""));
        assert_eq!(err(f.json(&failing).await), format!("{failing}: HTTP 503"));
        let garbled = serve(reply("200 OK", r#"{"m":1}"#));
        assert_eq!(
            err(f.json(&garbled).await),
            format!("{garbled}: unexpected JSON")
        );
        let silent = serve(None);
        assert_eq!(err(f.json(&silent).await), format!("{silent}: timed out"));
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/v1/x", l.local_addr().unwrap())
        };
        // Windows takes a couple of seconds to give up on a closed port
        let patient = Fetch::new(Duration::from_secs(20)).unwrap();
        assert_eq!(
            err(patient.json(&closed).await),
            format!("{closed}: could not connect")
        );
    }
}
