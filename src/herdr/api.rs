use std::collections::HashMap;

use serde_json::{json, Value};

use super::client::{HerdrClient, HerdrClientError};

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AgentSessionInfo {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_session: Option<AgentSessionInfo>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
}

impl PaneInfo {
    pub fn session_value(&self) -> Option<&str> {
        self.agent_session.as_ref()?.value.as_deref()
    }
}

pub fn find_u64(value: &Value, key: &str) -> Option<u64> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_u64)
            .or_else(|| map.values().find_map(|child| find_u64(child, key))),
        Value::Array(items) => items.iter().find_map(|child| find_u64(child, key)),
        _ => None,
    }
}

pub fn collect_pane_values(value: &Value) -> Vec<&Value> {
    let mut panes = Vec::new();
    match value {
        Value::Object(map) => {
            if map.get("pane_id").is_some_and(Value::is_string) {
                panes.push(value);
            }
            panes.extend(map.values().flat_map(collect_pane_values));
        }
        Value::Array(items) => panes.extend(items.iter().flat_map(collect_pane_values)),
        _ => {}
    }
    panes
}

impl HerdrClient {
    pub const METADATA_SOURCE: &'static str = "plugin:herdr-agent-watcher";

    pub fn pane_list(&self) -> Result<Vec<PaneInfo>, HerdrClientError> {
        let value = self.request("pane.list", json!({}))?;
        Ok(collect_pane_values(&value)
            .into_iter()
            .filter_map(|pane| serde_json::from_value(pane.clone()).ok())
            .collect())
    }

    pub fn pane_shell_pid(&self, pane_id: &str) -> Result<Option<u32>, HerdrClientError> {
        let value = self.request("pane.process_info", json!({ "pane_id": pane_id }))?;
        Ok(find_u64(&value, "shell_pid").map(|pid| pid as u32))
    }

    pub fn notification_show(&self, title: &str, body: &str) -> Result<(), HerdrClientError> {
        self.request("notification.show", json!({ "title": title, "body": body }))
            .map(|_| ())
    }

    pub fn plugin_pane_open(
        &self,
        plugin_id: &str,
        entrypoint: &str,
    ) -> Result<(), HerdrClientError> {
        self.request(
            "plugin.pane.open",
            json!({
                "plugin_id": plugin_id,
                "entrypoint": entrypoint,
                "placement": "split",
                "direction": "right",
                "focus": true,
            }),
        )
        .map(|_| ())
    }

    pub fn pane_report_metadata(
        &self,
        pane_id: &str,
        tokens: HashMap<String, Option<String>>,
        ttl_ms: u64,
    ) -> Result<(), HerdrClientError> {
        self.request(
            "pane.report_metadata",
            json!({
                "pane_id": pane_id,
                "source": Self::METADATA_SOURCE,
                "tokens": tokens,
                "ttl_ms": ttl_ms,
            }),
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_info_decodes_from_snapshot_fixture_panes() {
        let snapshot: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/session-snapshot.json"))
                .unwrap();
        let panes = collect_pane_values(&snapshot);
        assert!(!panes.is_empty(), "snapshot fixture contains panes");
        for pane in panes {
            let info: PaneInfo = serde_json::from_value(pane.clone()).expect("tolerant decode");
            assert!(!info.pane_id.is_empty());
        }
    }

    #[test]
    fn request_fields_match_captured_schemas() {
        let cases = [
            (
                include_str!("../../tests/fixtures/pane-process-info-params-schema.json"),
                json!({ "pane_id": "p1" }),
            ),
            (
                include_str!("../../tests/fixtures/notification-show-params-schema.json"),
                json!({ "title": "Agent Watcher", "body": "done" }),
            ),
            (
                include_str!("../../tests/fixtures/pane-report-metadata-params-schema.json"),
                json!({
                    "pane_id": "p1",
                    "source": HerdrClient::METADATA_SOURCE,
                    "tokens": { "agent_watcher_state": "active" },
                    "ttl_ms": 15_000,
                }),
            ),
            (
                include_str!("../../tests/fixtures/plugin-pane-open-params-schema.json"),
                json!({
                    "plugin_id": "herdr-agent-watcher",
                    "entrypoint": "sidebar",
                    "placement": "split",
                    "direction": "right",
                    "focus": true,
                }),
            ),
        ];

        for (schema, params) in cases {
            let schema: Value = serde_json::from_str(schema).unwrap();
            let allowed = schema["properties"].as_object().unwrap();
            assert!(params
                .as_object()
                .unwrap()
                .keys()
                .all(|key| allowed.contains_key(key)));
            assert!(schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|key| params.get(key.as_str().unwrap()).is_some()));
        }
    }
}
