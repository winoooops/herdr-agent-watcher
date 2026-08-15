//! Structured Claude bridge diagnostics shared by the CLI and future TUI.

use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckId {
    DaemonReachable,
    BridgeEnabled,
    ScriptsPresent,
    NothingShadowing,
    HooksEnabled,
    MetricsPresent,
}

#[derive(Debug, Clone)]
pub(crate) enum Remedy {
    PluginAction {
        id: &'static str,
    },
    /// The session must be recreated — its identity is wrong, not its timing.
    /// Reserved for the retired-pane-id case, where nothing the session does
    /// will make the daemon recognise it.
    RestartSession,
    /// Nothing is wrong; the pane simply has not rendered since the bridge was
    /// enabled. Claude re-reads its settings while running, so waiting works.
    WaitOrInteract,
    ReopenPane,
    WriteSettingsBlock {
        path: PathBuf,
        block: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Check {
    pub id: CheckId,
    pub level: Level,
    pub summary: String,
    pub evidence: Option<String>,
    pub remedy: Option<Remedy>,
}

#[derive(Debug, Clone)]
pub(crate) struct PaneFinding {
    pub pane_id: String,
    pub agent_session: Option<String>,
    pub level: Level,
    pub summary: String,
    pub window_size: Option<u64>,
    pub shadowed_by: Option<PathBuf>,
    pub remedy: Option<Remedy>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Report {
    pub checks: Vec<Check>,
    pub panes: Vec<PaneFinding>,
}

pub(crate) struct PaneInput {
    pub pane_id: String,
    pub agent_session: Option<String>,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The first PROJECT-tier `statusLine` above `cwd`, if any.
///
/// The walk stops at `user_settings`. Ancestors of a pane's cwd reach `$HOME`,
/// and `~/.claude/settings.json` is the tier the bridge installs itself into —
/// without this bound every pane reports itself shadowed by the bridge, and the
/// emitted remedy chains `statusline.sh` to itself. Tests over a `tempdir`
/// cannot see this: they never walk as far as the real home directory.
pub(crate) fn shadowing_status_line(cwd: &Path, user_settings: &Path) -> Option<(PathBuf, String)> {
    let user_tier = std::fs::canonicalize(user_settings).unwrap_or_else(|_| user_settings.into());
    for dir in cwd.ancestors() {
        for name in ["settings.local.json", "settings.json"] {
            let candidate = dir.join(".claude").join(name);
            let resolved = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
            if resolved == user_tier {
                // Reached the tier the bridge owns: nothing below it shadowed us.
                return None;
            }
            let Ok(text) = std::fs::read_to_string(&candidate) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(command) = value["statusLine"]["command"].as_str() {
                return Some((candidate, command.to_string()));
            }
        }
    }
    None
}

fn option_value<'a>(argv: &'a [String], index: usize, name: &str) -> Option<&'a str> {
    argv[index].strip_prefix(&format!("{name}=")).or_else(|| {
        (argv[index] == name)
            .then(|| argv.get(index + 1).map(String::as_str))
            .flatten()
    })
}

pub(crate) fn argv_shadows(argv: &[String]) -> bool {
    for (index, argument) in argv.iter().enumerate() {
        if argument == "--safe-mode" {
            return true;
        }
        if let Some(value) = option_value(argv, index, "--setting-sources") {
            if !value.split(',').any(|tier| tier.trim() == "user") {
                return true;
            }
        }
        if let Some(value) = option_value(argv, index, "--settings") {
            let text = std::fs::read_to_string(value).unwrap_or_else(|_| value.to_string());
            if serde_json::from_str::<Value>(&text)
                .ok()
                .is_some_and(|parsed| !parsed["statusLine"].is_null())
            {
                return true;
            }
        }
    }
    false
}

pub(crate) fn settings_local_block(statusline: &str, project_command: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": format!("{} -- {}", shell_quote(statusline), shell_quote(project_command)),
        }
    }))
    .expect("serialises")
}

pub(crate) fn hooks_disabled(cwd: &Path, user_settings: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for dir in cwd.ancestors() {
        candidates.push(dir.join(".claude/settings.local.json"));
        candidates.push(dir.join(".claude/settings.json"));
    }
    candidates.push(user_settings.to_path_buf());
    candidates.into_iter().find(|candidate| {
        std::fs::read_to_string(candidate)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .is_some_and(|value| value["disableAllHooks"] == Value::Bool(true))
    })
}

