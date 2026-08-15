//! Statusline JSON parser for Claude Code agent status.
//!
//! Step A-status of the v4-frozen refactor plan (#246) replaced the
//! former `serde_json::Value` pull-style extraction with typed DTOs
//! that flow through `#[derive(Deserialize)]`. Per-field
//! `Option<T> + #[serde(default)]` keeps the old "missing → default"
//! and "null → default" semantics; per-field
//! `deserialize_with = "...lenient_*"` keeps the old single-field
//! degradation on wrong-typed inputs (a JSON string where a `u64` was
//! expected no longer poisons the whole parse).

use serde::Deserialize;

use super::super::serde_helpers::{lenient_f64, lenient_object, lenient_string, lenient_u64};
use super::super::types::{stamp_snapshot, StatusSnapshot};
use crate::agent::types::{
    AgentStatusEvent, ContextWindowStatus, CostMetrics, CurrentUsage, RateLimitInfo, RateLimits,
};

/// Parsed statusline result wrapping the typed status event.
///
/// Step 0c (post-upsource review) dropped the former
/// `transcript_path: Option<String>` field — the watcher resolves the
/// transcript path independently via [`extract_transcript_path`]
/// (which `TranscriptPathSource::dynamic_hint` calls).
#[derive(Debug, Clone)]
pub struct ParsedStatusline {
    /// The typed agent status event.
    pub event: AgentStatusEvent,
}

// -------------------------- DTO layer --------------------------
//
// The DTOs below are the typed shape of Claude Code's statusline
// JSON. They are crate-private — production callers consume
// `AgentStatusEvent` via `parse_statusline`'s conversion path; tests
// reach behavior through the public entry points.
//
// Two invariants the DTO design preserves end-to-end:
// 1. `null` and missing are indistinguishable from the caller's view —
//    both produce `None` at the DTO level (`#[serde(default)]`), and
//    the conversion layer maps `None` to the same domain-specific
//    default the pull-style code used (`0` for counts, the computed
//    fallback for `used_percentage`, etc.).
// 2. A wrong-typed field (e.g. `"42"` for a `u64`) does NOT poison the
//    whole parse — `lenient_*` deserializers degrade the offending
//    field to `None` and let the rest of the document continue. This
//    mirrors the pre-A-status behavior of
//    `Value::as_u64().unwrap_or(0)` at every field.

#[derive(Deserialize, Default)]
struct ClaudeStatusDto {
    #[serde(default, deserialize_with = "lenient_string")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    version: Option<String>,
    // Nested struct fields use `lenient_object` so a wrong-typed
    // block (`"context_window": 42`, `"rate_limits": []`) degrades to
    // `None` and produces the per-block defaults the conversion layer
    // already encodes — instead of poisoning the whole `parse_statusline`
    // with an Err. Matches the pre-A-status `has_<block>(value)` /
    // `is_some_and(Value::is_object)` guards. Round-1 review fix
    // (Claude MEDIUM + codex connector P1).
    #[serde(default, deserialize_with = "lenient_object")]
    model: Option<ClaudeModelDto>,
    #[serde(default, deserialize_with = "lenient_object")]
    context_window: Option<ClaudeContextWindowDto>,
    #[serde(default, deserialize_with = "lenient_object")]
    cost: Option<ClaudeCostDto>,
    #[serde(default, deserialize_with = "lenient_object")]
    rate_limits: Option<ClaudeRateLimitsDto>,
}

#[derive(Deserialize, Default)]
struct ClaudeModelDto {
    #[serde(default, deserialize_with = "lenient_string")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    display_name: Option<String>,
}

#[derive(Deserialize, Default)]
struct ClaudeContextWindowDto {
    #[serde(default, deserialize_with = "lenient_f64")]
    used_percentage: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    remaining_percentage: Option<f64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    context_window_size: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    total_input_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    total_output_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient_object")]
    current_usage: Option<ClaudeCurrentUsageDto>,
}

