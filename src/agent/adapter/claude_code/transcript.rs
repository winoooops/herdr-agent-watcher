//! Transcript JSONL parser for Claude Code tool call tracking
//!
//! Tails a Claude Code transcript JSONL file and extracts activity events.
//! Emits `agent-tool-call` backend events for each tool call start/completion
//! and `agent-turn` events as real user prompts are observed.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use super::test_runners::emitter::TestRunEmitter;
use super::test_runners::matcher::{match_command, MatchedCommand};
use super::transcript_dto::{ClaudeToolResultDto, ClaudeToolUseDto, ClaudeTranscriptLineDto};
use crate::agent::adapter::base::{
    for_each_bounded_line, TranscriptDecoder, TranscriptHandle, TranscriptTailService,
};
use crate::agent::adapter::types::ValidateTranscriptError;
use crate::agent::events::{
    emit_agent_cwd, emit_agent_replay_summary, emit_agent_reply, emit_agent_session_title,
    emit_agent_tool_call, emit_agent_turn, emit_lifecycle_on_change, record_lifecycle,
    record_tool_call, ReplayActivity,
};
use crate::agent::reply::{extract_agent_reply, map_agent_reply_outcome};
use crate::agent::sanitize_title;
use crate::agent::types::{
    AgentCwdEvent, AgentPhase, AgentReplyEvent, AgentReviewEvent, AgentSessionTitleEvent,
    AgentToolCallEvent, AgentTurnEvent, TitleSource, ToolCallStatus,
};
use crate::runtime::EventSink;

/// Maximum length for the args summary string
const MAX_ARGS_LEN: usize = 1024;

const MAX_TOOL_RESULT_CONTENT_LEN: usize = 256 * 1024;
/// When truncating `tool_result` content that exceeds the cap, keep at
/// least this many bytes from the END of the content. Test-runner
/// parsers (`test_runners/cargo.rs`, `test_runners/vitest.rs`) look for
/// summary lines at the END of test output (`test result: ok. ...`,
/// `Tests N passed | M failed`, etc.). Head-only truncation would drop
/// those summary lines and cause large successful test runs to parse
/// as `None` → `maybe_build_snapshot` would skip the non-error case
/// → successful test runs vanish from the UI. Tail size chosen at
/// 64 KiB (~1/4 of cap) to comfortably hold a verbose test summary
/// plus failure stack traces (Codex review on PR #153, F15).
const TOOL_RESULT_TAIL_LEN: usize = 64 * 1024;
const TOOL_RESULT_TRUNCATED_MARKER: &str = "[output truncated]";

pub(crate) fn validate_transcript_path(
    transcript_path: &str,
) -> Result<PathBuf, ValidateTranscriptError> {
    if transcript_path.bytes().any(|b| b == 0) {
        return Err(ValidateTranscriptError::InvalidPath(
            "transcript path contains null byte".to_string(),
        ));
    }

    let path = PathBuf::from(transcript_path);
    let canonical = fs::canonicalize(&path).map_err(|e| {
        // Classify directly from the canonicalize failure where the
        // ErrorKind is authoritative (no second syscall = no TOCTOU
        // window). Only `PermissionDenied` is ambiguous: Windows can
        // return it for missing-but-unreadable parents, where the
        // `try_exists()` fallback is the authoritative classifier.
        // All other kinds (Interrupted, InvalidData, etc.) become
        // `Other` directly. Claude review on PR #153, F5 (cycle-2
        // narrowing followed by a cycle-2-retry tightening — codex
        // flagged that the previous "fall back for all non-NotFound"
        // shape only narrowed the race instead of eliminating it).
        let kind = e.kind();
        if kind == io::ErrorKind::NotFound {
            return ValidateTranscriptError::NotFound(path.clone());
        }
        if kind == io::ErrorKind::PermissionDenied {
            if let Ok(false) = path.try_exists() {
                return ValidateTranscriptError::NotFound(path.clone());
            }
        }
        ValidateTranscriptError::Other(format!(
            "invalid transcript path '{}': {}",
            transcript_path, e
        ))
    })?;

    if !canonical.is_file() {
        return Err(ValidateTranscriptError::NotAFile(canonical));
    }

    let home = dirs::home_dir().ok_or_else(|| {
        ValidateTranscriptError::Other("cannot determine home directory".to_string())
    })?;
    let claude_root = home.join(".claude");
    let claude_root = fs::canonicalize(&claude_root).map_err(|e| {
        ValidateTranscriptError::Other(format!(
            "cannot resolve Claude transcript root '{}': {}",
            claude_root.display(),
            e
        ))
    })?;

    if !canonical.starts_with(&claude_root) {
        return Err(ValidateTranscriptError::OutsideRoot {
            path: canonical,
            root: claude_root,
        });
    }

    Ok(canonical)
}

struct InFlightToolCall {
    started_at: Instant,
    started_at_iso: String,
    tool: String,
    args: String,
    is_test_file: bool,
    test_match: Option<MatchedCommand>,
}

type InFlightToolCalls = HashMap<String, InFlightToolCall>;

fn line_type(value: &Value) -> &str {
    value.get("type").and_then(Value::as_str).unwrap_or("")
}

fn tool_block_type(item: &Value) -> &str {
    line_type(item)
}

fn bash_command<'a>(name: &'a str, input: Option<&'a Value>) -> Option<&'a str> {
    if name != "Bash" {
        return None;
    }
    input?.get("command").and_then(Value::as_str)
}

fn tool_file_path<'a>(input: Option<&'a Value>) -> Option<&'a str> {
    input?.get("file_path").and_then(Value::as_str)
}

fn text_block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

fn text_block_text(block: &Value) -> Option<&str> {
    block.get("text").and_then(Value::as_str)
}

fn input_file_path(input: &Value) -> Option<&str> {
    input.get("file_path").and_then(Value::as_str)
}

fn input_command(input: &Value) -> Option<&str> {
    input.get("command").and_then(Value::as_str)
}

fn input_pattern(input: &Value) -> Option<&str> {
    input.get("pattern").and_then(Value::as_str)
}

/// Start tailing a transcript JSONL file.
/// Reads from the beginning to catch up on missed tool calls.
/// Emits `agent-tool-call` backend events for each tool call detected.
pub fn start_tailing(
    events: Arc<dyn EventSink>,
    session_id: String,
    transcript_path: PathBuf,
    cwd: Option<PathBuf>,
) -> Result<TranscriptHandle, String> {
    // Derive Claude's own agent_session_id from the transcript filename
    // stem. Each Claude Code session writes one JSONL file named
    // `<agent-session-uuid>.jsonl` under its `.claude/projects/.../` tree,
    // so the stem IS the id (PR #265 wiring; restored in PR #302 cycle 2
    // alongside the `ai-title` / `custom-title` arms it gates).
    let claude_agent_session_id = transcript_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "could not derive claude session id from transcript path: {}",
                transcript_path.display()
            )
        })?;

    let file = File::open(&transcript_path).map_err(|e| {
        format!(
            "Failed to open transcript: {}: {}",
            transcript_path.display(),
            e
        )
    })?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = stop_flag.clone();

    // Read from the beginning — each Claude Code session gets a unique JSONL
    // file, so all lines belong to the current session. Replay catches any
    // tool calls written before tailing started (the transcript file is often
    // created seconds after the statusline first reports its path).
    let decoder = ClaudeTranscriptDecoder::new(events, session_id, cwd, claude_agent_session_id);
    let service = TranscriptTailService::new(Box::new(decoder), "transcript");

    let join_handle = std::thread::spawn(move || {
        service.run(BufReader::new(file), stop_clone);
    });

    Ok(TranscriptHandle::new(stop_flag, join_handle))
}

pub(super) fn recover_replies(
    transcript_path: &Path,
    session_id: &str,
    nonces: &HashSet<String>,
) -> Result<Vec<AgentReplyEvent>, String> {
    let file = File::open(transcript_path).map_err(|e| {
        format!(
            "Failed to open Claude transcript for recovery: {}: {}",
            transcript_path.display(),
            e
        )
    })?;
    let mut recovered = Vec::new();
    let mut pending_nonces = nonces.clone();

    for_each_bounded_line(BufReader::new(file), "Claude transcript recovery", |line| {
        if pending_nonces.is_empty() {
            return;
        }
        let Ok(dto) = serde_json::from_str::<ClaudeTranscriptLineDto>(line) else {
            return;
        };
        if dto.line_type.as_deref() != Some("assistant") {
            return;
        }
        if let Some(event) = reply_event(&dto, session_id) {
            let Some(nonce) = event.nonce.as_ref() else {
                return;
            };
            if pending_nonces.remove(nonce) {
                recovered.push(event);
            }
        }
    })
    .map_err(|e| {
        format!(
            "Failed to read Claude transcript for recovery: {}: {}",
            transcript_path.display(),
            e
        )
    })?;

    Ok(recovered)
}

pub(super) fn recover_reviews(
    transcript_path: &Path,
    session_id: &str,
    nonces: &HashSet<String>,
) -> Result<Vec<AgentReviewEvent>, String> {
    let file = File::open(transcript_path).map_err(|e| {
        format!(
            "Failed to open Claude transcript for review recovery: {}: {}",
            transcript_path.display(),
            e
        )
    })?;
    let mut recovered = Vec::new();
    let mut pending_nonces = nonces.clone();

    for_each_bounded_line(BufReader::new(file), "Claude review recovery", |line| {
        if pending_nonces.is_empty() {
            return;
        }
        let Ok(dto) = serde_json::from_str::<ClaudeTranscriptLineDto>(line) else {
            return;
        };
        if dto.line_type.as_deref() != Some("assistant") {
            return;
        }
        if let Some(event) = review_event(&dto, session_id) {
            let Some(nonce) = event.nonce.as_ref() else {
                return;
            };
            if pending_nonces.remove(nonce) {
                recovered.push(event);
            }
        }
    })
    .map_err(|e| {
        format!(
            "Failed to read Claude transcript for review recovery: {}: {}",
            transcript_path.display(),
            e
        )
    })?;

    Ok(recovered)
}

