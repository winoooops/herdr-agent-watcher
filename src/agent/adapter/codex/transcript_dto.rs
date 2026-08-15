//! Typed, lenient DTOs for the Codex transcript JSONL parser (A-transcript,
//! option B). Scalar leaves are typed via the shared `lenient_*` deserializers
//! so wrong-typed/missing fields degrade to `None` (never erroring the parse);
//! the genuinely union/arbitrary fields (`arguments`, `output`, custom-tool
//! `input`) stay raw `Option<String>` and keep their ported helpers.
//!
//! Invariant: **every field is `#[serde(default)]`/lenient**, so a line of *any*
//! shape deserializes without error. The parser applies `CodexLineDto` to every
//! line; wrong-shaped payloads degrade via the classifier `Other` path, not a
//! hard parse error.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::agent::adapter::serde_helpers::{lenient_bool, lenient_i64, lenient_string};

/// Top-level record type classifier — manual over `Option<String>` so a
/// missing or non-string `type` falls through to `Other` instead of erroring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CodexRecordType {
    SessionMeta,
    ResponseItem,
    EventMsg,
    Other,
}

/// Inner payload-type classifier — same manual pattern for `payload.type`.
/// Used for BOTH `response_item` and `event_msg` payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CodexPayloadType {
    FunctionCall,
    FunctionCallOutput,
    ExecCommandEnd,
    UserMessage,
    PatchApplyEnd,
    CustomToolCall,
    CustomToolCallOutput,
    TaskStarted,
    TaskComplete,
    TurnAborted,
    ExecApprovalRequest,
    ApplyPatchApprovalRequest,
    RequestPermissions,
    ElicitationRequest,
    Other,
}

/// One transcript JSONL line: `{timestamp, type, payload}`.
#[derive(Deserialize, Default)]
pub(super) struct CodexLineDto {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub type_tag: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub payload: Value,
}

impl CodexLineDto {
    pub fn record_type(&self) -> CodexRecordType {
        match self.type_tag.as_deref() {
            Some("session_meta") => CodexRecordType::SessionMeta,
            Some("response_item") => CodexRecordType::ResponseItem,
            Some("event_msg") => CodexRecordType::EventMsg,
            _ => CodexRecordType::Other,
        }
    }
}

