pub mod fake_herdr;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

pub struct FakeHerdr {
    pub socket_path: PathBuf,
    recorded: Arc<Mutex<Vec<serde_json::Value>>>,
    panes: Arc<Mutex<serde_json::Value>>,
    shell_pid: Arc<Mutex<u64>>,
    listener_thread: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl FakeHerdr {
    pub fn start(dir: &Path) -> Self {
        let socket_path = dir.join("fake-herdr.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind fake herdr socket");
        listener.set_nonblocking(true).unwrap();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let panes = Arc::new(Mutex::new(serde_json::json!({ "panes": [] })));
        let shell_pid = Arc::new(Mutex::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (requests, pane_response, process_pid, stop) = (
            recorded.clone(),
            panes.clone(),
            shell_pid.clone(),
            shutdown.clone(),
        );
        let listener_thread = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // On BSD and macOS an accepted socket inherits the
                        // listener's `O_NONBLOCK`, so a client that has
                        // connected but not yet written makes `read_line`
                        // answer `WouldBlock` -- which the arm below reads as
                        // a dead client and drops the request. The timeout is
                        // what keeps a genuinely stuck one from holding the
                        // loop instead.
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let mut line = String::new();
                        if BufReader::new(stream.try_clone().unwrap())
                            .read_line(&mut line)
                            .is_err()
                            || line.is_empty()
                        {
                            continue;
                        }
                        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                        requests.lock().unwrap().push(request.clone());
                        let id = request["id"].clone();
                        let response = if !request["params"].is_object() {
                            serde_json::json!({
                                "id": id,
                                "error": {
                                    "code": "invalid_params",
                                    "message": "params object required",
                                },
                            })
                        } else {
                            let result = match request["method"].as_str().unwrap_or_default() {
                                "pane.list" => pane_response.lock().unwrap().clone(),
                                "pane.process_info" => serde_json::json!({
                                    "type": "pane_process_info",
                                    "process_info": { "shell_pid": *process_pid.lock().unwrap() },
                                }),
                                _ => serde_json::json!({ "type": "ok" }),
                            };
                            serde_json::json!({ "id": id, "result": result })
                        };
                        let mut writer = stream;
                        let _ =
                            writer.write_all(serde_json::to_string(&response).unwrap().as_bytes());
                        let _ = writer.write_all(b"\n");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            socket_path,
            recorded,
            panes,
            shell_pid,
            listener_thread: Some(listener_thread),
            shutdown,
        }
    }

    pub fn set_panes(&self, panes: serde_json::Value) {
        *self.panes.lock().unwrap() = serde_json::json!({ "panes": panes });
    }

    pub fn set_shell_pid(&self, pid: u32) {
        *self.shell_pid.lock().unwrap() = u64::from(pid);
    }

    pub fn calls_named(&self, method: &str) -> Vec<serde_json::Value> {
        self.recorded
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == method)
            .cloned()
            .collect()
    }

    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.socket_path);
        if let Some(thread) = self.listener_thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn wait_for(mut check: impl FnMut() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for condition");
}

