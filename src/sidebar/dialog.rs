//! Panels, drawn the way `view.rs` draws cards: a description in, lines out.
//!
//! It knows nothing about the terminal, the config, or `doctor`. `tui.rs`
//! flattens those into `Row`s, which is what lets this be tested as a table of
//! inputs -- and what lets it live outside the `runtime` feature gate while
//! `doctor` does not.

use crate::sidebar::format;
use crate::sidebar::style::{Line, Role, Span, Style};

pub struct Panel {
    pub title: String,
    pub rows: Vec<Row>,
    pub footer: String,
    /// `None` in a panel with nothing to select, which then scrolls instead.
    /// The doctor report is mostly evidence and remedies; a cursor that can
    /// land on those looks like a key that did nothing.
    pub cursor: Option<usize>,
    /// First row drawn. Only meaningful without a cursor.
    pub offset: usize,
}

pub enum Row {
    /// `enabled: false` is shown but cannot be acted on -- the daemon's
    /// interval, which belongs to another process.
    Entry {
        label: String,
        value: String,
        enabled: bool,
    },
    Note(String),
    Rule,
}

const VALUE_COLUMN: usize = 18;

fn framed(inner: String, width: usize) -> Line {
    let inner = format::truncate(&inner, width.saturating_sub(2));
    let pad = width.saturating_sub(2 + format::width(&inner));
    vec![
        Span::new("│", Style::role(Role::Rule)),
        Span::body(format!("{inner}{}", " ".repeat(pad))),
        Span::new("│", Style::role(Role::Rule)),
    ]
}

fn edge(left: char, right: char, title: &str, width: usize) -> Line {
    let label = if title.is_empty() {
        String::new()
    } else {
        format!(" {title} ")
    };
    let bar = width.saturating_sub(2 + format::width(&label)).max(0);
    vec![Span::new(
        format!("{left}{label}{}{right}", "─".repeat(bar)),
        Style::role(Role::Rule),
    )]
}

