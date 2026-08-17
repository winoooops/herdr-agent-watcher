//! Generates the Claude Code settings overlay Agent Watcher needs (§2, §3).
//! Nothing here touches `src/agent/**`; the frozen path helpers are used read-only.

use std::path::{Path, PathBuf};

/// §3.1: the root comes from the flag or the environment, and **never from a
/// guess**. The daemon's own fallback is `std::env::temp_dir()`
/// (`daemon/run.rs:21`), which is not the XDG path `verify-sidebar-state.sh` uses
/// to find the socket — two different paths for two different purposes. A
/// launcher inventing either one is wrong whenever the daemon used the other, and
/// both mistakes fail the same silent way: a bridge nobody reads.
pub(crate) fn resolve_state_dir(explicit: Option<&Path>) -> Result<PathBuf, String> {
    // Absolute, always. Every destination in the generated scripts is baked from
    // this root, and Claude Code runs them from ITS working directory — a
    // relative root would silently resolve somewhere else entirely.
    fn absolute(path: PathBuf) -> Result<PathBuf, String> {
        if path.is_absolute() {
            return Ok(path);
        }
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .map_err(|error| format!("cannot absolutise {}: {error}", path.display()))
    }
    if let Some(path) = explicit {
        return absolute(path.to_path_buf());
    }
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .map(absolute)
        .transpose()?
        .ok_or_else(|| {
            "cannot resolve the plugin state directory: pass --state-dir, or run where \
             HERDR_PLUGIN_STATE_DIR is set (herdr sets it for plugin processes)"
                .to_string()
        })
}

/// A pane id becomes a path component, so anything that could climb out of the
/// sessions directory is rejected rather than sanitised into something plausible.
pub(crate) fn validate_pane_id(pane_id: &str) -> Result<(), String> {
    let shape_ok = !pane_id.is_empty()
        && pane_id.split(':').count() == 2
        && pane_id.split(':').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
    if shape_ok {
        Ok(())
    } else {
        Err(format!(
            "invalid pane id {pane_id:?}: expected <workspace>:<pane>, e.g. w2:p1"
        ))
    }
}

/// §3.1: canonicalised before hashing, on both sides. Without this a path, the
/// same path with a trailing slash, and a symlink to it hash three ways while
/// naming one directory.
pub(crate) fn canonical_cwd(cwd: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(cwd)
        .map_err(|error| format!("cannot canonicalise {}: {error}", cwd.display()))
}

/// The bridge directory for a pane. Delegates to the frozen helper so the daemon
/// and the launcher cannot drift: its third parameter is named `session_id` but
/// its production caller passes a pane id (§2.3), which is why the layout is
/// `…/workspaces/<basename>-<hash>/sessions/<pane_id>/`.
pub(crate) fn bridge_dir(state_dir: &Path, cwd: &Path, pane_id: &str) -> PathBuf {
    crate::agent::adapter::claude_code::bridge::session_bridge_dir(state_dir, cwd, pane_id)
}

/// Everything a caller needs after generation. Paths, not handles: the scripts
/// are executed by Claude Code, not by us.
pub(crate) struct Generated {
    pub settings_path: PathBuf,
    pub statusline_path: PathBuf,
    pub attention_path: PathBuf,
    pub status_path: PathBuf,
    pub attention_file: PathBuf,
}

/// §2.2: single-quote for the shell. The frozen tree has an equivalent but it is
/// private (`fn shell_quote_path`, `bridge.rs:87`) and widening it would edit
/// `src/agent/**`.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// §2.1: written to a temp file and renamed into place. An in-place rewrite can be
/// read half-written by whatever is executing or parsing it at that moment, and
/// every file here is rewritten on every launch.
pub(crate) fn write_atomic(path: &Path, contents: &str, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path
        .parent()
        .ok_or_else(|| format!("no parent for {}", path.display()))?;
    // Unique per WRITER, not per process: `std::process::id()` alone is identical
    // across threads, so four concurrent generators in one process would share a
    // temp file and rename each other's partial writes. A monotonic counter makes
    // it unique within the process; the pid makes it unique across processes.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("bridge"),
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    std::fs::write(&tmp, contents).map_err(|error| format!("write {}: {error}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("chmod {}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|error| format!("rename to {}: {error}", path.display()))
}

const SOCKET_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);
const ATTENTION_MAX_BYTES: usize = 192 * 1024;

fn session_of(payload: &str) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(payload).map_err(|error| format!("payload is not JSON: {error}"))?;
    parsed
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "payload carries no session_id".to_string())
}

fn connect_bounded(socket: &Path) -> Result<std::os::unix::net::UnixStream, String> {
    let socket = socket.to_path_buf();
    let shown = socket.display().to_string();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sender.send(std::os::unix::net::UnixStream::connect(socket));
    });
    receiver
        .recv_timeout(SOCKET_TIMEOUT)
        .map_err(|_| format!("daemon connection timed out at {shown}"))?
        .map_err(|error| format!("daemon unreachable at {shown}: {error}"))
}

