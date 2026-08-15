//! Transcript tailer for kimi-code `wire.jsonl` files.
//!
//! Tails the persisted `wire.jsonl` and emits `agent-tool-call` /
//! `agent-turn` / `agent-lifecycle` events. Mirrors `codex/transcript.rs`
//! structure (validate-under-root + `TranscriptTailService` driven by a
//! per-session [`KimiTranscriptDecoder`]) without the codex test-runner
//! machinery (deferred per the kimi state spec).

use std::cmp::{Ordering as CmpOrdering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime};

use super::transcript_dto::{KimiLineDto, KimiLoopEventType, KimiRecordType};
use super::KimiLocator;
use crate::agent::adapter::base::{
    for_each_bounded_line, TranscriptDecoder, TranscriptHandle, TranscriptTailService,
};
use crate::agent::adapter::claude_code::test_runners::timestamps::compute_duration_ms;
use crate::agent::adapter::types::{stamp_snapshot, StatusSnapshot, ValidateTranscriptError};
use crate::agent::events::{
    emit_agent_cwd, emit_agent_replay_summary, emit_agent_reply, emit_agent_review,
    emit_agent_status, emit_agent_tool_call, emit_agent_turn, record_lifecycle, record_tool_call,
    ReplayActivity,
};
use crate::agent::reply::{extract_agent_reply, map_agent_reply_outcome, AgentReplyOutcome};
use crate::agent::review::{extract_agent_review, map_review_outcome, AgentReviewOutcome};
use crate::agent::types::{
    AgentCwdEvent, AgentPhase, AgentReplyEvent, AgentReviewEvent, AgentToolCallEvent,
    AgentTurnEvent, ToolCallStatus,
};
use crate::runtime::EventSink;

/// Maximum length for the args summary string.
const MAX_ARGS_LEN: usize = 1024;

/// Per-turn reply-text buffer cap (VIM-293) — tail-kept, since the reply
/// contract puts the sentinel block at the end of the turn. Matches the
/// opencode bridge plugin's `MAX_REPLY_TEXT`.
const MAX_TURN_TEXT_BYTES: usize = 32 * 1024;
const MAX_RECOVERY_BYTES_PER_PATH: u64 = 128 * 1024;
const MAX_RECOVERY_COMPLETED_TURNS: usize = 512;
const MAX_RECOVERED_REPLY_EVENTS: usize = 128;

/// How often the session supervisor rescans `state.json` for newly-spawned
/// sub-agents, and the tick at which it checks the stop flag while waiting.
const KIMI_AGENT_SCAN_INTERVAL_MS: u64 = 750;
const KIMI_SCAN_TICK_MS: u64 = 250;

struct InFlightToolCall {
    started_at: Instant,
    started_at_iso: String,
    tool: String,
    args: String,
    is_test_file: bool,
}

type InFlightToolCalls = HashMap<String, InFlightToolCall>;
type ChildTailers = HashMap<PathBuf, (Arc<AtomicBool>, JoinHandle<()>)>;

#[derive(Default)]
struct ReplayCoordinatorState {
    expected: usize,
    completed: usize,
    num_turns: u32,
    cwd: Option<String>,
    activity: ReplayActivity,
    flushed: bool,
}

/// Joins the main and sub-agent wire replays into one PTY-level summary. A
/// clean EOF is a session-wide barrier: every initial decoder drains running
/// calls first, the last decoder emits the aggregate summary, then all tailers
/// resume live delivery together.
struct KimiReplayCoordinator {
    events: Arc<dyn EventSink>,
    session_id: String,
    state: Mutex<ReplayCoordinatorState>,
    flushed: Condvar,
}

impl KimiReplayCoordinator {
    fn new(events: Arc<dyn EventSink>, session_id: String, cwd: Option<String>) -> Self {
        Self {
            events,
            session_id,
            state: Mutex::new(ReplayCoordinatorState {
                cwd,
                ..ReplayCoordinatorState::default()
            }),
            flushed: Condvar::new(),
        }
    }

    /// Register before spawning the decoder. Once the first generation has
    /// flushed, newly discovered wires are live and must not start a new
    /// replacement summary generation.
    fn register(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.flushed {
            return false;
        }
        state.expected = state.expected.saturating_add(1);
        true
    }

    fn finish(
        &self,
        activity: ReplayActivity,
        num_turns: u32,
        cwd: Option<String>,
        stop: Option<&AtomicBool>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
            return;
        }

        state.activity.merge(activity);
        state.num_turns = state.num_turns.saturating_add(num_turns);
        if cwd.is_some() {
            state.cwd = cwd;
        }
        state.completed = state.completed.saturating_add(1);

        if state.completed == state.expected {
            if stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
                return;
            }
            let summary = std::mem::take(&mut state.activity).into_summary(
                self.session_id.clone(),
                state.num_turns,
                state.cwd.clone(),
            );
            if summary.tool_call_total > 0 || summary.num_turns > 0 || summary.cwd.is_some() {
                if let Err(e) = emit_agent_replay_summary(self.events.as_ref(), &summary) {
                    log::warn!("Failed to emit agent-replay-summary event: {}", e);
                }
            }
            state.flushed = true;
            self.flushed.notify_all();
            return;
        }

        while !state.flushed {
            if stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
                return;
            }
            let (next, _) = self
                .flushed
                .wait_timeout(state, std::time::Duration::from_millis(50))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
        }
    }
}

/// Dedupe key for supervisor `agent-status` refreshes: the token metrics that
/// move the context display, plus the effective rate-limit values so a
/// plan-usage refresh re-emits even when tokens are unchanged (an idle session
/// after a `/usages` fetch lands). Percentages are compared as `f64::to_bits`.
#[derive(PartialEq)]
struct StatusSignature {
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_creation: u64,
    // (used_percentage bits, resets_at) for the 5-hour and weekly windows.
    five_hour: (u64, u64),
    seven_day: (u64, u64),
    // A landed fetch can carry placeholder-identical values (0% / no reset), so
    // the flip from default to fetched must itself break the dedupe — else the
    // gate never learns the fetch succeeded.
    usage_fetched: bool,
}

/// Validate a raw transcript path (null-byte check + canonicalize-under-root)
/// against a caller-supplied root. `KimiAdapter` passes the locator's
/// effective home so the trust root matches the per-process `KIMI_CODE_HOME`
/// the locator resolved.
pub(super) fn validate_transcript_path_with_root(
    transcript_path: &str,
    kimi_root: &Path,
) -> Result<PathBuf, ValidateTranscriptError> {
    if transcript_path.bytes().any(|b| b == 0) {
        return Err(ValidateTranscriptError::InvalidPath(
            "transcript path contains null byte".to_string(),
        ));
    }

    validate_transcript_path_under_root(transcript_path, kimi_root)
}

