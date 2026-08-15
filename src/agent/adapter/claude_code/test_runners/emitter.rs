//! Replay-aware emitter for test-run events. During the initial replay phase
//! of a tail_loop, `submit` keeps only the latest snapshot — when the loop
//! hits EOF for the first time and calls `finish_replay`, the latest pending
//! snapshot (if any) is emitted exactly once. After replay, every submit
//! emits immediately.

use std::sync::Arc;

use super::types::TestRunSnapshot;
use crate::runtime::{serialize_event, EventSink};

pub struct TestRunEmitter {
    events: Arc<dyn EventSink>,
    replay_done: bool,
    pending: Option<TestRunSnapshot>,
}

impl TestRunEmitter {
    pub fn new(events: Arc<dyn EventSink>) -> Self {
        Self {
            events,
            replay_done: false,
            pending: None,
        }
    }

    pub fn submit(&mut self, snapshot: TestRunSnapshot) {
        if self.replay_done {
            if let Err(e) = emit_test_run(self.events.as_ref(), &snapshot) {
                log::warn!("Failed to emit test-run event: {}", e);
            }
        } else {
            self.pending = Some(snapshot);
        }
    }

    pub fn clear_pending(&mut self) {
        self.pending = None;
    }

    pub fn finish_replay(&mut self) {
        if self.replay_done {
            return;
        }
        self.replay_done = true;
        if let Some(s) = self.pending.take() {
            if let Err(e) = emit_test_run(self.events.as_ref(), &s) {
                log::warn!("Failed to emit test-run event: {}", e);
            }
        }
    }
}

fn emit_test_run(events: &dyn EventSink, payload: &TestRunSnapshot) -> Result<(), String> {
    events.emit_json("test-run", serialize_event(payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::adapter::claude_code::test_runners::types::{TestRunStatus, TestRunSummary};
    use crate::runtime::FakeEventSink;

    fn snap(passed: u32) -> TestRunSnapshot {
        TestRunSnapshot {
            session_id: "s".to_string(),
            runner: "vitest".to_string(),
            command_preview: "vitest".to_string(),
            started_at: "2026-04-28T12:00:00Z".to_string(),
            finished_at: "2026-04-28T12:00:01Z".to_string(),
            duration_ms: 1000,
            status: TestRunStatus::Pass,
            summary: TestRunSummary {
                passed,
                failed: 0,
                skipped: 0,
                total: passed,
                groups: vec![],
            },
            output_excerpt: None,
        }
    }

    #[test]
    fn submit_during_replay_buffers_latest() {
        let sink = Arc::new(FakeEventSink::new());
        let mut e = TestRunEmitter::new(sink.clone());
        e.submit(snap(1));
        e.submit(snap(2));
        e.submit(snap(3));
        assert_eq!(sink.count("test-run"), 0);
        e.finish_replay();
        let v = sink.recorded();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].1["summary"]["passed"], 3);
    }

    #[test]
    fn clear_pending_drops_replay_buffer_without_ending_replay() {
        let sink = Arc::new(FakeEventSink::new());
        let mut e = TestRunEmitter::new(sink.clone());
        e.submit(snap(1));
        e.clear_pending();
        e.finish_replay();
        assert_eq!(sink.count("test-run"), 0);
    }

    #[test]
    fn submit_after_replay_emits_immediately() {
        let sink = Arc::new(FakeEventSink::new());
        let mut e = TestRunEmitter::new(sink.clone());
        e.finish_replay();
        e.submit(snap(7));
        assert_eq!(sink.count("test-run"), 1);
    }

    #[test]
    fn finish_replay_is_idempotent() {
        let sink = Arc::new(FakeEventSink::new());
        let mut e = TestRunEmitter::new(sink.clone());
        e.submit(snap(1));
        e.finish_replay();
        e.finish_replay(); // second call must not re-emit
        assert_eq!(sink.count("test-run"), 1);
    }
}
