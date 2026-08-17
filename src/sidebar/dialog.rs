//! Panels, drawn the way `view.rs` draws cards: a description in, lines out.
//!
//! It knows nothing about the terminal, the config, or `doctor`. `tui.rs`
//! flattens those into `Row`s, which is what lets this be tested as a table of
//! inputs -- and what lets it live outside the `runtime` feature gate while
//! `doctor` does not.

use crate::sidebar::format;
use crate::sidebar::style::{Line, Role, Semantic, Span, Style};

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
    /// `enabled: false` is shown but cannot be acted on.
    Entry {
        label: String,
        value: String,
        enabled: bool,
    },
    /// Evidence, a path, a remedy. Wrapped rather than truncated: a path cut
    /// off at the panel edge tells the reader a file is involved and not
    /// which one.
    Note(String),
    /// A note that has to be read. Same wrapping, `Semantic::Warn` styling --
    /// the daemon's interval needs a restart to take effect, and saying so in
    /// body text is saying it invisibly.
    Warn(String),
    Rule,
}

const GAP: usize = 3;

/// Split `text` to fit `width` cells, breaking at spaces where it can.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if format::width(&candidate) <= width {
            line = candidate;
            continue;
        }
        if !line.is_empty() {
            out.push(std::mem::take(&mut line));
        }
        // A single word longer than the line -- a path with no spaces -- is
        // broken across lines. NOT `format::truncate`, which appends an
        // ellipsis: that is for text being cut off, and this is text being
        // continued on the next line.
        let mut rest = word.to_string();
        while format::width(&rest) > width {
            let mut head = String::new();
            let mut taken = 0;
            for ch in rest.chars() {
                let next = format::width(&format!("{head}{ch}"));
                if next > width {
                    break;
                }
                head.push(ch);
                taken += 1;
            }
            if taken == 0 {
                break;
            }
            out.push(head);
            rest = rest.chars().skip(taken).collect();
        }
        line = rest;
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

/// How many lines the panel's body will draw, which is what a scroll offset
/// has to be bounded by once notes wrap.
pub fn line_count(panel: &Panel, width: u16) -> usize {
    let inner = (width as usize).saturating_sub(4);
    panel
        .rows
        .iter()
        .map(|row| match row {
            Row::Note(note) | Row::Warn(note) => wrap(note, inner).len(),
            _ => 1,
        })
        .sum()
}

