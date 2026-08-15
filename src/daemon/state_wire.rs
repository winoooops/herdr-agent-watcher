use std::collections::HashMap;

use crate::daemon::store::PaneTelemetry;

pub const WIRE_VERSION: u32 = 2;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct StatusPath {
    pub version: u32,
    pub path: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Hello {
    pub version: u32,
    pub seq: u64,
    #[serde(default)]
    pub panes: HashMap<String, PaneTelemetry>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Delta {
    pub seq: u64,
    pub pane_id: String,
    #[serde(default)]
    pub telemetry: Option<PaneTelemetry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_types_round_trip_and_default_optional_fields() {
        let hello: Hello = serde_json::from_str(r#"{"version":2,"seq":2}"#).unwrap();
        assert!(hello.panes.is_empty());
        let encoded = serde_json::to_string(&hello).unwrap();
        let decoded: Hello = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.version, WIRE_VERSION);

        let delta: Delta = serde_json::from_str(r#"{"seq":3,"pane_id":"p1"}"#).unwrap();
        assert!(delta.telemetry.is_none());
    }

    /// §3.1: no WIRE_VERSION bump, because the change is compatible in the
    /// direction that matters — a sidebar built BEFORE it ignores a field it
    /// does not know. The reverse (new struct, old payload) is what
    /// `#[serde(default)]` already covers and says nothing about this claim.
    #[test]
    fn a_sidebar_built_before_workspace_id_ignores_it() {
        #[derive(serde::Deserialize)]
        struct LegacyPaneTelemetry {
            #[serde(default)]
            agent: Option<String>,
            #[serde(default)]
            cwd: Option<String>,
            #[serde(default)]
            updated_seq: u64,
        }
        #[derive(serde::Deserialize)]
        struct LegacyDelta {
            seq: u64,
            pane_id: String,
            #[serde(default)]
            telemetry: Option<LegacyPaneTelemetry>,
        }

        let mut telemetry = PaneTelemetry::with_agent("claude");
        telemetry.cwd = Some("/work".into());
        telemetry.workspace_id = Some("w4".into());
        let payload = serde_json::to_string(&Delta {
            seq: 7,
            pane_id: "w4:p1".into(),
            telemetry: Some(telemetry),
        })
        .expect("serialize");
        assert!(payload.contains("workspace_id"), "{payload}");

        let legacy: LegacyDelta = serde_json::from_str(&payload).expect("legacy decode");
        assert_eq!(legacy.seq, 7);
        assert_eq!(legacy.pane_id, "w4:p1");
        let t = legacy.telemetry.expect("telemetry");
        assert_eq!(t.agent.as_deref(), Some("claude"));
        assert_eq!(t.cwd.as_deref(), Some("/work"));
        assert_eq!(t.updated_seq, 0);
    }
}
