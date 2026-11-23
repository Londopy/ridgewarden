//! Charts drawn as inline SVG on the server, so pages stay a single request with no
//! JavaScript. Colors come from the page CSS, so they follow light and dark mode.

use std::fmt::Write as _;

use crate::ui::Color;
use crate::util::{esc, short_time};

const W: f64 = 640.0;
const H: f64 = 170.0;
const MIN_LEFT: f64 = 46.0;
/// The margin never takes more than a third of the chart, however long a label is.
const MAX_LEFT: f64 = W / 3.0;
const RIGHT: f64 = 8.0;
const TOP: f64 = 18.0;
const BOTTOM: f64 = 22.0;

pub struct Line {
    pub label: String,
    /// (unix ms, value), in time order
    pub points: Vec<(i64, f64)>,
}

#[derive(Default)]
pub struct Chart {
    pub from: i64,
    pub to: i64,
    pub unit: String,
    /// at most three are styled apart; more are drawn alike
    pub lines: Vec<Line>,
    /// vertical lines at moments worth seeing, e.g. a firmware change; the text is the tooltip
    pub markers: Vec<(i64, String)>,
    /// dashed horizontal line, e.g. a threshold
    pub rule: Option<(f64, String)>,
    /// points further apart than this are not joined, so missing data shows as a gap
    pub gap_ms: i64,
}