fn framed_styled(inner: &str, style: Style, width: usize) -> Line {
    let inner = format::truncate(inner, width.saturating_sub(2));
    let pad = width.saturating_sub(2 + format::width(&inner));
    vec![
        Span::new("│", Style::role(Role::Rule)),
        Span::new(format!("{inner}{}", " ".repeat(pad)), style),
        Span::new("│", Style::role(Role::Rule)),
    ]
}

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
    let bar = width.saturating_sub(2 + format::width(&label));
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
    let inner = width.saturating_sub(2);
    let value_column = panel
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Entry { label, .. } => Some(format::width(label)),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .saturating_add(2 + GAP)
        .min(inner.saturating_sub(1));

    // Rows become lines first, because a wrapped note is more than one line
    // and the offset has to count what is drawn, not what it came from.
    let mut lines: Vec<(String, Style)> = Vec::new();
    for (index, row) in panel.rows.iter().enumerate() {
        let selected = panel.cursor == Some(index);
        match row {
            Row::Entry {
                label,
                value,
                enabled,
            } => {
                let mark = if selected { "▸" } else { " " };
                let label = format::truncate(label, value_column.saturating_sub(2 + GAP));
                let pad = value_column.saturating_sub(2 + format::width(&label));
                let dim = if *enabled || !mark_read_only {
                    ""
                } else {
                    "  (read-only)"
                };
                // Reverse video, the same way the selected card's header is
                // marked -- it follows whatever theme is in force instead of
                // naming a colour.
                let style = if selected {
                    Style {
                        role: Role::Emphasis,
                        reverse: true,
                        ..Style::default()
                    }
                } else {
                    Style::role(Role::Body)
                };
                lines.push((
                    format!("{mark} {label}{}{value}{dim}", " ".repeat(pad)),
                    style,
                ));
            }
            Row::Note(note) => {
                for piece in wrap(note, inner.saturating_sub(2)) {
                    lines.push((format!("  {piece}"), Style::role(Role::Label)));
                }
            }
            Row::Warn(note) => {
                for piece in wrap(note, inner.saturating_sub(2)) {
                    lines.push((
                        format!("  {piece}"),
                        Style::semantic(Role::Emphasis, Semantic::Warn),
                    ));
                }
            }
            Row::Rule => lines.push(("─".repeat(inner), Style::role(Role::Rule))),
        }
    }

    let first = if panel.cursor.is_some() {
        0
    } else {
        panel.offset.min(lines.len().saturating_sub(1))
    };
    for (text, style) in lines.iter().skip(first).take(body_rows) {
        out.push(framed_styled(text, *style, width));
    }
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
    fn the_value_column_moves_to_fit_the_longest_label() {
        let label = "a label longer than page down";
        let p = Panel {
            title: "Keys".into(),
            rows: vec![
                Row::Entry {
                    label: label.into(),
                    value: "does a thing".into(),
                    enabled: false,
                },
                Row::Entry {
                    label: "short".into(),
                    value: "also aligned".into(),
                    enabled: false,
                },
            ],
            footer: "esc".into(),
            cursor: None,
            offset: 0,
        };
        let text = plain(&render(&p, 60, 8));
        assert!(
            text.iter().any(|line| line.contains(label)),
            "the longest label was truncated: {text:?}"
        );
    }

    #[test]
    fn the_longest_label_keeps_the_minimum_gap() {
        let label = "prune after days";
        let value = "7";
        let p = Panel {
            title: "Settings".into(),
            rows: vec![Row::Entry {
                label: label.into(),
                value: value.into(),
                enabled: true,
            }],
            footer: "esc".into(),
            cursor: None,
            offset: 0,
        };
        let text = plain(&render(&p, 60, 7));
        let line = text
            .iter()
            .find(|line| line.contains(label))
            .unwrap_or_else(|| panic!("the label is missing from {text:?}"));
        let label_start = format::width(&line[..line.find(label).expect("label")]);
        let value_start = format::width(&line[..line.find(value).expect("value")]);

        let gap = value_start - label_start - format::width(label);
        assert!(
            gap >= 3,
            "the longest row is cramped: expected at least 3 spaces between its label and value, got {gap} in {line:?}"
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

    /// A path cut off at the panel edge says a file is involved and not which
    /// one. The doctor report is mostly paths and remedies.
    #[test]
    fn a_long_note_wraps_instead_of_being_cut_off() {
        let long =
            "/Users/winoooops/.local/state/herdr/plugins/herdr-agent-watcher/bin/statusline.sh";
        let p = Panel {
            title: "Doctor".into(),
            rows: vec![Row::Note(long.into())],
            footer: "esc".into(),
            cursor: None,
            offset: 0,
        };
        let text = plain(&render(&p, 40, 12));
        let joined: String = text
            .iter()
            .map(|l| l.trim_matches(['│', ' ']).to_string())
            .collect();
        assert!(
            joined.contains("statusline.sh"),
            "the end of the path survived: {text:?}"
        );
        assert!(!joined.contains('…'), "wrapped, not truncated: {text:?}");
    }

    /// `▸` alone is easy to lose. Reverse video is how the selected card's
    /// header is already marked, so it follows the theme in force.
    #[test]
    fn the_selected_row_is_reversed_not_only_marked() {
        let lines = render(&panel(), 40, 12);
        let reversed: Vec<_> = lines
            .iter()
            .filter(|line| line.iter().any(|span| span.style.reverse))
            .collect();
        assert_eq!(reversed.len(), 1, "exactly the cursor row");
        let text: String = reversed[0].iter().map(|s| s.text.as_str()).collect();
        assert!(
            text.contains("scope"),
            "and it is the row the cursor is on: {text}"
        );
    }

    #[test]
    fn a_warning_is_styled_as_one() {
        let p = Panel {
            title: "Settings".into(),
            rows: vec![Row::Warn("takes effect after restart-daemon".into())],
            footer: "esc".into(),
            cursor: None,
            offset: 0,
        };
        let lines = render(&p, 40, 12);
        assert!(
            lines.iter().any(|line| line
                .iter()
                .any(|span| span.style.semantic == Some(crate::sidebar::style::Semantic::Warn))),
            "a warning in body text is a warning said invisibly"
        );
    }

    #[test]
    fn a_short_frame_keeps_the_frame_and_drops_rows() {
        let text = plain(&render(&panel(), 40, 6));
        assert_eq!(text.len(), 6);
        assert!(text[0].starts_with('┌'));
        assert!(text.last().unwrap().starts_with('└'));
    }
}
