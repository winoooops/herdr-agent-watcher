//! Scroll geometry and maths (§3.5). Portable: no terminal, no ratatui.

/// Where a card starts in the scrollable region, and how tall it is. The view
/// computes it once; the shell never re-derives it (§3.4, §3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSpan {
    pub start: usize,
    pub height: usize,
}

// Scroll maths (§3.5). Pure, because a wrong answer here strands the view.

pub fn clamp_scroll(offset: u16, total_lines: usize, viewport_height: u16) -> u16 {
    let total_lines = u16::try_from(total_lines).unwrap_or(u16::MAX);
    offset.min(total_lines.saturating_sub(viewport_height))
}

/// Smallest offset change that keeps `card` visible, clamped to content.
/// A card taller than the viewport returns `card.start`: "smallest change"
/// would leave a header that happens to sit near the bottom exactly where it
/// is, with the whole body below the fold.
pub fn ensure_visible(offset: u16, card: LineSpan, viewport: u16, total_lines: usize) -> u16 {
    if viewport == 0 {
        return 0;
    }
    let start = u16::try_from(card.start).unwrap_or(u16::MAX);
    let height = u16::try_from(card.height).unwrap_or(u16::MAX);
    if height > viewport {
        return clamp_scroll(start, total_lines, viewport);
    }
    let end = start.saturating_add(height);
    let next = if start < offset {
        start
    } else if end > offset.saturating_add(viewport) {
        end.saturating_sub(viewport)
    } else {
        offset
    };
    clamp_scroll(next, total_lines, viewport)
}

/// Detached scrolling anchors to the card, not to a line number: signed,
/// because `↑` legitimately scrolls above the selected header (§3.5).
pub fn reanchor(
    offset: u16,
    old: LineSpan,
    new: LineSpan,
    viewport: u16,
    total_lines: usize,
) -> u16 {
    let intra = offset as i64 - old.start as i64;
    let next = new.start as i64 + intra;
    clamp_scroll(next.clamp(0, u16::MAX as i64) as u16, total_lines, viewport)
}

/// Which part of a card a content line lands on (spec §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Header,
    Body,
}

/// The card whose span contains `line`, and whether that line is its
/// header row. Lines inside no span — separator rows, anything past the
/// last card — are misses the caller treats as inert.
pub fn card_at(spans: &[(String, LineSpan)], line: usize) -> Option<(&str, Hit)> {
    spans.iter().find_map(|(id, span)| {
        let inside = line >= span.start && line < span.start + span.height;
        inside.then(|| {
            let hit = if line == span.start {
                Hit::Header
            } else {
                Hit::Body
            };
            (id.as_str(), hit)
        })
    })
}

pub fn trace_at(spans: &[(String, String, LineSpan)], line: usize) -> Option<(&str, &str)> {
    spans.iter().find_map(|(card, id, span)| {
        (line >= span.start && line < span.start + span.height)
            .then_some((card.as_str(), id.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::layout::LineSpan;

    #[test]
    fn clamp_scroll_bounds_both_ends() {
        assert_eq!(clamp_scroll(0, 100, 24), 0);
        assert_eq!(clamp_scroll(500, 100, 24), 76);
        assert_eq!(clamp_scroll(5, 10, 24), 0);
        assert_eq!(clamp_scroll(3, 0, 24), 0);
    }

    #[test]
    fn ensure_visible_moves_the_minimum_distance() {
        let card = LineSpan {
            start: 40,
            height: 4,
        };
        assert_eq!(ensure_visible(0, card, 20, 100), 24);
        assert_eq!(ensure_visible(60, card, 20, 100), 40);
        assert_eq!(ensure_visible(35, card, 20, 100), 35);
    }

    #[test]
    fn a_card_exactly_as_tall_as_the_viewport_is_positioned_normally() {
        let card = LineSpan {
            start: 40,
            height: 20,
        };
        assert_eq!(
            ensure_visible(0, card, 20, 100),
            40,
            "scrolled to show it whole"
        );
        assert_eq!(
            ensure_visible(40, card, 20, 100),
            40,
            "already exact: no movement"
        );
        assert_eq!(
            ensure_visible(45, card, 20, 100),
            40,
            "pulled back to the header"
        );
        let taller = LineSpan {
            start: 40,
            height: 21,
        };
        assert_eq!(
            ensure_visible(45, taller, 20, 100),
            40,
            "pinned to the header"
        );
    }

    #[test]
    fn ensure_visible_pins_an_oversized_card_to_its_header() {
        let card = LineSpan {
            start: 40,
            height: 30,
        };
        assert_eq!(ensure_visible(38, card, 20, 100), 40);
    }

    #[test]
    fn ensure_visible_returns_zero_for_a_zero_height_viewport() {
        let card = LineSpan {
            start: 40,
            height: 4,
        };
        assert_eq!(ensure_visible(17, card, 0, 100), 0);
    }

    #[test]
    fn reanchor_preserves_intra_card_offset_including_above_the_header() {
        let old = LineSpan {
            start: 40,
            height: 10,
        };
        let new = LineSpan {
            start: 10,
            height: 10,
        };
        assert_eq!(reanchor(45, old, new, 20, 100), 15);
        assert_eq!(reanchor(38, old, new, 20, 100), 8);
    }

    #[test]
    fn card_at_maps_lines_to_cards_and_header_rows() {
        let spans = vec![
            (
                "a".to_string(),
                LineSpan {
                    start: 0,
                    height: 3,
                },
            ),
            (
                "b".to_string(),
                LineSpan {
                    start: 4,
                    height: 20,
                },
            ),
        ];
        assert_eq!(card_at(&spans, 0), Some(("a", Hit::Header)));
        assert_eq!(card_at(&spans, 2), Some(("a", Hit::Body)), "last body row");
        assert_eq!(card_at(&spans, 3), None, "the separator row is inert");
        assert_eq!(card_at(&spans, 4), Some(("b", Hit::Header)));
        assert_eq!(card_at(&spans, 23), Some(("b", Hit::Body)));
        assert_eq!(card_at(&spans, 24), None, "one past the last card");
        assert_eq!(card_at(&spans, 500), None, "past all content");
        assert_eq!(card_at(&[], 0), None, "empty span list");
    }

    #[test]
    fn trace_at_maps_lines_to_rows() {
        let spans = vec![
            (
                "a".to_string(),
                "t1".to_string(),
                LineSpan {
                    start: 5,
                    height: 1,
                },
            ),
            (
                "a".to_string(),
                "t2".to_string(),
                LineSpan {
                    start: 6,
                    height: 1,
                },
            ),
        ];
        assert_eq!(trace_at(&spans, 5), Some(("a", "t1")));
        assert_eq!(trace_at(&spans, 6), Some(("a", "t2")));
        assert_eq!(trace_at(&spans, 4), None, "line before the rows");
        assert_eq!(trace_at(&spans, 7), None, "line past the rows");
        assert_eq!(trace_at(&[], 5), None, "empty spans");
    }
}
