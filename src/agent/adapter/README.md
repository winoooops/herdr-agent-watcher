# Agent Adapter

> **Status:** pre-refactor design analysis. Captures the current shape of the
> `agent::adapter` module, the JSON/JSONL schemas each adapter parses, and
> four refactor directions under consideration. Generated 2026-05-22 before
> the planned interface cleanup.

## What this module is for

From the frontend's perspective the user picks an agent (Claude Code, Codex)
and the backend pushes a uniform stream of events:

- `agent-status` — model, token usage, cost, rate limits
- `agent-tool-call` — tool calls start / done / failed (with duration)
- `agent-turn` — counted on every real user prompt
- `agent-cwd` — workspace cwd transitions reported by the agent
- `test-run` — structured test-runner snapshots (cargo, vitest, …)

The frontend never needs to know that Claude Code writes JSON to a
Vimeflow-owned app-data bridge status file while Codex writes JSONL to
`~/.codex/sessions/<yyyy>/<mm>/<dd>/rollout-*.jsonl` and indexes its sessions
through `~/.codex/state.sqlite` + `~/.codex/logs.sqlite`.

**The adapter is the translation layer between vendor-specific agent
telemetry and Vimeflow's unified event model.** It encapsulates three
provider-specific facts:

| Concern                           | Claude Code                                | Codex                                                                                                  |
| --------------------------------- | ------------------------------------------ | ------------------------------------------------------------------------------------------------------ |
| **Where** the telemetry lives     | One JSON file at a fixed path              | 2 SQLite DBs + 1 JSONL, 4 lookup strategies                                                            |
| **State JSON shape** (statusline) | One file says everything                   | Must fold across many JSONL lines                                                                      |
| **Activity JSONL shape**          | `assistant` / `user` / `tool_result` lines | `session_meta` / `turn_context` / `event_msg` / `response_item` lines, with split-completion semantics |

Data flow: `agent_type` enters at `start_agent_watcher_inner`, gets mapped
to an adapter by `<dyn AgentAdapter>::for_attach`, and from that point on
the rest of the backend speaks only in unified events.

## The four concerns mixed into one trait today

The current `AgentAdapter` trait carries five methods, but those methods
represent four logically independent concerns:

1. **Discovery** — `status_source(cwd, sid) -> StatusSource` — _"where's
   the file?"_
2. **State decoding** — `parse_status(sid, raw) -> ParsedStatus` — _"what's
   the agent doing right now?"_
3. **Path validation** — `validate_transcript(raw) -> PathBuf` — _"is this
   path safe to tail?"_
4. **Stream decoding + lifecycle** — `tail_transcript(events, sid, cwd,
path) -> TranscriptHandle` — _"spawn me a thread that emits events from
   this JSONL"_

The "feels bloated" sensation when reading this module is rooted here:
every adapter has to think about all four at once.

## Architecture diagrams

The PlantUML sources and rendered PNGs live in
[`docs/diagrams/agent-adapter/`](../../../../../docs/diagrams/agent-adapter/);
see that folder's [`README.md`](../../../../../docs/diagrams/agent-adapter/README.md)
for re-render instructions and an annotated index.

| #   | Diagram                                                                                             | What it shows                                                                                   |
| --- | --------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| 1   | [`01-trait-architecture.png`](../../../../../docs/diagrams/agent-adapter/01-trait-architecture.png) | `AgentAdapter` trait + 3 implementations + return types                                         |
| 2   | [`02-base-runtime.png`](../../../../../docs/diagrams/agent-adapter/02-base-runtime.png)             | `base/` runtime: watcher + transcript registry + diagnostics + path security                    |
| 3   | [`03-claude-code.png`](../../../../../docs/diagrams/agent-adapter/03-claude-code.png)               | Claude Code adapter internals: `statusline` + `transcript` + shared `test_runners`              |
| 4   | [`04-codex.png`](../../../../../docs/diagrams/agent-adapter/04-codex.png)                           | Codex adapter internals: `locator` (Composite / SqliteFirst / FsScan) + `parser` + `transcript` |
| 5   | [`05-lifecycle-sequence.png`](../../../../../docs/diagrams/agent-adapter/05-lifecycle-sequence.png) | Attach → status watch → tail thread sequence                                                    |

## JSON schemas

The adapters all parse JSON / JSONL today via `serde_json::Value` pull-style
helpers. The following types document the actual shape each adapter
recognizes.

### Claude Code — statusline (`status.json`)

