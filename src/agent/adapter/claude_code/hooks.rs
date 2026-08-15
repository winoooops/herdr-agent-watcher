//! Claude Code hook stream decoder for semantic attention events.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

use crate::agent::adapter::base::{TranscriptDecoder, TranscriptTailService};
use crate::agent::events::record_attention;
use crate::agent::types::{AgentAttentionEvent, AgentAttentionReason};
use crate::runtime::EventSink;

const MAX_BODY_LEN: usize = 500;
const MAX_ATTENTION_DEDUPE_LEN: usize = 256;

#[derive(Default, Deserialize)]
struct ClaudeHookDto {
    #[serde(default)]
    hook_event_name: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    error: Value,
    #[serde(default)]
    error_details: Option<String>,
}

struct ClaudeHookDecoder {
    events: Arc<dyn EventSink>,
    session_id: String,
    replay_done: bool,
}

impl ClaudeHookDecoder {
    fn new(events: Arc<dyn EventSink>, session_id: String) -> Self {
        Self {
            events,
            session_id,
            replay_done: false,
        }
    }

    fn emit(&self, dto: &ClaudeHookDto, reason: AgentAttentionReason, title: &str) {
        let body = if matches!(reason, AgentAttentionReason::AgentError) {
            dto.error
                .as_str()
                .or_else(|| dto.error.get("message").and_then(Value::as_str))
                .or(dto.error_details.as_deref())
                .filter(|body| !body.is_empty())
                .map(|body| truncate(body, MAX_BODY_LEN))
        } else {
            None
        };
        record_attention(
            &self.events,
            AgentAttentionEvent {
                pty_id: self.session_id.clone(),
                reason,
                title: title.to_string(),
                body,
                occurred_at: now_epoch_ms(),
                dedupe_key: dto
                    .tool_use_id
                    .as_deref()
                    .map(|id| truncate(id, MAX_ATTENTION_DEDUPE_LEN)),
            },
            self.replay_done,
        );
    }
}

impl TranscriptDecoder for ClaudeHookDecoder {
    fn decode_line(&mut self, line: &str) {
        let Ok(dto) = serde_json::from_str::<ClaudeHookDto>(line) else {
            return;
        };
        match dto.hook_event_name.as_str() {
            "PermissionRequest" => self.emit(
                &dto,
                AgentAttentionReason::ApprovalRequested,
                "Claude needs approval",
            ),
            "PreToolUse" if dto.tool_name == "AskUserQuestion" => self.emit(
                &dto,
                AgentAttentionReason::QuestionRequested,
                "Claude has a question",
            ),
            "StopFailure" => self.emit(&dto, AgentAttentionReason::AgentError, "Claude failed"),
            _ => {}
        }
    }

    fn on_caught_up(&mut self) {
        self.replay_done = true;
    }
}

pub(crate) fn start_tailing(
    path: &Path,
    events: Arc<dyn EventSink>,
    session_id: String,
) -> Result<(Arc<AtomicBool>, std::thread::JoinHandle<()>), String> {
    let file = File::open(path).map_err(|error| {
        format!(
            "failed to open Claude hook stream {}: {error}",
            path.display()
        )
    })?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let decoder = ClaudeHookDecoder::new(events, session_id);
    let service = TranscriptTailService::new(Box::new(decoder), "Claude hook stream");
    let join = std::thread::spawn(move || service.run(BufReader::new(file), thread_stop));
    Ok((stop, join))
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn truncate(value: &str, max_len: usize) -> String {
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .find(|index| *index >= max_len)
        .unwrap_or(value.len());
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::FakeEventSink;

    #[test]
    fn semantic_hooks_are_exact_and_live_only() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut decoder = ClaudeHookDecoder::new(events, "pty-1".to_string());

        decoder.decode_line(r#"{"hook_event_name":"PermissionRequest","tool_use_id":"replayed"}"#);
        decoder.on_caught_up();
        assert_eq!(sink.count("agent-attention"), 0);

        decoder.decode_line(r#"{"hook_event_name":"PermissionRequest","tool_use_id":"tool-1"}"#);
        decoder.decode_line(
            r#"{"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"tool-2"}"#,
        );
        decoder.decode_line(
            r#"{"hook_event_name":"PreToolUse","tool_name":"Read","tool_use_id":"tool-3"}"#,
        );
        decoder.decode_line(
            r#"{"hook_event_name":"StopFailure","error":{"message":"request failed"}}"#,
        );

        let attention: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-attention")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(attention.len(), 3);
        assert_eq!(attention[0]["reason"], "approval-requested");
        assert_eq!(attention[0]["dedupeKey"], "tool-1");
        assert_eq!(attention[1]["reason"], "question-requested");
        assert_eq!(attention[2]["reason"], "agent-error");
        assert_eq!(attention[2]["body"], "request failed");
    }

    #[test]
    fn semantic_hook_dedupe_key_is_bounded() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut decoder = ClaudeHookDecoder::new(events, "pty-1".to_string());
        decoder.on_caught_up();

        decoder.decode_line(&format!(
            r#"{{"hook_event_name":"PermissionRequest","tool_use_id":"{}"}}"#,
            "x".repeat(300)
        ));

        let attention = sink
            .recorded()
            .into_iter()
            .find(|(name, _)| name == "agent-attention")
            .expect("attention event")
            .1;
        assert_eq!(
            attention["dedupeKey"].as_str().expect("dedupe key").len(),
            MAX_ATTENTION_DEDUPE_LEN
        );
    }
}
