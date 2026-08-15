//! Transcript tailer for opencode bridge JSONL files.
//!
//! Mirrors `codex/transcript.rs`: [`start_tailing`] opens the
//! `<sessionID>.jsonl` the agent-watcher-opencode-bridge plugin writes, builds an
//! [`OpencodeTranscriptDecoder`], wraps it in the shared
//! [`TranscriptTailService`], and spawns the tail thread. The decoder folds
//! each bridge line into `agent-turn` / `agent-tool-call` / `agent-cwd` /
//! `test-run` events per spec §4.4.
//!
//! Bridge line → event mapping (spec §4.4):
//!
//! - `kind=event`, `type=message.updated`, `data.info.role == "user"` ⇒
//!   [`AgentTurnEvent`] — **once per user message id** (tracked in
//!   `seen_turns`). `session.created` is intentionally NOT a turn source.
//! - `kind=tool.before` ⇒ tool-call start (args preview already on the line).
//!   `kind=event`, `type=message.part.updated`, `data.part.type=="tool"`,
//!   `state.status` `pending`/`running` ⇒ start/running.
//! - `kind=tool.after` ⇒ authoritative tool-call done/failed from the process
//!   exit code when a `tool.before` line established that `tool.after` is
//!   expected. Terminal `message.part.updated` signals are used for calls that
//!   appear without a `tool.before`.
//! - `kind=tool.after` where `tool=="bash"` ⇒ feed `args.command` +
//!   `result.output` + `result.metadata.exit` into the shared
//!   `claude_code::test_runners` (cwd = the session cwd, NOT a per-command
//!   workdir). Only submit when `maybe_build_snapshot` returns `Some`.
//! - `kind=event`, `type=session.status` busy/retry ⇒ lifecycle running;
//!   `session.idle` or `session.status` idle ⇒ lifecycle idle.
//! - assistant `data.info.path.cwd` change ⇒ [`AgentCwdEvent`] when it differs
//!   from `last_cwd`. (The v1 bridge sanitizer drops `info.path`, so this is a
//!   defensive read that only fires if a future bridge re-adds the field; the
//!   authored fixture carries it so the path is exercised.)

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::agent::adapter::base::{TranscriptDecoder, TranscriptHandle, TranscriptTailService};
use crate::agent::adapter::claude_code::test_runners::build::{maybe_build_snapshot, BuildArgs};
use crate::agent::adapter::claude_code::test_runners::emitter::TestRunEmitter;
use crate::agent::adapter::claude_code::test_runners::matcher::match_command;
use crate::agent::adapter::claude_code::test_runners::test_file_patterns::is_test_file;
use crate::agent::adapter::claude_code::test_runners::timestamps::compute_duration_ms;
use crate::agent::adapter::claude_code::test_runners::types::CapturedOutput;
use crate::agent::events::{
    emit_agent_cwd, emit_agent_replay_summary, emit_agent_reply, emit_agent_tool_call,
    emit_agent_turn, emit_lifecycle_on_change, record_attention, record_lifecycle,
    record_tool_call, ReplayActivity,
};
use crate::agent::reply::{extract_agent_reply, AgentReplyOutcome};
use crate::agent::types::{
    AgentAttentionEvent, AgentAttentionReason, AgentCwdEvent, AgentPhase, AgentReplyEvent,
    AgentToolCallEvent, AgentTurnEvent, ToolCallStatus,
};
use crate::runtime::EventSink;

use super::locator::OpenCodeLocator;
use super::transcript_dto::{OpencodeEventType, OpencodeKind, OpencodeLineDto};

/// Cap on the displayed args preview, matching `codex/transcript.rs`'s
/// `MAX_ARGS_LEN` so the agent-status activity card renders a bounded string.
const MAX_ARGS_LEN: usize = 1024;
const MAX_ATTENTION_BODY_LEN: usize = 500;
const MAX_ATTENTION_DEDUPE_LEN: usize = 256;

/// An in-flight tool call awaiting its terminal `tool.after` (or a
/// `message.part.updated` `completed`/`error`). Keyed by `callID` (fallback the
/// part `id`).
struct InFlightToolCall {
    started_at: Instant,
    started_at_iso: String,
    tool: String,
    args: String,
    is_test_file: bool,
}

type InFlightToolCalls = HashMap<String, InFlightToolCall>;

/// Tool-call metadata that must survive display-state completion. opencode can
/// deliver `message.part.updated[completed]` before `tool.after`; the latter is
/// still needed for bash test-run parsing.
struct ToolCallMetadata {
    started_at: Instant,
    started_at_iso: String,
    tool: String,
    args: String,
    expects_tool_after: bool,
}

type ToolCallMetadataById = HashMap<String, ToolCallMetadata>;

