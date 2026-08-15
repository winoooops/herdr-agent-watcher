mod support;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime};

use support::wait_for;

struct ProcessGuard(Child);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Requires herdr >= 0.8.0 on PATH. Run with:
/// `cargo test --test e2e_real_herdr -- --ignored`
#[test]
#[ignore]
fn isolated_real_herdr_serves_the_daemon() {
    let real_session = dirs::home_dir().map(|home| home.join(".config/herdr/session.json"));
    let real_session_mtime = real_session.as_deref().and_then(modified);
    // macOS limits Unix socket paths to 103 bytes; its default tempfile root is long.
    let tmp = tempfile::Builder::new()
        .prefix("vf-e2e-")
        .tempdir_in("/tmp")
        .unwrap();
    let config = tmp.path().join("xdg-config");
    let state_dir = tmp.path().join("xdg-state");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let session = format!("vf-e2e-{}", std::process::id());

    let _herdr = ProcessGuard(
        Command::new("herdr")
            .args(["--session", &session, "server"])
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_STATE_HOME", &state_dir)
            .env("HOME", &home)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn isolated herdr server"),
    );

    let socket = config
        .join("herdr/sessions")
        .join(&session)
        .join("herdr.sock");
    wait_for(|| socket.exists(), Duration::from_secs(10));

    std::env::set_var("HERDR_SOCKET_PATH", &socket);
    let probe = herdr_agent_watcher::herdr::client::HerdrClient::from_env();
    wait_for(
        || match probe.pane_list() {
            Ok(_) => true,
            Err(error) => {
                eprintln!("pane.list probe failed: {error}");
                false
            }
        },
        Duration::from_secs(10),
    );

    let plugin_state = tmp.path().join("plugin-state");
    let mut daemon = ProcessGuard(
        Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
            .arg("daemon")
            .env("HERDR_SOCKET_PATH", &socket)
            .env("HERDR_PLUGIN_STATE_DIR", &plugin_state)
            .env("HOME", &home)
            .env("AGENT_WATCHER_INTERVAL_MS", "50")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || plugin_state.join("herdr-agent-watcher.lock").exists(),
        Duration::from_secs(5),
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "daemon exited during reconciliation"
    );
    assert_eq!(
        real_session.as_deref().and_then(modified),
        real_session_mtime,
        "isolated test changed the user's real Herdr session"
    );
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}