#[derive(Deserialize, Default)]
struct ClaudeCurrentUsageDto {
    #[serde(default, deserialize_with = "lenient_u64")]
    input_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    output_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct ClaudeCostDto {
    #[serde(default, deserialize_with = "lenient_f64")]
    total_cost_usd: Option<f64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    total_duration_ms: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    total_api_duration_ms: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    total_lines_added: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    total_lines_removed: Option<u64>,
}

#[derive(Deserialize, Default)]
struct ClaudeRateLimitsDto {
    #[serde(default, deserialize_with = "lenient_object")]
    five_hour: Option<ClaudeRateLimitInfoDto>,
    #[serde(default, deserialize_with = "lenient_object")]
    seven_day: Option<ClaudeRateLimitInfoDto>,
}

#[derive(Deserialize, Default)]
struct ClaudeRateLimitInfoDto {
    #[serde(default, deserialize_with = "lenient_f64")]
    used_percentage: Option<f64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    resets_at: Option<u64>,
}

/// Narrow DTO for the transcript-path extractor — keeps it independent
/// of the full statusline shape so
/// `TranscriptPathSource::dynamic_hint` can call it without paying for
/// the full status decode. The JSON parse itself still runs (same
/// `serde_json::from_str` cost as `parse_statusline`), but this DTO
/// has a single field, so serde doesn't build the rest of the
/// `AgentStatusEvent` graph.
#[derive(Deserialize, Default)]
struct ClaudeTranscriptPathDto {
    #[serde(default, deserialize_with = "lenient_string")]
    transcript_path: Option<String>,
}

// ----------------------- Entry points -------------------------

/// `transcript_path` extractor — used by
/// `TranscriptPathSource::dynamic_hint` for the Claude adapter so the
/// watcher can resolve a fresh transcript path on every statusline
/// update.
///
/// Returns `None` for malformed JSON, a missing field, or any
/// non-string value at the `transcript_path` key — the same lenience
/// the pre-A-status pull-style code applied.
pub fn extract_transcript_path(raw: &str) -> Option<String> {
    serde_json::from_str::<ClaudeTranscriptPathDto>(raw)
        .ok()
        .and_then(|dto| dto.transcript_path)
}

/// Parse the raw statusline JSON into an [`AgentStatusEvent`].
///
/// Returns `Err` when the input is not valid JSON or its top-level
/// shape is not a JSON object (Step A-status: serde reports the latter
/// as `"invalid JSON: invalid type: ..., expected struct ..."` —
/// behavior preserved, error message text different from the previous
/// hand-written `"statusline JSON is not an object"`).
pub fn parse_statusline(session_id: &str, json: &str) -> Result<ParsedStatusline, String> {
    let snapshot = parse_statusline_snapshot(json)?;
    Ok(ParsedStatusline {
        event: stamp_snapshot(session_id, snapshot),
    })
}

/// Step B' decoder entry point — session-id-free. Used by the new
/// [`crate::agent::adapter::traits::StateDecoder`] impl on
/// `ClaudeCodeAdapter`. The runtime composes
/// `AgentStatusEvent { session_id, ...snapshot }` after the decoder
/// returns; for now the session-id-stamping `parse_statusline`
/// wrapper above is what `AgentAdapter::parse_status` still calls
/// (until B'' / D' migrate the parse-side runtime composition).
pub(crate) fn parse_statusline_snapshot(json: &str) -> Result<StatusSnapshot, String> {
    let dto: ClaudeStatusDto =
        serde_json::from_str(json).map_err(|e| format!("invalid JSON: {}", e))?;
    Ok(dto_to_snapshot(dto))
}

// ------------------ DTO → StatusSnapshot / AgentStatusEvent --------

fn dto_to_snapshot(dto: ClaudeStatusDto) -> StatusSnapshot {
    let model_id = dto
        .model
        .as_ref()
        .and_then(|m| m.id.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let model_display_name = dto
        .model
        .as_ref()
        .and_then(|m| m.display_name.clone())
        .unwrap_or_else(|| model_id.clone());

    StatusSnapshot {
        agent_session_id: dto.session_id.unwrap_or_default(),
        model_id,
        model_display_name,
        version: dto.version.unwrap_or_default(),
        context_window: context_window_from_dto(dto.context_window),
        cost: cost_from_dto(dto.cost),
        rate_limits: rate_limits_from_dto(dto.rate_limits),
        // Network-fetched-usage is a kimi-only concept.
        usage_fetched: false,
    }
}

fn context_window_from_dto(cw: Option<ClaudeContextWindowDto>) -> ContextWindowStatus {
    // Block-missing short-circuit preserves the pre-A-status default of
    // `remaining_percentage = 100.0` even when no context_window block
    // was emitted. Test pin:
    // `handles_missing_context_window`.
    let cw = match cw {
        Some(c) => c,
        None => {
            return ContextWindowStatus {
                used_percentage: None,
                remaining_percentage: 100.0,
                context_window_size: 0,
                total_input_tokens: 0,
                total_output_tokens: 0,
                current_usage: None,
            }
        }
    };

    let used_raw = cw.used_percentage;
    let context_window_size = cw.context_window_size.unwrap_or(0);
    let total_input_tokens = cw.total_input_tokens.unwrap_or(0);
    let total_output_tokens = cw.total_output_tokens.unwrap_or(0);

    // If `used_percentage` is null (Claude Code's loading phase: skills,
    // MCPs, CLAUDE.md), compute it from
    // `total_input_tokens / context_window_size`. Test pins:
    // `clamps_computed_context_window_percentage`,
    // `handles_null_used_percentage`.
    let computed_percentage = used_raw
        .or_else(|| {
            if context_window_size > 0 && total_input_tokens > 0 {
                Some((total_input_tokens as f64 / context_window_size as f64) * 100.0)
            } else {
                None
            }
        })
        .map(clamp_percentage);

    // `remaining_percentage` priority: computed-from-used → raw input →
    // 100.0 default. Test pins: `computes_remaining_from_used_when_missing`,
    // `clamps_raw_remaining_percentage_when_no_computed_fallback`.
    let computed_remaining = computed_percentage
        .map(|used| clamp_percentage(100.0 - used))
        .unwrap_or_else(|| clamp_percentage(cw.remaining_percentage.unwrap_or(100.0)));

    ContextWindowStatus {
        used_percentage: computed_percentage,
        remaining_percentage: computed_remaining,
        context_window_size,
        total_input_tokens,
        total_output_tokens,
        current_usage: cw.current_usage.map(|cu| CurrentUsage {
            input_tokens: cu.input_tokens.unwrap_or(0),
            output_tokens: cu.output_tokens.unwrap_or(0),
            cache_creation_input_tokens: cu.cache_creation_input_tokens.unwrap_or(0),
            cache_read_input_tokens: cu.cache_read_input_tokens.unwrap_or(0),
        }),
    }
}

fn cost_from_dto(cost: Option<ClaudeCostDto>) -> CostMetrics {
    // Block-missing → `total_cost_usd = None`. Block-present-but-field-
    // missing → `Some(0.0)`. This is a Claude-protocol distinction the
    // pull-style code encoded via `has_cost(...)`; preserved here with
    // the explicit `match` on `Option<ClaudeCostDto>`. Test pins:
    // `parse_cost_metrics_returns_none_when_cost_block_missing`,
    // `parse_cost_metrics_returns_some_zero_when_field_missing`.
    let cost = match cost {
        Some(c) => c,
        None => {
            return CostMetrics {
                total_cost_usd: None,
                total_duration_ms: 0,
                total_api_duration_ms: 0,
                total_lines_added: 0,
                total_lines_removed: 0,
            }
        }
    };

    CostMetrics {
        total_cost_usd: Some(cost.total_cost_usd.unwrap_or(0.0)),
        total_duration_ms: cost.total_duration_ms.unwrap_or(0),
        total_api_duration_ms: cost.total_api_duration_ms.unwrap_or(0),
        total_lines_added: cost.total_lines_added.unwrap_or(0),
        total_lines_removed: cost.total_lines_removed.unwrap_or(0),
    }
}

fn rate_limits_from_dto(rl: Option<ClaudeRateLimitsDto>) -> RateLimits {
    let rl = rl.unwrap_or_default();

    // Five-hour window defaults to (0.0, 0) when missing or null —
    // matches `parses_minimal_json`'s expectation for an empty input.
    let five_hour = rl
        .five_hour
        .map(rate_limit_info_from_dto)
        .unwrap_or(RateLimitInfo {
            used_percentage: 0.0,
            resets_at: 0,
        });

    // Seven-day window: `Some(_)` when present and non-null, `None`
    // otherwise. Test pin: `handles_null_seven_day_rate_limit`.
    RateLimits {
        five_hour,
        seven_day: rl.seven_day.map(rate_limit_info_from_dto),
    }
}

fn rate_limit_info_from_dto(dto: ClaudeRateLimitInfoDto) -> RateLimitInfo {
    RateLimitInfo {
        used_percentage: dto.used_percentage.unwrap_or(0.0),
        resets_at: dto.resets_at.unwrap_or(0),
    }
}

fn clamp_percentage(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full Claude Code statusline JSON sample
    fn sample_full_json() -> &'static str {
        r#"{
            "session_id": "abc-123",
            "version": "1.0.30",
            "model": {
                "id": "claude-sonnet-4-20250514",
                "display_name": "Claude Sonnet 4"
            },
            "context_window": {
                "used_percentage": 42.5,
                "remaining_percentage": 57.5,
                "context_window_size": 200000,
                "total_input_tokens": 85000,
                "total_output_tokens": 5000,
                "current_usage": {
                    "input_tokens": 1200,
                    "output_tokens": 300,
                    "cache_creation_input_tokens": 500,
                    "cache_read_input_tokens": 200
                }
            },
            "cost": {
                "total_cost_usd": 0.42,
                "total_duration_ms": 120000,
                "total_api_duration_ms": 45000,
                "total_lines_added": 150,
                "total_lines_removed": 30
            },
            "rate_limits": {
                "five_hour": {
                    "used_percentage": 15.3,
                    "resets_at": 1776081600
                },
                "seven_day": {
                    "used_percentage": 5.0,
                    "resets_at": 1776340800
                }
            },
            "transcript_path": "/home/user/.claude/sessions/abc-123/transcript.jsonl"
        }"#
    }

