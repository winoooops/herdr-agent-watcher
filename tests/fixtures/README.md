# Captured Herdr 0.8.0 wire fixtures

Captured from a live Herdr 0.8.0 protocol 19 socket on 2026-08-10, then
sanitized with deterministic demo identifiers and paths.
Requests require an object-valued `params` field, including empty requests.
Pane fields are at `result.snapshot.panes[]`; `agent` is a string and `agent_session`
is an object containing `source`, `agent`, `kind`, and `value`.

The M0a-2 live binding matrix was captured after installing each corresponding Herdr
integration, then sanitized before commit:

| Integration | `agent` | `agent_session.source` | `kind` | `value` fixture |
| --- | --- | --- | --- | --- |
| `herdr integration install codex` | `codex` | `herdr:codex` | `id` | `demo-codex-session` |
| `herdr integration install kimi` | `kimi` | `herdr:kimi` | `id` | `demo-kimi-session` |
| `herdr integration install opencode` | `opencode` | `herdr:opencode` | `id` | `demo-opencode-session` |

The checked-in pane files preserve field names and discriminator values only. Live pane,
terminal, workspace, session, and filesystem identifiers are intentionally excluded.
