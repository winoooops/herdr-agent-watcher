//! Block-character gauges and bars (§2.4). All rounding is multiply-then-add-half;
//! dividing first collapses every ratio to zero in integer arithmetic.

pub const FILL: char = '█';
pub const TRACK: char = '░';

/// A reported percentage, clamped to 0-100 and rounded once — **for the gauge
/// only**. The printed label comes from `format::percent()` on the *unclamped*
/// value, so an anomalous 140% reads `99+%` beside a full bar (§2.4).
pub fn gauge_pct(reported: f64) -> u32 {
    if !reported.is_finite() || reported < 0.0 {
        return 0;
    }
    (reported.min(100.0) + 0.5) as u32
}

/// Fill is `(pct * cells + 50) / 100` on the already-rounded integer percent —
/// the second of the two specified quantizations. The extreme guards then
/// consult the RATIO, never the percent: `1/1000` must still show one cell, and
/// `999/1000` must still leave a gap, both of which the rounded percent has
/// already lost (§2.4).
pub fn gauge_cells(pct: u32, value: u64, max: u64, cells: u16) -> u16 {
    if cells == 0 {
        return 0;
    }
    let raw = ((pct.min(100) * cells as u32 + 50) / 100) as u16;
    let mut out = raw.min(cells);
    if value > 0 && out == 0 {
        out = 1;
    }
    if value < max && out == cells {
        out = cells - 1;
    }
    out
}

pub fn gauge(pct: u32, value: u64, max: u64, cells: u16) -> String {
    let on = gauge_cells(pct, value, max, cells);
    let mut s = String::new();
    for _ in 0..on {
        s.push(FILL);
    }
    for _ in on..cells {
        s.push(TRACK);
    }
    s
}

/// Bars scale to the largest tool WITHIN the card; they are not comparable across
/// agents, deliberately. They deliberately do NOT use `gauge_cells`: a gauge means
/// "how full is this", so it forbids a partial value from looking full, while a bar
/// means "how does this compare" — the largest tool *should* fill its bar, and
/// `109/112` should read as full-ish rather than being pushed down to 7 cells.
pub fn tool_bars(counts: &[(String, u64)], cells: u16) -> Vec<u16> {
    let max = counts.iter().map(|(_, n)| *n).max().unwrap_or(0);
    if max == 0 || cells == 0 {
        return counts.iter().map(|_| 0).collect();
    }
    counts
        .iter()
        .map(|(_, n)| {
            let raw = ((*n as u128 * cells as u128 + max as u128 / 2) / max as u128) as u16;
            if *n > 0 {
                raw.max(1).min(cells)
            } else {
                0
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reported_percentage_is_clamped_and_rounded_once() {
        assert_eq!(gauge_pct(24.5), 25);
        assert_eq!(
            gauge_pct(140.0),
            100,
            "for the BAR only; percent() still prints 99+%"
        );
        assert_eq!(gauge_pct(f64::NAN), 0);
        assert_eq!(gauge_pct(-3.0), 0);
    }

    #[test]
    fn gauge_quantizes_twice_and_guards_the_extremes() {
        assert_eq!(
            gauge_cells(25, 245, 1000, 6),
            2,
            "24.5% -> 25% -> 2 of 6 cells"
        );
        assert_eq!(gauge_cells(0, 0, 1, 10), 0, "a true zero is empty");
        assert_eq!(
            gauge_cells(0, 4, 1000, 10),
            1,
            "0.4% rounds to 0% and still shows a cell"
        );
        assert_eq!(
            gauge_cells(100, 999, 1000, 10),
            9,
            "99.9% rounds to 100% and still leaves a gap"
        );
        assert_eq!(
            gauge_cells(100, 1000, 1000, 10),
            10,
            "a true maximum is full"
        );
        assert_eq!(gauge_cells(50, 500, 1000, 14), 7, "50% of 14 cells");
    }

    #[test]
    fn gauge_renders_fill_and_track() {
        assert_eq!(gauge(50, 500, 1000, 14), "███████░░░░░░░");
    }

    #[test]
    fn tool_bars_scale_within_the_card_and_never_vanish() {
        let counts = [
            ("Edit".to_string(), 112u64),
            ("Bash".to_string(), 109),
            ("Read".to_string(), 23),
            ("TaskUpdate".to_string(), 11),
        ];
        let bars = tool_bars(&counts, 8);
        assert_eq!(bars[0], 8);
        assert_eq!(bars[1], 8, "near-equal counts honestly produce equal bars");
        assert_eq!(bars[2], 2);
        assert_eq!(bars[3], 1, "a nonzero tool never renders empty");
    }

    #[test]
    fn padding_counts_cells_not_characters() {
        assert_eq!(
            crate::sidebar::format::width(&crate::sidebar::format::pad("猫猫", 6)),
            6
        );
        assert_eq!(
            crate::sidebar::format::width(&crate::sidebar::format::pad("Edit", 6)),
            6
        );
    }

    #[test]
    fn tool_bars_tolerate_a_zero_only_map() {
        let counts = [("Edit".to_string(), 0u64)];
        assert_eq!(tool_bars(&counts, 8), vec![0], "no divide by zero");
    }
}
