//! Every number, age and string that reaches a cell goes through here (§2.6, §2.7).

use unicode_width::UnicodeWidthStr;

pub const UNAVAILABLE: &str = "—";
/// The full form, 28 cells (§2.6).
pub const UNAVAILABLE_LONG: &str = "— not reported by this agent";

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Control characters and tabs become spaces; ends are trimmed. Interior
/// whitespace is NEVER collapsed — it is data (§2.7).
pub fn sanitise(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\t' || c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Left-aligned padding by DISPLAY CELLS. `format!("{:<11}")` counts chars, so a
/// CJK tool name pads as if it were half its real width and overflows the pane
/// (§2.7). Truncates first, so the result is always exactly `cells` wide.
pub fn pad(s: &str, cells: usize) -> String {
    let mut out = truncate(s, cells);
    out.push_str(&" ".repeat(cells.saturating_sub(width(&out))));
    out
}

/// Truncates by display cells, always ending in `…` (§2.6). A budget below 2
/// yields an empty string: the caller omits the field entirely (§2.7).
pub fn truncate(s: &str, max_cells: usize) -> String {
    if width(s) <= max_cells {
        return s.to_string();
    }
    if max_cells < 2 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + w > max_cells - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Call counts. At most 4 cells; every bucket floors, so it is monotonic.
pub fn count(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{}k", n / 1_000),
        1_000_000..=9_999_999 => format!("{}.{}M", n / 1_000_000, (n % 1_000_000) / 100_000),
        10_000_000..=99_999_999 => format!("{}M", n / 1_000_000),
        _ => "99M+".to_string(),
    }
}

/// Token quantities. One decimal below a million so magnitudes line up.
pub fn tokens(n: u64) -> String {
    fn dec(value: u64, unit: u64, suffix: &str) -> String {
        let whole = value / unit;
        let frac = (value % unit) * 10 / unit;
        if frac == 0 {
            format!("{whole}{suffix}")
        } else {
            format!("{whole}.{frac}{suffix}")
        }
    }
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}.{}k", n / 1_000, (n % 1_000) / 100),
        1_000_000..=999_999_999 => dec(n, 1_000_000, "M"),
        1_000_000_000..=999_949_999_999 => dec(n, 1_000_000_000, "B"),
        _ => "999B+".to_string(),
    }
}

/// §2.6: the sentence at 44 columns and wider, the em dash alone below it —
/// 28 cells does not fit beside an 8-cell label at the 34-column minimum. The
/// dash is the invariant; the sentence is the courtesy. Trace ages keep the
/// short form at every width: an age is a field, not a missing capability.
pub fn unavailable(width: u16) -> &'static str {
    if width >= 44 {
        UNAVAILABLE_LONG
    } else {
        UNAVAILABLE
    }
}

pub fn percent(v: f64) -> String {
    if !v.is_finite() || v < 0.0 {
        return UNAVAILABLE.to_string();
    }
    if v > 100.0 {
        return "99+%".to_string();
    }
    format!("{}%", (v + 0.5) as u64)
}

pub fn money(v: f64) -> String {
    if !v.is_finite() || v < 0.0 {
        return UNAVAILABLE.to_string();
    }
    if v < 10.0 {
        format!("${:.2}", (v * 100.0).floor() / 100.0)
    } else if v < 1_000.0 {
        format!("${:.1}", (v * 10.0).floor() / 10.0)
    } else if v < 10_000.0 {
        format!("${}", v as u64)
    } else if v < 100_000.0 {
        let n = v as u64;
        format!("${}.{}k", n / 1_000, (n % 1_000) / 100)
    } else if v < 1_000_000.0 {
        format!("${}k", (v as u64) / 1_000)
    } else {
        "$999k+".to_string()
    }
}

/// Parses the adapters' ISO-8601 timestamps to epoch millis. `chrono` is already a
/// dependency and is pure Rust, so it stays in the portable set (§5.1).
pub fn parse_iso8601_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
}