pub(crate) fn window_size(status_path: &Path) -> Option<u64> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(status_path).ok()?).ok()?;
    let size = value["context_window"]["context_window_size"].as_u64()?;
    (size > 0).then_some(size)
}

fn daemon_answers(socket: &Path) -> bool {
    use std::io::{BufRead, BufReader, Write};
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(750)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(750)));
    if writeln!(stream, r#"{{"method":"snapshot"}}"#).is_err() {
        return false;
    }
    let Ok(reader) = stream.try_clone() else {
        return false;
    };
    let mut line = String::new();
    BufReader::new(reader).read_line(&mut line).is_ok()
        && serde_json::from_str::<Value>(&line)
            .ok()
            .is_some_and(|reply| !reply["version"].is_null())
}

fn executable_and_names(path: &Path, binary: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    meta.permissions().mode() & 0o111 != 0
        && std::fs::read_to_string(path)
            .map(|body| body.contains(&*binary.to_string_lossy()))
            .unwrap_or(false)
}

pub(crate) fn run(
    socket: &Path,
    settings_path: &Path,
    statusline: &Path,
    attention: &Path,
    binary: &Path,
    panes: &[PaneInput],
    resolve: &dyn Fn(&str, &str) -> Option<PathBuf>,
) -> Report {
    let mut report = Report::default();
    let up = daemon_answers(socket);
    report.checks.push(Check {
        id: CheckId::DaemonReachable,
        level: if up { Level::Ok } else { Level::Fail },
        summary: if up {
            "daemon answering"
        } else {
            "daemon is not answering"
        }
        .to_string(),
        evidence: Some(socket.display().to_string()),
        remedy: (!up).then_some(Remedy::PluginAction {
            id: "restart-daemon",
        }),
    });

    let enabled = std::fs::read_to_string(settings_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value["statusLine"]["command"].as_str().map(str::to_string))
        .is_some_and(|command| command.contains(&*statusline.to_string_lossy()));
    report.checks.push(Check {
        id: CheckId::BridgeEnabled,
        level: if enabled { Level::Ok } else { Level::Fail },
        summary: if enabled {
            "bridge enabled"
        } else {
            "bridge is not in this settings file"
        }
        .to_string(),
        evidence: Some(settings_path.display().to_string()),
        remedy: (!enabled).then_some(Remedy::PluginAction {
            id: "enable-claude-bridge",
        }),
    });

    let scripts_ok =
        executable_and_names(statusline, binary) && executable_and_names(attention, binary);
    report.checks.push(Check {
        id: CheckId::ScriptsPresent,
        level: if scripts_ok { Level::Ok } else { Level::Fail },
        summary: if scripts_ok {
            "scripts present, executable, and pointing at this binary"
        } else {
            "a script is missing, not executable, or names another binary"
        }
        .to_string(),
        evidence: Some(binary.display().to_string()),
        remedy: (!scripts_ok).then_some(Remedy::PluginAction {
            id: "enable-claude-bridge",
        }),
    });

    let mut shadowed = 0;
    for pane in panes {
        let shadow = shadowing_status_line(&pane.cwd, settings_path);
        let flag_shadow = argv_shadows(&pane.argv);
        let size = pane
            .agent_session
            .as_deref()
            .and_then(|session| resolve(&pane.pane_id, session))
            .as_deref()
            .and_then(window_size);
        let (level, summary, remedy) = match (&shadow, flag_shadow, &pane.agent_session, size) {
            (Some((path, command)), _, _, _) => {
                shadowed += 1;
                (
                    Level::Fail,
                    format!("shadowed by {}", path.display()),
                    Some(Remedy::WriteSettingsBlock {
                        path: path.with_file_name("settings.local.json"),
                        block: settings_local_block(&statusline.to_string_lossy(), command),
                    }),
                )
            }
            (None, true, _, _) => {
                shadowed += 1;
                (
                    Level::Fail,
                    "launched with flags that outrank the user settings tier".to_string(),
                    Some(Remedy::RestartSession),
                )
            }
            (None, false, None, _) => (
                Level::Warn,
                "herdr reports no agent session for this pane".to_string(),
                Some(Remedy::ReopenPane),
            ),
            // NOT "restart this session". Live verification showed sessions
            // that predate `enable` picking the bridge up on their own: Claude
            // re-reads its settings while running, so a session only needs to
            // render its status line once. An idle session has not, which is
            // the ordinary reason a bound pane shows nothing yet.
            (None, false, Some(_), None) => (
                Level::Warn,
                "no metrics yet; this pane has not rendered a status line since the bridge \
                 was enabled"
                    .to_string(),
                Some(Remedy::WaitOrInteract),
            ),
            (None, false, Some(_), Some(_)) => (Level::Ok, "reporting".to_string(), None),
        };
        report.panes.push(PaneFinding {
            pane_id: pane.pane_id.clone(),
            agent_session: pane.agent_session.clone(),
            level,
            summary,
            window_size: size,
            shadowed_by: shadow.map(|(path, _)| path),
            remedy,
        });
    }

    report.checks.push(Check {
        id: CheckId::NothingShadowing,
        level: if shadowed == 0 {
            Level::Ok
        } else {
            Level::Fail
        },
        summary: format!("{shadowed} pane(s) outranked by a higher-precedence statusLine"),
        evidence: None,
        remedy: None,
    });
    let silenced = panes
        .iter()
        .find_map(|pane| hooks_disabled(&pane.cwd, settings_path));
    report.checks.push(Check {
        id: CheckId::HooksEnabled,
        level: if silenced.is_some() {
            Level::Warn
        } else {
            Level::Ok
        },
        summary: if silenced.is_some() {
            "disableAllHooks silences the attention hooks; metrics are unaffected"
        } else {
            "hooks enabled"
        }
        .to_string(),
        evidence: silenced.map(|path| path.display().to_string()),
        remedy: None,
    });

    let others_ok = report.checks.iter().all(|check| check.level == Level::Ok);
    // Any non-Ok pane counts, not just Fail. A pane that has simply not
    // rendered yet is a Warn, and treating that as "all reporting" would put
    // the green back on an incomplete picture.
    let missing = report.panes.iter().any(|pane| pane.level != Level::Ok);
    report.checks.push(Check {
        id: CheckId::MetricsPresent,
        level: match (missing, others_ok) {
            (false, _) => Level::Ok,
            (true, true) => Level::Warn,
            (true, false) => Level::Fail,
        },
        summary: match (missing, others_ok) {
            (false, _) => "all bound panes are reporting".to_string(),
            (true, true) => "metrics are absent and every check I can run passes — I cannot see managed settings, workspace trust, or every launch flag".to_string(),
            (true, false) => "some panes are not reporting".to_string(),
        },
        evidence: None,
        remedy: None,
    });
    report
}

