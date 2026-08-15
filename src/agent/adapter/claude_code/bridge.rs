//! Claude Code bridge file generation.
//!
//! Generates per-session scripts, settings, and hook streams that allow Claude
//! Code to publish status and attention events for Vimeflow's file watchers.

use crate::aliases::{build_alias_lines, AgentAlias};

use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const BRIDGE_RUNTIME_DIR: &str = "runtime";
const BRIDGE_WORKSPACES_DIR: &str = "workspaces";
const BRIDGE_SESSIONS_DIR: &str = "sessions";

/// Result of generating statusline bridge files
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields read by spawn_pty and future cleanup logic
pub struct BridgeFiles {
    /// Directory containing the generated per-session status bridge files
    pub agent_status_dir_path: PathBuf,
    /// Path to the generated statusline script
    pub script_path: PathBuf,
    /// Path to the script Claude invokes for each semantic attention hook.
    /// The script reads one hook payload from stdin and appends it to
    /// `attention_file_path` as one JSONL record.
    pub attention_script_path: PathBuf,
    /// Directory prepended to PATH so aliases like `env ... claude` are bridged
    pub shim_dir_path: PathBuf,
    /// Path to the generated `claude` executable shim
    pub claude_shim_path: PathBuf,
    /// Path to the generated settings.json
    pub settings_path: PathBuf,
    /// Path to the status.json file that the script writes to
    pub status_file_path: PathBuf,
    /// Path to the append-only Claude hook transport stream. The backend tails
    /// this file and converts new records into live `agent-attention` events;
    /// it is not the notification center's history store.
    pub attention_file_path: PathBuf,
    /// Path to the shell init script that installs the `claude` PATH shim
    pub shell_init_path: PathBuf,
    /// Path to generated zsh environment file used when the user's shell is zsh
    pub zsh_env_path: PathBuf,
    /// Path to generated zsh rc file used when the user's shell is zsh
    pub zsh_rc_path: PathBuf,
}

