//! Agent event emission helpers.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::agent::types::{
    AgentAttentionEvent, AgentCwdEvent, AgentLifecycleEvent, AgentPhase, AgentReplaySummaryEvent,
    AgentReplyEvent, AgentReviewEvent, AgentSessionTitleEvent, AgentStatusEvent,
    AgentToolCallEvent, AgentTurnEvent, ToolCallStatus,
};
use crate::runtime::{serialize_event, EventSink};

/// Cap on the sliding window of completed tool calls carried by a replay
/// summary — mirrors the frontend `RECENT_TOOL_CALLS_LIMIT`.
const RECENT_TOOL_CALLS_LIMIT: usize = 50;

pub const AGENT_SESSION_TITLE: &str = "agent-session-title";

pub(crate) fn emit_agent_status(
    events: &dyn EventSink,
    payload: &AgentStatusEvent,
) -> Result<(), String> {
    events.emit_json("agent-status", serialize_event(payload)?)
}

pub(crate) fn emit_agent_tool_call(
    events: &dyn EventSink,
    payload: &AgentToolCallEvent,
) -> Result<(), String> {
    events.emit_json("agent-tool-call", serialize_event(payload)?)
}

pub(crate) fn emit_agent_turn(
    events: &dyn EventSink,
    payload: &AgentTurnEvent,
) -> Result<(), String> {
    events.emit_json("agent-turn", serialize_event(payload)?)
}

pub(crate) fn emit_agent_reply(
    events: &dyn EventSink,
    payload: &AgentReplyEvent,
) -> Result<(), String> {
    events.emit_json("agent-reply", serialize_event(payload)?)
}

pub(crate) fn emit_agent_review(
    events: &dyn EventSink,
    payload: &AgentReviewEvent,
) -> Result<(), String> {
    events.emit_json("agent-review", serialize_event(payload)?)
}

pub(crate) fn emit_agent_cwd(
    events: &dyn EventSink,
    payload: &AgentCwdEvent,
) -> Result<(), String> {
    events.emit_json("agent-cwd", serialize_event(payload)?)
}

pub(crate) fn emit_agent_session_title(
    events: &dyn EventSink,
    payload: &AgentSessionTitleEvent,
) -> Result<(), String> {
    events.emit_json(AGENT_SESSION_TITLE, serialize_event(payload)?)
}

#[allow(dead_code)] // wired by the per-adapter lifecycle emit
pub(crate) fn emit_agent_lifecycle(
    events: &dyn EventSink,
    payload: &AgentLifecycleEvent,
) -> Result<(), String> {
    events.emit_json("agent-lifecycle", serialize_event(payload)?)
}

pub(crate) fn emit_agent_attention(
    events: &dyn EventSink,
    payload: &AgentAttentionEvent,
) -> Result<(), String> {
    events.emit_json("agent-attention", serialize_event(payload)?)
}

/// Semantic attention is live-only. Replayed prompts and failures must not
/// notify again when a transcript watcher attaches or reconnects.
pub(crate) fn record_attention(
    events: &Arc<dyn EventSink>,
    payload: AgentAttentionEvent,
    replay_done: bool,
) {
    if replay_done {
        if let Err(error) = emit_agent_attention(events.as_ref(), &payload) {
            log::warn!("Failed to emit agent-attention event: {}", error);
        }
    }
}

#[allow(dead_code)] // wired by the per-adapter lifecycle emit
pub(crate) fn emit_lifecycle_on_change(
    events: &dyn EventSink,
    session_id: &str,
    agent_session_id: &str,
    last: &mut Option<AgentPhase>,
    phase: AgentPhase,
) {
    if *last == Some(phase) {
        return;
    }
    *last = Some(phase);
    let payload = AgentLifecycleEvent {
        session_id: session_id.to_string(),
        agent_session_id: agent_session_id.to_string(),
        phase,
    };
    if let Err(e) = emit_agent_lifecycle(events, &payload) {
        log::warn!("Failed to emit agent-lifecycle event: {}", e);
    }
}

