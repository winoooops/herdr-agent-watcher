mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use support::{wait_for, FakeHerdr};

struct ProcessGuard(Child);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn binds_pane_reports_state_and_exits_when_herdr_dies() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start(tmp.path());
    let home = tmp.path().join("home");
    let state = tmp.path().join("state");
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    support::write_claude_fixture(&home, &state, &cwd, "p1", "sess-1");

    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", bin.join("claude")).unwrap();
    let decoy = ProcessGuard(
        Command::new("sh")
            .args(["-c", &format!("{} 300; :", bin.join("claude").display())])
            .spawn()
            .unwrap(),
    );
    fake.set_shell_pid(decoy.0.id());
    fake.set_panes(serde_json::json!([{
        "pane_id": "p1",
        "workspace_id": "w1",
        "agent": "claude",
        "agent_session": {
            "source": "herdr:claude",
            "agent": "claude",
            "kind": "id",
            "value": "sess-1",
        },
        "cwd": cwd,
    }]));

    let daemon = ProcessGuard(
        Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
            .arg("daemon")
            .env("HERDR_SOCKET_PATH", &fake.socket_path)
            .env("HERDR_PLUGIN_STATE_DIR", &state)
            .env("HOME", &home)
            .env("AGENT_WATCHER_INTERVAL_MS", "25")
            .env("RUST_LOG", "agent_watcher=debug")
            .spawn()
            .unwrap(),
    );

    wait_for(
        || state.join("herdr-agent-watcher.lock").exists(),
        Duration::from_secs(3),
    );
    wait_for(
        || !fake.calls_named("pane.list").is_empty(),
        Duration::from_secs(3),
    );
    wait_for(
        || {
            fake.calls_named("pane.report_metadata").iter().any(|call| {
                call["params"]["pane_id"] == "p1"
                    && call["params"]["tokens"]
                        .get("agent_watcher_state")
                        .is_some()
            })
        },
        Duration::from_secs(5),
    );

    let state_socket = state.join("herdr-agent-watcher-state.sock");
    wait_for(
        || {
            support::state_snapshot(&state_socket).is_some_and(|snapshot| {
                snapshot["version"] == herdr_agent_watcher::daemon::state_wire::WIRE_VERSION
                    && snapshot["panes"]["p1"]
                        .get("status")
                        .is_some_and(|status| !status.is_null())
            })
        },
        Duration::from_secs(5),
    );

    let snapshot = support::state_snapshot(&state_socket).expect("snapshot");
    assert_eq!(
        snapshot["panes"]["p1"]["workspace_id"], "w1",
        "the reconcile loop must record each pane's workspace: {snapshot}"
    );

    let mut subscriber = std::os::unix::net::UnixStream::connect(&state_socket).unwrap();
    subscriber
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    subscriber
        .write_all(b"{\"method\":\"subscribe\"}\n")
        .unwrap();
    let mut reader = BufReader::new(subscriber);
    let mut hello = String::new();
    reader.read_line(&mut hello).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&hello).unwrap()["version"],
        herdr_agent_watcher::daemon::state_wire::WIRE_VERSION,
    );

    let transcript = home.join(".claude/projects/demo/sess-1.jsonl");
    let mut transcript = std::fs::OpenOptions::new()
        .append(true)
        .open(transcript)
        .unwrap();
    writeln!(
        transcript,
        r#"{{"type":"assistant","timestamp":"2026-08-11T09:30:00.000Z","message":{{"content":[{{"type":"tool_use","id":"toolu_e2e_state_socket","name":"Bash","input":{{"command":"true"}}}}]}}}}"#,
    )
    .unwrap();
    writeln!(
        transcript,
        r#"{{"type":"user","timestamp":"2026-08-11T09:30:01.000Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_e2e_state_socket","is_error":false,"content":"ok"}}]}}}}"#,
    )
    .unwrap();
    transcript.flush().unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut terminal_seen = false;
    while std::time::Instant::now() < deadline && !terminal_seen {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line.trim().is_empty() => continue,
            Ok(_) => {
                let delta: serde_json::Value = serde_json::from_str(&line).unwrap();
                terminal_seen = delta["pane_id"] == "p1"
                    && delta["telemetry"]["tool_calls"]
                        .as_array()
                        .is_some_and(|calls| {
                            calls.iter().any(|call| {
                                call["toolUseId"] == "toolu_e2e_state_socket"
                                    && matches!(call["status"].as_str(), Some("done" | "failed"))
                            })
                        });
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("state subscriber read failed: {error}"),
        }
    }
    assert!(terminal_seen, "state socket emitted the terminal tool fold");

    fake.stop();
    let mut daemon = daemon;
    wait_for(
        || daemon.0.try_wait().ok().flatten().is_some(),
        Duration::from_secs(2),
    );
}

