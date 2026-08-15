//! Transcript tailer for Codex rollout JSONL files.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::agent::adapter::base::{
    for_each_bounded_line, TranscriptDecoder, TranscriptHandle, TranscriptTailService,
};
use crate::agent::adapter::claude_code::test_runners::build::{maybe_build_snapshot, BuildArgs};
use crate::agent::adapter::claude_code::test_runners::emitter::TestRunEmitter;
use crate::agent::adapter::claude_code::test_runners::matcher::{match_command, MatchedCommand};
use crate::agent::adapter::claude_code::test_runners::test_file_patterns::is_test_file;
use crate::agent::adapter::claude_code::test_runners::timestamps::compute_duration_ms;
use crate::agent::adapter::claude_code::test_runners::types::CapturedOutput;
use crate::agent::adapter::types::ValidateTranscriptError;
use crate::agent::events::{
    emit_agent_cwd, emit_agent_replay_summary, emit_agent_reply, emit_agent_tool_call,
    emit_agent_turn, emit_lifecycle_on_change, record_attention, record_lifecycle,
    record_tool_call, ReplayActivity,
};
use crate::agent::reply::{extract_agent_reply, map_agent_reply_outcome};
use crate::agent::types::{
    AgentAttentionEvent, AgentAttentionReason, AgentCwdEvent, AgentPhase, AgentToolCallEvent,
    AgentTurnEvent, ToolCallStatus,
};
use crate::agent::types::{AgentReplyEvent, AgentReviewEvent};
use crate::runtime::EventSink;

use super::transcript_dto::{
    CodexCustomToolOutputDto, CodexExecArgsDto, CodexLineDto, CodexPayloadDto, CodexPayloadType,
    CodexRecordType,
};

// PR #302 (this branch) extracted the per-provider tail loop into the
// shared `TranscriptTailService` engine; `POLL_INTERVAL` is no longer
// referenced from this file (the engine owns the poll cadence).
// PR #303 (merged to main) widened `MAX_ARGS_LEN` from 100 to 1024 so
// the agent-status activity-detail card can show the full wrapped
// command/path. Both kept here.
const MAX_ARGS_LEN: usize = 1024;
const MAX_ATTENTION_DEDUPE_LEN: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompletionMode {
    Output,
    ExecCommandEnd,
    PatchApplyEnd,
}

struct InFlightToolCall {
    started_at: Instant,
    started_at_iso: String,
    tool: String,
    args: String,
    is_test_file: bool,
    test_match: Option<MatchedCommand>,
    completion_mode: CompletionMode,
}

type InFlightToolCalls = HashMap<String, InFlightToolCall>;

/// Pull session-start cwd off a Codex rollout JSONL line.
///
/// Returns `Some(cwd)` ONLY for `session_meta` entries — the
/// session-start anchor. `turn_context.cwd` is intentionally NOT
/// matched here: empirically it just repeats `session_meta.cwd`
/// every turn (no information value), and treating it as a live cwd
/// would cause false reverts on reasoning-only turns after an
/// execution-tool workdir transition has already moved us to a new directory.
/// See spec section 1.
fn extract_session_cwd(record_type: CodexRecordType, payload: &CodexPayloadDto) -> Option<String> {
    if !matches!(record_type, CodexRecordType::SessionMeta) {
        return None;
    }
    payload.cwd.clone().filter(|s| !s.is_empty())
}

fn decode_javascript_string(literal: &str) -> Option<String> {
    let bytes = literal.as_bytes();
    let quote = *bytes.first()?;
    if bytes.len() < 2 || bytes.last() != Some(&quote) {
        return None;
    }
    if quote == b'"' {
        return serde_json::from_str(literal).ok();
    }
    let inner = &literal[1..literal.len() - 1];
    match quote {
        b'\'' => decode_javascript_escaped_string(inner, b'\''),
        b'`' if !inner.contains("${") => decode_javascript_escaped_string(inner, b'`'),
        _ => None,
    }
}

fn decode_javascript_escaped_string(inner: &str, quote: u8) -> Option<String> {
    let quote = char::from(quote);
    let mut output = String::new();
    let mut chars = inner.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }

        let escaped = chars.next()?;
        match escaped {
            ch if ch == quote => output.push(ch),
            '\\' => output.push('\\'),
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            'b' => output.push('\u{0008}'),
            'f' => output.push('\u{000c}'),
            'v' => output.push('\u{000b}'),
            '0' => output.push('\0'),
            'x' => {
                let value = consume_hex_escape(&mut chars, 2)?;
                output.push(char::from_u32(value)?);
            }
            'u' => {
                let value = consume_unicode_escape(&mut chars)?;
                output.push(char::from_u32(value)?);
            }
            other => output.push(other),
        }
    }

    Some(output)
}

fn consume_hex_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    digits: usize,
) -> Option<u32> {
    let mut value = 0;
    for _ in 0..digits {
        value = value * 16 + chars.next()?.to_digit(16)?;
    }
    Some(value)
}

fn consume_unicode_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<u32> {
    if chars.peek() == Some(&'{') {
        chars.next();
        return consume_braced_unicode_escape(chars);
    }

    let value = consume_hex_escape(chars, 4)?;
    if !(0xd800..=0xdfff).contains(&value) {
        return Some(value);
    }

    if !(0xd800..=0xdbff).contains(&value) {
        return None;
    }

    if chars.next()? != '\\' || chars.next()? != 'u' || chars.peek() == Some(&'{') {
        return None;
    }

    let low = consume_hex_escape(chars, 4)?;
    if !(0xdc00..=0xdfff).contains(&low) {
        return None;
    }

    Some(0x10000 + ((value - 0xd800) << 10) + (low - 0xdc00))
}

fn consume_braced_unicode_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Option<u32> {
    let mut value = 0;
    let mut digits = 0;
    loop {
        let ch = chars.next()?;
        if ch == '}' {
            return (digits > 0 && value <= 0x10ffff).then_some(value);
        }
        value = value * 16 + ch.to_digit(16)?;
        digits += 1;
        if digits > 6 {
            return None;
        }
    }
}

enum JavascriptToken<'a> {
    Identifier(&'a str),
    String(&'a str, usize),
    Punctuation(u8),
}

fn javascript_string_end(bytes: &[u8], opening: usize) -> Option<usize> {
    let quote = *bytes.get(opening)?;
    let mut index = opening + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index += 2;
        } else if bytes[index] == quote {
            return Some(index + 1);
        } else {
            index += 1;
        }
    }
    None
}

fn has_javascript_line_terminator(input: &str, mut index: usize) -> bool {
    let bytes = input.as_bytes();
    while index < bytes.len() {
        match bytes[index] {
            b'\n' | b'\r' => return true,
            b' ' | b'\t' | 0x0b | 0x0c => index += 1,
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    if matches!(bytes[index], b'\n' | b'\r') {
                        return true;
                    }
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            _ => return false,
        }
    }
    false
}

fn tokenize_javascript(input: &str) -> Vec<JavascriptToken<'_>> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            byte if byte.is_ascii_whitespace() => index += 1,
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            b'\'' | b'"' | b'`' => {
                let Some(end) = javascript_string_end(bytes, index) else {
                    break;
                };
                tokens.push(JavascriptToken::String(&input[index..end], end));
                index = end;
            }
            byte if byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$') => {
                let start = index;
                index += 1;
                while bytes
                    .get(index)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
                {
                    index += 1;
                }
                tokens.push(JavascriptToken::Identifier(&input[start..index]));
            }
            byte => {
                tokens.push(JavascriptToken::Punctuation(byte));
                index += 1;
            }
        }
    }

    tokens
}

fn single_code_mode_tool(input: &str) -> Option<String> {
    let tokens = tokenize_javascript(input);
    let mut tool = None;

    for index in 0..tokens.len() {
        let is_tool_call = identifier_at(&tokens, index) == Some("tools")
            && is_punctuation(&tokens, index + 1, b'.')
            && identifier_at(&tokens, index + 2).is_some()
            && is_punctuation(&tokens, index + 3, b'(');
        if !is_tool_call {
            continue;
        }

        let candidate = identifier_at(&tokens, index + 2)?;
        match tool {
            Some(current) if current != candidate => return None,
            Some(_) => {}
            None => tool = Some(candidate),
        }
    }

    tool.map(str::to_string)
}

fn tool_call_name(payload: &CodexPayloadDto) -> String {
    let name = payload.name.as_deref().unwrap_or("unknown");
    if name == "exec" {
        if let Some(tool) = payload.input.as_deref().and_then(single_code_mode_tool) {
            return tool;
        }
    }

    match payload
        .namespace
        .as_deref()
        .filter(|namespace| !namespace.is_empty())
    {
        Some(namespace) => format!(
            "{}__{}",
            namespace.trim_end_matches('_'),
            name.trim_start_matches('_')
        ),
        None => name.to_string(),
    }
}

fn identifier_at<'a>(tokens: &[JavascriptToken<'a>], index: usize) -> Option<&'a str> {
    match tokens.get(index) {
        Some(JavascriptToken::Identifier(identifier)) => Some(*identifier),
        _ => None,
    }
}