pub(crate) fn emit_agent_replay_summary(
    events: &dyn EventSink,
    payload: &AgentReplaySummaryEvent,
) -> Result<(), String> {
    events.emit_json("agent-replay-summary", serialize_event(payload)?)
}

/// Accumulator for tool-call activity observed during transcript replay.
///
/// During replay the per-line `agent-tool-call` events are suppressed (they
/// would flood the IPC queue on resume) and their effect is folded into this
/// struct, then flushed once at the replay→live boundary as a single
/// [`AgentReplaySummaryEvent`]. Mirrors the frontend's `applyToolCallEvents`
/// fold so the summary reconstructs the same end state.
#[derive(Default)]
pub(crate) struct ReplayActivity {
    total: u32,
    by_type: HashMap<String, u32>,
    running: HashMap<String, AgentToolCallEvent>,
    /// Completed tool calls in arrival (oldest-first) order, capped at the
    /// limit; `into_summary` reverses it to newest-first.
    recent: VecDeque<AgentToolCallEvent>,
}

impl ReplayActivity {
    /// Fold another decoder's replay into this one. Kimi tails its main and
    /// sub-agent wires concurrently, so the supervisor joins their local
    /// accumulators before emitting one session-level summary.
    pub(crate) fn merge(&mut self, other: Self) {
        self.total = self.total.saturating_add(other.total);
        for (tool, count) in other.by_type {
            let total = self.by_type.entry(tool).or_default();
            *total = total.saturating_add(count);
        }
        self.running.extend(other.running);
        self.recent.extend(other.recent);

        let mut recent: Vec<_> = self.recent.drain(..).collect();
        recent.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        if recent.len() > RECENT_TOOL_CALLS_LIMIT {
            recent.drain(..recent.len() - RECENT_TOOL_CALLS_LIMIT);
        }
        self.recent = recent.into();
    }

    /// Fold one completed (done/failed) tool call into the running totals.
    fn record_completed(&mut self, event: AgentToolCallEvent) {
        self.running.remove(&event.tool_use_id);
        self.total = self.total.saturating_add(1);
        *self.by_type.entry(event.tool.clone()).or_default() += 1;
        self.recent.push_back(event);
        if self.recent.len() > RECENT_TOOL_CALLS_LIMIT {
            self.recent.pop_front();
        }
    }

    fn record_running(&mut self, event: AgentToolCallEvent) {
        self.running.insert(event.tool_use_id.clone(), event);
    }

    pub(crate) fn take_running(&mut self) -> Vec<AgentToolCallEvent> {
        self.running.drain().map(|(_, event)| event).collect()
    }

    /// Build the one-shot summary event, reversing `recent` to newest-first.
    pub(crate) fn into_summary(
        self,
        session_id: String,
        num_turns: u32,
        cwd: Option<String>,
    ) -> AgentReplaySummaryEvent {
        AgentReplaySummaryEvent {
            session_id,
            num_turns,
            cwd,
            tool_call_total: self.total,
            tool_call_by_type: self.by_type,
            recent_tool_calls: self.recent.into_iter().rev().collect(),
        }
    }
}

/// Route a tool-call event: emit live once replay is done, else fold settled
/// calls into the replay summary and keep replayed in-flight calls for the
/// replay->live boundary.
pub(crate) fn record_tool_call(
    events: &Arc<dyn EventSink>,
    event: AgentToolCallEvent,
    replay: &mut ReplayActivity,
    replay_done: bool,
) {
    if replay_done {
        if let Err(e) = emit_agent_tool_call(events.as_ref(), &event) {
            log::warn!("Failed to emit agent-tool-call event: {}", e);
        }
    } else {
        match event.status {
            ToolCallStatus::Running => replay.record_running(event),
            ToolCallStatus::Done | ToolCallStatus::Failed => replay.record_completed(event),
        }
    }
}

