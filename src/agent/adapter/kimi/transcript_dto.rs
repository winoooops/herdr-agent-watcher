//! Typed, lenient DTOs for the kimi-code `wire.jsonl` grammar.
//!
//! Every line is `{ "type": <dotted string>, "time": <epoch_ms>, ...payload }`.
//! The granular agent-loop lifecycle is nested inside
//! `context.append_loop_event.event`. Mirrors `codex/transcript_dto.rs`:
//! scalar leaves use the shared `lenient_*` deserializers so a wrong-typed
//! or missing field degrades to `None` rather than erroring the whole line.
//! The top-level record is classified manually so an unknown / missing
//! `type` falls through to `Other` instead of erroring.

use serde::Deserialize;
use serde_json::Value;

use crate::agent::adapter::serde_helpers::{lenient_string, lenient_u64};

/// Top-level record classifier — over the dotted `type` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KimiRecordType {
    Metadata,
    ConfigUpdate,
    TurnPrompt,
    AppendLoopEvent,
    UsageRecord,
    Other,
}

/// Inner `context.append_loop_event.event.type` classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KimiLoopEventType {
    ToolCall,
    ToolResult,
    ContentPart,
    StepEnd,
    Other,
}

/// One `wire.jsonl` line: `{type, time, ...payload}`. `#[serde(flatten)]`
/// keeps the payload fields inline so a single struct covers every record
/// shape; each consumer reads only the subset it cares about.
#[derive(Deserialize, Default)]
pub(super) struct KimiLineDto {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub type_tag: Option<String>,

    /// Envelope wall-clock stamp, epoch MILLISECONDS. Used as each emitted
    /// event's timestamp so replay reflects historical times, not `now`.
    #[serde(default, deserialize_with = "lenient_u64")]
    pub time: Option<u64>,

    /// `metadata.app_version` — the kimi-code CLI version.
    #[serde(default, deserialize_with = "lenient_string")]
    pub app_version: Option<String>,

    /// `config.update.modelAlias` — e.g. `"kimi-code/kimi-for-coding"`.
    #[serde(default, rename = "modelAlias", deserialize_with = "lenient_string")]
    pub model_alias: Option<String>,

    /// `usage.record.model` — e.g. `"kimi-code/kimi-for-coding"`.
    #[serde(default, deserialize_with = "lenient_string")]
    pub model: Option<String>,

    /// `usage.record.usage` — the cumulative token usage snapshot.
    #[serde(default)]
    pub usage: Option<KimiUsageDto>,

    /// `turn.prompt.origin` — `{kind}`; `"user"` is a real user turn.
    #[serde(default)]
    pub origin: Option<KimiOriginDto>,

    /// `context.append_loop_event.event` — the nested lifecycle record.
    #[serde(default)]
    pub event: Option<KimiLoopEventDto>,
}

impl KimiLineDto {
    pub fn record_type(&self) -> KimiRecordType {
        match self.type_tag.as_deref() {
            Some("metadata") => KimiRecordType::Metadata,
            Some("config.update") => KimiRecordType::ConfigUpdate,
            Some("turn.prompt") => KimiRecordType::TurnPrompt,
            Some("context.append_loop_event") => KimiRecordType::AppendLoopEvent,
            Some("usage.record") => KimiRecordType::UsageRecord,
            _ => KimiRecordType::Other,
        }
    }
}

/// Token usage block: `usage.record.usage` and `step.end.usage`.
#[derive(Deserialize, Default, Clone)]
pub(super) struct KimiUsageDto {
    #[serde(default, rename = "inputOther", deserialize_with = "lenient_u64")]
    pub input_other: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub output: Option<u64>,
    #[serde(default, rename = "inputCacheRead", deserialize_with = "lenient_u64")]
    pub input_cache_read: Option<u64>,
    #[serde(
        default,
        rename = "inputCacheCreation",
        deserialize_with = "lenient_u64"
    )]
    pub input_cache_creation: Option<u64>,
}

/// `turn.prompt.origin` / `context.append_message.message.origin`.
#[derive(Deserialize, Default)]
pub(super) struct KimiOriginDto {
    #[serde(default, deserialize_with = "lenient_string")]
    pub kind: Option<String>,
}