/// Per-session Claude Code decoder: owns the in-flight tool-call map, turn
/// count, last-seen cwd, and the replay-aware emitter, and turns each complete
/// transcript line into `agent-*` events. Driven by [`TranscriptTailService`],
/// which owns the read/buffer/poll loop.
struct ClaudeTranscriptDecoder {
    events: Arc<dyn EventSink>,
    session_id: String,
    cwd: Option<PathBuf>,
    /// In-flight tool calls: tool_use_id -> call details.
    in_flight: InFlightToolCalls,
    num_turns: u32,
    /// Last agent-reported cwd. Tracked so consecutive lines that share the
    /// same `cwd` field don't re-emit. Initial `None` so the first observed
    /// cwd always fires a transition event.
    last_cwd: Option<String>,
    /// Replay-aware emitter — buffers test-run snapshots during the initial
    /// catch-up read and emits the latest one (only) on the first EOF. Once
    /// we're tailing live, every snapshot emits immediately.
    emitter: TestRunEmitter,
    /// Claude's own agent session id (the transcript file's stem). Used by
    /// the `ai-title` / `custom-title` arms in `process_line` to filter
    /// out title rows whose `sessionId` field belongs to a different
    /// Claude session — Claude writes title lines stamped with the
    /// originating session id, and a tail thread should only emit
    /// `agent-session-title` for ITS session (PR #302 cycle 2 F1+F2).
    claude_agent_session_id: String,
    /// Per-decoder dedup memo for the title-emit path: skip duplicate
    /// `ai-title` rows (Claude can write the same `aiTitle` value
    /// multiple times in a row); `custom-title` ALWAYS emits because
    /// `/rename` is a user-initiated event that must round-trip even if
    /// the title text hasn't changed (matches the pre-refactor
    /// `emit_title` dedup contract from PR #265).
    last_title_memo: Option<String>,
    /// Live agent-lifecycle de-dup slot; left untouched during replay.
    last_phase: Option<AgentPhase>,
    /// Settled phase accumulated silently during replay, flushed once at
    /// the replay->live boundary.
    replay_phase: Option<AgentPhase>,
    /// Tool-call/turn/cwd activity accumulated silently during replay; flushed
    /// once at the replay->live boundary as a single agent-replay-summary.
    replay_activity: ReplayActivity,
    /// One-shot guard: false during replay, true after the first on_caught_up.
    replay_done: bool,
}

impl ClaudeTranscriptDecoder {
    fn new(
        events: Arc<dyn EventSink>,
        session_id: String,
        cwd: Option<PathBuf>,
        claude_agent_session_id: String,
    ) -> Self {
        let emitter = TestRunEmitter::new(events.clone());
        Self {
            events,
            session_id,
            cwd,
            in_flight: HashMap::new(),
            num_turns: 0,
            last_cwd: None,
            emitter,
            claude_agent_session_id,
            last_title_memo: None,
            last_phase: None,
            replay_phase: None,
            replay_activity: ReplayActivity::default(),
            replay_done: false,
        }
    }
}

impl TranscriptDecoder for ClaudeTranscriptDecoder {
    fn decode_line(&mut self, line: &str) {
        process_line(
            line,
            &self.session_id,
            self.cwd.as_deref(),
            &self.events,
            &mut self.emitter,
            &mut self.in_flight,
            &mut self.num_turns,
            &mut self.last_cwd,
            &self.claude_agent_session_id,
            &mut self.last_title_memo,
            &mut self.last_phase,
            &mut self.replay_phase,
            &mut self.replay_activity,
            self.replay_done,
        );
    }

    /// First EOF marks the end of replay; subsequent EOFs are idempotent.
    /// After this, every submit emits live.
    fn on_caught_up(&mut self) {
        if !self.replay_done {
            self.replay_done = true;
            if let Some(phase) = self.replay_phase.take() {
                emit_lifecycle_on_change(
                    self.events.as_ref(),
                    &self.session_id,
                    &self.claude_agent_session_id,
                    &mut self.last_phase,
                    phase,
                );
            }
            for event in self.replay_activity.take_running() {
                if let Err(e) = emit_agent_tool_call(self.events.as_ref(), &event) {
                    log::warn!("Failed to emit agent-tool-call event: {}", e);
                }
            }
            // Flush the replay-accumulated tool-call/turn/cwd activity as one
            // summary, replacing the thousands of per-line events suppressed
            // during replay. Only emit if it carries something, to avoid a
            // noisy empty summary for a fresh session.
            let summary = std::mem::take(&mut self.replay_activity).into_summary(
                self.session_id.clone(),
                self.num_turns,
                self.last_cwd.clone(),
            );
            if summary.tool_call_total > 0 || summary.num_turns > 0 || summary.cwd.is_some() {
                if let Err(e) = emit_agent_replay_summary(self.events.as_ref(), &summary) {
                    log::warn!("Failed to emit agent-replay-summary event: {}", e);
                }
            }
        }
        self.emitter.finish_replay();
    }
}

/// Process a single JSONL line and emit events if it's a tool call or a
/// title row.
///
/// `claude_agent_session_id` is the agent's own session id (derived from
/// the transcript filename stem in `start_tailing`). `ai-title` /
/// `custom-title` lines carry a `sessionId` field stamped by Claude; the
/// arms below filter on equality so a tail thread only emits
/// `agent-session-title` for ITS session and never leaks another
/// session's title through this PTY. `last_title_memo` dedups consecutive
/// identical `ai-title` rows; `custom-title` bypasses the memo since
/// `/rename` is user-initiated and must round-trip even when text is
/// unchanged (PR #302 cycle 2 F1+F2 — restored from PR #265 after the
/// PR #287 `tail_loop` → `TranscriptTailService` extraction dropped
/// both arms along with the title helper and 8 covering tests).
#[allow(clippy::too_many_arguments)]
fn process_line(
    line: &str,
    session_id: &str,
    cwd: Option<&Path>,
    events: &Arc<dyn EventSink>,
    emitter: &mut TestRunEmitter,
    in_flight: &mut InFlightToolCalls,
    num_turns: &mut u32,
    last_cwd: &mut Option<String>,
    claude_agent_session_id: &str,
    last_title_memo: &mut Option<String>,
    last_phase: &mut Option<AgentPhase>,
    replay_phase: &mut Option<AgentPhase>,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let dto: ClaudeTranscriptLineDto = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            // Malformed JSON — skip silently
            return;
        }
    };

    // Claude Code stamps every transcript line with its top-level `cwd`
    // tracking. Surface transitions through `agent-cwd` so the frontend
    // can mirror them into pane.cwd without depending on the interactive
    // shell emitting OSC 7.
    if let Some(observed) = dto.cwd.as_deref() {
        if !observed.is_empty() && last_cwd.as_deref().map_or(true, |seen| seen != observed) {
            // During replay, suppress the per-line cwd event; last_cwd still
            // accumulates so the summary carries the final cwd at the boundary.
            if replay_done {
                let event = AgentCwdEvent {
                    session_id: session_id.to_string(),
                    cwd: observed.to_string(),
                };

                if let Err(e) = emit_agent_cwd(events.as_ref(), &event) {
                    log::warn!("Failed to emit agent-cwd event: {}", e);
                }
            }
            *last_cwd = Some(observed.to_string());
        }
    }

    // Agent lifecycle: a phase from the assistant turn boundary (stop_reason)
    // or a real user prompt; replay-bounded via record_lifecycle.
    let phase = match dto.line_type.as_deref() {
        Some("assistant") => dto
            .message
            .as_ref()
            .and_then(|m| m.stop_reason.as_deref())
            .and_then(|reason| match reason {
                "tool_use" => Some(AgentPhase::Running),
                "end_turn" | "stop_sequence" | "max_tokens" => Some(AgentPhase::Idle),
                _ => None,
            }),
        Some("user")
            if dto
                .message
                .as_ref()
                .is_some_and(|m| is_user_prompt(&m.content)) =>
        {
            Some(AgentPhase::Running)
        }
        _ => None,
    };
    if let Some(phase) = phase {
        record_lifecycle(
            phase,
            session_id,
            claude_agent_session_id,
            events,
            last_phase,
            replay_phase,
            replay_done,
        );
    }

    match dto.line_type.as_deref().unwrap_or("") {
        "assistant" => {
            process_assistant_message(
                &dto,
                session_id,
                cwd,
                events,
                in_flight,
                replay_activity,
                replay_done,
            );
            emit_reply_if_present(&dto, session_id, events, replay_done);
            emit_review_if_present(&dto, session_id, events, replay_done);
        }
        "user" => {
            process_user_message(
                &dto,
                session_id,
                cwd,
                events,
                emitter,
                in_flight,
                num_turns,
                replay_activity,
                replay_done,
            );
        }
        "tool_result" => {
            let timestamp = extract_timestamp(dto.timestamp.as_deref());
            process_tool_result(
                dto.tool_use_id.as_deref(),
                dto.is_error,
                &dto.content,
                session_id,
                cwd,
                events,
                emitter,
                in_flight,
                &timestamp,
                replay_activity,
                replay_done,
            );
        }
        "ai-title" => {
            if dto.session_id_field.as_deref() == Some(claude_agent_session_id) {
                emit_title(
                    events,
                    session_id,
                    claude_agent_session_id,
                    dto.ai_title.as_deref().unwrap_or(""),
                    TitleSource::AiGenerated,
                    last_title_memo,
                );
            }
        }
        "custom-title" => {
            if dto.session_id_field.as_deref() == Some(claude_agent_session_id) {
                emit_title(
                    events,
                    session_id,
                    claude_agent_session_id,
                    dto.custom_title.as_deref().unwrap_or(""),
                    TitleSource::UserRenamed,
                    last_title_memo,
                );
            }
        }
        _ => {
            // Other message types — ignore
        }
    }
}