pub fn state_snapshot(socket: &Path) -> Option<serde_json::Value> {
    let mut client = std::os::unix::net::UnixStream::connect(socket).ok()?;
    client.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    client.write_all(b"{\"method\":\"snapshot\"}\n").ok()?;
    let mut line = String::new();
    BufReader::new(client).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

pub fn write_claude_fixture(
    home: &Path,
    state: &Path,
    cwd: &Path,
    pane_id: &str,
    session_id: &str,
) {
    let transcript_dir = home.join(".claude/projects/demo");
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let transcript_path = transcript_dir.join(format!("{session_id}.jsonl"));
    std::fs::write(
        &transcript_path,
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{session_id}\",\"timestamp\":\"2026-08-10T00:00:00Z\",\"message\":{{\"content\":\"demo prompt\"}},\"cwd\":{}}}\n",
            serde_json::to_string(&cwd.to_string_lossy()).unwrap(),
        ),
    )
    .unwrap();

    let basename = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace")
        .to_ascii_lowercase();
    let hash: String = Sha256::digest(cwd.to_string_lossy().as_bytes())
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let status_dir = state
        .join("runtime/workspaces")
        .join(format!("{basename}-{hash}"))
        .join("sessions")
        .join(pane_id);
    std::fs::create_dir_all(&status_dir).unwrap();
    std::fs::write(
        status_dir.join("status.json"),
        serde_json::to_vec(&serde_json::json!({
            "session_id": session_id,
            "transcript_path": transcript_path,
            "model": { "id": "claude-demo", "display_name": "Claude Demo" },
            "context_window": { "used_percentage": 12.5 },
            "cost": { "total_cost_usd": 0.01 },
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Mirrors `src/agent/adapter/codex/mod.rs::seed_codex_home_with_thread`.
pub fn write_codex_fixture(home: &Path, cwd: &Path, pid: u32) {
    let codex_home = home.join(".codex");
    let rollout_path = codex_home
        .join("sessions/2026/08/10")
        .join(format!("rollout-{pid}.jsonl"));
    std::fs::create_dir_all(rollout_path.parent().unwrap()).unwrap();
    std::fs::write(
        &rollout_path,
        include_str!("../fixtures/codex/rollout-minimal.jsonl"),
    )
    .unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap();
    let fresh_secs = now.as_secs() as i64 + 5;

    let logs = Connection::open(codex_home.join("logs.sqlite")).unwrap();
    logs.execute_batch(
        "CREATE TABLE logs (
            id INTEGER PRIMARY KEY,
            ts INTEGER NOT NULL,
            ts_nanos INTEGER NOT NULL,
            level TEXT,
            target TEXT,
            process_uuid TEXT NOT NULL,
            thread_id TEXT
        );
        CREATE INDEX idx_logs_ts ON logs(ts DESC, ts_nanos DESC, id DESC);",
    )
    .unwrap();
    logs.execute(
        "INSERT INTO logs (ts, ts_nanos, level, target, process_uuid, thread_id)
         VALUES (?1, 0, 'INFO', 'e2e', ?2, 'tid-e2e')",
        params![fresh_secs, format!("pid:{pid}:e2e")],
    )
    .unwrap();

    let state = Connection::open(codex_home.join("state.sqlite")).unwrap();
    state
        .execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                cwd TEXT,
                updated_at_ms INTEGER NOT NULL
            );",
        )
        .unwrap();
    state
        .execute(
            "INSERT INTO threads (id, rollout_path, cwd, updated_at_ms)
             VALUES ('tid-e2e', ?1, ?2, ?3)",
            params![
                rollout_path.to_str().unwrap(),
                cwd.to_str().unwrap(),
                now.as_millis() as i64 + 5_000,
            ],
        )
        .unwrap();
}

/// Mirrors the fixture layouts in `src/agent/adapter/kimi/transcript.rs`.
pub fn write_kimi_fixture(kimi_home: &Path, cwd: &Path, session_id: &str) {
    let session_dir = kimi_home.join("sessions").join(session_id);
    let main_dir = session_dir.join("agents/main");
    std::fs::create_dir_all(&main_dir).unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let sample = include_str!("../../src/agent/adapter/kimi/fixtures/sample_wire.jsonl");
    let mut lines = sample.lines();
    let mut first: serde_json::Value =
        serde_json::from_str(lines.next().expect("sample wire has lines")).unwrap();
    first["created_at"] = serde_json::json!(now_ms);
    std::fs::write(
        main_dir.join("wire.jsonl"),
        format!("{first}\n{}\n", lines.collect::<Vec<_>>().join("\n")),
    )
    .unwrap();
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::json!({
            "agents": { "main": { "homedir": main_dir.to_string_lossy(), "type": "main" } }
        })
        .to_string(),
    )
    .unwrap();
    let index_row = serde_json::json!({
        "sessionId": session_id,
        "sessionDir": session_dir.to_string_lossy(),
        "workDir": cwd.to_string_lossy(),
    });
    std::fs::write(
        kimi_home.join("session_index.jsonl"),
        format!("{index_row}\n"),
    )
    .unwrap();
}

/// Mirrors the bridge fixtures in `src/agent/adapter/opencode/locator.rs`.
pub fn write_opencode_fixture(bridge_dir: &Path, cwd: &Path, session_id: &str, pid: u32) {
    std::fs::create_dir_all(bridge_dir).unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let row = serde_json::json!({
        "sessionID": session_id,
        "pid": pid,
        "directory": cwd.to_string_lossy(),
        "slug": "e2e-otter",
        "time": now_ms,
    });
    std::fs::write(bridge_dir.join("index.jsonl"), format!("{row}\n")).unwrap();
    std::fs::write(
        bridge_dir.join(format!("{session_id}.jsonl")),
        include_str!("../../src/agent/adapter/opencode/fixtures/sample_bridge.jsonl"),
    )
    .unwrap();
}