#[test]
fn codex_binds_pane_and_reports_state() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start(tmp.path());
    let home = tmp.path().join("home");
    let state = tmp.path().join("state");
    let cwd = tmp.path().join("repo");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", bin.join("codex")).unwrap();
    let decoy = ProcessGuard(
        Command::new(bin.join("codex"))
            .arg("300")
            .current_dir(&cwd)
            .spawn()
            .unwrap(),
    );
    support::write_codex_fixture(&home, &cwd, decoy.0.id());
    fake.set_shell_pid(decoy.0.id());
    fake.set_panes(serde_json::json!([{
        "pane_id": "p-codex",
        "workspace_id": "w-demo",
        "agent": "codex",
        "agent_session": {
            "source": "herdr:codex",
            "agent": "codex",
            "kind": "id",
            "value": "demo-codex-session",
        },
        "cwd": cwd,
    }]));

    let daemon = ProcessGuard(
        Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
            .arg("daemon")
            .env("HERDR_SOCKET_PATH", &fake.socket_path)
            .env("HERDR_PLUGIN_STATE_DIR", &state)
            .env("HOME", &home)
            .env("AGENT_WATCHER_INTERVAL_MS", "25")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || {
            fake.calls_named("pane.report_metadata").iter().any(|call| {
                call["params"]["pane_id"] == "p-codex"
                    && (call["params"]["tokens"]
                        .get("agent_watcher_state")
                        .is_some()
                        || call["params"]["tokens"]
                            .get("agent_watcher_phase")
                            .is_some())
            })
        },
        Duration::from_secs(5),
    );
    fake.stop();
    drop(daemon);
}

#[test]
fn kimi_binds_pane_and_reports_state() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start(tmp.path());
    let home = tmp.path().join("home");
    let kimi_home = home.join(".kimi-code");
    let state = tmp.path().join("state");
    let cwd = tmp.path().join("repo");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", bin.join("kimi")).unwrap();
    let decoy = ProcessGuard(
        Command::new(bin.join("kimi"))
            .arg("300")
            .current_dir(&cwd)
            .spawn()
            .unwrap(),
    );
    support::write_kimi_fixture(&kimi_home, &cwd, "demo-kimi-session");
    fake.set_shell_pid(decoy.0.id());
    fake.set_panes(serde_json::json!([{
        "pane_id": "p-kimi",
        "workspace_id": "w-demo",
        "agent": "kimi",
        "agent_session": {
            "source": "herdr:kimi",
            "agent": "kimi",
            "kind": "id",
            "value": "demo-kimi-session",
        },
        "cwd": cwd,
    }]));

    let daemon = ProcessGuard(
        Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
            .arg("daemon")
            .env("HERDR_SOCKET_PATH", &fake.socket_path)
            .env("HERDR_PLUGIN_STATE_DIR", &state)
            .env("HOME", &home)
            .env("KIMI_CODE_HOME", &kimi_home)
            .env("AGENT_WATCHER_INTERVAL_MS", "25")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || {
            fake.calls_named("pane.report_metadata").iter().any(|call| {
                call["params"]["pane_id"] == "p-kimi"
                    && (call["params"]["tokens"]
                        .get("agent_watcher_state")
                        .is_some()
                        || call["params"]["tokens"]
                            .get("agent_watcher_phase")
                            .is_some())
            })
        },
        Duration::from_secs(5),
    );
    fake.stop();
    drop(daemon);
}