/// Per-payload DTO with all scalar leaves typed lenient. Different payload
/// types use different subsets; every field is `Option`/`Default` so an
/// unrecognized payload degrades gracefully.
#[derive(Deserialize, Default)]
pub(super) struct CodexPayloadDto {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub type_tag: Option<String>,
    /// session_meta's own session id; captured as the agent-lifecycle identity.
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub call_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub approval_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub request_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub namespace: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub message: Option<String>,
    /// The completed agent reply on a `task_complete` event_msg (VIM-283).
    #[serde(default, deserialize_with = "lenient_string")]
    pub last_agent_message: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub cwd: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub aggregated_output: Option<String>,
    #[serde(default, deserialize_with = "lenient_bool")]
    pub success: Option<bool>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub exit_code: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub arguments: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub output: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub input: Option<String>,
    /// Captures `duration` (and any other non-scalar fields). Presence is
    /// `rest.contains_key("duration")`; the value is `rest.get("duration")`.
    /// Present (even `null` / non-object) → `Some(0)`; absent → `None`.
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl CodexPayloadDto {
    pub fn payload_type(&self) -> CodexPayloadType {
        match self.type_tag.as_deref() {
            Some("function_call") => CodexPayloadType::FunctionCall,
            Some("function_call_output") => CodexPayloadType::FunctionCallOutput,
            Some("exec_command_end") => CodexPayloadType::ExecCommandEnd,
            Some("user_message") => CodexPayloadType::UserMessage,
            Some("patch_apply_end") => CodexPayloadType::PatchApplyEnd,
            Some("custom_tool_call") => CodexPayloadType::CustomToolCall,
            Some("custom_tool_call_output") => CodexPayloadType::CustomToolCallOutput,
            Some("task_started") => CodexPayloadType::TaskStarted,
            Some("task_complete") => CodexPayloadType::TaskComplete,
            Some("turn_aborted") => CodexPayloadType::TurnAborted,
            Some("exec_approval_request") => CodexPayloadType::ExecApprovalRequest,
            Some("apply_patch_approval_request") => CodexPayloadType::ApplyPatchApprovalRequest,
            Some("request_user_input") => CodexPayloadType::RequestPermissions,
            Some("elicitation_request") => CodexPayloadType::ElicitationRequest,
            _ => CodexPayloadType::Other,
        }
    }
}

/// Re-parsed from `function_call.arguments` (a JSON-encoded string). Fields use
/// `lenient_string` so a wrong-typed field degrades to `None` *independently* —
/// matching the original per-field `args.get(k).and_then(Value::as_str)` reads
/// (a plain `Option<String>` would fail the whole struct parse on one bad
/// field, losing the sibling `cmd`/`path` reads).
#[derive(Deserialize, Default)]
pub(super) struct CodexExecArgsDto {
    #[serde(default, deserialize_with = "lenient_string")]
    pub workdir: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub cmd: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub command: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub path: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub file_path: Option<String>,
}

/// Re-parsed from custom-tool `output` (a JSON-encoded string).
#[derive(Deserialize, Default)]
pub(super) struct CodexCustomToolOutputDto {
    #[serde(default)]
    pub metadata: Option<CodexCustomToolMetadataDto>,
}

#[derive(Deserialize, Default)]
pub(super) struct CodexCustomToolMetadataDto {
    #[serde(default, deserialize_with = "lenient_i64")]
    pub exit_code: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_record_type_falls_through_on_unknown_missing_or_non_string() {
        let unknown: CodexLineDto =
            serde_json::from_str(r#"{"type":"brand_new_kind","payload":{}}"#).unwrap();
        assert!(matches!(unknown.record_type(), CodexRecordType::Other));
        let missing: CodexLineDto = serde_json::from_str(r#"{"payload":{}}"#).unwrap();
        assert!(matches!(missing.record_type(), CodexRecordType::Other));
        let nonstring: CodexLineDto = serde_json::from_str(r#"{"type":42,"payload":{}}"#).unwrap();
        assert!(matches!(nonstring.record_type(), CodexRecordType::Other));
    }

    #[test]
    fn codex_payload_type_falls_through_on_unknown_missing_or_non_string() {
        let unknown: CodexPayloadDto =
            serde_json::from_str(r#"{"type":"brand_new_kind"}"#).unwrap();
        assert!(matches!(unknown.payload_type(), CodexPayloadType::Other));
        let missing: CodexPayloadDto = serde_json::from_str(r#"{}"#).unwrap();
        assert!(matches!(missing.payload_type(), CodexPayloadType::Other));
        let nonstring: CodexPayloadDto = serde_json::from_str(r#"{"type":42}"#).unwrap();
        assert!(matches!(nonstring.payload_type(), CodexPayloadType::Other));
    }

    #[test]
    fn codex_payload_preserves_function_call_namespace() {
        let payload: CodexPayloadDto = serde_json::from_str(
            r#"{"type":"function_call","name":"send_message","namespace":"collaboration"}"#,
        )
        .unwrap();

        assert_eq!(payload.name.as_deref(), Some("send_message"));
        assert_eq!(payload.namespace.as_deref(), Some("collaboration"));
    }

    #[test]
    fn codex_payload_classifies_semantic_attention_events() {
        for (type_tag, expected) in [
            (
                "exec_approval_request",
                CodexPayloadType::ExecApprovalRequest,
            ),
            (
                "apply_patch_approval_request",
                CodexPayloadType::ApplyPatchApprovalRequest,
            ),
            ("request_user_input", CodexPayloadType::RequestPermissions),
            ("elicitation_request", CodexPayloadType::ElicitationRequest),
        ] {
            let payload: CodexPayloadDto =
                serde_json::from_value(serde_json::json!({ "type": type_tag })).unwrap();
            assert_eq!(payload.payload_type(), expected);
        }
    }

    #[test]
    fn codex_exec_end_exit_code_is_lenient_and_duration_presence_preserved() {
        let p: CodexPayloadDto =
            serde_json::from_str(r#"{"exit_code":"bad","aggregated_output":"o"}"#).unwrap();
        assert_eq!(p.exit_code, None);
        assert!(p.rest.get("duration").is_none());

        let p2: CodexPayloadDto = serde_json::from_str(r#"{"duration":null}"#).unwrap();
        assert_eq!(p2.rest.get("duration"), Some(&Value::Null));
    }

    #[test]
    fn codex_duration_present_non_object_yields_some_zero() {
        let p: CodexPayloadDto = serde_json::from_str(r#"{"duration":123}"#).unwrap();
        assert!(p.rest.contains_key("duration"));
        assert_eq!(p.rest.get("duration"), Some(&Value::Number(123.into())));
    }

    #[test]
    fn codex_line_dto_parses_non_object_payload_without_error() {
        let dto: CodexLineDto =
            serde_json::from_str(r#"{"type":"event_msg","payload":42}"#).unwrap();
        assert_eq!(dto.record_type(), CodexRecordType::EventMsg);
        assert_eq!(dto.payload, Value::Number(42.into()));
    }

    #[test]
    fn codex_custom_tool_output_dto_reads_metadata_exit_code() {
        let dto: CodexCustomToolOutputDto =
            serde_json::from_str(r#"{"metadata":{"exit_code":1}}"#).unwrap();
        assert_eq!(dto.metadata.unwrap().exit_code, Some(1));

        let dto2: CodexCustomToolOutputDto =
            serde_json::from_str(r#"{"metadata":{"exit_code":"oops"}}"#).unwrap();
        assert_eq!(dto2.metadata.unwrap().exit_code, None);
    }
}