/// Relative age; units truncate toward zero so a label never overstates.
pub fn age(then_unix_ms: u64, now_unix_ms: u64) -> String {
    let elapsed_ms = now_unix_ms.saturating_sub(then_unix_ms);
    let secs = elapsed_ms / 1_000;
    match secs {
        0..=59 => "now".to_string(),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        _ => {
            let days = secs / 86_400;
            if days >= 100 {
                "99d+".to_string()
            } else {
                format!("{days}d")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_is_monotonic_across_every_boundary() {
        assert_eq!(count(283), "283");
        assert_eq!(count(9_999), "9999");
        assert_eq!(count(10_000), "10k");
        assert_eq!(count(999_999), "999k");
        assert_eq!(count(1_000_000), "1.0M");
        assert_eq!(count(1_290_000), "1.2M");
        assert_eq!(count(9_999_999), "9.9M");
        assert_eq!(count(10_000_000), "10M");
        assert_eq!(count(99_999_999), "99M");
        assert_eq!(count(100_000_000), "99M+");
        assert_eq!(count(u64::MAX), "99M+");
        for s in [283, 9_999, 10_000, 999_999, 1_000_000, 10_000_000]
            .iter()
            .map(|n| count(*n))
        {
            assert!(s.chars().count() <= 4, "count() is at most 4 cells: {s}");
        }
    }

    #[test]
    fn tokens_uses_one_decimal_below_a_million() {
        assert_eq!(tokens(0), "0");
        assert_eq!(tokens(347), "347");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1_000), "1.0k");
        assert_eq!(tokens(500_000), "500.0k");
        assert_eq!(tokens(999_999), "999.9k");
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(1_250_000), "1.2M");
        assert_eq!(tokens(999_999_999), "999.9M");
        assert_eq!(tokens(1_000_000_000), "1B");
        assert_eq!(tokens(2_500_000_000), "2.5B");
        assert_eq!(tokens(999_949_999_999), "999.9B");
        assert_eq!(tokens(999_950_000_000), "999B+");
        assert_eq!(tokens(u64::MAX), "999B+");
    }

    #[test]
    fn percent_rounds_and_bounds() {
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(12.13), "12%");
        assert_eq!(percent(12.5), "13%");
        assert_eq!(percent(99.4), "99%");
        assert_eq!(percent(99.6), "100%");
        assert_eq!(percent(100.0), "100%");
        assert_eq!(
            percent(100.1),
            "99+%",
            "above the maximum is an anomaly, not 100%"
        );
        assert_eq!(percent(140.0), "99+%");
        assert_eq!(percent(f64::NAN), "—");
        assert_eq!(percent(-1.0), "—");
    }

    #[test]
    fn money_buckets() {
        assert_eq!(money(0.0), "$0.00");
        assert_eq!(money(1.254), "$1.25");
        assert_eq!(money(9.99), "$9.99");
        assert_eq!(money(10.0), "$10.0");
        assert_eq!(money(42.59), "$42.5");
        assert_eq!(money(999.9), "$999.9");
        assert_eq!(money(1_000.0), "$1000");
        assert_eq!(money(1234.9), "$1234");
        assert_eq!(money(9_999.0), "$9999");
        assert_eq!(money(10_000.0), "$10.0k");
        assert_eq!(money(12_345.0), "$12.3k");
        assert_eq!(money(99_999.0), "$99.9k");
        assert_eq!(money(100_000.0), "$100k");
        assert_eq!(money(123_456.0), "$123k");
        assert_eq!(money(999_999.0), "$999k");
        assert_eq!(money(1_000_000.0), "$999k+");
        assert_eq!(money(2_000_000.0), "$999k+");
        assert_eq!(money(f64::INFINITY), "—");
        assert_eq!(money(-5.0), "—");
    }

    #[test]
    fn age_transitions_are_exact() {
        let now = 10_000_000_000_u64;
        assert_eq!(age(now, now), "now");
        assert_eq!(age(now - 59_000, now), "now");
        assert_eq!(age(now - 60_000, now), "1m");
        assert_eq!(age(now - 3_599_000, now), "59m");
        assert_eq!(age(now - 3_600_000, now), "1h");
        assert_eq!(age(now - 86_399_000, now), "23h");
        assert_eq!(age(now - 86_400_000, now), "1d");
        assert_eq!(age(now - 99 * 86_400_000, now), "99d");
        assert_eq!(age(now - 100 * 86_400_000, now), "99d+");
        assert_eq!(age(now + 5_000, now), "now", "clock skew clamps");
    }

    #[test]
    fn sanitise_keeps_interior_spacing_but_kills_controls() {
        assert_eq!(sanitise("printf 'a  b'"), "printf 'a  b'");
        assert_eq!(sanitise("a\nb\tc"), "a b c");
        assert_eq!(sanitise("  trimmed  "), "trimmed");
        assert_eq!(sanitise("x\u{7}y"), "x y");
    }

    #[test]
    fn truncate_measures_display_cells_and_ends_in_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("猫猫猫", 4), "猫…");
        assert_eq!(width("猫"), 2);
        assert_eq!(width("a"), 1);
    }

    #[test]
    fn every_glyph_in_the_vocabulary_is_one_cell() {
        for g in [
            "◐", "!", "○", "✕", "✓", "●", "▸", "▾", "›", "…", "█", "▓", "▒", "░", "─", "↵", "·",
        ] {
            assert_eq!(width(g), 1, "glyph {g} must occupy exactly one cell");
        }
    }
}