#[test]
fn kimi_consent_changes_reach_the_running_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start(tmp.path());
    let home = tmp.path().join("home");
    let kimi_home = home.join(".kimi-code");
    let state = tmp.path().join("state");
    let cwd = tmp.path().join("repo");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", bin.join("kimi")).unwrap();
    let decoy = ProcessGuard(
        Command::new(bin.join("kimi"))
            .arg("300")
            .current_dir(&cwd)
            .spawn()
            .unwrap(),
    );
    support::write_kimi_fixture(&kimi_home, &cwd, "demo-kimi-consent");

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let stop_server = Arc::new(AtomicBool::new(false));
    let server_requests = requests.clone();
    let server_stop = stop_server.clone();
    let server = std::thread::spawn(move || {
        while !server_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut buffer = [0_u8; 1024];
                    let _ = stream.read(&mut buffer);
                    server_requests.fetch_add(1, Ordering::Relaxed);
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    std::fs::write(
        kimi_home.join("config.toml"),
        format!(
            "[providers.\"managed:kimi-code\"]\nbase_url = \"http://{address}\"\napi_key = \"e2e-token\"\n"
        ),
    )
    .unwrap();

    fake.set_shell_pid(decoy.0.id());
    fake.set_panes(serde_json::json!([{
        "pane_id": "p-kimi-consent",
        "workspace_id": "w-demo",
        "agent": "kimi",
        "agent_session": {
            "source": "herdr:kimi",
            "agent": "kimi",
            "kind": "id",
            "value": "demo-kimi-consent",
        },
        "cwd": cwd,
    }]));
    let daemon = ProcessGuard(
        Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
            .arg("daemon")
            .env("HERDR_SOCKET_PATH", &fake.socket_path)
            .env("HERDR_PLUGIN_STATE_DIR", &state)
            .env("HOME", &home)
            .env("KIMI_CODE_HOME", &kimi_home)
            .env("AGENT_WATCHER_INTERVAL_MS", "25")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || {
            fake.calls_named("pane.report_metadata")
                .iter()
                .any(|call| call["params"]["pane_id"] == "p-kimi-consent")
        },
        Duration::from_secs(5),
    );
    std::thread::sleep(Duration::from_millis(1_600));
    assert_eq!(requests.load(Ordering::Relaxed), 0);

    assert!(Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
        .args(["kimi-consent", "on"])
        .env("HERDR_PLUGIN_STATE_DIR", &state)
        .status()
        .unwrap()
        .success());
    wait_for(
        || requests.load(Ordering::Relaxed) >= 1,
        Duration::from_secs(5),
    );

    assert!(Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
        .args(["kimi-consent", "off"])
        .env("HERDR_PLUGIN_STATE_DIR", &state)
        .status()
        .unwrap()
        .success());
    std::thread::sleep(Duration::from_millis(1_000));
    let after_revoke = requests.load(Ordering::Relaxed);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let wire = kimi_home.join("sessions/demo-kimi-consent/agents/main/wire.jsonl");
    let mut wire = std::fs::OpenOptions::new().append(true).open(wire).unwrap();
    for record in [
        serde_json::json!({
            "type": "turn.prompt",
            "input": [{"type": "text", "text": "demo prompt after revoke"}],
            "origin": {"kind": "user"},
            "time": now_ms,
        }),
        serde_json::json!({
            "type": "context.append_loop_event",
            "event": {
                "type": "step.end", "uuid": "demo-step-after-revoke",
                "turnId": "demo-turn-after-revoke", "step": 1,
                "usage": {"inputOther": 1, "output": 1, "inputCacheRead": 0, "inputCacheCreation": 0},
                "finishReason": "end_turn",
            },
            "time": now_ms + 1,
        }),
        serde_json::json!({
            "type": "usage.record", "model": "kimi-code/demo",
            "usage": {"inputOther": 1, "output": 1, "inputCacheRead": 0, "inputCacheCreation": 0},
            "usageScope": "turn", "time": now_ms + 2,
        }),
    ] {
        writeln!(wire, "{record}").unwrap();
    }
    std::thread::sleep(Duration::from_millis(1_600));
    assert_eq!(requests.load(Ordering::Relaxed), after_revoke);

    fake.stop();
    drop(daemon);
    stop_server.store(true, Ordering::Relaxed);
    server.join().unwrap();
}

#[test]
fn opencode_binds_pane_reports_state_and_installs_bridge() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start(tmp.path());
    let home = tmp.path().join("home");
    let state = tmp.path().join("state");
    let cwd = tmp.path().join("repo");
    let bin = tmp.path().join("bin");
    let bridge_dir = tmp.path().join("bridge");
    let plugins_dir = tmp.path().join("plugins");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", bin.join("opencode")).unwrap();
    let decoy = ProcessGuard(
        Command::new(bin.join("opencode"))
            .arg("300")
            .current_dir(&cwd)
            .spawn()
            .unwrap(),
    );
    support::write_opencode_fixture(&bridge_dir, &cwd, "demo-opencode-session", decoy.0.id());
    fake.set_shell_pid(decoy.0.id());
    fake.set_panes(serde_json::json!([{
        "pane_id": "p-opencode",
        "workspace_id": "w-demo",
        "agent": "opencode",
        "agent_session": {
            "source": "herdr:opencode",
            "agent": "opencode",
            "kind": "id",
            "value": "demo-opencode-session",
        },
        "cwd": cwd,
    }]));

    let daemon = ProcessGuard(
        Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
            .arg("daemon")
            .env("HERDR_SOCKET_PATH", &fake.socket_path)
            .env("HERDR_PLUGIN_STATE_DIR", &state)
            .env("HOME", &home)
            .env("AGENT_WATCHER_OPENCODE_BRIDGE_DIR", &bridge_dir)
            .env("AGENT_WATCHER_OPENCODE_PLUGINS_DIR", &plugins_dir)
            .env("AGENT_WATCHER_INTERVAL_MS", "25")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || {
            fake.calls_named("pane.report_metadata").iter().any(|call| {
                call["params"]["pane_id"] == "p-opencode"
                    && (call["params"]["tokens"]
                        .get("agent_watcher_state")
                        .is_some()
                        || call["params"]["tokens"]
                            .get("agent_watcher_phase")
                            .is_some())
            })
        },
        Duration::from_secs(5),
    );
    assert!(plugins_dir
        .join("agent-watcher-opencode-bridge.ts")
        .is_file());
    let mut transcript = std::fs::OpenOptions::new()
        .append(true)
        .open(bridge_dir.join("demo-opencode-session.jsonl"))
        .unwrap();
    for line in [
        r#"{"v":1,"ts":1,"kind":"event","type":"session.status","data":{"sessionID":"demo-opencode-session","status":{"type":"busy"}}}"#,
        r#"{"v":1,"ts":2,"kind":"event","type":"assistant.text","data":{"sessionID":"demo-opencode-session","text":"demo complete"}}"#,
        r#"{"v":1,"ts":3,"kind":"event","type":"session.status","data":{"sessionID":"demo-opencode-session","status":{"type":"idle"}}}"#,
        r#"{"v":1,"ts":4,"kind":"event","type":"session.idle","data":{"sessionID":"demo-opencode-session"}}"#,
    ] {
        writeln!(transcript, "{line}").unwrap();
    }
    transcript.flush().unwrap();
    wait_for(
        || fake.calls_named("notification.show").len() == 1,
        Duration::from_secs(5),
    );
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(fake.calls_named("notification.show").len(), 1);
    fake.stop();
    drop(daemon);
}

