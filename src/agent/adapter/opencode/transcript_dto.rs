//! Lenient wire DTOs for the opencode bridge JSONL (one record per line).
//!
//! The bridge plugin writes three line kinds — `event`, `tool.before`, and
//! `tool.after` — plus an `index.jsonl` with `{sessionID, pid, directory, slug,
//! time}` rows. [`OpencodeLineDto`] is a single flat, lenient struct covering
//! all three line kinds: every field is `#[serde(default)]` and scalar leaves
//! use the shared `lenient_*` deserializers, so a wrong-typed field degrades to
//! `None` instead of erroring the whole line (mirrors `codex/transcript_dto`).
//!
//! `kind` and the event `type` classify via `#[serde(other)]` enums so an
//! unknown line kind or a newer/unknown event type still parses (as `Unknown` /
//! `Other`) rather than poisoning the line.

use serde::Deserialize;
use serde_json::Value;

use crate::agent::adapter::serde_helpers::{lenient_i64, lenient_string, lenient_u64};

/// Line-kind classifier. The bridge emits exactly `event` / `tool.before` /
/// `tool.after`; anything else maps to `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum OpencodeKind {
    Event,
    #[serde(rename = "tool.before")]
    ToolBefore,
    #[serde(rename = "tool.after")]
    ToolAfter,
    #[serde(other)]
    Unknown,
}

/// Whitelisted opencode bus event types (the `type` field on a `kind: "event"`
/// line). Any non-whitelisted / newer type maps to `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) enum OpencodeEventType {
    #[serde(rename = "session.created")]
    SessionCreated,
    #[serde(rename = "session.updated")]
    SessionUpdated,
    #[serde(rename = "session.idle")]
    SessionIdle,
    #[serde(rename = "session.status")]
    SessionStatus,
    #[serde(rename = "session.error")]
    SessionError,
    #[serde(rename = "permission.asked")]
    PermissionAsked,
    #[serde(rename = "permission.replied")]
    PermissionReplied,
    #[serde(rename = "question.asked")]
    QuestionAsked,
    #[serde(rename = "question.replied")]
    QuestionReplied,
    #[serde(rename = "question.rejected")]
    QuestionRejected,
    #[serde(rename = "session.diff")]
    SessionDiff,
    #[serde(rename = "message.updated")]
    MessageUpdated,
    #[serde(rename = "message.part.updated")]
    MessagePartUpdated,
    /// Bridge-synthesized (not an opencode bus event): the assistant's
    /// aggregated turn text, written once at `session.idle` (VIM-293).
    #[serde(rename = "assistant.text")]
    AssistantText,
    #[serde(rename = "todo.updated")]
    TodoUpdated,
    #[serde(other)]
    Other,
}

impl OpencodeEventType {
    /// Classify an optional raw `type` string. A missing / non-matching value
    /// maps to `Other`.
    pub(crate) fn classify(raw: Option<&str>) -> Self {
        match raw {
            Some(value) => {
                serde_json::from_value(Value::String(value.to_string())).unwrap_or(Self::Other)
            }
            None => Self::Other,
        }
    }
}

/// One bridge JSONL line, flattened over all three kinds. Every field is
/// optional/lenient so any line shape deserializes without error.
///
/// `#[allow(dead_code)]`: this is a wire-schema mirror — `v` (schema version)
/// and the line-level `session_id` are parsed + test-asserted to document the
/// contract, but the v1 decoder routes off `kind`/`event_type`/`data`/`tool`/
/// `call_id`/`args`/`result` and never reads them. Keeping the fields makes the
/// DTO a faithful 1:1 of the bridge line.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct OpencodeLineDto {
    /// Schema version (`1`).
    #[serde(default, deserialize_with = "lenient_u64")]
    pub v: Option<u64>,
    /// Emit timestamp in ms.
    #[serde(default, deserialize_with = "lenient_i64")]
    pub ts: Option<i64>,
    /// Raw line kind; classify via [`OpencodeLineDto::kind`].
    #[serde(default, deserialize_with = "lenient_string")]
    pub kind: Option<String>,
    /// Bus event type (only on `kind: "event"` lines).
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub event_type: Option<String>,
    /// Event payload (`event.properties`) for `kind: "event"` lines.
    #[serde(default)]
    pub data: Value,
    /// Tool name (on tool lines).
    #[serde(default, deserialize_with = "lenient_string")]
    pub tool: Option<String>,
    /// Session id (on tool lines; bus lines carry it inside `data`).
    #[serde(rename = "sessionID", default, deserialize_with = "lenient_string")]
    pub session_id: Option<String>,
    /// Tool call id (on tool lines).
    #[serde(rename = "callID", default, deserialize_with = "lenient_string")]
    pub call_id: Option<String>,
    /// Previewed tool args (on `tool.before` lines).
    #[serde(default)]
    pub args: Value,
    /// Tool result block (on `tool.after` lines).
    #[serde(default)]
    pub result: Value,
}

