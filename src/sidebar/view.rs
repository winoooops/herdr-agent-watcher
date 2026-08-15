use serde_json::Value;

use crate::daemon::store::{CardState, PaneTelemetry};
use crate::sidebar::agent_ids;
use crate::sidebar::bars;
use crate::sidebar::config::{AgentMark, ToolCallStyle};
use crate::sidebar::format::{self, UNAVAILABLE};
use crate::sidebar::metrics;
pub use crate::sidebar::select::Sort;
pub use crate::sidebar::style::{
    AgentAppearance, AgentAppearances, ConfigStatus, Line, Rendered, Role, Semantic, Span, Style,
};

const GAUGE_MAX_CELLS: u16 = 14;
pub const MIN_WIDTH: u16 = 34;
const STATE_LABEL_MIN: u16 = 40;
pub(crate) const MODEL_LINE_MIN: u16 = 36;

#[derive(Clone, Copy)]
pub struct CardCtx<'a> {
    pub appearances: &'a AgentAppearances,
    pub mark: AgentMark,
    pub tool_calls: ToolCallStyle,
    pub trace_lines: u8,
    pub cwd_label: Option<&'a str>,
    pub width: u16,
    pub selected: bool,
    pub now_unix_ms: u64,
}

fn state_glyph(state: CardState) -> (&'static str, &'static str, Semantic) {
    match state {
        CardState::Running => ("◐", "working", Semantic::Good),
        CardState::Attention => ("!", "needs you", Semantic::Warn),
        CardState::Finished => ("✓", "finished", Semantic::Good),
        CardState::Error => ("✕", "error", Semantic::Bad),
        CardState::Idle => ("○", "idle", Semantic::Accent),
    }
}

fn canonical_of(t: &PaneTelemetry) -> Option<&'static str> {
    t.agent.as_deref().and_then(agent_ids::canonical)
}

/// Which "no value" message CONTEXT, CACHE and COST get. For every agent but
/// Claude the absence really is the agent's own limit. Claude reports all three
/// through a settings overlay, so their absence means the bridge never reached
/// this pane — telling the reader "not reported by this agent" there sends them
/// looking at the agent instead of at their PATH, cwd or pane id (README,
/// "Claude metrics bridge").
///
/// The tell is `contextWindowSize == 0`, NOT a missing `contextWindow` block.
/// `context_window_from_dto` returns a defaulted `ContextWindowStatus` when the
/// payload carries no block, and `context_window_size` is a plain `u64`, so the
/// key is present on every parsed status and only its value distinguishes the
/// seed from a real report. `metrics::context` bails on the same `window == 0`.
fn missing_metric(t: &PaneTelemetry, width: u16) -> &'static str {
    let reported_window = t
        .status
        .as_ref()
        .and_then(|s| s.get("contextWindow")?.get("contextWindowSize")?.as_u64())
        .unwrap_or(0);
    if canonical_of(t) == Some("claude") && reported_window == 0 {
        format::unbridged(width)
    } else {
        format::unavailable(width)
    }
}

fn look<'a>(t: &PaneTelemetry, appearances: &'a AgentAppearances) -> Option<&'a AgentAppearance> {
    appearances.get(canonical_of(t)?)
}

fn task_of(t: &PaneTelemetry) -> Option<String> {
    let raw = t.title.as_ref()?.get("title")?.as_str()?;
    let clean = format::sanitise(raw);
    if clean.is_empty() {
        None
    } else {
        Some(clean)
    }
}

pub fn cwd_labels(paths: &[(String, Option<String>)]) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;

    let parts: HashMap<&str, Vec<String>> = paths
        .iter()
        .filter_map(|(id, path)| {
            let path = format::sanitise(path.as_deref()?);
            Some((
                id.as_str(),
                path.trim_end_matches('/')
                    .split('/')
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
            ))
        })
        .collect();

    let mut depth: HashMap<&str, usize> = parts.keys().map(|id| (*id, 1usize)).collect();
    let label_at = |p: &Vec<String>, d: usize| -> String {
        if p.is_empty() {
            return "/".to_string();
        }
        let take = d.min(p.len());
        p[p.len() - take..].join("/")
    };

    loop {
        let mut groups: HashMap<String, Vec<&str>> = HashMap::new();
        for (id, p) in &parts {
            groups.entry(label_at(p, depth[id])).or_default().push(id);
        }
        let mut progressed = false;
        for (_, ids) in groups.iter().filter(|(_, ids)| ids.len() > 1) {
            let first = &parts[ids[0]];
            if ids.iter().all(|id| parts[*id] == *first) {
                continue;
            }
            for id in ids {
                if depth[id] < parts[id].len() {
                    *depth.get_mut(id).expect("known id") += 1;
                    progressed = true;
                }
            }
        }
        if !progressed {
            let mut groups: HashMap<String, Vec<&str>> = HashMap::new();
            for (id, p) in &parts {
                groups.entry(label_at(p, depth[id])).or_default().push(id);
            }
            let mut out: HashMap<String, String> = HashMap::new();
            for (label, ids) in groups {
                if ids.len() == 1 {
                    out.insert(ids[0].to_string(), label);
                    continue;
                }
                let handle_len = (2..=8).find(|k| {
                    let mut seen = std::collections::HashSet::new();
                    ids.iter().all(|id| seen.insert(suffix(id, *k)))
                });
                for id in ids {
                    let handle = match handle_len {
                        Some(k) => suffix(id, k),
                        None => format::truncate(id, 8),
                    };
                    out.insert(id.to_string(), format!("{label} ·{handle}"));
                }
            }
            return out;
        }
    }
}

fn suffix(id: &str, cells: usize) -> String {
    let chars: Vec<char> = id.chars().collect();
    chars[chars.len().saturating_sub(cells)..].iter().collect()
}

fn cwd_of(t: &PaneTelemetry, cx: &CardCtx<'_>) -> Option<String> {
    if let Some(label) = cx.cwd_label {
        let label = format::sanitise(label);
        return (!label.is_empty()).then_some(label);
    }
    let raw = format::sanitise(t.cwd.as_deref()?);
    if raw.is_empty() {
        return None;
    }
    if raw == "/" {
        return Some(raw);
    }
    Some(raw.rsplit('/').next().unwrap_or(&raw).to_string())
}

fn pad_to(line: &mut Line, width: u16) {
    let used: usize = line.iter().map(|s| format::width(&s.text)).sum();
    if (used as u16) < width {
        line.push(Span::body(" ".repeat(width as usize - used)));
    }
}

fn justify(left: Line, right: Line, width: u16) -> Line {
    let l: usize = left.iter().map(|s| format::width(&s.text)).sum();
    let r: usize = right.iter().map(|s| format::width(&s.text)).sum();
    let gap = (width as usize).saturating_sub(l + r).max(1);
    let mut out = left;
    out.push(Span::body(" ".repeat(gap)));
    out.extend(right);
    out
}

fn header(t: &PaneTelemetry, cx: &CardCtx<'_>, open: bool) -> Line {
    let (glyph, label, semantic) = state_glyph(t.card_state);
    let look = look(t, cx.appearances);
    let name = look.map(|a| a.label.clone()).unwrap_or_else(|| {
        let raw = format::sanitise(t.agent.as_deref().unwrap_or_default());
        if raw.is_empty() {
            "AGENT".to_string()
        } else {
            raw.to_uppercase()
        }
    });
    let name_budget = (cx.width as usize).saturating_sub(4 + 11 + 1);
    let name = format::truncate(&name, name_budget.max(3));
    let mark_style = Style {
        role: Role::Body,
        rgb: look.map(|a| a.rgb),
        ansi: look.map(|a| a.ansi),
        ..Style::default()
    };
    let mark = match cx.mark {
        AgentMark::Dot => "●".to_string(),
        AgentMark::Initial => {
            crate::sidebar::config::initial_mark(&name).unwrap_or_else(|| "●".to_string())
        }
        AgentMark::Symbol => look
            .and_then(|a| a.symbol.clone())
            .unwrap_or_else(|| "●".to_string()),
    };

    // Selection reverses the NAME only. Reversing the whole line turns every
    // foreground into a background, and this header is deliberately
    // multi-coloured — the brand-coloured mark and the semantic state glyph each
    // became their own differently-tinted block, so the "selection bar" arrived
    // as a patchwork rather than one band. Reverse video is only clean over
    // uniformly-styled text, and the name is the only such run here.
    //
    // The reversed run is the name EXACTLY as it renders unselected — no padding
    // around it. Padding would make the header one cell wider when selected, so
    // the name would jump sideways as the cursor moves down the list. Selection
    // changes styling, never layout.
    let name_span = if cx.selected {
        Span::new(
            name,
            Style {
                role: Role::Emphasis,
                reverse: true,
                ..Style::default()
            },
        )
    } else {
        Span::emphasis(name)
    };
    let left = vec![
        Span::label(if open { "▾ " } else { "▸ " }),
        Span::new(mark, mark_style),
        Span::body(" "),
        name_span,
    ];
    let right = if cx.width >= STATE_LABEL_MIN {
        vec![
            Span::new(glyph, Style::semantic(Role::Body, semantic)),
            Span::new(format!(" {label}"), Style::semantic(Role::Body, semantic)),
        ]
    } else {
        vec![Span::new(glyph, Style::semantic(Role::Body, semantic))]
    };
    justify(left, right, cx.width)
}