#[test]
fn takeover_replaces_running_daemon_within_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start(tmp.path());
    let state = tmp.path().join("state");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let spawn_daemon = || {
        Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
            .arg("daemon")
            .env("HERDR_SOCKET_PATH", &fake.socket_path)
            .env("HERDR_PLUGIN_STATE_DIR", &state)
            .env("HOME", &home)
            .env("AGENT_WATCHER_INTERVAL_MS", "60000")
            .spawn()
            .unwrap()
    };
    let mut first = ProcessGuard(spawn_daemon());
    wait_for(
        || !fake.calls_named("pane.list").is_empty(),
        Duration::from_secs(3),
    );

    let stalled =
        std::os::unix::net::UnixStream::connect(state.join("herdr-agent-watcher-control.sock"))
            .unwrap();
    let started = std::time::Instant::now();
    let mut second = ProcessGuard(spawn_daemon());
    wait_for(
        || first.0.try_wait().ok().flatten().is_some(),
        Duration::from_secs(5),
    );
    assert!(second.0.try_wait().unwrap().is_none());
    assert!(started.elapsed() < Duration::from_secs(5));
    drop(stalled);
    fake.stop();
}

#[test]
fn takeover_of_unresponsive_holder_fails_within_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(state.join("herdr-agent-watcher.lock"))
        .unwrap();
    use std::os::unix::io::AsRawFd;
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let listener =
        std::os::unix::net::UnixListener::bind(state.join("herdr-agent-watcher-control.sock"))
            .unwrap();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            std::thread::sleep(Duration::from_secs(6));
            drop(stream);
        }
    });

    let started = std::time::Instant::now();
    let status = Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
        .arg("daemon")
        .env("HERDR_PLUGIN_STATE_DIR", &state)
        .status()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(!status.success());
    assert!(
        elapsed < Duration::from_millis(5_250),
        "takeover took {elapsed:?}"
    );
    drop(lock);
}