Polled + watched once per session. Source:
[`claude_code/statusline.rs`](./claude_code/statusline.rs).

```typescript
type ClaudeStatusJson = {
  session_id?: string // agent-side session UUID
  version?: string // CLI version
  model?: {
    id?: string // e.g. "claude-sonnet-4-20250514"
    display_name?: string // falls back to model.id
  }
  context_window?: {
    used_percentage: number | null // null during loading phase
    remaining_percentage?: number // computed from used if missing
    context_window_size?: number
    total_input_tokens?: number
    total_output_tokens?: number
    current_usage?: {
      input_tokens: number
      output_tokens: number
      cache_creation_input_tokens: number
      cache_read_input_tokens: number
    }
  }
  cost?: {
    // entire object optional
    total_cost_usd: number
    total_duration_ms: number
    total_api_duration_ms: number
    total_lines_added: number
    total_lines_removed: number
  }
  rate_limits?: {
    five_hour: { used_percentage: number; resets_at: number }
    seven_day: { used_percentage: number; resets_at: number } | null
  }
  transcript_path?: string // pointer to the JSONL file below
}
```

### Claude Code — transcript JSONL

One line per event. Tailed by a background thread. Source:
[`claude_code/transcript.rs`](./claude_code/transcript.rs).

```typescript
type ClaudeLine =
  | {
      type: 'assistant'
      timestamp?: string
      cwd?: string
      message: { content: ClaudeBlock[] }
    }
  | {
      type: 'user'
      timestamp?: string
      cwd?: string
      message: { role: 'user'; content: string | ClaudeBlock[] }
    }
  | {
      type: 'tool_result'
      tool_use_id: string
      content: string | ClaudeBlock[]
      is_error?: boolean
    }

type ClaudeBlock =
  | { type: 'tool_use'; id: string; name: string; input: object }
  | {
      type: 'tool_result'
      tool_use_id: string
      content: string | ClaudeBlock[]
      is_error?: boolean
    }
  | { type: 'text'; text: string }
  | { type: 'image' | 'document' | string /* other shapes pass through */ }
```

### Codex — rollout JSONL

Mixes state (`token_count`, `task_complete`) and activity (`function_call`,
`exec_command_end`) into one JSONL stream. Source:
[`codex/parser.rs`](./codex/parser.rs) (state fold) +
[`codex/transcript.rs`](./codex/transcript.rs) (activity tailer).

```typescript
type CodexLine = { timestamp: string } & (
  | {
      type: 'session_meta'
      payload: { id: string; cli_version: string; cwd?: string }
    }
  | { type: 'turn_context'; payload: { model: string } }
  | { type: 'event_msg'; payload: CodexEventMsg }
  | { type: 'response_item'; payload: CodexResponseItem }
)

type CodexEventMsg =
  | { type: 'task_started'; model_context_window: number }
  | { type: 'task_complete'; duration_ms: number }
  | {
      type: 'token_count'
      info?: {
        last_token_usage?: {
          input_tokens
          output_tokens
          cached_input_tokens
          total_tokens
        }
        model_context_window: number
      }
      rate_limits?: {
        primary?: { used_percent; resets_at }
        secondary?: { used_percent; resets_at }
      }
    }
  | { type: 'user_message'; message: string }
  | {
      type: 'exec_command_end'
      call_id: string
      aggregated_output: string
      exit_code: number
      duration?: { secs: number; nanos: number }
    }
  | { type: 'patch_apply_end'; call_id: string; success: boolean }

type CodexResponseItem =
  | {
      type: 'function_call'
      call_id: string
      name: string
      arguments: string /* JSON-encoded */
    }
  | {
      type: 'custom_tool_call'
      call_id: string
      name: string
      input: string /* patch text */
    }
  | { type: 'function_call_output'; call_id: string; output: string }
  | {
      type: 'custom_tool_call_output'
      call_id: string
      output: string /* JSON-encoded {output, metadata: {exit_code}} */
    }
```

**Critical Codex quirk** — a `function_call`'s completion can arrive as
`function_call_output`, `exec_command_end`, OR `patch_apply_end`. That's why
`codex/transcript.rs` has a `CompletionMode { Output, ExecCommandEnd,
PatchApplyEnd }` enum on every in-flight call, and Claude Code does not.

## Refactor directions under consideration

Following [`rules/common/idea-framework.md`](../../../../../../rules/common/idea-framework.md)
for each option. Recommendation at the end.

### A — Schema-first with serde

