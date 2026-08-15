use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::EventSink;

pub struct RecordingEventSink {
    recorded: Mutex<Vec<(String, Value)>>,
    changed: Condvar,
}

impl RecordingEventSink {
    pub fn new() -> Self {
        Self {
            recorded: Mutex::new(Vec::new()),
            changed: Condvar::new(),
        }
    }

    pub fn recorded(&self) -> Vec<(String, Value)> {
        self.recorded
            .lock()
            .expect("RecordingEventSink poisoned")
            .clone()
    }

    #[allow(dead_code)]
    pub fn count(&self, event: &str) -> usize {
        self.recorded
            .lock()
            .expect("RecordingEventSink poisoned")
            .iter()
            .filter(|(name, _)| name == event)
            .count()
    }

    #[allow(dead_code)]
    pub fn wait_for_count(&self, event: &str, count: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut recorded = self.recorded.lock().expect("RecordingEventSink poisoned");

        loop {
            if recorded.iter().filter(|(name, _)| name == event).count() >= count {
                return true;
            }

            let now = Instant::now();
            if now >= deadline {
                return false;
            }

            let wait_for = deadline.saturating_duration_since(now);
            let (next, wait_result) = self
                .changed
                .wait_timeout(recorded, wait_for)
                .expect("RecordingEventSink poisoned");
            recorded = next;

            if wait_result.timed_out() {
                return recorded.iter().filter(|(name, _)| name == event).count() >= count;
            }
        }
    }
}

impl Default for RecordingEventSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for RecordingEventSink {
    fn emit_json(&self, event: &str, payload: Value) -> Result<(), String> {
        self.recorded
            .lock()
            .map_err(|err| format!("RecordingEventSink poisoned: {err}"))?
            .push((event.to_string(), payload));
        self.changed.notify_all();
        Ok(())
    }
}