/// Sanitize, dedup (for AI-generated only), and emit one
/// `agent-session-title` event. Mirrors the pre-refactor `emit_title`
/// helper from PR #265: AI-generated titles dedup against the memo to
/// avoid spamming the frontend on idempotent rewrites; user-renamed
/// titles ALWAYS emit because `/rename` is user-initiated and the
/// round-trip itself is a UX confirmation signal. `None` after
/// sanitization (empty / whitespace-only) clears the memo and emits an
/// empty-string title if a non-empty one was previously emitted, so the
/// frontend can revert to the pane's default name (PR #302 cycle 2
/// F1+F2).
fn emit_title(
    events: &Arc<dyn EventSink>,
    session_id: &str,
    claude_agent_session_id: &str,
    raw_title: &str,
    source: TitleSource,
    last_title_memo: &mut Option<String>,
) {
    let sanitized = sanitize_title(raw_title);
    let is_user_renamed = matches!(&source, TitleSource::UserRenamed);
    let (title, new_memo) = match sanitized {
        Some(title) if last_title_memo.as_deref() == Some(title.as_str()) && !is_user_renamed => {
            return;
        }
        Some(title) => (title.clone(), Some(title)),
        None if last_title_memo.is_some() => (String::new(), None),
        None => return,
    };

    let payload = AgentSessionTitleEvent {
        session_id: session_id.to_string(),
        agent_session_id: claude_agent_session_id.to_string(),
        title,
        source,
    };

    if let Err(err) = emit_agent_session_title(events.as_ref(), &payload) {
        log::warn!("agent-session-title emit failed: {}", err);
        return;
    }

    *last_title_memo = new_memo;
}

/// Pull the top-level `timestamp` field off a transcript line, or fall back
/// to the current clock. Claude Code JSONL lines carry the real event time —
/// `now_iso8601()` would otherwise stamp every event parsed in a single tick
/// (e.g. initial watch / batch catch-up) with the same "now", making the UI
/// feed look as if everything happened at once.
fn extract_timestamp(timestamp: Option<&str>) -> String {
    timestamp.map(str::to_string).unwrap_or_else(now_iso8601)
}

/// If a COMPLETED assistant turn's reply carries the VIM-283 sentinel block,
/// extract it and emit `agent-reply`. Fires once per finished turn (gated on the
/// terminal `stop_reason`), mirroring the Codex `emit_reply_if_present`.
fn emit_reply_if_present(
    dto: &ClaudeTranscriptLineDto,
    session_id: &str,
    events: &Arc<dyn EventSink>,
    replay_done: bool,
) {
    if !replay_done {
        return;
    }

    let Some(event) = reply_event(dto, session_id) else {
        return;
    };

    if let Err(e) = emit_agent_reply(events.as_ref(), &event) {
        log::warn!("Failed to emit agent-reply event: {}", e);
    }
}

fn reply_event(dto: &ClaudeTranscriptLineDto, session_id: &str) -> Option<AgentReplyEvent> {
    let ended = matches!(
        dto.message.as_ref().and_then(|m| m.stop_reason.as_deref()),
        Some("end_turn" | "stop_sequence" | "max_tokens")
    );
    if !ended {
        return None;
    }

    let blocks = message_content_items(dto)?;
    let reply_text = blocks
        .iter()
        .filter(|block| text_block_type(block) == Some("text"))
        .filter_map(text_block_text)
        .collect::<Vec<_>>()
        .join("\n");
    extract_agent_reply(&reply_text).map(|outcome| map_agent_reply_outcome(session_id, outcome))
}

/// If a completed assistant turn's `text` carries the VIM-304 review sentinel
/// block, extract it and emit `agent-review`. Same `replay_done` + completion
/// gates as the reply path. No sentinel → no event.
fn emit_review_if_present(
    dto: &ClaudeTranscriptLineDto,
    session_id: &str,
    events: &Arc<dyn EventSink>,
    replay_done: bool,
) {
    if !replay_done {
        return;
    }

    let Some(event) = review_event(dto, session_id) else {
        return;
    };

    if let Err(e) = crate::agent::events::emit_agent_review(events.as_ref(), &event) {
        log::warn!("Failed to emit agent-review event: {}", e);
    }
}

fn review_event(dto: &ClaudeTranscriptLineDto, session_id: &str) -> Option<AgentReviewEvent> {
    let ended = matches!(
        dto.message.as_ref().and_then(|m| m.stop_reason.as_deref()),
        Some("end_turn" | "stop_sequence" | "max_tokens")
    );
    if !ended {
        return None;
    }

    let blocks = message_content_items(dto)?;
    let reply_text = blocks
        .iter()
        .filter(|block| text_block_type(block) == Some("text"))
        .filter_map(text_block_text)
        .collect::<Vec<_>>()
        .join("\n");

    crate::agent::review::extract_agent_review(&reply_text)
        .map(|outcome| crate::agent::review::map_review_outcome(session_id, outcome))
}

/// Extract tool_use entries from an assistant message.
fn process_assistant_message(
    dto: &ClaudeTranscriptLineDto,
    session_id: &str,
    cwd: Option<&Path>,
    events: &Arc<dyn EventSink>,
    in_flight: &mut InFlightToolCalls,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let content = match message_content_items(dto) {
        Some(arr) => arr,
        None => return,
    };

    let timestamp = extract_timestamp(dto.timestamp.as_deref());

    for item in content {
        if tool_block_type(item) != "tool_use" {
            continue;
        }

        let tool_use_dto: ClaudeToolUseDto = match serde_json::from_value(item.clone()) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let id = match tool_use_dto.id.as_deref() {
            Some(id) => id.to_string(),
            None => continue,
        };

        let name = tool_use_dto.name.as_deref().unwrap_or("unknown");
        let input_value = tool_use_dto.rest.get("input");

        // Run the matcher BEFORE summarize_input truncates — the matcher
        // needs the full untruncated command to tokenize correctly.
        let test_match = bash_command(name, input_value).and_then(|cmd| match_command(cmd, cwd));

        let args = summarize_input(input_value);

        // Tag Write/Edit on test files. Use the FULL untruncated
        // input.file_path — `args` is summarized to MAX_ARGS_LEN and
        // long workspace paths could otherwise drop the suffix that
        // makes a file recognizable as a test (e.g. `…ndle.test.ts`).
        let is_test_file = if matches!(name, "Write" | "Edit") {
            tool_file_path(input_value)
                .map(super::test_runners::test_file_patterns::is_test_file)
                .unwrap_or(false)
        } else {
            false
        };

        let now = Instant::now();
        in_flight.insert(
            id.clone(),
            InFlightToolCall {
                started_at: now,
                started_at_iso: timestamp.clone(),
                tool: name.to_string(),
                args: args.clone(),
                is_test_file,
                test_match,
            },
        );

        let event = AgentToolCallEvent {
            session_id: session_id.to_string(),
            tool_use_id: id,
            tool: name.to_string(),
            args,
            status: ToolCallStatus::Running,
            timestamp: timestamp.clone(),
            duration_ms: 0,
            is_test_file,
        };

        record_tool_call(events, event, replay_activity, replay_done);
    }
}

/// Extract tool_result entries from a user message.
#[allow(clippy::too_many_arguments)]
fn process_user_message(
    dto: &ClaudeTranscriptLineDto,
    session_id: &str,
    cwd: Option<&Path>,
    events: &Arc<dyn EventSink>,
    emitter: &mut TestRunEmitter,
    in_flight: &mut InFlightToolCalls,
    num_turns: &mut u32,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let content = match dto.message.as_ref().map(|m| &m.content) {
        Some(content) => content,
        None => return,
    };

    let timestamp = extract_timestamp(dto.timestamp.as_deref());

    if let Some(items) = content.as_array() {
        for item in items {
            if is_tool_result_block(item) {
                let block_dto: ClaudeToolResultDto = match serde_json::from_value(item.clone()) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                process_tool_result(
                    block_dto.tool_use_id.as_deref(),
                    block_dto.is_error,
                    &block_dto.content,
                    session_id,
                    cwd,
                    events,
                    emitter,
                    in_flight,
                    &timestamp,
                    replay_activity,
                    replay_done,
                );
            }
        }
    }

    if is_user_prompt(content) {
        *num_turns = num_turns.saturating_add(1);

        // During replay, suppress the per-turn event; num_turns still
        // accumulates so the boundary summary carries the final count.
        if replay_done {
            let event = AgentTurnEvent {
                session_id: session_id.to_string(),
                num_turns: *num_turns,
            };

            if let Err(e) = emit_agent_turn(events.as_ref(), &event) {
                log::warn!("Failed to emit agent-turn event: {}", e);
            }
        }
    }
}