/// A COMPLETED tool call appended to a claude transcript — `tool_use` followed by
/// its `tool_result`. The `tool_use` alone only opens the call, and the store
/// counts settled calls (`done`/`failed`) only, so a lone `tool_use` would leave
/// `tool_call_total` at zero and the seed below would never arrive.
fn append_settled_tool_call(home: &std::path::Path, session_id: &str, tool: &str) {
    let path = home
        .join(".claude/projects/demo")
        .join(format!("{session_id}.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        file,
        r#"{{"type":"assistant","sessionId":"{session_id}","timestamp":"2026-08-12T00:00:01.000Z","message":{{"content":[{{"type":"tool_use","id":"toolu_1","name":"{tool}","input":{{}}}}]}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"user","sessionId":"{session_id}","timestamp":"2026-08-12T00:00:02.000Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_1","is_error":false,"content":"ok"}}]}}}}"#
    )
    .unwrap();
}

#[test]
fn non_default_config_reaches_what_is_drawn() {
    // The view is pure, so this needs no daemon and no TTY: load a config, build
    // the ViewInput from it, render, and assert on the text. This is the path
    // that would otherwise accept a setting and silently ignore it.
    use herdr_agent_watcher::daemon::store::{CardState, PaneTelemetry};
    use herdr_agent_watcher::sidebar::{config::Loaded, reducer::State, view};

    let cfg = Loaded::from_toml(
        "[list]\nsort = \"group\"\n\n[appearance]\nagent_mark = \"initial\"\n\n[cards]\ntool_calls = \"jar\"\nauto_expand = \"all\"\n",
    );
    assert_eq!(cfg.status.problems, 0, "the fixture itself must be valid");

    let telemetry = |agent: &str, task: &str| {
        let mut t = PaneTelemetry::with_agent(agent);
        t.card_state = CardState::Running;
        t.cwd = Some(format!("/w/{agent}"));
        t.title = Some(serde_json::json!({ "title": task }));
        t.tool_counts = [("Edit".to_string(), 3u64), ("Bash".to_string(), 1)]
            .into_iter()
            .collect();
        t.tool_call_total = 4;
        t
    };
    let mut panes = std::collections::HashMap::new();
    panes.insert("p1".to_string(), telemetry("codex", "review the diff"));
    panes.insert("p2".to_string(), telemetry("claude", "write the spec"));
    let state = State { panes, last_seq: 2 };

    let toggled = std::collections::HashSet::new();
    let input = view::ViewInput {
        cursor: None,
        toggled: &toggled,
        hide_idle: cfg.hide_idle,
        sort: cfg.sort,
        auto_expand: cfg.auto_expand,
        agent_mark: cfg.agent_mark,
        tool_calls: cfg.tool_calls,
        theme: cfg.theme,
        trace_lines: cfg.trace_lines,
        agents: &cfg.appearances,
        config: cfg.status,
    };
    let text = view::render(&state, &input, 44, 0).plain().join("\n");

    assert!(
        text.find("CLAUDE").unwrap() < text.find("CODEX").unwrap(),
        "sort = group orders by agent"
    );
    assert!(
        text.contains("C CLAUDE"),
        "agent_mark = initial reached the screen"
    );
    assert!(
        text.contains("Edit 3 · Bash 1"),
        "tool_calls = jar drew a legend, not bar rows"
    );
    assert!(text.contains("TOOLS"), "auto_expand = all opened the cards");
}

#[test]
fn a_rebind_resets_session_scoped_state_but_keeps_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start(tmp.path());
    let home = tmp.path().join("home");
    let state = tmp.path().join("state");
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    support::write_claude_fixture(&home, &state, &cwd, "p1", "sess-1");
    append_settled_tool_call(&home, "sess-1", "Read");

    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", bin.join("claude")).unwrap();
    let decoy = ProcessGuard(
        Command::new("sh")
            .args(["-c", &format!("{} 300; :", bin.join("claude").display())])
            .spawn()
            .unwrap(),
    );
    fake.set_shell_pid(decoy.0.id());

    let pane = |session: &str| {
        serde_json::json!([{
            "pane_id": "p1",
            "workspace_id": "w1",
            "agent": "claude",
            "agent_session": {
                "source": "herdr:claude", "agent": "claude",
                "kind": "id", "value": session,
            },
            "cwd": cwd,
        }])
    };
    fake.set_panes(pane("sess-1"));

    let _daemon = ProcessGuard(
        Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
            .arg("daemon")
            .env("HERDR_SOCKET_PATH", &fake.socket_path)
            .env("HERDR_PLUGIN_STATE_DIR", &state)
            .env("HOME", &home)
            .env("AGENT_WATCHER_INTERVAL_MS", "25")
            .spawn()
            .unwrap(),
    );

    let socket = state.join("herdr-agent-watcher-state.sock");
    let total = || {
        support::state_snapshot(&socket)
            .and_then(|snapshot| snapshot["panes"]["p1"]["tool_call_total"].as_u64())
    };

    // SEED FIRST. `tool_call_total` starts at zero, so waiting for zero after the
    // swap would succeed before anything had happened and prove nothing.
    wait_for(|| total().unwrap_or(0) > 0, Duration::from_secs(10));
    let before = support::state_snapshot(&socket).unwrap();
    let cwd_before = before["panes"]["p1"]["cwd"].clone();
    assert!(
        cwd_before.is_string(),
        "the pane-list cwd reached the store"
    );

    // Subscribe BEFORE the swap and watch the DELTAS. The final snapshot alone
    // would also match the old Unbind+Bind sequence, because the pane-list cwd
    // fallback repopulates the entry within a tick — the difference between
    // "replaced" and "deleted then recreated" only exists in the transition.
    let mut deltas = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    deltas.write_all(b"{\"method\":\"subscribe\"}\n").unwrap();
    deltas
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let mut lines = BufReader::new(deltas).lines();
    lines.next().expect("hello").expect("hello line"); // the initial snapshot

    // Same pane, same agent, new session. sess-2 runs a DIFFERENT tool: waiting
    // for `total == 0` alone would pass the instant `replace_session()` publishes
    // its provisional entry, before the replacement watcher had started at all.
    support::write_claude_fixture(&home, &state, &cwd, "p1", "sess-2");
    append_settled_tool_call(&home, "sess-2", "Bash");
    fake.set_panes(pane("sess-2"));

    let mut deletions = 0;
    let mut saw_replacement = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    for line in lines.by_ref() {
        assert!(
            std::time::Instant::now() < deadline,
            "no rebind observed within 15s (deletions so far: {deletions})"
        );
        let line = line.expect("state socket closed mid-test");
        // `state_server` writes a bare newline every 5s as a heartbeat; feeding
        // that to serde would panic instead of failing with something useful.
        if line.trim().is_empty() {
            continue;
        }
        let delta: serde_json::Value = serde_json::from_str(&line).expect("delta json");
        if delta["pane_id"] != "p1" {
            continue;
        }
        if delta["telemetry"].is_null() {
            deletions += 1; // an Unbind would show up exactly here
            continue;
        }
        if !saw_replacement {
            // `replace_session()` publishes an entry with the session-scoped
            // fields cleared. Accepting `Bash` before seeing THAT delta would
            // also accept the old watcher noticing the new transcript on its own,
            // which is not a rebind at all.
            saw_replacement = delta["telemetry"]["tool_call_total"] == 0
                && delta["telemetry"]["tool_counts"]
                    .as_object()
                    .map(|m| m.is_empty())
                    .unwrap_or(false);
            continue;
        }
        if delta["telemetry"]["tool_counts"]["Bash"] == 1 {
            break; // the replacement watcher is live
        }
    }
    assert!(
        saw_replacement,
        "the swap itself must appear on the wire, not only its result"
    );
    assert_eq!(
        deletions, 0,
        "a Rebind replaces the entry; it never deletes it (§1.4)"
    );

    let after = support::state_snapshot(&socket).unwrap();
    assert_eq!(after["panes"]["p1"]["tool_counts"]["Bash"], 1);
    assert!(
        after["panes"]["p1"]["tool_counts"]["Read"].is_null(),
        "session-scoped state did not carry across"
    );
    assert_eq!(
        total(),
        Some(1),
        "the new session's count, not the old one's"
    );
    assert_eq!(
        after["panes"]["p1"]["cwd"], cwd_before,
        "cwd is pane-scoped and survives the swap (§1.4)"
    );
    assert_eq!(
        after["panes"]["p1"]["agent"], "claude",
        "the entry was replaced, not removed"
    );
    fake.stop();
}