Replace `serde_json::Value` pull-parsing with strongly-typed serde structs
(the schemas above become Rust types). `parse_status` becomes
`serde_json::from_str::<Schema>(raw).map(into_event)`.

> 💡 IDEA
>
> - **I — Intent:** make the schemas first-class Rust types so contributors
>   learn the shape from the type definition, not by chasing
>   `.get("x").and_then(...)` chains.
> - **D — Danger:** strict serde rejects unknown fields by default — schema
>   drift from upstream becomes a hard error unless every enum opts into
>   `#[serde(other)]`. Needs fixture tests per provider so drift is caught
>   in CI.
> - **E — Explain:** aligns with the "make invalid states unrepresentable"
>   principle in
>   [`rules/common/design-philosophy.md`](../../../../../../rules/common/design-philosophy.md).
>   Today's parsers are tolerant via partial decoding; that tolerance is
>   the source of most "I had to grep 5 helpers to learn this field exists"
>   pain. Typed schemas with `#[serde(default)]` are equally tolerant but
>   discoverable.
> - **A — Alternatives:** keep Value pull-style and inline the schemas in a
>   doc — cheaper, but the doc drifts from reality the moment the parser
>   changes.

### B — Split the trait by concern

Replace the 5-method `AgentAdapter` trait with three small composable
traits + one composition struct:

```rust
trait StatusSourceLocator { fn locate(&self, cwd, sid) -> Result<StatusSource>; }
trait StateDecoder        { fn decode(&self, raw) -> Result<ParsedStatus>; }
trait TranscriptStreamer  { fn validate(&self, raw) -> Result<PathBuf>;
                            fn tail(&self, events, sid, cwd, path) -> Result<Handle>; }

struct Agent {
  agent_type: AgentType,
  locator:  Box<dyn StatusSourceLocator>,
  decoder:  Box<dyn StateDecoder>,
  streamer: Box<dyn TranscriptStreamer>,
}
```

> 💡 IDEA
>
> - **I — Intent:** each concern stands alone, gets its own tests, and can
>   be swapped without touching the others. Codex's stateful locator stops
>   infecting the parser/streamer.
> - **D — Danger:** more type names. If every caller uses the composite
>   `Agent`, the split adds ceremony without reducing what callers must
>   know. The split only pays off if at least one consumer (tests, future
>   agent types) uses pieces independently.
> - **E — Explain:** directly addresses the "too many interfaces" feeling —
>   the answer is _more_ interfaces, each single-purpose. Pairs naturally
>   with Direction A: a `StateDecoder` impl is literally
>   `serde_json::from_str::<Schema>(raw).map(into_event)`.
> - **A — Alternatives:** keep the single trait but ban cross-concern field
>   access (codex's `resolved_rollout_path` Mutex would need to live on a
>   sub-struct). Lighter-weight but doesn't actually shrink the trait
>   surface.

### C — Common tailer engine + per-adapter schemas

`claude_code/transcript.rs` and `codex/transcript.rs` duplicate ~70% of
their structure (open file → spawn thread → tail loop → in-flight map →
emit). Extract the engine:

```rust
trait TranscriptSchema {
    type Line: serde::de::DeserializeOwned;
    fn handle_line(&mut self, line: Self::Line) -> Vec<AgentEvent>;
}
struct Tailer<S: TranscriptSchema> { /* owns file, thread, stop flag, replay buffering */ }
```

> 💡 IDEA
>
> - **I — Intent:** adding a 3rd agent (Aider, Cursor, Roo) becomes "write
>   a ~100-line schema + handler" instead of "copy-paste a 700-line
>   transcript.rs and tweak it". Today's duplicated `InFlightToolCall`
>   struct is the canary.
> - **D — Danger:** the engine must support Codex's `CompletionMode`
>   (split-completion across line types) as a first-class concept. If you
>   bake "completion always co-located with start" into the engine, Codex
>   doesn't fit and you regress.
> - **E — Explain:** the duplication is real (~700 lines × 2) and the
>   divergence is a single explicit enum. Cheap to factor once you accept
>   that the engine must understand async/split completion.
> - **A — Alternatives:** leave the duplication; copy-paste-and-tweak when
>   agent #3 lands. Cheaper today, gets exponentially worse with each
>   new agent.

### D — Deep-module facade (`AgentSession`)

Hide the trait, the four-concern split, and the watcher orchestration
behind one stable type:

```rust
pub struct AgentSession { /* private */ }
impl AgentSession {
    pub fn attach(events, pty, ts, sid, cwd) -> Result<Self>;
    pub fn detach(self);
}
```

