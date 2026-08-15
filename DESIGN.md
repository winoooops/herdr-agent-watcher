# Design notes

Why this plugin is shaped the way it is. Behaviour lives in the code and the README;
this file holds the reasoning that would otherwise only exist in commit messages.

---

## 1. `src/agent/**` is a frozen port

That subtree is a copy of the Vimeflow Electron sidecar's agent layer. It is **read-only**:
extensions go in `src/agents/`, and any unavoidable divergence is registered in
`PORT-SURFACE.md` with the reason.

The rule exists so the two trees can still be diffed against each other. A change made
here that is not recorded there is invisible the next time the upstream tree moves, and
the two silently drift apart. Legacy identifiers inside the frozen tree — including the
`vimeflow_` prefixes and the `agent-watcher-opencode-bridge` filename — are deliberate.
They name a separate namespace from this package and were left alone through the rename.

Public names (environment variables, storage layout, Herdr metadata tokens) use the
`agent_watcher_*` namespace. The metadata tokens in particular are the integration surface
with Herdr's own stock UI: renaming them would break that UI for no benefit, so they did
not move when the package was renamed to `herdr-agent-watcher`.

## 2. Why Claude Code needs a bridge and the other three agents do not

Codex, Kimi and OpenCode expose context window, cache and cost through transcripts or APIs
the daemon can read directly. Claude Code does not.

Its hook payloads carry `session_id`, `prompt_id`, `transcript_path`, `cwd`,
`permission_mode`, `effort`, `hook_event_name` and the subagent fields — and no usage data
at all. Only the `statusLine` command receives it. **A hook-based design is therefore
impossible, not merely worse**, and the transcript alone cannot substitute: it carries
`message.usage` but never the context window size.

That single fact determines the whole shape of the bridge. Everything else follows from
"the status line is the only channel".

## 3. Why the user settings tier, and what it costs

The first implementation intercepted `claude` on `PATH` and passed `--settings`. It worked
and had one fatal property: **every way it could fail was silent and looked identical to
having no bridge at all.** A `PATH` that was not exported, a shell that had not been
reopened, a session started before setup — all produced the same blank card, and two
separate setups lost an hour each to exactly that.

The bridge now installs into Claude's own user settings file, which Claude reads with no
intervention. That removes the `PATH` step, the new-shell step, and the entire class of
failures above.

**The cost is precedence.** `--settings` is the second-highest tier; the user settings file
is the lowest. So a project that defines its own `statusLine` now outranks the bridge, as
does a Claude launched with `--settings` containing one, with `--setting-sources` excluding
`user`, or with `--safe-mode`.

That trade was accepted for one reason: **the new failure is detectable and the old ones
were not.** `doctor` reads the same tiers Claude does and names what shadowed the bridge.
Nothing could detect a `PATH` that was never exported in a shell that no longer exists.

A project's committed `.claude/settings.json` is never edited to work around this. The fix
goes in `.claude/settings.local.json` — personal, gitignored — and the generated script
takes its downstream command as an argument precisely so it can be reused at that tier.
The user keeps both their project's status line and their metrics.

## 4. The daemon owns the destination; the writer never derives it

The status file is keyed by `(cwd, pane_id)`, and the two sides can disagree about `cwd`.
The daemon binds a watcher using Herdr's `foreground_cwd.or(cwd)`. Anything that re-derives
a path from a *different* cwd — a launch-time `$PWD`, or Claude's own mutable
`workspace.current_dir` — can name a different file, and the symptom is metrics that
silently never arrive.

So the writer does not derive a path at all. It asks the daemon over the state socket,
sending the pane id **and** the session id, and the daemon answers only when both match a
live binding. Three things follow:

- Divergence is impossible by construction: one authority, not two sources that must agree.
- A retired pane id is detectable. The daemon does not recognise it, so the writer skips
  and `doctor` reports it — rather than writing to a file nobody reads, which looks exactly
  like success.
- A retired process cannot corrupt a rebound pane, because the session id is compared
  rather than assumed.

A cached index file was considered and rejected: it is simpler, but it can be stale at
exactly the moment that matters — a rebind — which reintroduces the divergence this design
removes.

