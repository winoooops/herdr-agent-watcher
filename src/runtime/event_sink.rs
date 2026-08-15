use serde::Serialize;
use serde_json::Value;

/// Runtime-neutral event emission.
pub trait EventSink: Send + Sync + 'static {
    fn emit_json(&self, event: &str, payload: Value) -> Result<(), String>;
}

#[inline]
pub(crate) fn serialize_event<T: Serialize>(value: &T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|err| format!("event serialize: {err}"))
}

#[cfg(any(test, feature = "e2e-test"))]
#[path = "test_event_sink.rs"]
mod test_event_sink;

#[cfg(any(test, feature = "e2e-test"))]
pub type FakeEventSink = test_event_sink::RecordingEventSink;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fake_event_sink_records_emit_json() {
        let sink = std::sync::Arc::new(FakeEventSink::new());
        sink.emit_json("pty-data", json!({"session_id": "s1", "data": "hello"}))
            .expect("emit");

        let recorded = sink.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "pty-data");
        assert_eq!(recorded[0].1["session_id"], "s1");
    }

    #[test]
    fn fake_event_sink_count_filters_by_name() {
        let sink = std::sync::Arc::new(FakeEventSink::new());
        sink.emit_json("pty-data", json!({})).expect("emit");
        sink.emit_json("pty-exit", json!({})).expect("emit");
        sink.emit_json("pty-data", json!({})).expect("emit");

        assert_eq!(sink.count("pty-data"), 2);
        assert_eq!(sink.count("pty-exit"), 1);
        assert_eq!(sink.count("nope"), 0);
    }

    #[test]
    fn fake_event_sink_concurrent_emits_record_all_events() {
        use std::thread;

        let sink = std::sync::Arc::new(FakeEventSink::new());
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let sink = sink.clone();
                thread::spawn(move || {
                    sink.emit_json(&format!("evt-{i}"), json!({}))
                        .expect("emit");
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("join");
        }

        assert_eq!(sink.recorded().len(), 10);
    }
}