/// Open the bridge JSONL, build the decoder, and spawn the tail thread.
///
/// Mirrors `codex::transcript::start_tailing` / `kimi::transcript::start_tailing`.
/// The `locator` is accepted for signature parity with the other providers and
/// so the streamer can prefer the locator's resolved cwd; the v1 opencode
/// locator carries no live process cwd beyond what `cwd` already supplies, so
/// the argument is otherwise unused.
pub(crate) fn start_tailing(
    events: Arc<dyn EventSink>,
    session_id: String,
    cwd: Option<PathBuf>,
    transcript_path: PathBuf,
    _locator: Arc<OpenCodeLocator>,
) -> Result<TranscriptHandle, String> {
    let file = File::open(&transcript_path).map_err(|e| {
        format!(
            "Failed to open opencode bridge transcript: {}: {}",
            transcript_path.display(),
            e
        )
    })?;

    // Surface the resolved workspace cwd once so the pane / file tree / git
    // follow the opencode project rather than the stale spawn cwd (mirrors
    // Kimi's `emit_initial_cwd`).
    if let Some(cwd_str) = cwd.as_deref().and_then(std::path::Path::to_str) {
        let event = AgentCwdEvent {
            session_id: session_id.clone(),
            cwd: cwd_str.to_string(),
        };
        if let Err(e) = emit_agent_cwd(events.as_ref(), &event) {
            log::warn!("Failed to emit opencode initial agent-cwd event: {}", e);
        }
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = stop_flag.clone();

    let decoder = OpencodeTranscriptDecoder::new(events, session_id, cwd);
    let service = TranscriptTailService::new(Box::new(decoder), "opencode transcript");

    let join_handle = std::thread::spawn(move || {
        service.run(BufReader::new(file), stop_clone);
    });

    Ok(TranscriptHandle::new(stop_flag, join_handle))
}

/// Per-session opencode decoder: owns the in-flight tool-call map, the
/// seen-turn message ids, turn count, last-seen cwd, the replay accumulators,
/// and a `replay_done` one-shot guard. Driven by
/// [`TranscriptTailService`], which owns the read/buffer/poll loop.
pub(crate) struct OpencodeTranscriptDecoder {
    events: Arc<dyn EventSink>,
    session_id: String,
    /// Session cwd passed into `start_tailing` — the cwd fed to the test-run
    /// parser (NOT any per-command workdir). Also the baseline for `last_cwd`.
    cwd: Option<PathBuf>,
    in_flight: InFlightToolCalls,
    tool_metadata: ToolCallMetadataById,
    /// Dedup set keyed by `(callID, status-discriminant)` so a repeated
    /// running/terminal update for the same call does not re-emit. The
    /// discriminant is a `&'static str` rather than `ToolCallStatus` so we don't
    /// have to widen the shared enum's derives (it isn't `Hash`/`Eq`).
    emitted: HashSet<(String, &'static str)>,
    /// Calls whose terminal status was already resolved by `tool.after`; later
    /// terminal part updates are non-authoritative for these call ids.
    resolved_by_tool_after: HashSet<String>,
    /// User-message ids already counted as a turn — a re-delivered
    /// `message.updated` for the same id does not double-count.
    seen_turns: HashSet<String>,
    num_turns: u32,
    /// Last cwd surfaced via `agent-cwd`; transitions only.
    last_cwd: Option<String>,
    emitter: TestRunEmitter,
    /// Historical tool calls are folded here until the first clean EOF.
    replay_activity: ReplayActivity,
    last_phase: Option<AgentPhase>,
    replay_phase: Option<AgentPhase>,
    agent_session_id: Option<String>,
    turn_active: bool,
    /// One-shot guard: false during replay, true after the first on_caught_up.
    replay_done: bool,
}

impl OpencodeTranscriptDecoder {
    fn new(events: Arc<dyn EventSink>, session_id: String, cwd: Option<PathBuf>) -> Self {
        let emitter = TestRunEmitter::new(events.clone());
        // Seed `last_cwd` with the session cwd so we don't re-emit the same
        // path the initial `emit_agent_cwd` in `start_tailing` already sent.
        let last_cwd = cwd
            .as_deref()
            .and_then(std::path::Path::to_str)
            .map(str::to_string);
        Self {
            events,
            session_id,
            cwd,
            in_flight: HashMap::new(),
            tool_metadata: HashMap::new(),
            emitted: HashSet::new(),
            resolved_by_tool_after: HashSet::new(),
            seen_turns: HashSet::new(),
            num_turns: 0,
            last_cwd,
            emitter,
            replay_activity: ReplayActivity::default(),
            last_phase: None,
            replay_phase: None,
            agent_session_id: None,
            turn_active: false,
            replay_done: false,
        }
    }

    fn process_line(&mut self, line: &str) {
        let dto: OpencodeLineDto = match serde_json::from_str(line) {
            Ok(dto) => dto,
            Err(_) => return,
        };
        let timestamp = ts_to_iso8601(dto.ts);

        match dto.kind() {
            OpencodeKind::ToolBefore => self.start_tool_call_from_before(&dto, &timestamp),
            OpencodeKind::ToolAfter => self.finish_tool_call_from_after(&dto, &timestamp),
            OpencodeKind::Event => self.process_event(&dto, &timestamp),
            OpencodeKind::Unknown => {}
        }
    }

    fn process_event(&mut self, dto: &OpencodeLineDto, timestamp: &str) {
        match dto.event_type() {
            OpencodeEventType::MessageUpdated => self.process_message_updated(dto),
            OpencodeEventType::MessagePartUpdated => self.process_part_updated(dto, timestamp),
            OpencodeEventType::AssistantText => self.process_assistant_text(dto),
            OpencodeEventType::SessionCreated | OpencodeEventType::SessionUpdated => {
                self.process_session_event(dto);
            }
            OpencodeEventType::SessionStatus => self.process_session_status(dto),
            OpencodeEventType::SessionIdle => self.process_session_idle(dto),
            OpencodeEventType::SessionError => self.process_session_error(dto),
            OpencodeEventType::PermissionAsked => self.process_attention(
                dto,
                AgentAttentionReason::ApprovalRequested,
                "OpenCode needs approval",
            ),
            OpencodeEventType::QuestionAsked => self.process_attention(
                dto,
                AgentAttentionReason::QuestionRequested,
                "OpenCode has a question",
            ),
            // Resolution only clears OpenCode's responder state. Notification
            // read state remains an explicit renderer action.
            _ => {}
        }
    }

    fn process_session_status(&mut self, dto: &OpencodeLineDto) {
        let Some(status) = dto
            .data
            .get("status")
            .and_then(|status| status.get("type"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let Some(agent_session_id) = opencode_session_id(dto) else {
            return;
        };
        if self.agent_session_id.as_deref() != Some(agent_session_id) {
            self.agent_session_id = Some(agent_session_id.to_string());
            self.last_phase = None;
        }

        match status {
            "busy" | "retry" => {
                self.turn_active = true;
                self.record_phase(agent_session_id, AgentPhase::Running);
            }
            "idle" => self.finish_turn(agent_session_id),
            _ => {}
        }
    }

    fn process_session_idle(&mut self, dto: &OpencodeLineDto) {
        if let Some(agent_session_id) = opencode_session_id(dto) {
            self.finish_turn(agent_session_id);
        }
    }

    fn finish_turn(&mut self, agent_session_id: &str) {
        if !self.turn_active {
            return;
        }
        self.turn_active = false;
        self.record_phase(agent_session_id, AgentPhase::Idle);
    }

    fn process_session_error(&mut self, dto: &OpencodeLineDto) {
        if !self.turn_active {
            return;
        }
        let Some(agent_session_id) = opencode_session_id(dto) else {
            return;
        };
        let body = dto
            .data
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .map(|message| truncate_string(message, MAX_ATTENTION_BODY_LEN));

        self.emit_attention(
            dto,
            AgentAttentionReason::AgentError,
            "OpenCode failed",
            body,
        );
        self.turn_active = false;
        self.record_phase(agent_session_id, AgentPhase::Idle);
    }

    fn process_attention(
        &mut self,
        dto: &OpencodeLineDto,
        reason: AgentAttentionReason,
        title: &str,
    ) {
        self.emit_attention(dto, reason, title, None);
    }

    fn emit_attention(
        &mut self,
        dto: &OpencodeLineDto,
        reason: AgentAttentionReason,
        title: &str,
        body: Option<String>,
    ) {
        let dedupe_key = dto
            .data
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(|id| truncate_string(id, MAX_ATTENTION_DEDUPE_LEN));

        record_attention(
            &self.events,
            AgentAttentionEvent {
                pty_id: self.session_id.clone(),
                reason,
                title: title.to_string(),
                body,
                occurred_at: dto.ts.and_then(|ts| u64::try_from(ts).ok()).unwrap_or(0),
                dedupe_key,
            },
            self.replay_done,
        );
    }

    fn record_phase(&mut self, agent_session_id: &str, phase: AgentPhase) {
        record_lifecycle(
            phase,
            &self.session_id,
            agent_session_id,
            &self.events,
            &mut self.last_phase,
            &mut self.replay_phase,
            self.replay_done,
        );
    }

    /// A bridge-synthesized `assistant.text` record: the assistant's whole
    /// turn text, aggregated by the plugin at `session.idle` (VIM-293).
    /// Extract a `VIMEFLOW_REPLY` block and emit it — one-call wiring like
    /// codex's `last_agent_message`, gated behind `replay_done` so replayed
    /// history never re-emits.
    fn process_assistant_text(&mut self, dto: &OpencodeLineDto) {
        if !self.replay_done {
            return;
        }
        let Some(text) = dto
            .data
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        else {
            return;
        };
        let Some(outcome) = extract_agent_reply(text) else {
            return;
        };

        let (raw_text, nonce, replies) = match outcome {
            AgentReplyOutcome::Structured {
                raw,
                nonce,
                replies,
            } => (raw, Some(nonce), Some(replies)),
            AgentReplyOutcome::Malformed { raw, nonce } => (raw, nonce, None),
        };

        let event = AgentReplyEvent {
            session_id: self.session_id.clone(),
            nonce,
            raw_text,
            replies,
        };
        if let Err(e) = emit_agent_reply(self.events.as_ref(), &event) {
            log::warn!("Failed to emit agent-reply event: {}", e);
        }
    }

    /// A user `message.updated` is one turn, counted once per `info.id`.
    fn process_message_updated(&mut self, dto: &OpencodeLineDto) {
        let info = match dto.data.get("info") {
            Some(info) => info,
            None => return,
        };
        if info.get("role").and_then(Value::as_str) != Some("user") {
            return;
        }
        let Some(id) = info
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        // Dedup by user-message id: a re-delivered update must not re-count.
        if !self.seen_turns.insert(id.to_string()) {
            return;
        }

        self.num_turns = self.num_turns.saturating_add(1);
        if self.replay_done {
            let event = AgentTurnEvent {
                session_id: self.session_id.clone(),
                num_turns: self.num_turns,
            };
            if let Err(e) = emit_agent_turn(self.events.as_ref(), &event) {
                log::warn!("Failed to emit opencode agent-turn event: {}", e);
            }
        }
    }

    /// `message.part.updated` with a `tool` part. `state.status`:
    /// `pending`/`running` ⇒ start/running; `completed` ⇒ done; `error` ⇒ failed.
    fn process_part_updated(&mut self, dto: &OpencodeLineDto, timestamp: &str) {
        let part = match dto.data.get("part") {
            Some(part) => part,
            None => return,
        };
        if part.get("type").and_then(Value::as_str) != Some("tool") {
            return;
        }
        let call_key = part_call_key(part);
        let Some(call_key) = call_key else {
            return;
        };
        let tool = part
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let state = part.get("state");
        let status = state
            .and_then(|s| s.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let args = summarize_state_args(state, &tool);
        let is_test_file = state_args_are_test_file(state);

        match status {
            "pending" | "running" => {
                self.start_tool_call(&call_key, tool, args, timestamp, is_test_file, false);
            }
            "completed" => {
                self.finish_tool_call_from_part_update(&call_key, ToolCallStatus::Done, timestamp);
            }
            "error" => {
                self.finish_tool_call_from_part_update(
                    &call_key,
                    ToolCallStatus::Failed,
                    timestamp,
                );
            }
            _ => {}
        }
    }

    /// `tool.before`: register the in-flight call + emit a running start
    /// (post-replay). The args preview is already on the line.
    fn start_tool_call_from_before(&mut self, dto: &OpencodeLineDto, timestamp: &str) {
        let Some(call_id) = dto.call_id.as_deref().filter(|s| !s.is_empty()) else {
            return;
        };
        let tool = dto.tool.as_deref().unwrap_or("unknown").to_string();
        let args = summarize_value_args(&dto.args, &tool);
        let is_test_file = value_args_are_test_file(&dto.args);
        self.start_tool_call(call_id, tool, args, timestamp, is_test_file, true);
    }

    /// `tool.after`: complete the in-flight call (`metadata.exit` decides
    /// done/failed) and, for `bash`, feed the captured output into the shared
    /// test-run parser.
    fn finish_tool_call_from_after(&mut self, dto: &OpencodeLineDto, timestamp: &str) {
        let Some(call_id) = dto.call_id.as_deref().filter(|s| !s.is_empty()) else {
            return;
        };
        let tool = dto.tool.as_deref().unwrap_or("unknown");
        let result = &dto.result;
        let exit = result
            .get("metadata")
            .and_then(|m| m.get("exit"))
            .and_then(Value::as_i64);
        let status = if exit.is_some_and(|code| code != 0) {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Done
        };

        // bash tool.after → shared test-run parser (cwd = session cwd).
        if tool == "bash" {
            self.maybe_submit_test_run(call_id, result, exit, timestamp);
        }

        self.finish_tool_call(call_id, status, timestamp);
        self.resolved_by_tool_after.insert(call_id.to_string());
        self.tool_metadata.remove(call_id);
    }

    /// Common start path: insert into `in_flight` (idempotent on the call key)
    /// and emit a `running` event once per call, post-replay. The dedup set is
    /// keyed by `(callID, Running)` so a `tool.before` followed by a `running`
    /// part update for the same call emits a single start.
    ///
    /// `tool.before` is the authoritative args source. opencode emits the
    /// `message.part.updated` tool `pending` (with EMPTY `state.args`) BEFORE
    /// the `tool.before` line that carries the real args. So when a later start
    /// upgrades a call's args from empty → non-empty, we patch the recorded
    /// `in_flight` args AND re-emit a refreshed `running` event (bypassing the
    /// `(callID, Running)` dedup that one time) so the activity feed shows the
    /// real filePath / command / pattern instead of `{}`.
    fn start_tool_call(
        &mut self,
        call_key: &str,
        tool: String,
        args: String,
        timestamp: &str,
        is_test_file: bool,
        expects_tool_after: bool,
    ) {
        // Did this call already have a recorded start, and is the incoming args
        // an upgrade over the previously-empty preview? Decide before the
        // `or_insert_with` so we know whether to patch + re-emit.
        let args_upgraded = self
            .in_flight
            .get(call_key)
            .is_some_and(|existing| existing.args.is_empty() && !args.is_empty());

        let entry = self
            .in_flight
            .entry(call_key.to_string())
            .or_insert_with(|| InFlightToolCall {
                started_at: Instant::now(),
                started_at_iso: timestamp.to_string(),
                tool: tool.clone(),
                args: args.clone(),
                is_test_file,
            });
        if args_upgraded {
            // Patch the authoritative args (and the tool name, which the empty
            // pending part may have left as a placeholder) onto the live record.
            entry.tool = tool.clone();
            entry.args = args.clone();
            entry.is_test_file = is_test_file;
        }
        self.record_tool_metadata(call_key, &tool, &args, timestamp, expects_tool_after);

        let dedup_key = (
            call_key.to_string(),
            status_discriminant(&ToolCallStatus::Running),
        );
        let first_emit = self.emitted.insert(dedup_key);
        // Emit when this is the first start OR when authoritative args have just
        // upgraded an already-emitted (empty-args) start — the refresh lets the
        // UI replace the placeholder `{}` with the real args.
        if !first_emit && !args_upgraded {
            return;
        }

        self.emit_tool_call(AgentToolCallEvent {
            session_id: self.session_id.clone(),
            tool_use_id: call_key.to_string(),
            tool,
            args,
            status: ToolCallStatus::Running,
            timestamp: timestamp.to_string(),
            duration_ms: 0,
            is_test_file,
        });
    }

    /// Terminal part updates are authoritative only for calls that started from
    /// the part stream. Calls with `tool.before` metadata are expected to emit a
    /// later `tool.after`, whose exit code decides the final display status.
    fn finish_tool_call_from_part_update(
        &mut self,
        call_key: &str,
        status: ToolCallStatus,
        timestamp: &str,
    ) {
        if self
            .tool_metadata
            .get(call_key)
            .is_some_and(|metadata| metadata.expects_tool_after)
            || self.resolved_by_tool_after.contains(call_key)
        {
            return;
        }
        self.finish_tool_call(call_key, status, timestamp);
        self.tool_metadata.remove(call_key);
    }

    /// Common completion path: remove the in-flight call and emit its terminal
    /// status once.
    fn finish_tool_call(&mut self, call_key: &str, status: ToolCallStatus, timestamp: &str) {
        let dedup_key = (call_key.to_string(), status_discriminant(&status));
        if !self.emitted.insert(dedup_key) {
            // Already emitted this terminal status for this call.
            self.in_flight.remove(call_key);
            return;
        }

        let (tool, args, duration_ms, is_test_file) = match self.in_flight.remove(call_key) {
            Some(call) => (
                call.tool,
                call.args,
                compute_duration_ms(&call.started_at_iso, timestamp, call.started_at.elapsed()),
                call.is_test_file,
            ),
            // A terminal update with no recorded start (replay began mid-call,
            // or the start was dropped). Emit with what we know.
            None => ("unknown".to_string(), String::new(), 0, false),
        };

        self.emit_tool_call(AgentToolCallEvent {
            session_id: self.session_id.clone(),
            tool_use_id: call_key.to_string(),
            tool,
            args,
            status,
            timestamp: timestamp.to_string(),
            duration_ms,
            is_test_file,
        });
    }

    fn record_tool_metadata(
        &mut self,
        call_key: &str,
        tool: &str,
        args: &str,
        timestamp: &str,
        expects_tool_after: bool,
    ) {
        self.tool_metadata
            .entry(call_key.to_string())
            .and_modify(|metadata| {
                if metadata.args.is_empty() && !args.is_empty() {
                    metadata.tool = tool.to_string();
                    metadata.args = args.to_string();
                }
                metadata.expects_tool_after |= expects_tool_after;
            })
            .or_insert_with(|| ToolCallMetadata {
                started_at: Instant::now(),
                started_at_iso: timestamp.to_string(),
                tool: tool.to_string(),
                args: args.to_string(),
                expects_tool_after,
            });
    }

    /// Feed a `bash` `tool.after` into the shared `claude_code::test_runners`.
    /// `cwd` is the session cwd (NOT any per-command workdir). Only submits when
    /// the command matches a known runner AND `maybe_build_snapshot` returns
    /// `Some`. The `TestRunEmitter` collapses replay-phase snapshots to the
    /// latest one, flushed at `finish_replay`.
    fn maybe_submit_test_run(
        &mut self,
        call_id: &str,
        result: &Value,
        exit: Option<i64>,
        timestamp: &str,
    ) {
        let Some(cwd) = self.cwd.clone() else {
            log::debug!(
                "Skipping opencode test-run snapshot for session {}: no workspace cwd resolved",
                self.session_id
            );
            return;
        };

        // The command preview lives on the start metadata (the `tool.before`'s
        // `args.command`). It is cached outside `in_flight` because a terminal
        // part update can remove display state before `tool.after` arrives.
        let command = self.tool_command(call_id).unwrap_or_default();
        if command.is_empty() {
            return;
        }
        let Some(matched) = match_command(&command, Some(cwd.as_path())) else {
            return;
        };

        let output = result
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let started_at = self
            .tool_metadata
            .get(call_id)
            .map(|c| c.started_at_iso.clone())
            .unwrap_or_else(|| timestamp.to_string());
        let instant_fallback = self
            .tool_metadata
            .get(call_id)
            .map(|c| c.started_at.elapsed())
            .unwrap_or_default();

        let captured = CapturedOutput {
            content: output,
            is_error: exit.is_some_and(|code| code != 0),
        };
        if let Some(snapshot) = maybe_build_snapshot(BuildArgs {
            session_id: &self.session_id,
            matched: &matched,
            started_at: &started_at,
            finished_at: timestamp,
            instant_fallback,
            captured,
            cwd: cwd.as_path(),
        }) {
            self.emitter.submit(snapshot);
        }
    }

    fn tool_command(&self, call_id: &str) -> Option<String> {
        self.tool_metadata
            .get(call_id)
            .filter(|metadata| metadata.tool == "bash")
            .map(|metadata| metadata.args.clone())
            .or_else(|| {
                self.in_flight
                    .get(call_id)
                    .filter(|call| call.tool == "bash")
                    .map(|call| call.args.clone())
            })
    }

    /// `session.created`/`session.updated`: track the cwd transition off
    /// `data.info.path.cwd`. Emit `agent-cwd` only when it differs from
    /// `last_cwd`.
    fn process_session_event(&mut self, dto: &OpencodeLineDto) {
        let observed = dto
            .data
            .get("info")
            .and_then(|info| info.get("path"))
            .and_then(|path| path.get("cwd"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let Some(observed) = observed else {
            return;
        };
        if self
            .last_cwd
            .as_deref()
            .map_or(true, |seen| seen != observed)
        {
            if self.replay_done {
                let event = AgentCwdEvent {
                    session_id: self.session_id.clone(),
                    cwd: observed.to_string(),
                };
                if let Err(e) = emit_agent_cwd(self.events.as_ref(), &event) {
                    log::warn!("Failed to emit opencode agent-cwd event: {}", e);
                }
            }
            self.last_cwd = Some(observed.to_string());
        }
    }

    fn emit_tool_call(&mut self, event: AgentToolCallEvent) {
        record_tool_call(
            &self.events,
            event,
            &mut self.replay_activity,
            self.replay_done,
        );
    }
}

impl TranscriptDecoder for OpencodeTranscriptDecoder {
    fn decode_line(&mut self, line: &str) {
        self.process_line(line);
    }

    /// First EOF marks the end of replay; subsequent EOFs are idempotent. The
    /// replay-flush (and any in-flight reconciliation) is gated behind
    /// `replay_done`, mirroring `codex/transcript.rs`.
    fn on_caught_up(&mut self) {
        if !self.replay_done {
            self.replay_done = true;
            if let Some(phase) = self.replay_phase.take() {
                if let Some(agent_session_id) = self.agent_session_id.as_deref() {
                    emit_lifecycle_on_change(
                        self.events.as_ref(),
                        &self.session_id,
                        agent_session_id,
                        &mut self.last_phase,
                        phase,
                    );
                }
            }
            for event in self.replay_activity.take_running() {
                if let Err(e) = emit_agent_tool_call(self.events.as_ref(), &event) {
                    log::warn!("Failed to emit opencode agent-tool-call event: {}", e);
                }
            }
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

fn opencode_session_id(dto: &OpencodeLineDto) -> Option<&str> {
    dto.data
        .get("sessionID")
        .and_then(Value::as_str)
        .filter(|session_id| !session_id.is_empty())
}

/// Stable `&'static str` discriminant for a [`ToolCallStatus`], used as the
/// second half of the `(callID, status)` dedup key. Keeping this local avoids
/// adding `Hash`/`Eq` to the shared `ToolCallStatus` enum.
fn status_discriminant(status: &ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Running => "running",
        ToolCallStatus::Done => "done",
        ToolCallStatus::Failed => "failed",
    }
}

/// Resolve the in-flight key for a `tool` part: prefer `callID`, fall back to
/// the part `id`. Empty strings are treated as absent.
fn part_call_key(part: &Value) -> Option<String> {
    part.get("callID")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            part.get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string)
}

/// Summarize a `tool.before` / `tool.after` `args` object into a bounded
/// preview string. Prefers the per-tool low-risk field the bridge keeps
/// (`command` for bash, `filePath` for read/edit/write, `pattern` for
/// glob/grep), else the raw JSON.
fn summarize_value_args(args: &Value, _tool: &str) -> String {
    if let Some(command) = args.get("command").and_then(Value::as_str) {
        return truncate_string(command, MAX_ARGS_LEN);
    }
    if let Some(file_path) = args.get("filePath").and_then(Value::as_str) {
        return truncate_string(file_path, MAX_ARGS_LEN);
    }
    if let Some(pattern) = args.get("pattern").and_then(Value::as_str) {
        return truncate_string(pattern, MAX_ARGS_LEN);
    }
    if args.is_null() {
        return String::new();
    }
    // An empty object (the `pending` tool part's `state.args:{}`) has no preview
    // — return the empty string so the streamer recognizes it as "no args yet"
    // and lets a later `tool.before` upgrade it (Bug A). Rendering `{}` here
    // would both look like junk in the feed AND defeat the empty-args upgrade.
    if args.as_object().is_some_and(|map| map.is_empty()) {
        return String::new();
    }
    truncate_string(&args.to_string(), MAX_ARGS_LEN)
}

/// Summarize a `message.part.updated` tool part's `state.args` (same shape as
/// the `tool.*` args). Falls back to the empty string when absent.
fn summarize_state_args(state: Option<&Value>, tool: &str) -> String {
    match state.and_then(|s| s.get("args")) {
        Some(args) => summarize_value_args(args, tool),
        None => String::new(),
    }
}

fn value_args_are_test_file(args: &Value) -> bool {
    args.get("filePath")
        .and_then(Value::as_str)
        .is_some_and(is_test_file)
}

fn state_args_are_test_file(state: Option<&Value>) -> bool {
    state
        .and_then(|s| s.get("args"))
        .is_some_and(value_args_are_test_file)
}

/// Truncate to `max_len` chars with a `...` suffix, char-boundary safe.
fn truncate_string(input: &str, max_len: usize) -> String {
    if input.chars().count() <= max_len {
        return input.to_string();
    }
    let end = input
        .char_indices()
        .nth(max_len.saturating_sub(3))
        .map_or(input.len(), |(idx, _)| idx);
    format!("{}...", &input[..end])
}

/// Convert the bridge `ts` (epoch-ms) into an ISO-8601 UTC string for event
/// payloads. `None` falls back to "now". Mirrors `codex/transcript.rs`'s
/// `now_iso8601` for the formatting half.
fn ts_to_iso8601(ts: Option<i64>) -> String {
    match ts {
        Some(ms) if ms >= 0 => {
            let total_secs = (ms / 1000) as u64;
            iso8601_from_unix_secs(total_secs)
        }
        _ => now_iso8601(),
    }
}

fn now_iso8601() -> String {
    let now = std::time::SystemTime::now();
    let since_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    iso8601_from_unix_secs(since_epoch.as_secs())
}

fn iso8601_from_unix_secs(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let secs_of_day = total_secs % 86_400;
    let (year, month, day) = days_to_date(days);
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, minute, second
    )
}

fn days_to_date(days_since_epoch: u64) -> (u64, u64, u64) {
    let z = days_since_epoch as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as u64, m as u64, d as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::FakeEventSink;
    use std::time::{Duration, SystemTime};

    const SAMPLE_BRIDGE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/agent/adapter/opencode/fixtures/sample_bridge.jsonl"
    ));

    fn tool_call_payloads(sink: &FakeEventSink) -> Vec<Value> {
        sink.recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-tool-call")
            .map(|(_, payload)| payload)
            .collect()
    }

    fn decoder(sink: &Arc<FakeEventSink>, cwd: Option<PathBuf>) -> OpencodeTranscriptDecoder {
        OpencodeTranscriptDecoder::new(sink.clone(), "sid".to_string(), cwd)
    }

    #[test]
    fn status_busy_to_session_idle_emits_one_live_completion_edge() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"busy"}}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2,"kind":"event","type":"session.idle","data":{"sessionID":"ses1"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":3,"kind":"event","type":"session.idle","data":{"sessionID":"ses1"}}"#,
        );

        let lifecycle: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-lifecycle")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(lifecycle.len(), 2);
        assert_eq!(lifecycle[0]["phase"], "running");
        assert_eq!(lifecycle[1]["phase"], "idle");
        assert_eq!(lifecycle[1]["agentSessionId"], "ses1");
    }

    #[test]
    fn semantic_attention_is_live_only_and_uses_stable_request_ids() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        let permission = r#"{"v":1,"ts":42,"kind":"event","type":"permission.asked","data":{"id":"permission-1","sessionID":"ses1"}}"#;

        dec.decode_line(permission);
        assert_eq!(sink.count("agent-attention"), 0);

        dec.on_caught_up();
        dec.decode_line(permission);
        dec.decode_line(
            r#"{"v":1,"ts":43,"kind":"event","type":"question.asked","data":{"id":"question-1","sessionID":"ses1"}}"#,
        );

        let attention: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-attention")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(attention.len(), 2);
        assert_eq!(attention[0]["reason"], "approval-requested");
        assert_eq!(attention[0]["dedupeKey"], "permission-1");
        assert_eq!(attention[1]["reason"], "question-requested");
    }

    #[test]
    fn session_error_notifies_and_returns_lifecycle_to_idle() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"busy"}}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2,"kind":"event","type":"session.error","data":{"sessionID":"ses1","error":{"name":"ProviderError","message":"Model stopped responding"}}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":3,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"idle"}}}"#,
        );

        assert_eq!(sink.count("agent-attention"), 1);
        let lifecycle: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-lifecycle")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(lifecycle.len(), 2);
        assert_eq!(lifecycle[0]["phase"], "running");
        assert_eq!(lifecycle[1]["phase"], "idle");
        let attention = sink
            .recorded()
            .into_iter()
            .find(|(name, _)| name == "agent-attention")
            .expect("error attention")
            .1;
        assert_eq!(attention["reason"], "agent-error");
        assert_eq!(attention["body"], "Model stopped responding");
    }

    /// A user `message.updated` ⇒ exactly one `agent-turn`; a duplicate user
    /// message id does NOT double-count.
    #[test]
    fn user_message_emits_one_turn_and_dedups_by_id() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        let line = r#"{"v":1,"ts":1,"kind":"event","type":"message.updated","data":{"info":{"id":"msg_u1","role":"user","sessionID":"ses1"}}}"#;
        dec.decode_line(line);
        dec.decode_line(line); // duplicate id — must not re-count

        assert_eq!(sink.count("agent-turn"), 1);
        let turns: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-turn")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(turns[0]["numTurns"], 1);
        assert_eq!(turns[0]["sessionId"], "sid");
    }

    /// An assistant `message.updated` (role != user) is NOT a turn.
    #[test]
    fn assistant_message_is_not_a_turn() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();
        dec.decode_line(
            r#"{"v":1,"ts":1,"kind":"event","type":"message.updated","data":{"info":{"id":"msg_a1","role":"assistant"}}}"#,
        );
        assert_eq!(sink.count("agent-turn"), 0);
    }

    /// `tool.before` then `tool.after` ⇒ one running start + one done, deduped
    /// by callID. (Live phase — replay already finished.)
    #[test]
    fn tool_before_after_emits_start_then_done_once() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1000,"kind":"tool.before","tool":"read","sessionID":"ses1","callID":"call_r1","args":{"filePath":"src/lib.rs"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2000,"kind":"tool.after","tool":"read","sessionID":"ses1","callID":"call_r1","result":{"output":"contents","metadata":{"exit":0}}}"#,
        );

        let payloads = tool_call_payloads(&sink);
        assert_eq!(payloads.len(), 2, "one running + one done");
        assert_eq!(payloads[0]["toolUseId"], "call_r1");
        assert_eq!(payloads[0]["status"], "running");
        assert_eq!(payloads[0]["tool"], "read");
        assert_eq!(payloads[0]["args"], "src/lib.rs");
        assert_eq!(payloads[1]["toolUseId"], "call_r1");
        assert_eq!(payloads[1]["status"], "done");
    }

    /// A repeated `running` part update for the same call does NOT re-emit a
    /// start — deduped by `(callID, Running)`.
    #[test]
    fn repeated_running_part_does_not_reemit_start() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        let running = r#"{"v":1,"ts":1,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_p1","tool":"bash","state":{"status":"running","args":{"command":"ls"}}}}}"#;
        dec.decode_line(running);
        dec.decode_line(running);
        dec.decode_line(running);

        let payloads = tool_call_payloads(&sink);
        assert_eq!(
            payloads.len(),
            1,
            "only one running start despite 3 updates"
        );
        assert_eq!(payloads[0]["status"], "running");
        assert_eq!(payloads[0]["toolUseId"], "call_p1");
    }

    /// Bug A — live opencode emits the `message.part.updated` tool `pending`
    /// (EMPTY `state.args`) BEFORE the `tool.before` line that carries the real
    /// args. The pending start must be REFRESHED once `tool.before` upgrades the
    /// args, so the activity feed shows the real filePath / command / pattern
    /// instead of `{}`. This FAILS on the pre-fix code (the `(callID, Running)`
    /// dedup dropped the `tool.before` re-emit, leaving the empty-args start).
    #[test]
    fn pending_part_then_tool_before_refreshes_args() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        // 1. Pending tool part with EMPTY args (arrives first, earlier ts).
        dec.decode_line(
            r#"{"v":1,"ts":1000,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_read1","tool":"read","state":{"status":"pending","args":{}}}}}"#,
        );
        // 2. tool.before with the AUTHORITATIVE args (arrives later).
        dec.decode_line(
            r#"{"v":1,"ts":1100,"kind":"tool.before","tool":"read","sessionID":"ses1","callID":"call_read1","args":{"filePath":"/tmp/x/util.ts"}}"#,
        );

        let payloads = tool_call_payloads(&sink);
        // One empty-args start, then one refreshed running with the real args.
        assert_eq!(payloads.len(), 2, "empty pending start + refreshed start");
        assert_eq!(payloads[0]["status"], "running");
        assert_eq!(payloads[0]["args"], "");
        assert_eq!(payloads[1]["status"], "running");
        assert_eq!(payloads[1]["toolUseId"], "call_read1");
        assert_eq!(payloads[1]["args"], "/tmp/x/util.ts");

        // And the recorded in-flight args are the authoritative ones, so the
        // eventual terminal (done/failed) event also carries them.
        assert_eq!(
            dec.in_flight.get("call_read1").map(|c| c.args.as_str()),
            Some("/tmp/x/util.ts"),
        );
    }

    /// The empty-args pending start must refresh derived metadata too. A later
    /// `tool.before` can reveal that the file path is a test file, and the
    /// terminal `tool.after` event should keep that classification.
    #[test]
    fn pending_part_then_tool_before_refreshes_test_file_classification() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1000,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_test","tool":"read","state":{"status":"pending","args":{}}}}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":1100,"kind":"tool.before","tool":"read","sessionID":"ses1","callID":"call_test","args":{"filePath":"src/util.test.ts"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":1200,"kind":"tool.after","tool":"read","sessionID":"ses1","callID":"call_test","result":{"output":"contents","metadata":{"exit":0}}}"#,
        );

        let payloads = tool_call_payloads(&sink);
        assert_eq!(payloads.len(), 3, "pending start + refresh + terminal");
        assert_eq!(payloads[1]["status"], "running");
        assert_eq!(payloads[1]["isTestFile"], true);
        assert_eq!(payloads[2]["status"], "done");
        assert_eq!(payloads[2]["args"], "src/util.test.ts");
        assert_eq!(payloads[2]["isTestFile"], true);
    }

    /// A second authoritative `tool.before` (or running part) carrying the SAME
    /// non-empty args must NOT re-emit — the refresh fires only on the
    /// empty → non-empty upgrade, not on every repeat.
    #[test]
    fn non_empty_args_repeat_does_not_refresh() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1,"kind":"tool.before","tool":"read","sessionID":"ses1","callID":"call_z","args":{"filePath":"/tmp/a"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_z","tool":"read","state":{"status":"running","args":{"filePath":"/tmp/a"}}}}}"#,
        );

        let payloads = tool_call_payloads(&sink);
        assert_eq!(payloads.len(), 1, "one start; no refresh for a repeat");
        assert_eq!(payloads[0]["args"], "/tmp/a");
    }

    /// A `tool.before`+`running` part for the SAME callID emit one start total.
    #[test]
    fn before_then_running_part_emit_single_start() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1,"kind":"tool.before","tool":"bash","sessionID":"ses1","callID":"call_x","args":{"command":"echo hi"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_x","tool":"bash","state":{"status":"running"}}}}"#,
        );
        let payloads = tool_call_payloads(&sink);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["status"], "running");
    }

    /// A tool part with `state.status == "error"` ⇒ failed when no
    /// `tool.before` established that `tool.after` is the authoritative
    /// terminal source.
    #[test]
    fn tool_state_error_emits_failed() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_e","tool":"bash","state":{"status":"running","args":{"command":"false"}}}}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_e","tool":"bash","state":{"status":"error"}}}}"#,
        );

        let payloads = tool_call_payloads(&sink);
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[1]["status"], "failed");
        assert_eq!(payloads[1]["toolUseId"], "call_e");
    }

    /// A call that exists only in the part-update stream has no later
    /// `tool.after` owner, so terminal part updates must clear cached metadata.
    #[test]
    fn part_update_only_completion_clears_tool_metadata() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_part_only","tool":"bash","state":{"status":"running","args":{"command":"echo hi"}}}}}"#,
        );
        assert!(
            dec.tool_metadata.contains_key("call_part_only"),
            "running part update caches metadata for display and command lookup",
        );

        dec.decode_line(
            r#"{"v":1,"ts":2,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_part_only","tool":"bash","state":{"status":"completed"}}}}"#,
        );

        assert!(
            !dec.tool_metadata.contains_key("call_part_only"),
            "part-update-only completion has no later owner for cached metadata",
        );
    }

    /// A `tool.after` with a non-zero `metadata.exit` ⇒ failed.
    #[test]
    fn tool_after_nonzero_exit_emits_failed() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1,"kind":"tool.before","tool":"bash","sessionID":"ses1","callID":"call_f","args":{"command":"exit 1"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2,"kind":"tool.after","tool":"bash","sessionID":"ses1","callID":"call_f","result":{"output":"boom","metadata":{"exit":2}}}"#,
        );

        let payloads = tool_call_payloads(&sink);
        assert_eq!(payloads.last().unwrap()["status"], "failed");
    }

    /// A `bash` `tool.after` running a test command ⇒ a test-run snapshot.
    #[test]
    fn bash_test_command_emits_test_run_snapshot() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().to_path_buf();
        let mut dec = decoder(&sink, Some(workspace));
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1000,"kind":"tool.before","tool":"bash","sessionID":"ses1","callID":"call_t","args":{"command":"cargo test"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2000,"kind":"tool.after","tool":"bash","sessionID":"ses1","callID":"call_t","result":{"output":"running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n","metadata":{"exit":0}}}"#,
        );

        assert_eq!(
            sink.count("test-run"),
            1,
            "passing cargo test ⇒ one snapshot"
        );
        let test_runs: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "test-run")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(test_runs[0]["runner"], "cargo");
        assert_eq!(test_runs[0]["summary"]["passed"], 1);
    }

    /// A completed `message.part.updated` can arrive before `tool.after`.
    /// Test-run parsing must still see the original bash command, and the
    /// authoritative `tool.after` result should provide the display terminal
    /// status.
    #[test]
    fn completed_part_before_tool_after_still_emits_test_run_snapshot() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().to_path_buf();
        let mut dec = decoder(&sink, Some(workspace));
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1000,"kind":"tool.before","tool":"bash","sessionID":"ses1","callID":"call_order","args":{"command":"cargo test"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":1500,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_order","tool":"bash","state":{"status":"completed"}}}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2000,"kind":"tool.after","tool":"bash","sessionID":"ses1","callID":"call_order","result":{"output":"running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n","metadata":{"exit":0}}}"#,
        );

        assert_eq!(
            sink.count("test-run"),
            1,
            "completed part before tool.after still emits a test-run snapshot",
        );
        let payloads = tool_call_payloads(&sink);
        assert_eq!(
            payloads.iter().filter(|p| p["status"] == "done").count(),
            1,
            "terminal status remains deduped",
        );
    }

    /// A completed part update is only the model-side completion signal; a
    /// later non-zero `tool.after` exit remains the authoritative display
    /// status and must not produce a preceding done event.
    #[test]
    fn completed_part_before_failing_tool_after_emits_only_failed_terminal_status() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1000,"kind":"tool.before","tool":"bash","sessionID":"ses1","callID":"call_fail_order","args":{"command":"cargo test"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":1500,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_fail_order","tool":"bash","state":{"status":"completed"}}}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2000,"kind":"tool.after","tool":"bash","sessionID":"ses1","callID":"call_fail_order","result":{"output":"test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n","metadata":{"exit":1}}}"#,
        );

        let terminal_payloads: Vec<Value> = tool_call_payloads(&sink)
            .into_iter()
            .filter(|payload| {
                payload["toolUseId"] == "call_fail_order" && payload["status"] != "running"
            })
            .collect();
        assert_eq!(terminal_payloads.len(), 1);
        assert_eq!(terminal_payloads[0]["status"], "failed");
    }

    /// Once `tool.after` provides the authoritative status, delayed terminal
    /// part updates for the same call must remain suppressed even after the
    /// metadata cache is cleared.
    #[test]
    fn completed_part_after_failing_tool_after_does_not_emit_done() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1000,"kind":"tool.before","tool":"bash","sessionID":"ses1","callID":"call_after_first","args":{"command":"cargo test"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":1500,"kind":"tool.after","tool":"bash","sessionID":"ses1","callID":"call_after_first","result":{"output":"test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n","metadata":{"exit":1}}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2000,"kind":"event","type":"message.part.updated","data":{"part":{"type":"tool","callID":"call_after_first","tool":"bash","state":{"status":"completed"}}}}"#,
        );

        let terminal_payloads: Vec<Value> = tool_call_payloads(&sink)
            .into_iter()
            .filter(|payload| {
                payload["toolUseId"] == "call_after_first" && payload["status"] != "running"
            })
            .collect();
        assert_eq!(terminal_payloads.len(), 1);
        assert_eq!(terminal_payloads[0]["status"], "failed");
    }

    /// A non-test `bash` `tool.after` ⇒ no test-run snapshot.
    #[test]
    fn non_test_bash_emits_no_test_run() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut dec = decoder(&sink, Some(tmp.path().to_path_buf()));
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1,"kind":"tool.before","tool":"bash","sessionID":"ses1","callID":"call_g","args":{"command":"git status"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2,"kind":"tool.after","tool":"bash","sessionID":"ses1","callID":"call_g","result":{"output":"clean","metadata":{"exit":0}}}"#,
        );

        assert_eq!(sink.count("test-run"), 0);
    }

    /// `on_caught_up` called twice ⇒ no duplicate replay flush (the buffered
    /// test-run is emitted exactly once).
    #[test]
    fn on_caught_up_twice_does_not_duplicate_replay_flush() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut dec = decoder(&sink, Some(tmp.path().to_path_buf()));

        // Replay phase (before any on_caught_up): a test-run is buffered.
        dec.decode_line(
            r#"{"v":1,"ts":1000,"kind":"tool.before","tool":"bash","sessionID":"ses1","callID":"call_r","args":{"command":"cargo test"}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2000,"kind":"tool.after","tool":"bash","sessionID":"ses1","callID":"call_r","result":{"output":"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n","metadata":{"exit":0}}}"#,
        );
        assert_eq!(sink.count("test-run"), 0, "buffered during replay");

        dec.on_caught_up();
        dec.on_caught_up();
        dec.on_caught_up();

        assert_eq!(sink.count("test-run"), 1, "flushed exactly once");
    }

    /// A tool call still in-flight at the replay boundary surfaces as running
    /// exactly once when `on_caught_up` fires.
    #[test]
    fn in_flight_tool_call_surfaces_running_at_caught_up() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);

        // Started during replay, never completed.
        dec.decode_line(
            r#"{"v":1,"ts":1,"kind":"tool.before","tool":"bash","sessionID":"ses1","callID":"call_live","args":{"command":"sleep 100"}}"#,
        );
        assert_eq!(sink.count("agent-tool-call"), 0, "no emit during replay");

        dec.on_caught_up();
        dec.on_caught_up(); // idempotent — must not re-emit

        let payloads = tool_call_payloads(&sink);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["status"], "running");
        assert_eq!(payloads[0]["toolUseId"], "call_live");
    }

    /// `session.updated` with a changed `info.path.cwd` ⇒ one `agent-cwd`;
    /// the same cwd does not re-emit.
    #[test]
    fn session_cwd_change_emits_agent_cwd_on_transition() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();

        dec.decode_line(
            r#"{"v":1,"ts":1,"kind":"event","type":"session.updated","data":{"info":{"id":"ses1","path":{"cwd":"/work/a"}}}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":2,"kind":"event","type":"session.updated","data":{"info":{"id":"ses1","path":{"cwd":"/work/a"}}}}"#,
        );
        dec.decode_line(
            r#"{"v":1,"ts":3,"kind":"event","type":"session.updated","data":{"info":{"id":"ses1","path":{"cwd":"/work/b"}}}}"#,
        );

        let cwds: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-cwd")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(cwds.len(), 2, "two distinct cwds; the repeat is suppressed");
        assert_eq!(cwds[0]["cwd"], "/work/a");
        assert_eq!(cwds[1]["cwd"], "/work/b");
    }

    /// End-to-end: feeding the authored `sample_bridge.jsonl` (REAL live shapes)
    /// through `start_tailing` coalesces one user turn and two settled tool calls
    /// into one replay summary. Each tool arrives as a
    /// `message.part.updated` `pending` (EMPTY args) FOLLOWED by its `tool.before`
    /// (real args) and `tool.after`, all during replay ⇒ each settles to a
    /// single summary entry carrying the AUTHORITATIVE `tool.before` args (Bug
    /// A: the args are NOT the empty `{}` from the pending part).
    ///
    /// No `test-run` is asserted: the fixture's bash `result.output` uses a
    /// Jest-style summary (`Tests: 12 passed, 12 total`), which the v1 vitest
    /// parser intentionally does not recognize (its summary line is
    /// `Tests  12 passed (12)`), so `maybe_build_snapshot` returns `None` and
    /// nothing is submitted. That is the genuine behavior for this fixture; the
    /// bash → shared test-run-parser wiring is proven against a real
    /// cargo-format output in `bash_test_command_emits_test_run_snapshot`. Uses
    /// the capturing `FakeEventSink` + `wait_for_count` for event-based sync
    /// (mirrors Codex's transcript tests).
    #[test]
    fn start_tailing_replays_sample_bridge_fixture() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("sample-project");
        std::fs::create_dir_all(&workspace).expect("mkdir workspace");

        let bridge_root = tmp.path().to_path_buf();
        let transcript_path = bridge_root.join("ses_sample001.jsonl");
        std::fs::write(&transcript_path, SAMPLE_BRIDGE).expect("write fixture transcript");

        let locator = Arc::new(OpenCodeLocator::new(
            bridge_root,
            4242,
            SystemTime::UNIX_EPOCH,
        ));

        let handle = start_tailing(
            sink.clone(),
            "sid-sample".to_string(),
            Some(workspace.clone()),
            transcript_path,
            locator,
        )
        .expect("tailing should start");

        assert!(
            sink.wait_for_count("agent-replay-summary", 1, Duration::from_secs(5)),
            "expected one coalesced replay summary",
        );
        handle.stop();

        assert_eq!(sink.count("agent-turn"), 0);
        assert_eq!(sink.count("agent-tool-call"), 0);
        let lifecycle: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-lifecycle")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(lifecycle.len(), 1);
        assert_eq!(lifecycle[0]["phase"], "idle");
        let summaries: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-replay-summary")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(summaries.len(), 1);
        let summary = &summaries[0];
        assert_eq!(summary["sessionId"], "sid-sample");
        assert_eq!(summary["numTurns"], 1);
        assert_eq!(summary["toolCallTotal"], 2);
        assert_eq!(summary["toolCallByType"]["bash"], 1);
        assert_eq!(summary["toolCallByType"]["read"], 1);

        // The fixture's bash tool (call_bash001): pending(empty) → tool.before →
        // tool.after, all during replay ⇒ a single settled `done` carrying the
        // authoritative `tool.before` args (Bug A regression — NOT `{}`).
        let recent = summary["recentToolCalls"]
            .as_array()
            .expect("recent tool calls");
        let bash_calls: Vec<&Value> = recent
            .iter()
            .filter(|p| p["toolUseId"] == "call_bash001")
            .collect();
        assert_eq!(bash_calls.len(), 1, "settled bash tool ⇒ one done");
        assert_eq!(bash_calls[0]["status"], "done");
        assert_eq!(bash_calls[0]["tool"], "bash");
        assert_eq!(bash_calls[0]["args"], "npm test");

        // The non-bash read tool (call_read001): same ordering, args from
        // `tool.before` (filePath), proving the fix covers non-bash tools too.
        let read_calls: Vec<&Value> = recent
            .iter()
            .filter(|p| p["toolUseId"] == "call_read001")
            .collect();
        assert_eq!(read_calls.len(), 1, "settled read tool ⇒ one done");
        assert_eq!(read_calls[0]["status"], "done");
        assert_eq!(read_calls[0]["tool"], "read");
        assert_eq!(read_calls[0]["args"], "/tmp/sample-project/src/util.ts");

        // The Jest-style fixture output is unparseable to the vitest runner, so
        // no test-run snapshot is produced (see the test docstring).
        assert_eq!(sink.count("test-run"), 0);
    }

    #[test]
    fn ts_to_iso8601_formats_epoch_ms() {
        // 1781965827596 ms ≈ 2026-06-18 (well-defined UTC date/time).
        let iso = ts_to_iso8601(Some(1_781_965_827_596));
        assert!(iso.starts_with("2026-"), "got: {iso}");
        assert!(iso.ends_with('Z'));
        // None falls back to a now-stamp, still ISO-shaped.
        assert!(ts_to_iso8601(None).ends_with('Z'));
    }

    #[test]
    fn truncate_string_caps_and_marks() {
        assert_eq!(truncate_string("short", 1024), "short");
        let long = "x".repeat(2000);
        let out = truncate_string(&long, 1024);
        assert!(out.ends_with("..."));
        assert_eq!(out.chars().count(), 1024);
    }

    #[test]
    fn malformed_line_is_skipped() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();
        dec.decode_line("this is not json");
        dec.decode_line(r#"{"v":1,"kind":"totally.unknown"}"#);
        assert_eq!(sink.count("agent-turn"), 0);
        assert_eq!(sink.count("agent-tool-call"), 0);
    }

    // --- agent-reply capture (VIM-293) ---

    fn assistant_text_line(text: &str) -> String {
        serde_json::json!({
            "v": 1,
            "ts": 1_781_965_830_000_i64,
            "kind": "event",
            "type": "assistant.text",
            "data": { "sessionID": "ses_sample001", "text": text }
        })
        .to_string()
    }

    const OPENCODE_SENTINEL_REPLY: &str = "prose before\n<<<VIMEFLOW_REPLY\n{\"v\":1,\"nonce\":\"oc4242\",\"replies\":[{\"id\":1,\"status\":\"resolved\",\"text\":\"fixed\"}]}\nVIMEFLOW_REPLY>>>";

    #[test]
    fn assistant_text_with_sentinel_emits_one_reply() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();
        dec.decode_line(&assistant_text_line(OPENCODE_SENTINEL_REPLY));

        let replies: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-reply")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["nonce"], "oc4242");
        assert_eq!(replies[0]["sessionId"], "sid");
    }

    #[test]
    fn assistant_text_without_sentinel_emits_nothing() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        dec.on_caught_up();
        dec.decode_line(&assistant_text_line("just a plain answer"));
        assert_eq!(sink.count("agent-reply"), 0);
    }

    #[test]
    fn replayed_assistant_text_never_re_emits() {
        let sink = Arc::new(FakeEventSink::new());
        let mut dec = decoder(&sink, None);
        // Before caught-up = historic replay.
        dec.decode_line(&assistant_text_line(OPENCODE_SENTINEL_REPLY));
        dec.on_caught_up();
        assert_eq!(sink.count("agent-reply"), 0);
        // Live line after caught-up emits.
        dec.decode_line(&assistant_text_line(OPENCODE_SENTINEL_REPLY));
        assert_eq!(sink.count("agent-reply"), 1);
    }
}