fn sanitized_component(raw: &str) -> String {
    let mut component = String::with_capacity(raw.len());
    let mut previous_was_dash = false;

    for ch in raw.chars() {
        let next = if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };

        if next == '-' {
            if previous_was_dash {
                continue;
            }
            previous_was_dash = true;
        } else {
            previous_was_dash = false;
        }

        component.push(next);
    }

    let trimmed = component.trim_matches('-');
    if trimmed.is_empty() {
        "workspace".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Quote a path so it survives being passed to `statusLine.command` and run
/// through a POSIX shell. Wraps the value in single quotes; any embedded
/// single quote is escaped by exiting the quoted string, emitting an escaped
/// quote, and re-entering.
fn shell_quote_path(path: &str) -> String {
    if path.is_empty() {
        return "''".to_string();
    }
    // A conservative whitelist: if the path contains no shell metacharacters,
    // leave it unquoted. Otherwise wrap the whole thing in single quotes.
    let needs_quote = path.bytes().any(|b| {
        b.is_ascii_whitespace()
            || b == b'\''
            || b == b'"'
            || b == b'`'
            || b == b'$'
            || b == b'\\'
            || b == b'|'
            || b == b'&'
            || b == b';'
            || b == b'<'
            || b == b'>'
            || b == b'('
            || b == b')'
            || b == b'#'
            || b == b'*'
            || b == b'?'
            || b == b'['
            || b == b']'
            || b == b'!'
            || b == b'~'
    });
    if !needs_quote {
        return path.to_string();
    }
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('\'');
    for ch in path.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn cwd_basename(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(OsStr::to_str)
        .map(sanitized_component)
        .unwrap_or_else(|| "workspace".to_string())
}

/// Maximum bytes for the sanitized basename portion of a workspace bridge
/// bucket, leaving room for the hyphen separator and the 12-character hex
/// hash (6 bytes × 2 hex digits) on filesystems with a 255-byte filename
/// limit.
const MAX_BUCKET_BASENAME_BYTES: usize = 242;

fn workspace_bridge_bucket(cwd: &Path) -> String {
    let digest = Sha256::digest(cwd.to_string_lossy().as_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    let basename = cwd_basename(cwd);
    let truncated = if basename.len() > MAX_BUCKET_BASENAME_BYTES {
        basename
            .char_indices()
            .take_while(|(i, _)| *i < MAX_BUCKET_BASENAME_BYTES)
            .map(|(_, ch)| ch)
            .collect()
    } else {
        basename
    };
    format!("{}-{}", truncated, hex)
}

pub(crate) fn workspace_bridge_root(app_data_dir: &Path, cwd: &Path) -> PathBuf {
    app_data_dir
        .join(BRIDGE_RUNTIME_DIR)
        .join(BRIDGE_WORKSPACES_DIR)
        .join(workspace_bridge_bucket(cwd))
}

pub(crate) fn session_bridge_dir(app_data_dir: &Path, cwd: &Path, session_id: &str) -> PathBuf {
    workspace_bridge_root(app_data_dir, cwd)
        .join(BRIDGE_SESSIONS_DIR)
        .join(session_id)
}

pub(crate) fn session_status_file(app_data_dir: &Path, cwd: &Path, session_id: &str) -> PathBuf {
    session_bridge_dir(app_data_dir, cwd, session_id).join("status.json")
}

/// Derive the Claude attention stream beside the session's status snapshot.
/// Both files therefore share the same workspace/session isolation and are
/// removed together when the session bridge directory is cleaned up.
pub(crate) fn session_attention_file(status_file: &Path) -> PathBuf {
    status_file.with_file_name("attention.jsonl")
}

/// Write a script file atomically and make it executable on Unix.
///
/// Uses a temp-file + rename pattern to avoid `ETXTBSY` ("Text file busy")
/// when the script is executed immediately after creation, which can happen
/// in heavily loaded CI environments.
#[cfg(unix)]
fn write_executable_script(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp_path = path.with_extension("tmp");
    let result = (|| {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp_path, path)?;
        // Sync parent directory so the rename is fully committed before callers
        // try to execute the script, avoiding ETXTBSY on heavily loaded runners.
        // Best-effort: a failed directory fsync must not abort the spawn after the
        // file has already been renamed into place.
        if let Some(parent) = path.parent() {
            let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

#[cfg(not(unix))]
fn write_executable_script(path: &Path, content: &str) -> std::io::Result<()> {
    std::fs::write(path, content)
}

/// Generate the statusline bridge files for a session.
///
/// Creates:
/// 1. `<dir>/statusline.sh` — receives JSON on stdin, writes to `status.json`
/// 2. `<dir>/settings.json` — Claude Code settings overlay pointing to the script
/// 3. `<shim_dir>/claude` — executable shim that wraps the real `claude` binary
/// 4. `<dir>/init.sh` — shell startup hook that prepends the shim to PATH
/// 5. `<dir>/.zshenv` and `<dir>/.zshrc` — zsh-specific startup hooks (when shell is zsh)
///
/// # Arguments
/// * `agent_status_dir` - Directory to create files in (typically under app data)
/// * `session_id` - Session identifier for the comment header
/// * `shim_dir` - Optional directory for the PATH shim; `None` falls back to `<agent_status_dir>/bin`
/// * `aliases` - User-defined agent aliases to inject into the shell init
/// * `shim_enabled` - Master toggle; when false, no aliases are injected
///
/// # Returns
/// * `Ok(BridgeFiles)` with paths to generated files
/// * `Err(String)` if directory creation or file writing fails
pub fn generate_bridge_files(
    agent_status_dir: &str,
    session_id: &str,
    shim_dir: Option<&str>,
    aliases: &[AgentAlias],
    shim_enabled: bool,
) -> Result<BridgeFiles, String> {
    let dir = Path::new(agent_status_dir);

    // Create the session directory
    fs::create_dir_all(dir)
        .map_err(|e| format!("failed to create agent status directory: {}", e))?;

    let script_path = dir.join("statusline.sh");
    let attention_script_path = dir.join("attention.sh");
    let shim_dir_path = shim_dir
        .map(Path::new)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| dir.join("bin"));
    let claude_shim_path = shim_dir_path.join("claude");
    let settings_path = dir.join("settings.json");
    let status_file_path = dir.join("status.json");
    let attention_file_path = session_attention_file(&status_file_path);
    let shell_init_path = dir.join("init.sh");
    let zsh_env_path = dir.join(".zshenv");
    let zsh_rc_path = dir.join(".zshrc");

    // Generate the statusline script. The path is passed via env var
    // (VIMEFLOW_STATUS_FILE) set by spawn_pty — avoids embedding paths
    // in the script which breaks if CWD contains quotes or metacharacters.
    let script_content = format!(
        "#!/usr/bin/env bash\n\
         # Auto-generated by Vimeflow for session {session_id}\n\
         # Receives Claude Code statusline JSON on stdin, writes to status file\n\
         cat > \"$VIMEFLOW_STATUS_FILE\"\n",
        session_id = session_id,
    );

    write_executable_script(&script_path, &script_content)
        .map_err(|e| format!("failed to write statusline script: {}", e))?;

    // Status updates overwrite one current snapshot, but attention hooks are
    // discrete events. Preserve their order by appending one complete payload
    // per line; the Claude watcher consumes only newly appended records.
    let attention_script_content = format!(
        "#!/usr/bin/env bash\n\
         # Auto-generated by Vimeflow for session {session_id}\n\
         max_payload_bytes=196608\n\
         payload=$(cat)\n\
         payload_bytes=$(printf '%s' \"$payload\" | wc -c | tr -d ' ')\n\
         if [ \"$payload_bytes\" -le \"$max_payload_bytes\" ]; then\n\
           printf '%s\\n' \"$payload\" >> \"$VIMEFLOW_ATTENTION_FILE\"\n\
           exit 0\n\
         fi\n\
         hook_event_name=$(printf '%s' \"$payload\" | sed -n 's/.*\"hook_event_name\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p' | head -n 1)\n\
         transcript_path=$(printf '%s' \"$payload\" | sed -n 's/.*\"transcript_path\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p' | head -n 1)\n\
         escape_json() {{ printf '%s' \"$1\" | sed 's/\\\\/\\\\\\\\/g; s/\"/\\\\\"/g'; }}\n\
         hook_event_name=$(escape_json \"${{hook_event_name:-Unknown}}\")\n\
         transcript_path=$(escape_json \"$transcript_path\")\n\
         if [ -n \"$transcript_path\" ]; then\n\
           printf '{{\"hook_event_name\":\"%s\",\"transcript_path\":\"%s\",\"vimeflow_truncated\":true}}\\n' \\\n\
             \"$hook_event_name\" \"$transcript_path\" >> \"$VIMEFLOW_ATTENTION_FILE\"\n\
         else\n\
           printf '{{\"hook_event_name\":\"%s\",\"vimeflow_truncated\":true}}\\n' \\\n\
             \"$hook_event_name\" >> \"$VIMEFLOW_ATTENTION_FILE\"\n\
         fi\n",
        session_id = session_id,
    );
    write_executable_script(&attention_script_path, &attention_script_content)
        .map_err(|e| format!("failed to write attention hook script: {}", e))?;
    // Bridge generation starts this session's transport stream empty. Clearing
    // the renderer notification list does not truncate this file; the live
    // watcher already has its cursor at EOF and never replays old records.
    fs::File::create(&attention_file_path)
        .map_err(|e| format!("failed to create attention event file: {}", e))?;

    fs::create_dir_all(&shim_dir_path)
        .map_err(|e| format!("failed to create claude shim directory: {}", e))?;

    // Generate a PATH shim in addition to shell startup. Aliases such as
    // `cc='env ... claude ...'` execute the external `env` binary, so a shell
    // function named `claude` is not in command position and cannot inject the
    // settings overlay. A PATH-level shim still catches those launches.
    let claude_shim_content = format!(
        "#!/usr/bin/env bash\n\
         # Auto-generated by Vimeflow for session {session_id}\n\
         shim_dir=\"$(cd \"$(dirname \"$0\")\" && pwd -P)\" || {{ printf '%s\\n' 'Vimeflow claude shim: unable to determine shim directory' >&2; exit 1; }}\n\
         if [ -z \"${{VIMEFLOW_CLAUDE_SETTINGS:-}}\" ]; then\n\
           printf '%s\\n' 'Vimeflow claude shim: VIMEFLOW_CLAUDE_SETTINGS not set' >&2\n\
           exit 1\n\
         fi\n\
         shopt -s execfail\n\
         IFS=':' read -r -a path_parts <<< \"${{PATH:-}}\"\n\
         for dir in \"${{path_parts[@]}}\"; do\n\
           if [ -z \"$dir\" ]; then\n\
             dir='.'\n\
           fi\n\
           resolved_dir=\"$(cd \"$dir\" 2>/dev/null && pwd -P)\" || continue\n\
           if [ \"$resolved_dir\" = \"$shim_dir\" ]; then\n\
             continue\n\
           fi\n\
           candidate=\"$dir/claude\"\n\
           if [ -x \"$candidate\" ]; then\n\
             exec \"$candidate\" --settings \"$VIMEFLOW_CLAUDE_SETTINGS\" \"$@\" || continue\n\
           fi\n\
         done\n\
         printf '%s\\n' 'Vimeflow claude shim: unable to locate real claude' >&2\n\
         exit 127\n",
        session_id = session_id,
    );
    write_executable_script(&claude_shim_path, &claude_shim_content)
        .map_err(|e| format!("failed to write claude shim: {}", e))?;

    // Generate the settings.json overlay using serde_json for proper escaping
    let attention_command = shell_quote_path(&attention_script_path.to_string_lossy());
    let hook = || {
        serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": attention_command
            }]
        })
    };
    let signal_hook = |event_name: &str| {
        let record = serde_json::json!({ "hook_event_name": event_name }).to_string();
        serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": format!(
                    "printf '%s\\n' {} >> \"$VIMEFLOW_ATTENTION_FILE\"",
                    shell_quote_path(&record),
                )
            }]
        })
    };
    let settings = serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": shell_quote_path(&script_path.to_string_lossy()),
            "refreshInterval": 5
        },
        "hooks": {
            "UserPromptSubmit": [signal_hook("UserPromptSubmit")],
            "Stop": [hook()],
            "PermissionRequest": [hook()],
            "PreToolUse": [{
                "matcher": "AskUserQuestion",
                "hooks": [{
                    "type": "command",
                    "command": attention_command
                }]
            }],
            "StopFailure": [hook()]
        }
    });
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&settings)
            .map_err(|e| format!("failed to serialize settings: {}", e))?,
    )
    .map_err(|e| format!("failed to write settings.json: {}", e))?;

    // Generate shell init script that installs the PATH shim.
    // Must unalias first — if the user has `alias claude='... claude ...'`,
    // `claude` should still resolve to the shim instead of the alias. Always
    // keep the shim at the front of PATH: user startup files may have already
    // put it later in PATH and then prepended `/usr/local/bin` ahead of it.
    // The shim is removed from PATH first and then prepended once, so sourcing
    // init.sh multiple times (e.g. .zshenv + .zshrc) is idempotent and never
    // leaves the shim behind a user-prepended real claude binary.
    // Uses tr/grep (external commands) instead of shell word-splitting so the
    // logic is identical in bash, zsh, and POSIX sh.
    let alias_lines = if shim_enabled {
        build_alias_lines(aliases)
    } else {
        String::new()
    };

    let shell_init_content = format!(
        "# Auto-generated by Vimeflow for session {session_id}\n\
         # Installs claude PATH shim to inject statusline bridge config\n\
         unalias claude 2>/dev/null\n\
         unset -f claude 2>/dev/null\n\
         if [ -n \"$VIMEFLOW_CLAUDE_SHIM_DIR\" ]; then\n\
           path_ends_colon=\"\"\n\
           case \"$PATH\" in *:) path_ends_colon=1 ;; esac\n\
           new_path=$(printf '%s' \"$PATH\" | tr ':' '\\n' | grep -Fxv \"$VIMEFLOW_CLAUDE_SHIM_DIR\" | tr '\\n' ':')\n\
           new_path=\"${{new_path%:}}\"\n\
           [ -n \"$path_ends_colon\" ] && new_path=\"${{new_path}}:\"\n\
           export PATH=\"$VIMEFLOW_CLAUDE_SHIM_DIR${{new_path:+:$new_path}}\"\n\
           hash -r 2>/dev/null || rehash 2>/dev/null || true\n\
         fi\n\
         {alias_lines}",
        session_id = session_id,
        alias_lines = alias_lines,
    );

    fs::write(&shell_init_path, &shell_init_content)
        .map_err(|e| format!("failed to write shell init script: {}", e))?;

    // zsh does not read BASH_ENV. macOS defaults to zsh, so give spawned zsh
    // sessions a private ZDOTDIR that preserves user startup files from HOME
    // and then installs the Claude wrapper.
    let zsh_env_content = format!(
        "# Auto-generated by Vimeflow for session {session_id}\n\
         [ -f \"$HOME/.zshenv\" ] && source \"$HOME/.zshenv\"\n\
         [ -n \"$VIMEFLOW_AGENT_INIT\" ] && source \"$VIMEFLOW_AGENT_INIT\"\n",
        session_id = session_id,
    );
    fs::write(&zsh_env_path, &zsh_env_content)
        .map_err(|e| format!("failed to write zsh env script: {}", e))?;

    let zsh_rc_content = format!(
        "# Auto-generated by Vimeflow for session {session_id}\n\
         [ -f \"$HOME/.zshrc\" ] && source \"$HOME/.zshrc\"\n\
         [ -n \"$VIMEFLOW_AGENT_INIT\" ] && source \"$VIMEFLOW_AGENT_INIT\"\n",
        session_id = session_id,
    );
    fs::write(&zsh_rc_path, &zsh_rc_content)
        .map_err(|e| format!("failed to write zsh rc script: {}", e))?;

    log::info!(
        "Generated statusline bridge for session {}: {}",
        session_id,
        dir.display()
    );

    Ok(BridgeFiles {
        agent_status_dir_path: dir.to_path_buf(),
        script_path,
        attention_script_path,
        shim_dir_path,
        claude_shim_path,
        settings_path,
        status_file_path,
        attention_file_path,
        shell_init_path,
        zsh_env_path,
        zsh_rc_path,
    })
}

