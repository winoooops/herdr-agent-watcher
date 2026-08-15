#!/bin/sh
set -eu

state_dir=${HERDR_PLUGIN_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/herdr-agent-watcher}
socket_path=$state_dir/herdr-agent-watcher-state.sock

if [ ! -S "$socket_path" ]; then
  printf 'Agent Watcher state socket not found: %s\n' "$socket_path" >&2
  printf 'Restart the daemon, then run this script again.\n' >&2
  exit 1
fi

printf '{"method":"snapshot"}\n' \
  | nc -U "$socket_path" \
  | jq -e '
      if .version != 1 then error("unsupported Agent Watcher state version")
      elif (.panes | type) != "object" then error("invalid Agent Watcher pane snapshot")
      else {
        version,
        seq,
        pane_count: (.panes | length),
        agents: ([.panes[].agent] | map(select(. != null)) | unique)
      }
      end
    '
