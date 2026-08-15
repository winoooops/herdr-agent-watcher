use std::collections::HashMap;

use crate::daemon::store::PaneTelemetry;

pub const WIRE_VERSION: u32 = 1;

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
        let hello: Hello = serde_json::from_str(r#"{"version":1,"seq":2}"#).unwrap();
        assert!(hello.panes.is_empty());
        let encoded = serde_json::to_string(&hello).unwrap();
        let decoded: Hello = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.version, WIRE_VERSION);

        let delta: Delta = serde_json::from_str(r#"{"seq":3,"pane_id":"p1"}"#).unwrap();
        assert!(delta.telemetry.is_none());
    }
}