/// `context.append_loop_event.event` — the nested lifecycle record. Fields
/// are the union across `tool.call` / `tool.result` / `step.end`; every one
/// is `Option`/lenient so an unrecognized event degrades gracefully.
#[derive(Deserialize, Default)]
pub(super) struct KimiLoopEventDto {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub type_tag: Option<String>,

    /// Provider event identity used to deduplicate completion notifications.
    #[serde(default, deserialize_with = "lenient_string")]
    pub uuid: Option<String>,

    /// `tool.call` / `tool.result` correlation id.
    #[serde(default, rename = "toolCallId", deserialize_with = "lenient_string")]
    pub tool_call_id: Option<String>,

    /// `tool.call` tool name (e.g. `"Read"`, `"Bash"`).
    #[serde(default, deserialize_with = "lenient_string")]
    pub name: Option<String>,

    /// `tool.call` arguments (object); summarized for the args string.
    #[serde(default)]
    pub args: Value,

    /// `tool.call` display hint — `{kind, operation, path}`.
    #[serde(default)]
    pub display: Option<KimiToolDisplayDto>,

    /// `tool.result` nested result — `{output}`. Documents the wire
    /// grammar; the decoder pairs DONE by `toolCallId` and does not yet
    /// classify success/failure from the output payload.
    #[serde(default)]
    #[allow(dead_code)]
    pub result: Option<KimiToolResultDto>,

    /// `step.end` per-step usage snapshot.
    #[serde(default)]
    pub usage: Option<KimiUsageDto>,

    /// `step.end` finish reason — `"tool_use"` / `"end_turn"`.
    #[serde(default, rename = "finishReason", deserialize_with = "lenient_string")]
    pub finish_reason: Option<String>,

    /// `content.part` completed assistant part — `{type, text|think}`. Kimi
    /// appends whole parts (not token deltas); only `type == "text"` carries
    /// reply prose (VIM-293).
    #[serde(default)]
    pub part: Option<KimiContentPartDto>,
}

impl KimiLoopEventDto {
    pub fn loop_event_type(&self) -> KimiLoopEventType {
        match self.type_tag.as_deref() {
            Some("tool.call") => KimiLoopEventType::ToolCall,
            Some("tool.result") => KimiLoopEventType::ToolResult,
            Some("content.part") => KimiLoopEventType::ContentPart,
            Some("step.end") => KimiLoopEventType::StepEnd,
            _ => KimiLoopEventType::Other,
        }
    }
}

/// `content.part.part` — one completed assistant content block. `think`
/// blocks carry a `think` field instead of `text`; only `text` is buffered.
#[derive(Deserialize, Default)]
pub(super) struct KimiContentPartDto {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub type_tag: Option<String>,

    #[serde(default, deserialize_with = "lenient_string")]
    pub text: Option<String>,
}

/// `tool.call.display` — carries a resolved absolute `path` for file ops.
#[derive(Deserialize, Default)]
pub(super) struct KimiToolDisplayDto {
    #[serde(default, deserialize_with = "lenient_string")]
    pub path: Option<String>,
}