    #[test]
    fn parses_full_statusline_json() {
        let result = parse_statusline("pty-1", sample_full_json());
        assert!(result.is_ok());

        let parsed = result.unwrap();
        let event = &parsed.event;

        assert_eq!(event.session_id, "pty-1");
        assert_eq!(event.agent_session_id, "abc-123");
        assert_eq!(event.model_id, "claude-sonnet-4-20250514");
        assert_eq!(event.model_display_name, "Claude Sonnet 4");
        assert_eq!(event.version, "1.0.30");

        // Context window
        assert_eq!(event.context_window.used_percentage, Some(42.5));
        assert!((event.context_window.remaining_percentage - 57.5).abs() < f64::EPSILON);
        assert_eq!(event.context_window.context_window_size, 200000);
        assert_eq!(event.context_window.total_input_tokens, 85000);
        assert_eq!(event.context_window.total_output_tokens, 5000);

        let cu = event.context_window.current_usage.as_ref().unwrap();
        assert_eq!(cu.input_tokens, 1200);
        assert_eq!(cu.output_tokens, 300);
        assert_eq!(cu.cache_creation_input_tokens, 500);
        assert_eq!(cu.cache_read_input_tokens, 200);

        // Cost
        assert_eq!(event.cost.total_cost_usd, Some(0.42));
        assert_eq!(event.cost.total_duration_ms, 120000);
        assert_eq!(event.cost.total_api_duration_ms, 45000);
        assert_eq!(event.cost.total_lines_added, 150);
        assert_eq!(event.cost.total_lines_removed, 30);

        // Rate limits
        assert!((event.rate_limits.five_hour.used_percentage - 15.3).abs() < f64::EPSILON);
        assert_eq!(event.rate_limits.five_hour.resets_at, 1776081600);
        let seven_day = event.rate_limits.seven_day.as_ref().unwrap();
        assert!((seven_day.used_percentage - 5.0).abs() < f64::EPSILON);

        // Transcript path: pinned separately via
        // `extract_transcript_path` tests below — `parse_statusline`
        // no longer surfaces it as of Step 0c.
    }

