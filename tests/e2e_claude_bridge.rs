//! The test that would have failed before M0a-6: a realistic Claude Code payload
//! through the GENERATED script must reach the store as live metrics.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn a_realistic_payload_through_the_generated_script_produces_metrics() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    // HOME is redirected: `claude-bridge` reads ~/.claude/settings.json to chain an
    // existing status line, and the generated script would then EXECUTE the
    // developer's real one. A test that runs whatever is on the machine is neither
    // hermetic nor safe.
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let settings = String::from_utf8(
        Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
            .args(["claude-bridge", "--pane", "w2:p1"])
            .arg("--state-dir")
            .arg(&state)
            .arg("--cwd")
            .arg(&cwd)
            .env("HOME", &home)
            .output()
            .expect("run claude-bridge")
            .stdout,
    )
    .unwrap();
    let settings_path = std::path::PathBuf::from(settings.trim());
    assert!(
        settings_path.exists(),
        "claude-bridge printed a path that does not exist"
    );

    // The 15-key shape Claude Code actually sends, trimmed to what the adapter reads.
    let payload = r#"{"session_id":"s1","transcript_path":"/tmp/t.jsonl",
      "model":{"id":"claude-opus-5[1m]","display_name":"Opus 5 (1M context)"},
      "context_window":{"used_percentage":44.16,"context_window_size":1000000,
        "total_input_tokens":441625,"total_output_tokens":811,
        "current_usage":{"input_tokens":2,"output_tokens":811,
          "cache_creation_input_tokens":357,"cache_read_input_tokens":441266}},
      "cost":{"total_cost_usd":1.25}}"#;

    let statusline = settings_path.with_file_name("statusline.sh");
    let mut child = Command::new(&statusline)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());

    let status_path = settings_path.with_file_name("status.json");
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&status_path).unwrap()).unwrap();
    assert_eq!(written["context_window"]["context_window_size"], 1000000);
    assert_eq!(written["cost"]["total_cost_usd"], 1.25);

    // And the adapter turns that file into the event the store consumes.
    // Verified signature: `parse_statusline(session_id: &str, json: &str)
    // -> Result<ParsedStatusline, String>`, and `ParsedStatusline` exposes one
    // field, `event: AgentStatusEvent`. The DTOs beneath it are crate-private on
    // purpose, so assertions go through the event.
    let parsed = herdr_agent_watcher::agent::adapter::claude_code::statusline::parse_statusline(
        "s1",
        &std::fs::read_to_string(&status_path).unwrap(),
    )
    .expect("adapter parses the generated file");
    let event = parsed.event;
    assert_eq!(event.context_window.context_window_size, 1000000);
    assert!(
        event.context_window.used_percentage.is_some(),
        "the percentage the CONTEXT gauge needs"
    );
    assert!(
        event.context_window.current_usage.is_some(),
        "the buckets the CACHE gauge needs"
    );
    assert_eq!(
        event.cost.total_cost_usd,
        Some(1.25),
        "COST is the third metric §7 names, and nothing else in this plan covers it"
    );

    // ...and through the store, which is what the card actually reads. Anything
    // short of this proves the file was written, not that the sidebar can show it.
    let store = herdr_agent_watcher::daemon::store::TelemetryStore::default();
    store.set_agent("w2:p1", "claude");
    store.record(
        "w2:p1",
        "agent-status",
        serde_json::to_value(&event).expect("event serialises"),
    );
    let pane = store.snapshot().1["w2:p1"].clone();
    let status = pane.status.expect("status reached the store");
    assert_eq!(status["contextWindow"]["contextWindowSize"], 1000000);
    assert!(
        status["contextWindow"]["currentUsage"]["cacheReadInputTokens"]
            .as_u64()
            .is_some()
    );
    assert_eq!(status["cost"]["totalCostUsd"], 1.25);
}