impl OpencodeLineDto {
    /// Classify the raw `kind` string. Missing / unknown maps to `Unknown`.
    pub(crate) fn kind(&self) -> OpencodeKind {
        match self.kind.as_deref() {
            Some(raw) => serde_json::from_value(Value::String(raw.to_string()))
                .unwrap_or(OpencodeKind::Unknown),
            None => OpencodeKind::Unknown,
        }
    }

    /// Classify the event `type`. Only meaningful on `kind: "event"` lines.
    pub(crate) fn event_type(&self) -> OpencodeEventType {
        OpencodeEventType::classify(self.event_type.as_deref())
    }
}

/// One `index.jsonl` row: identifies a session and its owning opencode process.
///
/// `#[allow(dead_code)]`: `slug` (the opencode session display slug) is parsed +
/// test-asserted to mirror the index schema, but the v1 locator resolves off
/// `session_id`/`pid`/`directory`/`time` only and never reads it.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct OpencodeIndexRowDto {
    #[serde(rename = "sessionID", default, deserialize_with = "lenient_string")]
    pub session_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub pid: Option<u64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub directory: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub slug: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub time: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/agent/adapter/opencode/fixtures")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    #[test]
    fn kind_classifies_known_and_unknown() {
        let event: OpencodeLineDto = serde_json::from_str(r#"{"kind":"event"}"#).unwrap();
        assert_eq!(event.kind(), OpencodeKind::Event);

        let before: OpencodeLineDto = serde_json::from_str(r#"{"kind":"tool.before"}"#).unwrap();
        assert_eq!(before.kind(), OpencodeKind::ToolBefore);

        let after: OpencodeLineDto = serde_json::from_str(r#"{"kind":"tool.after"}"#).unwrap();
        assert_eq!(after.kind(), OpencodeKind::ToolAfter);

        let unknown: OpencodeLineDto = serde_json::from_str(r#"{"kind":"brand.new"}"#).unwrap();
        assert_eq!(unknown.kind(), OpencodeKind::Unknown);

        let missing: OpencodeLineDto = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(missing.kind(), OpencodeKind::Unknown);
    }

    #[test]
    fn event_type_classifies_known_and_unknown() {
        let created: OpencodeLineDto =
            serde_json::from_str(r#"{"kind":"event","type":"session.created"}"#).unwrap();
        assert_eq!(created.event_type(), OpencodeEventType::SessionCreated);

        let part: OpencodeLineDto =
            serde_json::from_str(r#"{"kind":"event","type":"message.part.updated"}"#).unwrap();
        assert_eq!(part.event_type(), OpencodeEventType::MessagePartUpdated);

        // Unknown / newer event type still parses, classified Other.
        let drift: OpencodeLineDto =
            serde_json::from_str(r#"{"kind":"event","type":"session.teleported"}"#).unwrap();
        assert_eq!(drift.event_type(), OpencodeEventType::Other);

        let missing: OpencodeLineDto = serde_json::from_str(r#"{"kind":"event"}"#).unwrap();
        assert_eq!(missing.event_type(), OpencodeEventType::Other);
    }

    #[test]
    fn lenient_scalar_drift_degrades_to_none_without_erroring_line() {
        // `ts` is a number-typed field; a string there must degrade to None,
        // and the rest of the line must still parse.
        let dto: OpencodeLineDto = serde_json::from_str(
            r#"{"v":1,"ts":"not-a-number","kind":"event","type":"session.idle","tool":42}"#,
        )
        .expect("type-drifted line must still parse");
        assert_eq!(dto.ts, None);
        // `tool` is a string-typed field fed a number → None, siblings intact.
        assert_eq!(dto.tool, None);
        assert_eq!(dto.v, Some(1));
        assert_eq!(dto.kind(), OpencodeKind::Event);
        assert_eq!(dto.event_type(), OpencodeEventType::SessionIdle);
    }

    #[test]
    fn assistant_text_classifies_and_carries_data_text() {
        // Bridge-synthesized record (VIM-293) — not an opencode bus event.
        let dto: OpencodeLineDto = serde_json::from_str(
            r#"{"v":1,"ts":1,"kind":"event","type":"assistant.text","data":{"sessionID":"ses_x","text":"the reply"}}"#,
        )
        .expect("assistant.text parses");
        assert_eq!(dto.kind(), OpencodeKind::Event);
        assert_eq!(dto.event_type(), OpencodeEventType::AssistantText);
        assert_eq!(
            dto.data.get("text").and_then(serde_json::Value::as_str),
            Some("the reply")
        );
    }

    #[test]
    fn unknown_kind_and_type_still_parse() {
        let dto: OpencodeLineDto = serde_json::from_str(
            r#"{"v":1,"ts":1,"kind":"totally.new","type":"also.new","data":{"x":1}}"#,
        )
        .expect("unknown kind + type must not poison the line");
        assert_eq!(dto.kind(), OpencodeKind::Unknown);
        assert_eq!(dto.event_type(), OpencodeEventType::Other);
        assert_eq!(dto.data["x"], serde_json::json!(1));
    }

    #[test]
    fn index_row_round_trips() {
        let row: OpencodeIndexRowDto = serde_json::from_str(
            r#"{"sessionID":"ses_abc","pid":4242,"directory":"/work/proj","slug":"happy-otter","time":1781965827596}"#,
        )
        .unwrap();
        assert_eq!(row.session_id.as_deref(), Some("ses_abc"));
        assert_eq!(row.pid, Some(4242));
        assert_eq!(row.directory.as_deref(), Some("/work/proj"));
        assert_eq!(row.slug.as_deref(), Some("happy-otter"));
        assert_eq!(row.time, Some(1781965827596));
    }

    /// Every line in the authored bridge fixture parses, and the kinds / event
    /// types classify as expected.
    #[test]
    fn sample_bridge_fixture_parses_every_line() {
        let raw = fixture("sample_bridge.jsonl");
        let mut kinds = Vec::new();
        let mut event_types = Vec::new();

        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let dto: OpencodeLineDto = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("fixture line failed to parse: {line}\n{e}"));
            assert_eq!(dto.v, Some(1), "every line carries v=1: {line}");
            kinds.push(dto.kind());
            if dto.kind() == OpencodeKind::Event {
                let et = dto.event_type();
                assert_ne!(
                    et,
                    OpencodeEventType::Other,
                    "fixture event type unknown: {line}"
                );
                event_types.push(et);
            } else {
                // Tool lines must carry a session id + tool name.
                assert!(
                    dto.session_id.is_some(),
                    "tool line missing sessionID: {line}"
                );
                assert!(dto.tool.is_some(), "tool line missing tool: {line}");
            }
        }

        // The fixture must cover at least the keystone shapes.
        assert!(kinds.contains(&OpencodeKind::Event));
        assert!(kinds.contains(&OpencodeKind::ToolBefore));
        assert!(kinds.contains(&OpencodeKind::ToolAfter));
        assert!(event_types.contains(&OpencodeEventType::SessionCreated));
        assert!(event_types.contains(&OpencodeEventType::SessionUpdated));
        assert!(event_types.contains(&OpencodeEventType::MessageUpdated));
        assert!(event_types.contains(&OpencodeEventType::SessionStatus));
        assert!(event_types.contains(&OpencodeEventType::SessionIdle));
        assert!(event_types.contains(&OpencodeEventType::MessagePartUpdated));
    }

    #[test]
    fn sample_index_fixture_parses() {
        let raw = fixture("sample_index.jsonl");
        let mut rows = 0;
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let row: OpencodeIndexRowDto = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("index line failed to parse: {line}\n{e}"));
            assert!(
                row.session_id.is_some(),
                "index row missing sessionID: {line}"
            );
            assert!(row.pid.is_some(), "index row missing pid: {line}");
            rows += 1;
        }
        assert!(rows >= 1, "sample_index.jsonl must have at least one row");
    }
}
