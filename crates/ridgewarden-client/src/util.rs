//! Small shared helpers: time and HTML escaping.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Current wall time as unix milliseconds. Wall time can be wrong on a Pi with no RTC;
/// it is only used for display and time-series, never for ordering the log.
pub fn now_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

pub fn ms_to_rfc3339(ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
}

pub fn rfc3339_to_ms(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// "2026-11-18 14:02 UTC"
pub fn short_time(ms: i64) -> String {
    let s = ms_to_rfc3339(ms);
    format!("{} {} UTC", &s[..10], &s[11..16])
}

/// "3 min", "5 h", "12 d"
pub fn human_duration(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 90 {
        format!("{s} s")
    } else if s < 90 * 60 {
        format!("{} min", s / 60)
    } else if s < 36 * 3600 {
        format!("{} h", s / 3600)
    } else {
        format!("{} d", s / 86_400)
    }
}

/// Escape text for HTML. Everything shown on the dashboard comes from the mesh (node names,
/// service names), and any of it could contain markup.
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// Node names compare case-insensitively and with or without the `.local.mesh` suffix.
/// An AREDN node name after `norm_node`: a DNS label (letters, digits, hyphens). Anything
/// else is refused, because node names end up in URLs (the crawl) and on pages.
pub fn valid_node(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn norm_node(name: &str) -> String {
    let n = name.trim().to_ascii_lowercase();
    n.strip_suffix(".local.mesh").unwrap_or(&n).to_string()
}

pub fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let m = v.len() / 2;
    Some(if v.len().is_multiple_of(2) {
        (v[m - 1] + v[m]) / 2.0
    } else {
        v[m]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_round_trip() {
        let ms = 1_795_000_000_123;
        assert_eq!(rfc3339_to_ms(&ms_to_rfc3339(ms)), Some(ms));
    }

    #[test]
    fn escapes_markup() {
        assert_eq!(esc("<b a=\"x\">&'"), "&lt;b a=&quot;x&quot;&gt;&amp;&#39;");
    }

    #[test]
    fn node_names_normalize() {
        assert_eq!(norm_node("N0CALL-Cuesta.local.mesh"), "n0call-cuesta");
    }

    #[test]
    fn median_even_and_odd() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&mut []), None);
    }
}
