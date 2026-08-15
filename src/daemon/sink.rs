use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::runtime::EventSink;

pub type Tokens = HashMap<String, Option<String>>;

pub trait HerdrPort: Send + Sync + 'static {
    fn notify(&self, title: &str, body: &str) -> Result<(), String>;
    fn report_metadata(&self, pane_id: &str, tokens: Tokens) -> Result<(), String>;
}

pub struct LiveHerdrPort(pub crate::herdr::client::HerdrClient);

impl HerdrPort for LiveHerdrPort {
    fn notify(&self, title: &str, body: &str) -> Result<(), String> {
        self.0
            .notification_show(title, body)
            .map_err(|error| error.to_string())
    }

    fn report_metadata(&self, pane_id: &str, tokens: Tokens) -> Result<(), String> {
        self.0
            .pane_report_metadata(pane_id, tokens, 15_000)
            .map_err(|error| error.to_string())
    }
}

fn token(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

pub struct HerdrSink {
    port: Arc<dyn HerdrPort>,
    store: Arc<crate::daemon::store::TelemetryStore>,
}

impl HerdrSink {
    pub fn new(port: Arc<dyn HerdrPort>) -> Self {
        Self::with_store(
            port,
            Arc::new(crate::daemon::store::TelemetryStore::default()),
        )
    }

    pub fn with_store(
        port: Arc<dyn HerdrPort>,
        store: Arc<crate::daemon::store::TelemetryStore>,
    ) -> Self {
        Self { port, store }
    }
}

impl EventSink for HerdrSink {
    fn emit_json(&self, event: &str, payload: Value) -> Result<(), String> {
        let mut payload = payload;
        crate::daemon::store::normalise_event(event, &mut payload);

        match event {
            "agent-notification" => {
                if let Some(pane_id) = payload["ptyId"].as_str() {
                    self.store.record(pane_id, event, payload.clone());
                }
                let title = payload["title"].as_str().unwrap_or("Agent Watcher");
                let body = payload["body"].as_str().unwrap_or(event);
                self.port.notify(title, body)
            }
            "agent-attention" => {
                let title = payload["title"].as_str().unwrap_or("Agent Watcher");
                let body = payload["body"].as_str().unwrap_or(event);
                let notify_result = self.port.notify(title, body);
                if let Some(pane_id) = payload["ptyId"].as_str() {
                    self.store.record(pane_id, event, payload.clone());
                    if let Err(error) = self.port.report_metadata(
                        pane_id,
                        Tokens::from([("agent_watcher_attention".into(), Some("true".into()))]),
                    ) {
                        log::warn!("[sink] attention metadata failed: {error}");
                    }
                }
                notify_result
            }
            "agent-status" => {
                let Some(pane_id) = payload["sessionId"].as_str() else {
                    log::debug!("[sink] agent-status without sessionId; skipping");
                    return Ok(());
                };
                self.store.record(pane_id, event, payload.clone());
                self.port.report_metadata(
                    pane_id,
                    Tokens::from([
                        ("agent_watcher_state".into(), Some("active".into())),
                        (
                            "agent_watcher_model".into(),
                            token(&payload["modelDisplayName"]),
                        ),
                        (
                            "agent_watcher_context_pct".into(),
                            token(&payload["contextWindow"]["usedPercentage"]),
                        ),
                        (
                            "agent_watcher_cost_usd".into(),
                            token(&payload["cost"]["totalCostUsd"]),
                        ),
                    ]),
                )
            }
            "agent-lifecycle" => {
                let Some(pane_id) = payload["sessionId"].as_str() else {
                    log::debug!("[sink] agent-lifecycle without sessionId; skipping");
                    return Ok(());
                };
                self.store.record(pane_id, event, payload.clone());
                self.port.report_metadata(
                    pane_id,
                    Tokens::from([("agent_watcher_phase".into(), token(&payload["phase"]))]),
                )
            }
            "agent-session-title" => {
                let Some(pane_id) = payload["sessionId"].as_str() else {
                    log::debug!("[sink] agent-session-title without sessionId; skipping");
                    return Ok(());
                };
                self.store.record(pane_id, event, payload.clone());
                self.port.report_metadata(
                    pane_id,
                    Tokens::from([("agent_watcher_title".into(), token(&payload["title"]))]),
                )
            }
            "agent-cwd" => {
                let Some(pane_id) = payload["sessionId"].as_str() else {
                    log::debug!("[sink] agent-cwd without sessionId; skipping");
                    return Ok(());
                };
                self.store.record(pane_id, event, payload.clone());
                log::debug!("[sink] cwd for {pane_id}");
                Ok(())
            }
            "agent-tool-call" | "agent-replay-summary" => {
                let Some(pane_id) = payload["sessionId"].as_str() else {
                    log::debug!("[sink] {event} without sessionId; skipping");
                    return Ok(());
                };
                self.store.record(pane_id, event, payload.clone());
                log::debug!("[sink] recorded event {event}: {payload}");
                Ok(())
            }
            other => {
                log::debug!("[sink] pass-through event {other}: {payload}");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingPort {
        notifications: Mutex<Vec<(String, String)>>,
        metadata: Mutex<Vec<(String, Tokens)>>,
    }

    impl HerdrPort for RecordingPort {
        fn notify(&self, title: &str, body: &str) -> Result<(), String> {
            self.notifications
                .lock()
                .unwrap()
                .push((title.into(), body.into()));
            Ok(())
        }

        fn report_metadata(&self, pane_id: &str, tokens: Tokens) -> Result<(), String> {
            self.metadata.lock().unwrap().push((pane_id.into(), tokens));
            Ok(())
        }
    }

    #[test]
    fn agent_notification_routes_to_notify() {
        let port = Arc::new(RecordingPort::default());
        let sink = HerdrSink::new(port.clone());
        sink.emit_json(
            "agent-notification",
            json!({ "ptyId": "p1", "title": "Claude", "body": "Done" }),
        )
        .unwrap();
        assert_eq!(port.notifications.lock().unwrap().len(), 1);
    }

    #[test]
    fn attention_sets_token_and_notifies() {
        let port = Arc::new(RecordingPort::default());
        let sink = HerdrSink::new(port.clone());
        sink.emit_json(
            "agent-attention",
            json!({ "ptyId": "p1", "title": "claude", "body": "needs you" }),
        )
        .unwrap();
        assert_eq!(port.notifications.lock().unwrap().len(), 1);
        let metadata = port.metadata.lock().unwrap();
        assert_eq!(metadata[0].0, "p1");
        assert_eq!(
            metadata[0].1.get("agent_watcher_attention"),
            Some(&Some("true".to_string()))
        );
    }

    #[test]
    fn session_title_sets_title_token() {
        let port = Arc::new(RecordingPort::default());
        let sink = HerdrSink::new(port.clone());
        sink.emit_json(
            "agent-session-title",
            json!({ "sessionId": "p1", "title": "Fix CI flake" }),
        )
        .unwrap();
        assert_eq!(
            port.metadata.lock().unwrap()[0]
                .1
                .get("agent_watcher_title"),
            Some(&Some("Fix CI flake".to_string()))
        );
    }

    #[test]
    fn agent_status_routes_to_schema_valid_metadata_tokens() {
        let port = Arc::new(RecordingPort::default());
        let sink = HerdrSink::new(port.clone());
        sink.emit_json(
            "agent-status",
            json!({
                "sessionId": "p1",
                "modelDisplayName": "Sonnet",
                "contextWindow": { "usedPercentage": 41.5 },
                "cost": { "totalCostUsd": 1.25 },
            }),
        )
        .unwrap();
        let metadata = port.metadata.lock().unwrap();
        assert_eq!(metadata[0].0, "p1");
        assert_eq!(
            metadata[0].1.get("agent_watcher_state"),
            Some(&Some("active".to_string()))
        );
        assert!(metadata[0].1.keys().all(|key| key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')));
    }

    #[test]
    fn unmapped_events_are_logged_not_errors() {
        let sink = HerdrSink::new(Arc::new(RecordingPort::default()));
        assert!(sink
            .emit_json("agent-turn", json!({ "sessionId": "p1" }))
            .is_ok());
    }

    #[test]
    fn tool_call_and_replay_summary_reach_the_store() {
        let store = Arc::new(crate::daemon::store::TelemetryStore::default());
        store.set_agent("p1", "claude");
        let sink = HerdrSink::with_store(Arc::new(RecordingPort::default()), store.clone());
        sink.emit_json(
            "agent-tool-call",
            json!({"sessionId":"p1","toolUseId":"t1","tool":"Bash","status":"done"}),
        )
        .unwrap();
        sink.emit_json(
            "agent-replay-summary",
            json!({"sessionId":"p1","toolCallTotal":3,"recentToolCalls":[]}),
        )
        .unwrap();
        assert_eq!(store.snapshot().1["p1"].tool_call_total, 3);
    }

    #[test]
    fn notification_reaches_the_store_and_still_notifies() {
        let port = Arc::new(RecordingPort::default());
        let store = Arc::new(crate::daemon::store::TelemetryStore::default());
        store.set_agent("p1", "claude");
        let sink = HerdrSink::with_store(port.clone(), store.clone());
        sink.emit_json(
            "agent-notification",
            json!({"ptyId":"p1","reason":"turn-complete","title":"Done","body":"Ready"}),
        )
        .unwrap();
        assert_eq!(port.notifications.lock().unwrap().len(), 1);
        assert_eq!(
            store.snapshot().1["p1"].card_state,
            crate::daemon::store::CardState::Finished
        );
    }

    #[test]
    fn agent_cwd_routes_to_the_store_and_outranks_the_pane_list() {
        let store = Arc::new(crate::daemon::store::TelemetryStore::default());
        store.set_agent("p1", "claude");
        store.set_pane_list_cwd("p1", Some("/shell/dir".into()));
        let sink = HerdrSink::with_store(Arc::new(RecordingPort::default()), store.clone());

        sink.emit_json("agent-cwd", json!({"sessionId": "p1", "cwd": "/work/tree"}))
            .unwrap();
        assert_eq!(store.snapshot().1["p1"].cwd.as_deref(), Some("/work/tree"));

        store.set_pane_list_cwd("p1", Some("/shell/dir".into()));
        assert_eq!(store.snapshot().1["p1"].cwd.as_deref(), Some("/work/tree"));
    }

    #[test]
    fn reported_metadata_is_normalised_too_not_just_the_stored_copy() {
        let store = Arc::new(crate::daemon::store::TelemetryStore::default());
        store.set_agent("p1", "claude");
        let port = Arc::new(RecordingPort::default());
        let sink = HerdrSink::with_store(port.clone(), store);

        sink.emit_json(
            "agent-status",
            json!({"sessionId": "p1", "modelDisplayName": "Sonnet\t4.5 "}),
        )
        .unwrap();

        let (pane, tokens) = port.metadata.lock().unwrap()[0].clone();
        assert_eq!(pane, "p1");
        assert_eq!(
            tokens
                .get("agent_watcher_model")
                .cloned()
                .flatten()
                .as_deref(),
            Some("Sonnet 4.5")
        );
    }

    #[test]
    fn agent_cwd_without_a_session_id_is_skipped() {
        let store = Arc::new(crate::daemon::store::TelemetryStore::default());
        store.set_agent("p1", "claude");
        let sink = HerdrSink::with_store(Arc::new(RecordingPort::default()), store.clone());
        sink.emit_json("agent-cwd", json!({"cwd": "/work/tree"}))
            .unwrap();
        assert!(store.snapshot().1["p1"].cwd.is_none());
    }
}