/// Always exactly `height` lines of exactly `width` cells: the caller draws
/// this over a frame it has already laid out, so a short or ragged panel
/// leaves the cards showing through.
pub fn render(panel: &Panel, width: u16, height: u16) -> Vec<Line> {
    let width = width as usize;
    // Only worth saying when some rows ARE editable; otherwise it is on every
    // line and distinguishes nothing.
    let mark_read_only = panel
        .rows
        .iter()
        .any(|row| matches!(row, Row::Entry { enabled: true, .. }));
    let height = (height as usize).max(3);
    let mut out = vec![edge('┌', '┐', &panel.title, width)];

    // Two for the borders, two for the footer and its rule.
    let body_rows = height.saturating_sub(4);
    let first = if panel.cursor.is_some() {
        0
    } else {
        panel.offset
    };
    for (offset, row) in panel.rows.iter().skip(first).take(body_rows).enumerate() {
        let index = first + offset;
        let text = match row {
            Row::Entry {
                label,
                value,
                enabled,
            } => {
                let mark = if panel.cursor == Some(index) {
                    "▸"
                } else {
                    " "
                };
                let label = format::truncate(label, VALUE_COLUMN.saturating_sub(3));
                let pad = VALUE_COLUMN.saturating_sub(2 + format::width(&label));
                let dim = if *enabled || !mark_read_only {
                    ""
                } else {
                    "  (read-only)"
                };
                format!("{mark} {label}{}{value}{dim}", " ".repeat(pad))
            }
            Row::Note(note) => format!("  {note}"),
            Row::Rule => "─".repeat(width.saturating_sub(2)),
        };
        out.push(framed(text, width));
    }
    // Fill to three short of the height: the rule, the footer and the bottom
    // border are still to come. Filling to `height - 2` and then pushing three
    // makes the truncate below eat the bottom border, and the two tests that
    // assert the last line starts with `└` fail.
    while out.len() + 3 < height {
        out.push(framed(String::new(), width));
    }
    out.truncate(height.saturating_sub(3));
    out.push(edge('├', '┤', "", width));
    out.push(framed(format!(" {}", panel.footer), width));
    out.push(edge('└', '┘', "", width));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[crate::sidebar::style::Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    fn panel() -> Panel {
        Panel {
            title: "Settings".into(),
            rows: vec![
                Row::Entry {
                    label: "sort".into(),
                    value: "position".into(),
                    enabled: true,
                },
                Row::Entry {
                    label: "scope".into(),
                    value: "workspace".into(),
                    enabled: true,
                },
                Row::Rule,
                Row::Entry {
                    label: "interval_ms".into(),
                    value: "3000".into(),
                    enabled: false,
                },
                Row::Note("needs restart-daemon".into()),
            ],
            footer: "j/k move · ↵ change · esc".into(),
            cursor: Some(1),
            offset: 0,
        }
    }

    #[test]
    fn the_title_and_footer_are_framed() {
        let text = plain(&render(&panel(), 40, 12));
        assert!(
            text[0].starts_with('┌') && text[0].contains("Settings"),
            "{:?}",
            text[0]
        );
        assert!(text.last().unwrap().starts_with('└'), "{:?}", text.last());
        assert!(text.iter().any(|l| l.contains("j/k move")), "{text:?}");
    }

    #[test]
    fn the_cursor_marks_exactly_one_row() {
        for cursor in 0..2 {
            let mut p = panel();
            p.cursor = Some(cursor);
            let marked: Vec<usize> = plain(&render(&p, 40, 12))
                .iter()
                .enumerate()
                .filter(|(_, l)| l.contains('▸'))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(marked.len(), 1, "cursor={cursor}");
        }
    }

    #[test]
    fn values_align_in_a_column() {
        // Two traps here, and the second is why this measures cells.
        //
        // By the value, not by its first letter: "scope" contains a `p` before
        // "workspace" begins, so scanning for a character finds the label.
        //
        // And in DISPLAY CELLS, not bytes: `str::find` returns a byte offset,
        // and the cursor row starts with `▸`, which is three bytes and one
        // cell. Comparing byte offsets makes an aligned column look two apart.
        let text = plain(&render(&panel(), 40, 12));
        let column = |needle: &str| {
            text.iter()
                .find_map(|line| {
                    line.find(needle)
                        .map(|byte| crate::sidebar::format::width(&line[..byte]))
                })
                .unwrap_or_else(|| panic!("{needle} missing from {text:?}"))
        };
        assert_eq!(
            column("position"),
            column("workspace"),
            "the value column wandered: {text:?}"
        );
    }

    #[test]
    fn every_line_is_exactly_the_width() {
        for width in [20u16, 33, 40, 60] {
            for line in plain(&render(&panel(), width, 12)) {
                assert_eq!(
                    crate::sidebar::format::width(&line),
                    width as usize,
                    "width={width} line={line:?}"
                );
            }
        }
    }

    /// The marker distinguishes an editable row from one that is not. In a
    /// panel where nothing is editable there is nothing to distinguish, and it
    /// becomes thirteen columns of noise on every line.
    #[test]
    fn read_only_is_marked_only_where_something_else_is_editable() {
        let mixed = plain(&render(&panel(), 60, 12));
        assert!(
            mixed.iter().any(|l| l.contains("(read-only)")),
            "the daemon interval sits beside editable rows: {mixed:?}"
        );

        let all_locked = Panel {
            title: "Doctor".into(),
            rows: vec![
                Row::Entry {
                    label: "✓".into(),
                    value: "daemon answering".into(),
                    enabled: false,
                },
                Row::Entry {
                    label: "✗".into(),
                    value: "a script is missing".into(),
                    enabled: false,
                },
            ],
            footer: "esc close".into(),
            cursor: Some(0),
            offset: 0,
        };
        let text = plain(&render(&all_locked, 60, 12));
        assert!(
            !text.iter().any(|l| l.contains("(read-only)")),
            "nothing here is editable, so the marker says nothing: {text:?}"
        );
    }

    #[test]
    fn a_long_footer_is_truncated_not_wrapped() {
        let mut p = panel();
        p.footer = "j/k move · ↵ change · s save · r refresh · esc close · q close".into();
        let text = plain(&render(&p, 24, 12));
        assert!(
            text.iter().all(|l| crate::sidebar::format::width(l) == 24),
            "{text:?}"
        );
    }

    /// The doctor report is longer than any panel, and none of it is
    /// selectable. Without this the reader sees the first screenful and no way
    /// to the rest, while j/k appear to do nothing on the note rows.
    #[test]
    fn a_panel_with_no_cursor_scrolls_and_marks_nothing() {
        let rows: Vec<Row> = (0..30).map(|n| Row::Note(format!("line {n}"))).collect();
        let scrolled = Panel {
            title: "Doctor".into(),
            rows,
            footer: "esc close".into(),
            cursor: None,
            offset: 12,
        };
        let text = plain(&render(&scrolled, 40, 10));
        assert!(
            text.iter().all(|l| !l.contains('▸')),
            "nothing to select: {text:?}"
        );
        assert!(
            text.iter().any(|l| l.contains("line 12")),
            "starts at the offset: {text:?}"
        );
        assert!(!text.iter().any(|l| l.contains("line 11")), "{text:?}");
    }

    #[test]
    fn a_short_frame_keeps_the_frame_and_drops_rows() {
        let text = plain(&render(&panel(), 40, 6));
        assert_eq!(text.len(), 6);
        assert!(text[0].starts_with('┌'));
        assert!(text.last().unwrap().starts_with('└'));
    }
}