/// Route a derived phase: emit live (edge-triggered) once replay is done,
/// else accumulate the settled phase for the one-shot boundary flush.
pub(crate) fn record_lifecycle(
    phase: AgentPhase,
    session_id: &str,
    agent_session_id: &str,
    events: &Arc<dyn EventSink>,
    last_phase: &mut Option<AgentPhase>,
    replay_phase: &mut Option<AgentPhase>,
    replay_done: bool,
) {
    if replay_done {
        emit_lifecycle_on_change(
            events.as_ref(),
            session_id,
            agent_session_id,
            last_phase,
            phase,
        );
    } else {
        *replay_phase = Some(phase);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::agent::types::{
        AgentAttentionEvent, AgentAttentionReason, AgentPhase, AgentToolCallEvent, ToolCallStatus,
    };
    use crate::runtime::FakeEventSink;

    use super::{emit_lifecycle_on_change, record_attention, record_tool_call, ReplayActivity};

    fn tool_call(id: &str, tool: &str, status: ToolCallStatus) -> AgentToolCallEvent {
        AgentToolCallEvent {
            session_id: "sid".into(),
            tool_use_id: id.into(),
            tool: tool.into(),
            args: String::new(),
            status,
            timestamp: "2026-06-24T00:00:00Z".into(),
            duration_ms: 0,
            is_test_file: false,
        }
    }

    #[test]
    fn replay_activity_into_summary_counts_and_orders_newest_first() {
        let mut activity = ReplayActivity::default();
        record_tool_call(
            &(Arc::new(FakeEventSink::new()) as Arc<dyn crate::runtime::EventSink>),
            tool_call("c1", "Read", ToolCallStatus::Done),
            &mut activity,
            false,
        );
        record_tool_call(
            &(Arc::new(FakeEventSink::new()) as Arc<dyn crate::runtime::EventSink>),
            tool_call("c2", "Read", ToolCallStatus::Failed),
            &mut activity,
            false,
        );
        record_tool_call(
            &(Arc::new(FakeEventSink::new()) as Arc<dyn crate::runtime::EventSink>),
            tool_call("c3", "Bash", ToolCallStatus::Running),
            &mut activity,
            false,
        );

        let summary = activity.into_summary("sid".into(), 4, Some("/ws".into()));
        assert_eq!(summary.tool_call_total, 2);
        assert_eq!(summary.tool_call_by_type.get("Read"), Some(&2));
        assert_eq!(summary.tool_call_by_type.get("Bash"), None);
        assert_eq!(summary.num_turns, 4);
        assert_eq!(summary.cwd.as_deref(), Some("/ws"));
        assert_eq!(summary.recent_tool_calls.len(), 2);
        // Newest-first: c2 (the most recent completion) leads.
        assert_eq!(summary.recent_tool_calls[0].tool_use_id, "c2");
        assert_eq!(summary.recent_tool_calls[1].tool_use_id, "c1");
    }

    #[test]
    fn replay_activity_retains_unsettled_running_tool_calls() {
        let mut activity = ReplayActivity::default();
        let sink: Arc<dyn crate::runtime::EventSink> = Arc::new(FakeEventSink::new());

        record_tool_call(
            &sink,
            tool_call("c1", "Bash", ToolCallStatus::Running),
            &mut activity,
            false,
        );
        record_tool_call(
            &sink,
            tool_call("c2", "Read", ToolCallStatus::Running),
            &mut activity,
            false,
        );
        record_tool_call(
            &sink,
            tool_call("c2", "Read", ToolCallStatus::Done),
            &mut activity,
            false,
        );

        let running = activity.take_running();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].tool_use_id, "c1");
    }

    #[test]
    fn replay_activity_merge_sums_decoders_and_orders_by_timestamp() {
        let sink: Arc<dyn crate::runtime::EventSink> = Arc::new(FakeEventSink::new());
        let mut main = ReplayActivity::default();
        let mut sub = ReplayActivity::default();
        let mut newer = tool_call("main", "Bash", ToolCallStatus::Done);
        newer.timestamp = "2026-06-24T00:00:02Z".into();
        let mut older = tool_call("sub", "Read", ToolCallStatus::Failed);
        older.timestamp = "2026-06-24T00:00:01Z".into();
        record_tool_call(&sink, newer, &mut main, false);
        record_tool_call(&sink, older, &mut sub, false);

        main.merge(sub);
        let summary = main.into_summary("sid".into(), 1, None);

        assert_eq!(summary.tool_call_total, 2);
        assert_eq!(summary.tool_call_by_type.get("Bash"), Some(&1));
        assert_eq!(summary.tool_call_by_type.get("Read"), Some(&1));
        assert_eq!(summary.recent_tool_calls[0].tool_use_id, "main");
        assert_eq!(summary.recent_tool_calls[1].tool_use_id, "sub");
    }

    #[test]
    fn replay_activity_recent_window_is_capped_at_50_newest_first() {
        let mut activity = ReplayActivity::default();
        let sink: Arc<dyn crate::runtime::EventSink> = Arc::new(FakeEventSink::new());
        for i in 0..60 {
            record_tool_call(
                &sink,
                tool_call(&format!("c{i}"), "Read", ToolCallStatus::Done),
                &mut activity,
                false,
            );
        }
        let summary = activity.into_summary("sid".into(), 0, None);
        // Total counts every completed call; the recent window keeps only 50.
        assert_eq!(summary.tool_call_total, 60);
        assert_eq!(summary.recent_tool_calls.len(), 50);
        // Newest-first: the last-recorded call (c59) leads; the oldest 10 dropped.
        assert_eq!(summary.recent_tool_calls[0].tool_use_id, "c59");
        assert_eq!(summary.recent_tool_calls[49].tool_use_id, "c10");
    }

    #[test]
    fn record_tool_call_emits_live_after_replay_done() {
        let concrete = Arc::new(FakeEventSink::new());
        let sink: Arc<dyn crate::runtime::EventSink> = concrete.clone();
        let mut activity = ReplayActivity::default();
        // replay_done = true → emit live, do NOT accumulate.
        record_tool_call(
            &sink,
            tool_call("c1", "Read", ToolCallStatus::Done),
            &mut activity,
            true,
        );
        assert_eq!(concrete.count("agent-tool-call"), 1);
        let summary = activity.into_summary("sid".into(), 0, None);
        assert_eq!(summary.tool_call_total, 0);
    }

    #[test]
    fn lifecycle_emits_only_on_change() {
        let sink = Arc::new(FakeEventSink::new());
        let mut last: Option<AgentPhase> = None;
        emit_lifecycle_on_change(&*sink, "sid", "agent-1", &mut last, AgentPhase::Running);
        emit_lifecycle_on_change(&*sink, "sid", "agent-1", &mut last, AgentPhase::Running); // dup
        emit_lifecycle_on_change(&*sink, "sid", "agent-1", &mut last, AgentPhase::Idle);
        assert_eq!(sink.count("agent-lifecycle"), 2); // Running, Idle — the dup is suppressed
        assert_eq!(last, Some(AgentPhase::Idle));
    }

    #[test]
    fn agent_attention_is_live_only() {
        let concrete = Arc::new(FakeEventSink::new());
        let sink: Arc<dyn crate::runtime::EventSink> = concrete.clone();
        let event = AgentAttentionEvent {
            pty_id: "pty-1".into(),
            reason: AgentAttentionReason::ApprovalRequested,
            title: "Claude needs approval".into(),
            body: None,
            occurred_at: 1,
            dedupe_key: Some("permission-1".into()),
        };

        record_attention(&sink, event.clone(), false);
        assert_eq!(concrete.count("agent-attention"), 0);

        record_attention(&sink, event, true);
        assert_eq!(concrete.count("agent-attention"), 1);
        assert_eq!(concrete.recorded()[0].1["reason"], "approval-requested");
    }
}