    #[test]
    fn parses_minimal_json() {
        let json = r#"{}"#;
        let result = parse_statusline("pty-2", json);
        assert!(result.is_ok());

        let parsed = result.unwrap();
        let event = &parsed.event;

        assert_eq!(event.session_id, "pty-2");
        assert_eq!(event.agent_session_id, "");
        assert_eq!(event.model_id, "unknown");
        assert_eq!(event.model_display_name, "unknown");
        assert_eq!(event.version, "");
        assert_eq!(event.context_window.used_percentage, None);
        assert!((event.context_window.remaining_percentage - 100.0).abs() < f64::EPSILON);
        assert_eq!(event.context_window.context_window_size, 0);
        assert!(event.context_window.current_usage.is_none());
        assert_eq!(event.cost.total_cost_usd, None);
    }

    #[test]
    fn parse_cost_metrics_returns_none_when_cost_block_missing() {
        let json = r#"{}"#;
        let result = parse_statusline("pty-cost-none", json).expect("empty object parses");
        assert_eq!(result.event.cost.total_cost_usd, None);
    }

    #[test]
    fn parse_cost_metrics_returns_some_zero_when_field_missing() {
        let json = r#"{ "cost": { "total_duration_ms": 100 } }"#;
        let result =
            parse_statusline("pty-cost-zero", json).expect("cost block without field parses");
        assert_eq!(result.event.cost.total_cost_usd, Some(0.0));
    }