fn resolve_status_path(socket: &Path, pane_id: &str, session: &str) -> Result<PathBuf, String> {
    use std::io::{BufRead, BufReader, Write};
    let mut stream = connect_bounded(socket)?;
    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(SOCKET_TIMEOUT)))
        .map_err(|error| format!("cannot bound the socket: {error}"))?;
    let request = serde_json::json!({
        "method": "status-path", "pane_id": pane_id, "session_id": session,
    });
    writeln!(stream, "{request}").map_err(|error| format!("request failed: {error}"))?;
    let reader = stream
        .try_clone()
        .map_err(|error| format!("cannot read the reply: {error}"))?;
    let mut line = String::new();
    BufReader::new(reader)
        .read_line(&mut line)
        .map_err(|error| format!("no reply within the bound: {error}"))?;
    let reply: serde_json::Value =
        serde_json::from_str(&line).map_err(|error| format!("reply is not JSON: {error}"))?;
    reply
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| format!("no bound path for {pane_id} / {session}"))
}

pub(crate) fn write_status(socket: &Path, pane_id: &str, payload: &str) -> Result<(), String> {
    validate_pane_id(pane_id)?;
    let session = session_of(payload)?;
    let target = resolve_status_path(socket, pane_id, &session)?;
    write_atomic(&target, payload, 0o644)
}

pub(crate) fn write_attention(
    socket: &Path,
    pane_id: &str,
    event: &str,
    mode: &str,
    payload: &str,
) -> Result<(), String> {
    validate_pane_id(pane_id)?;
    let session = session_of(payload)?;
    let status = resolve_status_path(socket, pane_id, &session)?;
    let stream = crate::agent::adapter::claude_code::bridge::session_attention_file(&status);
    append_attention(&stream, event, mode, payload)
}

pub(crate) fn append_attention(
    stream_path: &Path,
    event: &str,
    mode: &str,
    payload: &str,
) -> Result<(), String> {
    use std::io::Write;
    let record = match mode {
        "name-only" => serde_json::json!({ "hook_event_name": event }).to_string(),
        "append" if payload.len() <= ATTENTION_MAX_BYTES => payload.trim_end().to_string(),
        "append" => {
            let transcript = serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .and_then(|value| {
                    value
                        .get("transcript_path")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                });
            match transcript {
                Some(path) => serde_json::json!({
                    "hook_event_name": event,
                    "transcript_path": path,
                    "vimeflow_truncated": true,
                }),
                None => serde_json::json!({
                    "hook_event_name": event,
                    "vimeflow_truncated": true,
                }),
            }
            .to_string()
        }
        _ => return Err(format!("unknown attention mode {mode:?}")),
    };
    if let Some(parent) = stream_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(stream_path)
        .map_err(|error| format!("open {}: {error}", stream_path.display()))?;
    writeln!(file, "{record}").map_err(|error| format!("append: {error}"))
}

/// The generated scripts: the same atomic replace, plus the executable bit Claude
/// Code needs to run them.
fn write_executable(path: &Path, contents: &str) -> Result<(), String> {
    write_atomic(path, contents, 0o755)
}

