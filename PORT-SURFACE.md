# Port surface

Generated from the copied sidecar `src/agent` tree on 2026-08-10.

The imported `src/agent/**` tree retains its legacy Vimeflow protocol identifiers for
mechanical source compatibility. Public package, plugin, storage, environment, and metadata
names use the product-neutral Agent Watcher namespace.

## Imports

```rust
    use crate::runtime::EventSink;
    use crate::runtime::FakeEventSink;
    use crate::runtime::event_sink::FakeEventSink;
    use crate::terminal::PtyState;
use crate::aliases::{build_alias_lines, AgentAlias};
use crate::runtime::EventSink;
use crate::runtime::FakeEventSink;
use crate::runtime::{serialize_event, EventSink};
use crate::terminal::PtyState;
use crate::terminal::state::PtyState;
use crate::terminal::types::SessionId;
```

## Fully qualified references

```text
crate::debug::debug_log
crate::filesystem::scope::open_nofollow
crate::runtime::EventSink
crate::runtime::FakeEventSink
crate::runtime::FakeEventSink::new
crate::runtime::event_sink::FakeEventSink
crate::terminal::PtyState
crate::terminal::state::ManagedSession
crate::terminal::state::PtyState
crate::terminal::state::RingBuffer::new
crate::terminal::types::SessionId
```

`src/agent/commands.rs` is intentionally excluded because it is the Electron-sidecar IPC
entrypoint. The remaining tree is kept mechanically identical to the source module except
for its module-boundary declaration.

## Agent Watcher bootstrap seeds

- Claude: reconstructs the status pointer from Herdr's declared session when needed.
- OpenCode: when the declared bridge transcript already exists, appends a fresh
  PID/cwd/session index row so an already-running session survives an Agent Watcher
  daemon restart. This compensates for the daemon-local first-seen timestamp without
  changing the frozen locator.

## Sanctioned modifications (Apache §4(b)-style registry)

### M0a-4 — `serde_helpers.rs` test data

`assert_eq!(p.ratio, Some(3.14))` → `1.5` in
`lenient_f64_accepts_numbers_rejects_others`. `clippy::approx_constant` is deny-by-default
and read 3.14 as π, which made `cargo clippy` unusable repo-wide. Test data only; no
production behaviour differs from the vimeflow original. (Commit `e249327`.)

### M0a-6 — `src/agents/claude_bridge.rs` forks bridge generation

`src/agent/adapter/claude_code/bridge.rs` is frozen, so Agent Watcher generates its own
overlay. Three deliberate differences from the original:

1. **Destinations are baked in, not read from `$VIMEFLOW_STATUS_FILE` /
   `$VIMEFLOW_ATTENTION_FILE`.** The plugin never spawns the PTY, so it cannot set env.
2. **`attention.jsonl` is created only when absent.** The original truncates on every
   generation, which was safe when generation ran once per spawned session; here it runs
   per launch, and the watcher holds a cursor.
3. **`UserPromptSubmit` synthesises a name-only record.** The original does too, via an
   inline command; routing it through the shared append path would have written the user's
   prompt text to disk.

`shell_quote_path` and `write_executable_script` are private in the frozen tree and are
reimplemented rather than widened.

The OpenCode bridge is a PUBLIC artifact installed into the user's OpenCode config, so it
follows the product-neutral naming policy rather than the frozen-tree rule (commit
`c0f374a`, intentional). Files diverging from the sidecar source as a result — future
sidecar merges must reconcile the opencode subtree by hand:

- `src/agent/adapter/bindings.rs`
- `src/agent/adapter/opencode/install.rs`
- `src/agent/adapter/opencode/mod.rs`
- `src/agent/adapter/opencode/model_catalog.rs`
- `src/agent/adapter/opencode/parser.rs`
- `src/agent/adapter/opencode/plugin/agent-watcher-opencode-bridge.ts` (renamed from `vimeflow-opencode-bridge.ts`)
- `src/agent/adapter/opencode/plugin/agent-watcher-opencode-bridge.test.ts`
- `src/agent/adapter/opencode/transcript.rs`
- `src/agent/adapter/opencode/types.rs`

### Kimi fallback resume parity — `src/agent/adapter/kimi/locator.rs`

The exact-bucket fallback now accepts `session resume` startup evidence through the same
cached `session_resume_at` path as index resolution. Previously the index path understood a
resumed session but the fallback only accepted sessions created by the current process; the
affected live session was absent from the index, so only the disagreeing fallback was
reachable. This makes the two resolution paths agree rather than adding new resume
behaviour. Future sidecar merges must reconcile this locator change by hand.