fn validate_transcript_path_under_root(
    transcript_path: &str,
    kimi_root: &Path,
) -> Result<PathBuf, ValidateTranscriptError> {
    let path = PathBuf::from(transcript_path);
    let canonical = fs::canonicalize(&path).map_err(|e| {
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

    let kimi_root = fs::canonicalize(kimi_root).map_err(|e| {
        ValidateTranscriptError::Other(format!(
            "cannot resolve kimi transcript root '{}': {}",
            kimi_root.display(),
            e
        ))
    })?;

    if !canonical.starts_with(&kimi_root) {
        return Err(ValidateTranscriptError::OutsideRoot {
            path: canonical,
            root: kimi_root,
        });
    }

    Ok(canonical)
}

pub(super) fn start_tailing(
    events: Arc<dyn EventSink>,
    session_id: String,
    transcript_path: PathBuf,
    cwd: Option<PathBuf>,
    locator: Arc<KimiLocator>,
) -> Result<TranscriptHandle, String> {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = stop_flag.clone();
    let summary_cwd = cwd.as_deref().and_then(Path::to_str).map(str::to_string);

    let Some(session_dir) = session_dir_from_wire(&transcript_path) else {
        // No resolvable session dir — tail the single wire (legacy path).
        let file = open_wire(&transcript_path)?;
        let decoder = KimiTranscriptDecoder::new_with_cwd(
            events,
            session_id,
            String::new(),
            String::new(),
            summary_cwd,
        );
        let service = TranscriptTailService::new(Box::new(decoder), "kimi wire transcript");
        let join_handle = std::thread::spawn(move || {
            service.run(BufReader::new(file), stop_clone);
        });
        return Ok(TranscriptHandle::new(stop_flag, join_handle));
    };

    // Surface the resolved workspace cwd once so the pane / file tree / git
    // follow the kimi project rather than the stale spawn cwd.
    emit_initial_cwd(events.as_ref(), &session_id, cwd.as_deref());

    let join_handle = std::thread::spawn(move || {
        run_session_supervisor(
            events,
            session_id,
            session_dir,
            transcript_path,
            summary_cwd,
            locator,
            stop_clone,
        );
    });
    Ok(TranscriptHandle::new(stop_flag, join_handle))
}

fn open_recovery_file(path: &Path, kimi_root: &Path) -> Result<Option<(PathBuf, File)>, String> {
    let canonical =
        match validate_transcript_path_with_root(path.to_string_lossy().as_ref(), kimi_root) {
            Ok(canonical) => canonical,
            Err(_) => return Ok(None),
        };
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to inspect Kimi transcript for recovery: {}: {}",
                path.display(),
                error
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(None);
    }

    let mut options = OpenOptions::new();
    options.read(true);
    crate::filesystem::scope::open_nofollow(path, options).map(|file| Some((canonical, file)))
}

struct RecoveredTurn {
    key: (u64, usize, usize),
    text: String,
}

impl PartialEq for RecoveredTurn {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for RecoveredTurn {}

impl PartialOrd for RecoveredTurn {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for RecoveredTurn {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.key.cmp(&other.key)
    }
}

fn retain_recovered_turn(turns: &mut BinaryHeap<Reverse<RecoveredTurn>>, turn: RecoveredTurn) {
    turns.push(Reverse(turn));
    if turns.len() > MAX_RECOVERY_COMPLETED_TURNS {
        turns.pop();
    }
}

fn recover_path_turns(mut file: File, path_ordinal: usize) -> Result<Vec<RecoveredTurn>, String> {
    let file_len = file
        .metadata()
        .map_err(|error| format!("Failed to inspect Kimi transcript recovery window: {error}"))?
        .len();
    let start = file_len.saturating_sub(MAX_RECOVERY_BYTES_PER_PATH);
    let starts_at_line_boundary = if start == 0 {
        true
    } else {
        file.seek(SeekFrom::Start(start - 1))
            .and_then(|_| {
                let mut preceding = [0u8; 1];
                file.read_exact(&mut preceding)?;
                Ok(preceding[0] == b'\n')
            })
            .map_err(|error| format!("Failed to seek Kimi transcript recovery window: {error}"))?
    };
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("Failed to seek Kimi transcript recovery window: {error}"))?;

    let mut reader = BufReader::new(file.take(MAX_RECOVERY_BYTES_PER_PATH));
    if !starts_at_line_boundary {
        let mut partial = Vec::new();
        reader
            .read_until(b'\n', &mut partial)
            .map_err(|error| format!("Failed to align Kimi transcript recovery window: {error}"))?;
    }
    // A bounded tail can begin after this turn's prompt. Treat that leading
    // fragment as an active turn so a sentinel near EOF remains recoverable.
    let mut in_user_turn = start > 0;
    let mut turn_text = String::new();
    let mut completed = BinaryHeap::new();
    let mut line_ordinal = 0;

    for_each_bounded_line(reader, "Kimi transcript recovery", |line| {
        let current_line = line_ordinal;
        line_ordinal += 1;
        let Ok(dto) = serde_json::from_str::<KimiLineDto>(line) else {
            return;
        };
        match dto.record_type() {
            KimiRecordType::TurnPrompt => {
                if dto
                    .origin
                    .as_ref()
                    .and_then(|origin| origin.kind.as_deref())
                    == Some("user")
                {
                    in_user_turn = true;
                    turn_text.clear();
                }
            }
            KimiRecordType::AppendLoopEvent if in_user_turn => {
                let Some(event) = dto.event.as_ref() else {
                    return;
                };
                match event.loop_event_type() {
                    KimiLoopEventType::ContentPart => {
                        let Some(text) = event
                            .part
                            .as_ref()
                            .filter(|part| part.type_tag.as_deref() == Some("text"))
                            .and_then(|part| part.text.as_deref())
                            .filter(|text| !text.is_empty())
                        else {
                            return;
                        };
                        if !turn_text.is_empty() {
                            turn_text.push('\n');
                        }
                        turn_text.push_str(text);
                        clamp_turn_text(&mut turn_text);
                    }
                    KimiLoopEventType::StepEnd
                        if event.finish_reason.as_deref() == Some("end_turn") =>
                    {
                        retain_recovered_turn(
                            &mut completed,
                            RecoveredTurn {
                                key: (dto.time.unwrap_or(0), path_ordinal, current_line),
                                text: std::mem::take(&mut turn_text),
                            },
                        );
                        in_user_turn = false;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    })
    .map_err(|error| format!("Failed to read Kimi transcript recovery window: {}", error))?;

    Ok(completed.into_iter().map(|Reverse(turn)| turn).collect())
}

fn recover_turn_texts(paths: &[PathBuf], kimi_root: &Path) -> Result<VecDeque<String>, String> {
    let mut seen = HashSet::new();
    let mut completed = BinaryHeap::new();
    for (path_ordinal, path) in paths.iter().enumerate() {
        let Some((canonical, file)) = open_recovery_file(path, kimi_root)? else {
            continue;
        };
        if !seen.insert(canonical) {
            continue;
        }
        for turn in recover_path_turns(file, path_ordinal)? {
            retain_recovered_turn(&mut completed, turn);
        }
    }
    let mut completed: Vec<_> = completed.into_iter().map(|Reverse(turn)| turn).collect();
    completed.sort_by_key(|turn| turn.key);
    Ok(completed.into_iter().map(|turn| turn.text).collect())
}

fn extract_turn_protocols(text: &str) -> (Option<AgentReplyOutcome>, Option<AgentReviewOutcome>) {
    let reply = extract_agent_reply(text);
    let review = extract_agent_review(text);
    if reply.is_some() && review.is_some() {
        return (None, None);
    }
    (reply, review)
}

pub(super) fn recover_replies(
    transcript_paths: &[PathBuf],
    kimi_root: &Path,
    session_id: &str,
    nonces: &HashSet<String>,
) -> Result<Vec<AgentReplyEvent>, String> {
    let mut recovered = VecDeque::new();
    for text in recover_turn_texts(transcript_paths, kimi_root)? {
        let (Some(outcome), None) = extract_turn_protocols(&text) else {
            continue;
        };
        let event = map_agent_reply_outcome(session_id, outcome);
        if event
            .nonce
            .as_ref()
            .is_some_and(|nonce| nonces.contains(nonce))
        {
            recovered.push_back(event);
            if recovered.len() > MAX_RECOVERED_REPLY_EVENTS {
                recovered.pop_front();
            }
        }
    }
    Ok(recovered.into())
}

pub(super) fn recover_reviews(
    transcript_paths: &[PathBuf],
    kimi_root: &Path,
    session_id: &str,
    nonces: &HashSet<String>,
) -> Result<Vec<AgentReviewEvent>, String> {
    let mut pending = nonces.clone();
    let mut recovered = Vec::new();
    for text in recover_turn_texts(transcript_paths, kimi_root)? {
        let (None, Some(outcome)) = extract_turn_protocols(&text) else {
            continue;
        };
        let event = map_review_outcome(session_id, outcome);
        if event
            .nonce
            .as_ref()
            .is_some_and(|nonce| pending.remove(nonce))
        {
            recovered.push(event);
        }
    }
    Ok(recovered)
}

fn clamp_turn_text(text: &mut String) {
    if text.len() <= MAX_TURN_TEXT_BYTES {
        return;
    }
    let excess = text.len() - MAX_TURN_TEXT_BYTES;
    let utf8_cut = (excess..text.len())
        .find(|&index| text.is_char_boundary(index))
        .unwrap_or(excess);
    let cut = if utf8_cut == 0 || text.as_bytes()[utf8_cut - 1] == b'\n' {
        utf8_cut
    } else {
        text[utf8_cut..]
            .find('\n')
            .map_or(text.len(), |newline| utf8_cut + newline + 1)
    };
    text.drain(..cut);
}

/// `wire.jsonl` path is `<session>/agents/<agent-id>/wire.jsonl`; three up is
/// the session dir, which lets the supervisor follow every sibling agent wire.
fn session_dir_from_wire(wire: &Path) -> Option<PathBuf> {
    wire.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

/// kimi's `session_*` id from a session dir (its final path component); empty
/// when it can't be read.
fn session_id_from_dir(session_dir: &Path) -> String {
    session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_default()
}

/// Maximum mtime of `state.json` and every known agent wire in `session_dir`.
/// Cheap metadata walk used to skip the full transcript reparse on idle polls
/// when none of the source files have changed.
fn session_source_mtime(session_dir: &Path) -> Option<SystemTime> {
    let state_path = session_dir.join("state.json");
    let mut max_mtime = std::fs::metadata(&state_path)
        .and_then(|m| m.modified())
        .ok()?;
    let wires = super::parser::read_agent_wires(session_dir)?;
    for wire in &wires {
        if let Ok(m) = std::fs::metadata(&wire.wire) {
            if let Ok(mtime) = m.modified() {
                if mtime > max_mtime {
                    max_mtime = mtime;
                }
            }
        }
    }
    Some(max_mtime)
}

/// Re-aggregate the session status and emit `agent-status` when it changed.
/// The shared watcher only re-decodes on MAIN-wire writes, so during
/// delegated work (sub-agent-only token writes) the supervisor is what keeps
/// the context/token metrics live. Deduped on a cheap token + rate-limit
/// signature.
///
/// `last_snapshot` / `last_mtime` cache the previous parse so idle polls do
/// not re-read and re-parse the full `wire.jsonl` files when nothing has
/// changed; the cheaper rate-limit merge still runs every poll so a pending
/// plan-usage fetch still lands as soon as it arrives.
fn emit_session_status(
    events: &dyn EventSink,
    session_id: &str,
    session_dir: &Path,
    locator: &KimiLocator,
    last: &mut Option<StatusSignature>,
    last_snapshot: &mut Option<StatusSnapshot>,
    last_turn_count: &mut Option<u64>,
    last_mtime: &mut Option<SystemTime>,
) {
    let current_mtime = session_source_mtime(session_dir);
    let sources_changed = match (&current_mtime, &*last_mtime) {
        (Some(cur), Some(last)) => cur != last,
        _ => true,
    };

    let (mut snapshot, settled_turn_count) = if sources_changed || last_snapshot.is_none() {
        let Some(snapshot) = super::parser::parse_session_aggregate(session_dir) else {
            return;
        };
        // Count SETTLED turns only when the source files actually changed — the
        // count is expensive (full main-wire parse) and unchanged files guarantee
        // an unchanged count. Cache it alongside the snapshot for idle polls.
        let settled_turn_count = if locator.usage_consented() {
            super::parser::main_settled_turn_count(session_dir)
        } else {
            0
        };
        *last_snapshot = Some(snapshot.clone());
        *last_turn_count = Some(settled_turn_count);
        *last_mtime = current_mtime;
        (snapshot, settled_turn_count)
    } else {
        (last_snapshot.clone().unwrap(), last_turn_count.unwrap_or(0))
    };

    // Drive the turn-debounced usage fetch every poll so an idle session — or
    // consent just enabled on an idle pane — still fetches without a fresh
    // prompt. Gate the (full main-wire parse) count behind consent so the default
    // opt-out path pays nothing; while OFF, only re-arm the catch-up.
    if locator.usage_consented() {
        locator.maybe_refresh_usage(settled_turn_count, &snapshot.version);
    } else {
        locator.disarm_usage();
    }
    // Merge the fetched plan-usage (consent-gated; `None` when OFF or not yet
    // fetched). Without this the supervisor would re-stamp the parser's zeroed
    // default and overwrite the bars the watcher's decode just filled — and on
    // an idle session this poll is the only thing that pushes a fetch result.
    // `usage_fetched` flags a real value so the gate tells LOADING from ON.
    let cached = locator.cached_rate_limits();
    snapshot.usage_fetched = cached.is_some();
    if let Some(rate_limits) = cached {
        snapshot.rate_limits = rate_limits;
    }
    // Include cache tokens: a sub-agent turn often changes only cache read /
    // creation, which still moves the context % + cache display — dropping
    // them here would suppress the refresh and leave the card stale.
    let usage = snapshot.context_window.current_usage.as_ref();
    let rate_limits = &snapshot.rate_limits;
    let signature = StatusSignature {
        model: snapshot.model_id.clone(),
        input_tokens: snapshot.context_window.total_input_tokens,
        output_tokens: snapshot.context_window.total_output_tokens,
        cache_read: usage.map_or(0, |u| u.cache_read_input_tokens),
        cache_creation: usage.map_or(0, |u| u.cache_creation_input_tokens),
        five_hour: (
            rate_limits.five_hour.used_percentage.to_bits(),
            rate_limits.five_hour.resets_at,
        ),
        seven_day: rate_limits.seven_day.as_ref().map_or((0, 0), |week| {
            (week.used_percentage.to_bits(), week.resets_at)
        }),
        usage_fetched: snapshot.usage_fetched,
    };
    if last.as_ref() == Some(&signature) {
        return;
    }
    *last = Some(signature);
    let event = stamp_snapshot(session_id, snapshot);
    if let Err(e) = emit_agent_status(events, &event) {
        log::warn!("Failed to emit kimi agent-status event: {}", e);
    }
}

fn open_wire(path: &Path) -> Result<File, String> {
    File::open(path).map_err(|e| {
        format!(
            "Failed to open kimi wire transcript: {}: {}",
            path.display(),
            e
        )
    })
}

/// Emit a single `agent-cwd` from the resolved kimi cwd so the rest of the
/// workspace tracks the project the agent is actually in.
fn emit_initial_cwd(events: &dyn EventSink, session_id: &str, cwd: Option<&Path>) {
    let Some(cwd) = cwd.and_then(Path::to_str) else {
        return;
    };
    let event = AgentCwdEvent {
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
    };
    if let Err(e) = emit_agent_cwd(events, &event) {
        log::warn!("Failed to emit kimi agent-cwd event: {}", e);
    }
}

/// Follow every `agents/*/wire.jsonl` of a kimi session: discover agents from
/// `state.json` (sub-agents appear when a turn delegates), spawn a per-agent
/// tail thread, and tear them all down on stop. A sub-agent's tool ids are
/// prefixed with its agent id so they don't collide with main's in the feed.
fn run_session_supervisor(
    events: Arc<dyn EventSink>,
    session_id: String,
    session_dir: PathBuf,
    main_wire: PathBuf,
    cwd: Option<String>,
    locator: Arc<KimiLocator>,
    stop: Arc<AtomicBool>,
) {
    // `read_agent_wires` canonicalizes discovered agent wires, so the main
    // wire must be canonicalized too or symlinked session/home layouts can
    // cause the same physical file to appear as two PathBuf values and get
    // tailed twice (duplicate events + inflated turn counts).
    let mut main_wire = fs::canonicalize(&main_wire).unwrap_or(main_wire);
    let mut session_dir = session_dir_from_wire(&main_wire).unwrap_or(session_dir);

    // Keyed by wire path so the always-tailed main wire is never double-spawned
    // when `state.json` lists it too.
    let mut children: ChildTailers = HashMap::new();
    let mut agent_session_id = session_id_from_dir(&session_dir);
    let mut last_status: Option<StatusSignature> = None;
    let mut last_status_snapshot: Option<StatusSnapshot> = None;
    let mut last_status_turn_count: Option<u64> = None;
    let mut last_status_mtime: Option<SystemTime> = None;
    let mut replay = Arc::new(KimiReplayCoordinator::new(
        events.clone(),
        session_id.clone(),
        cwd.clone(),
    ));
    // Track the refresh generation so a UI-requested refresh forces one fetch
    // (the start value is already covered by the attach catch-up).
    let mut acted_refresh_gen = crate::agent::kimi_usage_consent::refresh_gen();

    loop {
        if let Some(located) = locator.refresh_located_source() {
            let recovery_path = located.status_path.clone();
            let next_main_wire = fs::canonicalize(&located.status_path)
                .unwrap_or_else(|_| located.status_path.clone());
            if let Some(next_session_dir) = session_dir_from_wire(&next_main_wire) {
                let changed = next_main_wire != main_wire || next_session_dir != session_dir;
                if changed {
                    locator.remember_recovery_path(recovery_path);
                    super::kdbg(&format!(
                        "SUPERVISOR switch session old={} new={} main_wire={}",
                        session_dir.display(),
                        next_session_dir.display(),
                        next_main_wire.display()
                    ));
                    stop_child_tailers(&mut children);
                    session_dir = next_session_dir;
                    main_wire = next_main_wire;
                    agent_session_id = session_id_from_dir(&session_dir);
                    last_status = None;
                    last_status_snapshot = None;
                    last_status_turn_count = None;
                    last_status_mtime = None;
                    replay = Arc::new(KimiReplayCoordinator::new(
                        events.clone(),
                        session_id.clone(),
                        cwd.clone(),
                    ));
                    locator.disarm_usage();
                }
            }
        }

        // Always tail the main wire; add sub-agent wires as `state.json`
        // reveals them (a `/init`-style delegation spawns `agent-0`
        // mid-session). When there is no `state.json`, only main is tailed.
        let mut targets: Vec<(PathBuf, String)> = vec![(main_wire.clone(), String::new())];
        if let Some(wires) = super::parser::read_agent_wires(&session_dir) {
            for wire in wires {
                if wire.wire == main_wire {
                    continue;
                }
                let prefix = if wire.is_main {
                    String::new()
                } else {
                    format!("{}:", wire.agent_id)
                };
                targets.push((wire.wire, prefix));
            }
        }
        let mut pending_paths = HashSet::new();
        let pending: Vec<_> = targets
            .into_iter()
            .filter(|(wire, _)| !children.contains_key(wire) && pending_paths.insert(wire.clone()))
            .filter_map(|(wire, prefix)| File::open(&wire).ok().map(|file| (wire, prefix, file)))
            .collect();
        // Register the whole batch before spawning any thread so a fast main
        // decoder cannot flush while a sibling wire is still being enrolled.
        let replaying: Vec<_> = pending.iter().map(|_| replay.register()).collect();
        for ((wire, prefix, file), replaying) in pending.into_iter().zip(replaying) {
            let child_stop = Arc::new(AtomicBool::new(false));
            let decoder_cwd = if prefix.is_empty() { cwd.clone() } else { None };
            let decoder = KimiTranscriptDecoder::with_replay(
                events.clone(),
                session_id.clone(),
                agent_session_id.clone(),
                prefix,
                decoder_cwd,
                replay.clone(),
                replaying.then(|| child_stop.clone()),
                !replaying,
            );
            let service = TranscriptTailService::new(Box::new(decoder), "kimi wire transcript");
            let child_stop_run = child_stop.clone();
            let join = std::thread::spawn(move || {
                service.run(BufReader::new(file), child_stop_run);
            });
            children.insert(wire, (child_stop, join));
        }

        // A UI refresh request re-arms the usage fetch (clears the turn
        // debounce) so the next emit re-attempts even within the same turn.
        let gen = crate::agent::kimi_usage_consent::refresh_gen();
        if gen != acted_refresh_gen {
            locator.disarm_usage();
            acted_refresh_gen = gen;
        }

        // Refresh the aggregated status so context/tokens (and the fetched
        // plan-usage) stay live during sub-agent-only writes — and during idle,
        // where this poll is the only thing that pushes a landed usage fetch.
        emit_session_status(
            events.as_ref(),
            &session_id,
            &session_dir,
            locator.as_ref(),
            &mut last_status,
            &mut last_status_snapshot,
            &mut last_status_turn_count,
            &mut last_status_mtime,
        );

        // Poll for newly-spawned sub-agents; check stop on a short tick so
        // teardown stays prompt.
        let mut slept = 0;
        while slept < KIMI_AGENT_SCAN_INTERVAL_MS && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(KIMI_SCAN_TICK_MS));
            slept += KIMI_SCAN_TICK_MS;
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }

    stop_child_tailers(&mut children);
}

fn stop_child_tailers(children: &mut ChildTailers) {
    for (child_stop, _) in children.values() {
        child_stop.store(true, Ordering::Release);
    }
    for (_, (_, join)) in children.drain() {
        let _ = join.join();
    }
}

/// Per-session kimi decoder: owns the in-flight tool-call map, turn count, and
/// replay accumulators, turning each complete `wire.jsonl` line into live
/// `agent-*` events or one catch-up summary.
struct KimiTranscriptDecoder {
    events: Arc<dyn EventSink>,
    session_id: String,
    /// kimi's own `session_*` id, used as the lifecycle `agentSessionId` so
    /// the frontend's stale-event guard can tell an old run in this pane from
    /// a new one (the PTY id is identical across runs). Empty when unknown.
    agent_session_id: String,
    /// Prefix applied to emitted `tool_use_id`s so a sub-agent's tool calls
    /// can't collide with main's (or another sub-agent's) in the shared feed.
    /// Empty for the `main` agent.
    agent_prefix: String,
    in_flight: InFlightToolCalls,
    num_turns: u32,
    /// Latest cwd reported by a main-wire Bash call. During replay this is
    /// folded into the summary; live transitions emit `agent-cwd` directly.
    last_cwd: Option<String>,
    /// Main-wire assistant text parts for the CURRENT turn (VIM-293) — whole
    /// blocks, joined at `step.end`/`end_turn` and drained per turn.
    turn_text: String,
    /// Last non-empty normalized text part; becomes the completion body.
    notification_body: Option<String>,
    last_phase: Option<AgentPhase>,
    replay_phase: Option<AgentPhase>,
    /// Historical tool calls are folded here until the first clean EOF.
    replay_activity: ReplayActivity,
    replay: Arc<KimiReplayCoordinator>,
    replay_stop: Option<Arc<AtomicBool>>,
    replay_done: bool,
}

impl KimiTranscriptDecoder {
    #[cfg(test)]
    fn new(
        events: Arc<dyn EventSink>,
        session_id: String,
        agent_session_id: String,
        agent_prefix: String,
    ) -> Self {
        Self::new_with_cwd(events, session_id, agent_session_id, agent_prefix, None)
    }

    fn new_with_cwd(
        events: Arc<dyn EventSink>,
        session_id: String,
        agent_session_id: String,
        agent_prefix: String,
        cwd: Option<String>,
    ) -> Self {
        let replay = Arc::new(KimiReplayCoordinator::new(
            events.clone(),
            session_id.clone(),
            cwd.clone(),
        ));
        let replaying = replay.register();
        debug_assert!(replaying);
        Self::with_replay(
            events,
            session_id,
            agent_session_id,
            agent_prefix,
            cwd,
            replay,
            None,
            false,
        )
    }

    fn with_replay(
        events: Arc<dyn EventSink>,
        session_id: String,
        agent_session_id: String,
        agent_prefix: String,
        cwd: Option<String>,
        replay: Arc<KimiReplayCoordinator>,
        replay_stop: Option<Arc<AtomicBool>>,
        replay_done: bool,
    ) -> Self {
        Self {
            events,
            session_id,
            agent_session_id,
            agent_prefix,
            in_flight: HashMap::new(),
            num_turns: 0,
            last_cwd: cwd,
            turn_text: String::new(),
            notification_body: None,
            last_phase: None,
            replay_phase: None,
            replay_activity: ReplayActivity::default(),
            replay,
            replay_stop,
            replay_done,
        }
    }

    /// Namespace a wire `tool_call_id` with this agent's prefix.
    fn tool_use_id(&self, call_id: &str) -> String {
        format!("{}{}", self.agent_prefix, call_id)
    }

    /// Only the main agent drives session-level turns and lifecycle; a
    /// sub-agent (non-empty prefix) contributes tool calls alone, so its
    /// `end_turn` can't flip the whole pane idle while main is still running.
    fn is_main(&self) -> bool {
        self.agent_prefix.is_empty()
    }
}

impl TranscriptDecoder for KimiTranscriptDecoder {
    fn decode_line(&mut self, line: &str) {
        let dto: KimiLineDto = match serde_json::from_str(line) {
            Ok(dto) => dto,
            Err(_) => return,
        };

        match dto.record_type() {
            KimiRecordType::TurnPrompt => {
                self.process_turn_prompt(&dto);
            }
            KimiRecordType::AppendLoopEvent => {
                self.process_loop_event(&dto);
            }
            _ => {}
        }
    }

    fn on_caught_up(&mut self) {
        if !self.replay_done {
            if let Some(phase) = self.replay_phase.take() {
                let mut last = self.last_phase;
                crate::agent::events::emit_lifecycle_on_change(
                    self.events.as_ref(),
                    &self.session_id,
                    &self.agent_session_id,
                    &mut last,
                    phase,
                );
                self.last_phase = last;
            }
            for event in self.replay_activity.take_running() {
                if let Err(e) = emit_agent_tool_call(self.events.as_ref(), &event) {
                    log::warn!("Failed to emit agent-tool-call event: {}", e);
                }
            }
            self.replay.finish(
                std::mem::take(&mut self.replay_activity),
                self.num_turns,
                self.is_main().then(|| self.last_cwd.clone()).flatten(),
                self.replay_stop.as_deref(),
            );
            self.replay_done = true;
        }
    }
}

impl KimiTranscriptDecoder {
    fn process_turn_prompt(&mut self, dto: &KimiLineDto) {
        // Turns and lifecycle are session-level — only the main wire drives
        // them; a sub-agent's prompts must not add turns or move the phase.
        if !self.is_main() {
            return;
        }
        // Only `origin.kind == "user"` is a real user turn; `injection`
        // turns (permission-mode reminders, etc.) are skipped.
        let is_user = dto
            .origin
            .as_ref()
            .and_then(|o| o.kind.as_deref())
            .map(|kind| kind == "user")
            .unwrap_or(false);
        if !is_user {
            return;
        }

        // A fresh user turn starts a fresh reply buffer; injection prompts
        // (mid-turn) fall out above and must not clear accumulated text.
        self.turn_text.clear();
        self.notification_body = None;

        self.num_turns = self.num_turns.saturating_add(1);
        if self.replay_done {
            let event = AgentTurnEvent {
                session_id: self.session_id.clone(),
                num_turns: self.num_turns,
            };
            if let Err(e) = emit_agent_turn(self.events.as_ref(), &event) {
                log::warn!("Failed to emit agent-turn event: {}", e);
            }
        }

        // A new user prompt moves the agent into the running phase.
        self.record_phase(AgentPhase::Running);
    }

    fn process_loop_event(&mut self, dto: &KimiLineDto) {
        let Some(event) = dto.event.as_ref() else {
            return;
        };
        // Wire `time` is epoch-ms; use it so replay reflects historical
        // action times. Fall back to `now` only when the stamp is missing.
        let timestamp = dto.time.map_or_else(now_iso8601, epoch_ms_to_iso8601);

        match event.loop_event_type() {
            KimiLoopEventType::ToolCall => {
                let Some(call_id) = event.tool_call_id.as_deref() else {
                    return;
                };
                // Dedup by toolCallId — a replayed START for an already
                // in-flight id is ignored.
                if self.in_flight.contains_key(call_id) {
                    return;
                }
                self.record_cwd(event);
                let tool = event.name.as_deref().unwrap_or("unknown").to_string();
                let args = summarize_tool_args(event);
                let is_test_file = false;

                self.in_flight.insert(
                    call_id.to_string(),
                    InFlightToolCall {
                        started_at: Instant::now(),
                        started_at_iso: timestamp.clone(),
                        tool: tool.clone(),
                        args: args.clone(),
                        is_test_file,
                    },
                );

                self.emit_tool_call(AgentToolCallEvent {
                    session_id: self.session_id.clone(),
                    tool_use_id: self.tool_use_id(call_id),
                    tool,
                    args,
                    status: ToolCallStatus::Running,
                    timestamp,
                    duration_ms: 0,
                    is_test_file,
                });
            }
            KimiLoopEventType::ToolResult => {
                let Some(call_id) = event.tool_call_id.as_deref() else {
                    return;
                };
                let Some(call) = self.in_flight.remove(call_id) else {
                    return;
                };
                self.emit_tool_call(AgentToolCallEvent {
                    session_id: self.session_id.clone(),
                    tool_use_id: self.tool_use_id(call_id),
                    tool: call.tool,
                    args: call.args,
                    status: ToolCallStatus::Done,
                    timestamp: timestamp.clone(),
                    duration_ms: compute_duration_ms(
                        &call.started_at_iso,
                        &timestamp,
                        call.started_at.elapsed(),
                    ),
                    is_test_file: call.is_test_file,
                });
            }
            KimiLoopEventType::ContentPart => {
                // Kimi appends whole assistant parts (not token deltas); only
                // the main wire's `text` parts are reply prose — `think`
                // blocks and sub-agent output never reach the buffer.
                if !self.is_main() {
                    return;
                }
                let Some(text) = event
                    .part
                    .as_ref()
                    .filter(|part| part.type_tag.as_deref() == Some("text"))
                    .and_then(|part| part.text.as_deref())
                    .filter(|text| !text.is_empty())
                else {
                    return;
                };
                if !self.turn_text.is_empty() {
                    self.turn_text.push('\n');
                }
                self.turn_text.push_str(text);
                if let Some(body) = crate::agent::notification::normalize_notification_body(text) {
                    self.notification_body = Some(body);
                }
                // Tail-clamp so a pathological turn stays bounded — the reply
                // contract puts the sentinel block at the END of the turn.
                clamp_turn_text(&mut self.turn_text);
            }
            KimiLoopEventType::StepEnd => {
                // Only the main wire's `end_turn` settles the pane idle; a
                // sub-agent finishing a step must not, while main runs on.
                if self.is_main() && event.finish_reason.as_deref() == Some("end_turn") {
                    // Completion notifications are emitted here, not from a
                    // fixed-path wire watcher, because the supervisor follows
                    // Kimi's mid-session wire relocation.
                    if self.replay_done {
                        crate::agent::notification::emit_kimi_completion(
                            self.events.as_ref(),
                            &self.session_id,
                            &self.agent_session_id,
                            dto.time,
                            event.uuid.as_deref(),
                            self.notification_body.take(),
                        );
                    } else {
                        self.notification_body = None;
                    }
                    self.flush_turn_outputs();
                    self.record_phase(AgentPhase::Idle);
                }
            }
            KimiLoopEventType::Other => {}
        }
    }

    /// The main wire's turn ended: extract either shared structured protocol
    /// from the accumulated text. The buffer drains per
    /// turn regardless — replayed turns are drained without emitting,
    /// mirroring codex/claude_code's `replay_done` gate.
    fn flush_turn_outputs(&mut self) {
        let reply_text = std::mem::take(&mut self.turn_text);
        if !self.replay_done {
            return;
        }
        let (reply, review) = extract_turn_protocols(&reply_text);
        if let Some(reply) = reply {
            let event = map_agent_reply_outcome(&self.session_id, reply);
            if let Err(e) = emit_agent_reply(self.events.as_ref(), &event) {
                log::warn!("Failed to emit agent-reply event: {}", e);
            }
        }

        if let Some(review) = review {
            let event = map_review_outcome(&self.session_id, review);
            if let Err(e) = emit_agent_review(self.events.as_ref(), &event) {
                log::warn!("Failed to emit agent-review event: {}", e);
            }
        }
    }

    fn record_phase(&mut self, phase: AgentPhase) {
        record_lifecycle(
            phase,
            &self.session_id,
            &self.agent_session_id,
            &self.events,
            &mut self.last_phase,
            &mut self.replay_phase,
            self.replay_done,
        );
    }

    fn emit_tool_call(&mut self, event: AgentToolCallEvent) {
        record_tool_call(
            &self.events,
            event,
            &mut self.replay_activity,
            self.replay_done,
        );
    }

    fn record_cwd(&mut self, event: &super::transcript_dto::KimiLoopEventDto) {
        if !self.is_main() || event.name.as_deref() != Some("Bash") {
            return;
        }
        let Some(observed) = event
            .args
            .get("cwd")
            .and_then(|value| value.as_str())
            .filter(|cwd| !cwd.is_empty())
        else {
            return;
        };
        if self.last_cwd.as_deref() == Some(observed) {
            return;
        }
        if self.replay_done {
            let event = AgentCwdEvent {
                session_id: self.session_id.clone(),
                cwd: observed.to_string(),
            };
            if let Err(e) = emit_agent_cwd(self.events.as_ref(), &event) {
                log::warn!("Failed to emit kimi agent-cwd event: {}", e);
            }
        }
        self.last_cwd = Some(observed.to_string());
    }
}

/// Summarize a `tool.call` for the args string: prefer the resolved
/// `display.path`, then `args.path` / `args.command`, then the raw args
/// JSON. Truncated to `MAX_ARGS_LEN`.
fn summarize_tool_args(event: &super::transcript_dto::KimiLoopEventDto) -> String {
    if let Some(path) = event.display.as_ref().and_then(|d| d.path.as_deref()) {
        if !path.is_empty() {
            return truncate_string(path, MAX_ARGS_LEN);
        }
    }
    if let Some(path) = event.args.get("path").and_then(|v| v.as_str()) {
        return truncate_string(path, MAX_ARGS_LEN);
    }
    if let Some(command) = event.args.get("command").and_then(|v| v.as_str()) {
        return truncate_string(command, MAX_ARGS_LEN);
    }
    if event.args.is_null() {
        return String::new();
    }
    truncate_string(&event.args.to_string(), MAX_ARGS_LEN)
}

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

/// Convert an epoch-millisecond wire stamp to an RFC3339 string — the same
/// shape codex carries in `dto.timestamp` and `compute_duration_ms` parses.
/// Out-of-range stamps fall back to `now`.
fn epoch_ms_to_iso8601(epoch_ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(epoch_ms as i64)
        .map_or_else(now_iso8601, |dt| dt.to_rfc3339())
}

fn now_iso8601() -> String {
    let now = std::time::SystemTime::now();
    let since_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0));

    let total_secs = since_epoch.as_secs();
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
    use crate::agent::adapter::traits::StatusSourceLocator as _;
    use crate::runtime::FakeEventSink;
    use serde_json::{json, Value};
    use std::io::Write as _;
    use std::time::{Duration, Instant, SystemTime};

    fn fixture_path() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/agent/adapter/kimi/fixtures/sample_wire.jsonl")
    }

    #[test]
    fn validate_transcript_path_accepts_file_under_kimi_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kimi_root = tmp.path().join(".kimi-code");
        let transcript_path = kimi_root
            .join("sessions")
            .join("wd_x")
            .join("session_1")
            .join("agents")
            .join("main")
            .join("wire.jsonl");
        std::fs::create_dir_all(transcript_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&transcript_path, "").expect("write transcript");

        let canonical = validate_transcript_path_under_root(
            transcript_path.to_str().expect("utf8 path"),
            &kimi_root,
        )
        .expect("path under root validates");
        assert_eq!(
            canonical,
            std::fs::canonicalize(&transcript_path).expect("canonical path")
        );
    }

    #[test]
    fn validate_transcript_path_rejects_path_outside_kimi_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kimi_root = tmp.path().join(".kimi-code");
        let outside = tmp.path().join("wire.jsonl");
        std::fs::create_dir_all(&kimi_root).expect("mkdir root");
        std::fs::write(&outside, "").expect("write outside");

        let result =
            validate_transcript_path_under_root(outside.to_str().expect("utf8 path"), &kimi_root);
        assert!(matches!(
            result,
            Err(ValidateTranscriptError::OutsideRoot { .. })
        ));
    }

    #[test]
    fn validate_transcript_path_rejects_null_byte() {
        let result =
            validate_transcript_path_with_root("/tmp/wire\0.jsonl", Path::new("/tmp/.kimi-code"));
        assert!(matches!(
            result,
            Err(ValidateTranscriptError::InvalidPath(_))
        ));
    }

    fn tool_call_events(sink: &FakeEventSink) -> Vec<Value> {
        sink.recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-tool-call")
            .map(|(_, payload)| payload)
            .collect()
    }

    // A locator whose usage cache is empty (no fetch), so the supervisor's
    // status merge is a no-op — these tests assert on tool-call / turn events.
    fn test_locator() -> Arc<KimiLocator> {
        Arc::new(KimiLocator::new(
            std::path::PathBuf::from("/tmp/.kimi-code"),
            0,
            std::time::SystemTime::UNIX_EPOCH,
            None,
        ))
    }

    fn session_under(kimi_home: &Path, session_id: &str) -> PathBuf {
        kimi_home
            .join("sessions")
            .join("wd_project_deadbeef0000")
            .join(session_id)
    }

    fn write_main_session(session: &Path, raw: &str) -> PathBuf {
        let main_dir = session.join("agents").join("main");
        std::fs::create_dir_all(&main_dir).expect("main dir");
        let wire = main_dir.join("wire.jsonl");
        std::fs::write(&wire, raw).expect("main wire");
        std::fs::write(
            session.join("state.json"),
            json!({
                "agents": {
                    "main": {
                        "homedir": main_dir.to_string_lossy(),
                        "type": "main",
                    },
                },
            })
            .to_string(),
        )
        .expect("state.json");
        wire
    }

    fn write_session_index(kimi_home: &Path, entries: &[(&str, &Path, &Path)]) {
        let raw = entries
            .iter()
            .map(|(session_id, session_dir, work_dir)| {
                json!({
                    "sessionId": session_id,
                    "sessionDir": session_dir.to_string_lossy(),
                    "workDir": work_dir.to_string_lossy(),
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(kimi_home.join("session_index.jsonl"), format!("{raw}\n"))
            .expect("session index");
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for mtime")
            .set_modified(time)
            .expect("set mtime");
    }

    fn set_session_activity(session: &Path, time: SystemTime) {
        set_mtime(&session.join("state.json"), time);
        set_mtime(
            &session.join("agents").join("main").join("wire.jsonl"),
            time,
        );
    }

    fn wait_for_agent_status_session(
        sink: &FakeEventSink,
        agent_session_id: &str,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let found = sink.recorded().into_iter().any(|(name, payload)| {
                name == "agent-status" && payload["agentSessionId"] == agent_session_id
            });
            if found {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_replay_summary(
        sink: &FakeEventSink,
        num_turns: u64,
        tool_call_total: u64,
        timeout: Duration,
    ) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let summary = sink.recorded().into_iter().find_map(|(name, payload)| {
                (name == "agent-replay-summary"
                    && payload["numTurns"] == num_turns
                    && payload["toolCallTotal"] == tool_call_total)
                    .then_some(payload)
            });
            if summary.is_some() || Instant::now() >= deadline {
                return summary;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn fixture_replays_one_user_turn_and_paired_tool_call() {
        let sink = Arc::new(FakeEventSink::new());

        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript_path = tmp.path().join("wire.jsonl");
        std::fs::copy(fixture_path(), &transcript_path).expect("copy fixture");

        let handle = start_tailing(
            sink.clone(),
            "sid-kimi".to_string(),
            transcript_path,
            None,
            test_locator(),
        )
        .expect("tailing starts");

        assert!(
            sink.wait_for_count("agent-replay-summary", 1, Duration::from_secs(5)),
            "expected one coalesced replay summary",
        );
        handle.stop();

        assert_eq!(sink.count("agent-tool-call"), 0);
        assert_eq!(sink.count("agent-turn"), 0);
        let summaries: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-replay-summary")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0]["sessionId"], "sid-kimi");
        assert_eq!(summaries[0]["numTurns"], 1);
        assert_eq!(summaries[0]["toolCallTotal"], 1);
        assert_eq!(summaries[0]["toolCallByType"]["Read"], 1);
        assert_eq!(
            summaries[0]["recentToolCalls"][0]["toolUseId"],
            "tool_6antsBfZmrEAWM7d0ZbyUfAt",
        );
        assert_eq!(summaries[0]["recentToolCalls"][0]["status"], "done");
    }

    #[test]
    fn supervisor_surfaces_sub_agent_tool_calls() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let session = tmp.path().join("sessions").join("wd_x").join("session_y");
        let main_dir = session.join("agents").join("main");
        let sub_dir = session.join("agents").join("agent-0");
        std::fs::create_dir_all(&main_dir).expect("main dir");
        std::fs::create_dir_all(&sub_dir).expect("sub dir");

        // Main opens a user turn and leaves one tool running; the sub-agent
        // completes another tool. All initial wires belong to one replay
        // generation and must flush one aggregate summary.
        std::fs::write(
            main_dir.join("wire.jsonl"),
            "{\"type\":\"turn.prompt\",\"origin\":{\"kind\":\"user\"}}\n\
             {\"type\":\"context.append_loop_event\",\"time\":1781345364300,\"event\":{\"type\":\"tool.call\",\"toolCallId\":\"main-running\",\"name\":\"Bash\",\"args\":{\"command\":\"sleep 10\"}}}\n",
        )
        .expect("main wire");
        std::fs::write(
            sub_dir.join("wire.jsonl"),
            "{\"type\":\"context.append_loop_event\",\"time\":1781345364384,\"event\":{\"type\":\"tool.call\",\"toolCallId\":\"t1\",\"name\":\"Read\",\"args\":{\"path\":\"a\"}}}\n\
             {\"type\":\"context.append_loop_event\",\"time\":1781345364999,\"event\":{\"type\":\"tool.result\",\"toolCallId\":\"t1\"}}\n",
        )
        .expect("sub wire");
        std::fs::write(
            session.join("state.json"),
            format!(
                "{{\"agents\":{{\"main\":{{\"homedir\":\"{}\",\"type\":\"main\"}},\"agent-0\":{{\"homedir\":\"{}\",\"type\":\"sub\"}}}}}}",
                main_dir.display(),
                sub_dir.display(),
            ),
        )
        .expect("state.json");

        let main_wire = main_dir.join("wire.jsonl");
        let handle = start_tailing(
            sink.clone(),
            "sid".to_string(),
            main_wire,
            Some(workspace.clone()),
            test_locator(),
        )
        .expect("tailing starts");

        let summary = wait_for_replay_summary(&sink, 1, 1, Duration::from_secs(5))
            .expect("one aggregate main + sub-agent replay summary");
        handle.stop();

        assert_eq!(sink.count("agent-replay-summary"), 1);
        assert_eq!(sink.count("agent-turn"), 0);
        assert_eq!(sink.count("agent-cwd"), 1);
        assert_eq!(summary["cwd"], workspace.to_string_lossy().as_ref());
        let calls = summary["recentToolCalls"]
            .as_array()
            .expect("recent tool calls");
        assert_eq!(calls.len(), 1);
        // The sub-agent's tool id is namespaced so it can't collide with main.
        assert_eq!(calls[0]["toolUseId"], "agent-0:t1");
        assert_eq!(calls[0]["tool"], "Read");
        assert_eq!(calls[0]["status"], "done");

        let recorded = sink.recorded();
        let running_index = recorded
            .iter()
            .position(|(name, payload)| {
                name == "agent-tool-call" && payload["toolUseId"] == "main-running"
            })
            .expect("replayed running call surfaces at the boundary");
        let summary_index = recorded
            .iter()
            .position(|(name, _)| name == "agent-replay-summary")
            .expect("aggregate summary");
        assert!(running_index < summary_index);
    }

    #[test]
    fn supervisor_treats_sub_agent_discovered_after_replay_as_live() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let session = tmp.path().join("sessions").join("wd_x").join("session_y");
        let main_dir = session.join("agents").join("main");
        let sub_dir = session.join("agents").join("agent-0");
        std::fs::create_dir_all(&main_dir).expect("main dir");
        std::fs::write(
            main_dir.join("wire.jsonl"),
            "{\"type\":\"turn.prompt\",\"origin\":{\"kind\":\"user\"}}\n",
        )
        .expect("main wire");
        std::fs::write(
            session.join("state.json"),
            format!(
                "{{\"agents\":{{\"main\":{{\"homedir\":\"{}\",\"type\":\"main\"}}}}}}",
                main_dir.display(),
            ),
        )
        .expect("state.json");

        let handle = start_tailing(
            sink.clone(),
            "sid".to_string(),
            main_dir.join("wire.jsonl"),
            None,
            test_locator(),
        )
        .expect("tailing starts");
        wait_for_replay_summary(&sink, 1, 0, Duration::from_secs(5))
            .expect("initial replay summary");

        std::fs::create_dir_all(&sub_dir).expect("sub dir");
        std::fs::write(
            sub_dir.join("wire.jsonl"),
            "{\"type\":\"context.append_loop_event\",\"time\":1781345364384,\"event\":{\"type\":\"tool.call\",\"toolCallId\":\"late\",\"name\":\"Read\",\"args\":{\"path\":\"a\"}}}\n\
             {\"type\":\"context.append_loop_event\",\"time\":1781345364999,\"event\":{\"type\":\"tool.result\",\"toolCallId\":\"late\"}}\n",
        )
        .expect("sub wire");
        std::fs::write(
            session.join("state.json"),
            format!(
                "{{\"agents\":{{\"main\":{{\"homedir\":\"{}\",\"type\":\"main\"}},\"agent-0\":{{\"homedir\":\"{}\",\"type\":\"sub\"}}}}}}",
                main_dir.display(),
                sub_dir.display(),
            ),
        )
        .expect("state.json");

        assert!(
            sink.wait_for_count("agent-tool-call", 2, Duration::from_secs(5)),
            "late sub-agent START + DONE must stay live",
        );
        handle.stop();

        assert_eq!(sink.count("agent-replay-summary"), 1);
        let calls = tool_call_events(&sink);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["toolUseId"], "agent-0:late");
        assert_eq!(calls[0]["status"], "running");
        assert_eq!(calls[1]["toolUseId"], "agent-0:late");
        assert_eq!(calls[1]["status"], "done");
    }

    #[test]
    fn supervisor_switches_when_kimi_creates_new_main_session_after_attach() {
        let sink = Arc::new(FakeEventSink::new());
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let old_session = session_under(kimi_home.path(), "session_old");
        let old_wire = write_main_session(
            &old_session,
            "{\"type\":\"config.update\",\"modelAlias\":\"kimi-code/kimi-for-coding\"}\n",
        );
        let old_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let new_time = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        set_session_activity(&old_session, old_time);
        write_session_index(
            kimi_home.path(),
            &[("session_old", &old_session, work.path())],
        );

        let locator = Arc::new(KimiLocator::new(
            kimi_home.path().to_path_buf(),
            4242,
            SystemTime::UNIX_EPOCH,
            None,
        ));
        let located = locator.locate(work.path(), "pty").expect("initial locate");
        assert_eq!(located.status_path, old_wire);

        let handle = start_tailing(
            sink.clone(),
            "sid".to_string(),
            old_wire.clone(),
            Some(work.path().to_path_buf()),
            locator.clone(),
        )
        .expect("tailing starts");

        let new_session = session_under(kimi_home.path(), "session_new");
        let new_wire = write_main_session(
            &new_session,
            &format!(
                "{{\"type\":\"config.update\",\"modelAlias\":\"kimi-code/kimi-for-coding\"}}\n\
                 {{\"type\":\"turn.prompt\",\"origin\":{{\"kind\":\"user\"}}}}\n\
                 {{\"type\":\"context.append_loop_event\",\"time\":1781345364384,\"event\":{{\"type\":\"tool.call\",\"toolCallId\":\"new-tool\",\"name\":\"Glob\",\"args\":{{\"path\":\"src\"}}}}}}\n\
                 {{\"type\":\"context.append_loop_event\",\"time\":1781345364999,\"event\":{{\"type\":\"tool.result\",\"toolCallId\":\"new-tool\"}}}}\n{}\n{}\n\
                 {{\"type\":\"usage.record\",\"usage\":{{\"inputOther\":10,\"output\":2,\"inputCacheRead\":20,\"inputCacheCreation\":0}}}}\n",
                content_part_line("text", "text", KIMI_SENTINEL_REVIEW),
                END_TURN,
            ),
        );
        set_session_activity(&new_session, new_time);
        write_session_index(
            kimi_home.path(),
            &[
                ("session_old", &old_session, work.path()),
                ("session_new", &new_session, work.path()),
            ],
        );

        let summary = wait_for_replay_summary(&sink, 1, 1, Duration::from_secs(5))
            .expect("new session replay summary must surface after supervisor switch");
        assert!(
            wait_for_agent_status_session(&sink, "session_new", Duration::from_secs(5)),
            "status must carry the new kimi session id after supervisor switch",
        );
        assert_eq!(
            sink.count("agent-notification"),
            0,
            "relocated replay must not notify",
        );

        let mut relocated_wire = std::fs::OpenOptions::new()
            .append(true)
            .open(&new_wire)
            .expect("open relocated wire");
        writeln!(
            relocated_wire,
            "{}",
            r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#,
        )
        .expect("append relocated prompt");
        writeln!(
            relocated_wire,
            "{}",
            content_part_line("text", "text", "Relocated task complete"),
        )
        .expect("append relocated response");
        writeln!(
            relocated_wire,
            "{}",
            content_part_line("text", "text", "   "),
        )
        .expect("append empty trailing response part");
        writeln!(
            relocated_wire,
            "{}",
            r#"{"type":"context.append_loop_event","time":1781345366000,"event":{"type":"step.end","uuid":"relocated-turn","finishReason":"end_turn"}}"#,
        )
        .expect("append relocated completion");
        relocated_wire.flush().expect("flush relocated wire");

        assert!(
            sink.wait_for_count("agent-notification", 1, Duration::from_secs(5)),
            "live completion on the relocated wire must notify",
        );
        handle.stop();

        let recovery_paths = locator.recovery_paths();
        assert!(recovery_paths.contains(&new_wire));
        let reviews = recover_reviews(
            &recovery_paths,
            kimi_home.path(),
            "sid",
            &HashSet::from(["review123".to_string()]),
        )
        .expect("recover review from supervisor-selected session");
        assert_eq!(reviews.len(), 1);

        assert_eq!(sink.count("agent-tool-call"), 0);
        assert_eq!(summary["cwd"], work.path().to_string_lossy().as_ref());
        let calls = summary["recentToolCalls"]
            .as_array()
            .expect("recent tool calls");
        assert_eq!(calls[0]["toolUseId"], "new-tool");
        assert_eq!(calls[0]["tool"], "Glob");
        assert_eq!(calls[0]["status"], "done");
        let notification = sink
            .recorded()
            .into_iter()
            .find(|(name, _)| name == "agent-notification")
            .map(|(_, payload)| payload)
            .expect("relocated completion notification");
        assert_eq!(notification["ptyId"], "sid");
        assert_eq!(notification["agentSessionId"], "session_new");
        assert_eq!(notification["title"], "Kimi finished");
        assert_eq!(notification["body"], "Relocated task complete");
        assert_eq!(notification["occurredAt"], 1_781_345_366_000_u64);
        assert_eq!(notification["dedupeKey"], "turn:relocated-turn");
    }

    /// The supervisor's status refresh merges the locator's fetched plan-usage
    /// while consent is ON (so the bars show real limits instead of the
    /// parser's zero default), and emits zero once consent is OFF.
    #[test]
    fn emit_session_status_merges_cached_usage_under_consent() {
        use crate::agent::types::{RateLimitInfo, RateLimits};

        let _guard = crate::agent::kimi_usage_consent::test_serial_guard();
        let tmp = tempfile::tempdir().expect("tempdir");
        let session = tmp.path().join("sessions").join("wd_x").join("session_y");
        let main_dir = session.join("agents").join("main");
        std::fs::create_dir_all(&main_dir).expect("main dir");
        std::fs::write(
            main_dir.join("wire.jsonl"),
            "{\"type\":\"config.update\",\"modelAlias\":\"kimi-code/kimi-for-coding\"}\n\
             {\"type\":\"usage.record\",\"usage\":{\"inputOther\":10,\"output\":2,\"inputCacheRead\":0,\"inputCacheCreation\":0}}\n",
        )
        .expect("main wire");
        std::fs::write(
            session.join("state.json"),
            format!(
                "{{\"agents\":{{\"main\":{{\"homedir\":\"{}\",\"type\":\"main\"}}}}}}",
                main_dir.display(),
            ),
        )
        .expect("state.json");

        let locator = test_locator();
        locator.set_cached_rate_limits_for_test(RateLimits {
            five_hour: RateLimitInfo {
                used_percentage: 17.0,
                resets_at: 1781424046,
            },
            seven_day: Some(RateLimitInfo {
                used_percentage: 40.0,
                resets_at: 1781690446,
            }),
        });

        // Consent ON: the fetched limits are merged into the emitted status.
        crate::agent::kimi_usage_consent::set_for_test(true);
        let sink = Arc::new(FakeEventSink::new());
        emit_session_status(
            sink.as_ref(),
            "sid",
            &session,
            locator.as_ref(),
            &mut None,
            &mut None,
            &mut None,
            &mut None,
        );
        let status = sink
            .recorded()
            .into_iter()
            .find(|(name, _)| name == "agent-status")
            .map(|(_, payload)| payload)
            .expect("an agent-status event");
        assert_eq!(status["rateLimits"]["fiveHour"]["usedPercentage"], 17.0);
        assert_eq!(status["rateLimits"]["sevenDay"]["usedPercentage"], 40.0);

        // Consent OFF: no merge, so the bars fall back to the zeroed default.
        crate::agent::kimi_usage_consent::set_for_test(false);
        let sink_off = Arc::new(FakeEventSink::new());
        emit_session_status(
            sink_off.as_ref(),
            "sid",
            &session,
            locator.as_ref(),
            &mut None,
            &mut None,
            &mut None,
            &mut None,
        );
        let status_off = sink_off
            .recorded()
            .into_iter()
            .find(|(name, _)| name == "agent-status")
            .map(|(_, payload)| payload)
            .expect("an agent-status event");
        assert_eq!(status_off["rateLimits"]["fiveHour"]["usedPercentage"], 0.0);

        crate::agent::kimi_usage_consent::set_for_test(false);
    }

    /// A landed fetch whose values equal the placeholder (0% / no reset) must
    /// still re-emit because `usage_fetched` flips — otherwise the dedupe would
    /// suppress it and the gate would never learn the fetch succeeded.
    #[test]
    fn emit_re_emits_when_usage_fetched_flips_despite_placeholder_values() {
        use crate::agent::types::{RateLimitInfo, RateLimits};

        let _guard = crate::agent::kimi_usage_consent::test_serial_guard();
        let tmp = tempfile::tempdir().expect("tempdir");
        let session = tmp.path().join("sessions").join("wd_x").join("session_z");
        let main_dir = session.join("agents").join("main");
        std::fs::create_dir_all(&main_dir).expect("main dir");
        std::fs::write(
            main_dir.join("wire.jsonl"),
            "{\"type\":\"config.update\",\"modelAlias\":\"kimi-code/kimi-for-coding\"}\n",
        )
        .expect("main wire");
        std::fs::write(
            session.join("state.json"),
            format!(
                "{{\"agents\":{{\"main\":{{\"homedir\":\"{}\",\"type\":\"main\"}}}}}}",
                main_dir.display(),
            ),
        )
        .expect("state.json");

        // A real fetch that landed at 0% with no reset — identical VALUES to the
        // parser's zeroed default, so only `usage_fetched` distinguishes them.
        let locator = test_locator();
        locator.set_cached_rate_limits_for_test(RateLimits {
            five_hour: RateLimitInfo {
                used_percentage: 0.0,
                resets_at: 0,
            },
            seven_day: None,
        });

        let mut last = None;

        // Consent OFF: no merge → usage_fetched=false; records the signature.
        crate::agent::kimi_usage_consent::set_for_test(false);
        let sink_off = Arc::new(FakeEventSink::new());
        let mut last_snapshot = None;
        let mut last_turn_count = None;
        let mut last_mtime = None;
        emit_session_status(
            sink_off.as_ref(),
            "sid",
            &session,
            locator.as_ref(),
            &mut last,
            &mut last_snapshot,
            &mut last_turn_count,
            &mut last_mtime,
        );

        // Consent ON: same zero values, but usage_fetched flips true — the
        // dedupe must NOT suppress this emit.
        crate::agent::kimi_usage_consent::set_for_test(true);
        let sink_on = Arc::new(FakeEventSink::new());
        emit_session_status(
            sink_on.as_ref(),
            "sid",
            &session,
            locator.as_ref(),
            &mut last,
            &mut last_snapshot,
            &mut last_turn_count,
            &mut last_mtime,
        );
        let status = sink_on
            .recorded()
            .into_iter()
            .find(|(name, _)| name == "agent-status")
            .map(|(_, payload)| payload)
            .expect("the usage_fetched flip must re-emit despite equal values");
        assert_eq!(status["usageFetched"], true);

        crate::agent::kimi_usage_consent::set_for_test(false);
    }

    #[test]
    #[cfg(unix)]
    fn supervisor_does_not_double_tail_main_wire_through_symlink() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let session = tmp.path().join("sessions").join("wd_x").join("session_y");
        let main_dir = session.join("agents").join("main");
        std::fs::create_dir_all(&main_dir).expect("main dir");

        // Symlink the session dir so the caller-supplied main wire path is
        // non-canonical while `state.json` references the canonical layout.
        let link_dir = tmp.path().join("link");
        std::os::unix::fs::symlink(&session, &link_dir).expect("symlink session dir");

        std::fs::write(
            main_dir.join("wire.jsonl"),
            "{\"type\":\"turn.prompt\",\"origin\":{\"kind\":\"user\"}}\n",
        )
        .expect("main wire");
        std::fs::write(
            session.join("state.json"),
            format!(
                "{{\"agents\":{{\"main\":{{\"homedir\":\"{}\",\"type\":\"main\"}}}}}}",
                main_dir.display(),
            ),
        )
        .expect("state.json");

        let main_wire = link_dir.join("agents").join("main").join("wire.jsonl");
        let handle = start_tailing(
            sink.clone(),
            "sid".to_string(),
            main_wire,
            None,
            test_locator(),
        )
        .expect("tailing starts");

        assert!(
            sink.wait_for_count("agent-replay-summary", 1, Duration::from_secs(5)),
            "main's replay summary must surface exactly once",
        );
        // Give any duplicate tailer time to emit a second event.
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(
            sink.count("agent-replay-summary"),
            1,
            "symlinked main wire must not be tailed twice"
        );
        assert_eq!(sink.count("agent-turn"), 0);
        handle.stop();
    }

    #[test]
    fn sub_agent_decoder_emits_tools_but_not_turns_or_lifecycle() {
        let sink = Arc::new(FakeEventSink::new());
        // Non-empty prefix => sub-agent.
        let mut decoder = KimiTranscriptDecoder::new(
            sink.clone(),
            "sid".into(),
            "session_x".into(),
            "agent-0:".into(),
        );
        decoder.decode_line(r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#);
        decoder.decode_line(
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"t1","name":"Read","args":{"path":"a"}}}"#,
        );
        decoder.decode_line(
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","finishReason":"end_turn"}}"#,
        );
        decoder.on_caught_up();
        let names: Vec<String> = sink.recorded().into_iter().map(|(name, _)| name).collect();
        assert!(
            names.iter().any(|n| n == "agent-tool-call"),
            "sub-agent must still surface tool calls",
        );
        assert!(
            !names.iter().any(|n| n == "agent-turn"),
            "sub-agent must not emit session turns",
        );
        assert!(
            !names.iter().any(|n| n == "agent-lifecycle"),
            "sub-agent must not move the pane lifecycle",
        );
    }

    #[test]
    fn tool_call_timestamp_derives_from_wire_time_not_now() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            KimiTranscriptDecoder::new(sink.clone(), "sid".into(), String::new(), String::new());
        decoder.on_caught_up();
        // Mirrors the fixture's tool.call envelope `time` (epoch-ms).
        decoder.decode_line(
            r#"{"type":"context.append_loop_event","time":1781345364384,"event":{"type":"tool.call","toolCallId":"t1","name":"Read","args":{"path":"a"}}}"#,
        );
        let calls = tool_call_events(&sink);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["timestamp"], epoch_ms_to_iso8601(1781345364384));
        // The wire time is in 2026; today's `now_iso8601` would differ, so a
        // direct equality to the derived stamp proves it is not current-time.
        assert_ne!(calls[0]["timestamp"], now_iso8601());
    }

    #[test]
    fn bash_tool_cwd_drives_replay_summary_and_live_transitions() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder = KimiTranscriptDecoder::new_with_cwd(
            sink.clone(),
            "sid".into(),
            String::new(),
            String::new(),
            Some("/workspace/main".into()),
        );

        decoder.decode_line(
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"historic","name":"Bash","args":{"command":"npm install","cwd":"/workspace/.worktrees/feature"}}}"#,
        );
        decoder.on_caught_up();

        let summary = sink
            .recorded()
            .into_iter()
            .find(|(name, _)| name == "agent-replay-summary")
            .map(|(_, payload)| payload)
            .expect("replay summary");
        assert_eq!(summary["cwd"], "/workspace/.worktrees/feature");
        assert_eq!(sink.count("agent-cwd"), 0, "replay cwd is summarized");

        decoder.decode_line(
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"not-bash","name":"Read","args":{"path":"a","cwd":"/workspace/decoy"}}}"#,
        );
        decoder.decode_line(
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"live","name":"Bash","args":{"command":"git status","cwd":"/workspace/main"}}}"#,
        );

        let cwd_events: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-cwd")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(cwd_events.len(), 1);
        assert_eq!(cwd_events[0]["cwd"], "/workspace/main");
    }

    #[test]
    fn injection_turn_prompt_does_not_count() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            KimiTranscriptDecoder::new(sink.clone(), "sid".into(), String::new(), String::new());
        decoder.decode_line(
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"x"}],"origin":{"kind":"injection","variant":"permission_mode"}}"#,
        );
        assert_eq!(sink.count("agent-turn"), 0);
    }

    #[test]
    fn tool_call_dedups_by_id() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            KimiTranscriptDecoder::new(sink.clone(), "sid".into(), String::new(), String::new());
        decoder.on_caught_up();
        let start = r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"t1","name":"Read","args":{"path":"a"},"display":{"path":"/tmp/a"}}}"#;
        decoder.decode_line(start);
        decoder.decode_line(start);
        assert_eq!(
            sink.count("agent-tool-call"),
            1,
            "duplicate START suppressed"
        );
    }

    // --- agent-reply capture (VIM-293) ---

    fn agent_reply_events(sink: &FakeEventSink) -> Vec<Value> {
        sink.recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-reply")
            .map(|(_, payload)| payload)
            .collect()
    }

    fn agent_review_events(sink: &FakeEventSink) -> Vec<Value> {
        sink.recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-review")
            .map(|(_, payload)| payload)
            .collect()
    }

    fn content_part_line(part_type: &str, key: &str, body: &str) -> String {
        json!({
            "type": "context.append_loop_event",
            "event": {
                "type": "content.part",
                "turnId": "0",
                "part": { "type": part_type, key: body }
            }
        })
        .to_string()
    }

    const KIMI_SENTINEL_REPLY: &str = "<<<VIMEFLOW_REPLY\n{\"v\":1,\"nonce\":\"abc123\",\"replies\":[{\"id\":1,\"status\":\"reply\",\"text\":\"done\"}]}\nVIMEFLOW_REPLY>>>";
    const KIMI_SENTINEL_REVIEW: &str = "<<<VIMEFLOW_REVIEW\n{\"v\":1,\"nonce\":\"review123\",\"reviewer\":\"Kimi\",\"findings\":[]}\nVIMEFLOW_REVIEW>>>";
    const END_TURN: &str = r#"{"type":"context.append_loop_event","event":{"type":"step.end","finishReason":"end_turn"}}"#;

    #[test]
    fn text_parts_flush_one_reply_at_main_end_turn_and_reset_per_turn() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            KimiTranscriptDecoder::new(sink.clone(), "sid".into(), String::new(), String::new());
        decoder.on_caught_up();

        // Turn 1: the think part carries a DECOY sentinel block — if think
        // text leaked into the buffer, extraction would find the decoy first
        // and the nonce assertion below would fail. The real block sits in
        // the second text part, proving the join spans parts.
        let decoy = KIMI_SENTINEL_REPLY.replace("abc123", "decoy0");
        decoder.decode_line(r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#);
        decoder.decode_line(&content_part_line("think", "think", &decoy));
        decoder.decode_line(&content_part_line("text", "text", "answer prose"));
        decoder.decode_line(&content_part_line("text", "text", KIMI_SENTINEL_REPLY));
        decoder.decode_line(END_TURN);

        let replies = agent_reply_events(&sink);
        assert_eq!(replies.len(), 1, "one reply per completed turn");
        assert_eq!(replies[0]["nonce"], "abc123");
        // rawText is the extracted block (codex/claude semantics), not the
        // surrounding prose.
        let raw = replies[0]["rawText"].as_str().expect("rawText string");
        assert!(raw.contains("\"nonce\":\"abc123\""), "raw block: {raw}");

        // Turn 2: no sentinel — the buffer must have reset, nothing emits.
        decoder.decode_line(r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#);
        decoder.decode_line(&content_part_line("text", "text", "plain answer"));
        decoder.decode_line(END_TURN);
        assert_eq!(
            agent_reply_events(&sink).len(),
            1,
            "no sentinel, no second emit — turn-1 text must not leak"
        );
    }

    #[test]
    fn text_parts_flush_one_review_at_main_end_turn() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            KimiTranscriptDecoder::new(sink.clone(), "sid".into(), String::new(), String::new());
        decoder.on_caught_up();

        decoder.decode_line(r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#);
        decoder.decode_line(&content_part_line("text", "text", KIMI_SENTINEL_REVIEW));
        decoder.decode_line(END_TURN);

        let reviews = agent_review_events(&sink);
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0]["nonce"], "review123");
        assert_eq!(reviews[0]["reviewer"], "Kimi");
        assert_eq!(reviews[0]["findings"], json!([]));
    }

    #[test]
    fn turn_with_both_protocols_emits_neither_event() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            KimiTranscriptDecoder::new(sink.clone(), "sid".into(), String::new(), String::new());
        decoder.on_caught_up();

        decoder.decode_line(r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#);
        decoder.decode_line(&content_part_line(
            "text",
            "text",
            &format!("{KIMI_SENTINEL_REPLY}\n{KIMI_SENTINEL_REVIEW}"),
        ));
        decoder.decode_line(END_TURN);

        assert!(agent_reply_events(&sink).is_empty());
        assert!(agent_review_events(&sink).is_empty());
    }

    #[test]
    fn review_text_can_mention_the_reply_marker() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            KimiTranscriptDecoder::new(sink.clone(), "sid".into(), String::new(), String::new());
        decoder.on_caught_up();
        let review = KIMI_SENTINEL_REVIEW.replace(
            "\"findings\":[]",
            "\"findings\":[{\"path\":\"src/reply.rs\",\"scope\":\"file\",\"category\":\"bug\",\"text\":\"The literal <<<VIMEFLOW_REPLY appears in prose\"}]",
        );

        decoder.decode_line(r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#);
        decoder.decode_line(&content_part_line("text", "text", &review));
        decoder.decode_line(END_TURN);

        assert!(agent_reply_events(&sink).is_empty());
        assert_eq!(agent_review_events(&sink).len(), 1);
    }

    #[test]
    fn turn_text_clamp_does_not_promote_an_inline_marker() {
        let padding = "y".repeat(
            MAX_TURN_TEXT_BYTES
                .saturating_sub(KIMI_SENTINEL_REPLY.len())
                .saturating_sub(1),
        );
        let mut text = format!("inline{KIMI_SENTINEL_REPLY}\n{padding}");

        clamp_turn_text(&mut text);

        assert!(text.len() <= MAX_TURN_TEXT_BYTES);
        assert!(extract_agent_reply(&text).is_none());
    }

    #[test]
    fn recovery_reconstructs_requested_reply_and_review_turns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wire = tmp.path().join("wire.jsonl");
        let raw = [
            r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#.to_string(),
            content_part_line("text", "text", KIMI_SENTINEL_REPLY),
            END_TURN.to_string(),
            r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#.to_string(),
            content_part_line("text", "text", KIMI_SENTINEL_REVIEW),
            END_TURN.to_string(),
        ]
        .join("\n");
        std::fs::write(&wire, format!("{raw}\n")).expect("write wire");
        let paths = [wire.clone()];

        let reply_nonces = HashSet::from(["abc123".to_string()]);
        let replies =
            recover_replies(&paths, tmp.path(), "sid", &reply_nonces).expect("recover replies");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].nonce.as_deref(), Some("abc123"));

        let review_nonces = HashSet::from(["review123".to_string()]);
        let reviews =
            recover_reviews(&paths, tmp.path(), "sid", &review_nonces).expect("recover reviews");
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].nonce.as_deref(), Some("review123"));
    }

    #[test]
    fn recovery_keeps_bounded_same_nonce_reply_turns_in_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wire = tmp.path().join("wire.jsonl");
        let first = KIMI_SENTINEL_REPLY.replace("done", "first");
        let second = KIMI_SENTINEL_REPLY.replace("done", "second");
        let raw = [
            r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#.to_string(),
            content_part_line("text", "text", &first),
            END_TURN.to_string(),
            r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#.to_string(),
            content_part_line("text", "text", &second),
            END_TURN.to_string(),
        ]
        .join("\n");
        std::fs::write(&wire, format!("{raw}\n")).expect("write wire");

        let replies = recover_replies(
            &[wire],
            tmp.path(),
            "sid",
            &HashSet::from(["abc123".to_string()]),
        )
        .expect("recover replies");

        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].replies.as_ref().unwrap()[0].text, "first");
        assert_eq!(replies[1].replies.as_ref().unwrap()[0].text, "second");
    }

    #[test]
    fn recovery_orders_turns_across_revisited_transcripts_by_completion_time() {
        fn timed_end(time: u64) -> String {
            format!(
                r#"{{"type":"context.append_loop_event","time":{time},"event":{{"type":"step.end","finishReason":"end_turn"}}}}"#
            )
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let wire_a = tmp.path().join("a.jsonl");
        let wire_b = tmp.path().join("b.jsonl");
        let turn = |text: &str, time| {
            [
                r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#.to_string(),
                content_part_line("text", "text", &KIMI_SENTINEL_REPLY.replace("done", text)),
                timed_end(time),
            ]
            .join("\n")
        };
        std::fs::write(
            &wire_a,
            format!("{}\n{}\n", turn("first-a", 100), turn("last-a", 300)),
        )
        .expect("write wire a");
        std::fs::write(&wire_b, format!("{}\n", turn("middle-b", 200))).expect("write wire b");

        let replies = recover_replies(
            &[wire_b, wire_a],
            tmp.path(),
            "sid",
            &HashSet::from(["abc123".to_string()]),
        )
        .expect("recover replies");
        let texts: Vec<_> = replies
            .iter()
            .map(|reply| reply.replies.as_ref().unwrap()[0].text.as_str())
            .collect();

        assert_eq!(texts, ["first-a", "middle-b", "last-a"]);
    }

    #[test]
    fn recovery_reads_only_the_bounded_tail_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wire = tmp.path().join("wire.jsonl");
        let old = KIMI_SENTINEL_REPLY.replace("abc123", "old123");
        let recent = KIMI_SENTINEL_REPLY.replace("abc123", "new123");
        let raw = format!(
            "{{\"type\":\"turn.prompt\",\"origin\":{{\"kind\":\"user\"}}}}\n{}\n{}\n{}\n\
             {{\"type\":\"turn.prompt\",\"origin\":{{\"kind\":\"user\"}}}}\n{}\n{}\n",
            content_part_line("text", "text", &old),
            END_TURN,
            "x".repeat(MAX_RECOVERY_BYTES_PER_PATH as usize),
            content_part_line("text", "text", &recent),
            END_TURN,
        );
        std::fs::write(&wire, raw).expect("write wire");

        let replies = recover_replies(
            &[wire],
            tmp.path(),
            "sid",
            &HashSet::from(["old123".to_string(), "new123".to_string()]),
        )
        .expect("recover replies");

        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].nonce.as_deref(), Some("new123"));
    }

    #[test]
    fn recovery_keeps_a_turn_whose_prompt_precedes_the_tail_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wire = tmp.path().join("wire.jsonl");
        let raw = [
            r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#.to_string(),
            content_part_line(
                "text",
                "text",
                &"x".repeat(MAX_RECOVERY_BYTES_PER_PATH as usize),
            ),
            content_part_line("text", "text", KIMI_SENTINEL_REPLY),
            END_TURN.to_string(),
        ]
        .join("\n");
        std::fs::write(&wire, format!("{raw}\n")).expect("write wire");

        let replies = recover_replies(
            &[wire],
            tmp.path(),
            "sid",
            &HashSet::from(["abc123".to_string()]),
        )
        .expect("recover replies");

        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].nonce.as_deref(), Some("abc123"));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_a_transcript_replaced_by_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        let wire = root.path().join("wire.jsonl");
        let outside_wire = outside.path().join("wire.jsonl");
        std::fs::write(&wire, "").expect("write initial wire");
        std::fs::write(
            &outside_wire,
            format!(
                "{{\"type\":\"turn.prompt\",\"origin\":{{\"kind\":\"user\"}}}}\n{}\n{}\n",
                content_part_line("text", "text", KIMI_SENTINEL_REPLY),
                END_TURN,
            ),
        )
        .expect("write outside wire");
        std::fs::remove_file(&wire).expect("remove initial wire");
        symlink(&outside_wire, &wire).expect("replace with symlink");

        let replies = recover_replies(
            &[wire],
            root.path(),
            "sid",
            &HashSet::from(["abc123".to_string()]),
        )
        .expect("unsafe path is skipped");

        assert!(replies.is_empty());
    }

    #[test]
    fn sub_agent_text_parts_never_emit_replies() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder = KimiTranscriptDecoder::new(
            sink.clone(),
            "sid".into(),
            "session_x".into(),
            "agent-0:".into(),
        );
        decoder.on_caught_up();
        decoder.decode_line(&content_part_line("text", "text", KIMI_SENTINEL_REPLY));
        decoder.decode_line(END_TURN);
        assert_eq!(sink.count("agent-reply"), 0);
    }

    #[test]
    fn mid_turn_tool_use_step_end_neither_flushes_nor_clears() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            KimiTranscriptDecoder::new(sink.clone(), "sid".into(), String::new(), String::new());
        decoder.on_caught_up();

        decoder.decode_line(r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#);
        decoder.decode_line(&content_part_line("text", "text", "answer prose"));
        // A tool step ends mid-turn — must neither flush nor drop the buffer.
        decoder.decode_line(
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","finishReason":"tool_use"}}"#,
        );
        assert_eq!(
            sink.count("agent-reply"),
            0,
            "tool_use step.end must not flush"
        );
        decoder.decode_line(&content_part_line("text", "text", KIMI_SENTINEL_REPLY));
        decoder.decode_line(END_TURN);

        let replies = agent_reply_events(&sink);
        assert_eq!(replies.len(), 1, "exactly one emit, at end_turn");
        assert_eq!(replies[0]["nonce"], "abc123");
    }

    #[test]
    fn aborted_turn_buffer_never_leaks_into_the_next_turn() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            KimiTranscriptDecoder::new(sink.clone(), "sid".into(), String::new(), String::new());
        decoder.on_caught_up();

        // Turn 1 carries a sentinel but is aborted — no end_turn arrives.
        decoder.decode_line(r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#);
        decoder.decode_line(&content_part_line("text", "text", KIMI_SENTINEL_REPLY));
        // Turn 2: the fresh user prompt must clear the stale buffer.
        decoder.decode_line(r#"{"type":"turn.prompt","origin":{"kind":"user"}}"#);
        decoder.decode_line(&content_part_line("text", "text", "plain answer"));
        decoder.decode_line(END_TURN);

        assert_eq!(
            sink.count("agent-reply"),
            0,
            "turn-1's sentinel must not emit from turn-2's end_turn"
        );
    }

    #[test]
    fn replayed_turns_drain_the_buffer_without_emitting() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder =
            KimiTranscriptDecoder::new(sink.clone(), "sid".into(), String::new(), String::new());

        // Historic turn replayed before caught-up: no emit, buffer drained.
        decoder.decode_line(&content_part_line("text", "text", KIMI_SENTINEL_REPLY));
        decoder.decode_line(END_TURN);
        decoder.on_caught_up();
        assert_eq!(
            sink.count("agent-reply"),
            0,
            "replayed reply is not re-emitted"
        );

        // Live turn after caught-up emits normally.
        decoder.decode_line(&content_part_line("text", "text", KIMI_SENTINEL_REPLY));
        decoder.decode_line(END_TURN);
        assert_eq!(sink.count("agent-reply"), 1);
    }
}
