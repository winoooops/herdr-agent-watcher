# herdr-agent-watcher

**English** · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

Coding-agent observability for [Herdr](https://herdr.dev): live sidebar cards, lifecycle
notifications, and a zero-config metrics bridge for Claude Code.

![The Agent Watcher sidebar](docs/sidebar.png)

*Five sessions across four agents in one pane — working, finished, idle. The expanded
Claude card also carries CONTEXT, CACHE and COST: numbers Claude Code reports through a
status line and nowhere else, which is what the bridge puts there.*

The idea began inside [Vimeflow](https://github.com/winoooops/vimeflow), where watching
coding agents was one layer of a much larger Electron app.

Pairs with [herdr-agent-title-sync](https://github.com/winoooops/herdr-agent-title-sync),
which keeps Herdr pane titles in step with what each agent is doing.

Local observation is the default. The one thing that leaves your machine is Kimi's
plan-usage lookup, which is off until you turn it on — see
[Kimi usage consent](#kimi-usage-consent).

## Install

```sh
herdr plugin install winoooops/herdr-agent-watcher
```

Requires Herdr 0.8.0+. Install fetches a prebuilt binary for macOS and Linux (x86_64 and
arm64) and checks its SHA256. If no asset matches your platform, or anything about the
download fails, it builds from source instead — that path needs Rust 1.88+, which Herdr
reports rather than installs.

Installing into an already-running Herdr server does not start the daemon. Run it once:

```sh
herdr plugin action invoke restart-daemon --plugin herdr-agent-watcher
```

Herdr's own stock UI works without the sidebar: it receives lifecycle notifications and
pane metadata tokens — `agent_watcher_state`, `agent_watcher_phase`, `agent_watcher_model`,
`agent_watcher_context_pct`, `agent_watcher_attention` and `agent_watcher_title`. Those
names are the integration surface with Herdr and are deliberately stable.

## Verify

Scan every supported live agent pane without exposing pane or session IDs:

```sh
./tests/verify-live-agents.sh
./tests/verify-sidebar-state.sh
```

Tier A runs against the deterministic fake Herdr socket and is always enabled:

```sh
cargo test --test e2e_fake_herdr
```

Tier B starts the installed Herdr binary with isolated HOME/XDG directories and is ignored
by default:

```sh
cargo test --test e2e_real_herdr -- --ignored
```

Run all regular tests with `cargo test`.

## Commands

Every action is invoked the same way:

```sh
herdr plugin action invoke <id> --plugin herdr-agent-watcher
```

Output goes to the plugin log — read the last run with
`herdr plugin log list --plugin herdr-agent-watcher --limit 1`.

| Action | What it does | More |
| --- | --- | --- |
| `restart-daemon` | Start or restart the daemon | |
| `stop-daemon` | Stop the daemon | |
| `open-sidebar` | Open the live sidebar in a new split | [Sidebar](#sidebar) |
| `bind-sidebar-key` | Bind a key to open the sidebar | [Sidebar](#sidebar) |
| `unbind-sidebar-key` | Remove that binding | [Sidebar](#sidebar) |
| `enable-claude-bridge` | Install the metrics bridge into Claude's own settings | [Claude metrics bridge](#claude-metrics-bridge) |
| `disable-claude-bridge` | Restore the settings file to its pre-enable state | [Claude metrics bridge](#claude-metrics-bridge) |
| `doctor` | Say why metrics are missing, and what to do | [Doctor](#doctor) |
| `kimi-consent-on` | Allow Kimi plan-usage lookup | [Kimi usage consent](#kimi-usage-consent) |
| `kimi-consent-off` | Revoke it — takes effect without a restart | [Kimi usage consent](#kimi-usage-consent) |
| `kimi-consent-status` | Show the current setting | [Kimi usage consent](#kimi-usage-consent) |

## Configuration

Settings live in **the plugin's own** `config.toml` — not Herdr's. Herdr ignores tables it
does not recognise, so `[daemon]` placed in `~/.config/herdr/config.toml` does nothing
except make `herdr config check` report an unknown section.

The plugin's config directory is printed by:

```sh
herdr plugin list
```

By default that is `${XDG_CONFIG_HOME:-~/.config}/herdr/plugins/config/herdr-agent-watcher/`.
Create `config.toml` there if it does not exist.

Every key is optional and every bad value falls back to its default, so a mistake costs one
setting rather than the plugin.

```toml
[daemon]
interval_ms = 5000     # how often the daemon reconciles with Herdr; default 1000

[list]
scope = "workspace"    # "all" (default) lists every pane; "workspace" lists only this one
```

Apply a change with:

```sh
herdr plugin action invoke restart-daemon --plugin herdr-agent-watcher
```

A sidebar picks up `[list]` when it next opens.

`AGENT_WATCHER_INTERVAL_MS` still works and outranks the file. It is read from the **Herdr
server's** environment, not your shell, so setting it means restarting Herdr — which is why
the file exists.

Run [`doctor`](#doctor) to see whether a setting was rejected and what was used instead.

## Sidebar

```sh
herdr plugin action invoke open-sidebar --plugin herdr-agent-watcher
```

Each invocation intentionally opens another split. Cards show agent state, agent/model,
title, context use, cache hit rate, cost, tool count, and the three newest tool traces.
Use `j`/`k` or PageUp/PageDown to scroll, `o`/`↵` to expand, `z` to hide idle agents, and
`q`, Escape, or Ctrl-C to close.

With `scope = "workspace"` each sidebar lists only the panes in its own workspace, which is
what makes opening one per workspace useful. A pane the daemon has not placed yet is shown
rather than hidden. Without `HERDR_WORKSPACE_ID` — running the sidebar outside a Herdr pane
— the scope falls back to `all` and the sidebar says so above its footer.

To open it with a key:

```sh
herdr plugin action invoke bind-sidebar-key --plugin herdr-agent-watcher
```

That writes the binding into **Herdr's** config, refusing if the key is already taken and
naming what holds it. The default is `prefix+a` — with Herdr's default prefix, `ctrl+b` then
`a`. Change it in the plugin's `config.toml` before binding:

```toml
[keys]
open_sidebar = "prefix+a"
```

`unbind-sidebar-key` takes it back out. **Run it before uninstalling the plugin**, or the
binding outlives the action it points at — Herdr runs nothing on uninstall.

If the daemon is unavailable or disconnects, the pane says so and waits for a key. Its
state socket (`$HERDR_PLUGIN_STATE_DIR/herdr-agent-watcher-state.sock`,
`WIRE_VERSION = 2`) is plugin-internal, not a public API.

Stop the daemon with:

```sh
herdr plugin action invoke stop-daemon --plugin herdr-agent-watcher
```

## Supported agents

| Agent | Where its metrics come from | Bridge | State |
| --- | --- | --- | --- |
| Claude Code (`claude`, `claude-code`) | its status line, only | **required** — one `enable-claude-bridge` | ✅ |
| Codex CLI (`codex`) | rollout transcript | none | ✅ |
| Kimi Code (`kimi`) | transcript, plus an opt-in usage API | none | ✅ |
| OpenCode (`opencode`) | bundled bridge plugin | installed for you on first bind | ✅ |

Only Claude needs a bridge you invoke, and only because no hook event it emits carries
usage data — its status line is the single channel. OpenCode's bridge is a plugin this one
installs on your behalf; Codex and Kimi need nothing.

To add an agent, implement `AgentAdapter` under `src/agents/` and register it with
`AgentRegistry` in `src/daemon/run.rs`. Agent-specific parsing belongs in the adapter;
Herdr socket details stay behind `HerdrPort`.

## Claude metrics bridge

Claude Code reports CONTEXT, CACHE and COST only through its status line, so it needs a
bridge the other three agents do not.

```sh
herdr plugin action invoke enable-claude-bridge --plugin herdr-agent-watcher
```

That edits Claude's own user settings — it prints which file — and chains your existing
status line behind the bridge so it still runs. No `PATH`, no new shell: every Claude is
bridged in every pane, including sessions already running, which pick it up on their next
status-line render.

```sh
herdr plugin action invoke disable-claude-bridge --plugin herdr-agent-watcher
```

restores the file, removing the `statusLine` entirely if you had none. A status line you
changed after enabling is left alone — the bridge takes back only what is still its own.

## Doctor

Run it when a card reads `— bridge not connected (README)`, or any time metrics are
missing and you want to know why.

```sh
herdr plugin action invoke doctor --plugin herdr-agent-watcher
herdr plugin log list --plugin herdr-agent-watcher --limit 1
```

Doctor names the cause and prints the fix. The one case it cannot fix for you is a project
with its own `statusLine`, which outranks the user tier — it emits a block to paste into
that project's `.claude/settings.local.json`, keeping both the project's status line and
your metrics.

It never reports green on an incomplete picture: managed settings are not discoverable
from here, so if every check passes and metrics are still missing, it says exactly that.

## Troubleshooting

**A Claude card shows `—` for CONTEXT, CACHE and COST.** Those three arrive only through the
status line, so the bridge has to be on — [`doctor`](#doctor) says whether it is. If it is,
the pane has either not rendered a status line since you enabled it (send it a prompt), or
it is running a subagent: the status line then describes the subagent, whose model name
appears on the card and whose usage starts at zero. The main session's numbers return on its
next turn.

**A pane has no card at all.** Herdr reports no `agent_session` for a pane that was open
before the daemon started, so it cannot be bound. Close and reopen the pane.

**A setting changed nothing.** `[daemon]` and `[list]` belong to **the plugin's**
`config.toml`, not Herdr's. Herdr ignores tables it does not recognise, so a section in the
wrong file is silent except in `herdr config check`. `herdr plugin list` prints the right
directory.

**`doctor` passes every check and metrics are still missing.** It says exactly that rather
than reporting green: managed settings, workspace trust and launch flags are not visible
from here, and any of them can outrank a status line.

## Kimi usage consent

Kimi plan-usage lookup sends the configured API key to its `/usages` endpoint, so it stays
disabled until explicitly enabled. Use the Herdr plugin actions `kimi-consent-on`,
`kimi-consent-off`, and `kimi-consent-status`; revocation is picked up by the running
daemon without a restart.

## OpenCode bridge

The first OpenCode bind installs or updates the bundled bridge in OpenCode's plugin
directory. `AGENT_WATCHER_OPENCODE_PLUGINS_DIR` and `AGENT_WATCHER_OPENCODE_BRIDGE_DIR`
override the install and event directories. The bridge plugin keeps its
`agent-watcher-opencode-bridge` filename: it lives in the ported sidecar tree, which is
frozen.

## Design notes

[`DESIGN.md`](DESIGN.md) records why the plugin is shaped this way: why Claude needs a
bridge the other three agents do not, why it installs into Claude's own settings instead of
intercepting `PATH`, why the daemon owns the destination, and what a broken bridge is
required to degrade to.

## Known limitations

- **A pane that was open before the daemon started has no card.** Herdr reports no
  `agent_session` for it, so it cannot be bound. Close and reopen the pane.
- **A pane moved between workspaces keeps its retired id.** A process's pane id is fixed
  at exec, so the daemon no longer recognises it. Doctor shows this rather than failing
  silently, but the session has to be recreated.
- **One Herdr session at a time.** The daemon's lock and sockets live in the user-global
  `$HERDR_PLUGIN_STATE_DIR`, so a second Herdr session replaces the first daemon.
- **`attention.jsonl` is unbounded.** Append-only, with a 192KB per-payload cap but no
  total cap and no rotation.
- **A session's first turn may produce no completion notification**, and four of the five
  hook payloads are stored verbatim — so the guarantee is "the prompt is not persisted",
  not "no user text is persisted". Both live in the frozen `src/agent/**` tree.

## Future work

- [x] Read settings from the plugin's own `config.toml` instead of
      `AGENT_WATCHER_INTERVAL_MS`, so configuration no longer depends on which environment
      the Herdr server was launched from
- [ ] Reap dead bridge directories. Key the reaper on **liveness** — the pane id is absent
      from Herdr's pane list *and* no process holds it — never on unbind (rebind is the
      common case) and never on mtime (it would hit long-idle panes that are still open)
- [x] Publish a prebuilt binary per release so `cargo` is not required to install.
      `[[build]]` runs `scripts/fetch-or-build.sh`, which fetches the asset matching the
      platform, verifies its SHA256, and compiles instead on any failure
- [ ] A `:` command mode in the sidebar, with a full-page doctor view
- [ ] Fix the flaky `pane_without_cwd_uses_herdrs_cwd_for_that_pane` test

## Local development

```sh
cargo build --release
herdr plugin link "$PWD"
herdr plugin action invoke restart-daemon --plugin herdr-agent-watcher
```

`plugin link` skips the build step by design; build the working directory yourself.
