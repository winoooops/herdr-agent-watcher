# AGENTS.md — Agent Watcher

Guidance for AI coding agents working in this repository.

## Project overview

**Agent Watcher** is a coding-agent observability plugin for
[Herdr](https://github.com/) (a terminal/multiplexer host, min version 0.8.0).
It watches local coding-agent state and transcripts, reports pane metadata, and
sends lifecycle notifications. Supported agents (all live-verified):

- Claude Code (`claude`, `claude-code`)
- Codex CLI (`codex`)
- Kimi Code (`kimi`)
- OpenCode (`opencode`)

Single Rust crate (`agent-watcher`, Apache-2.0, edition 2021, **Rust 1.85+
required**). It builds one binary with three subcommands (`src/main.rs`):

- `agent-watcher daemon` — the long-running reconcile loop
- `agent-watcher stop` — stop the daemon (singleton takeover)
- `agent-watcher kimi-consent <on|off|status>` — manage Kimi usage-reporting consent

The plugin manifest is `herdr-plugin.toml`; it registers the daemon as a
startup command and the four Herdr plugin actions (`restart-daemon`,
`stop-daemon`, `kimi-consent-on/off/status`).

> **History note:** the project was renamed from `vimeflow-agents` on
> 2026-08-10. The `src/agent/**` tree was ported from the Vimeflow Electron
> sidecar and intentionally retains legacy protocol identifiers for mechanical
> source compatibility — see `PORT-SURFACE.md` for the exact mapping. Public
> package, plugin, storage, environment, and metadata names use the
> product-neutral `agent-watcher` / `agent_watcher_*` namespace. Some docs
> under `src/agent/` (README.md, adapter/README.md, adapter/REVIEW.md) are
> legacy Vimeflow-era design documents; their file paths
> (`crates/backend/...`) and frontend/IPC references describe the old host,
> not this repo. Treat them as design history, not current truth.

## Runtime architecture

The daemon (`src/daemon/run.rs`) is a poll loop, default interval 1000 ms
(`AGENT_WATCHER_INTERVAL_MS`):

1. Claim a singleton lock (`daemon/singleton.rs`); exit if another daemon holds
   it. Reload Kimi consent if the consent file changed.
2. Query Herdr over a Unix-socket JSON-RPC protocol (`src/herdr/client.rs`,
   one request per connection, 3 s read timeout; socket from
   `HERDR_SOCKET_PATH`, default `$XDG_CONFIG_HOME/herdr/herdr.sock`).
3. Diff the pane list against known bindings (`daemon/reconcile.rs`) into
   `Bind` / `Unbind` actions, tracking each pane's shell PID and cwd in
   `PtyState` (`src/terminal/`).
4. Dispatch actions through `AgentRegistry` (`src/agents/mod.rs`): the trait
   here is small — `ids()`, `bind(BoundPane)`, `unbind(pane_id)`. The single
   production implementation is `SidecarAdapter` (`src/agents/sidecar.rs`),
   which claims all supported agent IDs and bridges into the ported watcher
   pipeline in `src/agent/`.
5. Events flow out through the runtime-neutral `EventSink` trait
   (`src/runtime/`); production uses `HerdrSink` → `LiveHerdrPort`
   (`src/daemon/sink.rs`) which pushes notifications and pane metadata tokens
   (`agent_watcher_state`, `agent_watcher_phase`, `agent_watcher_model`,
   `agent_watcher_context_pct`, `agent_watcher_attention`,
   `agent_watcher_title`) back to Herdr. Tests use `FakeEventSink`.

Inside `src/agent/adapter/`, each agent has an adapter behind the legacy
five-method `AgentAdapter` trait (`status_source`, `parse_status`,
`validate_transcript`, `tail_transcript`, `agent_type`); `base/` holds the
shared watcher runtime (`notify` file watcher with 100 ms debounce + 3 s
polling fallback, transcript registry, diagnostics). Provider submodules:
`claude_code/`, `codex/`, `kimi/`, `opencode/` (OpenCode additionally installs
a bundled TypeScript bridge plugin, `opencode/plugin/`, overridable via
`AGENT_WATCHER_OPENCODE_PLUGINS_DIR` / `AGENT_WATCHER_OPENCODE_BRIDGE_DIR`).

To add a new agent: implement the adapter trait under `src/agent/adapter/`,
register the agent ID in `SIDECAR_AGENT_IDS` (`src/agents/sidecar.rs`), and add
detection in `src/agent/detector.rs`. Agent-specific parsing belongs in the
adapter; Herdr socket details stay behind the `herdr` module / `HerdrPort`.

## Module map

- `src/main.rs` — CLI entrypoint (daemon | stop | kimi-consent)
- `src/lib.rs` — library root (`agent_watcher`)
- `src/daemon/` — run loop, reconcile/diff, singleton lock, consent reload, Herdr event sink
- `src/herdr/` — Unix-socket JSON-RPC client + wire API types
- `src/agents/` — registry, `BoundPane`, small `AgentAdapter` trait, `SidecarAdapter`, Kimi consent
- `src/agent/` — ported watcher pipeline: adapters, detector, notifications, review/reply, event types
- `src/runtime/` — `EventSink` abstraction (`FakeEventSink` for tests)
- `src/terminal/` — `PtyState` (pane → pid/cwd), session types
- `src/filesystem/` — `open_nofollow` scoped file access
- `src/aliases.rs`, `src/debug.rs` — agent alias store, debug logging
- `bindings/` — **generated** TypeScript event types (gitignored)
- `tests/` — e2e suites, fixtures, shared fake-Herdr support
- `examples/probe.rs` — manual probe: lists panes and fires a test notification

## Build and test commands

Requires Rust 1.85+ and (for install/e2e tier B) Herdr 0.8.0+.

```sh
cargo build                                # debug build
cargo build --release                      # release binary at target/release/agent-watcher
cargo test                                 # all unit tests + tier-A e2e (also regenerates bindings/)
cargo test --test e2e_fake_herdr           # tier A only: deterministic fake Herdr socket
cargo test --test e2e_real_herdr -- --ignored   # tier B: real installed herdr, isolated HOME/XDG
./tests/verify-live-agents.sh              # scan live panes for agent_watcher_* tokens (needs live Herdr + jq + nc)
cargo run --example probe                  # manual live probe against the local Herdr socket
```

Install for local development:

```sh
cargo build --release
herdr plugin link "$PWD"
herdr plugin action invoke restart-daemon --plugin agent-watcher
# stop with: herdr plugin action invoke stop-daemon --plugin agent-watcher
```

There is no CI configuration in the repo and no formatter/linter config
checked in; match the existing style (standard `rustfmt` defaults) and keep
`cargo test` green.

## Testing strategy and conventions

- **Unit tests** live inline in `#[cfg(test)]` modules next to the code
  (e.g. `src/agents/mod.rs`, `src/herdr/client.rs`). Some live in dedicated
  sibling files (e.g. `claude_code/transcript_fixture_tests.rs`).
- **Fixtures** in `tests/fixtures/` are captured from a live Herdr 0.8.0
  (protocol 19) socket and sanitized with deterministic demo identifiers —
  never commit live pane/session/filesystem identifiers (see
  `tests/fixtures/README.md`). Per-adapter JSONL fixtures live under
  `src/agent/adapter/<agent>/fixtures/`.
- **Tier A** (`tests/e2e_fake_herdr.rs`) drives the real daemon against a fake
  Herdr Unix socket (`tests/support/mod.rs`); always enabled.
- **Tier B** (`tests/e2e_real_herdr.rs`) spawns the installed `herdr` binary
  with isolated HOME/XDG dirs; `#[ignore]`d by default. Note macOS socket-path
  length limits — tests use short tempdir prefixes under `/tmp`.
- A crate feature `e2e-test` exists in `Cargo.toml` for e2e-only hooks.
- **TypeScript bindings**: event types in `src/agent/types.rs` and
  `src/aliases.rs` derive `ts_rs::TS` with `#[cfg_attr(test, ts(export))]`;
  running `cargo test` regenerates `bindings/*.ts` (gitignored). Keep these
  derives in place when touching event payload types.

## Code style guidelines

- Errors at adapter/service boundaries are `Result<_, String>`; typed
  `thiserror` enums are used at the wire/client layer (`HerdrClientError`).
- All event emission goes through `Arc<dyn EventSink>` — never print or
  socket-write from adapter/parsing code.
- Parsers call **domain-named helpers** (`bash_command(item)`,
  `current_usage(value)`) rather than long `.get().and_then(...)` chains.
  There is intentionally **no shared `adapter::json` helper module** — keep
  JSON path helpers provider-private until a second adapter proves the same
  shape useful.
- Parser tolerance: upstream schemas drift, so decode partially and skip
  unknown lines rather than failing hard.
- Lifecycle ordering is load-bearing: `WatcherHandle::Drop` must drop the
  `notify` watcher *before* signalling/joining the poll thread and stopping
  the transcript tailer (documented in `src/agent/README.md`). Preserve that
  order.
- Legacy Vimeflow identifiers inside `src/agent/**` are deliberate (see
  `PORT-SURFACE.md`); do not mass-rename them. New public names use the
  `agent_watcher_*` namespace.

## Security considerations

- **Trust-root enforcement**: every adapter declares a `StatusSource` with a
  `trust_root`; `base/path_security.rs` canonicalizes and rejects any status
  or transcript path escaping that root, re-checking after `create_dir_all` to
  catch symlink races. File reads go through `filesystem::scope::open_nofollow`.
- **Kimi usage consent**: plan-usage lookup sends the user's API key to Kimi's
  `/usages` endpoint, so it is disabled by default and gated behind the
  explicit `kimi-consent` actions; revocation is picked up live by the running
  daemon. Never enable it programmatically.
- **Fixtures are sanitized**: live pane IDs, session IDs, and real paths must
  never be committed; the live verifier (`tests/verify-live-agents.sh`)
  redacts pane titles.
- The Herdr client speaks to a local Unix socket only; requests require an
  object-valued `params` field, and the 3 s read timeout must stay below the
  daemon's 5 s singleton takeover deadline.
