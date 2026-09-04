# Sidebar trace navigation — design spec

Date: 2026-09-01
Status: complete
Scope: `herdr-agent-watcher sidebar` subcommand only; no daemon changes; no
`src/agent/**` changes.
Builds on: `docs/superpowers/specs/2026-08-30-sidebar-mouse-support-design.md`
(shipped in v0.2.5).

## 1. Overview, goals, and decisions

**Problem.** The TRACES section of an expanded card is display-only: a
fixed window of recent tool calls (`trace_lines`, default 5) with no way to
focus it, walk it, or see what a call actually did. The store retains **50
calls per pane** and each carries an args preview the UI shows only as a
single width-truncated line — the complete retained preview is never
visible.

**Goal.** Make traces a first-class interaction surface: descend into an
expanded card's traces with `l`, walk rows with `j`/`k`, return with `h`;
open the selected trace with `o`/Enter into a **detail panel** showing
tool, status, timestamp, duration, and the complete retained `args`
preview — unwrapped across lines instead of today's single truncated row,
pretty-printed when it parses as JSON; and extend v0.2.5's mouse semantics
one level deeper — clicking a trace row selects it, clicking the selected
row opens its detail.

**Data reality (scopes this spec).** What reaches the sidebar is NOT the
raw tool-call arguments: adapters emit a provider-specific args *preview*
capped at 1,024 characters (for many tools just the command, path, or
pattern — e.g. `claude_code/transcript.rs` and `codex/transcript.rs`
previews), and the daemon sanitizes that string before it enters the
50-call ring. It is not guaranteed to be JSON or complete. **Result
content does not exist in the pipeline at all** — only status and
`duration_ms` — and the ring holds **settled calls only** (`done` /
`failed`): the store tracks `running` in a private open-set and never
publishes it into `tool_calls`, so every navigable row is a finished
call. The detail panel therefore
renders exactly the retained preview plus metadata, honestly labelled;
carrying raw bounded arguments or result content crosses into the
frozen-port adapters, the ts_rs event schema, and store truncation
policy, and both are explicitly a **follow-up spec**.

**Non-goals (v1).** Raw/uncapped argument capture and result-content
capture (above); mouse interaction
*inside* the detail panel (dialogs stay key-driven, matching v0.2.5);
trace search/filtering; any daemon or `src/agent/**` change — this remains
sidebar-only.

**Decisions.**

1. *`l`/`h` descend and return* — the new pair; `j`/`k` and `o`/Enter keep
   their meanings one level down. `esc`/`q` keep their existing global
   meaning (close panel / sidebar); `h` is the only way back up, so no
   overload.
2. *Focused traces show the whole ring* — while focused, the section
   renders **all retained calls (≤50)** instead of `trace_lines`,
   reverting on exit. Navigating 5-of-50 was not worth building;
   `trace_lines` stays the *unfocused* window size.
3. *Detail is a dialog panel* — reuses the existing `Panel` machinery
   (scroll, `esc`, any payload size); while open, the routing gate starves
   list input exactly as v0.2.5 specified.
4. *Mouse: click selects, click-again opens* — single-line rows have no
   header/body split, so the card rule adapts: selection first, action on
   the already-selected row. Requires sub-card geometry export (trace-row
   spans) and an **id-based** trace hit-test beside the v0.2.5 card
   hit-test (contract in Section 3).

## 2. Focus model & state machine

**State.** `Interaction` gains one field: `trace_focus: Option<String>` —
the **`toolUseId`** of the selected trace, not a row index. The ring
mutates under a live agent (new calls push in, old ones fall off), so an
index would silently retarget; the id is the pipeline's own stable,
ring-unique key. Rendered order stays newest-first. **Ring-unique means
per pane only**: the logical identity of a selection is always the pair
(anchor card = `it.cursor`, trace id = `trace_focus`), and every
comparison downstream — click-again, selection rendering — tests both
halves, never the id alone.

**Invariant.** `trace_focus.is_some()` implies `it.cursor` names a card
that is present *and expanded* (`auto_expand XOR toggled`, the v0.2.5
rule). The existing cursor-reconcile step extends: if the anchor card
disappears, collapses, or the focused id has no row anymore,
`trace_focus` degrades gracefully — an id gone from the ring snaps to the
nearest surviving row (newest side); a focused card whose ring is *empty*
(reachable: `replace_session` on rebind and replay summaries both swap in
an empty ring while the card stays present and expanded) drops focus to
the card zone; card gone/collapsed likewise resets focus to `None`, with
the cursor behaving exactly as today. `trace_focus` therefore always
denotes a real rendered row. Rows lacking a `toolUseId` (replay
summaries copy unvalidated shapes into the ring) are **display-only**:
they render as today but export no span, cannot be selected, and
`j`/`k` skip over them.