/// Generates the overlay for one pane. Idempotent: safe to call on every launch.
pub(crate) fn generate(
    state_dir: &Path,
    cwd: &Path,
    pane_id: &str,
    user_statusline: Option<&str>,
) -> Result<Generated, String> {
    validate_pane_id(pane_id)?;
    let cwd = canonical_cwd(cwd)?;
    let dir = bridge_dir(state_dir, &cwd, pane_id);
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;

    let status_path =
        crate::agent::adapter::claude_code::bridge::session_status_file(state_dir, &cwd, pane_id);
    let attention_file =
        crate::agent::adapter::claude_code::bridge::session_attention_file(&status_path);
    let statusline_path = dir.join("statusline.sh");
    let attention_path = dir.join("attention.sh");
    let settings_path = dir.join("settings.json");

    // §2.2: the payload is captured once, written, then replayed into the user's
    // own status line through `sh -c` — `statusLine.command` is a shell
    // expression (`jq -r '…'`, pipelines), not an executable name.
    let chain = match user_statusline {
        Some(command) if !command.trim().is_empty() => {
            format!(
                "printf '%s' \"$payload\" | sh -c {}\n",
                shell_quote(command)
            )
        }
        _ => String::new(),
    };
    write_executable(
        &statusline_path,
        &format!(
            "#!/usr/bin/env bash\n\
             # Agent Watcher bridge for pane {pane_id} — destination baked in (§2.2).\n\
             payload=$(cat)\n\
             printf '%s' \"$payload\" > {status}\n\
             {chain}",
            pane_id = pane_id,
            status = shell_quote(&status_path.to_string_lossy()),
            chain = chain,
        ),
    )?;

    // §2.2: two modes. `append` writes the payload verbatim; `name-only`
    // synthesises a record carrying just the event name, because Claude Code's
    // UserPromptSubmit payload contains the user's full prompt text.
    write_executable(
        &attention_path,
        &format!(
            "#!/usr/bin/env bash\n\
             # $1 = hook event name, $2 = append|name-only\n\
             event=\"$1\"; mode=\"$2\"\n\
             if [ \"$mode\" = 'name-only' ]; then\n\
             \x20 cat > /dev/null\n\
             \x20 printf '{{\"hook_event_name\":\"%s\"}}\\n' \"$event\" >> {stream}\n\
             \x20 exit 0\n\
             fi\n\
             payload=$(cat)\n\
             if [ \"$(printf '%s' \"$payload\" | wc -c | tr -d ' ')\" -le 196608 ]; then\n\
             \x20 printf '%s\\n' \"$payload\" >> {stream}\n\
             \x20 exit 0\n\
             fi\n\
             # Oversized: keep transcript_path, as the original does. The watcher\n\
             # derives notification bodies from the transcript, so a record without\n\
             # it is a completion event nothing can describe.\n\
             transcript=$(printf '%s' \"$payload\" | sed -n 's/.*\"transcript_path\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p' | head -n 1)\n\
             escaped=$(printf '%s' \"$transcript\" | sed 's/\\\\/\\\\\\\\/g; s/\"/\\\\\"/g')\n\
             if [ -n \"$escaped\" ]; then\n\
             \x20 printf '{{\"hook_event_name\":\"%s\",\"transcript_path\":\"%s\",\"vimeflow_truncated\":true}}\\n' \"$event\" \"$escaped\" >> {stream}\n\
             else\n\
             \x20 printf '{{\"hook_event_name\":\"%s\",\"vimeflow_truncated\":true}}\\n' \"$event\" >> {stream}\n\
             fi\n",
            stream = shell_quote(&attention_file.to_string_lossy()),
        ),
    )?;

    // §4.1: create only when absent. The watcher holds a byte cursor into this
    // file, and generation now runs on every launch.
    // `exists()` then `create()` is a race: a concurrent launch can create the file
    // between the two, and `File::create` truncates. `create_new` is one atomic
    // operation — AlreadyExists is the success case here.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&attention_file)
    {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create {}: {error}", attention_file.display())),
    }

    let hook = |event: &str, mode: &str| {
        serde_json::json!({"hooks": [{
            "type": "command",
            "command": format!("{} {} {}",
                shell_quote(&attention_path.to_string_lossy()),
                shell_quote(event), shell_quote(mode)),
        }]})
    };
    let settings = serde_json::json!({
        "statusLine": {
            // Claude runs this as a SHELL EXPRESSION, not an exec of a filename, so
            // an unquoted path with a space or a quote breaks it. This is the same
            // boundary `shell_quote` exists for everywhere else.
            "type": "command",
            "command": shell_quote(&statusline_path.to_string_lossy()),
            "refreshInterval": 5
        },
        "hooks": {
            // name-only: the payload carries the user's prompt (§2.2).
            "UserPromptSubmit": [hook("UserPromptSubmit", "name-only")],
            "Stop": [hook("Stop", "append")],
            "StopFailure": [hook("StopFailure", "append")],
            "PermissionRequest": [hook("PermissionRequest", "append")],
            "PreToolUse": [{
                "matcher": "AskUserQuestion",
                "hooks": hook("PreToolUse", "append")["hooks"].clone(),
            }],
        }
    });
    // The same atomic replace the scripts get. Claude Code reads this at launch, so
    // a concurrent regeneration that exposed a truncated file would launch it
    // unbridged — the exact symptom this milestone exists to remove.
    write_atomic(
        &settings_path,
        &serde_json::to_string_pretty(&settings)
            .map_err(|error| format!("serialize settings: {error}"))?,
        0o644,
    )?;

    Ok(Generated {
        settings_path,
        statusline_path,
        attention_path,
        status_path,
        attention_file,
    })
}

/// The status line Claude Code would use, so the bridge can chain it instead of
/// silently replacing it (§2.2). Later entries win.
pub(crate) fn effective_status_line(cwd: &Path, verbose: bool) -> Option<String> {
    // SCOPE, stated because getting this wrong loses the user's status line
    // silently: only the *file-based user and project* tiers are discovered.
    // `CLAUDE_CONFIG_DIR` relocates the user tier, so it is honoured. Managed
    // settings live in platform system directories and may be server/MDM
    // supplied — they are NOT discoverable here, so when one exists this reader
    // returns whatever it can see and the bridge may replace a status line it
    // never knew about. That is the §2.2 limitation, bounded rather than guessed.
    let user_root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))?;
    let candidates = [
        user_root.join("settings.json"),
        cwd.join(".claude/settings.json"),
        cwd.join(".claude/settings.local.json"),
    ];
    let read = |path: &Path| -> Option<String> {
        let text = std::fs::read_to_string(path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        value["statusLine"]["command"].as_str().map(str::to_string)
    };
    let mut found = None;
    for path in &candidates {
        if let Some(command) = read(path) {
            found = Some(command); // later tiers win
        }
    }
    // §2.2: a MANAGED statusLine is not overridable by `--settings` at all, so the
    // bridge's own line never runs and metrics stay absent. That is the machine
    // owner's policy, not something to work around — but it must be said, or the
    // symptom is indistinguishable from the bug this milestone removes.
    if verbose {
        match &found {
            Some(command) => {
                eprintln!("herdr-agent-watcher: chaining existing statusLine: {command}")
            }
            None => eprintln!(
                "herdr-agent-watcher: no file-based statusLine found (managed settings are not \
                 discoverable; if one is set, --settings cannot override it and metrics \
                 will not be reported)"
            ),
        }
    }
    found
}