fn is_punctuation(tokens: &[JavascriptToken<'_>], index: usize, expected: u8) -> bool {
    matches!(tokens.get(index), Some(JavascriptToken::Punctuation(actual)) if *actual == expected)
}

fn find_object_end(tokens: &[JavascriptToken<'_>], opening: usize) -> Option<usize> {
    let mut depth = 0_u32;
    for (index, token) in tokens.iter().enumerate().skip(opening) {
        match token {
            JavascriptToken::Punctuation(b'{') => depth = depth.saturating_add(1),
            JavascriptToken::Punctuation(b'}') => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn property_name_matches(token: &JavascriptToken<'_>, property_names: &[&str]) -> bool {
    match token {
        JavascriptToken::Identifier(name) => property_names.contains(name),
        JavascriptToken::String(key, _) => decode_javascript_string(key)
            .as_deref()
            .is_some_and(|name| property_names.contains(&name)),
        JavascriptToken::Punctuation(_) => false,
    }
}

fn extract_object_string_property(
    tokens: &[JavascriptToken<'_>],
    opening: usize,
    closing: usize,
    bindings: &HashMap<&str, String>,
    property_names: &[&str],
) -> Option<String> {
    let mut stack = Vec::new();
    let mut value = None;

    for index in opening + 1..closing {
        if stack.is_empty() {
            let is_property_key = index == opening + 1 || is_punctuation(tokens, index - 1, b',');
            let is_target_key = is_property_key
                && tokens
                    .get(index)
                    .is_some_and(|token| property_name_matches(token, property_names));
            if is_target_key {
                if is_punctuation(tokens, index + 1, b':') {
                    let value_index = index + 2;
                    let value_is_complete =
                        value_index + 1 == closing || is_punctuation(tokens, value_index + 1, b',');
                    if value_is_complete {
                        value = match tokens.get(value_index) {
                            Some(JavascriptToken::String(value, _)) => {
                                decode_javascript_string(value)
                            }
                            Some(JavascriptToken::Identifier(name)) => bindings.get(name).cloned(),
                            _ => None,
                        };
                    }
                } else if matches!(tokens.get(index), Some(JavascriptToken::Identifier(_)))
                    && (index + 1 == closing || is_punctuation(tokens, index + 1, b','))
                {
                    value =
                        identifier_at(tokens, index).and_then(|name| bindings.get(name).cloned());
                }
            }
        }

        match tokens.get(index) {
            Some(JavascriptToken::Punctuation(open @ (b'(' | b'[' | b'{'))) => stack.push(*open),
            Some(JavascriptToken::Punctuation(b')')) if stack.pop() != Some(b'(') => return None,
            Some(JavascriptToken::Punctuation(b']')) if stack.pop() != Some(b'[') => return None,
            Some(JavascriptToken::Punctuation(b'}')) if stack.pop() != Some(b'{') => return None,
            _ => {}
        }
    }

    stack.is_empty().then_some(value).flatten()
}

fn extract_object_workdir(
    tokens: &[JavascriptToken<'_>],
    opening: usize,
    closing: usize,
    bindings: &HashMap<&str, String>,
) -> Option<String> {
    extract_object_string_property(tokens, opening, closing, bindings, &["workdir"])
}

fn record_const_string_binding<'a>(
    input: &str,
    tokens: &[JavascriptToken<'a>],
    index: usize,
    bindings: &mut HashMap<&'a str, String>,
) {
    if identifier_at(tokens, index) != Some("const") {
        return;
    }

    let (Some(name), Some(JavascriptToken::String(value, value_end))) =
        (identifier_at(tokens, index + 1), tokens.get(index + 3))
    else {
        return;
    };

    let complete = is_punctuation(tokens, index + 2, b'=')
        && (index + 4 == tokens.len()
            || is_punctuation(tokens, index + 4, b';')
            || is_punctuation(tokens, index + 4, b',')
            || (identifier_at(tokens, index + 4).is_some()
                && has_javascript_line_terminator(input, *value_end)));
    if complete {
        if let Some(value) = decode_javascript_string(value) {
            bindings.insert(name, value);
        }
    }
}

fn extract_custom_exec_workdir(input: &str) -> Option<String> {
    // Parse only literal object arguments and simple const aliases;
    // computed JavaScript stays unresolved until Codex exposes nested metadata.
    let tokens = tokenize_javascript(input);
    let mut bindings = HashMap::new();
    let mut last_workdir = None;
    let mut index = 0;

    while index < tokens.len() {
        record_const_string_binding(input, &tokens, index, &mut bindings);

        let is_exec_call = identifier_at(&tokens, index) == Some("tools")
            && is_punctuation(&tokens, index + 1, b'.')
            && identifier_at(&tokens, index + 2) == Some("exec_command")
            && is_punctuation(&tokens, index + 3, b'(')
            && is_punctuation(&tokens, index + 4, b'{');
        if is_exec_call {
            let opening = index + 4;
            if let Some(closing) = find_object_end(&tokens, opening) {
                if let Some(candidate) =
                    extract_object_workdir(&tokens, opening, closing, &bindings)
                        .filter(|candidate| !candidate.is_empty())
                {
                    last_workdir = Some(candidate);
                }
                index = closing;
            }
        }
        index += 1;
    }

    last_workdir
}

fn extract_custom_exec_command_args(input: &str) -> Option<String> {
    let tokens = tokenize_javascript(input);
    let mut bindings = HashMap::new();
    let mut command = None;
    let mut index = 0;

    while index < tokens.len() {
        record_const_string_binding(input, &tokens, index, &mut bindings);

        let is_exec_call = identifier_at(&tokens, index) == Some("tools")
            && is_punctuation(&tokens, index + 1, b'.')
            && identifier_at(&tokens, index + 2) == Some("exec_command")
            && is_punctuation(&tokens, index + 3, b'(')
            && is_punctuation(&tokens, index + 4, b'{');
        if is_exec_call {
            let opening = index + 4;
            if let Some(closing) = find_object_end(&tokens, opening) {
                if let Some(candidate) = extract_object_string_property(
                    &tokens,
                    opening,
                    closing,
                    &bindings,
                    &["cmd", "command"],
                )
                .filter(|candidate| !candidate.is_empty())
                {
                    command = Some(candidate);
                }
                index = closing;
            }
        }
        index += 1;
    }

    command
}

fn extract_custom_apply_patch_input(input: &str) -> Option<String> {
    let tokens = tokenize_javascript(input);
    let mut bindings = HashMap::new();
    let mut patch = None;
    let mut index = 0;

    while index < tokens.len() {
        record_const_string_binding(input, &tokens, index, &mut bindings);

        let is_patch_call = identifier_at(&tokens, index) == Some("tools")
            && is_punctuation(&tokens, index + 1, b'.')
            && identifier_at(&tokens, index + 2) == Some("apply_patch")
            && is_punctuation(&tokens, index + 3, b'(');
        if is_patch_call {
            patch = match tokens.get(index + 4) {
                Some(JavascriptToken::String(value, _)) => decode_javascript_string(value),
                Some(JavascriptToken::Identifier(name)) => bindings.get(name).cloned(),
                _ => None,
            }
            .filter(|candidate| !candidate.is_empty());
        }
        index += 1;
    }

    patch
}

/// Pull the mid-session workdir off either Codex execution-call schema.
fn extract_exec_workdir(record_type: CodexRecordType, payload: &CodexPayloadDto) -> Option<String> {
    if !matches!(record_type, CodexRecordType::ResponseItem) {
        return None;
    }

    match (payload.payload_type(), payload.name.as_deref()) {
        (CodexPayloadType::FunctionCall, Some("exec_command")) => {
            let raw = payload.arguments.as_deref()?;
            let args: CodexExecArgsDto = serde_json::from_str(raw).ok()?;
            args.workdir.filter(|workdir| !workdir.is_empty())
        }
        (CodexPayloadType::CustomToolCall, Some("exec")) => {
            extract_custom_exec_workdir(payload.input.as_deref()?)
        }
        _ => None,
    }
}

/// Dispatcher returning the observed cwd from whichever source carries it.
fn extract_codex_cwd(record_type: CodexRecordType, payload: &CodexPayloadDto) -> Option<String> {
    if let Some(cwd) = extract_session_cwd(record_type, payload) {
        return Some(cwd);
    }
    extract_exec_workdir(record_type, payload)
}

pub(super) fn validate_transcript_path(
    transcript_path: &str,
    codex_root: &Path,
) -> Result<PathBuf, ValidateTranscriptError> {
    if transcript_path.bytes().any(|b| b == 0) {
        return Err(ValidateTranscriptError::InvalidPath(
            "transcript path contains null byte".to_string(),
        ));
    }

    validate_transcript_path_under_root(transcript_path, codex_root)
}

fn validate_transcript_path_under_root(
    transcript_path: &str,
    codex_root: &Path,
) -> Result<PathBuf, ValidateTranscriptError> {
    let path = PathBuf::from(transcript_path);
    let canonical = fs::canonicalize(&path).map_err(|e| {
        let kind = e.kind();
        if kind == io::ErrorKind::NotFound {
            return ValidateTranscriptError::NotFound(path.clone());
        }
        if kind == io::ErrorKind::PermissionDenied {
            if let Ok(false) = path.try_exists() {
                return ValidateTranscriptError::NotFound(path.clone());
            }
        }
        ValidateTranscriptError::Other(format!(
            "invalid transcript path '{}': {}",
            transcript_path, e
        ))
    })?;

    if !canonical.is_file() {
        return Err(ValidateTranscriptError::NotAFile(canonical));
    }

    let codex_root = fs::canonicalize(codex_root).map_err(|e| {
        ValidateTranscriptError::Other(format!(
            "cannot resolve Codex transcript root '{}': {}",
            codex_root.display(),
            e
        ))
    })?;

    if !canonical.starts_with(&codex_root) {
        return Err(ValidateTranscriptError::OutsideRoot {
            path: canonical,
            root: codex_root,
        });
    }

    Ok(canonical)
}

pub(super) fn start_tailing(
    events: Arc<dyn EventSink>,
    session_id: String,
    transcript_path: PathBuf,
    cwd: Option<PathBuf>,
) -> Result<TranscriptHandle, String> {
    let file = File::open(&transcript_path).map_err(|e| {
        format!(
            "Failed to open Codex rollout transcript: {}: {}",
            transcript_path.display(),
            e
        )
    })?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = stop_flag.clone();

    let decoder = CodexTranscriptDecoder::new(events, session_id, cwd);
    let service = TranscriptTailService::new(Box::new(decoder), "Codex rollout transcript");

    let join_handle = std::thread::spawn(move || {
        service.run(BufReader::new(file), stop_clone);
    });

    Ok(TranscriptHandle::new(stop_flag, join_handle))
}

pub(super) fn recover_replies(
    transcript_path: &Path,
    session_id: &str,
    nonces: &HashSet<String>,
) -> Result<Vec<AgentReplyEvent>, String> {
    let file = File::open(transcript_path).map_err(|e| {
        format!(
            "Failed to open Codex rollout transcript for recovery: {}: {}",
            transcript_path.display(),
            e
        )
    })?;
    let mut recovered = Vec::new();
    let mut pending_nonces = nonces.clone();

    for_each_bounded_line(BufReader::new(file), "Codex transcript recovery", |line| {
        if pending_nonces.is_empty() {
            return;
        }
        let Ok(dto) = serde_json::from_str::<CodexLineDto>(line) else {
            return;
        };
        if !matches!(dto.record_type(), CodexRecordType::EventMsg) {
            return;
        }
        let payload = serde_json::from_value::<CodexPayloadDto>(dto.payload).unwrap_or_default();
        if !matches!(payload.payload_type(), CodexPayloadType::TaskComplete) {
            return;
        }
        if let Some(event) = reply_event(&payload, session_id) {
            let Some(nonce) = event.nonce.as_ref() else {
                return;
            };
            if pending_nonces.remove(nonce) {
                recovered.push(event);
            }
        }
    })
    .map_err(|e| {
        format!(
            "Failed to read Codex rollout transcript for recovery: {}: {}",
            transcript_path.display(),
            e
        )
    })?;

    Ok(recovered)
}

pub(super) fn recover_reviews(
    transcript_path: &Path,
    session_id: &str,
    nonces: &HashSet<String>,
) -> Result<Vec<AgentReviewEvent>, String> {
    let file = File::open(transcript_path).map_err(|e| {
        format!(
            "Failed to open Codex rollout transcript for review recovery: {}: {}",
            transcript_path.display(),
            e
        )
    })?;
    let mut recovered = Vec::new();
    let mut pending_nonces = nonces.clone();

    for_each_bounded_line(BufReader::new(file), "Codex review recovery", |line| {
        if pending_nonces.is_empty() {
            return;
        }
        let Ok(dto) = serde_json::from_str::<CodexLineDto>(line) else {
            return;
        };
        if !matches!(dto.record_type(), CodexRecordType::EventMsg) {
            return;
        }
        let payload = serde_json::from_value::<CodexPayloadDto>(dto.payload).unwrap_or_default();
        if !matches!(payload.payload_type(), CodexPayloadType::TaskComplete) {
            return;
        }
        if let Some(event) = review_event(&payload, session_id) {
            let Some(nonce) = event.nonce.as_ref() else {
                return;
            };
            if pending_nonces.remove(nonce) {
                recovered.push(event);
            }
        }
    })
    .map_err(|e| {
        format!(
            "Failed to read Codex rollout transcript for review recovery: {}: {}",
            transcript_path.display(),
            e
        )
    })?;

    Ok(recovered)
}

/// Per-session Codex decoder: owns the in-flight tool-call map (each entry
/// carrying its `CompletionMode`), turn count, last-seen cwd, and the
/// replay-aware emitter, and turns each complete rollout line into `agent-*`
/// events. Driven by [`TranscriptTailService`], which owns the
/// read/buffer/poll loop (including the partial-line buffering Codex's old
/// `tail_loop` did inline).
struct CodexTranscriptDecoder {
    events: Arc<dyn EventSink>,
    session_id: String,
    cwd: Option<PathBuf>,
    in_flight: InFlightToolCalls,
    num_turns: u32,
    last_cwd: Option<String>,
    emitter: TestRunEmitter,
    /// Codex's own session id (from session_meta), the agent-lifecycle identity.
    codex_agent_session_id: String,
    /// Live agent-lifecycle de-dup slot; left untouched during replay.
    last_phase: Option<AgentPhase>,
    /// Settled phase accumulated silently during replay, flushed once at
    /// the replay->live boundary.
    replay_phase: Option<AgentPhase>,
    /// Tool-call/turn/cwd activity accumulated silently during replay; flushed
    /// once at the replay->live boundary as a single agent-replay-summary.
    replay_activity: ReplayActivity,
    /// One-shot guard: false during replay, true after the first on_caught_up.
    replay_done: bool,
}

impl CodexTranscriptDecoder {
    fn new(events: Arc<dyn EventSink>, session_id: String, cwd: Option<PathBuf>) -> Self {
        let emitter = TestRunEmitter::new(events.clone());
        Self {
            events,
            session_id,
            cwd,
            in_flight: HashMap::new(),
            num_turns: 0,
            last_cwd: None,
            emitter,
            codex_agent_session_id: String::new(),
            last_phase: None,
            replay_phase: None,
            replay_activity: ReplayActivity::default(),
            replay_done: false,
        }
    }
}

impl TranscriptDecoder for CodexTranscriptDecoder {
    fn decode_line(&mut self, line: &str) {
        process_line(
            line,
            &self.session_id,
            self.cwd.as_deref(),
            &self.events,
            &mut self.emitter,
            &mut self.in_flight,
            &mut self.num_turns,
            &mut self.last_cwd,
            &mut self.codex_agent_session_id,
            &mut self.last_phase,
            &mut self.replay_phase,
            &mut self.replay_activity,
            self.replay_done,
        );
    }

    /// First EOF marks the end of replay; subsequent EOFs are idempotent.
    fn on_caught_up(&mut self) {
        if !self.replay_done {
            self.replay_done = true;
            if !self.codex_agent_session_id.is_empty() {
                if let Some(phase) = self.replay_phase.take() {
                    emit_lifecycle_on_change(
                        self.events.as_ref(),
                        &self.session_id,
                        &self.codex_agent_session_id,
                        &mut self.last_phase,
                        phase,
                    );
                }
            }
            for event in self.replay_activity.take_running() {
                if let Err(e) = emit_agent_tool_call(self.events.as_ref(), &event) {
                    log::warn!("Failed to emit agent-tool-call event: {}", e);
                }
            }
            // Flush the replay-accumulated tool-call/turn/cwd activity as one
            // summary, replacing the thousands of per-line events suppressed
            // during replay. Only emit if it carries something, to avoid a
            // noisy empty summary for a fresh session.
            let summary = std::mem::take(&mut self.replay_activity).into_summary(
                self.session_id.clone(),
                self.num_turns,
                self.last_cwd.clone(),
            );
            if summary.tool_call_total > 0 || summary.num_turns > 0 || summary.cwd.is_some() {
                if let Err(e) = emit_agent_replay_summary(self.events.as_ref(), &summary) {
                    log::warn!("Failed to emit agent-replay-summary event: {}", e);
                }
            }
        }
        self.emitter.finish_replay();
    }
}

#[allow(clippy::too_many_arguments)]
fn process_line(
    line: &str,
    session_id: &str,
    cwd: Option<&Path>,
    events: &Arc<dyn EventSink>,
    emitter: &mut TestRunEmitter,
    in_flight: &mut InFlightToolCalls,
    num_turns: &mut u32,
    last_cwd: &mut Option<String>,
    codex_agent_session_id: &mut String,
    last_phase: &mut Option<AgentPhase>,
    replay_phase: &mut Option<AgentPhase>,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let dto: CodexLineDto = match serde_json::from_str(line) {
        Ok(dto) => dto,
        Err(_) => return,
    };
    let record_type = dto.record_type();

    // Unknown record types carry nothing we consume — skip the payload parse
    // entirely (pre-C never parsed the payload for them either).
    if matches!(record_type, CodexRecordType::Other) {
        return;
    }

    // Parse the payload ONCE per line; the cwd transition and the dispatch
    // share it. Pre-C, response_item lines parsed this twice — extract_exec_
    // workdir for the cwd, then process_response_item for the dispatch.
    let payload =
        serde_json::from_value::<CodexPayloadDto>(dto.payload.clone()).unwrap_or_default();

    // Capture Codex's own session id (session_meta) as the lifecycle identity,
    // then derive a phase from the event_msg turn boundary; replay-bounded.
    if matches!(record_type, CodexRecordType::SessionMeta) {
        if let Some(id) = payload.id.as_deref().filter(|id| !id.is_empty()) {
            if !codex_agent_session_id.is_empty() && codex_agent_session_id != id {
                in_flight.clear();
                *num_turns = 0;
                *last_cwd = None;
                *last_phase = None;
                *replay_phase = None;
                *replay_activity = ReplayActivity::default();
                emitter.clear_pending();
            }
            *codex_agent_session_id = id.to_string();
        }
    }
    if matches!(record_type, CodexRecordType::EventMsg) {
        let phase = match payload.payload_type() {
            CodexPayloadType::TaskStarted | CodexPayloadType::UserMessage => {
                Some(AgentPhase::Running)
            }
            CodexPayloadType::TaskComplete | CodexPayloadType::TurnAborted => {
                Some(AgentPhase::Idle)
            }
            _ => None,
        };
        if let Some(phase) = phase {
            if !codex_agent_session_id.is_empty() {
                record_lifecycle(
                    phase,
                    session_id,
                    codex_agent_session_id,
                    events,
                    last_phase,
                    replay_phase,
                    replay_done,
                );
            }
        }
    }

    // Emit agent-cwd on transitions only. Codex's sources are session_meta cwd
    // (session start) and the legacy or unified execution-call workdir
    // (mid-session). turn_context.cwd is intentionally NOT a source — see spec
    // section 1 and the regression test
    // `process_line_turn_context_after_exec_command_does_not_revert`.
    if let Some(observed) = extract_codex_cwd(record_type, &payload) {
        if last_cwd
            .as_deref()
            .map_or(true, |seen| seen != observed.as_str())
        {
            // During replay, suppress the per-line cwd event; last_cwd still
            // accumulates so the summary carries the final cwd at the boundary.
            if replay_done {
                let event = AgentCwdEvent {
                    session_id: session_id.to_string(),
                    cwd: observed.clone(),
                };
                if let Err(e) = emit_agent_cwd(events.as_ref(), &event) {
                    log::warn!("Failed to emit agent-cwd event: {}", e);
                }
            }
            *last_cwd = Some(observed);
        }
    }

    match record_type {
        CodexRecordType::ResponseItem => {
            process_response_item(
                &dto,
                &payload,
                session_id,
                cwd,
                events,
                in_flight,
                replay_activity,
                replay_done,
            );
        }
        CodexRecordType::EventMsg => {
            process_event_msg(
                &dto,
                &payload,
                session_id,
                cwd,
                events,
                emitter,
                in_flight,
                num_turns,
                replay_activity,
                replay_done,
            );
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn process_response_item(
    dto: &CodexLineDto,
    payload: &CodexPayloadDto,
    session_id: &str,
    cwd: Option<&Path>,
    events: &Arc<dyn EventSink>,
    in_flight: &mut InFlightToolCalls,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let timestamp = dto.timestamp.clone().unwrap_or_else(now_iso8601);

    match payload.payload_type() {
        CodexPayloadType::FunctionCall => {
            if payload.name.as_deref() == Some("request_user_input") {
                emit_attention(
                    payload,
                    session_id,
                    events,
                    AgentAttentionReason::QuestionRequested,
                    "Codex has a question",
                    replay_done,
                );
            } else if payload.name.as_deref() == Some("request_permissions") {
                emit_attention(
                    payload,
                    session_id,
                    events,
                    AgentAttentionReason::ApprovalRequested,
                    "Codex needs approval",
                    replay_done,
                );
            }
            start_function_call(
                payload,
                session_id,
                cwd,
                events,
                in_flight,
                &timestamp,
                replay_activity,
                replay_done,
            );
        }
        CodexPayloadType::CustomToolCall => {
            start_custom_tool_call(
                payload,
                session_id,
                events,
                in_flight,
                &timestamp,
                replay_activity,
                replay_done,
            );
        }
        CodexPayloadType::FunctionCallOutput => {
            process_output_completion(
                payload,
                session_id,
                events,
                in_flight,
                &timestamp,
                false,
                replay_activity,
                replay_done,
            );
        }
        CodexPayloadType::CustomToolCallOutput => {
            process_output_completion(
                payload,
                session_id,
                events,
                in_flight,
                &timestamp,
                true,
                replay_activity,
                replay_done,
            );
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn process_event_msg(
    dto: &CodexLineDto,
    payload: &CodexPayloadDto,
    session_id: &str,
    cwd: Option<&Path>,
    events: &Arc<dyn EventSink>,
    emitter: &mut TestRunEmitter,
    in_flight: &mut InFlightToolCalls,
    num_turns: &mut u32,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let timestamp = dto.timestamp.clone().unwrap_or_else(now_iso8601);

    match payload.payload_type() {
        CodexPayloadType::UserMessage => {
            process_user_message(payload, session_id, events, num_turns, replay_done);
        }
        CodexPayloadType::ExecCommandEnd => {
            process_exec_command_end(
                payload,
                session_id,
                cwd,
                events,
                emitter,
                in_flight,
                &timestamp,
                replay_activity,
                replay_done,
            );
        }
        CodexPayloadType::PatchApplyEnd => {
            process_patch_apply_end(
                payload,
                session_id,
                events,
                in_flight,
                &timestamp,
                replay_activity,
                replay_done,
            );
        }
        CodexPayloadType::TaskComplete => {
            flush_in_flight_tool_calls(
                session_id,
                events,
                in_flight,
                ToolCallStatus::Done,
                &timestamp,
                replay_activity,
                replay_done,
            );
            emit_reply_if_present(payload, session_id, events, replay_done);
            emit_review_if_present(payload, session_id, events, replay_done);
        }
        CodexPayloadType::TurnAborted => flush_in_flight_tool_calls(
            session_id,
            events,
            in_flight,
            ToolCallStatus::Failed,
            &timestamp,
            replay_activity,
            replay_done,
        ),
        CodexPayloadType::ExecApprovalRequest
        | CodexPayloadType::ApplyPatchApprovalRequest
        | CodexPayloadType::RequestPermissions => emit_attention(
            payload,
            session_id,
            events,
            AgentAttentionReason::ApprovalRequested,
            "Codex needs approval",
            replay_done,
        ),
        CodexPayloadType::ElicitationRequest => emit_attention(
            payload,
            session_id,
            events,
            AgentAttentionReason::QuestionRequested,
            "Codex has a question",
            replay_done,
        ),
        _ => {}
    }
}

fn emit_attention(
    payload: &CodexPayloadDto,
    session_id: &str,
    events: &Arc<dyn EventSink>,
    reason: AgentAttentionReason,
    title: &str,
    replay_done: bool,
) {
    record_attention(
        events,
        AgentAttentionEvent {
            pty_id: session_id.to_string(),
            reason,
            title: title.to_string(),
            body: None,
            occurred_at: now_epoch_ms(),
            dedupe_key: payload
                .approval_id
                .as_deref()
                .or(payload.request_id.as_deref())
                .or(payload.call_id.as_deref())
                .map(|key| truncate_string(key, MAX_ATTENTION_DEDUPE_LEN)),
        },
        replay_done,
    );
}

/// If the completed reply on a `task_complete` carries the VIM-304 review
/// sentinel block, extract it and emit `agent-review`. Same `replay_done` gate
/// as the reply path. No sentinel → no event.
fn emit_review_if_present(
    payload: &CodexPayloadDto,
    session_id: &str,
    events: &Arc<dyn EventSink>,
    replay_done: bool,
) {
    if !replay_done {
        return;
    }

    let Some(event) = review_event(payload, session_id) else {
        return;
    };

    if let Err(e) = crate::agent::events::emit_agent_review(events.as_ref(), &event) {
        log::warn!("Failed to emit agent-review event: {}", e);
    }
}

/// If the completed reply on a `task_complete` carries the VIM-283 sentinel
/// block, extract it and emit `agent-reply`. No sentinel → no event.
fn emit_reply_if_present(
    payload: &CodexPayloadDto,
    session_id: &str,
    events: &Arc<dyn EventSink>,
    replay_done: bool,
) {
    if !replay_done {
        return;
    }

    let Some(event) = reply_event(payload, session_id) else {
        return;
    };

    if let Err(e) = emit_agent_reply(events.as_ref(), &event) {
        log::warn!("Failed to emit agent-reply event: {}", e);
    }
}

fn reply_event(payload: &CodexPayloadDto, session_id: &str) -> Option<AgentReplyEvent> {
    payload
        .last_agent_message
        .as_deref()
        .and_then(extract_agent_reply)
        .map(|outcome| map_agent_reply_outcome(session_id, outcome))
}

fn review_event(payload: &CodexPayloadDto, session_id: &str) -> Option<AgentReviewEvent> {
    payload
        .last_agent_message
        .as_deref()
        .and_then(crate::agent::review::extract_agent_review)
        .map(|outcome| crate::agent::review::map_review_outcome(session_id, outcome))
}

fn process_user_message(
    payload: &CodexPayloadDto,
    session_id: &str,
    events: &Arc<dyn EventSink>,
    num_turns: &mut u32,
    replay_done: bool,
) {
    let Some(message) = payload.message.as_deref() else {
        return;
    };
    if message.trim().is_empty() {
        return;
    }

    *num_turns = num_turns.saturating_add(1);

    // During replay, suppress the per-turn event; num_turns still accumulates
    // so the boundary summary carries the final count. Emit live only after
    // the replay→live boundary.
    if replay_done {
        let event = AgentTurnEvent {
            session_id: session_id.to_string(),
            num_turns: *num_turns,
        };

        if let Err(e) = emit_agent_turn(events.as_ref(), &event) {
            log::warn!("Failed to emit agent-turn event: {}", e);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_function_call(
    payload: &CodexPayloadDto,
    session_id: &str,
    cwd: Option<&Path>,
    events: &Arc<dyn EventSink>,
    in_flight: &mut InFlightToolCalls,
    timestamp: &str,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let Some(call_id) = payload.call_id.as_deref() else {
        return;
    };
    let tool = tool_call_name(payload);
    let args = summarize_function_call_args(payload.arguments.as_deref());
    let cmd = function_call_cmd(payload.arguments.as_deref());
    let test_match = cmd
        .as_deref()
        .filter(|_| tool == "exec_command")
        .and_then(|cmd| match_command(cmd, cwd));

    in_flight.insert(
        call_id.to_string(),
        InFlightToolCall {
            started_at: Instant::now(),
            started_at_iso: timestamp.to_string(),
            tool: tool.clone(),
            args: args.clone(),
            is_test_file: false,
            test_match,
            completion_mode: if tool == "exec_command" {
                CompletionMode::ExecCommandEnd
            } else {
                CompletionMode::Output
            },
        },
    );

    emit_tool_call(
        events,
        AgentToolCallEvent {
            session_id: session_id.to_string(),
            tool_use_id: call_id.to_string(),
            tool,
            args,
            status: ToolCallStatus::Running,
            timestamp: timestamp.to_string(),
            duration_ms: 0,
            is_test_file: false,
        },
        replay_activity,
        replay_done,
    );
}

#[allow(clippy::too_many_arguments)]
fn start_custom_tool_call(
    payload: &CodexPayloadDto,
    session_id: &str,
    events: &Arc<dyn EventSink>,
    in_flight: &mut InFlightToolCalls,
    timestamp: &str,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let Some(call_id) = payload.call_id.as_deref() else {
        return;
    };
    let tool = tool_call_name(payload);
    let patch_paths = custom_tool_patch_paths(payload, &tool);
    let args = summarize_custom_tool_args(payload, &tool, &patch_paths);
    let is_test_file = custom_tool_is_test_file(&patch_paths);
    let completion_mode = if tool == "apply_patch" {
        CompletionMode::PatchApplyEnd
    } else if payload.name.as_deref() == Some("exec") && tool == "exec_command" {
        CompletionMode::ExecCommandEnd
    } else {
        CompletionMode::Output
    };

    in_flight.insert(
        call_id.to_string(),
        InFlightToolCall {
            started_at: Instant::now(),
            started_at_iso: timestamp.to_string(),
            tool: tool.clone(),
            args: args.clone(),
            is_test_file,
            test_match: None,
            completion_mode,
        },
    );

    emit_tool_call(
        events,
        AgentToolCallEvent {
            session_id: session_id.to_string(),
            tool_use_id: call_id.to_string(),
            tool,
            args,
            status: ToolCallStatus::Running,
            timestamp: timestamp.to_string(),
            duration_ms: 0,
            is_test_file,
        },
        replay_activity,
        replay_done,
    );
}

#[allow(clippy::too_many_arguments)]
fn process_output_completion(
    payload: &CodexPayloadDto,
    session_id: &str,
    events: &Arc<dyn EventSink>,
    in_flight: &mut InFlightToolCalls,
    timestamp: &str,
    is_custom_tool_output: bool,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let Some(call_id) = payload.call_id.as_deref() else {
        return;
    };

    // Preserve completion-mode guards: function_call_output and
    // custom_tool_call_output are not authoritative for exec_command or
    // apply_patch completion. Wait for the matching exec_command_end /
    // patch_apply_end event so failed commands/patches are not reported
    // as done.
    if let Some(call) = in_flight.get(call_id) {
        if matches!(
            call.completion_mode,
            CompletionMode::ExecCommandEnd | CompletionMode::PatchApplyEnd
        ) {
            return;
        }
    }

    let call = match in_flight.remove(call_id) {
        Some(call) => call,
        None => return,
    };

    let status = output_completion_status(payload, is_custom_tool_output);

    emit_tool_call(
        events,
        AgentToolCallEvent {
            session_id: session_id.to_string(),
            tool_use_id: call_id.to_string(),
            tool: call.tool,
            args: call.args,
            status,
            timestamp: timestamp.to_string(),
            duration_ms: compute_duration_ms(
                &call.started_at_iso,
                timestamp,
                call.started_at.elapsed(),
            ),
            is_test_file: call.is_test_file,
        },
        replay_activity,
        replay_done,
    );
}

fn output_completion_status(
    payload: &CodexPayloadDto,
    is_custom_tool_output: bool,
) -> ToolCallStatus {
    if is_custom_tool_output && custom_tool_output_failed(payload.output.as_deref()) {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Done
    }
}

#[allow(clippy::too_many_arguments)]
fn process_exec_command_end(
    payload: &CodexPayloadDto,
    session_id: &str,
    cwd: Option<&Path>,
    events: &Arc<dyn EventSink>,
    emitter: &mut TestRunEmitter,
    in_flight: &mut InFlightToolCalls,
    timestamp: &str,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let Some(call_id) = payload.call_id.as_deref() else {
        return;
    };

    let Some(call) = in_flight.remove(call_id) else {
        return;
    };

    if let Some(matched) = call.test_match {
        if let Some(cwd_ref) = cwd {
            let captured = CapturedOutput {
                content: payload
                    .aggregated_output
                    .as_deref()
                    .unwrap_or("")
                    .to_string(),
                is_error: payload.exit_code.is_some_and(|code| code != 0),
            };
            if let Some(snapshot) = maybe_build_snapshot(BuildArgs {
                session_id,
                matched: &matched,
                started_at: &call.started_at_iso,
                finished_at: timestamp,
                instant_fallback: call.started_at.elapsed(),
                captured,
                cwd: cwd_ref,
            }) {
                emitter.submit(snapshot);
            }
        } else {
            log::debug!(
                "Skipping Codex test-run snapshot for session {}: no workspace cwd resolved",
                session_id
            );
        }
    }

    let status = if payload.exit_code.is_some_and(|code| code != 0) {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Done
    };

    emit_tool_call(
        events,
        AgentToolCallEvent {
            session_id: session_id.to_string(),
            tool_use_id: call_id.to_string(),
            tool: call.tool,
            args: call.args,
            status,
            timestamp: timestamp.to_string(),
            duration_ms: exec_command_duration_ms(payload).unwrap_or_else(|| {
                compute_duration_ms(&call.started_at_iso, timestamp, call.started_at.elapsed())
            }),
            is_test_file: call.is_test_file,
        },
        replay_activity,
        replay_done,
    );
}

#[allow(clippy::too_many_arguments)]
fn process_patch_apply_end(
    payload: &CodexPayloadDto,
    session_id: &str,
    events: &Arc<dyn EventSink>,
    in_flight: &mut InFlightToolCalls,
    timestamp: &str,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    let Some(call_id) = payload.call_id.as_deref() else {
        return;
    };

    let Some(call) = in_flight.remove(call_id) else {
        return;
    };

    let status = if payload.success.unwrap_or(false) {
        ToolCallStatus::Done
    } else {
        ToolCallStatus::Failed
    };

    emit_tool_call(
        events,
        AgentToolCallEvent {
            session_id: session_id.to_string(),
            tool_use_id: call_id.to_string(),
            tool: call.tool,
            args: call.args,
            status,
            timestamp: timestamp.to_string(),
            duration_ms: compute_duration_ms(
                &call.started_at_iso,
                timestamp,
                call.started_at.elapsed(),
            ),
            is_test_file: call.is_test_file,
        },
        replay_activity,
        replay_done,
    );
}

fn flush_in_flight_tool_calls(
    session_id: &str,
    events: &Arc<dyn EventSink>,
    in_flight: &mut InFlightToolCalls,
    status: ToolCallStatus,
    timestamp: &str,
    replay_activity: &mut ReplayActivity,
    replay_done: bool,
) {
    for (call_id, call) in in_flight.drain() {
        emit_tool_call(
            events,
            AgentToolCallEvent {
                session_id: session_id.to_string(),
                tool_use_id: call_id,
                tool: call.tool,
                args: call.args,
                status: status.clone(),
                timestamp: timestamp.to_string(),
                duration_ms: compute_duration_ms(
                    &call.started_at_iso,
                    timestamp,
                    call.started_at.elapsed(),
                ),
                is_test_file: call.is_test_file,
            },
            replay_activity,
            replay_done,
        );
    }
}

/// Route a tool-call event through the replay-aware sink: live-emit once
/// replay is done, else fold into the accumulator for the boundary summary.
fn emit_tool_call(
    events: &Arc<dyn EventSink>,
    event: AgentToolCallEvent,
    replay: &mut ReplayActivity,
    replay_done: bool,
) {
    record_tool_call(events, event, replay, replay_done);
}

fn function_call_cmd(arguments: Option<&str>) -> Option<String> {
    let raw = arguments?;
    let args: CodexExecArgsDto = serde_json::from_str(raw).ok()?;
    args.cmd.or(args.command)
}

fn summarize_function_call_args(arguments: Option<&str>) -> String {
    if let Some(cmd) = function_call_cmd(arguments) {
        return truncate_string(&cmd, MAX_ARGS_LEN);
    }

    let raw = arguments.unwrap_or_default();

    if let Ok(args) = serde_json::from_str::<CodexExecArgsDto>(raw) {
        if let Some(path) = args.path.or(args.file_path) {
            return truncate_string(&path, MAX_ARGS_LEN);
        }
    }

    truncate_string(raw, MAX_ARGS_LEN)
}

fn summarize_custom_tool_input(input: Option<&str>) -> String {
    let input = input.unwrap_or_default();
    if let Some(first_path) = extract_patch_paths(input).into_iter().next() {
        return truncate_string(&first_path, MAX_ARGS_LEN);
    }
    truncate_string(input, MAX_ARGS_LEN)
}

fn custom_tool_patch_paths(payload: &CodexPayloadDto, tool: &str) -> Vec<String> {
    let input = payload.input.as_deref().unwrap_or_default();

    if payload.name.as_deref() == Some("exec") && tool == "apply_patch" {
        return extract_custom_apply_patch_input(input)
            .map(|patch| extract_patch_paths(&patch))
            .unwrap_or_default();
    }

    extract_patch_paths(input)
}

fn summarize_custom_tool_args(
    payload: &CodexPayloadDto,
    tool: &str,
    patch_paths: &[String],
) -> String {
    if payload.name.as_deref() == Some("exec") && tool == "exec_command" {
        if let Some(cmd) = payload
            .input
            .as_deref()
            .and_then(extract_custom_exec_command_args)
        {
            return truncate_string(&cmd, MAX_ARGS_LEN);
        }
    }

    if let Some(first_path) = patch_paths.first() {
        return truncate_string(&first_path, MAX_ARGS_LEN);
    }

    summarize_custom_tool_input(payload.input.as_deref())
}

fn custom_tool_is_test_file(patch_paths: &[String]) -> bool {
    patch_paths.iter().any(|path| is_test_file(path))
}

fn extract_patch_paths(input: &str) -> Vec<String> {
    input
        .lines()
        .filter_map(|line| {
            line.strip_prefix("*** Update File: ")
                .or_else(|| line.strip_prefix("*** Add File: "))
                .or_else(|| line.strip_prefix("*** Delete File: "))
                .map(str::to_string)
        })
        .collect()
}

fn custom_tool_output_failed(output: Option<&str>) -> bool {
    let raw = output.unwrap_or_default();
    let parsed: CodexCustomToolOutputDto = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(_) => return false,
    };

    parsed
        .metadata
        .and_then(|m| m.exit_code)
        .is_some_and(|code| code != 0)
}

fn exec_command_duration_ms(payload: &CodexPayloadDto) -> Option<u64> {
    if !payload.rest.contains_key("duration") {
        return None;
    }
    let duration = payload.rest.get("duration")?;
    let secs = duration.get("secs").and_then(Value::as_u64).unwrap_or(0);
    let nanos = duration.get("nanos").and_then(Value::as_u64).unwrap_or(0);
    Some(
        secs.saturating_mul(1000)
            .saturating_add(nanos.saturating_div(1_000_000)),
    )
}

fn truncate_string(input: &str, max_len: usize) -> String {
    if input.chars().count() <= max_len {
        return input.to_string();
    }

    let end = input
        .char_indices()
        .nth(max_len.saturating_sub(3))
        .map_or(input.len(), |(idx, _)| idx);
    format!("{}...", &input[..end])
}

fn now_iso8601() -> String {
    let now = std::time::SystemTime::now();
    let since_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));

    let total_secs = since_epoch.as_secs();
    let days = total_secs / 86_400;
    let secs_of_day = total_secs % 86_400;

    let (year, month, day) = days_to_date(days);
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, minute, second
    )
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn days_to_date(days_since_epoch: u64) -> (u64, u64, u64) {
    let z = days_since_epoch as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as u64, m as u64, d as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::FakeEventSink;
    use serde_json::json;

    #[test]
    fn recover_replies_returns_only_requested_completed_nonces() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let transcript_path = tmp.path().join("rollout.jsonl");
        let reply = |nonce: &str, text: &str| {
            json!({
                "type": "event_msg",
                "payload": {
                    "type": "task_complete",
                    "last_agent_message": format!(
                        "done\n<<<VIMEFLOW_REPLY\n{{\"v\":1,\"nonce\":\"{nonce}\",\"replies\":[{{\"id\":1,\"status\":\"resolved\",\"text\":\"{text}\"}}]}}\nVIMEFLOW_REPLY>>>"
                    )
                }
            })
        };
        std::fs::write(
            &transcript_path,
            format!(
                "{}\n{}\n{}\n",
                reply("wanted", "yes"),
                reply("wanted", "again"),
                reply("other", "no")
            ),
        )
        .expect("write transcript");

        let recovered = recover_replies(
            &transcript_path,
            "pty-1",
            &std::collections::HashSet::from(["wanted".to_string()]),
        )
        .expect("recover replies");

        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].session_id, "pty-1");
        assert_eq!(recovered[0].nonce.as_deref(), Some("wanted"));
        assert_eq!(
            recovered[0].replies.as_ref().expect("replies")[0].text,
            "yes"
        );
    }

    #[test]
    fn recover_reviews_returns_requested_completed_review() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let transcript_path = tmp.path().join("rollout.jsonl");
        let review = json!({
            "type": "event_msg",
            "payload": {
                "type": "task_complete",
                "last_agent_message": "done\n<<<VIMEFLOW_REVIEW\n{\"v\":1,\"nonce\":\"wanted\",\"reviewer\":\"Codex\",\"findings\":[{\"scope\":\"line\",\"path\":\"a.ts\",\"side\":\"additions\",\"line\":5,\"category\":\"bug\",\"text\":\"found\"}]}\nVIMEFLOW_REVIEW>>>"
            }
        });
        std::fs::write(&transcript_path, format!("{review}\n{review}\n"))
            .expect("write transcript");

        let recovered = recover_reviews(
            &transcript_path,
            "pty-1",
            &std::collections::HashSet::from(["wanted".to_string()]),
        )
        .expect("recover reviews");

        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].nonce.as_deref(), Some("wanted"));
        assert_eq!(
            recovered[0].findings.as_ref().expect("findings")[0].text,
            "found"
        );
    }

    fn lifecycle_phases(sink: &FakeEventSink) -> Vec<String> {
        sink.recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-lifecycle")
            .filter_map(|(_, p)| p["phase"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn codex_replay_flushes_only_the_settled_phase_once() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder = CodexTranscriptDecoder::new(sink.clone(), "sid".into(), None);
        decoder.decode_line(r#"{"type":"session_meta","payload":{"id":"cx-1","cwd":"/ws"}}"#);
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"task_started","model_context_window":1}}"#,
        );
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"task_complete","duration_ms":5}}"#,
        );
        decoder.on_caught_up();
        assert_eq!(lifecycle_phases(&sink), vec!["idle"]);
    }

    #[test]
    fn codex_live_emits_running_then_idle_transition() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder = CodexTranscriptDecoder::new(sink.clone(), "sid".into(), None);
        decoder.decode_line(r#"{"type":"session_meta","payload":{"id":"cx-1","cwd":"/ws"}}"#);
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"task_complete","duration_ms":1}}"#,
        );
        decoder.on_caught_up();
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"task_started","model_context_window":1}}"#,
        );
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"task_complete","duration_ms":5}}"#,
        );
        assert_eq!(lifecycle_phases(&sink), vec!["idle", "running", "idle"]);
    }

    #[test]
    fn codex_live_interrupt_emits_idle_transition() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder = CodexTranscriptDecoder::new(sink.clone(), "sid".into(), None);
        decoder.decode_line(r#"{"type":"session_meta","payload":{"id":"cx-1","cwd":"/ws"}}"#);
        decoder.on_caught_up();
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"turn-1","reason":"interrupted"}}"#,
        );

        assert_eq!(lifecycle_phases(&sink), vec!["running", "idle"]);
    }

    #[test]
    fn codex_semantic_attention_is_exact_and_live_only() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder = CodexTranscriptDecoder::new(sink.clone(), "sid".into(), None);
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"exec_approval_request","approval_id":"replayed"}}"#,
        );
        decoder.on_caught_up();
        assert_eq!(sink.count("agent-attention"), 0);

        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"exec_approval_request","approval_id":"approval-1"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"apply_patch_approval_request","call_id":"approval-2"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"question-1","arguments":"{}"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"request_permissions","call_id":"approval-3","arguments":"{}"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"elicitation_request","request_id":"question-2"}}"#,
        );

        let attention: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-attention")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(attention.len(), 5);
        assert_eq!(attention[0]["reason"], "approval-requested");
        assert_eq!(attention[0]["dedupeKey"], "approval-1");
        assert_eq!(attention[1]["reason"], "approval-requested");
        assert_eq!(attention[2]["reason"], "question-requested");
        assert_eq!(attention[2]["dedupeKey"], "question-1");
        assert_eq!(attention[3]["reason"], "approval-requested");
        assert_eq!(attention[3]["dedupeKey"], "approval-3");
        assert_eq!(attention[4]["reason"], "question-requested");
    }

    #[test]
    fn codex_semantic_attention_dedupe_key_is_bounded() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder = CodexTranscriptDecoder::new(sink.clone(), "sid".into(), None);
        decoder.on_caught_up();

        decoder.decode_line(&format!(
            r#"{{"type":"event_msg","payload":{{"type":"exec_approval_request","approval_id":"{}"}}}}"#,
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

    #[test]
    fn codex_replay_coalesces_tool_calls_then_live_emits_individually() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder = CodexTranscriptDecoder::new(sink.clone(), "sid".into(), None);

        // Replay: a user_message (turn) + one completed exec_command. Both are
        // suppressed during replay and folded into the boundary summary.
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"ls\"}"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"c1","exit_code":0}}"#,
        );

        // No per-line events during replay.
        assert_eq!(sink.count("agent-tool-call"), 0);
        assert_eq!(sink.count("agent-turn"), 0);

        decoder.on_caught_up();

        // One coalesced summary; tool_call_total counts the completed call.
        assert_eq!(sink.count("agent-replay-summary"), 1);
        let summaries: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-replay-summary")
            .collect();
        assert_eq!(summaries[0].1["numTurns"], 1);
        assert_eq!(summaries[0].1["toolCallTotal"], 1);

        // After catch-up, a live exec_command emits individual events again.
        decoder.decode_line(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c2","arguments":"{\"cmd\":\"ls\"}"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"c2","exit_code":0}}"#,
        );
        // running + done for the live call.
        assert_eq!(sink.count("agent-tool-call"), 2);
        // Still exactly one summary (boundary is one-shot).
        assert_eq!(sink.count("agent-replay-summary"), 1);
    }

    #[test]
    fn codex_live_emits_code_mode_and_namespaced_tool_names() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder = CodexTranscriptDecoder::new(sink.clone(), "sid".into(), None);
        decoder.on_caught_up();

        decoder.decode_line(
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "call_id": "c1",
                    "input": "const result = await tools.exec_command({ cmd: 'git status' })"
                }
            })
            .to_string(),
        );
        decoder.decode_line(
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "send_message",
                    "namespace": "collaboration",
                    "call_id": "c2",
                    "arguments": "{}"
                }
            })
            .to_string(),
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].1["tool"], "exec_command");
        assert_eq!(tool_calls[1].1["tool"], "collaboration__send_message");
    }

    #[test]
    fn codex_replay_flushes_in_flight_tool_call_at_catch_up() {
        let sink = Arc::new(FakeEventSink::new());
        let mut decoder = CodexTranscriptDecoder::new(sink.clone(), "sid".into(), None);

        decoder.decode_line(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"sleep 60\"}"}}"#,
        );
        decoder.on_caught_up();

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].1["toolUseId"], "c1");
        assert_eq!(tool_calls[0].1["status"], "running");
    }

    fn write_rollout(path: &Path, lines: &[Value]) {
        let body = lines
            .iter()
            .map(|line| serde_json::to_string(line).expect("serialize fixture line"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, format!("{}\n", body)).expect("write rollout fixture");
    }

    #[test]
    fn validate_transcript_path_accepts_file_under_codex_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let codex_root = tmp.path().join(".codex");
        let transcript_path = codex_root.join("sessions").join("rollout.jsonl");
        std::fs::create_dir_all(transcript_path.parent().expect("rollout parent directory"))
            .expect("mkdir codex root");
        std::fs::write(&transcript_path, "").expect("write transcript");

        let canonical = validate_transcript_path_under_root(
            transcript_path.to_str().expect("utf8 rollout path"),
            &codex_root,
        )
        .expect("path under root should validate");

        assert_eq!(
            canonical,
            std::fs::canonicalize(&transcript_path).expect("canonical rollout path")
        );
    }

    #[test]
    fn validate_transcript_path_rejects_path_outside_codex_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let codex_root = tmp.path().join(".codex");
        let transcript_path = tmp.path().join("rollout.jsonl");
        std::fs::create_dir_all(&codex_root).expect("mkdir codex root");
        std::fs::write(&transcript_path, "").expect("write transcript");

        let result = validate_transcript_path_under_root(
            transcript_path.to_str().expect("utf8 rollout path"),
            &codex_root,
        );

        assert!(matches!(
            result,
            Err(ValidateTranscriptError::OutsideRoot { .. })
        ));
    }

    #[test]
    fn start_tailing_replays_tool_calls_turns_and_test_runs() {
        let sink = Arc::new(FakeEventSink::new());

        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).expect("workspace dirs");
        std::fs::write(workspace.join("src/foo.test.ts"), "").expect("test file");

        let transcript_path = tmp.path().join("rollout.jsonl");
        write_rollout(
            &transcript_path,
            &[
                json!({
                    "timestamp": "2026-05-04T10:00:00Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "user_message",
                        "message": "run the tests"
                    }
                }),
                json!({
                    "timestamp": "2026-05-04T10:00:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": "{\"cmd\":\"cargo test\",\"workdir\":\"/tmp/ws\"}",
                        "call_id": "call_exec"
                    }
                }),
                json!({
                    "timestamp": "2026-05-04T10:00:02Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "exec_command_end",
                        "call_id": "call_exec",
                        "command": ["/bin/bash", "-lc", "cargo test"],
                        "aggregated_output": "running 1 test\ntest mycrate::tests::test_a ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
                        "exit_code": 0,
                        "duration": {
                            "secs": 1,
                            "nanos": 250000000
                        },
                        "status": "completed"
                    }
                }),
                json!({
                    "timestamp": "2026-05-04T10:00:02.100Z",
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_exec",
                        "output": "cargo test output"
                    }
                }),
                json!({
                    "timestamp": "2026-05-04T10:00:03Z",
                    "type": "response_item",
                    "payload": {
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": "call_patch",
                        "name": "apply_patch",
                        "input": format!(
                            "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch\n",
                            workspace.join("src/foo.test.ts").display()
                        )
                    }
                }),
                json!({
                    "timestamp": "2026-05-04T10:00:03.050Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "patch_apply_end",
                        "call_id": "call_patch",
                        "success": true,
                        "status": "completed"
                    }
                }),
                json!({
                    "timestamp": "2026-05-04T10:00:03.060Z",
                    "type": "response_item",
                    "payload": {
                        "type": "custom_tool_call_output",
                        "call_id": "call_patch",
                        "output": "{\"output\":\"Success\",\"metadata\":{\"exit_code\":0,\"duration_seconds\":0.0}}"
                    }
                }),
            ],
        );

        let handle = start_tailing(
            sink.clone(),
            "sid-1".to_string(),
            transcript_path,
            Some(workspace.clone()),
        )
        .expect("tailing should start");

        // Replay coalescing: the whole transcript replays before catch-up, so
        // the individual `agent-tool-call` / `agent-turn` / `agent-cwd` events
        // are suppressed and folded into ONE `agent-replay-summary` flushed at
        // the replay→live boundary (`on_caught_up`). `test-run` still emits
        // once (it routes through the separate replay-aware emitter that
        // collapses to latest). Wait for the summary, then snapshot.
        assert!(
            sink.wait_for_count("agent-replay-summary", 1, Duration::from_secs(5)),
            "expected 1 agent-replay-summary event within 5s",
        );
        assert!(
            sink.wait_for_count("test-run", 1, Duration::from_secs(5)),
            "expected 1 test-run event within 5s",
        );
        handle.stop();

        let recorded = sink.recorded();

        // During replay, no per-line tool-call / turn events fire.
        assert_eq!(
            sink.count("agent-tool-call"),
            0,
            "replay tool calls are coalesced into the summary, not emitted individually",
        );
        assert_eq!(
            sink.count("agent-turn"),
            0,
            "replay turns are coalesced into the summary",
        );

        let summary_payloads: Vec<Value> = recorded
            .iter()
            .filter(|(event, _)| event == "agent-replay-summary")
            .map(|(_, payload)| payload.clone())
            .collect();
        assert_eq!(summary_payloads.len(), 1);
        let summary = &summary_payloads[0];
        assert_eq!(summary["sessionId"], "sid-1");
        assert_eq!(summary["numTurns"], 1);
        // The exec_command workdir (mid-session cwd source) is the final cwd.
        assert_eq!(summary["cwd"], "/tmp/ws");
        // Only the two COMPLETED calls (call_exec done + call_patch done) are
        // folded; the two `running` events observed during replay are dropped.
        assert_eq!(summary["toolCallTotal"], 2);
        let recent = summary["recentToolCalls"]
            .as_array()
            .expect("recentToolCalls array");
        assert_eq!(recent.len(), 2);
        // Newest-first: the apply_patch completion is most recent.
        assert_eq!(recent[0]["toolUseId"], "call_patch");
        assert_eq!(recent[0]["status"], "done");
        assert_eq!(recent[0]["isTestFile"], true);
        assert_eq!(recent[1]["toolUseId"], "call_exec");
        assert_eq!(recent[1]["status"], "done");
        assert_eq!(recent[1]["durationMs"], 1250);

        let test_run_payloads: Vec<Value> = recorded
            .iter()
            .filter(|(event, _)| event == "test-run")
            .map(|(_, payload)| payload.clone())
            .collect();
        assert_eq!(test_run_payloads.len(), 1);
        assert_eq!(test_run_payloads[0]["runner"], "cargo");
        assert_eq!(test_run_payloads[0]["summary"]["passed"], 1);
        assert_eq!(test_run_payloads[0]["summary"]["failed"], 0);
    }

    /// Append `\n`-terminated JSONL records to an existing rollout (live-tail tests).
    fn append_rollout(path: &Path, lines: &[Value]) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open rollout for append");
        for line in lines {
            let serialized = serde_json::to_string(line).expect("serialize rollout line");
            writeln!(file, "{serialized}").expect("append rollout line");
        }
    }

    /// An `exec_command` `function_call` + its `exec_command_end` (a passing
    /// `cargo test` run → one `test-run` snapshot), matched by `call_id`.
    fn exec_test_pair(call_id: &str, ts_start: &str, ts_end: &str) -> [Value; 2] {
        [
            json!({
                "timestamp": ts_start,
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"cargo test\",\"workdir\":\"/tmp/ws\"}",
                    "call_id": call_id
                }
            }),
            json!({
                "timestamp": ts_end,
                "type": "event_msg",
                "payload": {
                    "type": "exec_command_end",
                    "call_id": call_id,
                    "command": ["/bin/bash", "-lc", "cargo test"],
                    "aggregated_output": "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
                    "exit_code": 0,
                    "duration": { "secs": 1, "nanos": 0 },
                    "status": "completed"
                }
            }),
        ]
    }

    /// Phase 0 (Task 0.2): pin the replay→live boundary via `test-run` (Codex).
    ///
    /// Characterization test — it must PASS against current code. Three
    /// `exec_command` test-run pairs replay-collapse to one `test-run` at
    /// `finish_replay`; a fourth pair appended *after* catch-up emits a second,
    /// live `test-run`. A sentinel `user_message` (→ agent-turn) is the drain
    /// barrier. We author >=3 pairs (the existing fixture has only one) so the
    /// collapse is observable: `count(test-run) == 2` uniquely means
    /// "1 collapsed + 1 live" (an uncollapsed replay would total >=3).
    #[test]
    fn rollout_replay_collapses_then_live_test_run_emits() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        let transcript_path = tmp.path().join("rollout.jsonl");

        let mut records = Vec::new();
        records.extend(exec_test_pair(
            "call_exec1",
            "2026-05-04T10:00:00Z",
            "2026-05-04T10:00:01Z",
        ));
        records.extend(exec_test_pair(
            "call_exec2",
            "2026-05-04T10:00:02Z",
            "2026-05-04T10:00:03Z",
        ));
        records.extend(exec_test_pair(
            "call_exec3",
            "2026-05-04T10:00:04Z",
            "2026-05-04T10:00:05Z",
        ));
        write_rollout(&transcript_path, &records);

        let handle = start_tailing(
            sink.clone(),
            "sid-replay-live".to_string(),
            transcript_path.clone(),
            Some(workspace.clone()), // cwd: test-run snapshot is skipped when None
        )
        .expect("tailing should start");

        // (a) Catch-up barrier: replay collapses the 3 snapshots to one test-run.
        assert!(
            sink.wait_for_count("test-run", 1, Duration::from_secs(5)),
            "replay should emit one collapsed test-run",
        );

        // (b) Append a NEW live pair (call_exec4).
        append_rollout(
            &transcript_path,
            &exec_test_pair("call_exec4", "2026-05-04T10:00:06Z", "2026-05-04T10:00:07Z"),
        );

        // (c) Drain barrier: a sentinel user_message -> agent-turn (baseline-relative).
        let turns_before = sink.count("agent-turn");
        append_rollout(
            &transcript_path,
            &[json!({
                "timestamp": "2026-05-04T10:00:08Z",
                "type": "event_msg",
                "payload": { "type": "user_message", "message": "sentinel" }
            })],
        );
        assert!(
            sink.wait_for_count("agent-turn", turns_before + 1, Duration::from_secs(5)),
            "sentinel agent-turn should drain past the live pair",
        );

        handle.stop();

        // (d) Exactly two test-runs: 1 replay-collapsed + 1 live.
        assert_eq!(sink.count("test-run"), 2, "1 replay-collapsed + 1 live");
    }

    #[test]
    fn summarize_function_call_args_prefers_exec_command_cmd() {
        assert_eq!(
            summarize_function_call_args(Some(
                "{\"cmd\":\"cargo test --workspace --all-features\",\"workdir\":\"/tmp/ws\"}"
            )),
            "cargo test --workspace --all-features"
        );
    }

    #[test]
    fn extract_custom_exec_command_args_reads_nested_cmd() {
        assert_eq!(
            extract_custom_exec_command_args(
                "const result = await tools.exec_command({ cmd: 'git status' })"
            ),
            Some("git status".to_string())
        );
    }

    #[test]
    fn extract_custom_exec_command_args_reads_nested_command_alias() {
        assert_eq!(
            extract_custom_exec_command_args(
                "const command = 'npm test'\nawait tools.exec_command({ command })"
            ),
            Some("npm test".to_string())
        );
    }

    #[test]
    fn extract_custom_exec_command_args_decodes_hex_and_unicode_escapes() {
        assert_eq!(
            extract_custom_exec_command_args(
                "const command = 'printf \\x2fworkspace\\u002fA\\u{20ac}'\n\
                 await tools.exec_command({ command })"
            ),
            Some("printf /workspace/A€".to_string())
        );
    }

    #[test]
    fn extract_custom_exec_command_args_abstains_on_malformed_hex_escape() {
        assert_eq!(
            extract_custom_exec_command_args(
                "const command = 'printf \\xZZ'\n\
                 await tools.exec_command({ command })"
            ),
            None
        );
    }

    #[test]
    fn extract_custom_apply_patch_input_decodes_string_argument() {
        assert_eq!(
            extract_custom_apply_patch_input(
                "await tools.apply_patch(\"*** Begin Patch\\n*** Update File: src/App.test.tsx\\n*** End Patch\")"
            ),
            Some("*** Begin Patch\n*** Update File: src/App.test.tsx\n*** End Patch".to_string())
        );
    }

    #[test]
    fn extract_custom_apply_patch_input_reads_const_alias() {
        assert_eq!(
            extract_custom_apply_patch_input(
                "const patch = '*** Begin Patch\\n*** Update File: src/App.test.tsx\\n*** End Patch'\ntext(await tools.apply_patch(patch))"
            ),
            Some("*** Begin Patch\n*** Update File: src/App.test.tsx\n*** End Patch".to_string())
        );
    }

    #[test]
    fn extract_custom_apply_patch_input_preserves_non_ascii_paths() {
        assert_eq!(
            extract_custom_apply_patch_input(
                "const patch = '*** Begin Patch\\n*** Update File: src/é.test.ts\\n*** End Patch'\nawait tools.apply_patch(patch)"
            ),
            Some("*** Begin Patch\n*** Update File: src/é.test.ts\n*** End Patch".to_string())
        );
    }

    #[test]
    fn extract_custom_apply_patch_input_decodes_unicode_path_escapes() {
        assert_eq!(
            extract_custom_apply_patch_input(
                "const patch = '*** Begin Patch\\n*** Update File: \
                 src/\\u00e9.test.ts\\n*** End Patch'\n\
                 await tools.apply_patch(patch)"
            ),
            Some("*** Begin Patch\n*** Update File: src/é.test.ts\n*** End Patch".to_string())
        );
    }

    #[test]
    fn extract_custom_apply_patch_input_abstains_on_malformed_unicode_escape() {
        assert_eq!(
            extract_custom_apply_patch_input(
                "const patch = '*** Begin Patch\\n*** Update File: \
                 src/\\u{110000}.test.ts\\n*** End Patch'\n\
                 await tools.apply_patch(patch)"
            ),
            None
        );
    }

    #[test]
    fn custom_tool_output_failed_reads_metadata_exit_code() {
        assert!(custom_tool_output_failed(Some(
            "{\"output\":\"nope\",\"metadata\":{\"exit_code\":1}}"
        )));
    }

    #[test]
    fn tool_call_name_preserves_direct_namespace() {
        let payload: CodexPayloadDto = serde_json::from_value(json!({
            "type": "function_call",
            "name": "send_message",
            "namespace": "collaboration"
        }))
        .unwrap();

        assert_eq!(tool_call_name(&payload), "collaboration__send_message");
    }

    #[test]
    fn tool_call_name_uses_single_code_mode_nested_tool() {
        let payload: CodexPayloadDto = serde_json::from_value(json!({
            "type": "custom_tool_call",
            "name": "exec",
            "input": "const first = await tools.exec_command({ cmd: 'pwd' })\nconst second = await tools.exec_command({ cmd: 'git status' })"
        }))
        .unwrap();

        assert_eq!(tool_call_name(&payload), "exec_command");
    }

    #[test]
    fn tool_call_name_keeps_mixed_code_mode_cells_as_exec() {
        let payload: CodexPayloadDto = serde_json::from_value(json!({
            "type": "custom_tool_call",
            "name": "exec",
            "input": "const result = await tools.exec_command({ cmd: 'pwd' })\ntext(await tools.mcp__linear__save_issue({ id: 'VIM-365' }))"
        }))
        .unwrap();

        assert_eq!(tool_call_name(&payload), "exec");
    }

    #[test]
    fn tool_call_name_ignores_code_and_comments_that_mention_tools() {
        let payload: CodexPayloadDto = serde_json::from_value(json!({
            "type": "custom_tool_call",
            "name": "exec",
            "input": "// tools.apply_patch('not a call')\nconst example = \"tools.update_plan({})\"\nawait tools.web__run({ search_query: [{ q: 'Codex' }] })"
        }))
        .unwrap();

        assert_eq!(tool_call_name(&payload), "web__run");
    }

    // ---- extract_session_cwd unit tests (v2: session_meta ONLY) ----

    /// Parse a rollout line into the payload the cwd extractors now take —
    /// mirrors the single parse `process_line` does before calling them.
    fn payload_of(dto: &CodexLineDto) -> CodexPayloadDto {
        serde_json::from_value::<CodexPayloadDto>(dto.payload.clone()).unwrap_or_default()
    }

    #[test]
    fn extract_session_cwd_session_meta_returns_cwd() {
        let dto: CodexLineDto =
            serde_json::from_str(r#"{"type":"session_meta","payload":{"cwd":"/workspace/A"}}"#)
                .unwrap();
        assert_eq!(
            extract_session_cwd(dto.record_type(), &payload_of(&dto)),
            Some("/workspace/A".to_string())
        );
    }

    #[test]
    fn extract_session_cwd_turn_context_returns_none() {
        // v2 spec section 1: turn_context is INTENTIONALLY NOT a cwd source.
        // Codex's turn_context.cwd is pinned to session-start and treating
        // it as live would cause false reverts after exec_command transitions.
        // This test is a defensive guard against re-introduction.
        let dto: CodexLineDto =
            serde_json::from_str(r#"{"type":"turn_context","payload":{"cwd":"/workspace/A"}}"#)
                .unwrap();
        assert_eq!(
            extract_session_cwd(dto.record_type(), &payload_of(&dto)),
            None
        );
    }

    #[test]
    fn extract_session_cwd_other_type_returns_none() {
        let dto: CodexLineDto =
            serde_json::from_str(r#"{"type":"event_msg","payload":{"cwd":"/workspace/A"}}"#)
                .unwrap();
        assert_eq!(
            extract_session_cwd(dto.record_type(), &payload_of(&dto)),
            None
        );
    }

    #[test]
    fn extract_session_cwd_empty_string_returns_none() {
        let dto: CodexLineDto =
            serde_json::from_str(r#"{"type":"session_meta","payload":{"cwd":""}}"#).unwrap();
        assert_eq!(
            extract_session_cwd(dto.record_type(), &payload_of(&dto)),
            None
        );
    }

    // ---- extract_exec_workdir unit tests (the mid-session signal) ----

    #[test]
    fn extract_exec_workdir_happy_path() {
        let dto: CodexLineDto = serde_json::from_str(r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"ls\",\"workdir\":\"/workspace/B\"}"}}"#).unwrap();
        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)).as_deref(),
            Some("/workspace/B")
        );
    }

    #[test]
    fn extract_exec_workdir_from_current_custom_exec_schema() {
        let dto: CodexLineDto = serde_json::from_value(json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "c1",
                "input": "const first = await tools.exec_command({\"cmd\":\"pwd\",\"workdir\":\"/workspace/A\"})\nconst second = await tools.exec_command({ workdir: '/workspace/B', cmd: 'workdir: \"/decoy\"' })"
            }
        }))
        .unwrap();

        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)).as_deref(),
            Some("/workspace/B")
        );
    }

    #[test]
    fn extract_exec_workdir_decodes_unicode_escapes_from_custom_exec() {
        let dto: CodexLineDto = serde_json::from_value(json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "c1",
                "input": "await tools.exec_command({ cmd: 'pwd', workdir: '/workspace/\\u0041' })"
            }
        }))
        .unwrap();

        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)).as_deref(),
            Some("/workspace/A")
        );
    }

    #[test]
    fn extract_exec_workdir_ignores_patch_source_text() {
        let dto: CodexLineDto = serde_json::from_value(json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "c1",
                "input": "const patch = \"*** Begin Patch\\n+ workdir: '/workspace/decoy' }\\n*** End Patch\";\ntext(await tools.apply_patch(patch));"
            }
        }))
        .unwrap();

        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)),
            None
        );
    }

    #[test]
    fn extract_exec_workdir_resolves_simple_const() {
        let dto: CodexLineDto = serde_json::from_value(json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "c1",
                "input": "const wd = '/workspace/B';\nconst result = await tools.exec_command({ cmd: \"pwd\", workdir: wd });\ntext(result.output);"
            }
        }))
        .unwrap();

        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)).as_deref(),
            Some("/workspace/B")
        );
    }

    #[test]
    fn extract_exec_workdir_resolves_asi_terminated_const() {
        let dto: CodexLineDto = serde_json::from_value(json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "c1",
                "input": "const wd = '/workspace/B'\nawait tools.exec_command({ cmd: 'pwd', workdir: wd })"
            }
        }))
        .unwrap();

        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)).as_deref(),
            Some("/workspace/B")
        );
    }

    #[test]
    fn extract_exec_workdir_rejects_multiline_computed_const() {
        let dto: CodexLineDto = serde_json::from_value(json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "c1",
                "input": "const wd = '/workspace'\n+ '/B'\nawait tools.exec_command({ cmd: 'pwd', workdir: wd })"
            }
        }))
        .unwrap();

        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)),
            None
        );
    }

    #[test]
    fn extract_exec_workdir_ignores_workdir_used_as_another_property_value() {
        let dto: CodexLineDto = serde_json::from_value(json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "c1",
                "input": "const workdir = '/workspace/decoy';\nawait tools.exec_command({ cmd: 'pwd', workdir: '/workspace/B' });\nawait tools.exec_command({ cmd: 'echo', cwd: workdir });"
            }
        }))
        .unwrap();

        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)).as_deref(),
            Some("/workspace/B")
        );
    }

    #[test]
    fn extract_exec_workdir_other_event_type_returns_none() {
        // event_msg carrying a function_call-shaped payload should still
        // be rejected — the outer event type gate is response_item.
        let dto: CodexLineDto = serde_json::from_str(r#"{"type":"event_msg","payload":{"type":"function_call","name":"exec_command","arguments":"{\"workdir\":\"/x\"}"}}"#).unwrap();
        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)),
            None
        );
    }

    #[test]
    fn extract_exec_workdir_non_function_call_returns_none() {
        let dto: CodexLineDto = serde_json::from_str(r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec_command","input":"{\"workdir\":\"/x\"}"}}"#).unwrap();
        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)),
            None
        );
    }

    #[test]
    fn extract_exec_workdir_non_exec_command_returns_none() {
        let dto: CodexLineDto = serde_json::from_str(r#"{"type":"response_item","payload":{"type":"function_call","name":"read_file","arguments":"{\"path\":\"/x\"}"}}"#).unwrap();
        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)),
            None
        );
    }

    #[test]
    fn extract_exec_workdir_malformed_arguments_json_returns_none() {
        let dto: CodexLineDto = serde_json::from_str(r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{not json"}}"#).unwrap();
        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)),
            None
        );
    }

    #[test]
    fn extract_exec_workdir_missing_workdir_field_returns_none() {
        let dto: CodexLineDto = serde_json::from_str(r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"}}"#).unwrap();
        assert_eq!(
            extract_exec_workdir(dto.record_type(), &payload_of(&dto)),
            None
        );
    }

    // ---- process_line transition-semantics tests ----

    fn empty_in_flight() -> InFlightToolCalls {
        HashMap::new()
    }

    #[test]
    fn process_line_first_cwd_always_emits() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let line = r#"{"timestamp":"2026-05-22T10:00:00Z","type":"session_meta","payload":{"id":"sid","cwd":"/workspace/A"}}"#;
        process_line(
            line,
            "sid-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let cwd_events: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-cwd")
            .collect();
        assert_eq!(cwd_events.len(), 1);
        assert_eq!(cwd_events[0].1["cwd"], "/workspace/A");
        assert_eq!(last_cwd.as_deref(), Some("/workspace/A"));
    }

    #[test]
    fn process_line_task_complete_with_reply_block_emits_agent_reply() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;

        // last_agent_message carries the sentinel block (escaped JSON string).
        let line = r#"{"timestamp":"t","type":"event_msg","payload":{"type":"task_complete","duration_ms":5,"last_agent_message":"done\n<<<VIMEFLOW_REPLY\n{\"v\":1,\"nonce\":\"abc\",\"replies\":[{\"id\":1,\"status\":\"answered\",\"text\":\"because latency\"}]}\nVIMEFLOW_REPLY>>>"}}"#;
        process_line(
            line,
            "pty-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut None,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let replies: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-reply")
            .collect();
        assert_eq!(replies.len(), 1);
        let payload = &replies[0].1;
        assert_eq!(payload["sessionId"], "pty-1");
        assert_eq!(payload["nonce"], "abc");
        assert_eq!(payload["replies"][0]["id"], 1);
        // legacy "answered" in the block is normalized to the canonical outcome.
        assert_eq!(payload["replies"][0]["status"], "reply");
    }

    #[test]
    fn process_line_task_complete_without_sentinel_emits_no_reply() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;

        let line = r#"{"timestamp":"t","type":"event_msg","payload":{"type":"task_complete","duration_ms":5,"last_agent_message":"just done"}}"#;
        process_line(
            line,
            "pty-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut None,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        assert!(sink
            .recorded()
            .iter()
            .all(|(name, _)| name != "agent-reply"));
    }

    #[test]
    fn process_line_task_complete_with_review_block_emits_agent_review() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;

        let line = r#"{"timestamp":"t","type":"event_msg","payload":{"type":"task_complete","duration_ms":5,"last_agent_message":"done\n<<<VIMEFLOW_REVIEW\n{\"v\":1,\"nonce\":\"n\",\"reviewer\":\"codex\",\"findings\":[{\"scope\":\"line\",\"path\":\"a.ts\",\"side\":\"additions\",\"line\":5,\"category\":\"bug\",\"text\":\"x\"}]}\nVIMEFLOW_REVIEW>>>"}}"#;
        process_line(
            line,
            "pty-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut None,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let reviews: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-review")
            .collect();
        assert_eq!(reviews.len(), 1);
        let payload = &reviews[0].1;
        assert_eq!(payload["sessionId"], "pty-1");
        assert_eq!(payload["nonce"], "n");
        assert_eq!(payload["reviewer"], "codex");
        assert_eq!(payload["findings"][0]["scope"], "line");
        assert_eq!(payload["findings"][0]["line"], 5);
    }

    #[test]
    fn process_line_task_complete_without_review_block_emits_no_review() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;

        let line = r#"{"timestamp":"t","type":"event_msg","payload":{"type":"task_complete","duration_ms":5,"last_agent_message":"just done"}}"#;
        process_line(
            line,
            "pty-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut None,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        assert!(sink
            .recorded()
            .iter()
            .all(|(name, _)| name != "agent-review"));
    }

    #[test]
    fn process_line_replayed_task_complete_with_reply_block_emits_no_reply() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;

        let line = r#"{"timestamp":"t","type":"event_msg","payload":{"type":"task_complete","duration_ms":5,"last_agent_message":"done\n<<<VIMEFLOW_REPLY\n{\"v\":1,\"nonce\":\"abc\",\"replies\":[{\"id\":1,\"status\":\"answered\",\"text\":\"because latency\"}]}\nVIMEFLOW_REPLY>>>"}}"#;
        process_line(
            line,
            "pty-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut None,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            false,
        );

        assert!(sink
            .recorded()
            .iter()
            .all(|(name, _)| name != "agent-reply"));
    }

    #[test]
    fn process_line_repeated_cwd_across_sources_suppresses() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let lines = [
            r#"{"timestamp":"2026-05-22T10:00:00Z","type":"session_meta","payload":{"id":"sid","cwd":"/workspace/A"}}"#,
            r#"{"timestamp":"2026-05-22T10:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"ls\",\"workdir\":\"/workspace/A\"}"}}"#,
        ];
        for line in lines {
            process_line(
                line,
                "sid-1",
                None,
                &events,
                &mut emitter,
                &mut in_flight,
                &mut num_turns,
                &mut last_cwd,
                &mut String::new(),
                &mut None,
                &mut None,
                &mut ReplayActivity::default(),
                true,
            );
        }

        let cwd_events: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-cwd")
            .collect();
        assert_eq!(cwd_events.len(), 1);
        assert_eq!(cwd_events[0].1["cwd"], "/workspace/A");
    }

    #[test]
    fn process_line_cwd_transition_across_sources_emits() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let lines = [
            r#"{"timestamp":"2026-05-22T10:00:00Z","type":"session_meta","payload":{"id":"sid","cwd":"/workspace/A"}}"#,
            r#"{"timestamp":"2026-05-22T10:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"ls\",\"workdir\":\"/workspace/B\"}"}}"#,
            r#"{"timestamp":"2026-05-22T10:00:02Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c2","arguments":"{\"cmd\":\"ls\",\"workdir\":\"/workspace/A\"}"}}"#,
        ];
        for line in lines {
            process_line(
                line,
                "sid-1",
                None,
                &events,
                &mut emitter,
                &mut in_flight,
                &mut num_turns,
                &mut last_cwd,
                &mut String::new(),
                &mut None,
                &mut None,
                &mut ReplayActivity::default(),
                true,
            );
        }

        let cwd_events: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-cwd")
            .collect();
        assert_eq!(cwd_events.len(), 3);
        assert_eq!(cwd_events[0].1["cwd"], "/workspace/A");
        assert_eq!(cwd_events[1].1["cwd"], "/workspace/B");
        assert_eq!(cwd_events[2].1["cwd"], "/workspace/A");
    }

    /// v2-critical regression guard. Codex review on the v2 spec (HIGH)
    /// flagged that including turn_context.cwd as a cwd source would
    /// cause a false revert: after session_meta(A) → exec_command(B),
    /// the next turn's turn_context(A) (pinned to session-start) would
    /// emit agent-cwd=A and bounce the pane chip back. This test locks
    /// in the v2 design decision to skip turn_context entirely.
    /// If anyone re-adds turn_context to extract_session_cwd, this
    /// test fires.
    #[test]
    fn process_line_turn_context_after_exec_command_does_not_revert() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let lines = [
            r#"{"timestamp":"2026-05-22T10:00:00Z","type":"session_meta","payload":{"id":"sid","cwd":"/workspace/A"}}"#,
            r#"{"timestamp":"2026-05-22T10:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"ls\",\"workdir\":\"/workspace/B\"}"}}"#,
            r#"{"timestamp":"2026-05-22T10:00:02Z","type":"turn_context","payload":{"turn_id":"t1","cwd":"/workspace/A"}}"#,
        ];
        for line in lines {
            process_line(
                line,
                "sid-1",
                None,
                &events,
                &mut emitter,
                &mut in_flight,
                &mut num_turns,
                &mut last_cwd,
                &mut String::new(),
                &mut None,
                &mut None,
                &mut ReplayActivity::default(),
                true,
            );
        }

        let cwd_events: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-cwd")
            .collect();
        assert_eq!(
            cwd_events.len(),
            2,
            "turn_context.cwd MUST NOT emit a cwd event \
             (would cause false revert to session-start after exec_command transition)"
        );
        assert_eq!(cwd_events[0].1["cwd"], "/workspace/A");
        assert_eq!(cwd_events[1].1["cwd"], "/workspace/B");
        // Crucially, last_cwd should still be B — the worktree we're in.
        assert_eq!(last_cwd.as_deref(), Some("/workspace/B"));
    }

    #[test]
    fn process_line_new_session_meta_resets_run_scoped_tail_state() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;
        let mut codex_agent_session_id = String::new();
        let mut last_phase = None;
        let mut replay_phase = None;

        let lines = [
            r#"{"timestamp":"2026-05-22T10:00:00Z","type":"session_meta","payload":{"id":"old-run","cwd":"/workspace/A"}}"#,
            r#"{"timestamp":"2026-05-22T10:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"old prompt"}}"#,
            r#"{"timestamp":"2026-05-22T10:00:02Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"old-call","arguments":"{\"cmd\":\"npm run lint\"}"}}"#,
        ];
        for line in lines {
            process_line(
                line,
                "sid-1",
                None,
                &events,
                &mut emitter,
                &mut in_flight,
                &mut num_turns,
                &mut last_cwd,
                &mut codex_agent_session_id,
                &mut last_phase,
                &mut replay_phase,
                &mut ReplayActivity::default(),
                true,
            );
        }

        assert_eq!(codex_agent_session_id, "old-run");
        assert_eq!(num_turns, 1);
        assert_eq!(in_flight.len(), 1);

        process_line(
            r#"{"timestamp":"2026-05-22T10:00:03Z","type":"session_meta","payload":{"id":"new-run","cwd":"/workspace/A"}}"#,
            "sid-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut codex_agent_session_id,
            &mut last_phase,
            &mut replay_phase,
            &mut ReplayActivity::default(),
            true,
        );

        assert_eq!(codex_agent_session_id, "new-run");
        assert_eq!(num_turns, 0);
        assert!(in_flight.is_empty());
        assert_eq!(last_cwd.as_deref(), Some("/workspace/A"));

        process_line(
            r#"{"timestamp":"2026-05-22T10:00:04Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"old-call","exit_code":0,"duration":10}}"#,
            "sid-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut codex_agent_session_id,
            &mut last_phase,
            &mut replay_phase,
            &mut ReplayActivity::default(),
            true,
        );
        process_line(
            r#"{"timestamp":"2026-05-22T10:00:05Z","type":"event_msg","payload":{"type":"user_message","message":"new prompt"}}"#,
            "sid-1",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut codex_agent_session_id,
            &mut last_phase,
            &mut replay_phase,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-tool-call")
            .collect();
        assert_eq!(
            tool_calls.len(),
            1,
            "the old running tool call must not complete after a /clear boundary"
        );
        assert_eq!(num_turns, 1);

        let turns: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(name, _)| name == "agent-turn")
            .map(|(_, payload)| payload["numTurns"].clone())
            .collect();
        assert_eq!(turns, vec![json!(1), json!(1)]);
    }

    #[test]
    fn process_line_new_session_meta_resets_replay_activity() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;
        let mut codex_agent_session_id = String::new();
        let mut last_phase = None;
        let mut replay_phase = None;
        let mut replay_activity = ReplayActivity::default();

        for line in [
            r#"{"timestamp":"2026-06-28T10:00:00Z","type":"session_meta","payload":{"id":"old-run","cwd":"/workspace/A"}}"#,
            r#"{"timestamp":"2026-06-28T10:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"old-call","arguments":"{\"cmd\":\"npm test\"}"}}"#,
            r#"{"timestamp":"2026-06-28T10:00:02Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"old-call","exit_code":0,"duration":10}}"#,
            r#"{"timestamp":"2026-06-28T10:00:03Z","type":"session_meta","payload":{"id":"new-run","cwd":"/workspace/A"}}"#,
        ] {
            process_line(
                line,
                "sid-1",
                None,
                &events,
                &mut emitter,
                &mut in_flight,
                &mut num_turns,
                &mut last_cwd,
                &mut codex_agent_session_id,
                &mut last_phase,
                &mut replay_phase,
                &mut replay_activity,
                false,
            );
        }

        let summary = replay_activity.into_summary("sid-1".into(), num_turns, last_cwd);
        assert_eq!(summary.tool_call_total, 0);
        assert!(summary.recent_tool_calls.is_empty());
    }

    // ---- DTO-migration regression tests (Task 1.6) ----

    #[test]
    fn process_line_wrong_typed_exit_code_emits_done() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let start = r#"{"timestamp":"2026-05-22T10:00:00Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"ls\"}"}}"#;
        process_line(
            start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let end = r#"{"timestamp":"2026-05-22T10:00:01Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"c1","exit_code":"bad","aggregated_output":"ok"}}"#;
        process_line(
            end,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[1].1["status"], "done");
    }

    #[test]
    fn process_line_exec_command_end_finalizes_after_function_call_output() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let start = r#"{"timestamp":"2026-06-15T10:00:00Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call_cmd","arguments":"{\"cmd\":\"git status\"}"}}"#;
        process_line(
            start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let output = r#"{"timestamp":"2026-06-15T10:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_cmd","output":"Chunk ID: abc\nWall time: 1.2345 seconds\nProcess exited with code 0\nOutput:\n## main\n"}}"#;
        process_line(
            output,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        assert!(
            in_flight.contains_key("call_cmd"),
            "function_call_output must keep exec_command in-flight until exec_command_end"
        );

        let end = r#"{"timestamp":"2026-06-15T10:00:03Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"call_cmd","command":["/bin/bash","-lc","git status"],"aggregated_output":"ok\n","exit_code":0,"duration":{"secs":1,"nanos":234500000},"status":"completed"}}"#;
        process_line(
            end,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].1["status"], "running");
        assert_eq!(tool_calls[1].1["toolUseId"], "call_cmd");
        assert_eq!(tool_calls[1].1["status"], "done");
        assert!(in_flight.is_empty());
    }

    #[test]
    fn process_line_exec_command_end_marks_failed_exec_command() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let start = r#"{"timestamp":"2026-06-15T10:00:00Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call_cmd","arguments":"{\"cmd\":\"false\"}"}}"#;
        process_line(
            start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let output = r#"{"timestamp":"2026-06-15T10:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_cmd","output":"Chunk ID: abc\nWall time: 0.0100 seconds\nProcess exited with code 1\nOutput:\n"}}"#;
        process_line(
            output,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        assert!(
            in_flight.contains_key("call_cmd"),
            "function_call_output must keep exec_command in-flight until exec_command_end"
        );

        let end = r#"{"timestamp":"2026-06-15T10:00:03Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"call_cmd","command":["/bin/bash","-lc","false"],"aggregated_output":"","exit_code":1,"duration":{"secs":0,"nanos":10000000},"status":"completed"}}"#;
        process_line(
            end,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[1].1["toolUseId"], "call_cmd");
        assert_eq!(tool_calls[1].1["status"], "failed");
        assert!(in_flight.is_empty());
    }

    #[test]
    fn process_line_code_mode_exec_waits_for_exec_command_end() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let start = r#"{"timestamp":"2026-06-15T10:00:00Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_cmd","input":"const result = await tools.exec_command({ cmd: 'false' })"}}"#;
        process_line(
            start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let output = r#"{"timestamp":"2026-06-15T10:00:02Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_cmd","output":"Chunk ID: abc\nWall time: 0.0100 seconds\nProcess exited with code 1\nOutput:\n"}}"#;
        process_line(
            output,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        assert!(
            in_flight.contains_key("call_cmd"),
            "custom_tool_call_output must not finalize promoted code-mode exec_command"
        );

        let end = r#"{"timestamp":"2026-06-15T10:00:03Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"call_cmd","command":["/bin/bash","-lc","false"],"aggregated_output":"","exit_code":1,"duration":{"secs":0,"nanos":10000000},"status":"completed"}}"#;
        process_line(
            end,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].1["tool"], "exec_command");
        assert_eq!(tool_calls[0].1["args"], "false");
        assert_eq!(tool_calls[1].1["toolUseId"], "call_cmd");
        assert_eq!(tool_calls[1].1["tool"], "exec_command");
        assert_eq!(tool_calls[1].1["args"], "false");
        assert_eq!(tool_calls[1].1["status"], "failed");
        assert!(in_flight.is_empty());
    }

    #[test]
    fn process_line_function_call_output_without_exit_code_keeps_exec_command_pending() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let start = r#"{"timestamp":"2026-06-15T10:00:00Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call_cmd","arguments":"{\"cmd\":\"timeout\"}"}}"#;
        process_line(
            start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let output = r#"{"timestamp":"2026-06-15T10:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_cmd","output":"Chunk ID: abc\nWall time: 30.0000 seconds\nOutput:\n"}}"#;
        process_line(
            output,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        assert!(
            in_flight.contains_key("call_cmd"),
            "function_call_output without an exit-code line must not finalize exec_command"
        );

        let end = r#"{"timestamp":"2026-06-15T10:00:03Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"call_cmd","command":["/bin/bash","-lc","timeout"],"aggregated_output":"","exit_code":0,"duration":{"secs":30,"nanos":0},"status":"completed"}}"#;
        process_line(
            end,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[1].1["toolUseId"], "call_cmd");
        assert_eq!(tool_calls[1].1["status"], "done");
        assert!(in_flight.is_empty());
    }

    #[test]
    fn process_line_custom_tool_call_output_preserves_patch_apply_end_guard() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let start = r#"{"timestamp":"2026-06-15T10:00:00Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"call_patch","input":"*** Begin Patch\n*** Update File: foo.ts\n*** End Patch"}}"#;
        process_line(
            start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let output = r#"{"timestamp":"2026-06-15T10:00:02Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_patch","output":"done"}}"#;
        process_line(
            output,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls_after_output: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(
            tool_calls_after_output.len(),
            1,
            "custom_tool_call_output must not finalize apply_patch"
        );
        assert!(
            in_flight.contains_key("call_patch"),
            "apply_patch must stay in-flight until patch_apply_end"
        );

        let end = r#"{"timestamp":"2026-06-15T10:00:03Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"call_patch","success":false}}"#;
        process_line(
            end,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[1].1["toolUseId"], "call_patch");
        assert_eq!(tool_calls[1].1["status"], "failed");
        assert!(in_flight.is_empty());
    }

    #[test]
    fn process_line_code_mode_apply_patch_waits_for_patch_apply_end() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let start = r#"{"timestamp":"2026-06-15T10:00:00Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_patch","input":"await tools.apply_patch(\"*** Begin Patch\\n*** Update File: src/App.test.tsx\\n*** End Patch\")"}}"#;
        process_line(
            start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let output = r#"{"timestamp":"2026-06-15T10:00:02Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_patch","output":"done"}}"#;
        process_line(
            output,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls_after_output: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(
            tool_calls_after_output.len(),
            1,
            "custom_tool_call_output must not finalize code-mode apply_patch"
        );
        assert_eq!(tool_calls_after_output[0].1["tool"], "apply_patch");
        assert_eq!(tool_calls_after_output[0].1["args"], "src/App.test.tsx");
        assert_eq!(tool_calls_after_output[0].1["isTestFile"], true);
        assert!(
            in_flight.contains_key("call_patch"),
            "code-mode apply_patch must stay in-flight until patch_apply_end"
        );

        let end = r#"{"timestamp":"2026-06-15T10:00:03Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"call_patch","success":false}}"#;
        process_line(
            end,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[1].1["toolUseId"], "call_patch");
        assert_eq!(tool_calls[1].1["tool"], "apply_patch");
        assert_eq!(tool_calls[1].1["args"], "src/App.test.tsx");
        assert_eq!(tool_calls[1].1["status"], "failed");
        assert_eq!(tool_calls[1].1["isTestFile"], true);
        assert!(in_flight.is_empty());
    }

    #[test]
    fn process_line_task_complete_flushes_unfinished_exec_command() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let start = r#"{"timestamp":"2026-06-15T10:00:00Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call_cmd","arguments":"{\"cmd\":\"git status\"}"}}"#;
        process_line(
            start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let complete = r#"{"timestamp":"2026-06-15T10:00:03Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":3000}}"#;
        process_line(
            complete,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[1].1["toolUseId"], "call_cmd");
        assert_eq!(tool_calls[1].1["status"], "done");
        assert!(in_flight.is_empty());
    }

    #[test]
    fn process_line_session_meta_reset_drops_unfinished_exec_command() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;
        let mut codex_agent_session_id = String::new();

        let session_start = r#"{"timestamp":"2026-06-15T10:00:00Z","type":"session_meta","payload":{"id":"codex-old","cwd":"/workspace/A"}}"#;
        process_line(
            session_start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut codex_agent_session_id,
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let start = r#"{"timestamp":"2026-06-15T10:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call_cmd","arguments":"{\"cmd\":\"git status\"}"}}"#;
        process_line(
            start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut codex_agent_session_id,
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let session_reset = r#"{"timestamp":"2026-06-15T10:00:04Z","type":"session_meta","payload":{"id":"codex-new","cwd":"/workspace/A"}}"#;
        process_line(
            session_reset,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut codex_agent_session_id,
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(
            tool_calls.len(),
            1,
            "session reset must not emit a stale completion for the old run"
        );
        assert_eq!(tool_calls[0].1["toolUseId"], "call_cmd");
        assert_eq!(tool_calls[0].1["status"], "running");
        assert_eq!(codex_agent_session_id, "codex-new");
        assert!(in_flight.is_empty());
    }

    #[test]
    fn process_line_wrong_typed_success_emits_failed() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let start = r#"{"timestamp":"2026-05-22T10:00:00Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"c1","input":"*** Begin Patch\n*** Update File: foo.ts\n*** End Patch"}}"#;
        process_line(
            start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let end = r#"{"timestamp":"2026-05-22T10:00:01Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"c1","success":"bad"}}"#;
        process_line(
            end,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[1].1["status"], "failed");
    }

    #[test]
    fn process_line_duration_null_yields_zero_duration() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let start = r#"{"timestamp":"2026-05-22T10:00:00Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"ls\"}"}}"#;
        process_line(
            start,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let end = r#"{"timestamp":"2026-05-22T10:00:01Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"c1","exit_code":0,"duration":null}}"#;
        process_line(
            end,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let tool_calls: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-tool-call")
            .collect();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[1].1["durationMs"], 0);
    }

    #[test]
    fn process_line_non_string_timestamp_emits_not_drops() {
        let sink = Arc::new(FakeEventSink::new());
        let events: Arc<dyn EventSink> = sink.clone();
        let mut emitter = TestRunEmitter::new(events.clone());
        let mut in_flight = empty_in_flight();
        let mut num_turns = 0u32;
        let mut last_cwd: Option<String> = None;

        let line = r#"{"timestamp":42,"type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#;
        process_line(
            line,
            "sid",
            None,
            &events,
            &mut emitter,
            &mut in_flight,
            &mut num_turns,
            &mut last_cwd,
            &mut String::new(),
            &mut None,
            &mut None,
            &mut ReplayActivity::default(),
            true,
        );

        let turns: Vec<_> = sink
            .recorded()
            .into_iter()
            .filter(|(n, _)| n == "agent-turn")
            .collect();
        assert_eq!(turns.len(), 1);
    }

    // ---- end-to-end watcher test (with v2 regression guard inline) ----

    #[test]
    fn start_tailing_emits_cwd_transitions_in_order() {
        let sink = Arc::new(FakeEventSink::new());

        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript_path = tmp.path().join("rollout.jsonl");

        // 7 lines, 3 expected emissions:
        //   1. session_meta cwd=/workspace/A             (emit)
        //   2. turn_context cwd=/workspace/A             (no emit — extractor rejects turn_context)
        //   3. exec_command workdir=/workspace/B         (emit — transition)
        //   4. event_msg task_started                    (no emit)
        //   5. exec_command workdir=/workspace/B         (suppressed — same as last_cwd)
        //   6. turn_context cwd=/workspace/A             (no emit — REGRESSION GUARD: must not revert)
        //   7. exec_command workdir=/workspace/A         (emit — transition back)
        write_rollout(
            &transcript_path,
            &[
                json!({"timestamp":"2026-05-22T10:00:00Z","type":"session_meta","payload":{"id":"sid","cwd":"/workspace/A"}}),
                json!({"timestamp":"2026-05-22T10:00:01Z","type":"turn_context","payload":{"turn_id":"t1","cwd":"/workspace/A"}}),
                json!({"timestamp":"2026-05-22T10:00:02Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"ls\",\"workdir\":\"/workspace/B\"}"}}),
                json!({"timestamp":"2026-05-22T10:00:03Z","type":"event_msg","payload":{"type":"task_started"}}),
                json!({"timestamp":"2026-05-22T10:00:04Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c2","arguments":"{\"cmd\":\"ls\",\"workdir\":\"/workspace/B\"}"}}),
                json!({"timestamp":"2026-05-22T10:00:04.5Z","type":"turn_context","payload":{"turn_id":"t2","cwd":"/workspace/A"}}),
                json!({"timestamp":"2026-05-22T10:00:05Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c3","arguments":"{\"cmd\":\"ls\",\"workdir\":\"/workspace/A\"}"}}),
            ],
        );

        let handle = start_tailing(sink.clone(), "sid-cwd".to_string(), transcript_path, None)
            .expect("start tailing");

        // The whole transcript replays before catch-up, so the per-line
        // `agent-cwd` transitions are suppressed; the summary at the
        // replay→live boundary carries the FINAL cwd (the last transition,
        // /workspace/A). The transition ORDER + dedup semantics are covered
        // by the `process_line_*` tests (which drive with replay_done=true).
        assert!(
            sink.wait_for_count("agent-replay-summary", 1, Duration::from_secs(5)),
            "expected 1 agent-replay-summary event within 5s",
        );
        handle.stop();

        assert_eq!(
            sink.count("agent-cwd"),
            0,
            "replay cwd transitions are coalesced into the summary, not emitted individually",
        );

        let summary_payloads: Vec<Value> = sink
            .recorded()
            .into_iter()
            .filter(|(event, _)| event == "agent-replay-summary")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(summary_payloads.len(), 1);
        assert_eq!(summary_payloads[0]["sessionId"], "sid-cwd");
        assert_eq!(
            summary_payloads[0]["cwd"], "/workspace/A",
            "summary carries the final cwd after all replay transitions",
        );
    }
}