/// Clean up statusline bridge files for a session.
///
/// Removes the entire session directory if it exists, and optionally removes
/// the separate shim directory created for PATH-level claude interception.
pub fn cleanup_bridge_files(agent_status_dir: &str, shim_dir: Option<&str>) -> Result<(), String> {
    let dir = Path::new(agent_status_dir);
    if dir.exists() {
        fs::remove_dir_all(dir).map_err(|e| format!("failed to clean up bridge files: {}", e))?;
        log::info!("Cleaned up bridge files: {}", dir.display());
    }
    if let Some(shim) = shim_dir {
        let shim_path = Path::new(shim);
        if shim_path.exists() {
            fs::remove_dir_all(shim_path)
                .map_err(|e| format!("failed to clean up shim directory: {}", e))?;
            log::info!("Cleaned up shim directory: {}", shim_path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::process::{Command, Stdio};

    #[test]
    fn session_bridge_dir_uses_app_data_safe_workspace_bucket() {
        let app_data = PathBuf::from("/tmp/Vimeflow Data");
        let cwd = PathBuf::from("/Users/test/Project With Quote's");

        let dir = session_bridge_dir(&app_data, &cwd, "session-abc");
        let workspace_root = workspace_bridge_root(&app_data, &cwd);
        let bucket = workspace_root
            .file_name()
            .and_then(OsStr::to_str)
            .expect("workspace bucket should be UTF-8");

        assert!(dir.starts_with(&app_data));
        assert_eq!(dir, workspace_root.join("sessions").join("session-abc"));
        assert!(!dir.components().any(|c| c.as_os_str() == ".vimeflow"));
        assert!(bucket.starts_with("project-with-quote-s-"));
        assert!(
            bucket
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "bucket should be safe as one macOS/Linux path component: {bucket}"
        );
    }

    #[test]
    fn workspace_bridge_bucket_distinguishes_same_basename_paths() {
        let app_data = PathBuf::from("/tmp/vimeflow-data");
        let first = workspace_bridge_root(&app_data, Path::new("/tmp/one/app"));
        let second = workspace_bridge_root(&app_data, Path::new("/tmp/two/app"));

        assert_ne!(first, second);
        assert!(first.starts_with(&app_data));
        assert!(second.starts_with(&app_data));
    }

    #[test]
    fn workspace_bridge_bucket_truncates_long_basenames() {
        let app_data = PathBuf::from("/tmp/vimeflow-data");
        let long_name = "a".repeat(300);
        let cwd = PathBuf::from(format!("/tmp/{long_name}"));

        let bucket = workspace_bridge_bucket(&cwd);
        let bucket_bytes = bucket.as_bytes();
        assert!(
            bucket_bytes.len() <= 255,
            "bucket component must fit in a 255-byte filesystem filename: {bucket}"
        );
        // Truncated basename + hyphen + 6-char hash.
        assert_eq!(
            bucket.matches('-').count(),
            1,
            "bucket should contain exactly one separator: {bucket}"
        );
        let (basename, hash) = bucket.rsplit_once('-').unwrap();
        assert_eq!(hash.len(), 12, "hash suffix should be 12 hex chars");
        assert_eq!(basename.len(), MAX_BUCKET_BASENAME_BYTES);
    }

    #[test]
    fn shell_quote_path_quotes_metacharacters() {
        assert_eq!(shell_quote_path("/safe/path"), "/safe/path");
        assert_eq!(
            shell_quote_path("/Users/foo/Application Support/vimeflow/statusline.sh"),
            "'/Users/foo/Application Support/vimeflow/statusline.sh'"
        );
        assert_eq!(
            shell_quote_path("/path/with'quote/statusline.sh"),
            "'/path/with'\\''quote/statusline.sh'"
        );
        assert_eq!(shell_quote_path(""), "''");
    }

    #[test]
    fn generates_bridge_files_in_temp_dir() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let dir = tmp.path().join("session-abc");

        let result = generate_bridge_files(dir.to_str().unwrap(), "session-abc", None, &[], true);
        assert!(result.is_ok(), "generate_bridge_files should succeed");

        let files = result.unwrap();

        // Script should exist and be executable
        assert!(files.script_path.exists());
        assert!(files.attention_script_path.exists());
        assert!(files.attention_file_path.exists());
        assert!(files.shim_dir_path.exists());
        assert!(files.claude_shim_path.exists());
        assert!(files.zsh_env_path.exists());
        assert!(files.zsh_rc_path.exists());
        #[cfg(unix)]
        {
            let meta = fs::metadata(&files.script_path).unwrap();
            assert!(
                meta.permissions().mode() & 0o111 != 0,
                "script should be executable"
            );
            let shim_meta = fs::metadata(&files.claude_shim_path).unwrap();
            assert!(
                shim_meta.permissions().mode() & 0o111 != 0,
                "claude shim should be executable"
            );
            let attention_meta = fs::metadata(&files.attention_script_path).unwrap();
            assert!(
                attention_meta.permissions().mode() & 0o111 != 0,
                "attention hook should be executable"
            );
        }

        // Script content should reference env var for status file
        let script = fs::read_to_string(&files.script_path).unwrap();
        assert!(script.contains("session-abc"));
        assert!(script.contains("VIMEFLOW_STATUS_FILE"));

        let zsh_env = fs::read_to_string(&files.zsh_env_path).unwrap();
        assert!(zsh_env.contains("VIMEFLOW_AGENT_INIT"));
        let init = fs::read_to_string(&files.shell_init_path).unwrap();
        assert!(init.contains("VIMEFLOW_CLAUDE_SHIM_DIR"));

        // Settings should exist and contain statusLine config
        assert!(files.settings_path.exists());
        let settings = fs::read_to_string(&files.settings_path).unwrap();
        assert!(settings.contains("statusLine"));
        assert!(settings.contains("statusline.sh"));
        assert!(settings.contains("refreshInterval"));
        assert!(settings.contains("PermissionRequest"));
        assert!(settings.contains("AskUserQuestion"));
        assert!(settings.contains("UserPromptSubmit"));
        assert!(settings.contains("\"Stop\""));
        assert!(settings.contains("StopFailure"));
        assert!(settings.contains("attention.sh"));
        let settings_json: serde_json::Value =
            serde_json::from_str(&settings).expect("settings JSON");
        let prompt_command = settings_json["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
            .as_str()
            .expect("prompt signal hook command");
        assert!(prompt_command.contains("{\"hook_event_name\":\"UserPromptSubmit\"}"));
        let stop_command = settings_json["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .expect("stop hook command");
        assert!(stop_command.contains("attention.sh"));
    }

    #[cfg(unix)]
    #[test]
    fn attention_hook_appends_one_json_line() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let files = generate_bridge_files(
            tmp.path().to_str().unwrap(),
            "session-hooks",
            None,
            &[],
            true,
        )
        .expect("generate bridge files");
        let mut child = Command::new(&files.attention_script_path)
            .env("VIMEFLOW_ATTENTION_FILE", &files.attention_file_path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn attention hook");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(br#"{"hook_event_name":"PermissionRequest"}"#)
            .expect("write hook payload");
        assert!(child.wait().expect("wait hook").success());
        assert_eq!(
            fs::read_to_string(&files.attention_file_path).expect("attention file"),
            "{\"hook_event_name\":\"PermissionRequest\"}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn attention_hook_bounds_oversized_stop_payload() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let files = generate_bridge_files(
            tmp.path().to_str().unwrap(),
            "session-hooks",
            None,
            &[],
            true,
        )
        .expect("generate bridge files");
        let mut child = Command::new(&files.attention_script_path)
            .env("VIMEFLOW_ATTENTION_FILE", &files.attention_file_path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn attention hook");
        let payload = format!(
            r#"{{"hook_event_name":"Stop","transcript_path":"/Users/me/.claude/projects/s/transcript.jsonl","last_assistant_message":"{}"}}"#,
            "x".repeat(260 * 1024)
        );
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(payload.as_bytes())
            .expect("write hook payload");
        assert!(child.wait().expect("wait hook").success());

        let written = fs::read_to_string(&files.attention_file_path).expect("attention file");
        assert!(written.len() < 4096);
        let record: serde_json::Value = serde_json::from_str(written.trim()).expect("record JSON");
        assert_eq!(record["hook_event_name"], "Stop");
        assert_eq!(
            record["transcript_path"],
            "/Users/me/.claude/projects/s/transcript.jsonl"
        );
        assert_eq!(record["vimeflow_truncated"], true);
    }

    #[test]
    fn creates_nested_directories() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let dir = tmp.path().join("deep").join("nested").join("session-xyz");

        let result = generate_bridge_files(dir.to_str().unwrap(), "session-xyz", None, &[], true);
        assert!(result.is_ok());
        assert!(dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn statusline_script_writes_status_file_with_quoted_path_chars() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let dir = tmp.path().join("space dir").join("quote's session");

        let files = generate_bridge_files(dir.to_str().unwrap(), "session-quoted", None, &[], true)
            .expect("generate bridge files");
        let status_json = r#"{"session_id":"session-quoted","transcript_path":null}"#;
        let mut child = Command::new(&files.script_path)
            .env("VIMEFLOW_STATUS_FILE", &files.status_file_path)
            .stdin(Stdio::piped())
            .spawn();

        // Retry on ETXTBSY — some kernels hold the inode busy briefly
        // after fs::write closes the handle.
        for _ in 0..10 {
            match child {
                Ok(c) => {
                    child = Ok(c);
                    break;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    child = Command::new(&files.script_path)
                        .env("VIMEFLOW_STATUS_FILE", &files.status_file_path)
                        .stdin(Stdio::piped())
                        .spawn();
                }
                Err(_) => break,
            }
        }
        let mut child = child.expect("spawn statusline script");

        child
            .stdin
            .as_mut()
            .expect("statusline stdin")
            .write_all(status_json.as_bytes())
            .expect("write status payload");

        let status = child.wait().expect("wait for statusline script");
        assert!(status.success(), "statusline script should exit cleanly");

        let written = fs::read_to_string(&files.status_file_path)
            .expect("statusline script should write status file");
        assert_eq!(written, status_json);
    }

    #[cfg(unix)]
    #[test]
    fn shell_init_shims_claude_with_settings_path_chars() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let dir = tmp.path().join("space dir").join("quote's session");
        let fake_bin = tmp.path().join("bin");
        let calls_path = tmp.path().join("claude-calls.txt");
        fs::create_dir_all(&fake_bin).expect("create fake bin");

        let fake_claude = fake_bin.join("claude");
        fs::write(
            &fake_claude,
            format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$@\" >> \"{}\"\n",
                calls_path.display()
            ),
        )
        .expect("write fake claude");
        fs::set_permissions(&fake_claude, fs::Permissions::from_mode(0o755))
            .expect("chmod fake claude");

        let files = generate_bridge_files(dir.to_str().unwrap(), "session-quoted", None, &[], true)
            .expect("generate bridge files");
        let path = format!(
            "{}:{}",
            fake_bin.to_string_lossy(),
            std::env::var("PATH").unwrap_or_default()
        );
        let status = Command::new("bash")
            .arg("-c")
            .arg("source \"$BASH_ENV\" && claude --model sonnet --print ok")
            .env("PATH", path)
            .env("BASH_ENV", &files.shell_init_path)
            .env("VIMEFLOW_CLAUDE_SHIM_DIR", &files.shim_dir_path)
            .env("VIMEFLOW_CLAUDE_SETTINGS", &files.settings_path)
            .status()
            .expect("run shell init");

        assert!(status.success(), "shell init should execute cleanly");

        let calls = fs::read_to_string(&calls_path).expect("fake claude should be invoked");
        let expected_lines = format!(
            "--settings\n{}\n--model\nsonnet\n--print\nok",
            files.settings_path.display()
        );
        assert_eq!(calls.trim(), expected_lines);
    }

    #[cfg(unix)]
    #[test]
    fn zsh_startup_shims_claude_with_settings_path_chars() {
        if Command::new("zsh").arg("--version").status().is_err() {
            return;
        }

        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let home = tmp.path().join("home without zsh files");
        let dir = tmp.path().join("space dir").join("quote's session");
        let fake_bin = tmp.path().join("bin");
        let calls_path = tmp.path().join("claude-calls.txt");
        fs::create_dir_all(&home).expect("create fake home");
        fs::create_dir_all(&fake_bin).expect("create fake bin");

        let fake_claude = fake_bin.join("claude");
        fs::write(
            &fake_claude,
            format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$@\" >> \"{}\"\n",
                calls_path.display()
            ),
        )
        .expect("write fake claude");
        fs::set_permissions(&fake_claude, fs::Permissions::from_mode(0o755))
            .expect("chmod fake claude");

        let files = generate_bridge_files(dir.to_str().unwrap(), "session-zsh", None, &[], true)
            .expect("generate bridge files");
        let path = format!(
            "{}:{}",
            fake_bin.to_string_lossy(),
            std::env::var("PATH").unwrap_or_default()
        );
        let status = Command::new("zsh")
            .arg("-c")
            .arg("claude --model sonnet --print ok")
            .env("HOME", &home)
            .env("PATH", path)
            .env("ZDOTDIR", &dir)
            .env("VIMEFLOW_AGENT_INIT", &files.shell_init_path)
            .env("VIMEFLOW_CLAUDE_SHIM_DIR", &files.shim_dir_path)
            .env("VIMEFLOW_CLAUDE_SETTINGS", &files.settings_path)
            .status()
            .expect("run zsh wrapper");

        assert!(status.success(), "zsh wrapper should execute cleanly");

        let calls = fs::read_to_string(&calls_path).expect("fake claude should be invoked");
        let expected_lines = format!(
            "--settings\n{}\n--model\nsonnet\n--print\nok",
            files.settings_path.display()
        );
        assert_eq!(calls.trim(), expected_lines);
    }

    #[cfg(unix)]
    #[test]
    fn zsh_alias_using_env_still_hits_claude_shim() {
        if Command::new("zsh").arg("--version").status().is_err() {
            return;
        }

        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let home = tmp.path().join("home with cc alias");
        let dir = tmp.path().join("space dir").join("quote's session");
        let fake_bin = tmp.path().join("bin");
        let calls_path = tmp.path().join("claude-calls.txt");
        fs::create_dir_all(&home).expect("create fake home");
        fs::create_dir_all(&fake_bin).expect("create fake bin");
        fs::write(
            home.join(".zshrc"),
            "alias cc='env CLAUDE_CODE_NO_FLICKER=1 claude --dangerously-skip-permissions'\n",
        )
        .expect("write fake zshrc");

        let fake_claude = fake_bin.join("claude");
        fs::write(
            &fake_claude,
            format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$@\" >> \"{}\"\n",
                calls_path.display()
            ),
        )
        .expect("write fake claude");
        fs::set_permissions(&fake_claude, fs::Permissions::from_mode(0o755))
            .expect("chmod fake claude");

        let files =
            generate_bridge_files(dir.to_str().unwrap(), "session-zsh-alias", None, &[], true)
                .expect("generate bridge files");
        let path = format!(
            "{}:{}",
            fake_bin.to_string_lossy(),
            std::env::var("PATH").unwrap_or_default()
        );
        let status = Command::new("zsh")
            .arg("-ic")
            .arg("cc --model sonnet")
            .env("HOME", &home)
            .env("PATH", path)
            .env("ZDOTDIR", &dir)
            .env("VIMEFLOW_AGENT_INIT", &files.shell_init_path)
            .env("VIMEFLOW_CLAUDE_SHIM_DIR", &files.shim_dir_path)
            .env("VIMEFLOW_CLAUDE_SETTINGS", &files.settings_path)
            .status()
            .expect("run zsh alias");

        assert!(status.success(), "zsh alias should execute cleanly");

        let calls = fs::read_to_string(&calls_path).expect("fake claude should be invoked");
        let expected_lines = format!(
            "--settings\n{}\n--dangerously-skip-permissions\n--model\nsonnet",
            files.settings_path.display()
        );
        assert_eq!(calls.trim(), expected_lines);
    }

    #[test]
    fn generate_bridge_files_injects_aliases_when_shim_enabled() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let dir = tmp.path().join("session-alias-on");

        let aliases = vec![
            AgentAlias {
                id: "a1".into(),
                alias: "c".into(),
                agent: "claude".into(),
                extra: "--continue".into(),
                account: None,
            },
            AgentAlias {
                id: "a2".into(),
                alias: "cx".into(),
                agent: "codex".into(),
                extra: "--dangerously-skip-permissions".into(),
                account: None,
            },
        ];

        let files = generate_bridge_files(
            dir.to_str().unwrap(),
            "session-alias-on",
            None,
            &aliases,
            true,
        )
        .expect("generate bridge files");
        let init = fs::read_to_string(&files.shell_init_path).unwrap();
        assert!(init.contains("alias c='claude --continue'"), "init: {init}");
        assert!(
            init.contains("alias cx='codex --dangerously-skip-permissions'"),
            "init: {init}"
        );
    }

    #[test]
    fn generate_bridge_files_omits_aliases_when_shim_disabled() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let dir = tmp.path().join("session-alias-off");

        let aliases = vec![AgentAlias {
            id: "a1".into(),
            alias: "c".into(),
            agent: "claude".into(),
            extra: "--continue".into(),
            account: None,
        }];

        let files = generate_bridge_files(
            dir.to_str().unwrap(),
            "session-alias-off",
            None,
            &aliases,
            false,
        )
        .expect("generate bridge files");
        let init = fs::read_to_string(&files.shell_init_path).unwrap();
        assert!(
            !init.contains("alias c='claude --continue'"),
            "init: {init}"
        );
    }

    #[test]
    fn cleanup_removes_directory() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let dir = tmp.path().join("session-cleanup");

        generate_bridge_files(dir.to_str().unwrap(), "session-cleanup", None, &[], true).unwrap();
        assert!(dir.exists());

        cleanup_bridge_files(dir.to_str().unwrap(), None).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn cleanup_removes_shim_directory() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let dir = tmp.path().join("session-cleanup");
        let shim = tmp.path().join("shims").join("session-cleanup");

        generate_bridge_files(
            dir.to_str().unwrap(),
            "session-cleanup",
            Some(shim.to_str().unwrap()),
            &[],
            true,
        )
        .unwrap();
        assert!(dir.exists());
        assert!(shim.exists());

        cleanup_bridge_files(dir.to_str().unwrap(), Some(shim.to_str().unwrap())).unwrap();
        assert!(!dir.exists());
        assert!(!shim.exists());
    }

    #[test]
    fn cleanup_handles_missing_directory() {
        let result = cleanup_bridge_files("/tmp/nonexistent-vimeflow-dir-12345", None);
        assert!(
            result.is_ok(),
            "cleanup should succeed for missing directory"
        );
    }
}