/// Process a tool_result line and emit Done/Failed event
#[allow(clippy::too_many_arguments)]
fn process_tool_result(
    tool_use_id: Option<&str>,
    is_error: Option<bool>,
    content: &Value,
    session_id: &str,
    cwd: Option<&Path>,
    events: &Arc<dyn EventSink>,
    emitter: &mut TestRunEmitter,
    in_flight: &mut InFlightToolCalls,
    timestamp: &str,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let tool_use_id = match tool_use_id {
        Some(id) => id.to_string(),
        None => return,
    };

    let is_error = is_error.unwrap_or(false);

    // An orphaned tool_result — a tool_result whose parent tool_use was
    // never recorded in_flight — arrives most often when a Claude Code
    // session auto-compacts: earlier assistant messages (containing the
    // tool_use blocks) get trimmed from the transcript, but the later
    // user messages carrying their tool_results stay. Emitting a placeholder
    // 'unknown' event for each one surfaces misleading noise in the UI —
    // the tool name is lost, args is empty, duration is zero, and the chip
    // summary grows an 'unknown N' bucket users can't act on. Drop it.
    let Some(call) = in_flight.remove(&tool_use_id) else {
        return;
    };
    let duration_ms = call.started_at.elapsed().as_millis() as u64;
    let tool_name = call.tool;
    let args = call.args;
    let is_test_file = call.is_test_file;

    if let Some(matched) = call.test_match {
        // Pull the captured Bash output content.
        let content_str = extract_tool_result_content(content);
        let captured = super::test_runners::types::CapturedOutput {
            content: content_str,
            is_error,
        };
        // Build the snapshot only when we have a workspace cwd. Falling
        // back to `Path::new(".")` would canonicalise to the backend
        // process's cwd — NOT the user's workspace — so test-file groups
        // would resolve against the wrong directory (silently producing
        // non-clickable or misleadingly-scoped rows). When cwd is absent
        // the standard agent-tool-call event still fires below; only the
        // structured test-run snapshot is skipped.
        if let Some(cwd_ref) = cwd {
            let snapshot = super::test_runners::build::maybe_build_snapshot(
                super::test_runners::build::BuildArgs {
                    session_id,
                    matched: &matched,
                    started_at: &call.started_at_iso,
                    finished_at: timestamp,
                    instant_fallback: call.started_at.elapsed(),
                    captured,
                    cwd: cwd_ref,
                },
            );
            if let Some(snap) = snapshot {
                // Route through the replay-aware emitter — during the
                // initial catch-up read this batches to latest-only;
                // after first EOF it emits live.
                emitter.submit(snap);
            }
        } else {
            log::debug!(
                "Skipping test-run snapshot for session {}: no workspace cwd resolved",
                session_id
            );
        }
    }

    let status = if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Done
    };

    let event = AgentToolCallEvent {
        session_id: session_id.to_string(),
        tool_use_id,
        tool: tool_name,
        args,
        status,
        timestamp: timestamp.to_string(),
        duration_ms,
        is_test_file,
    };

    record_tool_call(events, event, replay_activity, replay_done);
}

fn message_content_items(dto: &ClaudeTranscriptLineDto) -> Option<&[Value]> {
    dto.message.as_ref()?.content.as_array().map(Vec::as_slice)
}

fn is_tool_result_block(value: &Value) -> bool {
    line_type(value) == "tool_result"
}

/// Whether a single content block represents real user content. `tool_result`
/// blocks are tool returns, not prompts. `text` blocks count only when the
/// inner text is non-whitespace (mirrors the symmetric guard on the
/// string-typed content path in `is_user_prompt`). Other block types
/// (image, document, etc.) count as content if present.
fn is_non_empty_user_block(item: &Value) -> bool {
    if is_tool_result_block(item) {
        return false;
    }
    if text_block_type(item) == Some("text") {
        return text_block_text(item)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
    }
    true
}

pub(crate) fn is_user_prompt(content: &Value) -> bool {
    if let Some(text) = content.as_str() {
        return !text.trim().is_empty();
    }

    let Some(items) = content.as_array() else {
        return false;
    };

    items.iter().any(is_non_empty_user_block)
}

/// Summarize a tool input Value into a short string (~1024 chars max)
fn summarize_input(input: Option<&Value>) -> String {
    let input = match input {
        Some(v) => v,
        None => return String::new(),
    };

    // Try to extract file_path first (common across Read, Write, Edit)
    if let Some(path) = input_file_path(input) {
        return truncate_string(path, MAX_ARGS_LEN);
    }

    // Try command (Bash tool)
    if let Some(cmd) = input_command(input) {
        return truncate_string(cmd, MAX_ARGS_LEN);
    }

    // Try pattern (Grep tool)
    if let Some(pat) = input_pattern(input) {
        return truncate_string(&format!("pattern: {}", pat), MAX_ARGS_LEN);
    }

    // Fallback: stringify the whole input
    let s = input.to_string();
    truncate_string(&s, MAX_ARGS_LEN)
}

/// Truncate a string to max_len characters, appending "..." if truncated.
/// Uses char boundaries to avoid panics on multi-byte UTF-8 (emoji, CJK).
fn truncate_string(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .nth(max_len.saturating_sub(3))
            .map_or(s.len(), |(i, _)| i);
        format!("{}...", &s[..end])
    }
}