fn task_line(t: &PaneTelemetry, cx: &CardCtx<'_>) -> Line {
    let cwd = cwd_of(t, cx);
    let task = task_of(t);
    let budget = (cx.width as usize).saturating_sub(2);
    match (cwd, task) {
        (Some(cwd), Some(task)) => {
            let cwd_budget = ((cx.width as usize) * 4 / 10).max(6);
            let cwd = format::truncate(&cwd, cwd_budget);
            let rest = budget.saturating_sub(format::width(&cwd) + 3);
            vec![
                Span::body("  "),
                Span::label(cwd),
                Span::label(" › "),
                Span::emphasis(format::truncate(&task, rest)),
            ]
        }
        (Some(cwd), None) => vec![Span::body("  "), Span::body(format::truncate(&cwd, budget))],
        (None, Some(task)) => vec![
            Span::body("  "),
            Span::emphasis(format::truncate(&task, budget)),
        ],
        (None, None) => vec![
            Span::body("  "),
            Span::label(t.agent.clone().unwrap_or_else(|| "unknown".into())),
        ],
    }
}

fn metrics_line(t: &PaneTelemetry, cx: &CardCtx<'_>) -> Line {
    let cells = GAUGE_MAX_CELLS.min(cx.width.saturating_sub(26)).max(6);
    let ctx = t.status.as_ref().and_then(metrics::context);
    let left = match ctx {
        Some(c) => {
            let pct = format::percent(c.pct);
            let semantic = Semantic::Accent;
            vec![
                Span::body("  "),
                Span::new(
                    bars::gauge(bars::gauge_pct(c.pct), c.used, c.window, cells),
                    Style::semantic(Role::Body, semantic),
                ),
                Span::new(format!(" {pct}"), Style::semantic(Role::Body, semantic)),
            ]
        }
        None => vec![Span::body("  "), Span::body(missing_metric(t, cx.width))],
    };
    let right = vec![
        Span::emphasis(format::count(t.tool_call_total)),
        Span::label(" calls"),
    ];
    justify(left, right, cx.width)
}

pub fn compact_card(t: &PaneTelemetry, cx: &CardCtx<'_>) -> Vec<Line> {
    vec![header(t, cx, false), task_line(t, cx), metrics_line(t, cx)]
}

/// An 8-cell label gutter, then a body clipped to what is left (§2.7).
fn label_row(label: &str, body: Line, width: u16) -> Line {
    let budget = (width as usize).saturating_sub(8);
    let mut out = vec![Span::label(format!("{label:<8}"))];
    let mut used = 0usize;
    for span in body {
        let left = budget.saturating_sub(used);
        if left == 0 {
            break;
        }
        used += format::width(&span.text).min(left);
        let text = format::truncate(&span.text, left);
        out.push(Span { text, ..span });
    }
    out
}

fn blank() -> Line {
    vec![Span::body(String::new())]
}

fn detail(parts: &[(String, &str)], width: u16) -> Line {
    let budget = (width as usize).saturating_sub(2);
    let mut line = vec![Span::body("  ")];
    let mut used = 0usize;
    for (i, (value, qualifier)) in parts.iter().enumerate() {
        let sep = if i > 0 { " · " } else { "" };
        let text = format!("{value} {qualifier}").trim_end().to_string();
        let cost = format::width(sep) + format::width(&text);
        if used + cost <= budget {
            if !sep.is_empty() {
                line.push(Span::label(sep));
            }
            line.push(Span::emphasis(text));
            used += cost;
            continue;
        }
        let left = budget.saturating_sub(used + format::width(sep));
        if left >= 2 {
            if !sep.is_empty() {
                line.push(Span::label(sep));
            }
            line.push(Span::emphasis(format::truncate(&text, left)));
        }
        break;
    }
    line
}

fn bar_rows(top: &[(String, u64)], total_reported: u64, width: u16) -> Vec<Line> {
    let mut out = Vec::new();
    if top.is_empty() {
        if total_reported > 0 {
            out.push(vec![
                Span::body("  "),
                Span::body(format::unavailable(width)),
            ]);
        }
        return out;
    }
    let shown: Vec<(String, u64)> = top.iter().take(4).cloned().collect();
    let bar_cells = 8u16.min((width.saturating_sub(18)).max(1));
    let cellw = bars::tool_bars(&shown, bar_cells);
    for ((name, n), on) in shown.iter().zip(cellw) {
        out.push(vec![
            Span::body("  "),
            Span::body(format::pad(name, 11)),
            Span::emphasis(format!("{:>4}", format::count(*n))),
            Span::body(" "),
            Span::new(
                "█".repeat(on as usize),
                Style::semantic(Role::Body, Semantic::Accent),
            ),
        ]);
    }
    if top.len() > 4 {
        out.push(vec![Span::label(format!(
            "  +{} more tools",
            top.len() - 4
        ))]);
    }
    out
}

fn ranked_tools(t: &PaneTelemetry) -> Vec<(String, u64)> {
    let mut top: Vec<(String, u64)> = t
        .tool_counts
        .iter()
        .map(|(k, v)| (format::sanitise(k), *v))
        .filter(|(name, count)| !name.is_empty() && *count > 0)
        .collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    top
}

/// The `TOOLS` body below its header line. `bars` is up to four rows plus a
/// remainder; `jar` is always exactly two (§2.4).
fn tool_rows(t: &PaneTelemetry, cx: &CardCtx<'_>) -> Vec<Line> {
    let top = ranked_tools(t);
    match cx.tool_calls {
        ToolCallStyle::Bars => bar_rows(&top, t.tool_call_total, cx.width),
        ToolCallStyle::Jar => jar_rows(&top, t.tool_call_total, cx.width),
    }
}

/// Cell counts that fill `inner` exactly. Shares are multiplied then ROUNDED —
/// flooring every segment loses up to a cell per tool and leaves a ragged right
/// edge — and the remainder, positive or negative, lands on the first tool in
/// rendered order (§2.4). The walk past the first tool only runs in the
/// degenerate case where the first segment cannot absorb the whole overshoot;
/// filling `inner` exactly is the invariant, so it must not be optional.
fn jar_cells(counts: &[u64], inner: usize) -> Vec<usize> {
    if counts.is_empty() {
        return Vec::new();
    }
    let total: u128 = counts.iter().map(|n| *n as u128).sum::<u128>().max(1);
    let mut cells: Vec<usize> = counts
        .iter()
        .map(|n| (((*n as u128 * inner as u128 * 2) + total) / (total * 2)) as usize)
        .collect();
    let assigned: usize = cells.iter().sum();
    if assigned < inner {
        cells[0] += inner - assigned;
    } else {
        let mut over = assigned - inner;
        let mut i = 0;
        while over > 0 && i < cells.len() {
            let take = cells[i].min(over);
            cells[i] -= take;
            over -= take;
            i += 1;
        }
    }
    cells
}

const TRACK_EMPTY: &str = "░";
const JAR_SHADES: [char; 4] = ['█', '▓', '▒', '░'];

/// Measurement only — the fitting loop needs a width before it commits.
fn legend_text(shown: &[(String, u64)], omitted: usize) -> String {
    let mut parts: Vec<String> = shown
        .iter()
        .map(|(name, n)| format!("{name} {}", format::count(*n)))
        .collect();
    if omitted > 0 {
        parts.push(format!("+{omitted} more"));
    }
    parts.join(" · ")
}

/// The rendered legend. Tool names and counts are the only way to read the band,
/// so they are load-bearing and never take a role that can dim out (§2.5); only
/// the separators and the omitted-count do.
fn legend_spans(shown: &[(String, u64)], omitted: usize, inner: usize) -> Line {
    let suffix = format!("+{omitted} more");
    // The count's cells are RESERVED before any name gets one. A single
    // over-long tool name would otherwise consume the width and truncate the
    // suffix away, turning "and three more you cannot see" into silence — and
    // the shrink loop in `jar_rows` cannot help, because it never drops the last
    // entry (§2.4).
    let reserved = if omitted > 0 {
        format::width(&suffix) + 3
    } else {
        0
    };
    let names_budget = inner.saturating_sub(reserved);

    let mut out = vec![Span::body("  ")];
    let mut used = 0usize;
    // Takes the cells STILL AVAILABLE, not a budget, so each caller can reserve
    // for what comes after it. Not `mut`: it captures nothing it mutates.
    let push = |out: &mut Line, used: &mut usize, left: usize, text: String, dim: bool| {
        if left == 0 {
            return;
        }
        let text = format::truncate(&text, left);
        *used += format::width(&text);
        out.push(if dim {
            Span::label(text)
        } else {
            Span::emphasis(text)
        });
    };
    // Every remaining-cell figure is computed into a local FIRST. Passing
    // `&mut used` and reading `used` in the same argument list is `E0503`: the
    // mutable borrow is taken before the other arguments are evaluated.
    for (i, (name, n)) in shown.iter().enumerate() {
        if i > 0 {
            let left = names_budget.saturating_sub(used);
            push(&mut out, &mut used, left, " · ".to_string(), true);
        }
        // §2.7's priority is count before name, so the number's cells are held
        // back before the name gets any: a 50-character MCP tool name may elide,
        // but `Name 112` must never render as `A-RIDICULOUSLY-LONG-TOOL-NAM…`
        // with the count silently gone.
        let count = format!(" {}", format::count(*n));
        let for_name = names_budget
            .saturating_sub(used)
            .saturating_sub(format::width(&count));
        push(&mut out, &mut used, for_name, name.clone(), false);
        let left = names_budget.saturating_sub(used);
        push(&mut out, &mut used, left, count, false);
    }
    if omitted > 0 {
        if used > 0 {
            let left = inner.saturating_sub(used);
            push(&mut out, &mut used, left, " · ".to_string(), true);
        }
        let left = inner.saturating_sub(used);
        push(&mut out, &mut used, left, suffix, true);
    }
    out
}