The IPC layer (`start_agent_watcher_inner`) never touches `AgentAdapter`
again — it just constructs an `AgentSession`. The trait + base +
adapters all become implementation details.

> 💡 IDEA
>
> - **I — Intent:** matches the "deep modules" principle in
>   [`rules/common/design-philosophy.md`](../../../../../../rules/common/design-philosophy.md):
>   one narrow public surface, substantial behavior hidden behind it.
>   Today's `<dyn AgentAdapter>::for_attach` + `start` + `stop` extension
>   methods are a half-finished version of this.
> - **D — Danger:** this hides internal complexity but doesn't _reduce_ it.
>   If you do D without A/B/C first, you're papering over the mess. Best
>   done last, as the final "lock in the new public surface" step.
> - **E — Explain:** the IPC seam is the right place to draw the line —
>   anything below it can refactor freely without breaking the renderer
>   contract. The `_inner` helpers in
>   [`rules/rust/patterns.md`](../../../../../../rules/rust/patterns.md)
>   already gesture at this pattern.
> - **A — Alternatives:** skip the facade and just ban direct trait use
>   from the IPC layer via module visibility. Same effect, fewer new
>   types, but the boundary becomes implicit.

## Recommendation

**A → B → D, with C optional later.** Sequencing matters:

1. **A first** — typed schemas are pure documentation gain with no
   architectural risk. They give you the JSON schema reference you want
   _and_ make B's `StateDecoder` trait trivial to fill in. **~1 day.**
2. **B second** — splitting the trait once schemas are typed lets each
   split absorb the right type. Codex's stateful adapter naturally
   decomposes (`SqliteLocator` keeps its cache; `CodexStateDecoder` is
   stateless; `CodexTranscriptStreamer` only holds what it actually
   needs). **~1-2 days.**
3. **D third** — once A + B are done, the facade closes the seam and the
   IPC layer simplifies. **~half a day.**
4. **C later** — only worth it when agent #3 is on the horizon, because
   the engine design must absorb Codex's split-completion quirk to be
   useful.

## Codex critique → converged v4-frozen plan (2026-05-22 → 2026-05-23)

> **Tracking issue:** [#246](https://github.com/winoooops/vimeflow/issues/246) — the umbrella issue for all implementation PRs landing this refactor. Reference it in every PR (`Part of #246`).

This proposal was iterated against codex across **5 rounds**; the loop
converged on Round 5 with the explicit signal **"No new findings."**

Finding count per round: **9 → 5 → 5 → 3 → 0**. The plan version
landed at **v4-frozen**.

The original A → B → D ordering proposed in this file is **superseded**.
Read [`REVIEW.md`](./REVIEW.md) for the full critique log + the
[**Final plan (v4-frozen)**](./REVIEW.md#final-plan-v4-frozen),
which:

- Adds a mandatory **Step 0** (`0a` AttachContext, `0b` SessionRuntimeContext, `0c` LocatedStatusSource + StatusSnapshot + TranscriptPathSource).
- Splits A' into **A-status** (mandatory, before B') and **A-transcript** (optional, after D').
- Promotes B to a **5-trait split** with `AgentBindings` composition + `AttachError` domain enum.
- Treats `AgentWatcherService` (D') as a registry facade, not an RAII session value.

The refactor branch is `refactor/agent-adapter`; each round + each
implementation step lands as its own commit, with a single squash to
`main` at the end.

**Revised core-path estimate:** ~6.5–9 days (was ~2.5–3.5d in this
file). Full delivery (including the optional A-transcript and C
extensions): ~9.5–13 days.

## Cross-cutting smells worth fixing during the refactor

- **`test_runners/` lives under `claude_code/`** but is imported by
  `codex/transcript.rs` (six `use crate::agent::adapter::claude_code::
test_runners::…` lines). Hoist to `adapter::test_runners` or under
  `base/`.
- **`InFlightToolCall` is defined twice** with a near-identical shape
  (Codex's adds `completion_mode`). Unify after C, or share via a
  generic.
- **`CodexAdapter` is stateful** (locator cache + resolved rollout path
  Mutex) only because `parse_status` needs to surface the rollout path
  back as `ParsedStatus.transcript_path`. Consider plumbing the
  transcript path through the locator result instead so the adapter can
  stay stateless.
- **`base::start_for` + `<<dyn AgentAdapter>>::start/stop`** are a
  workaround for not having an `AgentSession` type — they go away with
  D.
