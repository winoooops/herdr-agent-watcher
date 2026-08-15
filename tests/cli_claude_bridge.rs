mod support;

#[test]
fn doctor_reports_a_rejected_interval_from_the_plugin_config() {
    let config = tempfile::tempdir().expect("tempdir");
    let state = tempfile::tempdir().expect("tempdir");
    let claude = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        config.path().join("config.toml"),
        "[daemon]\ninterval_ms = 0\n",
    )
    .expect("write");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
        .arg("doctor")
        .env("CLAUDE_CONFIG_DIR", claude.path())
        .env("HERDR_PLUGIN_CONFIG_DIR", config.path())
        .env("HERDR_PLUGIN_STATE_DIR", state.path())
        .env_remove("AGENT_WATCHER_INTERVAL_MS")
        .env_remove("HERDR_SOCKET_PATH")
        .output()
        .expect("run doctor");

    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("daemon.interval_ms"), "{text}");
    assert!(text.contains("1000"), "{text}");
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
fn the_write_flag_reaches_the_writer_and_prints_nothing() {
    use std::io::Write;
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("status.json");
    let socket = fake_state_socket(dir.path(), Some(&target.to_string_lossy()));
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
        .args(["claude-bridge", "--write", "--pane", "w1:p1", "--socket"])
        .arg(&socket)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"session_id":"s1","cost":{}}"#)
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "stdout would land in the status line"
    );
    assert!(std::fs::read_to_string(&target).unwrap().contains("s1"));
}

#[test]
fn claude_bridge_prints_one_path_and_nothing_else() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
        .args(["claude-bridge", "--pane", "w2:p1"])
        .arg("--state-dir")
        .arg(dir.path())
        .arg("--cwd")
        .arg(&cwd)
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.lines().count(),
        1,
        "exactly one line, for command substitution"
    );
    assert!(std::path::Path::new(stdout.trim()).exists());
}

#[test]
fn claude_bridge_without_a_pane_fails_instead_of_guessing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
        .arg("claude-bridge")
        .arg("--state-dir")
        .arg(dir.path())
        .env_remove("HERDR_PANE_ID")
        .output()
        .expect("run");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("pane"));
}

#[test]
fn claude_bridge_resolves_the_pane_from_the_environment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
        .arg("claude-bridge")
        .arg("--state-dir")
        .arg(dir.path())
        .arg("--cwd")
        .arg(&cwd)
        .env("HERDR_PANE_ID", "w9:p9")
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("w9:p9"));
}

#[test]
fn pane_without_cwd_uses_herdrs_cwd_for_that_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = support::FakeHerdr::start(tmp.path());
    let target = tmp.path().join("target-repo");
    std::fs::create_dir_all(&target).unwrap();
    fake.set_panes(serde_json::json!([{
        "pane_id": "w2:p1", "workspace_id": "w2", "agent": "claude",
        "cwd": target.to_string_lossy(),
        "foreground_cwd": target.to_string_lossy(),
    }]));

    // Run from a DIFFERENT directory: $PWD must not be what keys the bridge.
    let elsewhere = tmp.path().join("controller");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
        .args(["claude-bridge", "--pane", "w2:p1"])
        .arg("--state-dir")
        .arg(tmp.path())
        .current_dir(&elsewhere)
        .env("HERDR_SOCKET_PATH", &fake.socket_path)
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The frozen helper is crate-private, so assert the observable path shape.
    let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(printed.contains("/sessions/w2:p1/"), "{printed}");
    assert!(!printed.contains("controller-"), "{printed}");
    fake.stop();
}

#[test]
fn pane_without_a_reported_cwd_fails_instead_of_using_the_callers() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = support::FakeHerdr::start(tmp.path());
    fake.set_panes(serde_json::json!([{
        "pane_id": "w2:p1", "workspace_id": "w2", "agent": "claude"
    }]));
    let elsewhere = tmp.path().join("controller");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
        .args(["claude-bridge", "--pane", "w2:p1"])
        .arg("--state-dir")
        .arg(tmp.path())
        .current_dir(&elsewhere)
        .env("HERDR_SOCKET_PATH", &fake.socket_path)
        .output()
        .expect("run");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("did not report a cwd"));
    fake.stop();
}
