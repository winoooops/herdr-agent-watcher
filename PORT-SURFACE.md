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