fn glyph(level: Level) -> &'static str {
    match level {
        Level::Ok => "✓",
        Level::Warn => "!",
        Level::Fail => "✗",
    }
}

pub(crate) fn render(report: &Report) -> String {
    let mut out = String::from("Agent Watcher — Claude bridge doctor\n\n");
    for check in &report.checks {
        out.push_str(&format!("{} {}\n", glyph(check.level), check.summary));
        if let Some(evidence) = &check.evidence {
            out.push_str(&format!("    {evidence}\n"));
        }
        if let Some(remedy) = &check.remedy {
            out.push_str(&format!("    → {}\n", remedy_line(remedy)));
        }
    }
    out.push_str("\nPanes\n");
    for pane in &report.panes {
        let window = pane
            .window_size
            .map(|size| format!(" · {size} window"))
            .unwrap_or_default();
        out.push_str(&format!(
            "{} {}{window}  {}\n",
            glyph(pane.level),
            pane.pane_id,
            pane.summary
        ));
        if let Some(remedy) = &pane.remedy {
            out.push_str(&format!("    → {}\n", remedy_line(remedy)));
        }
    }
    out
}

fn remedy_line(remedy: &Remedy) -> String {
    match remedy {
        Remedy::PluginAction { id } => {
            format!("herdr plugin action invoke {id} --plugin herdr-agent-watcher")
        }
        Remedy::RestartSession => "recreate this Claude session".to_string(),
        Remedy::WaitOrInteract => {
            "wait for its next status-line render, or send it a prompt".to_string()
        }
        Remedy::ReopenPane => "close and reopen this pane".to_string(),
        Remedy::WriteSettingsBlock { path, block } => {
            format!("add to {}:\n{block}", path.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answering_socket(dir: &Path) -> PathBuf {
        use std::io::{BufRead, BufReader, Write};
        let socket = dir.join("state.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut line = String::new();
                let Ok(cloned) = stream.try_clone() else {
                    continue;
                };
                let _ = BufReader::new(cloned).read_line(&mut line);
                let _ = writeln!(stream, r#"{{"version":2,"seq":0,"panes":{{}}}}"#);
            }
        });
        socket
    }

    fn healthy(dir: &Path, binary: &Path) -> (PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let statusline = dir.join("statusline.sh");
        let attention = dir.join("attention.sh");
        for path in [&statusline, &attention] {
            std::fs::write(path, format!("#!/bin/sh\n# {}\n", binary.display())).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let settings = dir.join("settings.json");
        std::fs::write(
            &settings,
            json!({"statusLine":{"command":statusline.to_string_lossy()}}).to_string(),
        )
        .unwrap();
        (settings, statusline, attention)
    }

    fn input(id: &str, session: Option<&str>, cwd: &Path) -> PaneInput {
        PaneInput {
            pane_id: id.to_string(),
            agent_session: session.map(str::to_string),
            cwd: cwd.to_path_buf(),
            argv: vec!["claude".to_string()],
        }
    }

    #[test]
    fn parent_project_status_line_is_found_and_no_status_line_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let deep = repo.join("crate/src");
        let user = dir.path().join("elsewhere/settings.json");
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(
            repo.join(".claude/settings.json"),
            json!({"statusLine":{"command":"jq -r .model"}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            shadowing_status_line(&deep, &user),
            Some((repo.join(".claude/settings.json"), "jq -r .model".into()))
        );
        std::fs::write(
            repo.join(".claude/settings.json"),
            json!({"hooks":{"Stop":[]}}).to_string(),
        )
        .unwrap();
        assert!(shadowing_status_line(&deep, &user).is_none());
    }

    #[test]
    fn a_stale_shim_is_removed_but_a_real_claude_is_not() {
        use crate::agents::claude_bridge::remove_stale_shim_for_test as remove_stale_shim;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();

        // Anything not carrying the shim's own marker is left alone: this
        // directory is ours, but deleting a binary we did not write is not.
        let real = bin.join("claude");
        std::fs::write(&real, "#!/bin/sh\nexec /usr/local/bin/claude \"$@\"\n").unwrap();
        remove_stale_shim(&bin);
        assert!(real.exists(), "a claude we did not write was deleted");

        std::fs::write(
            &real,
            "#!/usr/bin/env bash\nprintf '%s\\n' 'herdr-agent-watcher shim: real claude not found' >&2\n",
        )
        .unwrap();
        remove_stale_shim(&bin);
        assert!(!real.exists(), "the superseded shim survived");
    }

    #[test]
    fn the_user_tier_is_never_reported_as_shadowing_itself() {
        // Live-verification regression. A pane's cwd ancestors reach $HOME, and
        // ~/.claude/settings.json is where the bridge installs itself. Walking
        // into it made every pane report "shadowed by ~/.claude/settings.json"
        // with a remedy that chained statusline.sh to itself. Every other test
        // here uses a tempdir and never walks far enough to see it.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let deep = home.join("projects/thing/src");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(&deep).unwrap();
        let user = home.join(".claude/settings.json");
        // Exactly what `enable` writes: the bridge's own status line.
        std::fs::write(
            &user,
            json!({"statusLine":{"command":"'/state/bin/statusline.sh'"}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            shadowing_status_line(&deep, &user),
            None,
            "the bridge's own tier is not a shadow"
        );

        // A real project statusLine BELOW it is still found.
        std::fs::create_dir_all(home.join("projects/thing/.claude")).unwrap();
        std::fs::write(
            home.join("projects/thing/.claude/settings.json"),
            json!({"statusLine":{"command":"jq -r .model"}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            shadowing_status_line(&deep, &user),
            Some((
                home.join("projects/thing/.claude/settings.json"),
                "jq -r .model".into()
            ))
        );
    }

    #[test]
    fn emitted_fix_runs_with_multiword_downstream() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("statusline.sh");
        std::fs::write(&script, "#!/usr/bin/env bash\npayload=$(cat)\ndownstream=''\nif [ \"${1:-}\" = '--' ]; then downstream=${2-}; fi\n[ -n \"$downstream\" ] && printf '%s' \"$payload\" | sh -c \"$downstream\"\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let block = settings_local_block(&script.to_string_lossy(), "printf %s MULTI-WORD-OK");
        let value: Value = serde_json::from_str(&block).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(value["statusLine"]["command"].as_str().unwrap())
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "MULTI-WORD-OK");
    }

    #[test]
    fn raw_window_uses_snake_case_and_seed_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let status = dir.path().join("status.json");
        std::fs::write(
            &status,
            json!({"context_window":{"context_window_size":200000}}).to_string(),
        )
        .unwrap();
        assert_eq!(window_size(&status), Some(200_000));
        std::fs::write(&status, json!({"session_id":"s","model":{}}).to_string()).unwrap();
        assert_eq!(window_size(&status), None);
    }

    #[test]
    fn disable_all_hooks_is_found_in_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("repo/crate");
        std::fs::create_dir_all(dir.path().join("repo/.claude")).unwrap();
        std::fs::create_dir_all(&deep).unwrap();
        let user = dir.path().join("user.json");
        std::fs::write(&user, "{}").unwrap();
        assert!(hooks_disabled(&deep, &user).is_none());
        let project = dir.path().join("repo/.claude/settings.json");
        std::fs::write(&project, json!({"disableAllHooks":true}).to_string()).unwrap();
        assert_eq!(hooks_disabled(&deep, &user), Some(project));
    }

    #[test]
    fn argv_flags_are_modelled_by_content_and_source() {
        assert!(!argv_shadows(&[
            "claude".into(),
            "--settings".into(),
            r#"{"model":"opus"}"#.into()
        ]));
        assert!(argv_shadows(&[
            "claude".into(),
            r#"--settings={"statusLine":{"command":"x"}}"#.into()
        ]));
        assert!(argv_shadows(&[
            "claude".into(),
            "--setting-sources=project,local".into()
        ]));
        assert!(!argv_shadows(&[
            "claude".into(),
            "--setting-sources".into(),
            "user,project".into()
        ]));
        assert!(argv_shadows(&["claude".into(), "--safe-mode".into()]));
    }

    #[test]
    fn assembled_report_covers_green_binding_shadow_and_unknown_cause() {
        let dir = tempfile::tempdir().unwrap();
        let binary = Path::new("/bin/herdr-agent-watcher");
        let (settings, statusline, attention) = healthy(dir.path(), binary);
        let socket = answering_socket(dir.path());
        let status = dir.path().join("status.json");
        std::fs::write(
            &status,
            json!({"context_window":{"context_window_size":200000}}).to_string(),
        )
        .unwrap();
        let report = run(
            &socket,
            &settings,
            &statusline,
            &attention,
            binary,
            &[input("w1:p1", Some("s1"), dir.path())],
            &|_, _| Some(status.clone()),
        );
        assert!(report.checks.iter().all(|check| check.level == Level::Ok));
        assert_eq!(report.panes[0].window_size, Some(200_000));

        let no_session = run(
            &socket,
            &settings,
            &statusline,
            &attention,
            binary,
            &[input("w1:p1", None, dir.path())],
            &|_, _| None,
        );
        assert!(matches!(
            no_session.panes[0].remedy,
            Some(Remedy::ReopenPane)
        ));

        std::fs::write(&status, json!({"session_id":"s1"}).to_string()).unwrap();
        let unknown = run(
            &socket,
            &settings,
            &statusline,
            &attention,
            binary,
            &[input("w1:p1", Some("s1"), dir.path())],
            &|_, _| Some(status.clone()),
        );
        let verdict = unknown
            .checks
            .iter()
            .find(|check| check.id == CheckId::MetricsPresent)
            .unwrap();
        assert_ne!(verdict.level, Level::Ok);
        assert!(verdict.summary.contains("cannot see"));
    }

    #[test]
    fn shadowed_pane_emits_file_and_pasteable_block() {
        let dir = tempfile::tempdir().unwrap();
        let binary = Path::new("/bin/herdr-agent-watcher");
        let (settings, statusline, attention) = healthy(dir.path(), binary);
        let project = dir.path().join("repo");
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        std::fs::write(
            project.join(".claude/settings.json"),
            json!({"statusLine":{"command":"jq -r .model"}}).to_string(),
        )
        .unwrap();
        let socket = answering_socket(dir.path());
        let report = run(
            &socket,
            &settings,
            &statusline,
            &attention,
            binary,
            &[input("w1:p1", Some("s1"), &project)],
            &|_, _| None,
        );
        match &report.panes[0].remedy {
            Some(Remedy::WriteSettingsBlock { path, block }) => {
                assert!(path.ends_with("settings.local.json"));
                assert!(block.contains("jq -r .model"));
            }
            other => panic!("unexpected remedy: {other:?}"),
        }
    }
}
