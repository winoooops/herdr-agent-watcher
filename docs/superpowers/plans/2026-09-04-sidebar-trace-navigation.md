# Sidebar Trace Navigation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Trace interaction in the sidebar — `l`/`h` descend into and return from an expanded card's traces, `j`/`k` walk settled calls (full ring while focused), `o`/Enter and mouse click-again open a frozen detail panel showing the retained args preview with width-aware scrolling.

**Architecture:** Focus is a pane-scoped pair (`it.cursor` card + `trace_focus` toolUseId). The view exports per-row trace spans beside card spans; pure hit-tests resolve clicks; keys and mouse express panel-opening and zone-entry as data (`OpenTrace` / `EnterTraces` outcomes) that the run loop — the only owner of `State` and the dialog slot — resolves, snapshots, and installs. `TraceDetail` is a fully frozen panel scrolled by rendered-line count.

**Tech Stack:** Rust (edition 2021), crossterm 0.29, ratatui 0.30 — all in-tree. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-01-sidebar-trace-navigation-design.md` — read it first; every contract below cites it.

## Global Constraints

- Scope: `sidebar` subcommand only; **no daemon changes; no `src/agent/**` changes** (spec header).
- Rust floor `rust-version = "1.88"`; no new dependencies; `cargo fmt` before every commit (CI enforces `--check`); no new clippy warnings.
- Conventional commits (`feat(sidebar): …`) as in `git log`.
- Ring payload keys (serde camelCase of `AgentToolCallEvent`): `toolUseId`, `tool`, `args`, `status`, `timestamp` (ISO-8601 string), `durationMs`.
- **Selectable row** = ring entry with a `toolUseId` string AND `status` exactly `"done"` or `"failed"` (spec §2). Everything else is display-only.
- The selection identity is always the pair (card id, tool_use_id) — never the id alone (spec §2).
- All tests inline `#[cfg(test)]`; no e2e-tier changes; no TS-bindings impact.

---

### Task 1: Compact duration formatter

**Files:**
- Modify: `src/sidebar/format.rs` (add `duration_ms` near `age`; tests in the existing `mod tests`)

**Interfaces:**
- Produces: `pub fn duration_ms(ms: u64) -> String` — `0..1000 → "NNNms"`, `1s..60s → one-decimal "N.Ns"`, `≥60s → "MmSSs"` (spec §4). Task 8's snapshot builder consumes it.

- [ ] **Step 1: Write the failing test** (append in `format.rs`'s `mod tests`):

```rust
    #[test]
    fn duration_ms_has_three_regimes_with_exact_boundaries() {
        assert_eq!(duration_ms(0), "0ms");
        assert_eq!(duration_ms(999), "999ms");
        assert_eq!(duration_ms(1000), "1.0s");
        assert_eq!(duration_ms(1234), "1.2s");
        assert_eq!(duration_ms(59_949), "59.9s");
        assert_eq!(duration_ms(60_000), "1m00s");
        assert_eq!(duration_ms(125_000), "2m05s");
    }
```

- [ ] **Step 2: Run to verify failure** — `cargo test sidebar::format::tests::duration_ms_has` — FAIL to compile (`cannot find function duration_ms`).

- [ ] **Step 3: Implement** (above `mod tests`):

```rust
/// Durations, not ages: `age` reports sub-minute deltas as "now", which
/// erases every real tool-call duration (spec §4).
pub fn duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", (ms / 100) as f64 / 10.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}
```

- [ ] **Step 4: Run to verify pass** — `cargo test sidebar::format` — PASS.
- [ ] **Step 5: Commit** — `cargo fmt && git add src/sidebar/format.rs && git commit -m "feat(sidebar): compact duration formatter"`

---

### Task 2: Pure trace hit-test

**Files:**
- Modify: `src/sidebar/layout.rs` (add `trace_at` beside `card_at`; tests in the existing module)

**Interfaces:**
- Consumes: `LineSpan`.
- Produces: `pub fn trace_at(spans: &[(String, String, LineSpan)], line: usize) -> Option<(&str, &str)>` returning `(card_id, tool_use_id)`. Task 5 calls it before `card_at`.

- [ ] **Step 1: Write the failing test:**

```rust
    #[test]
    fn trace_at_maps_lines_to_rows() {
        let spans = vec![
            ("a".to_string(), "t1".to_string(), LineSpan { start: 5, height: 1 }),
            ("a".to_string(), "t2".to_string(), LineSpan { start: 6, height: 1 }),
        ];
        assert_eq!(trace_at(&spans, 5), Some(("a", "t1")));
        assert_eq!(trace_at(&spans, 6), Some(("a", "t2")));
        assert_eq!(trace_at(&spans, 4), None, "line before the rows");
        assert_eq!(trace_at(&spans, 7), None, "line past the rows");
        assert_eq!(trace_at(&[], 5), None, "empty spans");
    }
```

- [ ] **Step 2: Verify failure** — `cargo test sidebar::layout::tests::trace_at_maps` — FAIL to compile.

- [ ] **Step 3: Implement:**

```rust
/// The trace row containing `line`, as its (card, toolUseId) pair (spec
/// §3). More specific than `card_at`: callers test this first.
pub fn trace_at(spans: &[(String, String, LineSpan)], line: usize) -> Option<(&str, &str)> {
    spans.iter().find_map(|(card, id, span)| {
        (line >= span.start && line < span.start + span.height)
            .then(|| (card.as_str(), id.as_str()))
    })
}
```

- [ ] **Step 4: Verify pass** — `cargo test sidebar::layout` — PASS.
- [ ] **Step 5: Commit** — `cargo fmt && git add src/sidebar/layout.rs && git commit -m "feat(sidebar): pure trace-row hit-test"`

---

### Task 3: View exports trace geometry and renders focus

**Files:**
- Modify: `src/sidebar/style.rs` — `Rendered` gains `trace_spans: Vec<(String, String, LineSpan)>` and `trace_span_for`.
- Modify: `src/sidebar/view.rs` — `ViewInput` gains `trace_focus: Option<(&'a str, &'a str)>`; `trace_rows` returns row metadata and honors focus (full ring, reversed selected row, dedup, display-only); `expanded_card` and `render` thread the metadata into absolute spans.
- Tests: `src/sidebar/view.rs`'s existing `mod tests`.

**Interfaces:**
- Consumes: `LineSpan`.
- Produces (later tasks rely on these exactly):
  - `Rendered.trace_spans: Vec<(String, String, LineSpan)>` — selectable rendered rows only (id-bearing AND status done/failed), newest-first, deduped by id (newest occurrence wins), full ring for the focused card, `trace_lines` window otherwise.
  - `Rendered.trace_span_for(card: &str, id: &str) -> Option<LineSpan>` (same shape as `span_for`).
  - `ViewInput.trace_focus: Option<(&'a str, &'a str)>` — (card id, tool id); the matching row renders `reverse: true` like the card cursor.

**Implementation notes (exact mechanics):**
- `trace_rows` signature becomes `fn trace_rows(t: &PaneTelemetry, cx: &CardCtx<'_>) -> (Vec<Line>, Vec<(String, usize)>)` — the second element is `(tool_use_id, line_offset_within_returned_lines)` for each **selectable** row (offset 0 is the `▾ TRACES` header, so first data row is offset 1).
- Row iteration: build the newest-first sequence with dedup first —
  ```rust
  let mut seen = std::collections::HashSet::new();
  let calls: Vec<&Value> = t
      .tool_calls
      .iter()
      .rev()
      .filter(|call| match call.get("toolUseId").and_then(Value::as_str) {
          Some(id) => seen.insert(id.to_string()),
          None => true, // id-less rows render (display-only), never dedup
      })
      .collect();
  ```
  then take `calls.iter().take(window)` where `window` is `usize::MAX` when `cx` says this card is trace-focused, else `cx.trace_lines as usize`.
- Selectable check per row: `let selectable = call.get("toolUseId").and_then(Value::as_str).is_some() && matches!(call.get("status").and_then(Value::as_str), Some("done") | Some("failed"));` — push `(id, offset)` metadata only when selectable.
- `CardCtx` gains `trace_focus: Option<&str>` (the selected id, already scoped to this card by the caller) — when `Some(id)` matches a row's id, wrap every span of that row with `reverse: true` styling exactly the way the card cursor row does (find the existing reversed-row treatment in `view.rs` and reuse the same `Style` mutation).
- `expanded_card` records `let traces_at = lines.len();` before `lines.extend(trace_rows(...).0)` and returns the metadata alongside; `render` (at the existing `let start = scrollable.len();` / `spans.push((...))` site, `view.rs:907–928`) converts each `(id, offset)` into `(pane_id.clone(), id, LineSpan { start: start + traces_at + offset, height: 1 })` pushed onto `trace_spans`. Collapsed cards contribute nothing.
- `render`'s `ViewInput.trace_focus` is consulted twice: (a) the focused card renders the full ring; (b) that card's `CardCtx.trace_focus = Some(id)`.
- The final `Rendered { scrollable, pinned, spans }` literal gains `trace_spans`. **Every `Rendered { … }` literal in the tree gains the field** — `grep -rn "Rendered {" src/` and add `trace_spans: Vec::new()` everywhere EXCEPT `two_cards()` in `tui.rs`, which gets exactly these two rows inside card `a` (Tasks 5–7's click coordinates depend on them):

```rust
            trace_spans: vec![
                (
                    "a".into(),
                    "t-new".into(),
                    LineSpan {
                        start: 1,
                        height: 1,
                    },
                ),
                (
                    "a".into(),
                    "t-old".into(),
                    LineSpan {
                        start: 2,
                        height: 1,
                    },
                ),
            ],
```

- [ ] **Step 1: Write the failing tests** (in `view.rs`'s `mod tests`; use the module's existing telemetry-building helpers — there are tests constructing `PaneTelemetry` with `tool_calls` today, e.g. the `traces_show_settled_calls_with_glyph_and_age` fixture pattern):

```rust
    fn call(id: Option<&str>, status: &str) -> Value {
        let mut v = serde_json::json!({
            "tool": "Bash", "args": "cargo test", "status": status,
            "timestamp": "2026-09-01T10:00:00.000Z", "durationMs": 1200,
        });
        if let Some(id) = id {
            v["toolUseId"] = Value::String(id.into());
        }
        v
    }

    #[test]
    fn trace_spans_export_selectable_rows_only() {
        let mut t = PaneTelemetry::with_agent("claude");
        t.card_state = CardState::Running;
        t.tool_calls.push_back(call(Some("t-old"), "done"));
        t.tool_calls.push_back(call(None, "done"));          // id-less
        t.tool_calls.push_back(call(Some("t-run"), "running")); // non-settled
        t.tool_calls.push_back(call(Some("t-new"), "failed"));
        let rendered = render_one_expanded("p1", t, None);
        let ids: Vec<&str> = rendered
            .trace_spans
            .iter()
            .map(|(_, id, _)| id.as_str())
            .collect();
        assert_eq!(ids, vec!["t-new", "t-old"], "newest-first, selectable only");
        assert!(rendered.trace_span_for("p1", "t-new").is_some());
        assert!(rendered.trace_span_for("p1", "t-run").is_none());
    }

    #[test]
    fn duplicate_ids_render_once_newest_wins() {
        let mut t = PaneTelemetry::with_agent("claude");
        t.card_state = CardState::Running;
        t.tool_calls.push_back(call(Some("dup"), "done"));
        t.tool_calls.push_back(call(Some("dup"), "failed"));
        let rendered = render_one_expanded("p1", t, None);
        let dups = rendered
            .trace_spans
            .iter()
            .filter(|(_, id, _)| id == "dup")
            .count();
        assert_eq!(dups, 1);
    }

    #[test]
    fn focus_renders_the_full_ring_and_reverses_the_selected_row() {
        let mut t = PaneTelemetry::with_agent("claude");
        t.card_state = CardState::Running;
        for i in 0..10 {
            t.tool_calls.push_back(call(Some(&format!("t{i}")), "done"));
        }
        // Unfocused: trace_lines (default 5) window.
        let windowed = render_one_expanded("p1", t.clone(), None);
        assert_eq!(windowed.trace_spans.len(), 5);
        // Focused: all 10, and the selected row is reversed.
        let focused = render_one_expanded("p1", t, Some(("p1", "t3")));
        assert_eq!(focused.trace_spans.len(), 10);
        let span = focused.trace_span_for("p1", "t3").expect("selected row");
        let row = &focused.scrollable[span.start];
        assert!(
            row.iter().any(|s| s.style.reverse),
            "selected trace row renders reversed"
        );
    }
```

`render_one_expanded(id, telemetry, trace_focus)` is a small test helper to add beside the module's existing render helpers: it builds the state map with one pane, a `ViewInput` with `cursor: Some(id)`, that pane toggled expanded (or `auto_expand: All`), `trace_lines: 5`, the given `trace_focus`, and calls `render` at width 80 — mirror the existing render-test setup in this module (reuse its `AgentAppearances`/`ConfigStatus` scaffolding).

- [ ] **Step 2: Verify failure** — `cargo test sidebar::view::tests::trace_spans_export` — FAIL to compile (`no field trace_spans`).
- [ ] **Step 3: Implement** per the notes above (style.rs field + `trace_span_for`; view.rs threading; fix every `Rendered` literal the grep finds).
- [ ] **Step 4: Verify pass** — `cargo test sidebar::view && cargo test sidebar::tui` (the latter proves fixture literals were all updated) — PASS.
- [ ] **Step 5: Commit** — `cargo fmt && git add -A src/sidebar && git commit -m "feat(sidebar): export trace-row geometry and render trace focus"`

---

### Task 4: Trace-focus state and reconcile

**Files:**
- Modify: `src/sidebar/tui.rs` — `Interaction` gains `trace_focus: Option<String>`; new pure `reconcile_trace_focus`; call it in the draw block immediately after the existing cursor reconcile (the code that computes `recovered`); tests.

**Interfaces:**
- Consumes: `Rendered.trace_spans` (Task 3).
- Produces: `Interaction.trace_focus: Option<String>`; `fn reconcile_trace_focus(cursor: &Option<String>, trace_focus: &mut Option<String>, previous: &[(String, String, LineSpan)], current: &[(String, String, LineSpan)]) -> bool` (returns "changed", which ORs into `dirty`). Rules (spec §2): focus survives only if the pair `(cursor, id)` exists in `current`; a vanished id with surviving rows on the same card snaps to the row nearest the id's PREVIOUS rendered position on the newest side (index `min(prev_index, current_card_rows - 1)` over that card's rows in `previous`/`current`); no surviving rows on the card, or no/changed cursor anchor → `None`.

- [ ] **Step 1: Write the failing tests:**

```rust
    fn tspan(card: &str, id: &str, start: usize) -> (String, String, LineSpan) {
        (card.into(), id.into(), LineSpan { start, height: 1 })
    }

    #[test]
    fn trace_focus_survives_when_the_pair_still_renders() {
        let cur = Some("a".to_string());
        let mut focus = Some("t1".to_string());
        let spans = vec![tspan("a", "t0", 5), tspan("a", "t1", 6)];
        assert!(!reconcile_trace_focus(&cur, &mut focus, &spans, &spans));
        assert_eq!(focus.as_deref(), Some("t1"));
    }

    #[test]
    fn an_evicted_id_snaps_to_the_nearest_surviving_row() {
        let cur = Some("a".to_string());
        let mut focus = Some("t9".to_string());
        let previous = vec![tspan("a", "t8", 5), tspan("a", "t9", 6)];
        let current = vec![tspan("a", "t8", 5), tspan("a", "t7", 6)];
        assert!(reconcile_trace_focus(&cur, &mut focus, &previous, &current));
        assert_eq!(focus.as_deref(), Some("t7"), "same index, newest side");
    }

    #[test]
    fn an_empty_or_foreign_ring_drops_focus() {
        let cur = Some("a".to_string());
        let mut focus = Some("t1".to_string());
        assert!(reconcile_trace_focus(&cur, &mut focus, &[], &[]));
        assert_eq!(focus, None);
        let mut focus = Some("t1".to_string());
        let other = vec![tspan("b", "t1", 3)];
        assert!(reconcile_trace_focus(&cur, &mut focus, &other, &other));
        assert_eq!(focus, None, "same id on another card is not our pair");
    }
```

- [ ] **Step 2: Verify failure** — FAIL to compile (`no field trace_focus` / missing fn).
- [ ] **Step 3: Implement** the field (`Interaction` derives `Default` — `Option` defaults fine) and:

```rust
/// Spec §2: the pair either still renders, snaps to the nearest surviving
/// row on its card (newest side), or drops to the card zone. Never leaves
/// focus on an unrendered row.
fn reconcile_trace_focus(
    cursor: &Option<String>,
    trace_focus: &mut Option<String>,
    previous: &[(String, String, LineSpan)],
    current: &[(String, String, LineSpan)],
) -> bool {
    let Some(id) = trace_focus.clone() else {
        return false;
    };
    let Some(card) = cursor.clone() else {
        return trace_focus.take().is_some();
    };
    let card_rows =
        |spans: &[(String, String, LineSpan)]| -> Vec<String> {
            spans
                .iter()
                .filter(|(c, ..)| *c == card)
                .map(|(_, id, _)| id.clone())
                .collect()
        };
    let now = card_rows(current);
    if now.iter().any(|r| *r == id) {
        return false;
    }
    let next = if now.is_empty() {
        None
    } else {
        let prev_index = card_rows(previous)
            .iter()
            .position(|r| *r == id)
            .unwrap_or(0);
        Some(now[prev_index.min(now.len() - 1)].clone())
    };
    let changed = *trace_focus != next;
    *trace_focus = next;
    changed
}
```

Wire it in the draw block right after the cursor reconcile: `if reconcile_trace_focus(&it.cursor, &mut it.trace_focus, &last_rendered.trace_spans, &out.trace_spans) { dirty = true; }` — the view rendered this frame with last frame's focus, so a correction converges on the next draw exactly like the cursor's `recovered` flag. Also pass the pair into `view::render`'s input: `trace_focus: it.trace_focus.as_deref().and_then(|id| it.cursor.as_deref().map(|c| (c, id)))`.

- [ ] **Step 4: Verify pass** — `cargo test sidebar::tui` — PASS.
- [ ] **Step 5: Commit** — `cargo fmt && git add src/sidebar/tui.rs && git commit -m "feat(sidebar): trace-focus state with pair-safe reconcile"`

---

### Task 5: Key transitions and outcome variants

**Files:**
- Modify: `src/sidebar/tui.rs` — `KeyOutcome` gains variants; `apply_key` gains the trace zone; tests.

**Interfaces:**
- Consumes: `Interaction.trace_focus`, `Rendered.trace_spans`, `Live.auto_expand`, `Interaction.toggled`.
- Produces (Task 9 consumes): `KeyOutcome::{Quit, Handled, EnterTraces { card_id: String }, OpenTrace { card_id: String, tool_use_id: String }}`. Zone rules (spec §2): with `trace_focus` set, `j`/`k` move over the cursor card's rows in `trace_spans` (clamped, `follow = true`), `h` clears focus, `o`/Enter yields `OpenTrace(pair)`, `l` is inert; in the card zone, `l` yields `EnterTraces` when the cursor card is expanded (`expanded = matches!(live.auto_expand, AutoExpand::All) ^ it.toggled.contains(id)`), and `h` is inert. `z`/PageUp/PageDown/arrows keep their existing arms in both zones; `q`/esc/ctrl-c unchanged.

- [ ] **Step 1: Write the failing tests** (the `two_cards()` fixture gains trace spans on card `a` in Task 3's literal update — give it `tspan("a","t-new",1)` and `tspan("a","t-old",2)` here if not already):

```rust
    fn focused(cursor: &str, id: &str) -> Interaction {
        Interaction {
            cursor: Some(cursor.into()),
            trace_focus: Some(id.into()),
            ..Default::default()
        }
    }

    #[test]
    fn l_yields_enter_traces_only_from_an_expanded_card() {
        let rendered = two_cards();
        let mut live = live_default();
        let mut it = Interaction {
            cursor: Some("a".into()),
            ..Default::default()
        };
        // Not expanded (auto_expand None, not toggled): inert.
        assert!(matches!(
            apply_key(press(KeyCode::Char('l')), &mut it, &mut live, &rendered, 20, 40),
            KeyOutcome::Handled
        ));
        it.toggled.insert("a".into());
        assert!(matches!(
            apply_key(press(KeyCode::Char('l')), &mut it, &mut live, &rendered, 20, 40),
            KeyOutcome::EnterTraces { ref card_id } if card_id == "a"
        ));
        // Inside the trace zone l is inert (spec §2).
        let mut it = focused("a", "t-new");
        assert!(matches!(
            apply_key(press(KeyCode::Char('l')), &mut it, &mut live, &rendered, 20, 40),
            KeyOutcome::Handled
        ));
        assert_eq!(it.trace_focus.as_deref(), Some("t-new"));
    }

    #[test]
    fn trace_zone_navigation_clamps_and_h_returns() {
        let rendered = two_cards();
        let mut live = live_default();
        let mut it = focused("a", "t-new");
        apply_key(press(KeyCode::Char('j')), &mut it, &mut live, &rendered, 20, 40);
        assert_eq!(it.trace_focus.as_deref(), Some("t-old"));
        assert!(it.follow);
        apply_key(press(KeyCode::Char('j')), &mut it, &mut live, &rendered, 20, 40);
        assert_eq!(it.trace_focus.as_deref(), Some("t-old"), "clamped at the end");
        apply_key(press(KeyCode::Char('k')), &mut it, &mut live, &rendered, 20, 40);
        assert_eq!(it.trace_focus.as_deref(), Some("t-new"));
        apply_key(press(KeyCode::Char('h')), &mut it, &mut live, &rendered, 20, 40);
        assert_eq!(it.trace_focus, None);
        assert_eq!(it.cursor.as_deref(), Some("a"), "cursor stays");
    }

    #[test]
    fn o_in_the_trace_zone_requests_the_pair() {
        let rendered = two_cards();
        let mut live = live_default();
        let mut it = focused("a", "t-old");
        assert!(matches!(
            apply_key(press(KeyCode::Char('o')), &mut it, &mut live, &rendered, 20, 40),
            KeyOutcome::OpenTrace { ref card_id, ref tool_use_id }
                if card_id == "a" && tool_use_id == "t-old"
        ));
        // Card zone o still toggles.
        let mut it = Interaction { cursor: Some("a".into()), ..Default::default() };
        apply_key(press(KeyCode::Char('o')), &mut it, &mut live, &rendered, 20, 40);
        assert!(it.toggled.contains("a"));
    }
```

- [ ] **Step 2: Verify failure** — FAIL to compile (missing variants).
- [ ] **Step 3: Implement.** Add the two variants to `enum KeyOutcome`. In `apply_key`, add a trace-zone dispatch before the existing match — when `it.trace_focus.is_some()`, handle `j`/`k` (index over `rendered.trace_spans` filtered to the cursor card, exactly the `move_cursor` pattern; set `follow = true`), `h` (clear focus), `o`/Enter (return `OpenTrace` from the pair), `l` (Handled), and fall through to the existing arms for everything else. In the card zone, add the `l` arm returning `EnterTraces` for an expanded cursor card (`Handled` otherwise); `h` falls to the existing catch-all. The existing `run()` match on `route(...)` result only checks `Quit`, so new variants pass through untouched until Task 9 — add `KeyOutcome::EnterTraces { .. } | KeyOutcome::OpenTrace { .. } => {}` arms wherever the compiler demands exhaustiveness.
- [ ] **Step 4: Verify pass** — `cargo test sidebar::tui` — PASS.
- [ ] **Step 5: Commit** — `cargo fmt && git add src/sidebar/tui.rs && git commit -m "feat(sidebar): trace-zone key transitions as outcomes"`

---

### Task 6: Mouse extension

**Files:**
- Modify: `src/sidebar/tui.rs` — `apply_mouse` returns `MouseOutcome`; trace-first resolution; `route_mouse` and the event arm adapt; tests.

**Interfaces:**
- Consumes: `trace_at` (Task 2), `trace_spans` (Task 3), the pair identity.
- Produces (Task 9 consumes): `enum MouseOutcome { Inert, Changed, OpenTrace { card_id: String, tool_use_id: String } }`; `apply_mouse(...) -> MouseOutcome`; `route_mouse(...) -> MouseOutcome` (gate returns `Inert`). Rules (spec §3): `trace_at` first — pair match → `OpenTrace`; other trace hit → deep-select (`cursor = card`, `trace_focus = id`, `follow = true`, `Changed`). Then `card_at` — header/body keep v0.2.5 semantics, plus **any cursor change or anchor collapse clears `trace_focus`**. Wheel/dirty rules unchanged.

- [ ] **Step 1: Write the failing tests:**

```rust
    #[test]
    fn a_trace_click_deep_selects_and_a_second_click_opens() {
        let rendered = two_cards();
        let mut it = Interaction::default();
        assert!(matches!(
            apply_mouse(click(1), &mut it, &rendered, 20, 40),
            MouseOutcome::Changed
        ));
        assert_eq!(it.cursor.as_deref(), Some("a"));
        assert_eq!(it.trace_focus.as_deref(), Some("t-new"));
        assert!(it.follow);
        assert!(matches!(
            apply_mouse(click(1), &mut it, &rendered, 20, 40),
            MouseOutcome::OpenTrace { ref card_id, ref tool_use_id }
                if card_id == "a" && tool_use_id == "t-new"
        ));
    }

    #[test]
    fn card_clicks_that_move_or_collapse_clear_trace_focus() {
        let rendered = two_cards();
        // Body click onto the other card.
        let mut it = focused("a", "t-new");
        apply_mouse(click(6), &mut it, &rendered, 20, 40); // card b body
        assert_eq!(it.cursor.as_deref(), Some("b"));
        assert_eq!(it.trace_focus, None, "pair cannot leak across cards");
        // Header click that collapses the anchor.
        let mut it = focused("a", "t-new");
        it.toggled.insert("a".into());
        apply_mouse(click(0), &mut it, &rendered, 20, 40); // card a header
        assert_eq!(it.trace_focus, None);
    }
```

(`two_cards()` trace spans sit at lines 1–2 inside card `a` (`start: 0, height: 3`); card `b` starts at line 4 — `click(6)` is body. If Task 3 placed the fixture spans differently, adjust the clicked rows to match the fixture, not the other way around.)

- [ ] **Step 2: Verify failure** — FAIL to compile (`MouseOutcome` missing / return type).
- [ ] **Step 3: Implement.** Define `MouseOutcome`; change `apply_mouse`'s signature and arms: in the `Down(Left)` branch, run `trace_at(&rendered.trace_spans, line)` first — pair-match test against `(it.cursor, it.trace_focus)` returns `OpenTrace`; otherwise deep-select and return `Changed`. In the `Hit::Header` arm add `it.trace_focus = None;` when the click toggles the anchor card or moves the cursor; in `Hit::Body` add `if it.cursor.as_deref() != Some(id.as_str()) { it.trace_focus = None; }` before the cursor assignment. Wheel arms return `Changed`/`Inert` in place of `true`/`false`. `route_mouse` returns `MouseOutcome` (`Inert` when gated); the event arm becomes:

```rust
            Ok(Event::Mouse(mouse)) => {
                match route_mouse(mouse, &open, live.mouse, &mut it, &last_rendered, viewport, total) {
                    MouseOutcome::Inert => {}
                    MouseOutcome::Changed => dirty = true,
                    MouseOutcome::OpenTrace { .. } => dirty = true, // resolved in Task 9
                }
            }
```

Update the existing v0.2.5 mouse tests mechanically: `assert!(apply_mouse(...))` → `assert!(matches!(..., MouseOutcome::Changed))`, `assert!(!...)` → `MouseOutcome::Inert`.

- [ ] **Step 4: Verify pass** — `cargo test sidebar::tui` — PASS (old and new).
- [ ] **Step 5: Commit** — `cargo fmt && git add src/sidebar/tui.rs && git commit -m "feat(sidebar): trace-aware mouse outcomes"`

---

### Task 7: TraceDetail dialog

**Files:**
- Modify: `src/sidebar/tui.rs` — `Dialog::TraceDetail` variant + `trace_panel` + shared `panel_width` + close/scroll routing; `panel_for` arm; tests.

**Interfaces:**
- Consumes: `dialog::{Panel, Row, line_count}` (all existing).
- Produces (Task 9 installs it): `Dialog::TraceDetail { title: String, rows: Vec<Row>, offset: usize }` — a **fully frozen snapshot**; `fn trace_panel(title: &str, rows: &[Row], offset: usize) -> Panel` (cursor `None`, footer `"j/k scroll · esc close"`); `fn panel_width(frame: u16) -> u16 { frame.min(60) }` used by BOTH the draw site (replacing the inline `area.width.min(60)`) and scroll bounding (spec §4 shared-width rule).
- Routing: `esc`/`q` on `TraceDetail` sets `*open = None` directly (its own arm ABOVE the generic back-to-menu branch — spec §4); `j`/`k` scroll by offset bounded by `dialog::line_count(&trace_panel(...), panel_width(width))` — `route` already receives `width`. `Dialog::len` returns 0 for it (no cursor); `offset`/`offset_mut` include it.

- [ ] **Step 1: Write the failing tests:**

```rust
    fn detail_rows() -> Vec<crate::sidebar::dialog::Row> {
        // 40 short Note lines: at width 60 each stays one rendered line.
        (0..40)
            .map(|i| crate::sidebar::dialog::Row::Note(format!("line {i}")))
            .collect()
    }

    #[test]
    fn trace_detail_closes_to_none_keeping_the_selection() {
        let mut open = Some(Dialog::TraceDetail {
            title: "Trace — Bash".into(),
            rows: detail_rows(),
            offset: 0,
        });
        let mut it = focused("a", "t-new");
        let mut live = live_default();
        let rendered = two_cards();
        route(press(KeyCode::Esc), &mut open, &mut it, &mut live, &rendered, 20, 40, 120, 40);
        assert!(open.is_none(), "no menu detour");
        assert_eq!(it.cursor.as_deref(), Some("a"));
        assert_eq!(it.trace_focus.as_deref(), Some("t-new"));
    }

    #[test]
    fn trace_detail_scrolls_to_the_last_rendered_line_on_wide_frames() {
        let mut open = Some(Dialog::TraceDetail {
            title: "Trace — Bash".into(),
            rows: detail_rows(),
            offset: 0,
        });
        let mut it = Interaction::default();
        let mut live = live_default();
        let rendered = two_cards();
        for _ in 0..200 {
            route(press(KeyCode::Char('j')), &mut open, &mut it, &mut live, &rendered, 20, 40, 120, 40);
        }
        let Some(Dialog::TraceDetail { offset, .. }) = open else {
            panic!("panel stays open");
        };
        // 40 one-line rows: the offset clamps at line_count - 1 = 39 even
        // though the frame (120) is wider than the 60-column panel clamp.
        assert_eq!(offset, 39);
    }
```

- [ ] **Step 2: Verify failure** — FAIL to compile (no variant).
- [ ] **Step 3: Implement.** Add the variant; extend `offset`/`offset_mut` matches; `len` → 0 arm; add to `row_count` an arm `Dialog::TraceDetail { title, rows, .. } => crate::sidebar::dialog::line_count(&trace_panel(title, rows, 0), 60)` — then in the `j` routing branch, special-case the width-aware bound: for `TraceDetail`, compute `let rows = crate::sidebar::dialog::line_count(&trace_panel(title, rows, 0), panel_width(width)) ;` (shadowing the generic `row_count()` value). Add the close arm before the generic esc branch:

```rust
            (KeyCode::Esc, _) | (KeyCode::Char('q'), _) if matches!(dialog, Dialog::TraceDetail { .. }) => {
                *open = None;
            }
```

`trace_panel` clones title/rows into a `Panel` (`cursor: None`, `offset`, footer as specified). `panel_for` gains `Dialog::TraceDetail { title, rows, offset } => trace_panel(title, rows, *offset)`. Replace the draw site's `area.width.min(60)` with `panel_width(area.width)`.

- [ ] **Step 4: Verify pass** — `cargo test sidebar::tui` — PASS.
- [ ] **Step 5: Commit** — `cargo fmt && git add src/sidebar/tui.rs && git commit -m "feat(sidebar): frozen TraceDetail panel with width-aware scrolling"`

---

### Task 8: Snapshot builder

**Files:**
- Modify: `src/sidebar/tui.rs` — pure `trace_snapshot` turning a ring `Value` into `(title, Vec<Row>)`; tests.

**Interfaces:**
- Consumes: `format::{duration_ms, age, parse_iso8601_ms, sanitise}`; `dialog::Row`.
- Produces (Task 9 calls it): `fn trace_snapshot(call: &serde_json::Value, now_unix_ms: u64) -> (String, Vec<crate::sidebar::dialog::Row>)`. Content (spec §4/§5): title `Trace — <tool>` (`?` fallback); rows = `Entry { label: "status", value: "<✓ done|✕ failed> · <duration>", enabled: false }` (duration segment omitted when `durationMs` is missing), `Entry { label: "when", value: "<ISO stamp> · <age>", ... }` (`—` when missing/unparseable — the age is computed HERE, once: the frozen when-line, spec §4), `Rule`, then the args body: `serde_json::from_str::<Value>(args)` OK → `serde_json::to_string_pretty` split into one `Note` per line (hard newlines preserved by construction; `Note` wraps width-aware at render); parse failure → the sanitised raw preview as one `Note`; empty → `Note("(no arguments retained)")`.

- [ ] **Step 1: Write the failing tests:**

```rust
    #[test]
    fn snapshot_freezes_pretty_args_and_metadata() {
        let call = serde_json::json!({
            "toolUseId": "t1", "tool": "Bash", "status": "failed",
            "args": "{\"command\":\"cargo test\"}",
            "timestamp": "2026-09-01T10:00:00.000Z", "durationMs": 1200,
        });
        let (title, rows) = trace_snapshot(&call, 1_764_000_000_000);
        assert_eq!(title, "Trace — Bash");
        let texts: Vec<String> = rows
            .iter()
            .map(|r| match r {
                crate::sidebar::dialog::Row::Entry { label, value, .. } => format!("{label}={value}"),
                crate::sidebar::dialog::Row::Note(n) => n.clone(),
                crate::sidebar::dialog::Row::Warn(w) => w.clone(),
                crate::sidebar::dialog::Row::Rule => "—rule—".into(),
            })
            .collect();
        assert!(texts[0].starts_with("status=✕ failed · 1.2s"));
        assert!(texts[1].starts_with("when=2026-09-01T10:00:00.000Z"));
        assert!(texts.iter().any(|t| t.contains("\"command\": \"cargo test\"")), "pretty-printed");
        assert!(texts.iter().all(|t| !t.contains('\n')), "one Note per line");
    }

    #[test]
    fn snapshot_degrades_malformed_metadata_per_spec() {
        let call = serde_json::json!({ "toolUseId": "t1", "status": "done", "args": "" });
        let (title, rows) = trace_snapshot(&call, 0);
        assert_eq!(title, "Trace — ?");
        let joined = rows
            .iter()
            .filter_map(|r| match r {
                crate::sidebar::dialog::Row::Entry { label, value, .. } => Some(format!("{label}={value}")),
                crate::sidebar::dialog::Row::Note(n) => Some(n.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|");
        assert!(joined.contains("status=✓ done"));
        assert!(!joined.contains("·"), "no duration segment without durationMs");
        assert!(joined.contains("when=—"));
        assert!(joined.contains("(no arguments retained)"));
    }
```

- [ ] **Step 2: Verify failure** — FAIL to compile.
- [ ] **Step 3: Implement** exactly per the interface block (a straight-line function of gets + fallbacks; glyph mapping `failed → ✕`, everything settled-else → `✓` is fine because Task 3 guarantees only done/failed rows are reachable).
- [ ] **Step 4: Verify pass** — `cargo test sidebar::tui::tests::snapshot` — PASS.
- [ ] **Step 5: Commit** — `cargo fmt && git add src/sidebar/tui.rs && git commit -m "feat(sidebar): frozen trace snapshot builder"`

---

### Task 9: Run-loop resolver and row-granularity follow

**Files:**
- Modify: `src/sidebar/tui.rs` — a `resolve_trace_outcome` helper called from BOTH event arms; draw-block follow uses the selected row's span; tests for the pure parts.

**Interfaces:**
- Consumes: everything above, plus `State.panes` (`state.panes.get(card).map(|t| &t.tool_calls)`).
- Produces: `fn canonical_call<'a>(ring: &'a std::collections::VecDeque<Value>, id: &str) -> Option<&'a Value>` (newest-first, first match — mirrors Task 3's dedup); `fn newest_selectable_id(ring: &std::collections::VecDeque<Value>) -> Option<String>` (spec §2 `EnterTraces` resolution: newest entry with a `toolUseId` and done/failed status). The run loop:
  - `KeyOutcome::EnterTraces { card_id }` → `newest_selectable_id` on that pane's ring → `Some(id)`: set `it.trace_focus = Some(id); it.follow = true; dirty = true`; `None`: nothing.
  - `KeyOutcome::OpenTrace { .. }` and `MouseOutcome::OpenTrace { .. }` → same path: small-frame gate first (`frame_size.width < MIN_DIALOG_WIDTH || frame_size.height < MIN_DIALOG_HEIGHT` → `it.notice = Some(format!("the frame is too small for a panel ({}x{}; needs {MIN_DIALOG_WIDTH}x{MIN_DIALOG_HEIGHT})", frame_size.width, frame_size.height)); dirty = true;` — same wording as the `x`/`?` branch), else `canonical_call` → found: `trace_snapshot` → `open = Some(Dialog::TraceDetail { title, rows, offset: 0 }); dirty = true;` absent: selection-only degrade (nothing else).
- Draw block: a named pure helper picks the span the follow logic tracks — the selected trace row when the pair is set, the cursor card otherwise:

```rust
/// Row-granularity follow (spec §2): while a trace is selected, the
/// visibility contract tracks the row, not its card.
fn follow_span(
    cursor: &Option<String>,
    trace_focus: &Option<String>,
    rendered: &Rendered,
) -> Option<LineSpan> {
    let card = cursor.as_deref()?;
    trace_focus
        .as_deref()
        .and_then(|id| rendered.trace_span_for(card, id))
        .or_else(|| rendered.span_for(card))
}
```

The draw block's `it.offset = match it.cursor.as_deref().and_then(|id| out.span_for(id))` becomes `it.offset = match follow_span(&it.cursor, &it.trace_focus, &out)` (and the `prev_span` side reads `follow_span(&it.cursor, &it.trace_focus, &last_rendered)` before `out` replaces it).

- [ ] **Step 1: Write the failing tests** (pure parts):

```rust
    #[test]
    fn canonical_call_and_newest_selectable_follow_ring_rules() {
        let mut ring = std::collections::VecDeque::new();
        ring.push_back(serde_json::json!({"toolUseId":"dup","status":"done","tool":"Old"}));
        ring.push_back(serde_json::json!({"status":"done","tool":"NoId"}));
        ring.push_back(serde_json::json!({"toolUseId":"run","status":"running"}));
        ring.push_back(serde_json::json!({"toolUseId":"dup","status":"failed","tool":"New"}));
        assert_eq!(
            canonical_call(&ring, "dup").and_then(|c| c.get("tool")).and_then(Value::as_str),
            Some("New"),
            "newest occurrence wins, matching the view's dedup"
        );
        assert_eq!(newest_selectable_id(&ring).as_deref(), Some("dup"));
        let empty: std::collections::VecDeque<Value> = [
            serde_json::json!({"status":"done"}),
            serde_json::json!({"toolUseId":"r","status":"running"}),
        ]
        .into_iter()
        .collect();
        assert_eq!(newest_selectable_id(&empty), None, "no selectable row anywhere");
    }

    #[test]
    fn follow_prefers_the_selected_trace_row_over_its_card() {
        let rendered = two_cards();
        let cursor = Some("a".to_string());
        let span = follow_span(&cursor, &Some("t-old".to_string()), &rendered).expect("row");
        assert_eq!(span.start, 2, "the row, not the card header");
        let span = follow_span(&cursor, &None, &rendered).expect("card");
        assert_eq!(span.start, 0, "card zone follows the card");
        assert_eq!(follow_span(&None, &Some("t-old".into()), &rendered), None);
    }
```

- [ ] **Step 2: Verify failure** — FAIL to compile.
- [ ] **Step 3: Implement** the two helpers (iterate `.iter().rev()`, apply the Global-Constraints selectable predicate) and wire the run loop per the interface block: the `Event::Key` arm matches the two new outcomes; the `Event::Mouse` arm's `OpenTrace` goes through the same resolver fn so the logic exists once. Then swap the draw-block span selection to `follow_span`.
- [ ] **Step 4: Verify pass** — `cargo test sidebar::tui` — PASS; then `cargo test` (whole suite) — PASS.
- [ ] **Step 5: Commit** — `cargo fmt && git add src/sidebar/tui.rs && git commit -m "feat(sidebar): resolve trace outcomes and follow the selected row"`

---

### Task 10: Help surface + final verification

**Files:**
- Modify: `src/sidebar/tui.rs` — `KEYS`, `routed()` (+ its driving test), the mouse-trailer string + its assertion, `o / ↵` description; full-suite run.

**Interfaces:**
- Consumes: everything shipped above.
- Produces (spec §4): `KEYS` gains `("l", "into traces")` and `("h", "back to cards")`; the `o / ↵` entry reads `"expand a card, or open the selected trace"`; the keys-panel trailer value becomes exactly `click selects · header toggles · trace re-click opens · wheel scrolls`. `routed()`'s tuple grows a 5th element `Option<&'static str>` — an initial `trace_focus` seed — and the invariant test seeds it and includes `trace_focus` in the before/after comparison, so `h` starts inside the trace zone and both new keys are observably active.

- [ ] **Step 1: Write the failing changes test-first.** Update the trailer assertion in `the_keys_sheet_carries_the_mouse_hint_outside_the_key_contract` to the new string; extend `routed()`'s type to `[(&'static str, KeyCode, KeyModifiers, &'static str, Option<&'static str>); 11]` with `None` on existing rows plus `("l", KeyCode::Char('l'), KeyModifiers::NONE, "a", None)` and `("h", KeyCode::Char('h'), KeyModifiers::NONE, "a", Some("t-new"))`; in the driving test, build each case's `Interaction` with the seeded `trace_focus`, ensure the seeded card `a` is toggled expanded for `l`, and extend the state comparison to include `it.trace_focus`.
- [ ] **Step 2: Verify failure** — `cargo test sidebar::tui` — the sheet↔routed invariant and trailer tests FAIL (KEYS missing rows, string mismatch).
- [ ] **Step 3: Implement** the `KEYS` rows, the description change, and the trailer string.
- [ ] **Step 4: Full verification** — `cargo fmt && cargo test` — everything PASSES; `cargo clippy --all-targets 2>&1 | grep -cE "^warning"` matches the pre-plan baseline (run it on the base commit first and record the number).
- [ ] **Step 5: Commit** — `git add src/sidebar/tui.rs && git commit -m "feat(sidebar): trace keys on the help sheet and trailer"`

---

## Spec coverage map

- §1 goal/decisions 1–4 → Tasks 5 (`l`/`h`), 3+9 (full ring, row follow), 7+8 (panel), 6 (mouse pair rules).
- §2 state/invariant/transitions → Tasks 4 (reconcile), 5 (keys), 9 (`EnterTraces` resolution, snapshot rule); display-only rows → Tasks 3 (no spans) and 9 (`newest_selectable_id`).
- §3 span export/hit-test/click rules/outcome plumbing/small-frame gate → Tasks 3, 2, 6, 9.
- §4 panel content/scroll/close/duration/frozen when/keys sheet/trailer → Tasks 8, 7, 1, 10.
- §5 anchor-loss matrix, churn, duplicates, malformed metadata, degenerate frames → Tasks 4, 3, 8, 9 tests.
- §6 test list → distributed exactly as written per task.
