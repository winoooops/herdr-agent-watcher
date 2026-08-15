# Agent adapter — iterative Codex critique log

> **Tracking issue:** [#246 — `refactor(agent-adapter): split monolithic trait into composable services`](https://github.com/winoooops/vimeflow/issues/246) (the single context anchor for all implementation PRs)
> **Status:** ✅ **Converged after 5 rounds** (Round 5: "No new findings").
> **Reviewer:** codex-cli 0.133.0 (default model, ChatGPT-account auth)
> **Subject:** the refactor proposal in [`README.md`](./README.md)
> **Date range:** 2026-05-22 → 2026-05-23

This file is an iterative-review log. Each round critiques the latest
plan; findings get folded into a revised plan; the next round critiques
the revision. Round 5 returned the stop signal.

**Jump to the conclusion:** [Final plan (v4-frozen)](#final-plan-v4-frozen) ↓

Iteration shape:

| Round | Findings | Plan version  | Nature                                                   |
| ----- | -------- | ------------- | -------------------------------------------------------- |
| 1     | 9        | v1            | Architectural — entire concepts missing                  |
| 2     | 5        | v2            | Specification gaps                                       |
| 3     | 5        | v3            | Contract ambiguity + scope/visibility                    |
| 4     | 3        | v4            | Tactical — locator wording, visibility scope, error-type |
| 5     | **0**    | **v4-frozen** | **Convergence**                                          |

## Findings

### 1. A first can freeze the wrong transcript-path boundary

`ParsedStatus.transcript_path` is not purely decoded state for Codex; it is
a side channel from `status_source` into `parse_status` via
`resolved_rollout_path` in [`codex/mod.rs:85`](./codex/mod.rs), then
consumed by `base` in
[`base/watcher_runtime.rs:320`](./base/watcher_runtime.rs).

> 💡 IDEA
>
> - **I — Intent:** A wants `parse_status` to become a simple
>   schema-to-event conversion.
> - **D — Danger:** landing A around the current `ParsedStatus` shape
>   preserves the Codex mutex coupling and makes B's "stateless
>   `StateDecoder`" harder, not easier.
> - **E — Explain:** the rollout path belongs to location/session
>   binding, not JSON state decoding.
> - **A — Alternatives:** move transcript path into `StatusSource` or an
>   attach/session context **before** A, then make state decoding return
>   only status state.

### 2. `StateDecoder::decode(raw)` drops required context

Current decoding needs the Vimeflow session id to stamp emitted status
events: see `parse_status(&self, session_id, raw)` in
[`mod.rs:34`](./mod.rs), Claude event construction in
[`claude_code/statusline.rs:52`](./claude_code/statusline.rs), and Codex
`into_event(session_id)` in [`codex/parser.rs:36`](./codex/parser.rs).

> 💡 IDEA
>
> - **I — Intent:** B tries to make state decoding independent and
>   reusable.
> - **D — Danger:** a decoder returning `ParsedStatus` cannot be
>   context-free unless session stamping moves elsewhere.
> - **E — Explain:** provider JSON contains the agent session id, but the
>   frontend event needs the PTY session id.
> - **A — Alternatives:** decode to provider-neutral status data, then
>   have runtime/session code attach `session_id`, or pass an explicit
>   decode context.

### 3. A's serde risk is misstated — the real risk is null/missing/JSONL tolerance

Serde structs **ignore** unknown fields by default, contrary to
[`README.md:197`](./README.md); the real risk is required fields and enum
variants failing where current code defaults, skips, or preserves prior
state.

> 💡 IDEA
>
> - **I — Intent:** make schemas discoverable without losing upstream
>   tolerance.
> - **D — Danger:** Claude `used_percentage: null` only survives if
>   conversion preserves the computed/clamped fallback in
>   [`claude_code/statusline.rs:97`](./claude_code/statusline.rs), and
>   Codex `info: null` only survives if the fold does not clear prior
>   token state in [`codex/parser.rs:195`](./codex/parser.rs).
> - **E — Explain:** Codex is JSONL, not a JSON document; current parsing
>   skips incomplete trailing lines and malformed mid-lines in
>   [`codex/parser.rs:18`](./codex/parser.rs).
> - **A — Alternatives:** use loose DTOs with `#[serde(default)]`,
>   catch-all enum variants, and line-by-line fallible parsing; keep the
>   existing fixture tests as contract tests.

### 4. Codex location does not fit `locate(cwd, sid)`

Codex binding needs agent pid and PTY start time from [`mod.rs:148`](./mod.rs),
stored on `CodexAdapter` in [`codex/mod.rs:24`](./codex/mod.rs), while
`BindContext` explicitly excludes `session_id` in
[`codex/types.rs:7`](./codex/types.rs).

> 💡 IDEA
>
> - **I — Intent:** make location a replaceable concern.
> - **D — Danger:** the proposed locator trait either hides attach-time
>   state inside objects again or cannot implement Codex correctly.
> - **E — Explain:** Codex uses SQLite, `/proc` resume args, open rollout
>   fds, cwd, and PTY timing; see
>   [`codex/locator.rs:281`](./codex/locator.rs) and
>   [`codex/locator.rs:465`](./codex/locator.rs).
> - **A — Alternatives:** introduce an `AttachContext` carrying
>   `session_id`, cwd, shell pid, agent pid, PTY start, and provider
>   home/proc roots.

### 5. Path security is a separate service, not part of `TranscriptStreamer`

Status-source trust enforcement happens in `base` before watching in
[`base/mod.rs:32`](./base/mod.rs), and transcript validation feeds
structured diagnostics in
[`base/watcher_runtime.rs:153`](./base/watcher_runtime.rs).

> 💡 IDEA
>
> - **I — Intent:** keep unsafe provider paths out of the runtime.
> - **D — Danger:** folding `validate` into `TranscriptStreamer` hides a
>   security boundary and leaves status-source trust as an orphan
>   concern.
> - **E — Explain:** `ValidateTranscriptError` variants drive `TxOutcome`
>   classification, not just adapter-local failure handling.
> - **A — Alternatives:** make `PathPolicy` or
>   `TranscriptPathValidator` explicit, and keep `StatusSource { path,
trust_root }` verification in the session/runtime layer.

### 6. `TranscriptState` is coupled to adapter spawning and lock ordering

`TranscriptState::start_or_replace` accepts `Arc<dyn AgentAdapter>` and
calls `tail_transcript` inside a carefully gated replace flow in
[`base/transcript_state.rs:101`](./base/transcript_state.rs) and
[`base/transcript_state.rs:168`](./base/transcript_state.rs).

> 💡 IDEA
>
> - **I — Intent:** split streaming from the rest of the adapter trait.
> - **D — Danger:** B touches concurrency-critical lifecycle code,
>   including the old-before-new stop invariant tested in
>   [`base/transcript_state.rs:412`](./base/transcript_state.rs).
> - **E — Explain:** `TranscriptHandle` is only the visible leak; the
>   registry also relies on adapter cloning, per-session start gates, and
>   replace semantics.
> - **A — Alternatives:** first change `TranscriptState` to depend on
>   `Arc<dyn TranscriptStreamer>` while preserving its tests, then split
>   the outer adapter.

### 7. CWD is a live runtime input, not attach-time data

`base` intentionally re-reads cwd from `PtyState` on every transcript
start attempt in
[`base/watcher_runtime.rs:137`](./base/watcher_runtime.rs) and
[`base/watcher_runtime.rs:184`](./base/watcher_runtime.rs), and
`TranscriptState` treats cwd changes as replacement identity in
[`base/transcript_state.rs:155`](./base/transcript_state.rs).

> 💡 IDEA
>
> - **I — Intent:** keep test-run parsing scoped to the live workspace
>   cwd.
> - **D — Danger:** a facade that captures cwd at attach time regresses
>   mid-session `cd` handling and test-run snapshots.
> - **E — Explain:** Claude and Codex transcript parsers both use cwd
>   for command matching and snapshot building.
> - **A — Alternatives:** model cwd as a live
>   `SessionContext`/`CwdProvider`, or leave cwd refresh in the runtime
>   layer and document it as part of the facade contract.

### 8. The `AgentSession` value facade does not match the IPC lifetime model

IPC only passes `sessionId` for start/stop in
[`runtime/ipc.rs:462`](../../../runtime/ipc.rs), while `BackendState`
owns shared `PtyState`, `AgentWatcherState`, `TranscriptState`, and
`EventSink` in [`runtime/state.rs:181`](../../../runtime/state.rs).

> 💡 IDEA
>
> - **I — Intent:** hide adapter internals from the IPC layer.
> - **D — Danger:** `detach(self)` is not usable when the caller never
>   receives or stores an `AgentSession` object.
> - **E — Explain:** current lifetime ownership is registry-based:
>   `AgentWatcherState` stores handles and `stop_agent_watcher_inner`
>   removes by session id.
> - **A — Alternatives:** build an
>   `AgentWatcherService`/`AgentSessions` facade with
>   `start(session_id)` and `stop(session_id)`, **not** a returned RAII
>   session object.

### 9. Estimates are optimistic because the proposal crosses tested behavior seams

A is only ~1 day if it is status-only; full "schema-first" touches Claude
transcript parsing, Codex transcript parsing, and Codex status folding
across roughly **2.8k lines** of parser/tailer code.

> 💡 IDEA
>
> - **I — Intent:** sequence small refactors with low regression risk.
> - **D — Danger:** A+B+D together touch `statusline.rs`, both
>   `transcript.rs` files, `parser.rs`, `locator.rs`,
>   `watcher_runtime.rs`, `transcript_state.rs`, and IPC state wiring.
> - **E — Explain:** many tests encode edge behavior: nulls, partial
>   JSONL lines, malformed lines, cwd replacement, duplicate-tail
>   prevention, path-security diagnostics.
> - **A — Alternatives:** add a **seam-prep step first**: move transcript
>   path out of `ParsedStatus`, introduce `AttachContext`, and preserve
>   the current runtime tests before converting parsers to typed DTOs.

## Synthesis — three recurring themes

1. **The proposal under-modelled "attach-time context".** Findings #1, #2,
   #4, #7 all converge on the same gap: there is no `AttachContext` /
   `SessionContext` carrying `{ session_id, cwd, shell_pid, agent_pid,
pty_start, provider_home, proc_root }` as a single bag. Without it,
   every split has to invent its own arg shape, and Codex's stateful
   adapter survives the refactor unchanged.

2. **A's tolerance story was wrong.** Finding #3 corrects a factual error
   in the README: serde ignores unknown fields by default, so the real
   risk is **preserving null/missing/computed-fallback behavior**, not
   rejecting unknown fields. DTOs must be loose (`#[serde(default)]`,
   catch-all enum variants, line-by-line fallible parsing).

3. **D's shape was wrong.** Findings #6, #7, #8 all push back on
   `AgentSession` as a RAII value type. The real IPC model is registry
   start/stop by `session_id`; D should be an `AgentWatcherService`, not
   a returned `Self`. CWD must stay a live runtime input, not captured
   at attach.

## Revised sequence (proposed)

| Step | Original            | Revised                                                                                                                                                                | Why                                                 |
| ---- | ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------- |
| 0    | _(none)_            | **Seam prep**: introduce `AttachContext` and move `transcript_path` out of `ParsedStatus` (into `StatusSource` or `AttachContext`). Keep all existing tests passing.   | Addresses #1, #2, #4, #7 before any wider refactor. |
| A'   | Schema-first serde  | Same intent, but loose DTOs (`#[serde(default)]`, catch-all variants) + line-by-line fallible parsing + existing fixtures become contract tests.                       | Addresses #3.                                       |
| B'   | 3-trait split       | 4-trait split: `StatusSourceLocator`, `StateDecoder` (provider-neutral output), `TranscriptStreamer`, `TranscriptPathValidator`. Composition struct = `AgentBindings`. | Addresses #4, #5.                                   |
| B''  | _(implicit)_        | Re-shape `TranscriptState` to depend on `Arc<dyn TranscriptStreamer>` while preserving its tests, **before** finishing the outer adapter split.                        | Addresses #6.                                       |
| D'   | `AgentSession` RAII | `AgentWatcherService` registry facade with `start(session_id)` / `stop(session_id)`. CWD stays a live `PtyState` lookup.                                               | Addresses #7, #8.                                   |
| C    | Common tailer       | Unchanged — still optional, still must absorb `CompletionMode`.                                                                                                        | n/a                                                 |

The big change: **step 0 is mandatory and lands first**, decoupled from
everything else. Once the attach context exists, A' / B' / D' each get a
smaller, less invasive shape.

Revised cost estimates (conservative, after codex feedback):

- Step 0 (seam prep): **~1 day** — small but invasive; touches both
  adapters and base.
- A' (loose DTOs): **~1.5–2 days** — bigger than ~1d because it crosses
  ~2.8k lines and many edge-case tests.
- B' + B'' (split + TranscriptState re-shape): **~2–3 days** — the
  TranscriptState concurrency invariants are the hard part.
- D' (service facade): **~0.5–1 day** — simpler than original D
  because we're not inventing a RAII type.
- **Total:** ~5–7 days, vs. the original ~2.5–3.5d estimate.

---

# Round 2 (2026-05-22)

> **Reviewer:** codex-cli 0.133.0
> **Subject:** the revised plan v1 above
> **Prompt:** see "Prompts used" at the bottom.

Codex was asked to (a) verify each of the 9 Round 1 findings is actually
closed by v1's mapped step, and (b) surface any new issues introduced by
the revision. Output: a partial-closure check plus 5 new findings.

## Closure check on Round 1's revised plan (v1)

| Finding                               | Closed by v1?                                                 |
| ------------------------------------- | ------------------------------------------------------------- |
| #1 `transcript_path` side channel     | **Partial** — move not backward-compatible. See R2.1.         |
| #2 `StateDecoder` drops context       | **Partial** — Step 0 alone insufficient. See R2.2.            |
| #3 Serde tolerance wording            | **Conditional** — DTO precision required. See R2.4.           |
| #4 Codex location fit                 | **Partial** — `AttachContext` field set incomplete. See R2.3. |
| #5 Path security as own service       | **Closed.**                                                   |
| #6 `TranscriptState` adapter coupling | **Closed if B'' implemented exactly.**                        |
| #7 CWD as runtime input               | **Mostly closed**, depends on `AttachContext` wording (R2.3). |
| #8 IPC lifetime model                 | **Closed.**                                                   |
| #9 Cost realism                       | **Mostly closed**, estimates depend on gaps above.            |

## New findings (R2.1 – R2.5)

### R2.1. Step 0's `transcript_path` move is not backward-compatible

Moving `ParsedStatus.transcript_path` directly into `StatusSource` or
`AttachContext` does not fit the current watcher flow. `StatusSource` is
computed **once** in `base::start_for` at
[`base/mod.rs:32`](./base/mod.rs), but `watcher_runtime` currently
**re-discovers** transcript paths on **every** status update at
[`base/watcher_runtime.rs:320`](./base/watcher_runtime.rs),
[`base/watcher_runtime.rs:393`](./base/watcher_runtime.rs), and
[`base/watcher_runtime.rs:485`](./base/watcher_runtime.rs). Claude's path
is **dynamic JSON content** from
[`claude_code/statusline.rs:215`](./claude_code/statusline.rs), while
Codex's path is **locator output** currently smuggled through
`resolved_rollout_path` at [`codex/mod.rs:85`](./codex/mod.rs).

> 💡 IDEA
>
> - **I — Intent:** remove Codex's locator-to-parser mutex coupling
>   before schema refactors.
> - **D — Danger:** a one-time `StatusSource.transcript_path` preserves
>   Codex but regresses Claude path changes and the watcher diagnostics
>   around `tx_path`.
> - **E — Explain:** there are **two path origins** today —
>   static/source-bound for Codex, dynamic/status-payload-bound for
>   Claude. A single static field can't represent both.
> - **A — Alternatives:** introduce an explicit `TranscriptPathHint` /
>   `StatusObservation` outside status state. `StatusSource` may carry
>   a static hint; the status-parse path may carry a dynamic hint.

### R2.2. Finding #2 is not closed by Step 0 alone

`AttachContext` can carry `session_id`, but that does not make decoding
context-free. Current decoders **stamp** the Vimeflow `session_id`
**inside** `parse_status(&self, session_id, raw)` at
[`mod.rs:34`](./mod.rs), Claude event construction at
[`claude_code/statusline.rs:52`](./claude_code/statusline.rs), and Codex
`into_event(session_id)` at [`codex/parser.rs:63`](./codex/parser.rs).
The v1 table maps #2 to Step 0, but the real closure is B': decode to a
provider-neutral `StatusSnapshot`, then runtime stamps `session_id`.

> 💡 IDEA
>
> - **I — Intent:** keep `StateDecoder` reusable and not tied to PTY /
>   frontend session IDs.
> - **D — Danger:** passing `AttachContext` into `StateDecoder` just
>   renames the old context dependency and keeps decoding coupled to
>   runtime identity.
> - **E — Explain:** provider JSON has the **agent** session identity;
>   Vimeflow event identity belongs to runtime / session orchestration.
> - **A — Alternatives:** `StateDecoder::decode(raw) -> StatusSnapshot`,
>   then watcher runtime builds `AgentStatusEvent { session_id,
...snapshot }`.

### R2.3. `AttachContext` field set incomplete and wrong location

The v1 table doesn't pin down the field set. Codex `BindContext` only
carries `cwd`, `pid`, `pty_start` at
[`codex/types.rs:14`](./codex/types.rs); `/proc` is hidden inside
`SqliteFirstLocator` at [`codex/locator.rs:104`](./codex/locator.rs);
transcript validators still call `dirs::home_dir()` directly at
[`claude_code/transcript.rs:85`](./claude_code/transcript.rs) and
[`codex/transcript.rs:116`](./codex/transcript.rs). `AttachContext`
needs: `session_id`, `initial_cwd`, `shell_pid`, `agent_pid`,
`pty_start`, `agent_type`, provider roots (`codex_home`, `claude_home`
or a resolver), and `proc_root`. The type should live in a new
`adapter/attach.rs` (or `adapter/context.rs`), **not `base/`** —
locators and validators must not depend on watcher-runtime internals.

> 💡 IDEA
>
> - **I — Intent:** make attach-time facts visible so Codex locator
>   state stops leaking through adapter fields.
> - **D — Danger:** if roots / proc / session fields stay hidden in
>   provider objects, `AgentBindings` becomes the same opaque state
>   bag as today's adapter.
> - **E — Explain:** `base` owns orchestration; attach context is part
>   of the adapter contract used by locator, binding factory, and
>   tests.
> - **A — Alternatives:** split `AttachContext` (immutable attach-time
>   facts) from `SessionRuntimeContext` (live cwd from PtyState). Use
>   `initial_cwd` for binding; keep live cwd lookup in `PtyState`.

### R2.4. A' DTO precision is insufficient

`#[serde(default)]` + catch-all variants is necessary but not
sufficient. Concretely:

- Claude `used_percentage` must be `Option<f64>` with
  `#[serde(default)]`, not `f64`, so `null` reaches the computed
  fallback at
  [`claude_code/statusline.rs:97`](./claude_code/statusline.rs).
- Codex `token_count.info` must be `#[serde(default)] info:
Option<TokenCountInfoDto>`, and folding must update
  `last_token_count_info` **only on `Some(info)`** to preserve the
  prior-state behavior at [`codex/parser.rs:195`](./codex/parser.rs).
- `#[serde(default)]` does **not** tolerate wrong-typed present
  fields (e.g. integer where float expected); current `Value::as_*`
  parsing is lenient. If wrong-type tolerance matters, DTO fields
  need `deserialize_with = "..."` per field or `Value` fallback
  wrappers.

> 💡 IDEA
>
> - **I — Intent:** make schemas discoverable without turning upstream
>   partial data into parse failures.
> - **D — Danger:** plain serde DTOs can clear token state to zero on
>   `info: None`, or reject a valid-but-partial line that current code
>   silently ignores.
> - **E — Explain:** `Option<T>` handles null/missing; it does not
>   handle wrong-type drift. `default` only applies when a field is
>   absent.
> - **A — Alternatives:** define DTOs as **lossy contracts** — `Option`
>   for nullable state, catch-all enum variants, and per-field
>   tolerant helpers (`deserialize_with`) where current `Value`
>   parsing defaults.

### R2.5. B' missing explicit boundary for transcript-path extraction

`TranscriptPathValidator` validates a raw path, but it does not answer
**where the raw path came from**. The four traits fit location, status
decoding, validation, and streaming — but **not** Claude's dynamic
extraction from status JSON vs. Codex's static path from location.
Without this boundary, either `StateDecoder` keeps returning non-state
metadata, or `AgentBindings` hides another side channel.

> 💡 IDEA
>
> - **I — Intent:** split provider concerns without losing the security
>   boundary around transcript files.
> - **D — Danger:** the new traits can recreate today's
>   `ParsedStatus.transcript_path` leak under a different name.
> - **E — Explain:** **extraction** and **validation** are separate
>   concerns; validation cannot replace extraction.
> - **A — Alternatives:** add `TranscriptPathSource` /
>   `TranscriptPathResolver`; or make `StatusObservation` contain
>   `{ status_snapshot, transcript_hint }` while keeping `StateDecoder`
>   status-only.

## Revised plan v2

Key changes from v1:

1. **Step 0 splits into 0a / 0b / 0c** (AttachContext, SessionRuntimeContext, TranscriptPathSource extraction).
2. **B' becomes a 5-trait split** (adds `TranscriptPathSource`).
3. **A' gets precise DTO specs** (per-field `Option<T> + #[serde(default)]`, Some-only fold for token state, wrong-type tolerance via custom deserializers).
4. **`StateDecoder` returns `StatusSnapshot`** (no `session_id`); runtime composes `AgentStatusEvent { session_id, ...snapshot }`.

| Step    | v1                       | v2                                                                                                                                                                                                                                                                                                                        | Addresses              |
| ------- | ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------- |
| **0a**  | "Seam prep"              | Define `AttachContext` in a **new** `crates/backend/src/agent/adapter/attach.rs` (NOT `base/`). Fields: `session_id`, `initial_cwd`, `shell_pid`, `agent_pid`, `pty_start`, `agent_type`, `codex_home`, `claude_home_or_resolver`, `proc_root`. Immutable attach-time facts.                                              | R2.3                   |
| **0b**  | _(implicit)_             | Add `SessionRuntimeContext` (distinct from `AttachContext`): carries the live `PtyState` handle for cwd lookups. AttachContext = immutable; SessionRuntimeContext = live.                                                                                                                                                 | R2.3, #7               |
| **0c**  | "move transcript_path"   | Introduce `TranscriptPathSource` extraction (trait or paired types): handles Claude's **dynamic** path (from status JSON) AND Codex's **static** path (from locator). `StatusSource` may carry a static hint; `StatusObservation` (returned by decoder) may carry a dynamic hint. Removes `ParsedStatus.transcript_path`. | #1, R2.1, R2.5         |
| **A'**  | loose DTOs (vague)       | Loose DTOs with **precise** specs: `Option<T> + #[serde(default)]` on every nullable/missing field; fold must preserve token state on `info: None` (Codex); wrong-type tolerance via `deserialize_with` per field or `Value` fallback wrappers.                                                                           | #3, R2.4               |
| **B'**  | 4-trait split            | **5-trait split**: `StatusSourceLocator`, `StateDecoder` (returns `StatusSnapshot`, **no `session_id`**), `TranscriptPathSource`, `TranscriptPathValidator`, `TranscriptStreamer`. Watcher runtime composes `AgentStatusEvent { session_id, ...snapshot }`.                                                               | #2, #4, #5, R2.2, R2.5 |
| **B''** | TranscriptState re-shape | **Unchanged.** Preserve: per-session `start_gate` Mutex ([`transcript_state.rs:117`](./base/transcript_state.rs)), old-before-new stop order ([`transcript_state.rs:153`](./base/transcript_state.rs)), regression test at line 428.                                                                                      | #6                     |
| **D'**  | `AgentWatcherService`    | **Unchanged.** Owns/clones `PtyState`, `AgentWatcherState`, `TranscriptState`, `EventSink`. `start(session_id)` builds `AttachContext` from live `PtyState`; `stop(session_id)` removes the watcher.                                                                                                                      | #7, #8                 |
| **C**   | common tailer            | **Unchanged.** Still optional. Engine must support Codex `CompletionMode` + partial-line buffering.                                                                                                                                                                                                                       | n/a                    |

### Revised cost estimates (post-Round-2)

- 0a (AttachContext): **~0.5 day**
- 0b (SessionRuntimeContext): **~0.25 day**
- 0c (TranscriptPathSource extraction): **~1 day**
- A' (precise DTOs + wrong-type tolerance): **~2 days**
- B' (5-trait split + AttachContext plumbing): **~1.5 day**
- B'' (TranscriptState re-shape): **~1.5 day**
- D' (AgentWatcherService): **~0.5–1 day**
- **Total v2: ~7–9 days** (vs. v1's ~5–7d, original ~2.5–3.5d)

---

# Round 3 (2026-05-22)

> **Reviewer:** codex-cli 0.133.0
> **Subject:** the v2 plan from Round 2

Codex was asked to (a) verify closure on R2.1–R2.5, (b) regression-check
the original 9, and (c) surface any new issues introduced by v2's
reshape. Output: 5 new findings (R3.1–R3.5).

## Closure check on Round 2's revised plan (v2)

| Round 2 finding                             | Closed by v2?                                                                          |
| ------------------------------------------- | -------------------------------------------------------------------------------------- |
| R2.1 `transcript_path` move backward-compat | **Mostly.** `StatusObservation` vs `StatusSnapshot` wording still ambiguous. See R3.1. |
| R2.2 Step 0 alone insufficient for #2       | **Closed.** `StatusSnapshot` + runtime session-id stamping is correct.                 |
| R2.3 `AttachContext` field set + location   | **Mostly.** `AgentBindings` underspecified. See R3.2.                                  |
| R2.4 A' DTO precision                       | **Closed.** Per-field `Option`, Some-only fold, wrong-type tolerance covered.          |
| R2.5 transcript-path extraction boundary    | **Partial.** `TranscriptPathSource` direction right but contract unpinned. See R3.1.   |

## Regression check on original 9 findings

| Round 1 finding        | v2 status                                                                                           |
| ---------------------- | --------------------------------------------------------------------------------------------------- |
| #1, #4                 | **Conditionally closed.** Transcript-path extraction & Codex locator binding still under-specified. |
| #2, #3, #5, #6, #7, #8 | **Not re-opened.**                                                                                  |
| #9 (cost realism)      | **Improved**, but A' scope (status-only vs. full schema-first) determines whether estimate holds.   |

## New findings (R3.1 – R3.5)

### R3.1. `TranscriptPathSource`, `StatusObservation`, `StatusSnapshot` ownership overlap

v2 says `StatusObservation` returned by the decoder may carry the
dynamic hint, but it also says `StateDecoder` returns `StatusSnapshot`
only. **Those cannot both be the stable contract.** Pin one flow before
implementation: either `StateDecoder::decode(raw) -> StatusSnapshot`
**and** `TranscriptPathSource::dynamic_hint(raw, &LocatedStatusSource) -> Option<RawTranscriptPath>`,
or rename the decoder contract to return an observation that explicitly
includes both status and hint.

> 💡 IDEA
>
> - **I — Intent:** preserve Claude's per-update transcript extraction
>   and Codex's once-bound rollout path without keeping
>   `ParsedStatus.transcript_path`.
> - **D — Danger:** the current wording can recreate today's non-state
>   side channel in a new wrapper, or force duplicate raw-JSON parsing
>   for Claude.
> - **E — Explain:** the watcher extracts transcript paths on every
>   parse path at [`base/watcher_runtime.rs:320`](./base/watcher_runtime.rs);
>   Codex currently smuggles the located path through a mutex at
>   [`codex/mod.rs:85`](./codex/mod.rs).
> - **A — Alternatives:** introduce `LocatedStatusSource { status_path,
trust_root, static_transcript_hint }` plus `StatusSnapshot`; let
>   `TranscriptPathSource` select `dynamic_hint(raw)` for Claude and
>   `static_hint` for Codex.

### R3.2. `AgentBindings` needs to be specified as the per-attach owner, not just named

v2 still does not define what `AgentBindings` owns, where it lives, or
how the factory builds it from `AttachContext`. **This is where Codex's
current stateful fields in [`codex/mod.rs:24`](./codex/mod.rs) must
disappear** — otherwise the 5-trait split still hides `pid`, `pty_start`,
roots, and resolved rollout state inside provider objects.

> 💡 IDEA
>
> - **I — Intent:** replace the monolithic adapter with explicit
>   provider capabilities.
> - **D — Danger:** without a concrete binding struct, `AttachContext`
>   exists but doesn't prevent opaque per-provider state bags.
> - **E — Explain:** `start_agent_watcher_inner` currently resolves
>   attach facts at [`mod.rs:148`](./mod.rs), then constructs one
>   adapter object at [`mod.rs:151`](./mod.rs).
> - **A — Alternatives:** add `crates/backend/src/agent/adapter/bindings.rs`
>   with `AgentBindings { agent_type, locator, decoder, transcript_paths,
validator, streamer }` and `AgentBindings::for_attach(&AttachContext)
-> Result<Self, String>`.

### R3.3. B'' needs an explicit visibility decision for `TranscriptStreamer`

`TranscriptState::start_or_replace` is currently `pub` and takes
`Arc<dyn AgentAdapter>` at
[`base/transcript_state.rs:101`](./base/transcript_state.rs). If it
changes to `Arc<dyn TranscriptStreamer>`, that trait must either be
`#[doc(hidden)] pub` too, or `start_or_replace` must become
`pub(crate)`. The current `TranscriptHandle::new` remains `pub(crate)`
at [`base/transcript_state.rs:26`](./base/transcript_state.rs); that
constraint must be preserved.

> 💡 IDEA
>
> - **I — Intent:** decouple transcript lifecycle from the full adapter
>   trait while preserving tested replace ordering.
> - **D — Danger:** an unplanned visibility mix can either fail Rust's
>   public-interface checks or accidentally widen the supported
>   extension API.
> - **E — Explain:** the existing `#[doc(hidden)] pub` surface is
>   already a compromise because `TranscriptHandle` appears in a public
>   trait signature.
> - **A — Alternatives:** make `TranscriptStreamer` `#[doc(hidden)]
pub` and keep the constructor gated, **or** reduce
>   `TranscriptState::start_or_replace` to `pub(crate)` if integration
>   tests don't need it.

### R3.4. Locator split should expose only `CompositeLocator` as `StatusSourceLocator`

v2 must explicitly say that `CompositeLocator` is the Codex
`StatusSourceLocator`, while `SqliteFirstLocator` and `FsScanFallback`
remain Codex-private strategies. Making each one a top-level locator
would leak fallback policy and schema-drift dispatch out of
[`codex/locator.rs:589`](./codex/locator.rs).

> 💡 IDEA
>
> - **I — Intent:** make location replaceable without making the
>   watcher runtime understand Codex internals.
> - **D — Danger:** exposing each sub-locator separately pushes the
>   `schema drift -> fs fallback` policy into `AgentBindings` or
>   runtime code.
> - **E — Explain:** today the fallback chain is cleanly contained in
>   `CompositeLocator::resolve_rollout` at
>   [`codex/locator.rs:603`](./codex/locator.rs).
> - **A — Alternatives:** implement the new `StatusSourceLocator` only
>   for `CompositeLocator`; keep `SqliteFirstLocator` / `FsScanFallback`
>   under a Codex-private `CodexSessionLocator`.

### R3.5. The ~7–9d estimate is realistic only if A' is status-first, not full schema-first

The estimate is now much closer to reality, but A' still reads like
the original "schema-first refactor" could include transcript tailers.
If A' includes Claude AND Codex transcript DTO conversion, **2 days is
still optimistic** given the stateful tail loops and Codex
`CompletionMode` paths in
[`codex/transcript.rs:28`](./codex/transcript.rs).

> 💡 IDEA
>
> - **I — Intent:** give the refactor a believable budget after adding
>   seam prep.
> - **D — Danger:** undercounting A' recreates the original risk: one
>   step touches status parsing, transcript parsing, watcher flow, and
>   fixture contracts.
> - **E — Explain:** status DTOs are bounded; full transcript DTO
>   migration crosses both transcript tailers plus partial-line and
>   replay behavior.
> - **A — Alternatives:** **split A' into `A-status` (before B') and
>   `A-transcript` (after B''/D')**, or explicitly defer transcript
>   DTOs.

## Revised plan v3

Key changes from v2:

1. **Pin the contract** between `StatusSource` / `StatusSnapshot` / `TranscriptPathSource`:
   - `LocatedStatusSource { status_path, trust_root, static_transcript_hint }`
   - `StatusSnapshot` (decoder output; status-only — no `session_id`, no transcript hint)
   - `TranscriptPathSource` with `static_hint(&LocatedStatusSource)` (Codex) and `dynamic_hint(raw)` (Claude) methods
2. **Specify `AgentBindings` concretely** in `crates/backend/src/agent/adapter/bindings.rs`: holds `{ agent_type, locator, decoder, transcript_paths, validator, streamer }`, built by `AgentBindings::for_attach(&AttachContext) -> Result<Self, String>`.
3. **Lock down visibility**: `TranscriptStreamer` is `#[doc(hidden)] pub`; `TranscriptState::start_or_replace` narrows to `pub(crate)` if tests don't need it.
4. **Constrain locator split**: only `CompositeLocator` implements `StatusSourceLocator`; `SqliteFirstLocator` / `FsScanFallback` stay Codex-private.
5. **Split A' into `A-status` (early) and `A-transcript` (late)**:
   - **`A-status`** lands **before B'** — status DTOs only (`statusline.rs` + `parser.rs` state-fold).
   - **`A-transcript`** lands **after D'** — transcript JSONL DTOs (`claude_code/transcript.rs` + `codex/transcript.rs`). Must preserve Codex `CompletionMode` + partial-line buffering.

### v3 ordering

| Step                          | What                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | Addresses                          |
| ----------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------- |
| **0a**                        | Define `AttachContext` in `adapter/attach.rs` (immutable attach facts).                                                                                                                                                                                                                                                                                                                                                                                                                                  | R2.3                               |
| **0b**                        | Define `SessionRuntimeContext` (live `PtyState` handle for cwd lookups).                                                                                                                                                                                                                                                                                                                                                                                                                                 | R2.3, #7                           |
| **0c**                        | Introduce `LocatedStatusSource { status_path, trust_root, static_transcript_hint }` + `StatusSnapshot` + `TranscriptPathSource` with explicit `static_hint(&LocatedStatusSource) -> Option<RawPath>` (Codex) and `dynamic_hint(raw) -> Option<RawPath>` (Claude) methods. Remove `ParsedStatus.transcript_path` side channel.                                                                                                                                                                            | R2.1, R2.5, #1, R3.1               |
| **A-status**                  | Loose DTOs for STATUS only (`statusline.rs` + `parser.rs` state-fold). Per-field `Option<T> + #[serde(default)]`, Some-only fold for Codex token state, `deserialize_with` for wrong-type tolerance. **Defer transcript DTOs to `A-transcript`.**                                                                                                                                                                                                                                                        | #3, R2.4, R3.5                     |
| **B'**                        | 5-trait split, each `#[doc(hidden)] pub`: `StatusSourceLocator`, `StateDecoder` (returns `StatusSnapshot`), `TranscriptPathSource`, `TranscriptPathValidator`, `TranscriptStreamer`. **Only `CompositeLocator` implements `StatusSourceLocator`**; `SqliteFirstLocator` / `FsScanFallback` stay Codex-private. Define `AgentBindings { agent_type, locator, decoder, transcript_paths, validator, streamer }` in `adapter/bindings.rs` with `AgentBindings::for_attach(&AttachContext) -> Result<Self>`. | #2, #4, #5, R2.2, R2.5, R3.2, R3.4 |
| **B''**                       | TranscriptState re-shape to `Arc<dyn TranscriptStreamer>`. Narrow `start_or_replace` to `pub(crate)` if integration tests don't need it. Preserve concurrency invariants ([`transcript_state.rs:117`](./base/transcript_state.rs), [`transcript_state.rs:153`](./base/transcript_state.rs), test at line 428).                                                                                                                                                                                           | #6, R3.3                           |
| **D'**                        | `AgentWatcherService` registry facade. Unchanged shape.                                                                                                                                                                                                                                                                                                                                                                                                                                                  | #7, #8                             |
| **A-transcript** _(deferred)_ | Loose DTOs for TRANSCRIPT JSONL (`claude_code/transcript.rs` + `codex/transcript.rs`). Must preserve Codex `CompletionMode` + partial-line buffering. Land AFTER D'.                                                                                                                                                                                                                                                                                                                                     | R3.5                               |
| **C** _(optional)_            | Common tailer engine. Lands after `A-transcript`. Must still absorb Codex `CompletionMode` + partial-line buffering.                                                                                                                                                                                                                                                                                                                                                                                     | n/a                                |

### Revised cost estimates (post-Round-3)

**Core path (0a → 0b → 0c → A-status → B' → B'' → D'): ~6–8 days**

- 0a (AttachContext): ~0.5d
- 0b (SessionRuntimeContext): ~0.25d
- 0c (`LocatedStatusSource` + `StatusSnapshot` + `TranscriptPathSource`): ~1d
- A-status (status DTOs only): ~1d
- B' (5-trait split + `AgentBindings` + locator scope): ~1.5d
- B'' (TranscriptState re-shape + visibility): ~1.5d
- D' (AgentWatcherService): ~0.5–1d

**Optional extensions:**

- A-transcript (transcript JSONL DTOs, after D'): +1.5–2d
- C (common tailer engine, after A-transcript): +1.5–2d

**Full delivery: ~9–12 days. Core-only: ~6–8 days.**

---

# Round 4 (2026-05-23)

> **Reviewer:** codex-cli 0.133.0
> **Subject:** the v3 plan from Round 3

Codex was asked to (a) verify closure on R3.1–R3.5, (b) regression-check
all prior findings, and (c) surface any new issues introduced by v3's
contract-pinning. Output: **3 tactical findings** — meaningfully smaller
than rounds 1–3, signaling convergence.

## Closure check on Round 3's revised plan (v3)

| Round 3 finding                                                       | Closed by v3?                                                                                                                       |
| --------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| R3.1 ownership overlap (`StatusSnapshot` / source / hint)             | **Closed.** `LocatedStatusSource` + `StatusSnapshot` + `TranscriptPathSource::{static_hint,dynamic_hint}` pin the contract cleanly. |
| R3.2 `AgentBindings` concrete spec                                    | **Mostly closed.** Spec exists, but locator-wording in R4.1 affects it.                                                             |
| R3.3 B'' visibility decision for `TranscriptStreamer`                 | **Closed on explicitness.** R4.2 pushes back on the _choice_ itself.                                                                |
| R3.4 locator scope (only `CompositeLocator` is `StatusSourceLocator`) | **Mostly closed.** R4.1 corrects the wording — Claude still needs a (trivial) locator.                                              |
| R3.5 A' split into `A-status` / `A-transcript`                        | **Closed.** Estimate and step ordering hold up.                                                                                     |

## Regression check on all prior findings

v3 does **not** re-open the original #1–#9 or R2.1–R2.5 beyond the
locator/visibility/error-boundary issues called out below. If
`A-transcript` never lands, the core path still makes sense; only the
optional common-tailer extension is naturally blocked.

## New findings (R4.1 – R4.3)

### R4.1. `StatusSourceLocator` is worded as both provider-neutral and Codex-only

v3 gives every `AgentBindings` a `locator`, but also says **"only
`CompositeLocator` implements `StatusSourceLocator`"**. Taken literally,
**Claude cannot construct bindings** even though it still needs a fixed
`cwd/.vimeflow/sessions/<sid>/status.json` locator. Also pin Codex
**retry ownership**: retry currently wraps `CompositeLocator::resolve_rollout`
at [`codex/mod.rs:83`](./codex/mod.rs), while `CompositeLocator` itself
only owns fallback dispatch at
[`codex/locator.rs:603`](./codex/locator.rs).

> 💡 IDEA
>
> - **I — Intent:** keep Codex sub-locator policy private while making
>   location a uniform binding concern.
> - **D — Danger:** a literal implementation either fails Claude
>   bindings or moves Codex retry/fallback policy back into runtime
>   glue.
> - **E — Explain:** "Only `CompositeLocator`" should mean _only among
>   Codex internals_, not globally.
> - **A — Alternatives:** define a trivial
>   `ClaudeStatusFileLocator` / `FixedStatusFileLocator` for Claude;
>   move Codex retry **into** `CompositeLocator`'s `StatusSourceLocator`
>   impl (so retry + fallback policy stay in one place).

### R4.2. `#[doc(hidden)] pub` is over-applied to the 5-trait split

The visibility decision is unambiguous, but **too broad**. v3 marks all
five traits `#[doc(hidden)] pub`. Existing `#[doc(hidden)] pub` usage is
a narrow workaround for types that leak through public signatures,
documented at
[`base/transcript_state.rs:15`](./base/transcript_state.rs). If B''
narrows `TranscriptState::start_or_replace` to `pub(crate)` as planned,
`TranscriptStreamer` no longer needs to be public either — and the
other four traits never did.

> 💡 IDEA
>
> - **I — Intent:** satisfy Rust visibility rules without publishing an
>   adapter extension API.
> - **D — Danger:** doc-hidden public traits still become part of
>   `vimeflow_lib`'s public surface and make future signature changes
>   harder.
> - **E — Explain:** the broader backend pattern keeps runtime internals
>   behind `BackendState`; `#[doc(hidden)] pub` types are exceptions,
>   not the default.
> - **A — Alternatives:** make `AgentBindings` and all five traits
>   `pub(crate)` by default. Use `#[doc(hidden)] pub` only for a type
>   that _must_ remain in an unavoidable public signature.

### R4.3. `AgentBindings::for_attach -> Result<Self, String>` flattens too early

The renderer still needs a bare string, but `AgentBindings::for_attach`
is **below** the IPC boundary. v3 specifies `Result<Self, String>`,
while the Rust pattern is **domain error internally, string at
`BackendState` boundary** —
[`rules/rust/patterns.md:97`](../../../../../rules/rust/patterns.md).
Attach/location has meaningful variants: no detected agent, unsupported
adapter, Codex retry exhausted, ambiguous candidates, schema-drift
fallback failure, fatal DB/proc errors.

> 💡 IDEA
>
> - **I — Intent:** preserve the current IPC rejection contract.
> - **D — Danger:** flattening inside bindings repeats the old
>   stringly-coupling problem and makes tests/diagnostics less precise.
> - **E — Explain:** this code already introduced
>   `ValidateTranscriptError` to avoid classifying adapter failures by
>   message text.
> - **A — Alternatives:** use `AttachError` / `LocateError` internally
>   and map to `.to_string()` in `start_agent_watcher_inner` (or the
>   eventual `AgentWatcherService`).

## Revised plan v4

Three tactical amendments on top of v3:

1. **Claude gets a `ClaudeStatusFileLocator`** (trivial — builds the
   `cwd/.vimeflow/sessions/<sid>/status.json` path). Codex's
   `CompositeLocator` is the Codex-side `StatusSourceLocator` impl, with
   **retry + fallback dispatch both owned by `CompositeLocator`'s
   impl** (the retry loop in `CodexAdapter::status_source` moves
   into it).
2. **All 5 traits and `AgentBindings` are `pub(crate)`** by default;
   `TranscriptState::start_or_replace` narrows to `pub(crate)` as
   already planned. `#[doc(hidden)] pub` is reserved for genuinely
   unavoidable signature leaks (`TranscriptHandle` remains the only
   such case).
3. **`AgentBindings::for_attach -> Result<Self, AttachError>`** — a
   new domain enum carrying meaningful variants (NoAgentDetected,
   UnsupportedAgent, LocatorExhausted, LocatorFatal, LocatorAmbiguous,
   ValidatorRejected, …). String conversion happens at
   `start_agent_watcher_inner` (or the future
   `AgentWatcherService::start`) via `.to_string()`.

### v4 ordering (same as v3, with tactical adjustments)

| Step                      | Change vs. v3                                                                                                                                                                                                                                                                                                         | Addresses              |
| ------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------- |
| 0a / 0b                   | Unchanged.                                                                                                                                                                                                                                                                                                            | —                      |
| 0c                        | Unchanged (`LocatedStatusSource` + `StatusSnapshot` + `TranscriptPathSource` with `static_hint` / `dynamic_hint`).                                                                                                                                                                                                    | —                      |
| A-status                  | Unchanged.                                                                                                                                                                                                                                                                                                            | —                      |
| **B'**                    | **Tactical changes:** (a) all 5 traits + `AgentBindings` are `pub(crate)` (not `#[doc(hidden)] pub`); (b) Claude implements `ClaudeStatusFileLocator`; Codex implements `CompositeLocator` (which now owns retry + fallback); (c) `AgentBindings::for_attach -> Result<Self, AttachError>` with a proper domain enum. | R3.4, R4.1, R4.2, R4.3 |
| **B''**                   | Unchanged (`TranscriptState::start_or_replace` narrows to `pub(crate)`).                                                                                                                                                                                                                                              | R3.3                   |
| D'                        | Unchanged. Returned `AttachError` is mapped to `String` at this boundary.                                                                                                                                                                                                                                             | R4.3                   |
| A-transcript _(optional)_ | Unchanged.                                                                                                                                                                                                                                                                                                            | —                      |
| C _(optional)_            | Unchanged.                                                                                                                                                                                                                                                                                                            | —                      |

### Revised cost estimates (post-Round-4)

R4 amendments add ~0.5–1 day total — primarily for the `AttachError`
enum + per-site mapping at the IPC boundary, and Claude's trivial
locator impl + Codex retry relocation.

**Core path (0a → 0b → 0c → A-status → B' → B'' → D'): ~6.5–9 days**
(was 6–8 in v3).

- 0a: 0.5d, 0b: 0.25d, 0c: 1d, A-status: 1d
- B' (5-trait split + AgentBindings + Claude locator + Codex retry move + AttachError enum): **~2d** (was 1.5d)
- B'': 1.5d
- D': 0.5–1d (now includes AttachError → String mapping at boundary)

**Optional extensions** (unchanged):

- A-transcript: +1.5–2d (after D')
- C (common tailer): +1.5–2d (after A-transcript)

**Full delivery: ~9.5–13 days. Core-only: ~6.5–9 days.**

---

# Round 5 (2026-05-23) — Convergence ✅

> **Reviewer:** codex-cli 0.133.0
> **Subject:** the v4 plan from Round 4
> **Verdict:** **"No new findings. The revised plan addresses all previously-raised issues."**

The iterative review loop has converged after 5 rounds.

## Closure summary

| Round 4 finding                              | Status                                                                                                                                                                                                                                                                                                                                               |
| -------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R4.1 locator wording + Codex retry ownership | **Closed** — with the constraint that retry wraps `CompositeLocator::resolve_rollout` at the composite level (not around SQLite/FS strategies individually). Retry policy stays private; a test-only override is fine but not part of the trait.                                                                                                     |
| R4.2 `pub(crate)` for the 5-trait split      | **Closed** — coherent if the boundary lands at `BackendState` / IPC / generated event types. Direct adapter tests stay co-located module tests; no `#[cfg(test)] pub` aliases needed because external integration tests exercise the binary IPC, not adapter internals.                                                                              |
| R4.3 `AttachError` domain enum               | **Closed** — with one constraint: keep subdomain errors internally where they carry useful classification. `AttachError` is attach/service-level; `LocatorError` stays provider-local underneath it; **`ValidateTranscriptError` stays separate** (it feeds `TxOutcome` diagnostics, not attach failures, and must not collapse into `AttachError`). |

## Regression check (final)

v4 does **not** reopen #1–#9, R2.1–R2.5, or R3.1–R3.5.

## Cost honesty

The **6.5–9 day core estimate is believable if scope stays core-only**:
seam prep (0a/0b/0c) + status DTOs (A-status) + 5-trait split (B') +
TranscriptState reshape (B'') + service facade (D').

⚠️ **Inflation risk:** the estimate will inflate again if `A-transcript`
or `C` sneak back into the core path. Treat both as strictly optional
extensions, gated by a separate decision after D' lands.

## Iteration shape

| Round | Findings | Nature                                                                         |
| ----- | -------- | ------------------------------------------------------------------------------ |
| 1     | 9        | Architectural — entire concepts missing (`AttachContext`, path-source duality) |
| 2     | 5        | Specification gaps — types named but not defined                               |
| 3     | 5        | Contract ambiguity + scope/visibility decisions                                |
| 4     | 3        | Tactical — locator wording, visibility scope, error type                       |
| 5     | **0**    | **Convergence** — v4 holds under regression + closure checks                   |

The decreasing finding count (9 → 5 → 5 → 3 → 0) reflects genuine
convergence rather than codex losing interest: rounds 4 and 5 both
included specific regression-check questions, and round 5 verified the
v4 amendments concretely (Claude locator shape, retry containment,
pub(crate) boundary location, error-enum sub-domains).

---

# Final plan (v4-frozen)

This is the plan ready for implementation. Each step is its own commit
on `refactor/agent-adapter`; the branch merges to `main` only when all
core-path steps land.

## Sequence

| Step                          | What                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | Estimate    |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------- |
| **0a**                        | Define `AttachContext` in new `crates/backend/src/agent/adapter/attach.rs`. Fields: `session_id`, `initial_cwd`, `shell_pid`, `agent_pid`, `pty_start`, `agent_type`, `provider_home: Option<PathBuf>`, `proc_root: Option<PathBuf>`. **Provider-neutral** — no `codex_*` / `claude_*` field names. All per-agent metadata lives in a new `crates/backend/src/agent/config.rs` registry (`AGENT_SPECS` const slice, `spec_for(agent_type)`, `agent_type_for_binary(name)`). `detector.rs` migrates to the registry too. Amended 2026-05-23 per PR #247 review (the original `codex_home` + `claude_home_or_resolver` two-field shape would have forced struct edits on every new agent). | ~1d         |
| **0b**                        | Define `SessionRuntimeContext` (live `PtyState` handle for cwd lookups; distinct from `AttachContext`).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  | ~0.25d      |
| **0c**                        | Introduce `LocatedStatusSource { status_path, trust_root, static_transcript_hint }` + `StatusSnapshot` (decoder output, status-only) + `TranscriptPathSource` trait with `static_hint(&LocatedStatusSource)` (Codex) and `dynamic_hint(raw)` (Claude) methods. Remove `ParsedStatus.transcript_path` side channel.                                                                                                                                                                                                                                                                                                                                                                       | ~1d         |
| **A-status**                  | Loose DTOs for STATUS only (`claude_code/statusline.rs` + `codex/parser.rs` state-fold). Per-field `Option<T> + #[serde(default)]`; Some-only fold for Codex token state; `deserialize_with` for wrong-type tolerance. **Defer transcript DTOs.**                                                                                                                                                                                                                                                                                                                                                                                                                                        | ~1d         |
| **B'**                        | 5-trait split + `ClaudeStatusFileLocator` (trivial, stateless: returns `cwd/.vimeflow/sessions/<sid>/status.json`, `trust_root = cwd`, no static hint) + Codex retry folded into `CompositeLocator`'s `StatusSourceLocator` impl + `AgentBindings { agent_type, locator, decoder, transcript_paths, validator, streamer }` in `adapter/bindings.rs` + `AgentBindings::for_attach(&AttachContext) -> Result<Self, AttachError>`. All 5 traits and `AgentBindings` are **`pub(crate)`**.                                                                                                                                                                                                   | ~2d         |
| **B''**                       | `TranscriptState` re-shape to `Arc<dyn TranscriptStreamer>`. Narrow `start_or_replace` to `pub(crate)`. Preserve concurrency invariants (per-session `start_gate` Mutex, old-before-new stop order, regression test at line 428).                                                                                                                                                                                                                                                                                                                                                                                                                                                        | ~1.5d       |
| **D'**                        | `AgentWatcherService` registry facade. Owns/clones `PtyState`, `AgentWatcherState`, `TranscriptState`, `EventSink`. `start(session_id)` builds `AttachContext` from live `PtyState`; `stop(session_id)` removes the watcher. **`AttachError` → `String` mapping happens at this boundary** (and at `start_agent_watcher_inner`).                                                                                                                                                                                                                                                                                                                                                         | ~0.5–1d     |
| **Core total**                |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          | **~6.5–9d** |
| **A-transcript** _(optional)_ | Loose DTOs for TRANSCRIPT JSONL (`claude_code/transcript.rs` + `codex/transcript.rs`). Must preserve Codex `CompletionMode` + partial-line buffering. Lands AFTER D' — decide separately.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                | +1.5–2d     |
| **C** _(optional)_            | Common tailer engine. Lands after A-transcript. Engine must absorb `CompletionMode` + partial-line buffering.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            | +1.5–2d     |

## Frozen implementation constraints

These came out of the 5-round loop and must not be relitigated during
implementation:

1. **`ClaudeStatusFileLocator` is stateless and trivial** — no
   `dirs::home_dir()` lookups inside it; its `static_transcript_hint`
   is always `None` (Claude path is dynamic via `dynamic_hint`).
2. **Codex retry lives inside `CompositeLocator::resolve_rollout`'s
   `StatusSourceLocator` impl**, NOT around individual SQLite/FS
   strategies. Retry policy stays private to the impl. Test-only
   overrides are fine; they must not appear in the trait.
3. **`pub(crate)` is the default** for all 5 traits + `AgentBindings`
   - `TranscriptStreamer`. The only `#[doc(hidden)] pub` exception
     remains `TranscriptHandle` (existing, documented at
     [`base/transcript_state.rs:15`](./base/transcript_state.rs)).
4. **Three error enums, three layers:**
   - `LocatorError` — provider-local (Codex), inside the locator impl.
   - `AttachError` — attach/service-level, wraps locator failures.
   - `ValidateTranscriptError` — **stays separate** because it feeds
     `TxOutcome` diagnostics in the runtime, not attach failures.
5. **CWD is a live runtime input** — `AttachContext.initial_cwd` is
   the attach-time snapshot; runtime callers (transcript replace,
   test-runner cwd resolution) **must** read live cwd from
   `PtyState` via `SessionRuntimeContext`, not from `AttachContext`.
6. **A-transcript and C are gated by a separate decision after D'
   lands.** Do not bundle them into the core PR scope.

## Pre-flight references (per-step targets)

| Step     | Files to touch                                                                                                                                                                                                                                       |
| -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0a/0b    | new: `adapter/attach.rs` + `adapter/session_runtime.rs` (or co-located). edit: `adapter/mod.rs::resolve_bind_inputs` to populate AttachContext.                                                                                                      |
| 0c       | new: `adapter/source.rs` (LocatedStatusSource + TranscriptPathSource). edit: `adapter/types.rs::ParsedStatus` (remove transcript_path). edit: `base/watcher_runtime.rs` (parse path now goes through TranscriptPathSource).                          |
| A-status | edit: `claude_code/statusline.rs`, `codex/parser.rs`. add: status DTOs (Claude + Codex).                                                                                                                                                             |
| B'       | new: `adapter/bindings.rs` (AgentBindings). new: 5 trait modules. new: `adapter/error.rs` (AttachError). edit: `claude_code/mod.rs`, `codex/mod.rs` (impls). edit: `codex/locator.rs` (retry move into CompositeLocator's StatusSourceLocator impl). |
| B''      | edit: `base/transcript_state.rs` (Arc<dyn TranscriptStreamer> instead of Arc<dyn AgentAdapter>). narrow `start_or_replace` to pub(crate).                                                                                                            |
| D'       | new: `adapter/service.rs` (AgentWatcherService). edit: `adapter/mod.rs::start_agent_watcher_inner` to delegate to the service + map AttachError.                                                                                                     |

---

## Prompts used

### Round 1 prompt

```
Read crates/backend/src/agent/adapter/README.md — a pre-refactor design
analysis I just wrote. It proposes refactoring the AgentAdapter trait
through 4 directions (A: schema-first serde, B: split trait by concern
into Locator/StateDecoder/TranscriptStreamer, C: common tailer engine,
D: AgentSession facade) in order A->B->D, with C optional later.

Then read the actual code:
- crates/backend/src/agent/adapter/mod.rs, types.rs (trait surface)
- crates/backend/src/agent/adapter/base/ (runtime: watcher_runtime,
  transcript_state, diagnostics, path_security)
- crates/backend/src/agent/adapter/claude_code/ (statusline, transcript,
  test_runners)
- crates/backend/src/agent/adapter/codex/ (locator, parser, transcript,
  types)
- crates/backend/src/runtime/state.rs and src/agent/adapter/mod.rs::
  start_agent_watcher_inner (the IPC seam)

Your job: critique the refactor proposal. Identify dangers and blockers
I underestimated or missed. Be specific — name files, line numbers,
functions.

[focus list — A->B->D blockers, hidden coupling, cost realism, A/B/D
specifics]

Output: numbered findings (max 10), each with an IDEA block per
rules/common/idea-framework.md.
```

### Round 2 prompt

```
Round 2 of refactor roadmap review.

Read:
1. crates/backend/src/agent/adapter/README.md — original A->B->D proposal
2. crates/backend/src/agent/adapter/REVIEW.md — your previous 9 findings +
   REVISED sequence (Step 0 -> A' -> B' -> B'' -> D')

Then re-read the actual code [list of files].

Your job: critique the REVISED plan in REVIEW.md.

PART 1 — VALIDATE previous 9 findings closure.
PART 2 — NEW issues introduced by the revision (Step 0, A', B', B'', D', C).
PART 3 — STOP CRITERION: if no new issues AND v1 closes all 9, say
EXPLICITLY: "No new findings. The revised plan addresses all
previously-raised issues."

Otherwise: numbered findings (max 10), each with IDEA block.
```

### Round 5 prompt

```
Round 5 of refactor roadmap review.

Trajectory: 9 -> 5 -> 5 -> 3 findings. v4 should be very close to stable.

CLOSURE CHECK on R4.1-R4.3 (locator wording, visibility scope, error type).
REGRESSION CHECK on all prior findings.
NEW ISSUES:
- retry_locator fold into StatusSourceLocator
- AttachError Send+Sync+'static across spawn_blocking
- pub(crate) tractability vs crates/backend/tests
- cost estimate stability

STOP CRITERION: explicit "No new findings." if v4 holds.
```

### Round 4 prompt

```
Round 4 of refactor roadmap review.

Validate v3 closes R3.1-R3.5 and doesn't re-open earlier findings. Focus
new issues on:
- Locator scope (only CompositeLocator? what about Claude?)
- Visibility decision (#[doc(hidden)] pub for all 5 traits — too broad?)
- AgentBindings::for_attach error type
- 5-trait split shared state, locator uniformity, A-transcript optionality
- Cost estimate honesty

STOP CRITERION: explicit "No new findings." if v3 holds.
Otherwise prefix findings R4.* (max 10) with IDEA blocks.
```

### Round 3 prompt

```
Round 3 of refactor roadmap review.

[Reads README.md + REVIEW.md (now containing Round 1 + Round 2 + v2 plan)
+ the actual code.]

PART 1 — CLOSURE on R2.1-R2.5.
PART 2 — REGRESSION check on the original 9.
PART 3 — NEW issues introduced by v2:
  - Step 0a/0b/0c boundaries
  - 7-9d estimate realism
  - TranscriptState re-shape (B'') visibility constraints
  - AgentBindings concrete spec
  - Locator subtrait split (CompositeLocator only?)
PART 4 — STOP CRITERION: if no new issues, say EXPLICITLY:
  "No new findings. The revised plan addresses all previously-raised issues."

Otherwise: numbered findings prefixed R3.* (max 10), each with IDEA block.
```

To re-run any round:

```bash
codex exec "<the prompt above>" < /dev/null
```