fn jar_rows(top: &[(String, u64)], total_reported: u64, width: u16) -> Vec<Line> {
    let inner = (width as usize).saturating_sub(2);
    if top.is_empty() {
        // Both empty meanings draw the same empty track — that IS the shape of
        // "no breakdown"; only the legend distinguishes them (§2.4).
        let legend = if total_reported > 0 {
            format::unavailable(width).to_string()
        } else {
            "0 calls".to_string()
        };
        return vec![
            vec![Span::body("  "), Span::label(TRACK_EMPTY.repeat(inner))],
            // Body, not Label: `0 calls` and the unavailable marker are the only
            // thing that distinguishes the two empty meanings (§2.4).
            vec![Span::body("  "), Span::body(legend)],
        ];
    }

    // Legend FIRST, band second: a segment must never belong to a tool the
    // legend dropped (§2.4). Grow while it fits, then shrink until the `+N more`
    // suffix fits too — a truthful count outranks one more name.
    let capped: Vec<(String, u64)> = top.iter().take(4).cloned().collect();
    let mut shown: Vec<(String, u64)> = Vec::new();
    for entry in &capped {
        let mut trial = shown.clone();
        trial.push(entry.clone());
        if format::width(&legend_text(&trial, top.len() - trial.len())) <= inner {
            shown = trial;
        } else {
            break;
        }
    }
    if shown.is_empty() {
        shown.push(capped[0].clone());
    }
    while shown.len() > 1 && format::width(&legend_text(&shown, top.len() - shown.len())) > inner {
        shown.pop();
    }
    let legend = legend_spans(&shown, top.len() - shown.len(), inner);

    let cells = jar_cells(&shown.iter().map(|(_, n)| *n).collect::<Vec<_>>(), inner);
    let mut band = vec![Span::body("  ")];
    for (i, n) in cells.iter().enumerate() {
        band.push(Span::new(
            JAR_SHADES[i % JAR_SHADES.len()].to_string().repeat(*n),
            Style::semantic(
                Role::Body,
                if i == 0 {
                    Semantic::Accent
                } else {
                    Semantic::Good
                },
            ),
        ));
    }
    vec![band, legend]
}

pub fn expanded_card(t: &PaneTelemetry, cx: &CardCtx<'_>) -> Vec<Line> {
    let width = cx.width;
    let mut lines = vec![header(t, cx, true), task_line(t, cx)];
    if width >= MODEL_LINE_MIN {
        let model = t
            .status
            .as_ref()
            .and_then(|s| s.get("modelDisplayName"))
            .and_then(Value::as_str)
            .map(format::sanitise)
            .filter(|s| !s.is_empty());
        lines.push(match model {
            Some(m) => vec![
                Span::body("  "),
                Span::body(format::truncate(&m, (width as usize).saturating_sub(2))),
            ],
            None => vec![Span::body("  "), Span::body(format::unavailable(width))],
        });
    }
    lines.push(blank());
    let cells = GAUGE_MAX_CELLS.min(width.saturating_sub(13)).max(6);

    // CONTEXT, CACHE and COST go missing together and for one reason, so the
    // sentence is spent on the FIRST row that lacks a value and every later row
    // falls back to the bare dash. Repeated five times it reads as five separate
    // problems, which is the opposite of what the message is for.
    let mut explained = false;
    let mut mark = || {
        if std::mem::replace(&mut explained, true) {
            format::UNAVAILABLE
        } else {
            missing_metric(t, width)
        }
    };

    match t.status.as_ref().and_then(metrics::context) {
        Some(c) => {
            lines.push(label_row(
                "CONTEXT",
                vec![
                    Span::new(
                        bars::gauge(bars::gauge_pct(c.pct), c.used, c.window, cells),
                        Style::semantic(Role::Body, Semantic::Accent),
                    ),
                    Span::body(format!(" {}", format::percent(c.pct))),
                ],
                width,
            ));
            lines.push(detail(
                &[
                    (format::tokens(c.used), "used"),
                    (format::tokens(c.left), "left"),
                    (format::tokens(c.window), ""),
                ],
                width,
            ));
        }
        None => {
            lines.push(label_row("CONTEXT", vec![Span::body(mark())], width));
            lines.push(detail(&[(mark().to_string(), "")], width));
        }
    }
    lines.push(blank());

    match t
        .status
        .as_ref()
        .zip(canonical_of(t))
        .and_then(|(s, agent)| metrics::cache(s, agent))
    {
        Some(c) => {
            lines.push(label_row(
                "CACHE",
                vec![
                    Span::new(
                        bars::gauge(c.pct as u32, c.read, c.denom, cells),
                        Style::semantic(Role::Body, Semantic::Good),
                    ),
                    Span::body(format!(" {}%", c.pct)),
                ],
                width,
            ));
            lines.push(detail(
                &[
                    (format::tokens(c.read), "cached"),
                    (format::tokens(c.wrote), "wrote"),
                    (format::tokens(c.fresh), "fresh"),
                ],
                width,
            ));
        }
        None => {
            lines.push(label_row("CACHE", vec![Span::body(mark())], width));
            lines.push(detail(&[(mark().to_string(), "")], width));
        }
    }
    lines.push(blank());

    let cost = t
        .status
        .as_ref()
        .and_then(|s| s.get("cost")?.get("totalCostUsd")?.as_f64());
    lines.push(label_row(
        "COST",
        vec![match cost {
            Some(v) => Span::emphasis(format!("{} session", format::money(v))),
            None => Span::body(mark().to_string()),
        }],
        width,
    ));
    lines.push(blank());
    lines.push(label_row(
        "TOOLS",
        vec![
            Span::emphasis(format::count(t.tool_call_total)),
            Span::body(" calls"),
        ],
        width,
    ));
    lines.extend(tool_rows(t, cx));
    lines.push(blank());
    lines.extend(trace_rows(t, cx));
    lines
}

/// Settled calls only, newest first. In-flight rows are cut from v1 (§1.3);
/// work in progress reads as the card state.
fn trace_rows(t: &PaneTelemetry, cx: &CardCtx<'_>) -> Vec<Line> {
    let width = cx.width;
    let retained = t.tool_calls.len();
    let mut out = vec![vec![
        Span::label("▾ TRACES "),
        Span::emphasis(format::count(retained as u64)),
        Span::body(" retained"),
    ]];

    for call in t.tool_calls.iter().rev().take(cx.trace_lines as usize) {
        let failed = call.get("status").and_then(Value::as_str) == Some("failed");
        let (glyph, semantic) = if failed {
            ("✕", Semantic::Bad)
        } else {
            ("✓", Semantic::Good)
        };
        let tool = format::sanitise(call.get("tool").and_then(Value::as_str).unwrap_or("?"));
        let args = format::sanitise(call.get("args").and_then(Value::as_str).unwrap_or(""));
        // `AgentToolCallEvent.timestamp` is an ISO-8601 STRING (`agent/types.rs`),
        // not epoch millis — reading it as a number renders every trace as an em
        // dash, which is exactly the silent-wrong this spec is about.
        let stamp = call
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(format::parse_iso8601_ms);
        let age = stamp
            .map(|ms| format::age(ms, cx.now_unix_ms))
            .unwrap_or_else(|| UNAVAILABLE.into());

        // 2 indent + 2 glyph + 6 tool, then the age at the right edge with one
        // separating cell. A budget under 2 cells drops the argument entirely
        // rather than rendering a lone ellipsis (§2.7).
        let fixed = 2 + 2 + 6 + format::width(&age) + 1;
        let arg_budget = (width as usize).saturating_sub(fixed);
        let args = if arg_budget >= 2 {
            format::truncate(&args, arg_budget)
        } else {
            String::new()
        };
        let used = 2 + 2 + 6 + format::width(&args);
        let gap = (width as usize)
            .saturating_sub(used + format::width(&age))
            .max(1);

        out.push(vec![
            Span::body("  "),
            Span::new(format!("{glyph} "), Style::semantic(Role::Body, semantic)),
            Span::emphasis(format::pad(&tool, 6)),
            Span::body(args),
            Span::body(" ".repeat(gap)),
            // Body, never Label: an age is a numeral the reader acts on, and
            // §2.5 forbids load-bearing spans from taking a role that can dim
            // out of existence.
            Span::body(age),
        ]);
    }

    let shown = retained.min(cx.trace_lines as usize);
    if retained > shown {
        out.push(vec![Span::label(format!(
            "    +{} older",
            retained - shown
        ))]);
    }
    out
}

/// Everything the view needs that is not telemetry. Owned by the shell,
/// mutated by keys, passed in whole — the view stays a pure function of
/// (telemetry, interaction, width, clock).
#[derive(Debug, Clone)]
pub struct ViewInput<'a> {
    pub cursor: Option<&'a str>,
    /// Pane ids whose expansion is FLIPPED from the auto_expand default (§3.2).
    pub toggled: &'a std::collections::HashSet<String>,
    pub hide_idle: bool,
    pub sort: crate::sidebar::select::Sort,
    pub auto_expand: crate::sidebar::config::AutoExpand,
    pub agent_mark: crate::sidebar::config::AgentMark,
    pub tool_calls: crate::sidebar::config::ToolCallStyle,
    pub theme: crate::sidebar::config::Theme,
    pub trace_lines: u8,
    pub agents: &'a AgentAppearances,
    pub config: crate::sidebar::style::ConfigStatus,
}