    #[test]
    fn parse_cost_metrics_returns_some_value_when_field_present() {
        let json = r#"{ "cost": { "total_cost_usd": 0.42 } }"#;
        let result =
            parse_statusline("pty-cost-value", json).expect("cost block with field parses");
        assert_eq!(result.event.cost.total_cost_usd, Some(0.42));
    }

    #[test]
    fn handles_null_used_percentage() {
        let json = r#"{
            "context_window": {
                "used_percentage": null,
                "remaining_percentage": 100.0,
                "context_window_size": 200000,
                "total_input_tokens": 0,
                "total_output_tokens": 0
            }
        }"#;
        let result = parse_statusline("pty-3", json);
        assert!(result.is_ok());

        let event = &result.unwrap().event;
        assert_eq!(event.context_window.used_percentage, None);
        assert!((event.context_window.remaining_percentage - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn handles_missing_context_window() {
        let json = r#"{"model": {"id": "claude-opus-4-20250514"}}"#;
        let result = parse_statusline("pty-4", json);
        assert!(result.is_ok());

        let event = &result.unwrap().event;
        assert_eq!(event.model_id, "claude-opus-4-20250514");
        assert_eq!(event.context_window.used_percentage, None);
        assert_eq!(event.context_window.context_window_size, 0);
    }

    #[test]
    fn handles_null_seven_day_rate_limit() {
        let json = r#"{
            "rate_limits": {
                "five_hour": {
                    "used_percentage": 10.0,
                    "resets_at": 1776081600
                },
                "seven_day": null
            }
        }"#;
        let result = parse_statusline("pty-5", json);
        assert!(result.is_ok());

        let event = &result.unwrap().event;
        assert!(event.rate_limits.seven_day.is_none());
        assert!((event.rate_limits.five_hour.used_percentage - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_invalid_json() {
        let result = parse_statusline("pty-6", "not json at all");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid JSON"));
    }

    #[test]
    fn rejects_non_object_json() {
        // Step A-status: typed-DTO migration dropped the hand-written
        // `is_object` precheck. A non-object top-level value now fails
        // at serde's `expected struct` step rather than at our former
        // `"not an object"` Err. Behavior preserved (Err), error text
        // changes from `"statusline JSON is not an object"` to
        // `"invalid JSON: invalid type: integer ..., expected struct
        // ClaudeStatusDto"`. No production caller string-matches on
        // either form; the assertion below pins the `invalid JSON`
        // prefix we still own.
        let result = parse_statusline("pty-7", "42");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("invalid JSON"),
            "expected the `invalid JSON:` prefix, got: {}",
            err
        );
    }

    #[test]
    fn handles_partial_cost_fields() {
        let json = r#"{
            "cost": {
                "total_cost_usd": 1.23
            }
        }"#;
        let result = parse_statusline("pty-8", json);
        assert!(result.is_ok());

        let event = &result.unwrap().event;
        assert_eq!(event.cost.total_cost_usd, Some(1.23));
        assert_eq!(event.cost.total_duration_ms, 0);
        assert_eq!(event.cost.total_api_duration_ms, 0);
        assert_eq!(event.cost.total_lines_added, 0);
        assert_eq!(event.cost.total_lines_removed, 0);
    }

    #[test]
    fn computes_remaining_from_used_when_missing() {
        let json = r#"{
            "context_window": {
                "used_percentage": 30.0,
                "context_window_size": 200000,
                "total_input_tokens": 60000,
                "total_output_tokens": 0
            }
        }"#;
        let result = parse_statusline("pty-9", json);
        assert!(result.is_ok());

        let event = &result.unwrap().event;
        assert_eq!(event.context_window.used_percentage, Some(30.0));
        assert!((event.context_window.remaining_percentage - 70.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clamps_context_window_percentages() {
        let json = r#"{
            "context_window": {
                "used_percentage": 150.0,
                "remaining_percentage": -50.0,
                "context_window_size": 200000,
                "total_input_tokens": 300000,
                "total_output_tokens": 0
            }
        }"#;
        let result = parse_statusline("pty-clamp", json);
        assert!(result.is_ok());

        let event = &result.unwrap().event;
        assert_eq!(event.context_window.used_percentage, Some(100.0));
        assert!((event.context_window.remaining_percentage - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clamps_computed_context_window_percentage() {
        let json = r#"{
            "context_window": {
                "used_percentage": null,
                "context_window_size": 200000,
                "total_input_tokens": 300000,
                "total_output_tokens": 0
            }
        }"#;
        let result = parse_statusline("pty-computed-clamp", json);
        assert!(result.is_ok());

        let event = &result.unwrap().event;
        assert_eq!(event.context_window.used_percentage, Some(100.0));
        assert!((event.context_window.remaining_percentage - 0.0).abs() < f64::EPSILON);
    }

    /// F13 regression (Claude review on PR #153). When `used_percentage`
    /// is null AND counts are zero (no computed fallback), the
    /// `unwrap_or_else(|| clamp_percentage(remaining_percentage))` raw
    /// fallback path takes over. This is the path most likely to receive
    /// adversarial / partial-flush data, so it must clamp negatives and
    /// over-100 values to the [0, 100] range.
    #[test]
    fn clamps_raw_remaining_percentage_when_no_computed_fallback() {
        // Negative `remaining_percentage` with no computed value (null
        // `used_percentage` AND zero counts) → must clamp to 0.0.
        let json_below = r#"{
            "context_window": {
                "used_percentage": null,
                "remaining_percentage": -50.0,
                "context_window_size": 0,
                "total_input_tokens": 0,
                "total_output_tokens": 0
            }
        }"#;
        let event = &parse_statusline("pty-raw-below", json_below)
            .expect("parse_statusline should succeed")
            .event;
        assert_eq!(event.context_window.used_percentage, None);
        assert!(
            (event.context_window.remaining_percentage - 0.0).abs() < f64::EPSILON,
            "negative raw remaining_percentage must clamp to 0.0, got {}",
            event.context_window.remaining_percentage
        );

        // Over-100 `remaining_percentage` on the same fallback path →
        // must clamp to 100.0.
        let json_above = r#"{
            "context_window": {
                "used_percentage": null,
                "remaining_percentage": 150.0,
                "context_window_size": 0,
                "total_input_tokens": 0,
                "total_output_tokens": 0
            }
        }"#;
        let event = &parse_statusline("pty-raw-above", json_above)
            .expect("parse_statusline should succeed")
            .event;
        assert_eq!(event.context_window.used_percentage, None);
        assert!(
            (event.context_window.remaining_percentage - 100.0).abs() < f64::EPSILON,
            "over-100 raw remaining_percentage must clamp to 100.0, got {}",
            event.context_window.remaining_percentage
        );
    }

    #[test]
    fn model_display_name_falls_back_to_id() {
        let json = r#"{"model": {"id": "claude-opus-4-20250514"}}"#;
        let result = parse_statusline("pty-10", json);
        assert!(result.is_ok());

        let event = &result.unwrap().event;
        assert_eq!(event.model_id, "claude-opus-4-20250514");
        assert_eq!(event.model_display_name, "claude-opus-4-20250514");
    }

    /// Round-1 upsource fix: a wrong-typed nested block (e.g.
    /// `"context_window": 42`) must degrade to the per-block defaults
    /// instead of poisoning the whole `parse_statusline` call. This
    /// pins the `lenient_object` round of the helpers: before that
    /// helper landed, this input made the entire status update an
    /// `Err` and the watcher dropped the event.
    #[test]
    fn wrong_typed_nested_blocks_degrade_to_defaults_per_block() {
        let json = r#"{
            "session_id": "abc",
            "model": {"id": "claude-x", "display_name": "X"},
            "context_window": 42,
            "cost": {"total_cost_usd": 1.5}
        }"#;
        let event = &parse_statusline("pty-wrong-ctx", json)
            .expect("wrong-typed nested block must NOT poison parse")
            .event;
        assert_eq!(event.agent_session_id, "abc");
        assert_eq!(event.model_id, "claude-x");
        // context_window block lost → all defaults.
        assert_eq!(event.context_window.used_percentage, None);
        assert_eq!(event.context_window.context_window_size, 0);
        assert!((event.context_window.remaining_percentage - 100.0).abs() < f64::EPSILON);
        // cost block survived independently.
        assert_eq!(event.cost.total_cost_usd, Some(1.5));
    }

    /// Companion: wrong-typed `cost` block should degrade
    /// `cost.total_cost_usd` to `None` (block-absent semantics —
    /// distinct from `Some(0.0)` which only fires when the block IS
    /// present but the inner field is missing).
    #[test]
    fn wrong_typed_cost_block_degrades_to_none() {
        let json = r#"{"cost": ["not", "an", "object"]}"#;
        let event = &parse_statusline("pty-wrong-cost", json)
            .expect("wrong-typed cost block must NOT poison parse")
            .event;
        assert_eq!(event.cost.total_cost_usd, None);
        assert_eq!(event.cost.total_duration_ms, 0);
    }

    /// Companion: wrong-typed `rate_limits` + nested `current_usage`.
    /// Pins that `lenient_object` is applied at both the top-level
    /// `ClaudeStatusDto.rate_limits` and the nested
    /// `ClaudeContextWindowDto.current_usage` positions.
    #[test]
    fn wrong_typed_rate_limits_and_current_usage_degrade() {
        let json = r#"{
            "context_window": {
                "used_percentage": 25.0,
                "context_window_size": 100,
                "total_input_tokens": 25,
                "current_usage": "garbage"
            },
            "rate_limits": 99
        }"#;
        let event = &parse_statusline("pty-wrong-rl-cu", json)
            .expect("wrong-typed nested blocks must NOT poison parse")
            .event;
        assert_eq!(event.context_window.used_percentage, Some(25.0));
        assert!(event.context_window.current_usage.is_none());
        assert_eq!(event.rate_limits.five_hour.used_percentage, 0.0);
        assert_eq!(event.rate_limits.five_hour.resets_at, 0);
        assert!(event.rate_limits.seven_day.is_none());
    }

    /// Step 0c (post-upsource cycle 2): replaces the deleted
    /// `parsed.transcript_path` assertions in `parses_full_statusline_json`
    /// + `parses_minimal_json`. The semantic coverage migrates from
    /// `parse_statusline`'s dropped field to the standalone
    /// `extract_transcript_path` helper. The full live-path contract
    /// (Claude adapter → `TranscriptPathSource::dynamic_hint`) is
    /// covered in `claude_code/mod.rs::dynamic_hint_extracts_transcript_path_when_present`.
    #[test]
    fn extract_transcript_path_returns_field_when_present_else_none() {
        let with_path =
            r#"{"transcript_path": "/home/u/.claude/sessions/x.jsonl", "model": {"id": "x"}}"#;
        assert_eq!(
            extract_transcript_path(with_path).as_deref(),
            Some("/home/u/.claude/sessions/x.jsonl"),
        );

        let without_path = r#"{"model": {"id": "x"}}"#;
        assert_eq!(extract_transcript_path(without_path), None);

        // Lenient on malformed JSON — returns None instead of panicking.
        let malformed = r#"{not valid json"#;
        assert_eq!(extract_transcript_path(malformed), None);
    }
}
