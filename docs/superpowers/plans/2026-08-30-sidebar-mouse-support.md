# Sidebar Mouse Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Opt-in mouse support for the Agent Watcher sidebar — click a card header to select+toggle, click a body to select, wheel to scroll — behind a `[cards] mouse` config key that defaults off.

**Architecture:** A pure hit-test (`card_at`) joins the existing scroll maths in `layout.rs`; a pure `apply_mouse` sibling of `apply_key` mutates `Interaction` in `tui.rs`, reached through a `route_mouse` gate that starves mouse events while a dialog is open or the setting is off. `TerminalGuard` owns capture with a conservative "may be applied" flag, and the run loop reconciles requested-vs-attempted once per change of the requested value.

**Tech Stack:** Rust (edition 2021), crossterm 0.29, ratatui 0.30, toml_edit — all already in the tree. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-30-sidebar-mouse-support-design.md` — read it first; every contract below cites it. Its Section 6 host gate **passed on 2026-08-30** (probe results recorded in the spec): herdr forwards the full SGR event set with pane-local 0-based coordinates.

## Global Constraints

- Rust floor: `rust-version = "1.88"` (Cargo.toml). Do not use features newer than 1.88.
- No new dependencies. All sidebar code already sits behind the default `runtime` feature.
- `cargo fmt` before every commit — CI runs `cargo fmt --check`. Clippy has no `-D warnings` gate, but add no new warnings.
- Commit style: conventional commits as in `git log` (`feat(sidebar): …`, `test(sidebar): …`).
- `TerminalGuard::Drop` stays the single terminal cleanup both exit paths funnel through (spec §2 rule 3). The re-exec sites call `drop(guard)` explicitly — never add a second cleanup path.
- Spec §3: only `Down(Left)` with empty modifiers, `ScrollUp`, `ScrollDown` act. Everything else is inert and must not trigger a redraw.
- All tests are inline `#[cfg(test)]` per repo convention. Run the full suite with `cargo test` before the final commit (it regenerates `bindings/` — no TypeScript surface changes are expected from this plan).

---

### Task 1: Pure hit-test `card_at` in layout.rs

**Files:**
- Modify: `src/sidebar/layout.rs` (add `Hit`, `card_at`, tests; file currently ends with its `tests` module)

**Interfaces:**
- Consumes: `LineSpan { start: usize, height: usize }` (already defined at the top of this file).
- Produces: `pub enum Hit { Header, Body }` and `pub fn card_at(spans: &[(String, LineSpan)], line: usize) -> Option<(&str, Hit)>` — Tasks 5's `apply_mouse` calls this exact signature.

- [ ] **Step 1: Write the failing tests** — append inside the existing `mod tests` in `src/sidebar/layout.rs`:

```rust
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test sidebar::layout::tests::card_at_maps -- --nocapture`
Expected: FAIL to compile — `cannot find function card_at` / `cannot find type Hit`.

- [ ] **Step 3: Implement** — add above the `#[cfg(test)]` module in `src/sidebar/layout.rs`:

