# Sidebar mouse support — design spec

Date: 2026-08-30
Status: complete
Scope: `herdr-agent-watcher sidebar` subcommand only; no daemon changes.

## 1. Overview, goals, and decisions

**Problem.** The Agent Watcher sidebar is keyboard-only. Users inspecting a
wall of agent cards must `j`/`k` to a card and press `o` to expand it; there
is no pointer path at all — mouse capture was never enabled.

**Goal.** Let a user opt into mouse interaction: click a card's header to
select *and* toggle it (identical state change to pressing `o`), click a
card's body to select without collapsing it, and scroll the card list with
the wheel. Everything a click can do remains reachable by key; the mouse is
an accelerator, never the only path.

**Non-goals (v1).** Clickable rows inside dialogs (settings/doctor/
keybindings panels keep their own key handlers and do not export row
geometry); drag interactions; click-to-focus of the underlying agent pane;
any daemon-side change — this feature is entirely within the `sidebar`
subcommand.

**Decisions.**

1. *Header toggles, body selects* — clicking inside an expanded card's body
   must not collapse it under the pointer.
2. *Opt-in, default off* — enabling capture takes native drag-to-select text
   away from the pane; users who copy costs/titles out of the sidebar keep
   that by default. Config key `[cards] mouse`, toggleable live from the
   settings panel.
3. *Click + wheel ship together* — once capture is on, the terminal stops
   handling the wheel itself, so a click-only cut would silently kill
   scrolling.
4. *Host dependency acknowledged* — the sidebar lives in a herdr pane; herdr
   must forward SGR mouse sequences for capture to see anything. A
   five-minute probe gates implementation (Section 6).

