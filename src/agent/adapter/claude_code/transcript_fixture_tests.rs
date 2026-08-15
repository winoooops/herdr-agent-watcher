use std::sync::Arc;
use std::time::Duration;

use crate::agent::adapter::base::TranscriptState;
use crate::agent::adapter::claude_code::ClaudeCodeAdapter;
use crate::agent::adapter::traits::TranscriptStreamer;
use crate::runtime::FakeEventSink;

#[test]
fn transcript_emits_turn_events_for_real_user_prompts_only() {
    let sink = Arc::new(FakeEventSink::new());

    let tmp = tempfile::tempdir().expect("temp transcript dir");
    let transcript_path = tmp.path().join("turns.jsonl");
    // Six-line fixture covers five message shapes:
    //   1. plain-string user prompt   -> emits turn 1
    //   2. assistant tool_use         -> no event
    //   3. user array of tool_result  -> no event (tool return, not a prompt)
    //   4. user array of text block   -> emits turn 2
    //   5. assistant tool_use         -> no event (seeds in_flight for #6)
    //   6. user array mixing tool_result + text (mixed content) -> emits turn 3
    std::fs::write(
        &transcript_path,
        concat!(
            r#"{"type":"user","timestamp":"2026-04-28T11:00:00.000Z","message":{"content":"first prompt"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-04-28T11:00:01.000Z","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"src/App.tsx"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-04-28T11:00:02.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":false,"content":"ok"}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-04-28T11:00:03.000Z","message":{"content":[{"type":"text","text":"second prompt"}]}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-04-28T11:00:04.000Z","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file_path":"src/App.tsx"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-04-28T11:00:05.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_2","is_error":false,"content":"ok"},{"type":"text","text":"follow-up"}]}}"#,
            "\n",
        ),
    )
    .expect("write transcript fixture");

    let state = TranscriptState::new();
    let adapter: Arc<dyn TranscriptStreamer> = Arc::new(ClaudeCodeAdapter);
    state
        .start_or_replace(
            adapter,
            sink.clone(),
            "session-turns".to_string(),
            transcript_path,
            None,
            None,
            None,
        )
        .expect("start watcher");

    // The whole fixture replays before catch-up, so the per-prompt
    // `agent-turn` events are coalesced into one `agent-replay-summary` at the
    // replay→live boundary. num_turns still accumulates per real prompt; the
    // summary carries the final count (3). The per-shape prompt-counting
    // contract is covered by the is_user_prompt unit tests in transcript.rs.
    assert!(
        sink.wait_for_count("agent-replay-summary", 1, Duration::from_secs(5)),
        "expected one replay-summary at the replay→live boundary",
    );
    state.stop("session-turns").ok();

    assert_eq!(
        sink.count("agent-turn"),
        0,
        "replay turns are coalesced into the summary, not emitted individually",
    );

    let summaries: Vec<_> = sink
        .recorded()
        .into_iter()
        .filter(|(event, _)| event == "agent-replay-summary")
        .collect();
    assert_eq!(summaries.len(), 1);
    // Three real user prompts (plain string, text block, and the mixed
    // tool_result + text block whose text has non-whitespace content).
    assert_eq!(summaries[0].1["numTurns"], 3);
}

#[test]
fn vitest_pass_fixture_emits_one_test_run() {
    let sink = Arc::new(FakeEventSink::new());

    let state = TranscriptState::new();
    let adapter: Arc<dyn TranscriptStreamer> = Arc::new(ClaudeCodeAdapter);
    let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcript_vitest_pass.jsonl");

    // Pass a valid cwd: process_tool_result skips test-run snapshots when cwd
    // is None to avoid resolving file groups against the wrong directory. The
    // fixture has no per-file rows so a temp cwd is fine.
    let cwd = tempfile::tempdir().expect("temp cwd");

    state
        .start_or_replace(
            adapter,
            sink.clone(),
            "session-fixture".to_string(),
            fixture_path,
            Some(cwd.path().to_path_buf()),
            None,
            None,
        )
        .expect("start watcher");

    assert!(
        sink.wait_for_count("test-run", 1, Duration::from_secs(5)),
        "expected exactly one test-run event",
    );
    state.stop("session-fixture").ok();

    let events: Vec<_> = sink
        .recorded()
        .into_iter()
        .filter(|(event, _)| event == "test-run")
        .collect();
    assert_eq!(events.len(), 1, "expected exactly one test-run event");
    let payload = &events[0].1;
    assert_eq!(payload["runner"], "vitest");
    assert_eq!(payload["summary"]["passed"], 3);
    assert_eq!(payload["summary"]["total"], 3);
    assert_eq!(payload["status"], "pass");
}

