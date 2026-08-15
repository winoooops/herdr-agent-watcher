# Agent Watcher for Herdr

> Renamed from `vimeflow-agents` on 2026-08-10. Agent Watcher is a standalone,
> product-neutral Herdr plugin; Vimeflow may consume it later but does not own its scope.

Coding-agent observability for Herdr. The plugin watches local agent state and
transcripts, reports pane metadata, and sends lifecycle notifications. Local
observation is the default; Kimi plan-usage reporting is an explicit opt-in.

## Install for local development

Requires Herdr 0.8.0+ and Rust 1.88+.

```sh
cargo build --release
herdr plugin link "$PWD"
herdr plugin action invoke restart-daemon --plugin agent-watcher
```

The stock Herdr UI receives notifications and pane metadata tokens including
`agent_watcher_state`, `agent_watcher_phase`, `agent_watcher_model`,
`agent_watcher_context_pct`, `agent_watcher_attention`, and
`agent_watcher_title`.

## Sidebar

Open the live Agent Watcher sidebar in a right-hand split:

```sh
herdr plugin action invoke open-sidebar --plugin agent-watcher
```

Each invocation intentionally opens another split. Cards show agent state,
agent/model, title, context use, cache hit rate, cost, tool count, and the three
newest tool traces. Use Up/Down or PageUp/PageDown to scroll and `q`, Escape, or
Ctrl-C to close.

If the daemon is unavailable or disconnects, the pane reports that state and
waits for a key before closing. The sidebar's state socket is plugin-internal:
`$HERDR_PLUGIN_STATE_DIR/agent-watcher-state.sock`. Its newline-delimited JSON
protocol is currently `WIRE_VERSION = 1` and is not a public integration API.

Stop the daemon with:

```sh
herdr plugin action invoke stop-daemon --plugin agent-watcher
```

## Supported agents

- Claude Code (`claude`, `claude-code`) — live verified
- Codex CLI (`codex`) — live verified
- Kimi Code (`kimi`) — live verified
- OpenCode (`opencode`) — live verified

To add an agent, implement `AgentAdapter` under `src/agents/` and register it
with `AgentRegistry` in `src/daemon/run.rs`. Agent-specific parsing belongs in
the adapter; Herdr socket details stay behind `HerdrPort`.

### Kimi usage consent

Kimi plan-usage lookup sends the configured API key to its `/usages` endpoint,
so it stays disabled until explicitly enabled. Use the Herdr plugin actions
`kimi-consent-on`, `kimi-consent-off`, and `kimi-consent-status`; revocation is
picked up by the running daemon without a restart.

### OpenCode bridge

The first OpenCode bind installs or updates the bundled bridge in OpenCode's
plugin directory. `AGENT_WATCHER_OPENCODE_PLUGINS_DIR` and
`AGENT_WATCHER_OPENCODE_BRIDGE_DIR` override the install and event directories.

## Configuration

`AGENT_WATCHER_INTERVAL_MS` sets the reconciliation interval in milliseconds.
It must be positive and defaults to `1000`. Set it in the environment that
launches Herdr, then restart the daemon.

## Verify

Scan every supported live agent pane without exposing pane or session IDs:

```sh
./tests/verify-live-agents.sh
```

Verify the sidebar's internal state socket with the same sanitized output:

```sh
./tests/verify-sidebar-state.sh
```

Tier A runs against the deterministic fake Herdr socket and is always enabled:

```sh
cargo test --test e2e_fake_herdr
```

Tier B starts the installed Herdr binary with isolated HOME/XDG directories and
is ignored by default:

```sh
cargo test --test e2e_real_herdr -- --ignored
```

Run all regular tests with `cargo test`.