**Transitions.**

- `l` (card zone, cursor on an expanded card with ≥1 **selectable** —
  id-bearing — row): enter traces, select the newest selectable row (a
  newest id-less row is skipped over). Collapsed card, no selectable
  rows (including an all-id-less ring), or no cursor → inert.
- `h` (trace zone): back to the card zone; cursor unchanged. `h` in the
  card zone stays inert; `l` in the trace zone is inert (nothing deeper).
- `j`/`k` (trace zone): move selection by rendered row, clamped at both
  ends — no wrap, matching the card list.
- `o`/Enter (trace zone): request the detail panel for the selected
  trace — routing yields an `OpenTrace` outcome and the run loop
  resolves and installs it (mechanism in Section 3). The panel renders a
  **snapshot** taken at open — if the call falls off the ring while the
  panel is up, the panel keeps displaying its copy; no live mutation
  under the reader.
- `z`, `x`, `?`, `q`/`esc`, `ctrl-c`: unchanged and global in both zones.
  While any panel is open, the starvation rule covers keys and mouse
  identically (v0.2.5).

**Rendering while focused** (geometry in Section 3): the focused card's
traces section shows the full ring (Decision 2). The selected-row
visibility guarantee is conditional, mirroring the card contract exactly:
`l`/`j`/`k` set `follow = true` and the next draw keeps the selected
trace row on screen (`ensure_visible` extended to row granularity); the
wheel still detaches (`follow = false`, redraw uses `reanchor`), and a
detached view may legitimately leave the selected row off-screen until
keyboard navigation resumes.

**No new wire data.** The state stream already delivers the ring; this
section changes only in-process interaction state.

## 3. Geometry export & mouse extension

**Span export.** `Rendered` gains
`trace_spans: Vec<(String, String, LineSpan)>` — `(card_id, tool_use_id,
row span)` for every trace row the view actually rendered: the
`trace_lines` window on unfocused cards, the full ring on the focused
one. Computed in the same pass that builds card spans; same coordinate
space (content lines). The view renders **at most one row per id**: if
the ring ever carries duplicates (buggy provider), only the newest
occurrence renders or exports a span, so a duplicate can never be
clicked, and the run-loop resolver resolves against the same
canonicalized sequence.

**Hit-test** (`layout.rs`, pure): a sibling
`trace_at(trace_spans, line) -> Option<(&str, &str)>` returning
`(card_id, tool_use_id)`. `card_at` is unchanged. Resolution order in
`apply_mouse`: `trace_at` first, then `card_at` — a trace row is *inside*
a card's span, so the more specific hit must win.