fn expanded_by_default(auto: crate::sidebar::config::AutoExpand) -> bool {
    matches!(auto, crate::sidebar::config::AutoExpand::All)
}

pub fn render(
    telemetry: &crate::sidebar::reducer::State,
    view: &ViewInput<'_>,
    width: u16,
    now_unix_ms: u64,
) -> crate::sidebar::style::Rendered {
    use crate::sidebar::select;

    let width = width.max(MIN_WIDTH);
    let visible = select::visible(&telemetry.panes, view.sort, view.hide_idle, None);
    let labels = cwd_labels(
        &visible
            .panes
            .iter()
            .map(|(id, t)| (id.clone(), t.cwd.clone()))
            .collect::<Vec<_>>(),
    );

    let mut scrollable: Vec<Line> = Vec::new();
    let mut spans: Vec<(String, crate::sidebar::layout::LineSpan)> = Vec::new();
    if visible.panes.is_empty() && visible.hidden_idle == 0 {
        scrollable.push(vec![Span::label("  no agents bound")]);
    }

    for (i, (id, t)) in visible.panes.iter().enumerate() {
        if i > 0 {
            scrollable.push(vec![Span::new(
                "─".repeat(width as usize),
                Style::role(Role::Rule),
            )]);
        }
        let open = expanded_by_default(view.auto_expand) ^ view.toggled.contains(id);
        let start = scrollable.len();
        let cx = CardCtx {
            appearances: view.agents,
            mark: view.agent_mark,
            tool_calls: view.tool_calls,
            trace_lines: view.trace_lines,
            cwd_label: labels.get(id.as_str()).map(String::as_str),
            width,
            selected: view.cursor == Some(id.as_str()),
            now_unix_ms,
        };
        let card = if open {
            expanded_card(t, &cx)
        } else {
            compact_card(t, &cx)
        };
        let height = card.len();
        scrollable.extend(card);
        spans.push((
            id.clone(),
            crate::sidebar::layout::LineSpan { start, height },
        ));
    }

    let mut pinned: Vec<Line> = Vec::new();
    if view.config.problems > 0 {
        pinned.push(vec![Span::label(config_notice(view.config, width))]);
    }
    if visible.hidden_idle > 0 {
        let text = if width >= 34 {
            format!("  +{} idle hidden", visible.hidden_idle)
        } else {
            format!("  +{} idle", visible.hidden_idle)
        };
        pinned.push(vec![Span::label(text)]);
    }
    pinned.push(footer(width));

    crate::sidebar::style::Rendered {
        scrollable,
        pinned,
        spans,
    }
}

fn config_notice(status: crate::sidebar::style::ConfigStatus, width: u16) -> String {
    let n = if status.problems > 99 {
        "99+".to_string()
    } else {
        status.problems.to_string()
    };
    let full = if status.log_written {
        format!("config.toml: {n} problems — see config-problems.log")
    } else {
        format!("config.toml: {n} problems (log unavailable)")
    };
    if width >= 51 {
        full
    } else if width >= 25 {
        format!("config.toml: {n} problems")
    } else {
        format!("config: {n}")
    }
}