/// Get current time as ISO 8601 string (UTC)
fn now_iso8601() -> String {
    // Use std::time for a simple UTC timestamp without chrono dependency
    let now = std::time::SystemTime::now();
    let since_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_epoch.as_secs();

    // Simple UTC formatting: YYYY-MM-DDTHH:MM:SSZ
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Days since epoch to date (simplified Gregorian)
    let (year, month, day) = days_to_date(days);

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since Unix epoch to (year, month, day)
fn days_to_date(days: u64) -> (u64, u64, u64) {
    // Civil calendar algorithm
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Pull the textual `content` out of a tool_result JSON value.
/// Handles both the simple-string shape and the array-of-blocks shape
/// (where each block may carry `{type:"text", text:"..."}` payloads).
fn extract_tool_result_content(content: &Value) -> String {
    // Takes the `content` *value* directly (not the enclosing block) so the
    // A-transcript migration can pass `&dto.content`. Absent `content` (passed
    // as `Value::Null` by callers) and `null` both miss the str/array arms and
    // fall through to `""` — behavior-neutral with the prior `.get("content")`.
    if let Some(s) = content.as_str() {
        return cap_with_head_and_tail(s);
    }
    if let Some(arr) = content.as_array() {
        let memory_cap = MAX_TOOL_RESULT_CONTENT_LEN + TOOL_RESULT_TAIL_LEN;
        let prune_threshold = memory_cap * 2;
        let mut buf = String::new();
        for block in arr {
            if text_block_type(block) != Some("text") {
                continue;
            }
            let Some(text) = text_block_text(block) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            if !buf.is_empty() && !buf.ends_with('\n') {
                buf.push('\n');
            }
            // Pre-cap an oversized block so push_str can't grow `buf`
            // unbounded before the post-append prune fires.
            if text.len() > memory_cap {
                let capped = cap_with_head_and_tail(text);
                buf.push_str(&capped);
            } else {
                buf.push_str(text);
            }
            if buf.len() > prune_threshold {
                buf = cap_with_head_and_tail(&buf);
            }
        }
        return cap_with_head_and_tail(&buf);
    }
    String::new()
}

/// Cap content at `MAX_TOOL_RESULT_CONTENT_LEN` bytes via head-and-tail truncation.
fn cap_with_head_and_tail(content: &str) -> String {
    if content.len() <= MAX_TOOL_RESULT_CONTENT_LEN {
        return content.to_string();
    }

    // Reserve room for the truncation marker (leading newline + marker
    // + trailing newline). Clamp the effective tail size so
    // `head + marker + tail ≤ MAX` is guaranteed even if the
    // `TOOL_RESULT_TAIL_LEN` constant ever exceeds `MAX_TOOL_RESULT_CONTENT_LEN`.
    let marker_budget = TOOL_RESULT_TRUNCATED_MARKER.len() + 2;
    let max_tail_room = MAX_TOOL_RESULT_CONTENT_LEN.saturating_sub(marker_budget);
    let effective_tail = TOOL_RESULT_TAIL_LEN.min(max_tail_room);
    let head_target = MAX_TOOL_RESULT_CONTENT_LEN
        .saturating_sub(effective_tail)
        .saturating_sub(marker_budget);

    let mut head_end = head_target;
    while head_end > 0 && !content.is_char_boundary(head_end) {
        head_end -= 1;
    }

    let mut tail_start = content.len().saturating_sub(effective_tail);
    while tail_start < content.len() && !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    // After the clamp + char-boundary correction, head and tail
    // windows can no longer overlap on any reachable input — keep a
    // defensive fallback for safety, returning a tail-only slice
    // that still respects the cap.
    if tail_start <= head_end {
        let fallback_len = effective_tail;
        let mut tail_only_start = content.len().saturating_sub(fallback_len);
        while tail_only_start < content.len() && !content.is_char_boundary(tail_only_start) {
            tail_only_start += 1;
        }
        return content[tail_only_start..].to_string();
    }

    // Single allocation sized to exactly head + marker + tail.
    let head_slice = &content[..head_end];
    let tail_slice = &content[tail_start..];
    let needs_leading_newline = !head_slice.ends_with('\n');
    let marker_extra =
        if needs_leading_newline { 1 } else { 0 } + TOOL_RESULT_TRUNCATED_MARKER.len() + 1;

    let mut out = String::with_capacity(head_slice.len() + marker_extra + tail_slice.len());
    out.push_str(head_slice);
    if needs_leading_newline {
        out.push('\n');
    }
    out.push_str(TOOL_RESULT_TRUNCATED_MARKER);
    out.push('\n');
    out.push_str(tail_slice);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::FakeEventSink;

    #[test]
    fn recover_replies_requires_a_completed_assistant_turn() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let transcript_path = tmp.path().join("claude.jsonl");
        let reply_text = "done\n<<<VIMEFLOW_REPLY\n{\"v\":1,\"nonce\":\"wanted\",\"replies\":[{\"id\":1,\"status\":\"reply\",\"text\":\"yes\"}]}\nVIMEFLOW_REPLY>>>";
        let incomplete = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": reply_text}],
                "stop_reason": "tool_use"
            }
        });
        let complete = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": reply_text}],
                "stop_reason": "end_turn"
            }
        });
        std::fs::write(
            &transcript_path,
            format!("{incomplete}\n{complete}\n{complete}\n"),
        )
        .expect("write transcript");

        let recovered = recover_replies(
            &transcript_path,
            "pty-1",
            &std::collections::HashSet::from(["wanted".to_string()]),
        )
        .expect("recover replies");

        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].nonce.as_deref(), Some("wanted"));
    }

    #[test]
    fn recover_reviews_requires_a_completed_assistant_turn() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let transcript_path = tmp.path().join("claude.jsonl");
        let review_text = "done\n<<<VIMEFLOW_REVIEW\n{\"v\":1,\"nonce\":\"wanted\",\"reviewer\":\"Claude\",\"findings\":[{\"scope\":\"file\",\"path\":\"a.ts\",\"category\":\"bug\",\"text\":\"found\"}]}\nVIMEFLOW_REVIEW>>>";
        let incomplete = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": review_text}],
                "stop_reason": "tool_use"
            }
        });
        let complete = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": review_text}],
                "stop_reason": "end_turn"
            }
        });
        std::fs::write(
            &transcript_path,
            format!("{incomplete}\n{complete}\n{complete}\n"),
        )
        .expect("write transcript");

        let recovered = recover_reviews(
            &transcript_path,
            "pty-1",
            &std::collections::HashSet::from(["wanted".to_string()]),
        )
        .expect("recover reviews");

        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].nonce.as_deref(), Some("wanted"));
    }

    fn lifecycle_phases(sink: &FakeEventSink) -> Vec<String> {
        sink.recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-lifecycle")
            .filter_map(|(_, p)| p["phase"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn claude_replay_flushes_only_the_settled_phase_once() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            ClaudeTranscriptDecoder::new(sink.clone(), "sid".into(), None, "agent-1".into());
        decoder.decode_line(r#"{"type":"user","message":{"content":"hi"}}"#);
        decoder.decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}],"stop_reason":"tool_use"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#,
        );
        decoder.on_caught_up();
        assert_eq!(lifecycle_phases(&sink), vec!["idle"]);
    }

    #[test]
    fn claude_live_emits_running_then_idle_transition() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            ClaudeTranscriptDecoder::new(sink.clone(), "sid".into(), None, "agent-1".into());
        decoder.decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ready"}],"stop_reason":"end_turn"}}"#,
        );
        decoder.on_caught_up();
        decoder.decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Read","input":{}}],"stop_reason":"tool_use"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}}"#,
        );
        assert_eq!(lifecycle_phases(&sink), vec!["idle", "running", "idle"]);
    }

    #[test]
    fn claude_replay_coalesces_tool_calls_then_live_emits_individually() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            ClaudeTranscriptDecoder::new(sink.clone(), "sid".into(), None, "agent-1".into());

        // Replay: a user prompt (turn) + one tool_use whose tool_result
        // completes it. Both are suppressed during replay and folded into the
        // boundary summary.
        decoder.decode_line(r#"{"type":"user","message":{"content":"hi"}}"#);
        decoder.decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/a.ts"}}]}}"#,
        );
        decoder.decode_line(
            r#"{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}"#,
        );

        assert_eq!(sink.count("agent-tool-call"), 0);
        assert_eq!(sink.count("agent-turn"), 0);

        decoder.on_caught_up();

        assert_eq!(sink.count("agent-replay-summary"), 1);
        let summaries: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-replay-summary")
            .collect();
        assert_eq!(summaries[0].1["numTurns"], 1);
        assert_eq!(summaries[0].1["toolCallTotal"], 1);
        assert_eq!(summaries[0].1["recentToolCalls"][0]["toolUseId"], "t1");
        assert_eq!(summaries[0].1["recentToolCalls"][0]["status"], "done");

        // After catch-up, a live tool_use/tool_result emits individual events.
        decoder.decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/b.ts"}}]}}"#,
        );
        decoder.decode_line(
            r#"{"type":"tool_result","tool_use_id":"t2","content":"ok","is_error":false}"#,
        );
        assert_eq!(sink.count("agent-tool-call"), 2);
        assert_eq!(sink.count("agent-replay-summary"), 1);
    }

    #[test]
    fn claude_replay_emits_running_tool_calls_at_catch_up() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            ClaudeTranscriptDecoder::new(sink.clone(), "sid".into(), None, "agent-1".into());

        decoder.decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"npm test"}}],"stop_reason":"tool_use"}}"#,
        );

        assert_eq!(sink.count("agent-tool-call"), 0);

        decoder.on_caught_up();

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].1["toolUseId"], "t1");
        assert_eq!(tool_calls[0].1["status"], "running");
    }

    #[test]
    fn validate_transcript_path_rejects_path_outside_claude_root() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let transcript_path = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript_path, "").expect("failed to write transcript");

        let result = validate_transcript_path(
            transcript_path
                .to_str()
                .expect("temp transcript path should be UTF-8"),
        );

        assert!(result.is_err());
    }

    #[test]
    fn validate_transcript_path_rejects_null_byte() {
        let result = validate_transcript_path("/home/user/.claude/x.jsonl\0../../etc/passwd");

        assert!(matches!(
            result,
            Err(ValidateTranscriptError::InvalidPath(message))
                if message.contains("null byte")
        ));
    }

    #[test]
    fn parse_tool_use_from_assistant_line() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_abc","name":"Read","input":{"file_path":"/src/foo.ts"}}]}}"#;
        let value: Value = serde_json::from_str(line).unwrap();

        let content = value["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);

        let item = &content[0];
        assert_eq!(item["type"].as_str().unwrap(), "tool_use");
        assert_eq!(item["id"].as_str().unwrap(), "toolu_abc");
        assert_eq!(item["name"].as_str().unwrap(), "Read");
        assert_eq!(item["input"]["file_path"].as_str().unwrap(), "/src/foo.ts");
    }

    #[test]
    fn parse_tool_result_line() {
        let line = r#"{"type":"tool_result","tool_use_id":"toolu_abc","content":"file contents...","is_error":false}"#;
        let value: Value = serde_json::from_str(line).unwrap();

        assert_eq!(value["type"].as_str().unwrap(), "tool_result");
        assert_eq!(value["tool_use_id"].as_str().unwrap(), "toolu_abc");
        assert!(!value["is_error"].as_bool().unwrap());
    }

    #[test]
    fn parse_tool_result_with_error() {
        let line = r#"{"type":"tool_result","tool_use_id":"toolu_xyz","content":"error msg","is_error":true}"#;
        let value: Value = serde_json::from_str(line).unwrap();

        assert!(value["is_error"].as_bool().unwrap());
    }

    #[test]
    fn extract_tool_result_content_caps_simple_string() {
        let value = serde_json::json!({
            "content": "a".repeat(MAX_TOOL_RESULT_CONTENT_LEN + 1024)
        });

        let content = extract_tool_result_content(&value["content"]);

        assert!(content.len() < MAX_TOOL_RESULT_CONTENT_LEN + 1024);
        assert!(content.contains(TOOL_RESULT_TRUNCATED_MARKER));
    }

    /// Updated for F15 (Codex review on PR #153). The cycle-8 fix
    /// switched from head-only truncation to head-and-tail truncation.
    /// For a 2-block input where the FIRST block alone overflows the
    /// cap, BOTH the first block's tail bytes AND the second block's
    /// content land within the kept tail window — that's the whole
    /// point of head-and-tail (test-runner summary lines at the END
    /// must survive). Asserts: head marker (head window), tail marker
    /// from FIRST block (tail window can include first-block tail
    /// bytes when their position falls inside the kept-tail range),
    /// and block-two-summary (last block's content always survives).
    #[test]
    fn extract_tool_result_content_caps_text_blocks() {
        let head_marker = "HEAD-MARKER";
        let block_one_tail_marker = "BLOCK-ONE-TAIL-MARKER";
        let bulk = "x".repeat(MAX_TOOL_RESULT_CONTENT_LEN);
        let combined = format!("{head_marker}\n{bulk}\n{block_one_tail_marker}");
        let value = serde_json::json!({
            "content": [
                { "type": "text", "text": &combined },
                { "type": "text", "text": "block-two-summary" }
            ]
        });

        let content = extract_tool_result_content(&value["content"]);

        assert!(content.contains(TOOL_RESULT_TRUNCATED_MARKER));
        assert!(
            content.contains(head_marker),
            "head marker must be preserved in the head window"
        );
        assert!(
            content.contains(block_one_tail_marker),
            "tail bytes from the FIRST oversized block must survive in the tail window — \
             head-and-tail truncation must preserve content from the END of EARLIER blocks too, \
             not just the last block (Codex cycle-8 deferred-LOW)"
        );
        assert!(
            content.contains("block-two-summary"),
            "second block must be preserved in the tail window (F15)"
        );
    }

    /// F1 regression (Claude review on PR #153). When the LAST text
    /// block exactly fills `MAX_TOOL_RESULT_CONTENT_LEN` and does not
    /// end with `\n`, the pre-fix code emitted `[output truncated]`
    /// even though no content was dropped. With the cycle-8 head-and-tail
    /// rewrite, exact-cap content is under the threshold for truncation
    /// (`<=` not `<`), so it returns unchanged with no marker.
    #[test]
    fn extract_tool_result_content_no_marker_on_exact_cap_fill_last_block() {
        let exactly_at_cap: String = "a".repeat(MAX_TOOL_RESULT_CONTENT_LEN);
        let value = serde_json::json!({
            "content": [
                { "type": "text", "text": exactly_at_cap }
            ]
        });

        let content = extract_tool_result_content(&value["content"]);

        assert_eq!(content.len(), MAX_TOOL_RESULT_CONTENT_LEN);
        assert!(
            !content.contains(TOOL_RESULT_TRUNCATED_MARKER),
            "marker must not appear for an exact-cap fill"
        );
    }

    /// F15 regression (Codex review on PR #153). Test-runner output
    /// (cargo, vitest) puts summary lines at the END of the buffer.
    /// Head-only truncation (the cycle-0–cycle-7 implementation)
    /// would drop the summary, causing `maybe_build_snapshot` to skip
    /// emitting non-error snapshots, so successful large test runs
    /// silently vanished from the UI. Head-and-tail truncation
    /// preserves the trailing summary line within the tail window.
    #[test]
    fn extract_tool_result_content_preserves_tail_summary_for_test_runner_output() {
        // Simulate a verbose test run: 350 KiB of output that ends
        // with the kind of summary line cargo's parser keys on.
        let summary_line = "test result: ok. 1234 passed; 0 failed; 0 ignored";
        let bulk = "compiling...\n".repeat(30000); // ~30k * 13 bytes ≈ 390 KiB
        let combined = format!("{bulk}\n{summary_line}\n");
        assert!(combined.len() > MAX_TOOL_RESULT_CONTENT_LEN);

        let value = serde_json::json!({
            "content": &combined
        });

        let content = extract_tool_result_content(&value["content"]);

        assert!(
            content.contains(summary_line),
            "trailing summary line must survive truncation so test-runner parsers \
             can emit non-error snapshots for large successful runs (F15). \
             Got content of length {}, last 200 chars: {:?}",
            content.len(),
            &content[content.len().saturating_sub(200)..]
        );
        assert!(
            content.contains(TOOL_RESULT_TRUNCATED_MARKER),
            "marker must appear when content exceeded the cap"
        );
        assert!(
            content.len() <= MAX_TOOL_RESULT_CONTENT_LEN + 128,
            "post-truncation length should be within ~MAX (plus marker), got {}",
            content.len()
        );
    }

    /// F10 regression (Claude review on PR #153). Streaming JSONL
    /// flushes can emit `{"type":"text","text":""}` empty-string
    /// sentinel blocks. Those carry no content, so they must NOT be
    /// counted as "skipped blocks" when the cap is exactly hit on the
    /// last meaningful block — otherwise the `[output truncated]`
    /// marker fires falsely. The two `iter.any(...)` calls in
    /// `extract_tool_result_content` now require the trailing block
    /// to have non-empty text before counting it as skipped.
    #[test]
    fn extract_tool_result_content_no_marker_when_only_remaining_blocks_are_empty() {
        let exactly_at_cap: String = "a".repeat(MAX_TOOL_RESULT_CONTENT_LEN);
        let value = serde_json::json!({
            "content": [
                { "type": "text", "text": exactly_at_cap },
                { "type": "text", "text": "" },
                { "type": "text", "text": "" }
            ]
        });

        let content = extract_tool_result_content(&value["content"]);

        assert_eq!(content.len(), MAX_TOOL_RESULT_CONTENT_LEN);
        assert!(
            !content.contains(TOOL_RESULT_TRUNCATED_MARKER),
            "marker must not appear when the only remaining blocks are empty-string sentinels"
        );
    }

    #[test]
    fn parse_nested_tool_result_from_user_message() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"file contents...","is_error":false}]}}"#;
        let dto: ClaudeTranscriptLineDto = serde_json::from_str(line).unwrap();

        let content = message_content_items(&dto).unwrap();
        let result = content
            .iter()
            .find(|item| is_tool_result_block(item))
            .unwrap();

        assert_eq!(result["type"].as_str().unwrap(), "tool_result");
        assert_eq!(result["tool_use_id"].as_str().unwrap(), "toolu_abc");
        assert!(!result["is_error"].as_bool().unwrap());
    }

    #[test]
    fn malformed_json_does_not_panic() {
        let bad_lines = vec![
            "{not valid json",
            "",
            "null",
            r#"{"type":"unknown"}"#,
            r#"{"type":"assistant","message":{}}"#,
            r#"{"type":"assistant","message":{"content":"not an array"}}"#,
            r#"{"type":"tool_result"}"#, // missing tool_use_id
        ];

        // Every line shape must deserialize without error (DTO fields are
        // all defaulted/lenient). Lines that are not valid JSON at all are
        // skipped silently by process_line.
        for line in &bad_lines {
            let result: Result<ClaudeTranscriptLineDto, _> = serde_json::from_str(line);
            if let Ok(dto) = result {
                let _ = dto.line_type.as_deref().unwrap_or("");
                let _ = dto.message.as_ref().map(|m| m.content.as_array());
                let _ = dto.tool_use_id.as_deref();
                let _ = dto.is_error;
                let _ = &dto.content;
            }
        }
    }

    #[test]
    fn summarize_input_file_path() {
        let input: Value =
            serde_json::from_str(r#"{"file_path":"/src/foo.ts","content":"hello"}"#).unwrap();
        let summary = summarize_input(Some(&input));
        assert_eq!(summary, "/src/foo.ts");
    }

    #[test]
    fn summarize_input_command() {
        let input: Value = serde_json::from_str(r#"{"command":"ls -la /tmp"}"#).unwrap();
        let summary = summarize_input(Some(&input));
        assert_eq!(summary, "ls -la /tmp");
    }

    #[test]
    fn summarize_input_pattern() {
        let input: Value = serde_json::from_str(r#"{"pattern":"foo.*bar"}"#).unwrap();
        let summary = summarize_input(Some(&input));
        assert_eq!(summary, "pattern: foo.*bar");
    }

    #[test]
    fn summarize_input_truncation() {
        let long_path = format!("/very/long/path/{}", "a".repeat(1100));
        let input: Value =
            serde_json::from_str(&format!(r#"{{"file_path":"{}"}}"#, long_path)).unwrap();
        let summary = summarize_input(Some(&input));
        assert!(summary.chars().count() <= MAX_ARGS_LEN);
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn summarize_input_none() {
        let summary = summarize_input(None);
        assert!(summary.is_empty());
    }

    #[test]
    fn summarize_input_fallback() {
        let input: Value = serde_json::from_str(r#"{"some_field":"some_value"}"#).unwrap();
        let summary = summarize_input(Some(&input));
        assert!(!summary.is_empty());
        assert!(summary.contains("some_field"));
    }

    #[test]
    fn truncate_string_short() {
        assert_eq!(truncate_string("hello", 100), "hello");
    }

    #[test]
    fn truncate_string_long() {
        let long = "a".repeat(1200);
        let result = truncate_string(&long, MAX_ARGS_LEN);
        // truncate_string's contract is bounded by character count, not
        // byte count — assert against `chars().count()` (Claude review
        // on PR #152, F17). For all-ASCII inputs `len()` and
        // `chars().count()` agree, so this also matches the previous
        // assertion.
        assert_eq!(result.chars().count(), MAX_ARGS_LEN);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_string_long_cjk_respects_char_boundary() {
        // Multi-byte UTF-8 input: each CJK char is 3 bytes. The byte
        // length of the truncated string can significantly exceed
        // max_len (97 chars × 3 bytes + 3 bytes for "..." = 294 bytes
        // for max_len=1024), but the char count must remain at the cap.
        // Regression guard for F17: previously the test asserted
        // `result.len() <= MAX_ARGS_LEN` (bytes), which would have
        // passed for ASCII but given false assurance about CJK paths.
        let cjk = "中".repeat(1200);
        let result = truncate_string(&cjk, MAX_ARGS_LEN);
        assert_eq!(result.chars().count(), MAX_ARGS_LEN);
        assert!(result.ends_with("..."));
        // Byte length is unbounded by design — sanity check it's larger
        // than max_len in chars, proving the char-count is the real cap.
        assert!(
            result.len() > MAX_ARGS_LEN,
            "byte length should exceed char cap"
        );
    }

    #[test]
    fn now_iso8601_format() {
        let ts = now_iso8601();
        // Should match pattern: YYYY-MM-DDTHH:MM:SSZ
        assert!(ts.ends_with('Z'));
        assert_eq!(ts.len(), 20);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }

    #[test]
    fn days_to_date_pinned_dates() {
        // Pinned-date regression guards for the hand-rolled Hinnant
        // civil-calendar algorithm in `days_to_date` (Claude review on
        // PR #152, F18). The shape-only `now_iso8601_format` test
        // above can't catch off-by-one errors in leap-year accounting
        // or the March-epoch offset; these assertions can. Each entry
        // is `(days_since_unix_epoch, expected (year, month, day))`.
        // Sources: Hinnant reference, https://howardhinnant.github.io/date_algorithms.html
        let cases: [(u64, (u64, u64, u64)); 9] = [
            // Unix epoch.
            (0, (1970, 1, 1)),
            // First leap day after the epoch.
            (789, (1972, 2, 29)),
            // Day after a leap day.
            (790, (1972, 3, 1)),
            // Year-2000 leap year (divisible by 400 — leap).
            (11016, (2000, 2, 29)),
            // Day after a year-2000 leap day.
            (11017, (2000, 3, 1)),
            // Year-2100 NON-leap (divisible by 100 but not 400 — not leap).
            // 2100-02-28 = days_since_epoch 47540.
            (47540, (2100, 2, 28)),
            // Day after that — should be 2100-03-01, NOT 2100-02-29.
            (47541, (2100, 3, 1)),
            // Post-2038 sanity (32-bit-time-rollover-irrelevant for our u64).
            (25339, (2039, 5, 18)),
            // A round-millennium turnover.
            (10957, (2000, 1, 1)),
        ];
        for (days, expected) in cases.iter() {
            let got = days_to_date(*days);
            assert_eq!(
                got, *expected,
                "days_to_date({}) = {:?}, expected {:?}",
                days, got, expected
            );
        }
    }

    #[test]
    fn multiple_tool_uses_in_single_message() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/a.ts"}},{"type":"text","text":"reading"},{"type":"tool_use","id":"toolu_2","name":"Write","input":{"file_path":"/b.ts","content":"x"}}]}}"#;
        let value: Value = serde_json::from_str(line).unwrap();

        let content = value["message"]["content"].as_array().unwrap();
        let tool_uses: Vec<&Value> = content
            .iter()
            .filter(|item| item["type"].as_str() == Some("tool_use"))
            .collect();

        assert_eq!(tool_uses.len(), 2);
        assert_eq!(tool_uses[0]["name"].as_str().unwrap(), "Read");
        assert_eq!(tool_uses[1]["name"].as_str().unwrap(), "Write");
    }

    #[test]
    fn in_flight_tracking() {
        // Simulate the in-flight map lifecycle
        let mut in_flight: InFlightToolCalls = HashMap::new();

        // Tool call starts
        let start = Instant::now();
        in_flight.insert(
            "toolu_abc".to_string(),
            InFlightToolCall {
                started_at: start,
                started_at_iso: "2026-04-28T12:00:00Z".to_string(),
                tool: "Read".to_string(),
                args: "/src/foo.ts".to_string(),
                is_test_file: false,
                test_match: None,
            },
        );
        assert!(in_flight.contains_key("toolu_abc"));

        // Tool result arrives
        let call = in_flight.remove("toolu_abc").unwrap();
        let duration = call.started_at.elapsed().as_millis() as u64;
        assert_eq!(call.tool, "Read");
        assert_eq!(call.args, "/src/foo.ts");
        assert!(duration < 1000); // Should be near-instant in test
        assert!(in_flight.is_empty());
    }

    #[test]
    fn orphan_signal_is_missing_in_flight_entry() {
        // process_tool_result keys its orphan-drop behavior off exactly
        // this signal: in_flight.remove(id) returns None when the parent
        // tool_use was never recorded (typically because Claude Code
        // transcript compaction trimmed it). The test asserts the
        // prerequisite, not the full emit-side path; those assertions live
        // in EventSink-backed integration coverage.
        //
        // If the orphan-drop signal ever changes (e.g. we switch to a
        // different data structure), update this test and the `let Some
        // (call) = in_flight.remove(...) else { return };` guard in
        // process_tool_result together.
        let mut in_flight: InFlightToolCalls = HashMap::new();
        let result = in_flight.remove("toolu_nonexistent");
        assert!(result.is_none());
    }

    // extract_timestamp drives the emitted AgentToolCallEvent.timestamp on
    // every line we parse; before this helper existed, every event was
    // stamped with now_iso8601() and the UI feed collapsed to "all happened
    // just now" on initial watch / batch catch-up. These tests lock in the
    // contract so future refactors can't silently regress it.
    #[test]
    fn extract_timestamp_uses_transcript_field_when_present() {
        assert_eq!(
            extract_timestamp(Some("2026-04-22T10:30:00Z")),
            "2026-04-22T10:30:00Z"
        );
    }

    #[test]
    fn extract_timestamp_falls_back_to_now_when_absent() {
        let ts = extract_timestamp(None);

        // Shape check against now_iso8601 — same ISO-8601 UTC format
        // (YYYY-MM-DDTHH:MM:SSZ). We can't compare exact values because
        // the fallback calls now_iso8601() which reads the wall clock.
        assert!(ts.ends_with('Z'));
        assert_eq!(ts.len(), 20);
        assert!(ts.starts_with("20"));
    }

    #[test]
    fn extract_timestamp_ignores_non_string_field() {
        // Integration guard for the full JSONL → DTO → fallback path: a
        // non-string `timestamp` degrades to None via the DTO's lenient_string
        // deserializer, so extract_timestamp falls back to now_iso8601 (this is
        // the round-trip the migration must not lose — distinct from the
        // extract_timestamp(None) unit case above).
        let dto: ClaudeTranscriptLineDto = serde_json::from_str(
            r#"{"type":"assistant","timestamp":1234567890,"message":{"content":[]}}"#,
        )
        .expect("line with non-string timestamp still parses");
        assert!(
            dto.timestamp.is_none(),
            "non-string timestamp degrades to None via lenient_string"
        );

        let ts = extract_timestamp(dto.timestamp.as_deref());
        assert!(ts.ends_with('Z'));
        assert_eq!(ts.len(), 20);
    }

    #[test]
    fn extract_timestamp_preserves_full_iso_string_exactly() {
        // Sub-second precision and timezone offsets should pass through
        // untouched — the frontend parses whatever we emit.
        assert_eq!(
            extract_timestamp(Some("2026-04-22T10:30:45.123Z")),
            "2026-04-22T10:30:45.123Z"
        );
    }

    // is_user_prompt / is_non_empty_user_block — direct in-module coverage.
    // transcript_fixture_tests exercises these end to end through the watcher;
    // these unit tests pin the predicate contract so a refactor can't silently
    // regress the edge cases.

    #[test]
    fn is_user_prompt_string_path_rejects_whitespace_only() {
        assert!(!is_user_prompt(&Value::String("   ".into())));
        assert!(!is_user_prompt(&Value::String("\n\t  \n".into())));
    }

    #[test]
    fn is_user_prompt_string_path_rejects_empty_string() {
        assert!(!is_user_prompt(&Value::String(String::new())));
    }

    #[test]
    fn is_user_prompt_string_path_accepts_non_whitespace() {
        assert!(is_user_prompt(&Value::String("hi".into())));
    }

    #[test]
    fn is_user_prompt_array_path_empty_array_is_not_a_prompt() {
        let content: Value = serde_json::from_str("[]").unwrap();
        assert!(!is_user_prompt(&content));
    }

    #[test]
    fn is_user_prompt_array_path_only_tool_result_is_not_a_prompt() {
        let content: Value = serde_json::from_str(
            r#"[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]"#,
        )
        .unwrap();
        assert!(!is_user_prompt(&content));
    }

    #[test]
    fn is_user_prompt_array_path_whitespace_only_text_block_is_not_a_prompt() {
        let content: Value = serde_json::from_str(r#"[{"type":"text","text":"   "}]"#).unwrap();
        assert!(!is_user_prompt(&content));
    }

    #[test]
    fn is_user_prompt_array_path_mixed_tool_result_plus_text_is_a_prompt() {
        let content: Value = serde_json::from_str(
            r#"[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"},{"type":"text","text":"follow-up"}]"#,
        )
        .unwrap();
        assert!(is_user_prompt(&content));
    }

    #[test]
    fn is_user_prompt_neither_string_nor_array_is_not_a_prompt() {
        // Object / number / null content shapes should fail closed.
        let object: Value = serde_json::from_str(r#"{"key":"value"}"#).unwrap();
        assert!(!is_user_prompt(&object));
        assert!(!is_user_prompt(&Value::Null));
        assert!(!is_user_prompt(&serde_json::json!(42)));
    }

    #[test]
    fn is_non_empty_user_block_unknown_block_type_counts_as_content() {
        // An unknown non-tool_result block type (e.g. image, document, or
        // a future Claude block) is treated as content. Intentional default —
        // documented here so a future contributor doesn't tighten this
        // without thinking through the regression on, say, image messages.
        let block: Value =
            serde_json::from_str(r#"{"type":"image","source":{"type":"base64","data":"..."}}"#)
                .unwrap();
        assert!(is_non_empty_user_block(&block));
    }

    #[test]
    fn is_non_empty_user_block_text_block_with_missing_text_field_is_not_content() {
        // A `text` block without a `text` field, or with a non-string `text`
        // value, fails the non-whitespace check and does not count as content.
        let no_text: Value = serde_json::from_str(r#"{"type":"text"}"#).unwrap();
        assert!(!is_non_empty_user_block(&no_text));

        let non_string_text: Value = serde_json::from_str(r#"{"type":"text","text":42}"#).unwrap();
        assert!(!is_non_empty_user_block(&non_string_text));
    }

    #[test]
    fn is_non_empty_user_block_block_with_missing_or_non_string_type_falls_through_to_content() {
        // Blocks where the `type` field is absent or non-string take the
        // unknown-block fall-through and count as content. This mirrors the
        // image / document / future-Claude-block case (see
        // is_non_empty_user_block_unknown_block_type_counts_as_content) —
        // anything that is not explicitly `tool_result` and not an empty
        // `text` block is permissive by default. Documenting the contract
        // here so a future tightening (e.g. require an explicit allowlist
        // of known content types) is a deliberate change, not silent drift.
        let no_type: Value = serde_json::from_str("{}").unwrap();
        assert!(is_non_empty_user_block(&no_type));

        let non_string_type: Value = serde_json::from_str(r#"{"type":42,"text":"hello"}"#).unwrap();
        assert!(is_non_empty_user_block(&non_string_type));

        let null_type: Value = serde_json::from_str(r#"{"type":null,"text":"hello"}"#).unwrap();
        assert!(is_non_empty_user_block(&null_type));
    }

    /// Regression: a tool_result line with a wrong-typed `is_error` must
    /// still emit the `agent-tool-call` event (lenient degradation, not a
    /// silent drop). The DTO's `lenient_bool` turns `"oops"` → `None`,
    /// which `process_tool_result` maps to `false` → `Done` status.
    #[test]
    fn process_line_emits_with_wrong_typed_is_error() {
        let concrete = Arc::new(crate::runtime::FakeEventSink::new());
        let events: Arc<dyn EventSink> = concrete.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight: InFlightToolCalls = HashMap::new();
        let mut num_turns = 0;
        let mut last_cwd = None;

        // Seed in_flight so the tool_result has a match
        in_flight.insert(
            "toolu_xyz".to_string(),
            InFlightToolCall {
                started_at: Instant::now(),
                started_at_iso: "2026-04-28T12:00:00Z".to_string(),
                tool: "Bash".to_string(),
                args: "echo hi".to_string(),
                is_test_file: false,
                test_match: None,
            },
        );

        let line =
            r#"{"type":"tool_result","tool_use_id":"toolu_xyz","content":"ok","is_error":"oops"}"#;
        let mut last_title_memo: Option<String> = None;
        process_line(
            line,
            "sess-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            "claude-agent-sid",
            &mut last_title_memo,
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        assert_eq!(concrete.count("agent-tool-call"), 1);
        assert!(in_flight.is_empty(), "matched tool_use should be removed");
    }

    fn drive_assistant_line(concrete: &Arc<FakeEventSink>, line: &str, replay_done: bool) {
        let events: Arc<dyn EventSink> = concrete.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight: InFlightToolCalls = HashMap::new();
        let mut num_turns = 0;
        let mut last_cwd = None;
        let mut last_title_memo: Option<String> = None;

        process_line(
            line,
            "sess-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            "claude-agent-sid",
            &mut last_title_memo,
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            replay_done,
        );
    }

    /// A completed assistant transcript line whose text is `text` — built from a
    /// JSON object so the test reads without hand-escaped quotes.
    fn assistant_line(text: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{ "type": "text", "text": text }],
                "stop_reason": "end_turn",
            },
        })
        .to_string()
    }

    /// Wrap a JSON object in the review sentinels.
    fn review_block(inner: serde_json::Value) -> String {
        format!("<<<VIMEFLOW_REVIEW\n{inner}\nVIMEFLOW_REVIEW>>>")
    }

    #[test]
    fn assistant_end_turn_with_reply_block_emits_agent_reply() {
        let concrete = Arc::new(FakeEventSink::new());
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done\n<<<VIMEFLOW_REPLY\n{\"v\":1,\"nonce\":\"abc\",\"replies\":[{\"id\":1,\"status\":\"answered\",\"text\":\"because latency\"}]}\nVIMEFLOW_REPLY>>>"}],"stop_reason":"end_turn"}}"#;
        drive_assistant_line(&concrete, line, true);

        let replies: Vec<_> = concrete
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-reply")
            .collect();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].1["sessionId"], "sess-1");
        assert_eq!(replies[0].1["nonce"], "abc");
        assert_eq!(replies[0].1["replies"][0]["id"], 1);
    }

    #[test]
    fn assistant_end_turn_without_sentinel_emits_no_reply() {
        let concrete = Arc::new(FakeEventSink::new());
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"just done"}],"stop_reason":"end_turn"}}"#;
        drive_assistant_line(&concrete, line, true);

        assert!(concrete
            .recorded()
            .iter()
            .all(|(name, _)| name != "agent-reply"));
    }

    #[test]
    fn assistant_end_turn_with_review_block_emits_agent_review() {
        let concrete = Arc::new(FakeEventSink::new());
        let block = review_block(serde_json::json!({
            "v": 1,
            "nonce": "n",
            "reviewer": "codex",
            "findings": [
                { "scope": "file", "path": "a.ts", "category": "bug", "text": "x" },
            ],
        }));
        let line = assistant_line(&format!("done\n{block}"));
        drive_assistant_line(&concrete, &line, true);

        let reviews: Vec<_> = concrete
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-review")
            .collect();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].1["sessionId"], "sess-1");
        assert_eq!(reviews[0].1["reviewer"], "codex");
        assert_eq!(reviews[0].1["findings"][0]["scope"], "file");
    }

    #[test]
    fn assistant_end_turn_without_review_block_emits_no_review() {
        let concrete = Arc::new(FakeEventSink::new());
        let line = assistant_line("just done");
        drive_assistant_line(&concrete, &line, true);

        assert!(concrete
            .recorded()
            .iter()
            .all(|(name, _)| name != "agent-review"));
    }

    #[test]
    fn assistant_replay_with_reply_block_emits_no_reply() {
        let concrete = Arc::new(FakeEventSink::new());
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done\n<<<VIMEFLOW_REPLY\n{\"v\":1,\"nonce\":\"stale\",\"replies\":[{\"id\":1,\"status\":\"answered\",\"text\":\"old answer\"}]}\nVIMEFLOW_REPLY>>>"}],"stop_reason":"end_turn"}}"#;
        drive_assistant_line(&concrete, line, false);

        assert!(concrete
            .recorded()
            .iter()
            .all(|(name, _)| name != "agent-reply"));
    }

    /// Regression: `summarize_input` must preserve the absent vs present-null
    /// distinction ("" vs "null") that the DTO's `#[serde(flatten)] rest`
    /// map preserves for `tool_use.input`.
    #[test]
    fn summarize_input_preserves_absent_vs_null() {
        assert_eq!(summarize_input(None), "");
        assert_eq!(summarize_input(Some(&serde_json::Value::Null)), "null");
    }

    /// Test helper for the title-emit regressions below. Drives a single
    /// transcript line through `process_line` with default emitter / state,
    /// then returns the recorded `agent-session-title` payloads.
    fn drive_title_line(
        line: &str,
        claude_agent_session_id: &str,
        last_title_memo: &mut Option<String>,
    ) -> Vec<serde_json::Value> {
        let concrete = Arc::new(crate::runtime::FakeEventSink::new());
        let events: Arc<dyn EventSink> = concrete.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight: InFlightToolCalls = HashMap::new();
        let mut num_turns = 0_u32;
        let mut last_cwd = None;

        process_line(
            line,
            "pty-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            claude_agent_session_id,
            last_title_memo,
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        concrete
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-session-title")
            .map(|(_, payload)| payload)
            .collect()
    }

    /// PR #302 cycle 2 F1+F2 — `ai-title` rows whose `sessionId` matches
    /// the tail's `claude_agent_session_id` emit an `agent-session-title`
    /// event with `source = ai-generated`.
    #[test]
    fn ai_title_matching_session_id_emits_ai_generated() {
        let agent_id = "claude-abc-123";
        let mut memo = None;
        let line = format!(
            r#"{{"type":"ai-title","aiTitle":"Investigate slow startup","sessionId":"{}"}}"#,
            agent_id
        );
        let titles = drive_title_line(&line, agent_id, &mut memo);
        assert_eq!(titles.len(), 1);
        assert_eq!(titles[0]["title"], "Investigate slow startup");
        assert_eq!(titles[0]["source"], "ai-generated");
        assert_eq!(titles[0]["sessionId"], "pty-1");
        assert_eq!(titles[0]["agentSessionId"], agent_id);
        assert_eq!(memo.as_deref(), Some("Investigate slow startup"));
    }

    /// PR #302 cycle 2 F1+F2 — `custom-title` rows (from `/rename`) emit
    /// with `source = user-renamed` and bypass the dedup memo so a
    /// re-rename to the same title still round-trips.
    #[test]
    fn custom_title_matching_session_id_emits_user_renamed_and_bypasses_dedup() {
        let agent_id = "claude-abc-123";
        let mut memo = Some("my-feature".to_string());
        let line = format!(
            r#"{{"type":"custom-title","customTitle":"my-feature","sessionId":"{}"}}"#,
            agent_id
        );
        let titles = drive_title_line(&line, agent_id, &mut memo);
        assert_eq!(titles.len(), 1, "custom-title must always emit");
        assert_eq!(titles[0]["title"], "my-feature");
        assert_eq!(titles[0]["source"], "user-renamed");
    }

    /// PR #302 cycle 2 F1+F2 — title rows whose `sessionId` does NOT
    /// match the tail's `claude_agent_session_id` are dropped silently.
    /// Per-session isolation: a tail thread must only emit for ITS
    /// session, never another Claude session's title.
    #[test]
    fn ai_title_mismatched_session_id_is_dropped() {
        let mut memo = None;
        let line = r#"{"type":"ai-title","aiTitle":"Wrong","sessionId":"other-session"}"#;
        let titles = drive_title_line(line, "claude-abc-123", &mut memo);
        assert!(titles.is_empty());
        assert_eq!(memo, None);
    }

    /// PR #302 cycle 2 F1+F2 — `ai-title` dedup against the per-decoder
    /// memo. Two identical AI-generated titles back-to-back emit only
    /// once; the second is suppressed.
    #[test]
    fn ai_title_dedups_against_memo() {
        let agent_id = "claude-abc-123";
        let mut memo = None;
        let line = format!(
            r#"{{"type":"ai-title","aiTitle":"Same","sessionId":"{}"}}"#,
            agent_id
        );
        let first = drive_title_line(&line, agent_id, &mut memo);
        assert_eq!(first.len(), 1);
        let second = drive_title_line(&line, agent_id, &mut memo);
        assert!(second.is_empty(), "duplicate ai-title must dedup");
    }

    /// End-to-end G3 carve-out: a real `tool_use` line split across the
    /// replay→live EOF boundary — where *neither half is valid JSON* — must
    /// still emit `agent-tool-call`. The old per-provider `tail_loop` handed
    /// each partial to the parser as if it were a complete line (both halves
    /// failed `from_str`, so the event was silently dropped); the shared
    /// `TranscriptTailService` buffers the partial across the non-terminal EOF
    /// and rejoins it. This is the sanctioned Phase 2 behavior change
    /// (F-EVENTS two-sided G3 carve-out), so the assertion is PASS, not the
    /// pre-C drop.
    #[test]
    fn split_tool_use_line_across_eof_still_emits() {
        use crate::agent::adapter::base::{ScriptedReader, Step};

        let concrete = Arc::new(crate::runtime::FakeEventSink::new());
        let events: Arc<dyn EventSink> = concrete.clone();
        let decoder = ClaudeTranscriptDecoder::new(
            events,
            "sess-g3".to_string(),
            None,
            "claude-sess-g3".to_string(),
        );

        // One valid tool_use assistant line, split mid-string-value so neither
        // side parses on its own: `..."tool_us` | `e"...`.
        let first: &[u8] = br#"{"type":"assistant","message":{"content":[{"type":"tool_us"#;
        let second: &[u8] =
            b"e\",\"id\":\"toolu_g3\",\"name\":\"Read\",\"input\":{\"file_path\":\"/a.ts\"}}]}}\n";

        // A leading clean EOF (empty partial buffer) fires `on_caught_up` and
        // ends replay BEFORE the split line arrives, so the rejoined `running`
        // tool_use decodes on the LIVE path and emits an `agent-tool-call`
        // (during replay a lone `running` event is folded into the summary and
        // dropped — see record_tool_call). The split still straddles a
        // NON-terminal EOF (partial buffer non-empty), exercising the rejoin.
        let stop = Arc::new(AtomicBool::new(false));
        TranscriptTailService::new(Box::new(decoder), "transcript")
            .with_poll_interval(std::time::Duration::ZERO)
            .run(
                ScriptedReader::new(
                    vec![
                        Step::Eof,
                        Step::Chunk(first),
                        Step::Eof,
                        Step::Chunk(second),
                        Step::EofStop,
                    ]
                    .into_iter(),
                    stop.clone(),
                ),
                stop,
            );

        assert_eq!(
            concrete.count("agent-tool-call"),
            1,
            "split tool_use line must rejoin across EOF and emit exactly one event"
        );
    }
}