```rust
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
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test sidebar::layout -- --nocapture`
Expected: PASS (all layout tests, old and new).

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add src/sidebar/layout.rs && git commit -m "feat(sidebar): pure card hit-test beside the scroll maths"
```

---

### Task 2: `[cards] mouse` config key

**Files:**
- Modify: `src/sidebar/config.rs` — `Loaded` struct (field after `plan_usage: bool`), its manual `Default` impl (entry after `plan_usage: true`), `read_cards` (arm after `"plan_usage"`), tests.

**Interfaces:**
- Produces: `Loaded.mouse: bool` (default `false`) — Task 3 copies it into `Live`.

- [ ] **Step 1: Write the failing tests** — append inside `mod tests` in `src/sidebar/config.rs` (the module already defines `fn load_str(s: &str) -> Loaded`):

```rust
    #[test]
    fn mouse_defaults_off_and_reads_the_cards_key() {
        assert!(!load_str("").mouse);
        assert!(load_str("[cards]\nmouse = true\n").mouse);
        assert!(!load_str("[cards]\nmouse = false\n").mouse);
        let invalid = load_str("[cards]\nmouse = \"sure\"\n");
        assert!(!invalid.mouse, "a bad value falls back to off");
        assert_eq!(invalid.status.problems, 1);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test sidebar::config::tests::mouse_defaults -- --nocapture`
Expected: FAIL to compile — `no field mouse on type Loaded`.

- [ ] **Step 3: Implement.** Three edits in `src/sidebar/config.rs`:

In `pub struct Loaded`, after `pub plan_usage: bool,`:

```rust
    pub mouse: bool,
```

In `impl Default for Loaded`, after `plan_usage: true,`:

```rust
            mouse: false,
```

In `read_cards`, after the `"plan_usage"` arm and before `other =>`:

```rust
                "mouse" => match val.as_bool() {
                    Some(enabled) => self.mouse = enabled,
                    None => self.problem("invalid value for cards.mouse".into()),
                },
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test sidebar::config -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add src/sidebar/config.rs && git commit -m "feat(sidebar): [cards] mouse config key, default off"
```

---

### Task 3: `Live.mouse` + `Setting::Mouse` row

**Files:**
- Modify: `src/sidebar/live.rs` — `Live` struct (after `plan_usage`), `Live::build` (after `plan_usage: cfg.plan_usage,`), `Setting` enum (after `PlanUsage`), `SETTINGS` array (12 entries, `Mouse` after `PlanUsage`), `label()`, `value()`, `cycle()`, and the existing `live_is_seeded_from_the_loaded_config` test.
- Modify: `src/sidebar/settings_file.rs` — ONE line in `table_and_key` (its match is exhaustive over `Setting`, so the crate does not compile without it; the save *behavior* is Task 4's).

**Interfaces:**
- Consumes: `Loaded.mouse: bool` (Task 2).
- Produces: `Live.mouse: bool`, `Setting::Mouse` — Task 4 persists it, Task 6 gates on it, Task 8 reconciles it. Displayed value is `"on"`/`"off"` (spec §4); `cycle` flips the bool.

- [ ] **Step 1: Write the failing tests.** Extend `live_is_seeded_from_the_loaded_config` in `src/sidebar/live.rs` — add before `let live = Live::from(&cfg);`:

```rust
        cfg.mouse = true;
```

and after the last existing assert:

```rust
        assert!(live.mouse);
```

Then append a new test in the same module:

```rust
    #[test]
    fn the_mouse_setting_cycles_and_reports_on_off() {
        let mut live = Live::from(&Loaded::from_missing());
        assert_eq!(live.value(Setting::Mouse), "off");
        live.cycle(Setting::Mouse, None);
        assert_eq!(live.value(Setting::Mouse), "on");
        assert!(live.mouse);
        live.cycle_back(Setting::Mouse, None);
        assert!(!live.mouse);
        assert!(SETTINGS.contains(&Setting::Mouse));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test sidebar::live -- --nocapture`
Expected: FAIL to compile — `no field mouse`, `no variant Mouse`.

- [ ] **Step 3: Implement.** Six edits in `src/sidebar/live.rs`:

`Live` struct, after `pub plan_usage: bool,`:

```rust
    /// Requested state (spec §5): what the user asked for, what the row
    /// shows, what saves persist. Whether capture is applied lives in
    /// the terminal guard, never here.
    pub mouse: bool,
```

`Live::build`, after `plan_usage: cfg.plan_usage,`:

```rust
            mouse: cfg.mouse,
```

`Setting` enum, after `PlanUsage,`:

```rust
    Mouse,
```

`SETTINGS` — change the array to 12 entries with `Mouse` after `PlanUsage`:

```rust
pub const SETTINGS: [Setting; 12] = [
    Setting::Sort,
    Setting::Scope,
    Setting::HideIdle,
    Setting::AutoExpand,
    Setting::ToolCalls,
    Setting::TraceLines,
    Setting::PlanUsage,
    Setting::Mouse,
    Setting::Theme,
    Setting::AgentMark,
    Setting::IntervalMs,
    Setting::PruneAfterDays,
];
```

`label()`, after the `PlanUsage` arm:

```rust
            Setting::Mouse => "mouse",
```

`value()`, after the `PlanUsage` arm:

```rust
            Setting::Mouse => if self.mouse { "on" } else { "off" }.into(),
```

`cycle()`, after the `PlanUsage` arm:

```rust
            Setting::Mouse => self.mouse = !self.mouse,
```

(`cycle_back` needs no change: its catch-all `other => self.cycle(other, workspace)` already covers a two-state toggle.)

And in `src/sidebar/settings_file.rs`, `table_and_key` — after the `PlanUsage` arm, purely so the exhaustive match compiles (Task 4 owns the save behavior and its test):

```rust
        Setting::Mouse => ("cards", "mouse"),
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test sidebar::live -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add src/sidebar/live.rs src/sidebar/settings_file.rs && git commit -m "feat(sidebar): live mouse setting with an on/off panel row"
```

---

### Task 4: Persist `Setting::Mouse` to `[cards] mouse`

**Files:**
- Modify: `src/sidebar/settings_file.rs` — `item_for` (explicit bool branch; `table_and_key` already gained its arm in Task 3 for compilation), tests.

**Interfaces:**
- Consumes: `Setting::Mouse`, `Live.mouse` (Task 3), plus this file's existing `edit(current, live, dirty)` entry point.
- Produces: settings-panel saves write `mouse = true|false` under `[cards]` via `toml_edit` without disturbing the document.

- [ ] **Step 1: Write the failing test** — append inside this file's `mod tests`:

```rust
    #[test]
    fn mouse_saves_under_cards_without_disturbing_the_document() {
        let mut live = Live::from(&crate::sidebar::config::Loaded::from_missing());
        live.mouse = true;
        let current = "# a comment the save must keep\n[cards]\nauto_expand = \"all\"\n";
        let edited = edit(current, &live, &[Setting::Mouse]).expect("edit");
        assert!(edited.contains("# a comment the save must keep"));
        assert!(edited.contains("auto_expand = \"all\""));
        assert!(edited.contains("mouse = true"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test sidebar::settings_file::tests::mouse_saves -- --nocapture`
Expected: FAIL the assertion — without an explicit `item_for` arm, the `other => value(live.value(other))` fallback serialises the *display string*, writing `mouse = "on"`, so `edited.contains("mouse = true")` is false. (This is exactly why the arm must exist.)

- [ ] **Step 3: Implement.** In `item_for`, extend the explicit bool group (the arms above the `other =>` fallback):

```rust
        Setting::Mouse => value(live.mouse),
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test sidebar::settings_file -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add src/sidebar/settings_file.rs && git commit -m "feat(sidebar): persist the mouse toggle as a boolean under [cards]"
```

---

### Task 5: `apply_mouse` in tui.rs

**Files:**
- Modify: `src/sidebar/tui.rs` — extend the crossterm import (line 5), add `MOUSE_SCROLL_LINES` near `INPUT_POLL_EVERY`, add `apply_mouse` directly below `apply_key`, tests in the existing `mod tests` (which already has `two_cards()`).

**Interfaces:**
- Consumes: `card_at`/`Hit` (Task 1), `Interaction` (fields `cursor: Option<String>`, `toggled: HashSet<String>`, `offset: u16`, `follow: bool`), `Rendered.spans`, `clamp_scroll`.
- Produces: `fn apply_mouse(mouse: crossterm::event::MouseEvent, it: &mut Interaction, rendered: &Rendered, viewport: u16, total: usize) -> bool` — `true` means state changed (the caller's dirty flag). Task 6 wraps it.

- [ ] **Step 1: Extend imports.** Line 5 of `src/sidebar/tui.rs` becomes:

```rust
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
```

and line 9 becomes:

```rust
use crate::sidebar::layout::{card_at, clamp_scroll, ensure_visible, reanchor, Hit};
```

- [ ] **Step 2: Write the failing tests** — append inside `mod tests` in `src/sidebar/tui.rs`:

```rust
    fn click(row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn wheel(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn a_header_click_selects_toggles_and_follows() {
        let rendered = two_cards();
        let mut it = Interaction::default();
        assert!(apply_mouse(click(0), &mut it, &rendered, 20, 40));
        assert_eq!(it.cursor.as_deref(), Some("a"));
        assert!(it.toggled.contains("a"));
        assert!(it.follow);
        // Idempotence: the second header click un-toggles.
        assert!(apply_mouse(click(0), &mut it, &rendered, 20, 40));
        assert!(!it.toggled.contains("a"));
    }

    #[test]
    fn a_body_click_selects_without_toggling_or_scrolling() {
        let rendered = two_cards();
        let mut it = Interaction {
            follow: true,
            offset: 2,
            ..Default::default()
        };
        // Row 3 + offset 2 = line 5, inside card b's body.
        assert!(apply_mouse(click(3), &mut it, &rendered, 20, 40));
        assert_eq!(it.cursor.as_deref(), Some("b"));
        assert!(it.toggled.is_empty());
        assert!(!it.follow, "spec §3: a body click detaches, never scrolls");
        assert_eq!(it.offset, 2, "offset untouched");
        // The identical body click again changes nothing — and reports it.
        assert!(!apply_mouse(click(3), &mut it, &rendered, 20, 40));
    }

    #[test]
    fn a_click_through_a_scrolled_offset_resolves_the_scrolled_card() {
        let rendered = two_cards();
        let mut it = Interaction {
            offset: 4,
            ..Default::default()
        };
        // Row 0 + offset 4 = line 4 = card b's header.
        assert!(apply_mouse(click(0), &mut it, &rendered, 20, 40));
        assert_eq!(it.cursor.as_deref(), Some("b"));
        assert!(it.toggled.contains("b"));
    }

    #[test]
    fn inert_clicks_report_no_state_change() {
        let rendered = two_cards();
        let mut it = Interaction::default();
        // The separator line between the cards.
        assert!(!apply_mouse(click(3), &mut it, &rendered, 20, 40));
        // The pinned footer region.
        assert!(!apply_mouse(click(20), &mut it, &rendered, 20, 40));
        // A modified click.
        let shifted = MouseEvent {
            modifiers: KeyModifiers::SHIFT,
            ..click(0)
        };
        assert!(!apply_mouse(shifted, &mut it, &rendered, 20, 40));
        // A zero viewport.
        assert!(!apply_mouse(click(0), &mut it, &rendered, 0, 40));
        assert_eq!(it.cursor, None, "no inert event moved the cursor");
        assert!(it.toggled.is_empty());
    }

    #[test]
    fn the_wheel_moves_three_lines_and_detaches_only_when_it_moves() {
        let rendered = two_cards();
        let mut it = Interaction {
            follow: true,
            ..Default::default()
        };
        // At the top, ScrollUp cannot move: no change, follow untouched.
        assert!(!apply_mouse(wheel(MouseEventKind::ScrollUp), &mut it, &rendered, 20, 40));
        assert!(it.follow);
        assert!(apply_mouse(wheel(MouseEventKind::ScrollDown), &mut it, &rendered, 20, 40));
        assert_eq!(it.offset, 3);
        assert!(!it.follow, "a moved wheel detaches like PageDown");
        // Saturate: total 40, viewport 20 clamps at 20.
        for _ in 0..10 {
            apply_mouse(wheel(MouseEventKind::ScrollDown), &mut it, &rendered, 20, 40);
        }
        assert_eq!(it.offset, 20);
        assert!(!apply_mouse(wheel(MouseEventKind::ScrollDown), &mut it, &rendered, 20, 40));
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test sidebar::tui::tests::a_header_click -- --nocapture`
Expected: FAIL to compile — `cannot find function apply_mouse`.

- [ ] **Step 4: Implement.** Add below `INPUT_POLL_EVERY`:

```rust
/// Matches herdr's own `mouse_scroll_lines` default of 3.
const MOUSE_SCROLL_LINES: u16 = 3;
```

Add directly below `apply_key`:

```rust
/// The card-list mutation for one mouse event (spec §3). Returns whether
/// state changed: mouse events arrive in floods, so an inert click or a
/// saturated scroll must not cost a redraw.
fn apply_mouse(
    mouse: MouseEvent,
    it: &mut Interaction,
    rendered: &Rendered,
    viewport: u16,
    total: usize,
) -> bool {
    if viewport == 0 {
        return false;
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) if mouse.modifiers.is_empty() => {
            if mouse.row >= viewport {
                return false;
            }
            let line = it.offset as usize + mouse.row as usize;
            let Some((id, hit)) = card_at(&rendered.spans, line) else {
                return false;
            };
            let id = id.to_string();
            match hit {
                Hit::Header => {
                    it.cursor = Some(id.clone());
                    if !it.toggled.remove(&id) {
                        it.toggled.insert(id);
                    }
                    it.follow = true;
                    true
                }
                // follow stays off: `ensure_visible` would scroll a
                // partially visible card into view and pin an oversized
                // one to its header — moving content under the pointer.
                // And a repeated click on the already-selected body is a
                // no-op that must say so (spec §3 dirty discipline).
                Hit::Body => {
                    let changed = it.cursor.as_deref() != Some(id.as_str()) || it.follow;
                    it.cursor = Some(id);
                    it.follow = false;
                    changed
                }
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let before = it.offset;
            it.offset = match mouse.kind {
                MouseEventKind::ScrollUp => it.offset.saturating_sub(MOUSE_SCROLL_LINES),
                _ => clamp_scroll(
                    it.offset.saturating_add(MOUSE_SCROLL_LINES),
                    total,
                    viewport,
                ),
            };
            if it.offset == before {
                return false;
            }
            it.follow = false;
            true
        }
        _ => false,
    }
}
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test sidebar::tui -- --nocapture`
Expected: PASS (new tests and the whole existing tui table).

- [ ] **Step 6: Commit**

```bash
cargo fmt && git add src/sidebar/tui.rs && git commit -m "feat(sidebar): apply_mouse — header toggles, body selects, wheel scrolls"
```

---

### Task 6: `route_mouse` gate + the `Event::Mouse` arm

**Files:**
- Modify: `src/sidebar/tui.rs` — add `route_mouse` directly below `route`, add the event arm in `run()`'s input match, tests.

**Interfaces:**
- Consumes: `apply_mouse` (Task 5), `Live.mouse` (Task 3), `Dialog`.
- Produces: `fn route_mouse(mouse: MouseEvent, open: &Option<Dialog>, mouse_enabled: bool, it: &mut Interaction, rendered: &Rendered, viewport: u16, total: usize) -> bool`.

- [ ] **Step 1: Write the failing tests** — append inside `mod tests`:

```rust
    #[test]
    fn no_mouse_event_reaches_the_card_list_while_a_panel_is_open() {
        let rendered = two_cards();
        let mut it = Interaction::default();
        let open = Some(keys_dialog());
        assert!(!route_mouse(click(0), &open, true, &mut it, &rendered, 20, 40));
        assert_eq!(it.cursor, None);
        assert!(it.toggled.is_empty());
    }

    #[test]
    fn no_mouse_event_acts_while_the_setting_is_off() {
        // Spec §3: capture can be physically stuck on after a failed
        // disable; events from a stuck capture must not mutate cards.
        let rendered = two_cards();
        let mut it = Interaction::default();
        assert!(!route_mouse(click(0), &None, false, &mut it, &rendered, 20, 40));
        assert_eq!(it.cursor, None);
    }

    #[test]
    fn an_open_gate_delegates_to_apply_mouse() {
        let rendered = two_cards();
        let mut it = Interaction::default();
        assert!(route_mouse(click(0), &None, true, &mut it, &rendered, 20, 40));
        assert_eq!(it.cursor.as_deref(), Some("a"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test sidebar::tui::tests::no_mouse_event -- --nocapture`
Expected: FAIL to compile — `cannot find function route_mouse`.

- [ ] **Step 3: Implement.** Add directly below `route`:

```rust
/// The mouse counterpart of `route`'s starvation rule, plus the off
/// switch (spec §3). A delegation seam rather than inline in the event
/// arm so the gate is unit-testable the way `route` is.
#[allow(clippy::too_many_arguments)]
fn route_mouse(
    mouse: MouseEvent,
    open: &Option<Dialog>,
    mouse_enabled: bool,
    it: &mut Interaction,
    rendered: &Rendered,
    viewport: u16,
    total: usize,
) -> bool {
    if open.is_some() || !mouse_enabled {
        return false;
    }
    apply_mouse(mouse, it, rendered, viewport, total)
}
```

In `run()`'s input match (currently `Ok(Event::Key(key)) => { … }`, `Ok(Event::Resize(_, _)) => dirty = true,`, `Err(_) => return 0,`, `Ok(_) => {}`), insert between the `Key` and `Resize` arms:

```rust
            Ok(Event::Mouse(mouse)) => {
                if route_mouse(
                    mouse,
                    &open,
                    live.mouse,
                    &mut it,
                    &last_rendered,
                    viewport,
                    total,
                ) {
                    dirty = true;
                }
            }
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test sidebar::tui -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add src/sidebar/tui.rs && git commit -m "feat(sidebar): route mouse events through a starvation gate"
```

---

### Task 7: `TerminalGuard` capture lifecycle

**Files:**
- Modify: `src/sidebar/tui.rs` — `TerminalGuard` struct (new `mouse: bool` field), `enter()` (initialize it), a new generic impl block with `set_mouse`, the generic `Drop` impl (capture off first), tests with a flaky writer.

**Interfaces:**
- Consumes: nothing new.
- Produces: `fn set_mouse(&mut self, on: bool) -> std::io::Result<()>` on `impl<W: Write, D: FnMut() -> std::io::Result<()>> TerminalGuard<W, D>`; `Drop` emits `DisableMouseCapture` before `LeaveAlternateScreen` whenever the flag is set. Task 8 calls `set_mouse`.

- [ ] **Step 1: Write the failing tests** — append inside `mod tests`:

```rust
    /// A writer the test can make fail on demand, with a shared view of
    /// everything successfully written.
    #[derive(Clone)]
    struct FlakyWriter {
        fail: std::rc::Rc<std::cell::Cell<bool>>,
        wrote: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    }

    impl FlakyWriter {
        fn new() -> Self {
            Self {
                fail: std::rc::Rc::new(std::cell::Cell::new(false)),
                wrote: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            }
        }

        fn written(&self) -> String {
            String::from_utf8_lossy(&self.wrote.borrow()).into_owned()
        }
    }

    impl Write for FlakyWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.fail.get() {
                return Err(std::io::Error::other("flaky"));
            }
            self.wrote.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail.get() {
                return Err(std::io::Error::other("flaky"));
            }
            Ok(())
        }
    }

    fn test_guard(writer: FlakyWriter) -> TerminalGuard<FlakyWriter, fn() -> std::io::Result<()>> {
        TerminalGuard {
            output: writer,
            disable_raw_mode: || Ok(()),
            mouse: false,
        }
    }

    #[test]
    fn drop_disables_capture_before_leaving_the_alternate_screen() {
        let writer = FlakyWriter::new();
        let mut guard = test_guard(writer.clone());
        guard.set_mouse(true).expect("enable");
        drop(guard);
        let bytes = writer.written();
        let disable = bytes.find("?1000l").expect("disable-capture bytes");
        let leave = bytes.find("?1049l").expect("leave-alternate-screen bytes");
        assert!(disable < leave, "capture off strictly before leave-alt");
    }

    #[test]
    fn drop_without_capture_never_emits_a_disable() {
        let writer = FlakyWriter::new();
        let guard = test_guard(writer.clone());
        drop(guard);
        assert!(!writer.written().contains("?1000l"));
        assert!(writer.written().contains("?1049l"));
    }

    #[test]
    fn the_conservative_flag_survives_failures_and_never_short_circuits() {
        let writer = FlakyWriter::new();
        let mut guard = test_guard(writer.clone());

        // A failed enable still marks "may be applied" (spec §2).
        writer.fail.set(true);
        assert!(guard.set_mouse(true).is_err());
        assert!(guard.mouse, "flag set before the attempt");

        // A failed disable does not clear it.
        assert!(guard.set_mouse(false).is_err());
        assert!(guard.mouse, "cleared only after a successful disable");

        // Recovery emits bytes: a new `on` request writes the enable
        // sequence even though the flag already reads true.
        writer.fail.set(false);
        writer.wrote.borrow_mut().clear();
        guard.set_mouse(true).expect("re-enable");
        assert!(
            writer.written().contains("?1000h"),
            "no equality short-circuit on the conservative flag"
        );

        // And a successful disable finally clears it.
        guard.set_mouse(false).expect("disable");
        assert!(!guard.mouse);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test sidebar::tui::tests::the_conservative_flag -- --nocapture`
Expected: FAIL to compile — `no field mouse`, `no method set_mouse`.

- [ ] **Step 3: Implement.** Struct gains the field:

```rust
struct TerminalGuard<W: Write = std::io::Stdout, D: FnMut() -> std::io::Result<()> = DisableRawMode>
{
    output: W,
    disable_raw_mode: D,
    /// Conservative "capture may be applied" (spec §2): set before an
    /// enable is attempted, cleared only after a successful disable.
    mouse: bool,
}
```

`enter()`'s `Ok(Self { … })` gains `mouse: false,`. Add a new impl block below `impl TerminalGuard { … }` — on the GENERIC impl, so test guards over in-memory writers have the method:

```rust
impl<W: Write, D: FnMut() -> std::io::Result<()>> TerminalGuard<W, D> {
    /// Every scheduled transition writes its bytes — no equality
    /// short-circuit on `self.mouse`, because after a failed disable the
    /// flag reads true while reporting may be partially off (spec §2).
    fn set_mouse(&mut self, on: bool) -> std::io::Result<()> {
        if on {
            self.mouse = true;
            crossterm::execute!(&mut self.output, crossterm::event::EnableMouseCapture)
        } else {
            crossterm::execute!(&mut self.output, crossterm::event::DisableMouseCapture)?;
            self.mouse = false;
            Ok(())
        }
    }
}
```

`Drop` becomes:

```rust
impl<W: Write, D: FnMut() -> std::io::Result<()>> Drop for TerminalGuard<W, D> {
    fn drop(&mut self) {
        // Capture off strictly first: leaking capture into the user's
        // shell is the worst failure this feature can add (spec §2).
        if self.mouse {
            let _ = crossterm::execute!(&mut self.output, crossterm::event::DisableMouseCapture);
        }
        let _ = crossterm::execute!(&mut self.output, crossterm::terminal::LeaveAlternateScreen);
        let _ = (self.disable_raw_mode)();
    }
}
```

(The re-exec sites already call `drop(guard)` explicitly, so this single cleanup covers both exit paths — do not add another.)

Also update the EXISTING test `dropping_the_terminal_guard_leaves_the_screen_and_raw_mode` (near the bottom of the file, ~line 2183): its literal `TerminalGuard { output: &mut output, disable_raw_mode: || { … } }` gains the new field — add `mouse: false,` — or this task's own test run fails on the missing field.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test sidebar::tui -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add src/sidebar/tui.rs && git commit -m "feat(sidebar): conservative mouse-capture lifecycle in TerminalGuard"
```

---

### Task 8: Once-per-change reconcile in `run()`

**Files:**
- Modify: `src/sidebar/tui.rs` — pure `mouse_transition` helper + table test, `run()` wiring (`let mut guard`, `mouse_attempted` state, loop-top reconcile, failure notice).

**Interfaces:**
- Consumes: `set_mouse` (Task 7), `Live.mouse` (Task 3), `Interaction.notice`.
- Produces: `fn mouse_transition(requested: bool, last_attempted: Option<bool>) -> Option<bool>`.

- [ ] **Step 1: Write the failing test** — append inside `mod tests`:

```rust
    #[test]
    fn the_reconciler_attempts_once_per_requested_change() {
        // Startup None forces exactly one attempt, whatever the request.
        assert_eq!(mouse_transition(true, None), Some(true));
        assert_eq!(mouse_transition(false, None), Some(false));
        // A repeated tick with the same request does nothing…
        assert_eq!(mouse_transition(true, Some(true)), None);
        assert_eq!(mouse_transition(false, Some(false)), None);
        // …and each change attempts exactly once, success or not.
        assert_eq!(mouse_transition(false, Some(true)), Some(false));
        assert_eq!(mouse_transition(true, Some(false)), Some(true));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test sidebar::tui::tests::the_reconciler -- --nocapture`
Expected: FAIL to compile — `cannot find function mouse_transition`.

- [ ] **Step 3: Implement.** Add near `apply_mouse`:

```rust
/// One attempt per change of the requested value (spec §2 rule 1),
/// keyed on the request — never on the guard's conservative flag, which
/// diverges forever after a failed disable and would retry every tick.
fn mouse_transition(requested: bool, last_attempted: Option<bool>) -> Option<bool> {
    (last_attempted != Some(requested)).then_some(requested)
}
```

In `run()`: change `let guard = match TerminalGuard::enter() {` to `let mut guard = match TerminalGuard::enter() {`. Next to `let mut dirty = true;` add:

```rust
    let mut mouse_attempted: Option<bool> = None;
```

Immediately after the `loop {` line (before `let mut reopen = None;`) insert:

```rust
        // Requested-vs-attempted reconcile (spec §2 rule 1): the settings
        // toggle takes effect this iteration, startup applies the loaded
        // config exactly once, and a failing terminal is never hammered.
        if let Some(requested) = mouse_transition(live.mouse, mouse_attempted) {
            mouse_attempted = Some(requested);
            if guard.set_mouse(requested).is_err() && requested {
                // Intent stands (spec §5): live.mouse stays true, the file
                // keeps what the user asked for; only the notice reports.
                it.notice = Some("mouse capture failed".into());
                dirty = true;
            }
        }
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test sidebar::tui -- --nocapture`
Expected: PASS (the wiring compiles; behavior is covered by the pure helper plus Task 7's guard tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add src/sidebar/tui.rs && git commit -m "feat(sidebar): reconcile mouse capture once per requested change"
```

---

### Task 9: Keys-sheet hint outside the key contract

**Files:**
- Modify: `src/sidebar/tui.rs` — `Dialog::len` (`Keys` arm), `panel_for` (`Dialog::Keys` arm), test.

**Interfaces:**
- Consumes: `KEYS` (unchanged, stays 9 entries), `Row::Entry`, `panel_for`.
- Produces: the `?` sheet renders one trailer row; `the_sheet_and_the_driven_table_describe_the_same_keys` passes untouched.

- [ ] **Step 1: Write the failing test** — append inside `mod tests`:

```rust
    #[test]
    fn the_keys_sheet_carries_the_mouse_hint_outside_the_key_contract() {
        let live = live_default();
        let cfg = crate::sidebar::config::Loaded::from_missing();
        let panel = panel_for(&keys_dialog(), &live, &cfg, 0);
        assert_eq!(panel.rows.len(), KEYS.len() + 1);
        assert_eq!(keys_dialog().len(), KEYS.len() + 1, "scroll bounds cover the trailer");
        let Some(crate::sidebar::dialog::Row::Entry { label, value, .. }) = panel.rows.last()
        else {
            panic!("the trailer is an entry row");
        };
        assert_eq!(label, "mouse (when on)");
        assert_eq!(value, "click selects · header click toggles · wheel scrolls");
    }
```

(A `let-else` on the borrowed last row, deliberately — it needs no `Debug` on `Row`. If `Row` is imported under a different path in the test module, match the existing imports; the invariant is the row count, the trailer text, and `len()`.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test sidebar::tui::tests::the_keys_sheet_carries -- --nocapture`
Expected: FAIL — `panel.rows.len()` is `KEYS.len()`.

- [ ] **Step 3: Implement.** `Dialog::len`'s `Keys` arm becomes:

```rust
            // KEYS plus the mouse hint trailer the panel appends.
            Dialog::Keys { .. } => KEYS.len() + 1,
```

`panel_for`'s `Dialog::Keys` arm — the `rows` expression becomes:

```rust
            rows: KEYS
                .iter()
                .map(|(key, what)| Row::Entry {
                    label: (*key).into(),
                    value: (*what).into(),
                    enabled: false,
                })
                .chain(std::iter::once(Row::Entry {
                    // Outside KEYS on purpose: it names no key, so the
                    // sheet↔routed invariant stays exact (spec §4).
                    label: "mouse (when on)".into(),
                    value: "click selects · header click toggles · wheel scrolls".into(),
                    enabled: false,
                }))
                .collect(),
```

The dialog renders entry rows as two padded columns, so the hint displays as label + description, not one `·`-joined string. Amend the spec's §4 wording to match what actually renders — in `docs/superpowers/specs/2026-08-30-sidebar-mouse-support-design.md`, replace:

```
**Discoverability**: the `?` sheet gains one static hint line —
`mouse (when on) · click selects · header click toggles · wheel scrolls`
— rendered by the keys panel as a trailer, NOT appended to `KEYS`.
```

with:

```
**Discoverability**: the `?` sheet gains one static trailer row — label
`mouse (when on)`, description
`click selects · header click toggles · wheel scrolls`, rendered by the
keys panel in its normal two-column layout — NOT appended to `KEYS`.
```

and include the spec file in this task's commit.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test sidebar::tui -- --nocapture`
Expected: PASS — including `the_sheet_and_the_driven_table_describe_the_same_keys`, unchanged.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add src/sidebar/tui.rs docs/superpowers/specs/2026-08-30-sidebar-mouse-support-design.md && git commit -m "feat(sidebar): mouse hint on the keys sheet, outside the key contract"
```

---

### Task 10: Documentation + full verification

**Files:**
- Modify: `README.md` (the `## Configuration` section, line ~125)

- [ ] **Step 1: Document the key.** Read `README.md`'s `## Configuration` section and add a `mouse` entry alongside the other `[cards]` keys, exactly:

```markdown
- `cards.mouse` (default `false`) — capture the mouse in the sidebar: click a card header to expand/collapse it, click a body to select, scroll with the wheel. Off by default because capture takes native drag-to-select text away from the pane.
```

(Match the section's existing list formatting; if it uses a table, add a table row with the same three facts: key, default, one-line description.)

- [ ] **Step 2: Full suite + format**

Run: `cargo fmt && cargo test`
Expected: all tests pass; `git status` shows only `README.md` changed (plus regenerated gitignored `bindings/`).

- [ ] **Step 3: Manual smoke test (the only step needing a human)**

Run in a herdr pane: `cargo build --release && herdr plugin action invoke open-sidebar --plugin herdr-agent-watcher` — then in the sidebar: `x` → Settings → toggle `mouse` to `on` → click a card header (expands), click its body (selects, no collapse), wheel (scrolls), toggle `mouse` off → clicks go inert and drag-select works again. Quit the sidebar and confirm the shell is not left with mouse capture (typing and selection behave normally).

- [ ] **Step 4: Commit**

```bash
git add README.md && git commit -m "docs: document cards.mouse in the configuration section"
```

- [ ] **Step 5: Release-gate reminders (spec §6 rollout — actions at release time, not now).** Record these two items wherever the release is tracked, and honor them in the release that ships this feature:

1. Run `cargo run --example mouse-probe` on the other supported OS (macOS if this was implemented on Linux) inside a herdr pane before tagging.
2. Per this repo's release convention (CLAUDE.md "Releasing": notes live in the `chore: <version>` bump commit body; no separate changelog file), include a line in that body such as: `sidebar: opt-in mouse support — set [cards] mouse = true to click and scroll the card list`.

---

## Spec coverage map

- §1 decisions 1–3 → Tasks 5 (click/wheel semantics), 2–4 (opt-in key). Decision 4 → gate already passed; probe example already in-tree (`examples/mouse-probe.rs`).
- §2 event path/arm → Task 6; capture lifecycle rules 1–3 → Tasks 7–8; module deltas → Tasks 1–8.
- §3 hit-test → Task 1; click/wheel/dirty/gate → Tasks 5–6.
- §4 load/live/persist/apply → Tasks 2, 3, 4, 8; discoverability → Task 9.
- §5 requested-vs-applied, enable/disable failure, stuck capture → Tasks 7, 8, 6; degenerate viewport → Task 5.
- §6 probe → done pre-plan (2026-08-30, recorded in spec).
- §7 test list → distributed across Tasks 1–9 exactly as specified.
