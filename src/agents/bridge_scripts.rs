//! Global Claude scripts. They fail open because Claude runs them outside Herdr too.

use std::path::{Path, PathBuf};

pub(crate) struct Scripts {
    pub statusline: PathBuf,
    pub attention: PathBuf,
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

pub(crate) fn generate(
    bin_dir: &Path,
    agent_watcher: &Path,
    state_socket: &Path,
    downstream: Option<&str>,
) -> Result<Scripts, String> {
    std::fs::create_dir_all(bin_dir)
        .map_err(|error| format!("create {}: {error}", bin_dir.display()))?;
    let statusline = bin_dir.join("statusline.sh");
    let attention = bin_dir.join("attention.sh");

    crate::agents::claude_bridge::write_atomic(
        &statusline,
        &format!(
            "#!/usr/bin/env bash\n\
             # Agent Watcher bridge. Installed by `enable-claude-bridge`.\n\
             set +e +u\n\
             payload=$(cat)\n\
             if [ -n \"${{HERDR_PANE_ID:-}}\" ]; then\n\
             \x20 printf '%s' \"$payload\" | {aw} claude-bridge --write \\\n+             \x20   --pane \"$HERDR_PANE_ID\" --socket {socket} >/dev/null 2>&1\n\
             fi\n\
             downstream={baked}\n\
             if [ \"${{1:-}}\" = '--' ]; then downstream=${{2-}}; fi\n\
             [ -n \"$downstream\" ] && printf '%s' \"$payload\" | sh -c \"$downstream\"\n\
             exit 0\n",
            aw = shell_quote(&agent_watcher.to_string_lossy()),
            socket = shell_quote(&state_socket.to_string_lossy()),
            baked = shell_quote(downstream.unwrap_or_default()),
        ),
        0o755,
    )?;

    crate::agents::claude_bridge::write_atomic(
        &attention,
        &format!(
            "#!/usr/bin/env bash\n\
             # $1 = hook event name, $2 = append|name-only\n\
             set +e +u\n\
             payload=$(cat)\n\
             if [ -n \"${{HERDR_PANE_ID:-}}\" ]; then\n\
             \x20 printf '%s' \"$payload\" | {aw} claude-bridge --write-attention \\\n+             \x20   --pane \"$HERDR_PANE_ID\" --socket {socket} \\\n+             \x20   --event \"${{1:-Unknown}}\" --mode \"${{2:-append}}\" >/dev/null 2>&1\n\
             fi\n\
             exit 0\n",
            aw = shell_quote(&agent_watcher.to_string_lossy()),
            socket = shell_quote(&state_socket.to_string_lossy()),
        ),
        0o755,
    )?;

    Ok(Scripts {
        statusline,
        attention,
    })
}
