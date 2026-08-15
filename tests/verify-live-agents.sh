#!/bin/sh
set -eu

socket_path=${HERDR_SOCKET_PATH:-${XDG_CONFIG_HOME:-$HOME/.config}/herdr/herdr.sock}
filter='[
  .result.panes[]
  | select(.agent as $agent | ["claude", "claude-code", "codex", "kimi", "opencode"] | index($agent))
  | {
      agent,
      active: (.agent_session.value? | type == "string"),
      tokens: (
        .tokens // {}
        | with_entries(select(.key | startswith("agent_watcher_")))
        | if has("agent_watcher_title") then .agent_watcher_title = "<redacted>" else . end
      )
    }
] as $panes
| [$panes[] | select(.active)] as $active
| [$active[] | select((.tokens | length) == 0) | .agent] | unique as $missing
| if ($panes | length) == 0 then error("no supported live agent panes found")
  elif ($active | length) == 0 then error("no supported panes have an active agent session")
  elif ($missing | length) > 0 then error("active panes have no Agent Watcher tokens: " + ($missing | join(", ")))
  else $panes
  end'

herdr plugin action invoke restart-daemon --plugin agent-watcher >/dev/null

attempt=0
while [ "$attempt" -lt 5 ]; do
  response=$(printf '{"id":"verify","method":"pane.list","params":{}}\n' | nc -U "$socket_path")
  if output=$(printf '%s\n' "$response" | jq -e "$filter" 2>/dev/null); then
    printf '%s\n' "$output"
    exit 0
  fi
  attempt=$((attempt + 1))
  sleep 1
done

printf '%s\n' "$response" | jq -e "$filter"