fn footer(width: u16) -> Line {
    let hints = [("j/k", "move"), ("o/↵", "expand"), ("z", "idle")];
    let cost = |keep: usize| -> usize {
        if keep == 0 {
            return 3;
        }
        3 + hints[..keep]
            .iter()
            .map(|(k, v)| format::width(k) + 1 + format::width(v))
            .sum::<usize>()
            + 3 * (keep - 1)
    };
    let mut keep = hints.len();
    while keep > 0 && cost(keep) > width as usize {
        keep -= 1;
    }
    let mut line = vec![Span::new("── ", Style::role(Role::Rule))];
    for (i, (key, action)) in hints[..keep].iter().enumerate() {
        if i > 0 {
            line.push(Span::label(" · "));
        }
        line.push(Span::emphasis(*key));
        line.push(Span::label(format!(" {action}")));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::store::CardState;
    use crate::daemon::store::PaneTelemetry;
    use crate::sidebar::config::AgentMark;
    use serde_json::json;

    const W: u16 = 44;

    fn claude() -> PaneTelemetry {
        let mut t = PaneTelemetry::with_agent("claude");
        t.card_state = CardState::Running;
        t.cwd = Some("/Users/w/projects/vimeflow".into());
        t.title = Some(json!({"title": "drafting the m0a4 spec"}));
        t.status = Some(json!({"contextWindow": {
            "usedPercentage": 50.0, "contextWindowSize": 1_000_000}}));
        t.tool_call_total = 283;
        t
    }

    fn appearances() -> crate::sidebar::style::AgentAppearances {
        crate::sidebar::agent_ids::CANONICAL_IDS
            .iter()
            .filter_map(|id| {
                crate::sidebar::agent_ids::appearance(id).map(|a| ((*id).to_string(), a))
            })
            .collect()
    }

    /// The one place tests build a context. Mirrors `render()`: width is
    /// clamped, everything else is the config default.
    fn ctx<'a>(app: &'a crate::sidebar::style::AgentAppearances, width: u16) -> CardCtx<'a> {
        CardCtx {
            appearances: app,
            mark: AgentMark::default(),
            tool_calls: crate::sidebar::config::ToolCallStyle::default(),
            trace_lines: 5,
            cwd_label: None,
            width: width.max(MIN_WIDTH),
            selected: false,
            now_unix_ms: 0,
        }
    }

    fn plain(card: &[crate::sidebar::style::Line]) -> Vec<String> {
        card.iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn compact_card_is_exactly_three_lines_at_the_declared_width() {
        let app = appearances();
        let text = plain(&compact_card(&claude(), &ctx(&app, W)));
        assert_eq!(text.len(), 3);
        assert_eq!(text[0], "▸ ● CLAUDE                         ◐ working");
        assert_eq!(text[1], "  vimeflow › drafting the m0a4 spec");
        assert_eq!(text[2], "  ███████░░░░░░░ 50%               283 calls");
        assert_eq!(crate::sidebar::format::width(&text[0]), 44);
        assert_eq!(crate::sidebar::format::width(&text[2]), 44);
    }

    #[test]
    fn at_the_34_column_minimum_the_state_label_drops_and_nothing_wraps() {
        let app = appearances();
        let text = plain(&compact_card(&claude(), &ctx(&app, 34)));
        assert_eq!(
            text[0], "▸ ● CLAUDE                       ◐",
            "glyph survives, label does not"
        );
        assert_eq!(text[1], "  vimeflow › drafting the m0a4 sp…");
        assert_eq!(text[2], "  ████░░░░ 50%           283 calls");
        for line in &text {
            assert!(
                crate::sidebar::format::width(line) <= 34,
                "no line exceeds the pane: {line:?}"
            );
        }
    }

    #[test]
    fn at_54_columns_every_line_fills_exactly_and_the_label_is_back() {
        let app = appearances();
        let text = plain(&compact_card(&claude(), &ctx(&app, 54)));
        assert_eq!(crate::sidebar::format::width(&text[0]), 54);
        assert_eq!(crate::sidebar::format::width(&text[2]), 54);
        assert!(text[0].ends_with("◐ working"));
        assert!(
            text[1].ends_with("drafting the m0a4 spec"),
            "the task no longer truncates"
        );
    }

    #[test]
    fn colliding_cwds_extend_leftward_until_they_differ() {
        let paths = vec![
            ("p1".to_string(), Some("/w/agents/api".to_string())),
            ("p2".to_string(), Some("/w/web/api".to_string())),
            ("p3".to_string(), Some("/w/solo".to_string())),
        ];
        let labels = cwd_labels(&paths);
        assert_eq!(labels["p1"], "agents/api");
        assert_eq!(labels["p2"], "web/api");
        assert_eq!(labels["p3"], "solo", "an unambiguous path stays short");
    }

    #[test]
    fn two_agents_in_one_directory_still_get_distinguishable_labels() {
        // Real pane ids share a workspace prefix, so a LEADING slice would give
        // both cards `·demo` and distinguish nothing (§2.1).
        let paths = vec![
            ("demo:codex".to_string(), Some("/w/vimeflow".to_string())),
            ("demo:claude".to_string(), Some("/w/vimeflow".to_string())),
            ("demo:kimi".to_string(), Some("/w/solo".to_string())),
        ];
        let labels = cwd_labels(&paths);
        assert_eq!(labels["demo:codex"], "vimeflow ·ex");
        assert_eq!(labels["demo:claude"], "vimeflow ·de");
        assert_eq!(
            labels["demo:kimi"], "solo",
            "an unambiguous label never grows a handle"
        );
    }

    #[test]
    fn a_supplied_label_beats_the_cards_own_final_component() {
        let app = appearances();
        let mut cx = ctx(&app, W);
        cx.cwd_label = Some("agents/vimeflow");
        assert_eq!(
            plain(&compact_card(&claude(), &cx))[1],
            "  agents/vimeflow › drafting the m0a4 spec",
            "render() disambiguates across the visible set and the card must honour it"
        );
    }

    #[test]
    fn cwd_renders_its_final_component_when_no_label_was_supplied() {
        let app = appearances();
        assert!(plain(&compact_card(&claude(), &ctx(&app, W)))[1].starts_with("  vimeflow ›"));
    }

    #[test]
    fn a_title_less_agent_shows_cwd_alone_and_still_three_lines() {
        let app = appearances();
        let mut kimi = claude();
        kimi.agent = Some("kimi".into());
        kimi.title = None;
        kimi.cwd = Some("/w/herdr-fork".into());
        let text = plain(&compact_card(&kimi, &ctx(&app, W)));
        assert_eq!(text.len(), 3);
        assert_eq!(text[1], "  herdr-fork", "no dangling separator");
    }

    #[test]
    fn an_unrecognised_agent_keeps_its_own_name_and_gets_no_branding() {
        let app = appearances();
        let mut t = claude();
        t.agent = Some("mystery-cli".into());
        let lines = compact_card(&t, &ctx(&app, W));
        let text = plain(&lines);
        assert!(
            text[0].contains("MYSTERY-CLI"),
            "identity is reported, never guessed"
        );
        assert!(!text[0].contains("CLAUDE"));
        assert!(
            lines[0].iter().all(|s| s.style.rgb.is_none()),
            "and it does not borrow another agent's brand colour"
        );
    }

    #[test]
    fn an_unsanitised_cwd_from_an_older_daemon_cannot_break_the_line_count() {
        let app = appearances();
        let mut t = claude();
        t.cwd = Some("/w/agent\none".into()); // a daemon that predates §2.7
        let text = plain(&compact_card(&t, &ctx(&app, W)));
        assert_eq!(text.len(), 3, "still three lines");
        assert!(text[1].contains("agent one"), "the newline became a space");
    }

    #[test]
    fn an_empty_title_is_treated_as_absent() {
        let app = appearances();
        let mut t = claude();
        t.title = Some(json!({"title": "   "})); // the explicit clear sentinel
        assert_eq!(plain(&compact_card(&t, &ctx(&app, W)))[1], "  vimeflow");
    }

    #[test]
    fn agent_mark_config_reaches_the_header() {
        let app = appearances();
        let mut cx = ctx(&app, W);
        let dot = plain(&compact_card(&claude(), &cx))[0].clone();
        cx.mark = AgentMark::Initial;
        let initial = plain(&compact_card(&claude(), &cx))[0].clone();
        assert!(dot.contains("● CLAUDE"));
        assert!(
            initial.contains("C CLAUDE"),
            "initial mark must actually render"
        );
    }

    #[test]
    fn a_long_configured_label_is_truncated_rather_than_wrapping_the_header() {
        let mut app = appearances();
        app.get_mut("claude").unwrap().label = "A-VERY-LONG-CONFIGURED-AGENT-NAME".into();
        let mut t = claude();
        t.card_state = CardState::Attention; // `! needs you` — the widest state
        let line = plain(&compact_card(&t, &ctx(&app, W)))[0].clone();
        assert_eq!(
            crate::sidebar::format::width(&line),
            44,
            "still exactly one pane wide"
        );
        assert!(line.contains('…'), "and the elision is visible (§2.6)");
        assert!(
            line.ends_with("! needs you"),
            "the state still reaches the right edge"
        );
    }

    #[test]
    fn selection_reverses_the_name_and_nothing_else() {
        let app = appearances();
        let mut cx = ctx(&app, W);
        cx.selected = true;
        let lines = compact_card(&claude(), &cx);

        let reversed: Vec<&str> = lines[0]
            .iter()
            .filter(|s| s.style.reverse)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(
            reversed,
            vec!["CLAUDE"],
            "exactly one reversed run, and it is the name"
        );

        // The mark and the state glyph carry their own colours; reversing them
        // would turn each foreground into a background and break the bar into
        // mismatched blocks. That is the bug this test exists to prevent.
        for span in &lines[0] {
            if span.style.reverse {
                continue;
            }
            assert!(
                !span.style.reverse,
                "coloured spans must not reverse: {:?}",
                span.text
            );
        }
        assert!(
            lines[1].iter().all(|s| !s.style.reverse) && lines[2].iter().all(|s| !s.style.reverse),
            "only the header carries the selection"
        );
    }

    #[test]
    fn an_unselected_card_reverses_nothing() {
        let app = appearances();
        let lines = compact_card(&claude(), &ctx(&app, W));
        assert!(lines.iter().flatten().all(|s| !s.style.reverse));
    }

    #[test]
    fn load_bearing_spans_never_use_the_dim_role() {
        use crate::sidebar::style::Role;
        let app = appearances();
        let lines = compact_card(&claude(), &ctx(&app, W));
        let name = lines[0]
            .iter()
            .find(|s| s.text.contains("CLAUDE"))
            .expect("name span");
        assert_ne!(name.style.role, Role::Label);
        let task = lines[1]
            .iter()
            .find(|s| s.text.contains("drafting"))
            .expect("task span");
        assert_ne!(task.style.role, Role::Label);
    }

    fn claude_expanded() -> PaneTelemetry {
        let mut t = claude();
        t.status = Some(json!({
            "modelDisplayName": "Sonnet 4.5",
            "contextWindow": {
                "usedPercentage": 50.0,
                "contextWindowSize": 1_000_000,
                "currentUsage": {
                    "inputTokens": 347, "cacheCreationInputTokens": 0,
                    "cacheReadInputTokens": 501_300
                }
            },
            "cost": { "totalCostUsd": 1.25 }
        }));
        t.tool_counts = [
            ("Edit".to_string(), 112u64),
            ("Bash".to_string(), 109),
            ("Read".to_string(), 23),
            ("TaskUpdate".to_string(), 11),
            ("Write".to_string(), 10),
            ("TaskCreate".to_string(), 7),
            ("AskUser".to_string(), 5),
            ("Glob".to_string(), 6),
        ]
        .into_iter()
        .collect();
        t
    }

    #[test]
    fn expanded_card_shares_lines_one_and_two_with_compact() {
        let app = appearances();
        let cx = ctx(&app, W);
        let c = plain(&compact_card(&claude_expanded(), &cx));
        let e = plain(&expanded_card(&claude_expanded(), &cx));
        assert!(
            c[0].starts_with('▸') && e[0].starts_with('▾'),
            "the caret reports state"
        );
        assert_eq!(
            c[0].replacen('▸', "▾", 1),
            e[0],
            "and it is the ONLY difference"
        );
        assert_eq!(c[1], e[1]);
        assert_eq!(
            e[2], "  Sonnet 4.5",
            "line 3 diverges: model, not the gauge"
        );
    }

    #[test]
    fn the_model_line_drops_below_36_columns_and_returns_at_36() {
        let app = appearances();
        let at_34 = plain(&expanded_card(&claude_expanded(), &ctx(&app, 34)));
        let at_36 = plain(&expanded_card(&claude_expanded(), &ctx(&app, 36)));
        assert!(
            !at_34.iter().any(|l| l.contains("Sonnet")),
            "no room for the model (§2.7)"
        );
        assert!(at_36.iter().any(|l| l.contains("Sonnet")));
        assert_eq!(
            at_36.len(),
            at_34.len() + 1,
            "exactly one line, not a reflow"
        );
    }

    #[test]
    fn no_expanded_row_exceeds_the_minimum_width() {
        let app = appearances();
        for line in plain(&expanded_card(&claude_expanded(), &ctx(&app, 34))) {
            assert!(
                crate::sidebar::format::width(&line) <= 34,
                "row would wrap at the supported minimum: {line:?}"
            );
        }
    }

    #[test]
    fn a_long_detail_row_truncates_with_an_ellipsis_rather_than_wrapping() {
        let app = appearances();
        let text = plain(&expanded_card(&claude_expanded(), &ctx(&app, 34)));
        let detail = text
            .iter()
            .find(|l| l.contains("cached"))
            .expect("the cache detail row");
        assert!(
            detail.ends_with('…'),
            "elided data must end in an ellipsis (§2.6): {detail:?}"
        );
        assert_eq!(crate::sidebar::format::width(detail), 34);
        assert!(
            text.iter()
                .any(|l| l.contains("500.0k used") && l.contains("500.0k left")),
            "a row that fits is left alone"
        );
    }

    #[test]
    fn expanded_card_shows_the_four_biggest_tools_and_the_remainder() {
        let app = appearances();
        let joined = plain(&expanded_card(&claude_expanded(), &ctx(&app, W))).join("\n");
        assert!(joined.contains("TOOLS   283 calls"));
        assert!(joined.contains("  Edit        112 ████████"));
        assert!(joined.contains("  Bash        109 ████████"));
        assert!(joined.contains("  +4 more tools"));
    }

    #[test]
    fn an_empty_breakdown_with_a_nonzero_total_says_unavailable() {
        let app = appearances();
        let mut t = claude_expanded();
        t.tool_counts.clear();
        let joined = plain(&expanded_card(&t, &ctx(&app, W))).join("\n");
        assert!(joined.contains("283 calls"));
        assert!(joined.contains(crate::sidebar::format::UNAVAILABLE));
    }

    #[test]
    fn nothing_load_bearing_is_allowed_to_dim() {
        use crate::sidebar::style::Role;
        let app = appearances();
        let mut bare = claude_expanded();
        bare.status = Some(json!({}));
        // Line 2 is deliberately excluded: §2.1 dims the cwd *when a task follows
        // it*, because the task is the identifier in that case and the directory
        // is context. That is a considered rule with its own tests in Task 9, not
        // an oversight this one should second-guess. Everything else — metric
        // values, unavailable markers, tool names — is what this test is about.
        let dim_texts: Vec<String> = expanded_card(&bare, &ctx(&app, W))
            .into_iter()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .flat_map(|(_, line)| line)
            .filter(|s| s.style.role == Role::Label)
            .map(|s| s.text.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        for text in &dim_texts {
            assert!(
                ["CONTEXT", "CACHE", "COST", "TOOLS"].contains(&text.as_str())
                    || text.starts_with('+')
                    || text.starts_with('·')
                    || text.starts_with('▾')
                    || text.starts_with('›')
                    || text.chars().all(|c| c == '─'),
                "load-bearing span using the dim role: {text:?}"
            );
        }
        assert!(
            !dim_texts.iter().any(|t| t.starts_with('—')),
            "the unavailable marker IS the value (§2.4a)"
        );

        let named = expanded_card(&claude_expanded(), &ctx(&app, W))
            .into_iter()
            .flatten()
            .find(|s| s.text.starts_with("Edit"))
            .expect("a tool name");
        assert_ne!(
            named.style.role,
            Role::Label,
            "a bar with no name says nothing"
        );
    }

    #[test]
    fn an_agent_that_reports_nothing_draws_a_card_of_the_same_height() {
        let app = appearances();
        let mut bare = claude_expanded();
        bare.status = Some(json!({}));
        assert_eq!(
            plain(&expanded_card(&bare, &ctx(&app, W))).len(),
            plain(&expanded_card(&claude_expanded(), &ctx(&app, W))).len(),
            "height is the contract §3.5 scrolls against; silence must not shrink it"
        );
    }

    #[test]
    fn the_unavailable_marker_has_two_forms_and_both_are_used() {
        // Kimi, not Claude: for Claude a missing metric means an unlanded bridge and
        // draws the other marker, so testing the width rule here would test that
        // instead.
        let app = appearances();
        let mut t = PaneTelemetry::with_agent("kimi");
        t.card_state = CardState::Running;
        t.status = Some(json!({}));
        let wide = plain(&expanded_card(&t, &ctx(&app, 44))).join("\n");
        let narrow = plain(&expanded_card(&t, &ctx(&app, 34))).join("\n");
        assert!(
            wide.contains("— not reported by this agent"),
            "the sentence fits at 44"
        );
        assert!(
            !narrow.contains("not reported"),
            "28 cells do not fit at 34 (§2.6)"
        );
        assert!(narrow.contains('—'), "but the em dash is the invariant");
    }

    /// The status an UNBRIDGED Claude actually carries, built by running the real
    /// decoder over the exact seed `SidecarAdapter::seed_claude_status` writes.
    ///
    /// Do not hand-write this shape. The first version of this test asserted
    /// against `{"modelDisplayName": …}` — something `parse_statusline` never
    /// emits — so it passed green while the live card stayed wrong.
    fn unbridged_status() -> Value {
        let seed = json!({
            "session_id": "s1",
            "transcript_path": "/tmp/s1.jsonl",
            "model": { "id": "unknown", "display_name": "Claude Code" },
        })
        .to_string();
        let event = crate::agent::adapter::claude_code::statusline::parse_statusline("s1", &seed)
            .expect("the seed parses")
            .event;
        let status = serde_json::to_value(&event).expect("the event serialises");
        // Pin the trap itself: the block is PRESENT, only its size is zero. A
        // predicate testing `contextWindow.is_none()` reads this as bridged.
        assert!(
            status.get("contextWindow").is_some(),
            "a defaulted block is still emitted: {status}"
        );
        assert_eq!(status["contextWindow"]["contextWindowSize"], 0);
        status
    }

    #[test]
    fn a_claude_without_the_bridge_says_so_instead_of_blaming_the_agent() {
        // The whole point: four different misconfigurations (PATH, a session that
        // predates the shim, a divergent cwd, a stale pane id) all land here, and
        // "not reported by this agent" sends the reader to the wrong place.
        let app = appearances();
        let mut t = claude_expanded();
        t.status = Some(unbridged_status());
        let card = plain(&expanded_card(&t, &ctx(&app, 44))).join("\n");
        assert!(card.contains("— bridge not connected (README)"), "{card}");
        assert!(
            !card.contains("not reported by this agent"),
            "Claude does report these — the bridge did not land:\n{card}"
        );

        // Said ONCE. All three metrics are missing for the same reason, and five
        // copies of one sentence read as five problems.
        assert_eq!(
            card.matches("bridge not connected").count(),
            1,
            "the explanation is spent on the first empty row only:\n{card}"
        );
        assert!(
            card.contains("CACHE   —") && card.contains("COST    —"),
            "the later rows still mark themselves empty:\n{card}"
        );

        // Narrow falls back to the same em dash as the other marker (§2.6).
        let narrow = plain(&expanded_card(&t, &ctx(&app, 34))).join("\n");
        assert!(!narrow.contains("bridge not connected"));
        assert!(narrow.contains('—'));
    }

    #[test]
    fn the_unbridged_marker_never_overflows_a_pane() {
        // `justify` does NOT truncate — its gap bottoms out at 1 — so a marker
        // wider than the old one can push the compact metrics line past the pane
        // edge. The expanded card is safe (label_row clips), the compact one is
        // not, and a big call count is what closes the last cells.
        let app = appearances();
        let mut t = claude_expanded();
        t.status = Some(unbridged_status());
        t.tool_call_total = 999_999;
        // From 34: `header` overflows below that for EVERY agent, bridged or not
        // (a pre-existing clip bug in the state label, not this marker's).
        for width in 34u16..=80 {
            for line in compact_card(&t, &ctx(&app, width))
                .into_iter()
                .chain(expanded_card(&t, &ctx(&app, width)))
            {
                let drawn: usize = line.iter().map(|s| format::width(&s.text)).sum();
                assert!(
                    drawn <= width as usize,
                    "width {width}: drew {drawn} cells: {:?}",
                    plain(&[line])
                );
            }
        }
    }

    #[test]
    fn a_bridged_claude_is_never_labelled_unbridged() {
        let app = appearances();
        let card = plain(&expanded_card(&claude_expanded(), &ctx(&app, 44))).join("\n");
        assert!(!card.contains("bridge not connected"), "{card}");
        assert!(card.contains("COST"), "the real metrics render: {card}");
    }

    #[test]
    fn a_non_claude_agent_is_never_labelled_unbridged() {
        // Kimi, Codex and OpenCode read their own transcripts; they have no bridge
        // to be missing, so the marker would be meaningless there.
        let app = appearances();
        for agent in ["kimi", "codex", "opencode"] {
            let mut t = PaneTelemetry::with_agent(agent);
            t.card_state = CardState::Running;
            t.status = Some(json!({}));
            let card = plain(&expanded_card(&t, &ctx(&app, 44))).join("\n");
            assert!(!card.contains("bridge not connected"), "{agent}: {card}");
            assert!(
                card.contains("not reported by this agent"),
                "{agent}: {card}"
            );
        }
    }

    #[test]
    fn zero_calls_is_not_the_same_as_unavailable() {
        let app = appearances();
        let mut t = claude_expanded();
        t.tool_counts.clear();
        t.tool_call_total = 0;
        let joined = plain(&expanded_card(&t, &ctx(&app, W))).join("\n");
        assert!(joined.contains("TOOLS   0 calls"));
        assert!(
            !joined.contains("— not reported"),
            "nothing is missing, so nothing is marked"
        );
    }

    #[test]
    fn cache_uses_the_codex_denominator_for_codex() {
        let app = appearances();
        let mut t = claude_expanded();
        t.agent = Some("codex".into());
        t.status = Some(json!({"contextWindow": {"usedPercentage": 10.0,
            "contextWindowSize": 200_000,
            "currentUsage": {"inputTokens": 9000, "cacheCreationInputTokens": 0,
                             "cacheReadInputTokens": 7000}}}));
        let joined = plain(&expanded_card(&t, &ctx(&app, W))).join("\n");
        assert!(joined.contains("78%"), "subset denominator, not 44%");
    }

    /// Fixtures speak the wire format, not a convenient one.
    fn iso(unix_ms: u64) -> String {
        chrono::DateTime::from_timestamp_millis(unix_ms as i64)
            .expect("in range")
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    #[test]
    fn traces_show_settled_calls_with_glyph_and_age() {
        let app = appearances();
        let mut t = claude_expanded();
        let now = 1_000_000_000u64;
        t.tool_calls = vec![
            json!({"tool":"BASH","args":"cargo test","status":"done",
                   "timestamp": iso(now - 720_000)}),
            json!({"tool":"BASH","args":"npm build","status":"failed",
                   "timestamp": iso(now - 780_000)}),
        ]
        .into();
        let mut cx = ctx(&app, W);
        cx.now_unix_ms = now;
        let joined = plain(&expanded_card(&t, &cx)).join("\n");
        assert!(joined.contains("▾ TRACES 2 retained"));
        assert!(joined.contains("  ✓ BASH  cargo test"));
        assert!(joined.contains("  ✕ BASH  npm build"));
        assert!(joined.contains("12m"));
    }

    #[test]
    fn traces_respect_trace_lines_and_report_the_remainder() {
        let app = appearances();
        let mut t = claude_expanded();
        let now = 2_000_000_000u64;
        t.tool_calls = (0..10)
            .map(|i| {
                json!({"tool":"EDIT","args":format!("file{i}"),
                            "status":"done","timestamp": iso(now - 60_000)})
            })
            .collect::<Vec<_>>()
            .into();
        let mut cx = ctx(&app, W);
        cx.now_unix_ms = now;
        let joined = plain(&expanded_card(&t, &cx)).join("\n");
        assert!(joined.contains("▾ TRACES 10 retained"));
        assert_eq!(
            joined.matches("EDIT").count(),
            5,
            "trace_lines caps displayed rows"
        );
        assert!(joined.contains("+5 older"));
    }

    #[test]
    fn traces_parse_iso8601_timestamps() {
        let app = appearances();
        let mut t = claude_expanded();
        // 2026-08-12T00:12:00Z is 12 minutes before 2026-08-12T00:24:00Z
        let now = crate::sidebar::format::parse_iso8601_ms("2026-08-12T00:24:00Z").unwrap();
        t.tool_calls = vec![json!({"tool":"BASH","args":"cargo test","status":"done",
                                   "timestamp":"2026-08-12T00:12:00Z"})]
        .into();
        let mut cx = ctx(&app, W);
        cx.now_unix_ms = now;
        let joined = plain(&expanded_card(&t, &cx)).join("\n");
        assert!(
            joined.contains("12m"),
            "ISO-8601 strings must parse, not fall back to —"
        );
    }

    #[test]
    fn a_numeric_timestamp_is_not_silently_accepted() {
        let app = appearances();
        let mut t = claude_expanded();
        t.tool_calls = vec![json!({"tool":"BASH","args":"x","status":"done",
                                   "timestamp": 1_000_000_000u64})]
        .into();
        let mut cx = ctx(&app, W);
        cx.now_unix_ms = 1_000_720_000;
        let joined = plain(&expanded_card(&t, &cx)).join("\n");
        assert!(
            joined.contains(crate::sidebar::format::UNAVAILABLE),
            "an off-contract shape reads as unknown, never as a plausible age"
        );
    }

    #[test]
    fn a_trace_age_is_never_dim() {
        use crate::sidebar::style::Role;
        let app = appearances();
        let mut t = claude_expanded();
        let now = 4_000_000_000u64;
        t.tool_calls = vec![json!({"tool":"BASH","args":"x","status":"done",
                                   "timestamp": iso(now - 720_000)})]
        .into();
        let mut cx = ctx(&app, W);
        cx.now_unix_ms = now;
        let age = expanded_card(&t, &cx)
            .into_iter()
            .flatten()
            .find(|s| s.text.trim() == "12m")
            .expect("the age span");
        assert_ne!(
            age.style.role,
            Role::Label,
            "the reader acts on this number (§2.5)"
        );
    }

    #[test]
    fn a_missing_timestamp_renders_an_em_dash_not_a_fake_age() {
        let app = appearances();
        let mut t = claude_expanded();
        t.tool_calls = vec![json!({"tool":"BASH","args":"x","status":"done"})].into();
        let mut cx = ctx(&app, W);
        cx.now_unix_ms = 9_999;
        let joined = plain(&expanded_card(&t, &cx)).join("\n");
        assert!(joined.contains(crate::sidebar::format::UNAVAILABLE));
    }

    #[test]
    fn a_trace_argument_with_no_room_left_is_dropped_whole() {
        let app = appearances();
        let mut t = claude_expanded();
        let now = 3_000_000_000u64;
        t.tool_calls = vec![json!({"tool":"BASH",
            "args":"cargo test --workspace --all-features -- --nocapture",
            "status":"done","timestamp": iso(now - 60_000)})]
        .into();
        let mut cx = ctx(&app, 34);
        cx.now_unix_ms = now;
        let row = plain(&expanded_card(&t, &cx))
            .into_iter()
            .find(|l| l.contains("BASH"))
            .expect("the trace row");
        assert!(crate::sidebar::format::width(&row) <= 34);
        assert!(
            row.contains("1m"),
            "identity and age survive; prose is what gets cut (§2.7)"
        );
    }

    fn jar(t: &PaneTelemetry, width: u16) -> Vec<String> {
        let app = appearances();
        let mut cx = ctx(&app, width);
        cx.tool_calls = crate::sidebar::config::ToolCallStyle::Jar;
        plain(&tool_rows(t, &cx))
    }

    #[test]
    fn jar_is_always_two_lines_and_its_band_matches_its_legend() {
        let rows = jar(&claude_expanded(), W);
        assert_eq!(
            rows.len(),
            2,
            "jar is exactly two lines — that is why it exists"
        );
        for tool in ["Edit", "Bash"] {
            assert!(rows[1].contains(tool), "{tool} in legend");
        }
        assert_eq!(
            crate::sidebar::format::width(&rows[0]),
            W as usize,
            "two cells of indent plus a band that fills the rest exactly"
        );
    }

    #[test]
    fn jar_narrows_its_legend_and_the_band_follows() {
        let rows = jar(&claude_expanded(), 34);
        assert!(crate::sidebar::format::width(&rows[1]) <= 34);
        assert!(
            rows[1].contains('+'),
            "omitted tools are reported, not dropped silently"
        );
        assert_eq!(crate::sidebar::format::width(&rows[0]), 34);
    }

    #[test]
    fn jar_with_no_calls_says_zero_not_unavailable() {
        let mut t = claude_expanded();
        t.tool_counts.clear();
        t.tool_call_total = 0;
        let rows = jar(&t, W);
        assert!(rows[1].contains("0 calls"));
        assert!(!rows[1].contains(crate::sidebar::format::UNAVAILABLE));
    }

    #[test]
    fn a_breakdown_of_nothing_but_zeroes_reads_as_zero_in_both_modes() {
        let app = appearances();
        let mut t = claude_expanded();
        t.tool_counts = [("Edit".to_string(), 0u64), ("Bash".to_string(), 0)]
            .into_iter()
            .collect();
        t.tool_call_total = 0;

        let bars = plain(&tool_rows(&t, &ctx(&app, W)));
        assert!(
            !bars.iter().any(|l| l.contains('█')),
            "no bar for no calls: {bars:?}"
        );

        let mut jar_cx = ctx(&app, W);
        jar_cx.tool_calls = crate::sidebar::config::ToolCallStyle::Jar;
        let jar = plain(&tool_rows(&t, &jar_cx));
        assert!(jar[1].contains("0 calls"));
        assert!(
            !jar[0].contains('█'),
            "an empty track, not a full band: {:?}",
            jar[0]
        );
    }

    #[test]
    fn a_single_over_long_tool_name_never_eats_the_omitted_count() {
        let app = appearances();
        let mut t = claude_expanded();
        t.tool_counts = [
            (
                "A-RIDICULOUSLY-LONG-TOOL-NAME-FROM-SOME-MCP-SERVER".to_string(),
                90u64,
            ),
            ("Bash".to_string(), 5),
            ("Read".to_string(), 3),
        ]
        .into_iter()
        .collect();
        let mut cx = ctx(&app, 34);
        cx.tool_calls = crate::sidebar::config::ToolCallStyle::Jar;
        let rows = plain(&tool_rows(&t, &cx));
        assert_eq!(rows.len(), 2);
        assert!(
            rows[1].contains("+2 more"),
            "the count survives the name, not the other way round: {:?}",
            rows[1]
        );
        assert!(
            rows[1].contains("90"),
            "and the tool's OWN count survives its name too (§2.7): {:?}",
            rows[1]
        );
        assert!(rows[1].contains('…'), "which means the name is what elided");
        assert!(crate::sidebar::format::width(&rows[1]) <= 34);
    }

    #[test]
    fn the_jar_legend_never_dims_a_tool_name_or_a_count() {
        use crate::sidebar::style::Role;
        let app = appearances();
        let mut cx = ctx(&app, W);
        cx.tool_calls = crate::sidebar::config::ToolCallStyle::Jar;
        let rows = tool_rows(&claude_expanded(), &cx);
        let dim: Vec<&str> = rows[1]
            .iter()
            .filter(|s| s.style.role == Role::Label)
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            dim.iter().all(|t| t.trim() == "·" || t.starts_with('+')),
            "only separators and the omitted-count may dim: {dim:?}"
        );
        assert!(
            rows[1]
                .iter()
                .any(|s| s.text.contains("Edit") && s.style.role != Role::Label),
            "the band is uninterpretable without its names"
        );
    }

    #[test]
    fn jar_with_a_total_but_no_breakdown_says_unavailable() {
        let mut t = claude_expanded();
        t.tool_counts.clear();
        let rows = jar(&t, W);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].contains(crate::sidebar::format::UNAVAILABLE));
    }

    #[test]
    fn jar_cells_round_and_place_the_remainder_on_the_first_tool() {
        let table: &[(&[u64], usize, &[usize])] = &[
            (&[1], 32, &[32]),
            (&[1, 1], 32, &[16, 16]),
            (&[1, 1], 33, &[16, 17]),
            (&[3, 3, 3], 32, &[10, 11, 11]),
            (&[5, 1], 32, &[27, 5]),
            (&[112, 109, 23, 11], 42, &[18, 18, 4, 2]),
        ];
        for (counts, inner, want) in table {
            let got = jar_cells(counts, *inner);
            assert_eq!(&got, want, "counts {counts:?} at inner {inner}");
            assert_eq!(
                got.iter().sum::<usize>(),
                *inner,
                "the band always fills exactly"
            );
        }
    }

    #[test]
    fn jar_cells_still_fill_exactly_when_every_share_rounds_up() {
        let got = jar_cells(&[1, 1, 1, 1], 2);
        assert_eq!(got.iter().sum::<usize>(), 2);
    }

    fn view_input<'a>(
        cursor: Option<&'a str>,
        toggled: &'a std::collections::HashSet<String>,
        agents: &'a crate::sidebar::style::AgentAppearances,
    ) -> ViewInput<'a> {
        ViewInput {
            cursor,
            toggled,
            hide_idle: false,
            sort: crate::sidebar::select::Sort::default(),
            auto_expand: crate::sidebar::config::AutoExpand::default(),
            agent_mark: AgentMark::default(),
            tool_calls: crate::sidebar::config::ToolCallStyle::default(),
            theme: crate::sidebar::config::Theme::default(),
            trace_lines: 5,
            agents,
            config: crate::sidebar::style::ConfigStatus::default(),
        }
    }

    #[test]
    fn render_pins_the_footer_and_reports_card_spans() {
        let mut panes = std::collections::HashMap::new();
        panes.insert("p1".to_string(), claude());
        let state = crate::sidebar::reducer::State { panes, last_seq: 1 };
        let app = appearances();
        let toggled = std::collections::HashSet::new();
        let view = view_input(Some("p1"), &toggled, &app);
        let out = render(&state, &view, W, 0);

        assert_eq!(out.spans.len(), 1);
        assert_eq!(
            out.span_for("p1"),
            Some(crate::sidebar::layout::LineSpan {
                start: 0,
                height: 3
            }),
            "the first card starts on the first line — there is no global header"
        );
        assert!(out
            .pinned
            .last()
            .expect("footer")
            .iter()
            .any(|s| s.text.contains("expand")));
    }

    #[test]
    fn the_whole_rendered_value_is_a_golden() {
        let app = appearances();
        let mut panes = std::collections::HashMap::new();
        panes.insert("p1".to_string(), claude());
        let mut second = claude();
        second.agent = Some("codex".into());
        second.cwd = Some("/w/herdr-fork".into());
        second.title = None;
        second.tool_call_total = 4;
        panes.insert("p2".to_string(), second);
        let state = crate::sidebar::reducer::State { panes, last_seq: 2 };
        let toggled = std::collections::HashSet::new();
        let out = render(&state, &view_input(Some("p1"), &toggled, &app), W, 0);

        assert_eq!(
            out.scrollable
                .iter()
                .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
                .collect::<Vec<_>>(),
            vec![
                "▸ ● CLAUDE                         ◐ working",
                "  vimeflow › drafting the m0a4 spec",
                "  ███████░░░░░░░ 50%               283 calls",
                "────────────────────────────────────────────",
                "▸ ● CODEX                          ◐ working",
                "  herdr-fork",
                "  ███████░░░░░░░ 50%                 4 calls",
            ],
            "Smart order ends in pane_id ascending, so equal states break p1 then p2"
        );
        assert_eq!(
            out.pinned
                .iter()
                .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
                .collect::<Vec<_>>(),
            vec!["── j/k move · o/↵ expand · z idle"],
            "no config notice, no idle notice, footer last"
        );
        assert_eq!(
            out.spans,
            vec![
                (
                    "p1".to_string(),
                    crate::sidebar::layout::LineSpan {
                        start: 0,
                        height: 3
                    }
                ),
                (
                    "p2".to_string(),
                    crate::sidebar::layout::LineSpan {
                        start: 4,
                        height: 3
                    }
                ),
            ]
        );
    }

    #[test]
    fn render_reports_the_real_height_of_every_card_it_drew() {
        let app = appearances();
        let mut panes = std::collections::HashMap::new();
        panes.insert("p1".to_string(), claude());
        panes.insert("p2".to_string(), claude_expanded());
        let state = crate::sidebar::reducer::State { panes, last_seq: 2 };
        let mut toggled = std::collections::HashSet::new();
        toggled.insert("p2".to_string());
        let v = view_input(Some("p1"), &toggled, &app);
        let out = render(&state, &v, W, 0);

        assert_eq!(out.spans.len(), 2, "one span per visible card");
        let p1 = out.span_for("p1").expect("p1");
        let p2 = out.span_for("p2").expect("p2");
        assert_eq!(p1.height, 3, "a compact card is exactly three lines");
        let (above, below) = if p1.start < p2.start {
            (p1, p2)
        } else {
            (p2, p1)
        };
        assert_eq!(
            below.start,
            above.start + above.height + 1,
            "one rule line between cards"
        );
        assert_eq!(
            p2.height,
            expanded_card(&claude_expanded(), &ctx(&app, W)).len(),
            "the reported height IS the card's height — §3.5 scrolls against it"
        );
        assert!(
            p2.start + p2.height <= out.scrollable.len(),
            "no span points past the region it indexes"
        );
    }

    #[test]
    fn only_separators_and_gutters_use_roles_that_are_allowed_to_dim() {
        use crate::sidebar::style::Role;
        let mut panes = std::collections::HashMap::new();
        panes.insert("p1".to_string(), claude());
        panes.insert("p2".to_string(), claude());
        let state = crate::sidebar::reducer::State { panes, last_seq: 2 };
        let app = appearances();
        let toggled = std::collections::HashSet::new();
        let out = render(&state, &view_input(None, &toggled, &app), W, 0);
        let rules: Vec<_> = out
            .scrollable
            .iter()
            .flatten()
            .filter(|s| s.style.role == Role::Rule)
            .collect();
        assert!(!rules.is_empty(), "cards are separated by a rule (§2.4)");
        assert!(
            rules.iter().all(|s| s.text.chars().all(|c| c == '─')),
            "the Rule role is for separators only, never for data"
        );
    }

    #[test]
    fn a_pane_narrower_than_the_minimum_clamps_instead_of_underflowing() {
        let mut panes = std::collections::HashMap::new();
        panes.insert("p1".to_string(), claude());
        let state = crate::sidebar::reducer::State { panes, last_seq: 1 };
        let app = appearances();
        let toggled = std::collections::HashSet::new();
        let v = view_input(Some("p1"), &toggled, &app);
        assert_eq!(
            render(&state, &v, 12, 0).plain(),
            render(&state, &v, 34, 0).plain()
        );
    }

    #[test]
    fn sibling_worktrees_get_distinguishable_labels_on_screen() {
        let mut panes = std::collections::HashMap::new();
        let mut a = claude();
        a.cwd = Some("/w/agents/api".into());
        let mut b = claude();
        b.agent = Some("codex".into());
        b.cwd = Some("/w/web/api".into());
        panes.insert("p1".to_string(), a);
        panes.insert("p2".to_string(), b);
        let state = crate::sidebar::reducer::State { panes, last_seq: 2 };
        let app = appearances();
        let toggled = std::collections::HashSet::new();
        let text = render(&state, &view_input(None, &toggled, &app), W, 0)
            .plain()
            .join("\n");
        assert!(
            text.contains("agents/api"),
            "disambiguation must reach the cards"
        );
        assert!(text.contains("web/api"));
    }

    #[test]
    fn the_footer_reads_exactly_as_specified_and_drops_from_the_end() {
        assert_eq!(plain(&[footer(44)])[0], "── j/k move · o/↵ expand · z idle");
        assert_eq!(plain(&[footer(34)])[0], "── j/k move · o/↵ expand · z idle");
        assert_eq!(plain(&[footer(30)])[0], "── j/k move · o/↵ expand");
        assert_eq!(plain(&[footer(12)])[0], "── j/k move");
    }

    #[test]
    fn render_shows_a_quiet_hint_when_nothing_is_bound() {
        let state = crate::sidebar::reducer::State::default();
        let app = appearances();
        let toggled = std::collections::HashSet::new();
        let out = render(&state, &view_input(None, &toggled, &app), W, 0);
        assert!(out.plain().join("\n").to_lowercase().contains("no agents"));
    }

    #[test]
    fn hidden_idle_agents_are_announced_in_the_pinned_region() {
        let mut panes = std::collections::HashMap::new();
        let mut idle = claude();
        idle.card_state = CardState::Idle;
        panes.insert("p9".to_string(), idle);
        let state = crate::sidebar::reducer::State { panes, last_seq: 1 };
        let app = appearances();
        let toggled = std::collections::HashSet::new();
        let mut v = view_input(None, &toggled, &app);
        v.hide_idle = true;
        let out = render(&state, &v, W, 0);
        assert!(out
            .pinned
            .iter()
            .any(|l| l.iter().any(|s| s.text.contains("+1 idle"))));
        assert!(
            !out.plain().join("\n").to_lowercase().contains("no agents"),
            "filtered is not empty — the hint would contradict the count beside it (§3.6)"
        );
    }

    #[test]
    fn config_problems_render_a_pinned_notice() {
        let state = crate::sidebar::reducer::State::default();
        let app = appearances();
        let toggled = std::collections::HashSet::new();
        let mut v = view_input(None, &toggled, &app);
        v.config = crate::sidebar::style::ConfigStatus {
            problems: 2,
            log_written: true,
        };
        let out = render(&state, &v, W, 0);
        assert!(out
            .pinned
            .iter()
            .any(|l| l.iter().any(|s| s.text.contains("2 problems"))));
    }
}
