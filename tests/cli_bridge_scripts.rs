use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PAYLOAD: &str = r#"{"session_id":"s1","workspace":{"current_dir":"/tmp"}}"#;

fn chmod_755(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn generate(dir: &Path, aw: &str, socket: &str, downstream: &str) -> PathBuf {
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
        .args(["generate-scripts", "--bin-dir"])
        .arg(dir)
        .args([
            "--herdr-agent-watcher",
            aw,
            "--socket",
            socket,
            "--downstream",
            downstream,
        ])
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir.join("statusline.sh")
}

fn run(command: &str, pane: Option<&str>, env: &[(&str, &str)]) -> (String, String, bool) {
    let mut child = Command::new("sh");
    child
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match pane {
        Some(pane) => {
            child.env("HERDR_PANE_ID", pane);
        }
        None => {
            child.env_remove("HERDR_PANE_ID");
        }
    }
    for (key, value) in env {
        child.env(key, value);
    }
    let mut child = child.spawn().expect("spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(PAYLOAD.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

#[test]
fn outside_herdr_the_script_writes_nothing_and_still_renders() {
    let dir = tempfile::tempdir().unwrap();
    let script = generate(
        dir.path(),
        "/nonexistent/aw",
        "/tmp/absent.sock",
        "printf USER",
    );
    let (stdout, _, ok) = run(&format!("'{}'", script.display()), None, &[]);
    assert!(ok);
    assert_eq!(stdout, "USER");
}

#[test]
fn a_missing_binary_still_renders_the_users_line() {
    let dir = tempfile::tempdir().unwrap();
    let script = generate(
        dir.path(),
        "/nonexistent/aw",
        "/tmp/absent.sock",
        "printf USER",
    );
    let (stdout, _, ok) = run(&format!("'{}'", script.display()), Some("w1:p1"), &[]);
    assert!(ok);
    assert_eq!(stdout, "USER");
}

#[test]
fn writer_output_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let noisy = dir.path().join("noisy");
    std::fs::write(
        &noisy,
        "#!/usr/bin/env bash\necho STDOUT-NOISE\necho STDERR-NOISE >&2\n",
    )
    .unwrap();
    chmod_755(&noisy);
    let script = generate(
        dir.path(),
        &noisy.to_string_lossy(),
        "/tmp/absent.sock",
        "printf USER",
    );
    let (stdout, stderr, ok) = run(&format!("'{}'", script.display()), Some("w1:p1"), &[]);
    assert!(ok);
    assert_eq!(stdout, "USER");
    assert!(!stderr.contains("STDERR-NOISE"));
}

#[test]
fn inherited_errexit_and_nounset_do_not_abort_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("profile.sh");
    std::fs::write(&profile, "set -u\nset -e\n").unwrap();
    let script = generate(
        dir.path(),
        "/nonexistent/aw",
        "/tmp/absent.sock",
        "printf USER",
    );
    let (stdout, _, ok) = run(
        &format!("'{}'", script.display()),
        None,
        &[("BASH_ENV", &profile.to_string_lossy())],
    );
    assert!(ok);
    assert_eq!(stdout, "USER");
}

#[test]
fn argument_downstream_wins_and_survives_as_one_argument() {
    let dir = tempfile::tempdir().unwrap();
    let script = generate(
        dir.path(),
        "/nonexistent/aw",
        "/tmp/absent.sock",
        "printf BAKED",
    );
    let command = format!("'{}' -- 'printf %s MULTI-WORD-OK'", script.display());
    let (stdout, _, ok) = run(&command, None, &[]);
    assert!(ok);
    assert_eq!(stdout, "MULTI-WORD-OK");
}

#[test]
fn downstream_quote_and_real_newline_survive() {
    let dir = tempfile::tempdir().unwrap();
    let script = generate(
        dir.path(),
        "/nonexistent/aw",
        "/tmp/absent.sock",
        "printf 'it'\\''s\nfine'",
    );
    let (stdout, _, ok) = run(&format!("'{}'", script.display()), None, &[]);
    assert!(ok);
    assert_eq!(stdout, "it's\nfine");
}

#[test]
fn daemon_answer_routes_payload_to_bound_file() {
    use std::io::{BufRead, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("status.json");
    let socket = dir.path().join("state.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let reply = target.to_string_lossy().to_string();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut line = String::new();
        let cloned = stream.try_clone().unwrap();
        let _ = BufReader::new(cloned).read_line(&mut line);
        let body = serde_json::json!({"version": 2, "path": reply});
        let _ = writeln!(stream, "{body}");
    });
    let script = generate(
        dir.path(),
        env!("CARGO_BIN_EXE_herdr-agent-watcher"),
        &socket.to_string_lossy(),
        "printf USER",
    );
    let (stdout, _, ok) = run(&format!("'{}'", script.display()), Some("w1:p1"), &[]);
    assert!(ok);
    assert_eq!(stdout, "USER");
    assert!(std::fs::read_to_string(target).unwrap().contains("s1"));
}

#[test]
fn attention_script_exits_zero_when_writer_fails() {
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), "/nonexistent/aw", "/tmp/absent.sock", "");
    let attention = dir.path().join("attention.sh");
    let (_, _, ok) = run(
        &format!("'{}' Stop append", attention.display()),
        Some("w1:p1"),
        &[],
    );
    assert!(ok);
}

/// The generated script's continuation lines carried a stray `+` for two
/// milestones, which arrived at the writer as an extra positional argument.
/// It happened to be tolerated, so nothing ever failed — assert on the argv
/// the writer actually receives rather than on the text of the script.
#[test]
fn the_writer_is_invoked_with_exactly_the_arguments_it_expects() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let fake = dir.path().join("fake-aw");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let script = generate(
        dir.path(),
        &fake.to_string_lossy(),
        "/tmp/probe.sock",
        "printf DOWNSTREAM",
    );
    chmod_755(&script);
    let (stdout, _, ok) = run(&script.to_string_lossy(), Some("w1:p1"), &[]);
    assert!(ok);
    assert_eq!(stdout, "DOWNSTREAM", "the downstream must still run");

    let argv = std::fs::read_to_string(&log).expect("the writer ran");
    assert_eq!(
        argv.trim(),
        "claude-bridge --write --pane w1:p1 --socket /tmp/probe.sock"
    );
}

#[test]
fn the_attention_hook_is_invoked_with_exactly_its_arguments() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let fake = dir.path().join("fake-aw");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    generate(dir.path(), &fake.to_string_lossy(), "/tmp/probe.sock", "");
    let attention = dir.path().join("attention.sh");
    chmod_755(&attention);
    let (_, _, ok) = run(
        &format!("{} Stop append", attention.to_string_lossy()),
        Some("w1:p1"),
        &[],
    );
    assert!(ok);

    let argv = std::fs::read_to_string(&log).expect("the hook ran");
    assert_eq!(
        argv.trim(),
        "claude-bridge --write-attention --pane w1:p1 --socket /tmp/probe.sock \
         --event Stop --mode append"
    );
}