**Click rules** (modifier-free `Down(Left)`, extending v0.2.5's table):

- Trace-row hit whose card matches `it.cursor` **and** whose id matches
  `trace_focus` (the pair test, Section 2) → an `OpenTrace` outcome for
  its detail panel.
- Trace-row hit otherwise → `cursor = that card`,
  `trace_focus = that id`, `follow = true` — the mouse deep-selects in
  one click, including across cards. The clicked row's *screen position*
  is stable only when no focused card shrinks above it (first entry from
  the card zone, or clicks within the already-focused card): expansion
  appends older rows below the newest-first window. A **cross-card**
  deep-select additionally reverts the previously focused card to its
  `trace_lines` window, and when that card sits above the target the
  clicked row shifts up — a documented reflow, not a defect. The
  selection itself is always correct, and the row-granularity follow
  pulls the selected row into view on the next draw; a second click
  simply needs re-aiming after a cross-card reflow.
- Header/body clicks keep their v0.2.5 semantics, with the Section-2
  invariant enforced: a body click that moves the cursor to another card,
  or a header click that collapses the anchor card, clears `trace_focus`.
- Wheel: unchanged — scrolls the list, detaches.

**Outcome plumbing.** Neither `apply_mouse` nor `apply_key` touches
`State` or the dialog slot, and that stays true: opening a detail panel
is expressed as data. `apply_mouse`'s return grows from `bool` into a
small outcome (changed / unchanged / `OpenTrace { card_id, tool_use_id
}`), and key routing gains the matching `KeyOutcome` variant for
`o`/Enter in the trace zone. The **run loop is the single resolver**: it
looks the pair up in `State`, clones the call into the Section-2
snapshot, installs the panel, and marks dirty. Resolution happens in the
same loop iteration as the input — the loop is single-threaded, so the
ring cannot change in between; if the pair is somehow absent at
resolution, the outcome degrades to selection-only (no panel, no crash).
The resolver also enforces the existing small-frame refusal
(`MIN_DIALOG_WIDTH`/`MIN_DIALOG_HEIGHT`, same notice wording as the
`x`/`?` branch) — today that check lives only in the `x`/`?` key path,
and without it here a too-small frame would install an unreadable panel
that then starves all list input. Refusal keeps the selection and
installs nothing.

**Selection rendering.** The trace row whose (card, id) pair matches
(`it.cursor`, `trace_focus`) renders reversed (the card-cursor
convention); no other styling changes. Rows remain full-width targets;
columns never matter.

**Dirty discipline** carries over verbatim: every new click outcome
reports whether state changed, and a repeated deep-select of the
already-focused row is *not* a no-op — it opens the panel — so the only
inert trace clicks are misses and modified clicks.

## 4. Detail panel & help surface

**Panel.** A new `Dialog::TraceDetail` variant holding the Section-2
snapshot — card id + agent label, `tool`, `status`, `timestamp`,
`duration_ms`, the retained args preview, `tool_use_id` — plus the
cursor/offset the `Panel` machinery already uses for scrolling. Title:
`Trace — <tool>`. Rows: a status line (existing status glyph + word + a
duration from a **new compact duration formatter** — `format::age` is an
absolute-delta formatter that reports every sub-minute interval as
"now" and cannot express durations; the new contract is
`0..1000ms → "NNNms"` including `"0ms"`, `1s..60s → one-decimal
seconds`, `≥60s → "MmSSs"`), a when line (absolute timestamp plus age —
`age` IS correct there), a rule, then the **args body**: pretty-printed
via `serde_json` when the preview parses as JSON, verbatim otherwise.
The body renders as `Row::Text(String)` — one new variant, nothing else
in the panel grammar changes — which **preserves hard newlines and
wraps width-aware**. Scroll correctness is load-bearing: `dialog::render`
flattens wrapped content into display lines and interprets `offset` in
that flattened space, while the existing key routing bounds offsets by
logical `row_count()`. `TraceDetail` therefore renders with
`cursor: None` (pure offset scrolling) and bounds its offset by a
**width-aware rendered-line count** (`dialog::line_count(panel, width)`)
— otherwise most of a 1,024-character preview is unreachable. The width
fed to `line_count` is the **shared effective panel width**: one
function computes it for both the draw site (which clamps to
`area.width.min(60)`) and the offset bounds — feeding routing's
unclamped frame width would undercount wrapped lines on terminals wider
than 60 columns and recreate the unreachable tail. Footer:
`j/k scroll · esc close`.

**Close semantics.** `esc`/`q` closes the panel only — the trace zone,
selection pair, and full-ring rendering survive, so close-and-reopen
lands where you left. This needs its **own routing arm**: the existing
generic dialog branch returns non-menu panels to `Dialog::menu()` on
close, which is wrong here — `TraceDetail` closes to `None` directly
(it was never entered through the menu), leaving cursor and
`trace_focus` untouched.

**Keys sheet.** `l` ("into traces") and `h` ("back to cards") join
`KEYS` *and* `routed()`. Growing the fixture with traces alone is not
enough to prove them: routed cases seed only a card cursor today, and
the before/after comparison never looks at trace focus — so `h` would
start in the wrong zone and `l`'s effect would be invisible. The routed
table gains an optional **initial trace-focus seed** per case, and
`trace_focus` joins the compared state, making both keys observably
active rather than sheet-only decoration. The `o / ↵` description widens
to "expand a card, or open the selected trace".

**Mouse hint trailer** (v0.2.5) updates to: label `mouse (when on)`,
value `click selects · header toggles · trace re-click opens · wheel
scrolls` — and the v0.2.5 test that asserts the exact trailer string
updates with it, called out here so the change is deliberate, not drift.

## 5. Failure modes & edge cases

**Anchor-loss matrix** (all funnel into Section 2's reconcile): pane
unbind mid-focus → `trace_focus` clears; the card cursor rehomes by the
existing nearest-index `reconcile_cursor` rule (it resets to `None` only
when no cards remain). `z` (hide idle)
while focused on an idle card → the card leaves the list → focus drops
with it. `auto_expand` flipped in the settings panel → on panel close
the anchor may now be collapsed → focus drops. No path may leave
`trace_focus` pointing at an unrendered row — that is the Section 2
invariant restated as the test surface.

**Ring churn while focused.** New calls push the selected row visually
downward; the id anchor means selection *identity* never drifts. While
attached (`follow = true`), **every dirty draw** keeps the selected row
visible — churn cannot push it off-screen; only a wheel-detached view
lets rows slide away, and the next `l`/`j`/`k` reattaches. Duplicate
ids are handled *before* geometry exists: the view canonicalizes
newest-first and renders at most one row per id (Section 3), so
hit-testing and selection are never ambiguous — pinned by a plan test.

**Open panel is immune to churn** — the snapshot rule; the panel never
live-updates (no text jumps under a reading user). A `running` state
can never appear in it: the ring holds settled calls only (Section 1),
so every openable trace is finished and the status row shows `done` or
`failed`, nothing else.

**Empty or hostile previews.** An empty retained preview renders
`(no arguments retained)` — the panel is never blank. Ring entries are
untyped JSON, so every metadata field follows the trace-row fallback
conventions the view already uses: unknown tool renders `?`, a missing
duration omits its segment, a missing timestamp renders `—` on the when
line — the resolver never unwraps malformed metadata. Previews are
treated strictly as data: rendered as plain text rows, never
interpreted (the daemon's sanitize pass is the trust boundary; the view
adds no second parser beyond the optional `serde_json` pretty-print
attempt, whose failure just means verbatim display).

**Degenerate frames.** `l`/`h`/`j`/`k` mutate state, not geometry, so
they work at any size; row-granularity `ensure_visible` keeps the
existing `viewport == 0` early return; panel opening at tiny sizes is
already refused by the Section 3 resolver gate.

## 6. Testing

Test-first, inline `#[cfg(test)]`, no e2e-tier changes, no
TypeScript-bindings impact (nothing ts_rs-exported changes).

**`layout.rs`** — `trace_at` boundary table: hit, miss between rows,
past-end, empty spans.

**`view.rs`** — span export: `trace_lines` window vs full ring by
focus; newest-first **dedup-by-id** (a ring with duplicates renders
once); id-less rows render but export no span; the selected-pair row
renders reversed, and only when the card also matches the cursor.

**`tui.rs` interactions** — `apply_mouse` table: deep-select
(same-card, and cross-card with focus retarget), pair-match click-again
yields `OpenTrace`, a trace hit beats a card-body hit, misses and
modified clicks inert with no state change, **and both card-click
focus-clearing branches** — a body click that changes cards clears
`trace_focus`, a header click that collapses the anchor clears it, each
exercised with the *same* `toolUseId` present on two cards so pair
leakage cannot masquerade as valid selection. Key table: `l`
(expanded+selectable / collapsed / empty / **all-id-less** /
**newest-id-less-mixed** / no-cursor), `h` in both zones, `j`/`k`
clamped and skipping id-less rows, `o` yielding `OpenTrace`, and
**TraceDetail close**: `esc` and `q` each land on `open == None` (not
the menu) with cursor and `trace_focus` unchanged. Reconcile matrix:
unbind (cursor rehomes, focus clears), collapse, `auto_expand` flip,
empty-ring swap, **focused-id eviction with survivors** (newest-side
snap), and churn redraw both ways — attached keeps the selected row
visible on the very next draw, wheel-detached does not.

**Routed/KEYS invariant** — extended cases seed trace focus;
`trace_focus` joins the compared state; the exact mouse-trailer string
assertion updates.

**Dialog** — `Row::Text` preserves hard newlines and wraps width-aware;
`line_count` bounds `TraceDetail` scrolling through the **shared
effective panel width**, exercised on a frame wider than 60 columns so
the clamp mismatch cannot regress (a 1,024-character preview's last
line is reachable); `cursor: None`.

**`format`** — duration boundary table: `0ms`, `999ms`, `1.0s`,
`59.9s`, `1m00s`, `2m05s`.

**Resolver** — the **happy path first**: a valid `OpenTrace` resolves
the canonical call, clones it, and installs `Dialog::TraceDetail`; the
ring is then mutated and the installed snapshot is asserted unchanged.
Then the failure paths: small-frame refusal keeps selection; an absent
pair degrades to selection-only; snapshot fallbacks (`?`, `—`, omitted
segments) for malformed metadata.