/// `herdr-agent-watcher claude-bridge` — prints the overlay path on stdout and nothing
/// else, so it is safe in command substitution.
pub fn cli_claude_bridge(args: &[String]) -> i32 {
    let get = |flag: &str| -> Option<String> {
        args.iter()
            .position(|argument| argument == flag)
            .and_then(|index| args.get(index + 1))
            .cloned()
    };
    if args.iter().any(|argument| argument == "--write")
        || args.iter().any(|argument| argument == "--write-attention")
    {
        let socket = match get("--socket") {
            Some(path) => PathBuf::from(path),
            None => {
                eprintln!("--write requires --socket");
                return 1;
            }
        };
        let pane = match get("--pane").or_else(|| std::env::var("HERDR_PANE_ID").ok()) {
            Some(pane) => pane,
            None => {
                eprintln!("no pane: pass --pane, or run inside a herdr pane");
                return 1;
            }
        };
        let mut payload = String::new();
        if let Err(error) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut payload) {
            eprintln!("cannot read the payload: {error}");
            return 1;
        }
        let outcome = if args.iter().any(|argument| argument == "--write-attention") {
            write_attention(
                &socket,
                &pane,
                &get("--event").unwrap_or_else(|| "Unknown".to_string()),
                &get("--mode").unwrap_or_else(|| "append".to_string()),
                &payload,
            )
        } else {
            write_status(&socket, &pane, &payload)
        };
        return match outcome {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error}");
                1
            }
        };
    }
    let verbose = args.iter().any(|argument| argument == "--verbose");
    let state = match resolve_state_dir(get("--state-dir").as_deref().map(Path::new)) {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let pane = match get("--pane").or_else(|| std::env::var("HERDR_PANE_ID").ok()) {
        Some(pane) => pane,
        None => {
            eprintln!("no pane: pass --pane, or run inside a herdr pane");
            return 1;
        }
    };
    // §3.1: --pane means THAT pane's cwd, resolved through herdr with the
    // daemon's exact precedence. Defaulting to $PWD would key the bridge to the
    // caller whenever the flag route is used from a controlling pane.
    let cwd = match get("--cwd") {
        Some(cwd) => PathBuf::from(cwd),
        None if get("--pane").is_some() => {
            let client = crate::herdr::client::HerdrClient::from_env();
            match client.pane_list().ok().and_then(|panes| {
                panes
                    .into_iter()
                    .find(|info| info.pane_id == pane)
                    .and_then(|info| info.foreground_cwd.clone().or(info.cwd.clone()))
            }) {
                Some(cwd) => PathBuf::from(cwd),
                None => {
                    eprintln!("herdr did not report a cwd for {pane}");
                    return 1;
                }
            }
        }
        None => std::env::current_dir().unwrap_or_default(),
    };
    let user_line = effective_status_line(&cwd, verbose);
    match generate(&state, &cwd, &pane, user_line.as_deref()) {
        Ok(generated) => {
            if verbose {
                eprintln!(
                    "state={} pane={pane} cwd={}",
                    state.display(),
                    cwd.display()
                );
            }
            println!("{}", generated.settings_path.display());
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

pub fn cli_generate_scripts(args: &[String]) -> i32 {
    let get = |flag: &str| -> Option<String> {
        args.iter()
            .position(|argument| argument == flag)
            .and_then(|index| args.get(index + 1))
            .cloned()
    };
    let (Some(bin), Some(aw), Some(socket)) = (
        get("--bin-dir"),
        get("--herdr-agent-watcher"),
        get("--socket"),
    ) else {
        eprintln!(
            "usage: generate-scripts --bin-dir D --herdr-agent-watcher P --socket S [--downstream C]"
        );
        return 2;
    };
    match crate::agents::bridge_scripts::generate(
        Path::new(&bin),
        Path::new(&aw),
        Path::new(&socket),
        get("--downstream").as_deref(),
    ) {
        Ok(_) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn bridge_paths() -> Result<(PathBuf, PathBuf, PathBuf, PathBuf), String> {
    let state = resolve_state_dir(None)?;
    let bin = state.join("bin");
    Ok((
        bin.join("statusline.sh"),
        bin.join("attention.sh"),
        state.join("bridge-install.json"),
        bin,
    ))
}

/// A `claude` interceptor left in our own `bin/` by the removed PATH shim.
///
/// It is not inert. It passes `--settings`, which outranks the user tier this
/// bridge installs into, so every Claude launched with that directory on `PATH`
/// silently keeps using the old per-pane overlay and the new bridge never
/// applies. Found by live verification, not by any test: the tests never had a
/// previous installation to leave residue behind.
///
/// Only `bin/claude` is removed. `bin/herdr-agent-watcher` is a launcher for this
/// binary and harmless, and the operator's own `PATH` line is theirs to delete —
/// `enable` says so rather than editing their shell profile.
#[cfg(test)]
pub(crate) fn remove_stale_shim_for_test(bin: &Path) {
    remove_stale_shim(bin);
}

fn remove_stale_shim(bin: &Path) {
    let shim = bin.join("claude");
    let is_ours = std::fs::read_to_string(&shim)
        .map(|body| body.contains("herdr-agent-watcher shim"))
        .unwrap_or(false);
    if !is_ours {
        return;
    }
    match std::fs::remove_file(&shim) {
        Ok(()) => eprintln!(
            "removed the superseded PATH shim at {}.\n\
             If your shell profile still puts {} on PATH, that line can go too.",
            shim.display(),
            bin.display()
        ),
        Err(error) => eprintln!(
            "could not remove the superseded shim at {}: {error}\n\
             Delete it by hand: while it is on PATH it outranks this bridge.",
            shim.display()
        ),
    }
}

pub fn cli_enable(_args: &[String]) -> i32 {
    let settings = match crate::agents::bridge_settings::user_settings_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let (statusline, attention, sidecar, bin) = match bridge_paths() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let me = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("cannot resolve own path: {error}");
            return 1;
        }
    };
    let current = std::fs::read_to_string(&settings)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let current_command = current
        .as_ref()
        .and_then(|value| value["statusLine"]["command"].as_str());
    let downstream = if current_command
        .is_some_and(|command| command.contains(&*statusline.to_string_lossy()))
    {
        let record = match std::fs::read_to_string(&sidecar)
            .map_err(|error| {
                format!("bridge is installed but its install record is unavailable: {error}")
            })
            .and_then(|text| {
                serde_json::from_str::<crate::agents::bridge_settings::Sidecar>(&text)
                    .map_err(|error| format!("bridge install record is unreadable: {error}"))
            }) {
            Ok(record) => record,
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        };
        record
            .previous_status_line
            .and_then(|value| value["command"].as_str().map(str::to_string))
    } else {
        current_command.map(str::to_string)
    };
    if let Err(error) = crate::agents::bridge_scripts::generate(
        &bin,
        &me,
        &crate::daemon::state_socket_path(),
        downstream.as_deref(),
    ) {
        eprintln!("{error}");
        return 1;
    }
    remove_stale_shim(&bin);
    match crate::agents::bridge_settings::enable(
        &settings,
        &sidecar,
        &statusline.to_string_lossy(),
        &attention.to_string_lossy(),
    ) {
        Ok(()) => {
            println!("{}", settings.display());
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

pub fn cli_disable(_args: &[String]) -> i32 {
    let settings = match crate::agents::bridge_settings::user_settings_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let (_, _, sidecar, _) = match bridge_paths() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    match crate::agents::bridge_settings::disable(&settings, &sidecar) {
        Ok(()) => {
            println!("{}", settings.display());
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

/// Everything `doctor::run` needs, gathered once. The sidebar's doctor panel
/// is the second reader; a copy of this is how the two would come to show
/// different checks.
pub(crate) fn doctor_report() -> Result<crate::agents::doctor::Report, String> {
    use crate::agents::doctor;
    let settings = crate::agents::bridge_settings::user_settings_path()?;
    let (statusline, attention, _, _) = bridge_paths()?;
    let me = std::env::current_exe().unwrap_or_default();
    let socket = crate::daemon::state_socket_path();
    let client = crate::herdr::client::HerdrClient::from_env();
    let panes: Vec<doctor::PaneInput> = client
        .pane_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|info| matches!(info.agent.as_deref(), Some("claude" | "claude-code")))
        .map(|info| doctor::PaneInput {
            pane_id: info.pane_id.clone(),
            agent_session: info.session_value().map(str::to_string),
            cwd: PathBuf::from(
                info.foreground_cwd
                    .clone()
                    .or(info.cwd.clone())
                    .unwrap_or_default(),
            ),
            argv: process_argv_for_pane(&info.pane_id),
        })
        .collect();
    let config_problems = crate::daemon::config::DaemonConfig::load().problems;
    // The settings this plugin owns, found in the file next door. Twice now
    // that is where they were written first.
    let misplaced = crate::agents::keybinding::herdr_config_path()
        .ok()
        .and_then(|herdr| {
            let text = std::fs::read_to_string(&herdr).ok()?;
            let tables = doctor::misplaced_plugin_tables(&text);
            let ours = crate::sidebar::config::config_path()?;
            (!tables.is_empty()).then_some((tables, herdr, ours))
        });
    Ok(doctor::run(
        &socket,
        &settings,
        &statusline,
        &attention,
        &me,
        &panes,
        &|pane, session| resolve_status_path(&socket, pane, session).ok(),
        &config_problems,
        misplaced,
    ))
}

/// The value after `flag`, if it was passed.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|argument| argument == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

pub fn cli_doctor(args: &[String]) -> i32 {
    // The failure this command reports most often is the missing state
    // directory, and it names `--state-dir` as the fix -- so the flag has to
    // work here, not only in `claude-bridge`. Doctor is the one command you
    // run by hand, outside the plugin process that would have set the
    // variable. Every path it inspects comes from that variable, so setting
    // it is the whole of honouring the flag.
    if let Some(dir) = flag_value(args, "--state-dir") {
        match resolve_state_dir(Some(Path::new(&dir))) {
            Ok(absolute) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", absolute),
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        }
    }
    match doctor_report() {
        Ok(report) => {
            print!("{}", crate::agents::doctor::render(&report));
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn process_argv_for_pane(pane_id: &str) -> Vec<String> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-ww", "-eo", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((pid, command)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        if !command.contains("claude") {
            continue;
        }
        let Ok(environment) = std::process::Command::new("ps")
            .args(["eww", "-p", pid])
            .output()
        else {
            continue;
        };
        if String::from_utf8_lossy(&environment.stdout)
            .contains(&format!("HERDR_PANE_ID={pane_id}"))
        {
            return shell_words::split(command)
                .unwrap_or_else(|_| command.split_whitespace().map(str::to_string).collect());
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::with_env;

    /// Both readers call this; the sidebar's panel is the second. A copy is
    /// how the two come to show different checks.
    #[test]
    fn the_report_is_gathered_in_one_place() {
        let state = tempfile::tempdir().expect("tempdir");
        let claude = tempfile::tempdir().expect("tempdir");
        let config = tempfile::tempdir().expect("tempdir");
        let report = crate::test_env::with_env(
            &[
                ("HERDR_PLUGIN_STATE_DIR", Some(state.path().into())),
                ("CLAUDE_CONFIG_DIR", Some(claude.path().into())),
                ("HERDR_PLUGIN_CONFIG_DIR", Some(config.path().into())),
                (
                    "HERDR_SOCKET_PATH",
                    Some(state.path().join("absent.sock").into()),
                ),
            ],
            doctor_report,
        )
        .expect("the gatherer produced a report");
        assert!(report
            .checks
            .iter()
            .any(|c| c.id == crate::agents::doctor::CheckId::ConfigValid));
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.id == crate::agents::doctor::CheckId::DaemonReachable),
            "with no daemon this is a finding, not an error"
        );
    }

    fn fake_state_socket(dir: &std::path::Path, reply: Option<&str>) -> std::path::PathBuf {
        use std::io::{BufRead, BufReader, Write};
        let socket = dir.join("state.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        let reply = reply.map(str::to_string);
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut line = String::new();
            let cloned = stream.try_clone().expect("clone");
            let _ = BufReader::new(cloned).read_line(&mut line);
            let body = serde_json::json!({"version": 2, "path": reply});
            let _ = writeln!(stream, "{body}");
        });
        socket
    }

    #[test]
    fn the_writer_writes_where_the_daemon_says() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("status.json");
        let socket = fake_state_socket(dir.path(), Some(&target.to_string_lossy()));
        write_status(&socket, "w1:p1", r#"{"session_id":"s1","cost":{}}"#).expect("write");
        assert!(std::fs::read_to_string(&target).unwrap().contains("s1"));
    }

    #[test]
    fn a_daemon_that_reports_no_path_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = fake_state_socket(dir.path(), None);
        assert!(write_status(&socket, "w1:p1", r#"{"session_id":"s1"}"#).is_err());
    }

    #[test]
    fn an_unreachable_daemon_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(write_status(
            &dir.path().join("absent.sock"),
            "w1:p1",
            r#"{"session_id":"s1"}"#
        )
        .is_err());
    }

    #[test]
    fn a_socket_that_never_answers_gives_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("silent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(held);
        });
        let started = std::time::Instant::now();
        assert!(write_status(&socket, "w1:p1", r#"{"session_id":"s1"}"#).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_payload_without_a_session_is_rejected_before_the_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(write_status(&dir.path().join("never.sock"), "w1:p1", r#"{"cost":{}}"#).is_err());
    }

    #[test]
    fn the_attention_stream_is_derived_from_the_daemon_supplied_status_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = dir.path().join("status.json");
        let socket = fake_state_socket(dir.path(), Some(&status.to_string_lossy()));
        write_attention(
            &socket,
            "w1:p1",
            "Stop",
            "append",
            r#"{"session_id":"s1","x":1}"#,
        )
        .expect("write");
        let stream = crate::agent::adapter::claude_code::bridge::session_attention_file(&status);
        assert!(std::fs::read_to_string(stream).unwrap().contains("\"x\":1"));
    }

    #[test]
    fn an_attention_record_appends_without_truncating() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stream = dir.path().join("attention.jsonl");
        std::fs::write(&stream, "{\"hook_event_name\":\"Stop\"}\n").expect("seed");
        append_attention(
            &stream,
            "Stop",
            "append",
            r#"{"hook_event_name":"Stop","x":1}"#,
        )
        .expect("append");
        let body = std::fs::read_to_string(&stream).expect("read");
        assert!(body.contains("\"x\":1"));
        assert_eq!(body.lines().count(), 2);
    }

    #[test]
    fn name_only_mode_never_persists_the_prompt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stream = dir.path().join("attention.jsonl");
        append_attention(
            &stream,
            "UserPromptSubmit",
            "name-only",
            r#"{"hook_event_name":"UserPromptSubmit","prompt":"SECRET-TEXT"}"#,
        )
        .expect("append");
        let body = std::fs::read_to_string(&stream).expect("read");
        assert!(body.contains("UserPromptSubmit"));
        assert!(!body.contains("SECRET-TEXT"));
    }

    #[test]
    fn an_oversized_payload_keeps_only_the_transcript_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stream = dir.path().join("attention.jsonl");
        let payload = format!(
            r#"{{"hook_event_name":"Stop","transcript_path":"/tmp/t.jsonl","pad":"{}"}}"#,
            "x".repeat(200_000)
        );
        append_attention(&stream, "Stop", "append", &payload).expect("append");
        let body = std::fs::read_to_string(&stream).expect("read");
        assert!(body.len() < 1_000);
        assert!(body.contains("/tmp/t.jsonl"));
        assert!(body.contains("\"vimeflow_truncated\":true"));
    }

    fn with_state_dir_env<T>(value: Option<&std::ffi::OsStr>, body: impl FnOnce() -> T) -> T {
        with_env(&[("HERDR_PLUGIN_STATE_DIR", value.map(Into::into))], body)
    }

    #[test]
    fn state_dir_prefers_the_explicit_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let got = with_state_dir_env(Some("/should/not/win".as_ref()), || {
            resolve_state_dir(Some(dir.path())).expect("explicit wins")
        });
        assert_eq!(got, dir.path());
    }

    #[test]
    fn state_dir_errors_rather_than_guessing() {
        let err = with_state_dir_env(None, || {
            resolve_state_dir(None).expect_err("no fallback is allowed")
        });
        assert!(
            err.contains("--state-dir"),
            "the error must name the fix, got {err:?}"
        );
    }

    #[test]
    fn a_relative_state_dir_becomes_absolute() {
        let got = resolve_state_dir(Some(Path::new("relative/state"))).expect("resolves");
        assert!(
            got.is_absolute(),
            "baked paths resolve against Claude's cwd, not ours: {}",
            got.display()
        );
        assert!(got.ends_with("relative/state"));
    }

    #[test]
    fn a_pane_id_that_could_escape_the_directory_is_rejected() {
        assert!(validate_pane_id("w2:p1").is_ok());
        for bad in ["../etc", "w2/p1", "", "..", "a b"] {
            assert!(validate_pane_id(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn cwd_forms_that_name_one_directory_canonicalise_to_one_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("work");
        std::fs::create_dir(&real).expect("mkdir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let a = canonical_cwd(&real).expect("plain");
        let b = canonical_cwd(&real.join("")).expect("trailing slash");
        let c = canonical_cwd(&link).expect("symlink");
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn the_layout_matches_the_frozen_helper_exactly() {
        // If these ever disagree, the launcher writes where the daemon does not
        // read — the whole failure mode this milestone removes.
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let canon = canonical_cwd(&cwd).expect("canonical");
        assert_eq!(
            bridge_dir(dir.path(), &canon, "w2:p1"),
            crate::agent::adapter::claude_code::bridge::session_bridge_dir(
                dir.path(),
                &canon,
                "w2:p1"
            )
        );
        // ...and the documented shape: <root>/runtime/workspaces/<bucket>/sessions/<pane>
        let shown = bridge_dir(dir.path(), &canon, "w2:p1")
            .display()
            .to_string();
        assert!(shown.ends_with("/sessions/w2:p1"), "{shown}");
        assert!(shown.contains("/workspaces/"), "{shown}");
    }

    #[test]
    fn two_cwds_for_one_pane_key_two_bridges() {
        // §5: the launcher and the daemon disagreeing about cwd is a KNOWN
        // hazard, pinned here so it is known rather than discovered.
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert_ne!(
            bridge_dir(dir.path(), &canonical_cwd(&a).unwrap(), "w2:p1"),
            bridge_dir(dir.path(), &canonical_cwd(&b).unwrap(), "w2:p1")
        );
    }

    fn generate_into(dir: &std::path::Path, user_statusline: Option<&str>) -> Generated {
        let cwd = dir.join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        generate(dir, &cwd, "w2:p1", user_statusline).expect("generate")
    }

    #[test]
    fn no_environment_reference_survives_in_the_overlay() {
        let dir = tempfile::tempdir().expect("tempdir");
        let g = generate_into(dir.path(), None);
        let settings = std::fs::read_to_string(&g.settings_path).expect("read settings");
        assert!(
            !settings.contains("$VIMEFLOW_"),
            "a destination is still read from the environment:\n{settings}"
        );
        let statusline = std::fs::read_to_string(&g.statusline_path).expect("read statusline");
        assert!(!statusline.contains("$VIMEFLOW_"));
        assert!(statusline.contains(&g.status_path.display().to_string()));
    }

    #[test]
    fn all_five_hooks_are_present_and_route_through_attention_sh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let g = generate_into(dir.path(), None);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&g.settings_path).unwrap()).unwrap();
        let hooks = v["hooks"].as_object().expect("hooks object");
        for name in [
            "UserPromptSubmit",
            "Stop",
            "StopFailure",
            "PermissionRequest",
            "PreToolUse",
        ] {
            assert!(
                hooks.contains_key(name),
                "{name} missing — it is the one the decoder needs"
            );
        }
        let all = serde_json::to_string(&v).unwrap();
        let script = g.attention_path.display().to_string();
        assert_eq!(
            all.matches(&script).count(),
            5,
            "every hook command must invoke attention.sh, not write inline"
        );
    }

    #[test]
    fn the_prompt_is_never_persisted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let g = generate_into(dir.path(), None);
        let secret = "SECRET-PROMPT-TEXT";
        let payload = format!(r#"{{"hook_event_name":"UserPromptSubmit","prompt":"{secret}"}}"#);
        run_script(
            &g.attention_path,
            &["UserPromptSubmit", "name-only"],
            &payload,
        );
        let written = std::fs::read_to_string(&g.attention_file).expect("read stream");
        assert!(
            written.contains("UserPromptSubmit"),
            "the event name is what the decoder needs"
        );
        assert!(
            !written.contains(secret),
            "the prompt must not reach disk:\n{written}"
        );

        // Stop DOES append verbatim — the two modes differ and both matter.
        run_script(
            &g.attention_path,
            &["Stop", "append"],
            r#"{"hook_event_name":"Stop","x":1}"#,
        );
        let written = std::fs::read_to_string(&g.attention_file).expect("read stream");
        assert!(written.contains(r#""x":1"#));
    }

    #[test]
    fn an_existing_attention_stream_is_never_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let g = generate_into(dir.path(), None);
        std::fs::write(&g.attention_file, "{\"hook_event_name\":\"Stop\"}\n").expect("seed");
        let before = std::fs::read(&g.attention_file).expect("read");
        generate_into(dir.path(), None);
        assert_eq!(
            std::fs::read(&g.attention_file).expect("read"),
            before,
            "regeneration must not discard records the watcher has not read yet"
        );
    }

    #[test]
    fn concurrent_generation_does_not_lose_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let g = generate_into(dir.path(), None);
        std::fs::write(&g.attention_file, "{\"hook_event_name\":\"Stop\"}\n").expect("seed");

        let root = dir.path().to_path_buf();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let cwd = root.join("repo");
                    generate(&root, &cwd, "w2:p1", None).expect("generate")
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread");
        }
        assert_eq!(
            std::fs::read_to_string(&g.attention_file).expect("read"),
            "{\"hook_event_name\":\"Stop\"}\n",
            "a concurrent launch must not truncate the stream"
        );
    }

    #[test]
    fn settings_json_is_replaced_not_truncated_in_place() {
        // A rename gives the path a NEW inode; `fs::write` truncates the existing
        // one, and a reader in that window sees an empty file. Claude Code reads
        // this at launch, so a truncated read launches it unbridged.
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let g = generate_into(dir.path(), None);
        let before = std::fs::metadata(&g.settings_path).expect("stat").ino();
        generate_into(dir.path(), None);
        let after = std::fs::metadata(&g.settings_path).expect("stat").ino();
        assert_ne!(
            before, after,
            "settings.json must be renamed into place, not rewritten"
        );
    }

    #[test]
    fn generated_scripts_are_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let g = generate_into(dir.path(), None);
        for path in [&g.statusline_path, &g.attention_path] {
            let mode = std::fs::metadata(path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "{} is not executable", path.display());
        }
    }

    #[test]
    fn an_existing_status_line_is_chained_not_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let g = generate_into(dir.path(), Some("printf 'USER-LINE'"));
        let out = run_script(&g.statusline_path, &[], r#"{"session_id":"s1"}"#);
        assert_eq!(
            out.trim(),
            "USER-LINE",
            "the user's status line still renders"
        );
        assert!(
            std::fs::read_to_string(&g.status_path)
                .unwrap()
                .contains("s1"),
            "and the payload still reaches status.json"
        );
    }

    #[test]
    fn a_status_line_with_arguments_and_a_pipe_still_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A real one looks like `jq -r '.model.display_name'` — a shell expression,
        // not an executable name.
        let g = generate_into(dir.path(), Some("tr 'a-z' 'A-Z' | tr -d '\\n'"));
        let out = run_script(&g.statusline_path, &[], "quiet");
        assert_eq!(out, "QUIET");
    }

    #[test]
    fn a_path_with_a_quote_and_a_space_survives_the_shell() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The metacharacters must be in the STATE DIR, not the cwd: the frozen
        // helper hashes the cwd basename into a bucket name, so a quote there
        // never reaches a generated path. In the state dir it does.
        let state = dir.path().join("it's a state dir");
        std::fs::create_dir_all(&state).expect("mkdir state");
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        let g = generate(&state, &cwd, "w2:p1", None).expect("generate");

        // Run the command STRING FROM settings.json through a shell — invoking the
        // script directly with Command::new would bypass the quoting boundary this
        // test exists to check.
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&g.settings_path).unwrap()).unwrap();
        let command = v["statusLine"]["command"]
            .as_str()
            .expect("statusLine command");

        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn via shell");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(br#"{"session_id":"quoted"}"#)
            .unwrap();
        assert!(
            child.wait().unwrap().success(),
            "the quoted command must run"
        );
        assert!(std::fs::read_to_string(&g.status_path)
            .unwrap()
            .contains("quoted"));
    }

    /// Runs a generated script with `payload` on stdin, returns stdout.
    fn run_script(path: &std::path::Path, args: &[&str], payload: &str) -> String {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new(path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn generated script");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let out = child.wait_with_output().expect("wait");
        assert!(out.status.success(), "script failed: {out:?}");
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}