## 5. Three claims that were wrong, and the pattern behind them

Recorded because each is tempting, and because the pattern repeated three times before it
was named.

- *"Re-reading `HERDR_PANE_ID` per render fixes a stale pane id."* A process environment is
  fixed at exec. Reading a variable more often does not refresh it.
- *"Taking Claude's own cwd removes cwd divergence."* The daemon binds on Herdr's cwd, so
  this adds a second disagreement instead of removing the first.
- *"Session identity comes free once the daemon owns the path."* Moving a lookup does not
  add a check. A request carrying only a pane id returns the path bound *now*, which is
  precisely what a retired process needs in order to overwrite it.

Each claimed a benefit that would follow from a **different** change than the one being
made. That pattern is worth more than the three corrections.

A fourth, found by running the thing rather than reasoning about it: *"already-running
sessions cannot be retrofitted, because settings are read at startup."* They can. Claude
re-reads its settings while running, and a session that predated the bridge began reporting
on its next status-line render.

## 6. Fail-open is stricter here than it looks

The generated scripts are the user's status line and hooks, on **every** Claude they run,
inside Herdr or not. So:

- No `HERDR_PANE_ID` → write nothing, chain the user's own command, exit 0.
- Daemon unreachable, pane unknown, session mismatched → write nothing, exit 0.
- Binary missing → chain, exit 0. Uninstalling the plugin without disabling the bridge
  leaves settings pointing at a script pointing at a deleted binary; that must degrade to
  "no metrics", never to "no status line".
- Every socket operation is bounded. A status line cannot supply its own timeout —
  `timeout(1)` is GNU coreutils and absent from a stock macOS — so the bound lives in the
  binary, and a wedged daemon cannot freeze what the user sees.

A hook that exits non-zero does not merely lose telemetry: it can block the tool call
Claude was about to make. The attention script carries the same guarantees as the status
line, not weaker ones.

**The test that matters is not that the bridge works. It is that a broken bridge is
invisible to someone who does not use Herdr.**

## 7. Editing a file the user owns

`enable` mutates `~/.claude/settings.json`. Every guard there exists because a mistake
destroys someone's configuration:

- The document is read as an untyped value and mutated in place, never rebuilt from a typed
  struct. Keys this crate does not know about are the user's and must survive.
- Only `NotFound` starts from an empty document. A parse error, a permission error, or a
  valid non-object is a hard error that changes nothing — treating any of them as "start
  fresh" would replace the whole file.
- The install record is written **before** the settings it describes. The reverse order
  lets a failed record turn a later `disable` into deletion of the user's original status
  line rather than restoration of it.
- `disable` restores only what is still the bridge's own, entry by entry. A status line or
  a hook the user changed afterwards is theirs now.
- The file is re-read immediately before the rename and the write aborts if it changed;
  Claude's own `/statusline` writes this file too.
- The replacement follows symlinks rather than replacing them, and preserves the original
  mode — a fresh temp file would widen a `0600` settings file to `0644`.
- JSON key order is preserved. Alphabetising someone's configuration file because you
  edited one key in it is rude.

## 8. Doctor reports what it can prove

Findings carry their remedy as **data**, not as pre-rendered prose, so a future TUI renders
the same report without re-deriving any diagnosis.

More importantly, doctor never reports green on an incomplete picture. Managed settings are
not discoverable from here and workspace trust is an operator prerequisite, so when every
check it *can* run passes and metrics are still absent, it says it cannot account for the
absence. A false green returns the reader to the silent-failure state this whole design
exists to end.

Two of its checks are shaped by mistakes that live verification caught, not tests:

- The project-settings walk stops at the tier the bridge owns. Ancestors of a pane's cwd
  reach `$HOME`, so without that bound every pane reported itself shadowed by the bridge,
  with a remedy that chained the status line script to itself. Every test used a temporary
  directory and never walked far enough to see it.
- The daemon probe sends a snapshot request rather than merely connecting. The singleton
  control socket accepts only shutdown; a health check written against it would kill the
  daemon it was diagnosing.