#[test]
fn cargo_mixed_fixture_emits_test_run_with_groups() {
    let sink = Arc::new(FakeEventSink::new());

    let state = TranscriptState::new();
    let adapter: Arc<dyn TranscriptStreamer> = Arc::new(ClaudeCodeAdapter);
    let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcript_cargo_mixed.jsonl");

    // Pass a valid cwd: process_tool_result skips the snapshot entirely when
    // cwd is None. A temp dir is enough; cargo groups always have path: None,
    // so the rest of the assertions hold.
    let cwd = tempfile::tempdir().expect("temp cwd");

    state
        .start_or_replace(
            adapter,
            sink.clone(),
            "session-cargo".to_string(),
            fixture_path,
            Some(cwd.path().to_path_buf()),
            None,
            None,
        )
        .expect("start watcher");

    assert!(
        sink.wait_for_count("test-run", 1, Duration::from_secs(5)),
        "expected exactly one test-run event",
    );
    state.stop("session-cargo").ok();

    let events: Vec<_> = sink
        .recorded()
        .into_iter()
        .filter(|(event, _)| event == "test-run")
        .collect();
    assert_eq!(events.len(), 1);
    let payload = &events[0].1;
    assert_eq!(payload["runner"], "cargo");
    assert_eq!(payload["summary"]["passed"], 1);
    assert_eq!(payload["summary"]["failed"], 1);
    assert_eq!(payload["summary"]["skipped"], 1);
    assert_eq!(payload["status"], "fail");
    assert!(payload["summary"]["groups"]
        .to_string()
        .contains(r#""kind":"module""#));
    assert!(payload["summary"]["groups"]
        .to_string()
        .contains(r#""path":null"#));
}

#[test]
fn transcript_emits_agent_cwd_event_on_each_cwd_transition() {
    // Real-world reproduction: Claude Code's built-in `EnterWorktree` tool
    // switches the agent's working directory WITHOUT mutating the
    // interactive shell. Every JSONL entry carries the agent's current cwd
    // at the top level — that's the structured signal vimeflow needs to
    // mirror into pane.cwd. The watcher must:
    //   - emit on the first observed cwd (transition from None)
    //   - emit again when cwd changes mid-session (e.g. EnterWorktree)
    //   - NOT re-emit when consecutive lines share the same cwd (dedup)
    let sink = Arc::new(FakeEventSink::new());

    let tmp = tempfile::tempdir().expect("temp transcript dir");
    let transcript_path = tmp.path().join("cwd.jsonl");
    std::fs::write(
        &transcript_path,
        concat!(
            // Line 1: starting cwd in the regression worktree.
            r#"{"type":"user","timestamp":"2026-05-21T17:49:44.801Z","message":{"content":"create a dummy worktree and enter it"},"cwd":"/home/will/projects/vimeflow-agent-cwd-regression"}"#,
            "\n",
            // Line 2: same cwd — no new emit.
            r#"{"type":"assistant","timestamp":"2026-05-21T17:49:53.818Z","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"rules/CLAUDE.md"}}]},"cwd":"/home/will/projects/vimeflow-agent-cwd-regression"}"#,
            "\n",
            // Line 3: EnterWorktree result — cwd is now the new worktree.
            r#"{"type":"assistant","timestamp":"2026-05-21T17:50:18.255Z","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"EnterWorktree","input":{"path":"/home/will/projects/vimeflow/.claude/worktrees/dummy"}}]},"cwd":"/home/will/projects/vimeflow/.claude/worktrees/dummy"}"#,
            "\n",
            // Line 4: subsequent tool calls in the new worktree — no new emit.
            r#"{"type":"assistant","timestamp":"2026-05-21T17:50:30.000Z","message":{"content":[{"type":"tool_use","id":"toolu_3","name":"Bash","input":{"command":"ls"}}]},"cwd":"/home/will/projects/vimeflow/.claude/worktrees/dummy"}"#,
            "\n",
        ),
    )
    .expect("write transcript fixture");

    let state = TranscriptState::new();
    let adapter: Arc<dyn TranscriptStreamer> = Arc::new(ClaudeCodeAdapter);
    state
        .start_or_replace(
            adapter,
            sink.clone(),
            "session-cwd".to_string(),
            transcript_path,
            None,
            None,
            None,
        )
        .expect("start watcher");

    // The whole fixture replays before catch-up, so per-line `agent-cwd`
    // transitions are coalesced into one `agent-replay-summary` carrying the
    // FINAL cwd (the worktree the agent ended in). last_cwd still tracks every
    // transition during replay; the cwd transition + dedup semantics are
    // covered by the process_line_* unit tests in transcript.rs.
    assert!(
        sink.wait_for_count("agent-replay-summary", 1, Duration::from_secs(5)),
        "expected one replay-summary at the replay→live boundary",
    );
    state.stop("session-cwd").ok();

    assert_eq!(
        sink.count("agent-cwd"),
        0,
        "replay cwd transitions are coalesced into the summary, not emitted individually",
    );

    let summaries: Vec<_> = sink
        .recorded()
        .into_iter()
        .filter(|(event, _)| event == "agent-replay-summary")
        .collect();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].1["sessionId"], "session-cwd");
    assert_eq!(
        summaries[0].1["cwd"], "/home/will/projects/vimeflow/.claude/worktrees/dummy",
        "summary carries the final cwd after all replay transitions",
    );
}

#[test]
fn replay_emits_only_latest_snapshot() {
    let sink = Arc::new(FakeEventSink::new());

    let state = TranscriptState::new();
    let adapter: Arc<dyn TranscriptStreamer> = Arc::new(ClaudeCodeAdapter);
    let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcript_vitest_replay.jsonl");

    // Pass a valid cwd: process_tool_result skips test-run snapshots when cwd
    // is None. Replay-batching behaviour is independent of cwd.
    let cwd = tempfile::tempdir().expect("temp cwd");

    state
        .start_or_replace(
            adapter,
            sink.clone(),
            "session-replay".to_string(),
            fixture_path,
            Some(cwd.path().to_path_buf()),
            None,
            None,
        )
        .expect("start watcher");

    assert!(
        sink.wait_for_count("test-run", 1, Duration::from_secs(5)),
        "expected exactly one test-run event",
    );
    state.stop("session-replay").ok();

    let events: Vec<_> = sink
        .recorded()
        .into_iter()
        .filter(|(event, _)| event == "test-run")
        .collect();
    // 3 historical runs in the fixture, but replay batching collapses to 1
    // emit containing the latest run (passed=3).
    assert_eq!(events.len(), 1, "expected exactly one emit after replay");
    assert_eq!(events[0].1["summary"]["passed"], 3);
}

/// Append `\n`-terminated lines to an existing transcript file (live-tail tests).
fn append_lines(path: &std::path::Path, lines: &[&str]) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open transcript for append");
    for line in lines {
        writeln!(file, "{line}").expect("append transcript line");
    }
}

/// Phase 0 (Task 0.1): pin the replay→live boundary via `test-run`.
///
/// Characterization test — it must PASS against current code. Replay collapses
/// the fixture's 3 test-run pairs to one `test-run` at `finish_replay`; a pair
/// appended *after* catch-up emits a second, live `test-run`. A sentinel
/// `agent-turn` is the drain barrier: observing it proves the live pair was
/// processed before the exact-count assertion (a bare `wait_for_count(test-run,
/// N)` only proves "≥N arrived"). The fixture is copied into a temp file so the
/// live appends never mutate the checked-in fixture.
#[test]
fn replay_collapses_then_live_test_run_emits() {
    let sink = Arc::new(FakeEventSink::new());
    let tmp = tempfile::tempdir().expect("temp transcript dir");
    let transcript_path = tmp.path().join("replay.jsonl");

    let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcript_vitest_replay.jsonl");
    let fixture = std::fs::read_to_string(&fixture_path).expect("read replay fixture");
    std::fs::write(&transcript_path, fixture).expect("seed temp transcript");

    let state = TranscriptState::new();
    let adapter: Arc<dyn TranscriptStreamer> = Arc::new(ClaudeCodeAdapter);
    state
        .start_or_replace(
            adapter,
            sink.clone(),
            "session-replay-live".to_string(),
            transcript_path.clone(),
            // Some(cwd) REQUIRED: process_tool_result skips test-run when cwd is None.
            Some(tmp.path().to_path_buf()),
            None,
            None,
        )
        .expect("start watcher");

    // (a) Catch-up barrier: replay buffers the 3 snapshots; finish_replay emits
    //     exactly one collapsed test-run (passed=3).
    assert!(
        sink.wait_for_count("test-run", 1, Duration::from_secs(5)),
        "replay should emit one collapsed test-run",
    );

    // (b) Append a NEW live test-run pair (vitest Bash tool_use + tool_result),
    //     mirroring the fixture shapes (passed=4).
    append_lines(
        &transcript_path,
        &[
            r#"{"type":"assistant","timestamp":"2026-04-28T11:03:00.000Z","message":{"content":[{"type":"tool_use","id":"r4","name":"Bash","input":{"command":"vitest run"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-04-28T11:03:01.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"r4","is_error":false,"content":"     Tests  4 passed (4)\n"}]}}"#,
        ],
    );

    // (c) Drain barrier: a sentinel user prompt → agent-turn (baseline-relative).
    //     Observing it proves every prior line — incl. the live pair — was read.
    let turns_before = sink.count("agent-turn");
    append_lines(
        &transcript_path,
        &[
            r#"{"type":"user","timestamp":"2026-04-28T11:03:02.000Z","message":{"content":"sentinel prompt"}}"#,
        ],
    );
    assert!(
        sink.wait_for_count("agent-turn", turns_before + 1, Duration::from_secs(5)),
        "sentinel agent-turn should drain past the live pair",
    );

    state.stop("session-replay-live").ok();

    // (d) Exactly two test-runs: 1 replay-collapsed (passed=3) + 1 live (passed=4).
    let runs: Vec<_> = sink
        .recorded()
        .into_iter()
        .filter(|(event, _)| event == "test-run")
        .collect();
    assert_eq!(runs.len(), 2, "1 replay-collapsed + 1 live");
    assert_eq!(
        runs[0].1["summary"]["passed"], 3,
        "collapsed replay = latest of 3"
    );
    assert_eq!(runs[1].1["summary"]["passed"], 4, "live pair");
}