/// `tool.result.result` — `{output}`. Output is union/arbitrary so it stays
/// a raw `Option<String>` via the lenient deserializer.
#[derive(Deserialize, Default)]
pub(super) struct KimiToolResultDto {
    #[serde(default, deserialize_with = "lenient_string")]
    #[allow(dead_code)]
    pub output: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_type_classifies_dotted_types() {
        let meta: KimiLineDto =
            serde_json::from_str(r#"{"type":"metadata","app_version":"0.14.2"}"#).unwrap();
        assert_eq!(meta.record_type(), KimiRecordType::Metadata);
        assert_eq!(meta.app_version.as_deref(), Some("0.14.2"));

        let cfg: KimiLineDto = serde_json::from_str(
            r#"{"type":"config.update","modelAlias":"kimi-code/kimi-for-coding"}"#,
        )
        .unwrap();
        assert_eq!(cfg.record_type(), KimiRecordType::ConfigUpdate);
        assert_eq!(
            cfg.model_alias.as_deref(),
            Some("kimi-code/kimi-for-coding")
        );

        let loop_evt: KimiLineDto = serde_json::from_str(
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call"}}"#,
        )
        .unwrap();
        assert_eq!(loop_evt.record_type(), KimiRecordType::AppendLoopEvent);
    }

    #[test]
    fn record_type_falls_through_on_unknown_or_missing() {
        let unknown: KimiLineDto = serde_json::from_str(r#"{"type":"brand.new.kind"}"#).unwrap();
        assert_eq!(unknown.record_type(), KimiRecordType::Other);
        let missing: KimiLineDto = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(missing.record_type(), KimiRecordType::Other);
    }

    #[test]
    fn usage_record_maps_camel_case_token_fields() {
        let line: KimiLineDto = serde_json::from_str(
            r#"{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":1884,"output":63,"inputCacheRead":14336,"inputCacheCreation":0}}"#,
        )
        .unwrap();
        assert_eq!(line.record_type(), KimiRecordType::UsageRecord);
        let usage = line.usage.expect("usage block present");
        assert_eq!(usage.input_other, Some(1884));
        assert_eq!(usage.output, Some(63));
        assert_eq!(usage.input_cache_read, Some(14336));
        assert_eq!(usage.input_cache_creation, Some(0));
    }

    #[test]
    fn loop_event_tool_call_parses_id_name_and_display_path() {
        let line: KimiLineDto = serde_json::from_str(
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"tool_abc","name":"Read","args":{"path":"note.txt"},"display":{"kind":"file_io","operation":"read","path":"/tmp/kimi-probe/note.txt"}}}"#,
        )
        .unwrap();
        let event = line.event.expect("event present");
        assert_eq!(event.loop_event_type(), KimiLoopEventType::ToolCall);
        assert_eq!(event.tool_call_id.as_deref(), Some("tool_abc"));
        assert_eq!(event.name.as_deref(), Some("Read"));
        assert_eq!(
            event.display.and_then(|d| d.path).as_deref(),
            Some("/tmp/kimi-probe/note.txt")
        );
    }

    #[test]
    fn loop_event_content_part_parses_text_and_skips_think() {
        // Mirrors a real wire line: the part is a COMPLETE block, not a delta.
        let line: KimiLineDto = serde_json::from_str(
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","uuid":"u1","turnId":"0","step":1,"part":{"type":"text","text":"the reply prose"}}}"#,
        )
        .unwrap();
        let event = line.event.expect("event present");
        assert_eq!(event.loop_event_type(), KimiLoopEventType::ContentPart);
        let part = event.part.expect("part present");
        assert_eq!(part.type_tag.as_deref(), Some("text"));
        assert_eq!(part.text.as_deref(), Some("the reply prose"));

        let think: KimiLineDto = serde_json::from_str(
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"think","think":"reasoning"}}}"#,
        )
        .unwrap();
        let think_part = think.event.expect("event").part.expect("part");
        assert_eq!(think_part.type_tag.as_deref(), Some("think"));
        assert_eq!(think_part.text, None);
    }

    #[test]
    fn loop_event_step_end_reads_usage_and_finish_reason() {
        let line: KimiLineDto = serde_json::from_str(
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","usage":{"inputOther":211,"output":35,"inputCacheRead":16128,"inputCacheCreation":0},"finishReason":"end_turn"}}"#,
        )
        .unwrap();
        let event = line.event.expect("event present");
        assert_eq!(event.loop_event_type(), KimiLoopEventType::StepEnd);
        assert_eq!(event.finish_reason.as_deref(), Some("end_turn"));
        assert_eq!(event.usage.expect("step usage").output, Some(35));
    }

    #[test]
    fn turn_prompt_origin_kind_user() {
        let line: KimiLineDto = serde_json::from_str(
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"origin":{"kind":"user"}}"#,
        )
        .unwrap();
        assert_eq!(line.record_type(), KimiRecordType::TurnPrompt);
        assert_eq!(line.origin.and_then(|o| o.kind).as_deref(), Some("user"));
    }
}