**Approaches considered and rejected.** (a) Translating mouse events into
synthetic key presses — a click needs a target ("toggle the card at row
14"), and the key handler has no vocabulary for targets, so the translation
layer would smuggle in hit-testing anyway while making tests lie about what
the user did. (b) Hit-testing inside the view/render layer — the view is
deliberately pure (`ViewInput` in, `Rendered` out; the shell converts the
contained lines to ratatui lines) and the shell owns the terminal; mouse
geometry belongs where scroll geometry already lives (`layout.rs`), with a
pure `apply_mouse` sibling to `apply_key` in the shell.

## 2. Architecture & event flow

**Event path.** herdr forwards SGR mouse sequences to the pane's PTY;
crossterm parses them into `Event::Mouse` in the same `event::read()` the
loop already drains. One new match arm routes them:

```
SGR bytes ─▶ crossterm ─▶ Event::Mouse
                            │  dialog open, or live.mouse off?
                            │    ──▶ ignored (panel starvation; a stuck
                            ▼        capture must not act when off)
                   apply_mouse(mouse, &mut it, &last_rendered,
                               viewport, total)
                            │  row < viewport ─▶ content line =
                            │    it.offset + row ─▶ card_at(spans, line)
                            ▼
             Interaction mutation (cursor / toggled / offset / follow)
                            ─▶ dirty ─▶ redraw
```

**Which events act.** Only `Down(Left)`, `ScrollUp`, `ScrollDown`.
Everything else (`Moved`, `Drag`, `Up`, other buttons) is ignored *without*
setting `dirty` — capture makes the terminal emit a flood of move events,
and redrawing on each would peg the loop.

**Frame-coherence invariant.** The loop is single-threaded and redraws
(when dirty) before polling again, so at poll time `last_rendered`,
`viewport`, and `it.offset` always describe what is actually on screen. A
click therefore hit-tests against current geometry — the same invariant key
routing already relies on, now stated because mouse is the first input
where coordinates matter. Rows at or below `viewport` land in the pinned
footer region and are inert.

**Capture lifecycle.** `TerminalGuard` gains a tracked `mouse: bool` and a
`set_mouse(on)` method issuing `EnableMouseCapture`/`DisableMouseCapture`,
defined on the generic `impl<W: Write, D: FnMut() -> io::Result<()>>`
block where `Drop` already lives — not beside the concrete `enter()` — so
guards built over an in-memory writer in tests have the method at all.
The flag means "capture may be applied" and is tracked conservatively: set
*before* attempting enable, cleared only after a *successful* disable — a
partially written enable can never strand live capture behind a false
flag. `set_mouse` never short-circuits on `self.mouse == on`: every
scheduled transition (rule 1 below) issues its command bytes regardless
of the conservative flag, because after a failed disable the flag reads
`true` while reporting may be partially off — an equality guard would
silently suppress the re-enable. Three rules:

1. The run loop keys reconciliation on a third piece of state it owns:
   `last_attempted: Option<bool>`, `None` at startup so the initial
   requested value gets exactly one attempt. A transition is attempted
   iff `last_attempted != Some(live.mouse)` — never by comparing the
   request to the guard's conservative flag, which diverges forever
   after a failed disable and would make a naive comparison retry every
   tick.
   Each attempt, successful or not, records
   `last_attempted = Some(requested)`. The settings toggle still takes
   effect immediately, and a failing terminal is never hammered
   (failure semantics in Section 5).
2. `Drop` issues `DisableMouseCapture` whenever the flag is set, *before*
   `LeaveAlternateScreen`. A spurious disable is an idempotent no-op; a
   skipped one leaks capture into the user's shell — the worst failure
   this feature can add.
3. The update flow's re-exec path already restores the terminal before
   `exec` via an explicit `drop(guard)` (destructors never run on a
   successful exec). `Drop` therefore remains the single idempotent
   cleanup that both exit paths funnel through; nothing may bypass it.

**Module deltas.** `layout.rs`: pure hit-test (`card_at`, contract in
Section 3). `tui.rs`: event arm, `apply_mouse`, guard methods,
reconcile-on-tick. `config.rs`, `live.rs`, `settings_file.rs`: the
`[cards] mouse` load → live-toggle → save path (contracts in Section 4).
`view.rs`: none — `Rendered.spans` already carries `(pane_id, LineSpan)`
per card.

## 3. Interaction semantics

**Hit-test contract** (`layout.rs`, pure):

```rust
pub enum Hit { Header, Body }
pub fn card_at(spans: &[(String, LineSpan)], line: usize)
    -> Option<(&str, Hit)>
```

Returns the card whose span contains `line`
(`start <= line < start + height`); `Hit::Header` exactly when
`line == start`. Lines inside no span — separator rows between cards,
anything past the last card — return `None`. Columns never matter: every
card row is a full-width target.

**Click rules** (`Down(Left)`, modifier-free only — modified clicks stay
inert so Shift-click keeps meaning "native terminal selection" where the
emulator offers it):

- Row in the pinned footer region (`row >= viewport`) → inert.
- `line = it.offset + row`; `card_at` miss → inert, cursor unchanged (keys
  never clear the cursor; neither does the mouse).
- `Hit::Header` → select the target (`cursor = Some(id)`), then apply the
  same toggle `o` performs (flip `id` in `toggled`), and `follow = true` —
  after a toggle the card legitimately changes shape, and the follow
  scroll reveals its new extent.
- `Hit::Body` → `cursor = Some(id)` only; `toggled` untouched;
  `follow = false`, offset untouched. Setting `follow` here would route
  the next draw through `ensure_visible`, which scrolls a partially
  visible card into full view and pins an oversized card to its header —
  moving content under the pointer. Leaving the view detached keeps the
  frame exactly where the user clicked; later content shifts re-anchor
  relative to the now-selected card (`reanchor`).

**Wheel rules** (`ScrollUp`/`ScrollDown`, anywhere in the pane, any row):
move `it.offset` by **3 lines** per notch (matching herdr's own
`mouse_scroll_lines` default) through `clamp_scroll`; when the offset
actually moves, `follow = false` — the wheel detaches the view exactly
like `↑`/`PageUp` do. Wheel does not scroll dialogs (non-goal, Section 1).

**Dirty discipline.** `apply_mouse` returns whether state changed; the
loop redraws only then. Keys may keep their unconditional redraw — they
are rare — but mouse events arrive in floods, and an inert click or
saturated-scroll must not cost a frame.

**Routing gate.** `route()` accepts a `KeyEvent` and stays key-only. The
event loop's new `Event::Mouse` arm is a delegation one-liner into a
`route_mouse()` sibling, which drops the event before `apply_mouse` is
ever called when a panel is open (`open.is_some()`) — same starvation
rule as keys — **or** when `live.mouse` is off. The helper exists so the
gate is unit-testable the way `route()` is. The second condition is load-bearing: capture can be
physically stuck on after a failed disable (Section 5), and events from a
stuck capture must not mutate cards while the setting says off. Closing
panels by clicking outside them is explicitly out of scope for v1.

## 4. Config, settings & help surface

**Load** (`config.rs`): `Loaded` gains `mouse: bool`, default `false`.
`read_cards` accepts `mouse = true|false`; any other value records
`problem("invalid value for cards.mouse")` and the default stands — same
tolerance as every other key, surfaced through the existing config-notice
line.

**Live state** (`live.rs`): `Live.mouse: bool` copied from `Loaded` at
startup (and again when the update flow re-execs the sidebar). There is no
config-reload loop: after startup the settings panel mutates `Live`
directly and persists through `settings_file`. New `Setting::Mouse`; `SETTINGS` grows 11 → 12, inserted
after `PlanUsage` so the cards-group rows stay clustered (auto expand,
tool calls, trace lines, plan usage, mouse) before the appearance group.
Label `"mouse"`, displayed value `"on"`/`"off"`, `l`/`h` flips the bool —
identical ergonomics to `hide idle`.

**Persist** (`settings_file.rs`): `Setting::Mouse => ("cards", "mouse")`
in `table_and_key`, boolean branch in `item_for` (`value(live.mouse)`), so
the panel toggle writes `mouse = true` under `[cards]` via `toml_edit`
without disturbing the rest of the document.

**Apply** (`tui.rs`): the toggle takes effect the same tick through the
Section-2 reconcile rule — no restart, no reopen. On startup, capture is
applied after `TerminalGuard::enter()` only when the loaded config says
on.

**Discoverability**: the `?` sheet gains one static hint line —
`mouse (when on) · click selects · header click toggles · wheel scrolls`
— rendered by the keys panel as a trailer, NOT appended to `KEYS`. The
`KEYS` table stays key-only so the load-bearing
`the_sheet_and_the_driven_table_describe_the_same_keys` invariant (every
`KEYS` label appears in `routed()`) is untouched; the panel's `len()`
accounts for the extra line, and the hint is exempt from the routed-key
contract because it names no key. The settings row itself is the switch.
The pinned key footer does not change: it is the most space-starved
surface in the pane, and a disabled-by-default feature does not earn a
permanent line there.

## 5. Failure modes & edge cases

**Requested vs applied.** `live.mouse` is the *requested* state: what the
user asked for, what the settings row displays, and what `settings_file`
persists. The guard's conservative flag is the *applied* state. The two
are never conflated — a capture failure must not silently rewrite the
user's intent through the `value(live.mouse)` save path.

**Enable fails.** Crossterm cannot detect "terminal does not support
mouse": `EnableMouseCapture` only writes ANSI mode sequences and gets no
acknowledgement, so an unsupported emulator is the silent-no-events case
below, and an `Err` is an *output-write* failure. Handling: keep
`live.mouse = true` (intent stands — the row shows `on`, the file keeps
`true`), set `it.notice` to "mouse capture failed" best-effort (if stdout
is truly broken the next `terminal.draw` panics today through its
`expect("draw sidebar")` — terminal-gone handling covers the input side
only, and this spec does not change draw-error handling), and do not
retry until the requested value changes again — the once-per-change
reconcile rule from Section 2.

**Disable fails.** The same bounded rule: one attempt per requested
change, with the conservative flag left set so `Drop` retries the disable
on exit. In between, the routing gate (Section 3) already drops every
mouse event because `live.mouse` is off — a physically stuck capture
cannot mutate cards.

**Resize.** Covered by the frame-coherence invariant: the resize event
marks the frame dirty, the redraw recomputes `viewport` and spans, and
only then is the next event polled — a click after a resize always tests
against post-resize geometry.

**Degenerate viewport.** `viewport == 0` (pane shrunk to the pinned
rows): every row is pinned-region, clicks inert; the wheel takes the same
`viewport == 0` early-return the arrow keys already have.

**No events arrive.** A host that does not deliver SGR mouse reports
makes the feature a silent no-op: capture requested, nothing to route.
Herdr is documented to be the good case — its default config states that
pane applications "can still receive mouse when they request it" even
while herdr's own `mouse_capture` handling is disabled
(`tests/fixtures/herdr-default-config.toml`) — and Section 6's probe
verifies that empirically before implementation begins.

**Wheel compatibility when off.** Terminals (herdr included) often
translate wheel motion in an alternate-screen app into arrow keys
("alternate scroll"). That is how the sidebar scrolls under the wheel
*today*, and it keeps working untouched when `mouse = off` — enabling
capture merely switches the wheel's transport from synthesized arrows to
real mouse events.

**No double-click semantics.** Every `Down(Left)` stands alone; two fast
header clicks toggle twice, returning to the start state. Idempotent, no
timing state.

## 6. Host probe & rollout gate

**Probe.** A ~40-line diagnostic example, `examples/mouse-probe.rs`
(sibling to the existing `examples/probe.rs`), kept in-tree as a support
tool rather than thrown away. It reproduces the sidebar's exact terminal
lifecycle — raw mode, *enter the alternate screen*, then
`EnableMouseCapture` — because wheel transport differs between primary
and alternate screens (Section 5), and a primary-screen probe could
green-light a mode the sidebar never runs in. It prints each
`Event::Mouse` as one line (`kind`, `column`, `row`, `modifiers`) inside
the alternate screen, and on `q`/`Ctrl-C` restores in the sidebar's
order (capture off → leave alternate screen → raw-mode off — the probe
must not leak capture either), then prints a per-kind event count
summary to the primary screen so results survive the screen switch.

**Procedure & pass criteria.** First run `cargo run --example
mouse-probe` directly in ghostty: this control must pass before any
herdr run is scored — if the control fails, the probe or the emulator
assumption is the suspect, not herdr. Then run it inside a herdr pane
under the default config, and again with `ui.mouse_capture = false` —
applied with `herdr server reload-config` after editing the file (the
same live transition the repo's keybinding installer uses), and
reverted plus reloaded afterward; without the reload, both herdr runs
can silently exercise the same state. Pass = `Down(Left)` with **empty
modifiers** (Section 3 discards modified clicks, so a host-supplied
phantom modifier must fail the gate, not pass it), plus `ScrollUp` and
`ScrollDown`, all with sane 0-based in-pane coordinates, in both herdr
cases. herdr's default config documents that pane apps receive mouse on
request, so this verifies documented behavior — but the sidebar is not
lazygit, and five minutes of proof beats an assumption baked into a
feature branch.

**Gate.** Implementation does not start until the probe passes. Failure
routing follows the control: if the ghostty control fails, fix the probe
or the emulator assumption first — herdr is not implicated. If the
control passes and a herdr run fails, the work item converts into a
herdr-side forwarding investigation and this spec goes dormant — nothing
in Sections 2–5 is worth building against a host that will not deliver
the events. A partial pass routes the same way: missing wheel events
cannot be manufactured client-side, and the sidebar cannot correct
host-global coordinates — the decoded `PaneInfo` carries no pane
rectangle (`src/herdr/api.rs`) to transform against. Both shapes are
herdr-side findings. Only if that investigation yields a concrete
pane-local transform (and this spec is amended to obtain the rect) does
a Section 3 geometry change become an option.

**Rollout.** Ships default-off inside a normal release; no migration, no
config written on upgrade. The changelog line and README's settings
table mention `[cards] mouse`. Platform note: the plugin already
declares `platforms = ["macos", "linux"]`, crossterm's SGR handling is
portable across both, and the probe run on the other OS before the
release tag is the cheap cross-check.

## 7. Testing

Written test-first, red-green per contract, matching the repo's inline
`#[cfg(test)]` habit. No e2e-tier changes — tiers A/B exercise the
daemon, and this feature never leaves the sidebar. No TypeScript bindings
impact: no event payload types change, so the `ts_rs` surface is
untouched.

**`layout.rs` — `card_at`** (pure): header row hits `Hit::Header`; last
body row hits `Hit::Body`; `start + height` (one past the card) misses; a
gap line between two cards misses; a line past all content misses; empty
span list misses.

**`tui.rs` — `apply_mouse`** against the existing `two_cards()` fixture,
mirroring the `apply_key` table: header click retargets cursor, flips
`toggled`, sets `follow`; body click retargets cursor only, `follow`
becomes false, offset untouched; second header click on the same card
un-toggles (idempotence); click through a non-zero `it.offset` resolves
the scrolled card (the arithmetic test); pinned-region click, gap click,
and modified click are inert *and report no state change*; wheel moves
offset by 3 through `clamp_scroll` and clears `follow` only when the
offset actually moved; wheel at the clamp boundary reports no change;
`viewport == 0` short-circuits.

**Routing gate**: `route_mouse()` (the arm's delegate, Section 3) returns
without reaching `apply_mouse` when a dialog is open or `live.mouse` is
off — unit-tested directly, mirroring the existing
`no_key_reaches_the_card_list_while_a_panel_is_open` pattern.

**`TerminalGuard`**: `set_mouse` lives on the generic
`impl<W: Write, D>` block (Section 2), so guards over test writers have
it. A capturing `Vec<u8>` writer asserts byte order — disable-capture
bytes precede leave-alternate-screen in `Drop`, and appear only when the
flag is set. `Vec<u8>` never fails, so the conservative error rules take
a failing-`Write` double (a writer that errors on demand): enable
failure leaves the flag set; disable failure does not clear it; and the
failed-disable recovery emits bytes — disable fails, then a new `on`
request writes enable bytes anyway, proving `set_mouse` has no equality
short-circuit.
`last_attempted` reconcile logic is a pure function with its own table:
startup `None` attempts once, repeated ticks do not, each requested
change attempts exactly once regardless of outcome —
`last_attempted != Some(live.mouse)` is the executable predicate.

**Config & settings**: `read_cards` accepts `true`/`false`, records
`problem` and keeps the default on anything else; `settings_file` writes
`("cards", "mouse")` without disturbing the document (existing
`toml_edit` round-trip pattern); `Setting::Mouse` cycles on/off;
`SETTINGS` covers the new row; and the existing
`live_is_seeded_from_the_loaded_config` test extends to assert the
`Loaded.mouse` → `Live.mouse` hop — without it, every other test here
can pass while `[cards] mouse = true` still starts with capture off.

**Help sheet**: the trailer line renders, `KEYS` is unchanged, and
`the_sheet_and_the_driven_table_describe_the_same_keys` passes as-is —
the trailer's exemption is the test's proof that non-key hints do not
corrupt the key contract.
