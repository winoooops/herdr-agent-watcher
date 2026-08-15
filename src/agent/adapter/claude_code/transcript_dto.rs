//! Typed, lenient DTOs for the Claude transcript JSONL parser (A-transcript,
//! option B). Scalar leaves are typed via the shared `lenient_*` deserializers
//! so wrong-typed/missing fields degrade to `None` (never erroring the parse);
//! the genuinely union/arbitrary fields (`message.content`, `tool_*.content`,
//! `tool_use.input`) stay raw `Value` and keep their ported predicates/helpers.
//!
//! Invariant: **every field is `#[serde(default)]`/lenient**, so a line of *any*
//! shape deserializes without error — a non-`tool_result` line simply gets
//! `content = Value::Null` and `tool_use_id`/`is_error = None`. This lets the
//! parser apply `ClaudeTranscriptLineDto` to every line, not just `tool_result`.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::agent::adapter::serde_helpers::{lenient_bool, lenient_object, lenient_string};

/// One transcript JSONL line. Carries the top-level fields `process_line`
/// reads — including the top-level `cwd` (emits `agent-cwd` before the
/// `line_type` dispatch), the top-level `tool_result` shape
/// (`tool_use_id` / `is_error` / `content`), and the top-level `ai-title`
/// / `custom-title` shape (`sessionId` / `aiTitle` / `customTitle`), since
/// all of these are top-level line types alongside `assistant` / `user`.
#[derive(Deserialize, Default)]
pub(super) struct ClaudeTranscriptLineDto {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub line_type: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub cwd: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub timestamp: Option<String>,
    #[serde(default, deserialize_with = "lenient_object")]
    pub message: Option<ClaudeMessageDto>,
    // Top-level tool_result fields:
    #[serde(default, deserialize_with = "lenient_string")]
    pub tool_use_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_bool")]
    pub is_error: Option<bool>,
    /// Raw — consumed by the ported `extract_tool_result_content`.
    #[serde(default)]
    pub content: Value,
    // Top-level title-line fields (PR #302 cycle 2 — F1+F2: re-add
    // `ai-title` / `custom-title` handling that was dropped during PR
    // #287's tail_loop → TranscriptTailService extraction). The lines
    // carry `sessionId` (Claude's own session id; filter against the
    // tailed file's stem) and one of `aiTitle` / `customTitle`.
    #[serde(default, deserialize_with = "lenient_string", rename = "sessionId")]
    pub session_id_field: Option<String>,
    #[serde(default, deserialize_with = "lenient_string", rename = "aiTitle")]
    pub ai_title: Option<String>,
    #[serde(default, deserialize_with = "lenient_string", rename = "customTitle")]
    pub custom_title: Option<String>,
}

/// `message` envelope. `content` is string | array | other and stays raw —
/// classified by the ported predicates (`is_user_prompt`, `line_type`, …),
/// which a tagged enum can't express (missing/non-string block `type` must
/// count as content).
#[derive(Deserialize, Default)]
pub(super) struct ClaudeMessageDto {
    #[serde(default)]
    pub content: Value,
    /// Assistant turn-boundary signal: `tool_use` = running, `end_turn` /
    /// `stop_sequence` / `max_tokens` = idle. Absent on non-assistant lines.
    #[serde(default, deserialize_with = "lenient_string")]
    pub stop_reason: Option<String>,
}

/// A `tool_use` content block. Scalars are typed; `input` is presence-sensitive
/// (`summarize_input` returns `""` for absent vs `"null"` for present-`null`),
/// so it is read off the flattened raw map rather than a collapsing `Option`.
#[derive(Deserialize, Default)]
pub(super) struct ClaudeToolUseDto {
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub name: Option<String>,
    /// Captures `input` (and any other non-scalar fields). `rest.get("input")`
    /// gives the `Option<&Value>` the input consumers expect: `None` = absent,
    /// `Some(Null)` = present-`null`.
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// A `tool_result` content block. Scalars typed; `content` raw (string |
/// array-of-text-blocks) — `extract_tool_result_content` consumes the value.
#[derive(Deserialize, Default)]
pub(super) struct ClaudeToolResultDto {
    #[serde(default, deserialize_with = "lenient_string")]
    pub tool_use_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_bool")]
    pub is_error: Option<bool>,
    #[serde(default)]
    pub content: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_line_dto_parses_envelope_and_top_level_cwd() {
        let dto: ClaudeTranscriptLineDto = serde_json::from_str(
            r#"{"type":"assistant","cwd":"/ws","timestamp":"t","message":{"content":[]}}"#,
        )
        .expect("envelope parses");
        assert_eq!(dto.line_type.as_deref(), Some("assistant"));
        assert_eq!(dto.cwd.as_deref(), Some("/ws"));
        assert_eq!(dto.timestamp.as_deref(), Some("t"));
        assert!(dto.message.is_some());
    }

    #[test]
    fn claude_line_dto_tolerates_any_shape_without_error() {
        // Non-tool_result line: no top-level content / tool_use_id → defaults, no error.
        let dto: ClaudeTranscriptLineDto =
            serde_json::from_str(r#"{"type":"user","message":{"content":"hi"}}"#)
                .expect("non-tool_result line parses");
        assert_eq!(dto.line_type.as_deref(), Some("user"));
        assert!(dto.tool_use_id.is_none());
        assert!(dto.is_error.is_none());
        assert_eq!(dto.content, Value::Null);

        // Wrong-shaped `message` degrades to None via lenient_object (not an error).
        let dto: ClaudeTranscriptLineDto =
            serde_json::from_str(r#"{"type":"user","message":42}"#).expect("wrong message parses");
        assert!(dto.message.is_none());

        // Entirely unknown shape still parses (every field defaulted/lenient).
        let dto: ClaudeTranscriptLineDto =
            serde_json::from_str(r#"{"unexpected":true}"#).expect("unknown line parses");
        assert!(dto.line_type.is_none());
    }

    #[test]
    fn claude_tool_result_dto_is_error_is_lenient() {
        // Wrong-typed is_error degrades to None instead of erroring the block.
        let dto: ClaudeToolResultDto =
            serde_json::from_str(r#"{"tool_use_id":"x","is_error":"oops","content":"c"}"#)
                .expect("tool_result with bad is_error parses");
        assert_eq!(dto.tool_use_id.as_deref(), Some("x"));
        assert_eq!(dto.is_error, None);
        assert_eq!(dto.content, Value::String("c".to_string()));
    }

    #[test]
    fn claude_tool_use_dto_distinguishes_absent_vs_null_input() {
        let absent: ClaudeToolUseDto =
            serde_json::from_str(r#"{"id":"i","name":"Read"}"#).expect("tool_use absent input");
        assert_eq!(absent.id.as_deref(), Some("i"));
        assert_eq!(absent.name.as_deref(), Some("Read"));
        assert!(absent.rest.get("input").is_none(), "absent input → None");

        let nulled: ClaudeToolUseDto =
            serde_json::from_str(r#"{"id":"i","name":"Read","input":null}"#)
                .expect("tool_use null input");
        assert_eq!(
            nulled.rest.get("input"),
            Some(&Value::Null),
            "present-null input preserved (≠ absent)"
        );
    }
}