fn nice(v: f64) -> String {
    if v.abs() >= 100.0 || v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

impl Chart {
    fn y_range(&self) -> Option<(f64, f64)> {
        let mut vals = self.lines.iter().flat_map(|l| l.points.iter().map(|p| p.1));
        let first = vals.next()?;
        let (mut lo, mut hi) = vals.fold((first, first), |(a, b), v| (a.min(v), b.max(v)));
        if let Some((r, _)) = self.rule {
            lo = lo.min(r);
            hi = hi.max(r);
        }
        if hi - lo < 1e-9 {
            lo -= 1.0;
            hi += 1.0;
        }
        let pad = (hi - lo) * 0.08;
        Some((lo - pad, hi + pad))
    }

    pub fn render(&self, title: &str) -> String {
        let mut s = String::new();
        let _ = write!(
            s,
            "<svg class=chart viewBox=\"0 0 {W} {H}\" role=img aria-label=\"{}\"><title>{}</title>",
            esc(title),
            esc(title)
        );
        let Some((lo, hi)) = self.y_range() else {
            let _ = write!(
                s,
                "<text x=\"{}\" y=\"{}\" class=axis text-anchor=middle>no data yet</text></svg>",
                W / 2.0,
                H / 2.0
            );
            return s;
        };
        let ticks: Vec<(f64, String)> = [
            hi - (hi - lo) * 0.08,
            (hi + lo) / 2.0,
            lo + (hi - lo) * 0.08,
        ]
        .into_iter()
        .map(|v| {
            let text = if self.unit.is_empty() {
                nice(v)
            } else {
                format!("{} {}", nice(v), self.unit)
            };
            (v, text)
        })
        .collect();
        // labels are right-aligned against the frame, so long ones need a wider margin
        let widest = ticks
            .iter()
            .map(|(_, t)| t.chars().count())
            .max()
            .unwrap_or(0);
        let left = (6.5 * widest as f64 + 8.0).clamp(MIN_LEFT, MAX_LEFT);
        let span = (self.to - self.from).max(1) as f64;
        let x = |t: i64| left + (t - self.from) as f64 / span * (W - left - RIGHT);
        let y = |v: f64| TOP + (hi - v) / (hi - lo) * (H - TOP - BOTTOM);

        let _ = write!(
            s,
            "<rect class=frame x=\"{left}\" y=\"{TOP}\" width=\"{}\" height=\"{}\"/>",
            W - left - RIGHT,
            H - TOP - BOTTOM
        );
        for (v, text) in &ticks {
            let _ = write!(
                s,
                "<text class=axis x=\"{}\" y=\"{:.1}\" text-anchor=end>{}</text>",
                left - 4.0,
                y(*v) + 4.0,
                esc(text)
            );
        }
        let _ = write!(
            s,
            "<text class=axis x=\"{left}\" y=\"{}\">{}</text>",
            H - 6.0,
            esc(&short_time(self.from))
        );
        let _ = write!(
            s,
            "<text class=axis x=\"{}\" y=\"{}\" text-anchor=end>{}</text>",
            W - RIGHT,
            H - 6.0,
            esc(&short_time(self.to))
        );

        if let Some((r, label)) = &self.rule {
            let _ = write!(
                s,
                "<line class=rule x1=\"{left}\" x2=\"{}\" y1=\"{:.1}\" y2=\"{:.1}\"><title>{}</title></line>",
                W - RIGHT,
                y(*r),
                y(*r),
                esc(label)
            );
        }
        for (t, text) in &self.markers {
            if *t < self.from || *t > self.to {
                continue;
            }
            let _ = write!(
                s,
                "<line class=marker x1=\"{:.1}\" x2=\"{:.1}\" y1=\"{TOP}\" y2=\"{}\"><title>{}</title></line>",
                x(*t),
                x(*t),
                H - BOTTOM,
                esc(&format!("{}: {text}", short_time(*t)))
            );
        }
        for (i, line) in self.lines.iter().enumerate() {
            let joined = |a: i64, b: i64| self.gap_ms <= 0 || b - a <= self.gap_ms;
            let mut d = String::new();
            let mut dots = String::new();
            let pts = &line.points;
            for (k, &(t, v)) in pts.iter().enumerate() {
                let from_prev = k > 0 && joined(pts[k - 1].0, t);
                let to_next = k + 1 < pts.len() && joined(t, pts[k + 1].0);
                let _ = write!(
                    d,
                    "{}{:.1},{:.1}",
                    if from_prev { "L" } else { "M" },
                    x(t),
                    y(v)
                );
                // a point with gaps on both sides would be invisible as a path
                if !from_prev && !to_next {
                    let _ = write!(
                        dots,
                        "<circle class=\"d{}\" cx=\"{:.1}\" cy=\"{:.1}\" r=\"2.5\"/>",
                        i.min(2),
                        x(t),
                        y(v)
                    );
                }
            }
            let _ = write!(
                s,
                "<path class=\"s{}\" d=\"{d}\"><title>{}</title></path>{dots}",
                i.min(2),
                esc(&line.label)
            );
        }
        if self.lines.len() > 1 {
            let mut lx = left + 4.0;
            for (i, line) in self.lines.iter().enumerate() {
                let _ = write!(
                    s,
                    "<text class=\"key k{}\" x=\"{lx:.1}\" y=\"12\">{}</text>",
                    i.min(2),
                    esc(&line.label)
                );
                lx += 14.0 + 7.0 * line.label.chars().count() as f64;
            }
        }
        s.push_str("</svg>");
        s
    }
}

/// A row of small blocks, oldest first, one per check. Each has its own tooltip.
pub fn strip(cells: &[(Color, String)]) -> String {
    let w = 8.0;
    let total = (cells.len().max(1) as f64) * w;
    let mut s = format!(
        "<svg class=strip viewBox=\"0 0 {total} 20\" preserveAspectRatio=none role=img aria-label=\"recent checks\">"
    );
    for (i, (c, tip)) in cells.iter().enumerate() {
        let _ = write!(
            s,
            "<rect class=\"b{}\" x=\"{}\" y=\"0\" width=\"{}\" height=\"20\"><title>{}</title></rect>",
            c.css(),
            i as f64 * w + 0.5,
            w - 1.0,
            esc(tip)
        );
    }
    s.push_str("</svg>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaps_break_the_line() {
        let c = Chart {
            from: 0,
            to: 10_000,
            gap_ms: 1500,
            lines: vec![Line {
                label: "snr".into(),
                points: vec![(0, 20.0), (1000, 21.0), (5000, 19.0), (6000, 18.0)],
            }],
            ..Default::default()
        };
        let svg = c.render("SNR");
        let d = svg
            .split(" d=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert_eq!(d.matches('M').count(), 2, "{d}");
        assert_eq!(d.matches('L').count(), 2, "{d}");
        assert!(!svg.contains("<circle"), "no lone points here");
        let lone = Chart {
            from: 0,
            to: 10_000,
            gap_ms: 1500,
            lines: vec![Line {
                label: "x".into(),
                points: vec![(0, 1.0), (5000, 2.0), (6000, 3.0)],
            }],
            ..Default::default()
        };
        assert_eq!(
            lone.render("x").matches("<circle").count(),
            1,
            "the point at 0 stands alone"
        );
    }

    #[test]
    fn labels_are_escaped_and_empty_charts_say_so() {
        let c = Chart {
            from: 0,
            to: 10,
            lines: vec![Line {
                label: "<x>".into(),
                points: vec![],
            }],
            ..Default::default()
        };
        let svg = c.render("a<b");
        assert!(svg.contains("no data yet"));
        assert!(!svg.contains("<b"));
        let s = strip(&[(Color::Red, "<down>".into())]);
        assert!(s.contains("&lt;down&gt;") && s.contains("bred"));
    }

    fn frame_x(svg: &str) -> f64 {
        let rest = svg.split("class=frame x=\"").nth(1).unwrap();
        rest.split('"').next().unwrap().parse().unwrap()
    }

    #[test]
    fn long_axis_labels_get_room_left_of_the_frame() {
        let chart = |unit: &str, v: f64| Chart {
            from: 0,
            to: 10,
            unit: unit.into(),
            lines: vec![Line {
                label: "n".into(),
                points: vec![(0, v), (10, v - 1.5)],
            }],
            ..Default::default()
        };
        assert_eq!(frame_x(&chart("", 5.0).render("short")), MIN_LEFT);
        // "-94.0 dBm" is 9 characters, about 58 px at the axis font size
        let wide = frame_x(&chart("dBm", -94.0).render("noise"));
        assert!(wide >= 6.5 * 9.0 + 8.0, "frame starts at {wide}");
        let unit = "packets per second over the backbone".repeat(4);
        assert_eq!(frame_x(&chart(&unit, 5.0).render("long")), MAX_LEFT);
    }
}
