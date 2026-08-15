//! Parser for Codex rollout JSONL files.
//!
//! Step A-status of the v4-frozen refactor plan (#246) replaced the
//! former `serde_json::Value` pull-style fold with typed DTOs. Each
//! rollout line deserializes through an internally-tagged enum
//! (`CodexRolloutLine` on the outer `type` discriminator), then a
//! second internally-tagged enum for `event_msg.payload` sub-types.
//! Per-field `Option<T> + #[serde(default)]` + `lenient_*`
//! deserializers preserve the pre-A-status leniency: a wrong-typed
//! field or an unknown event tag does NOT poison the rest of the
//! document.
//!
//! The two load-bearing invariants from the v4-frozen plan are
//! structurally encoded by the DTOs:
//!
//! 1. **`token_count.info: Option<TokenCountInfoDto>`** — the fold
//!    updates `state.last_token_count_info` ONLY when the deserialized
//!    `info` is `Some(_)`. A `token_count` event with `info: null` or
//!    `info: <missing>` preserves prior context (the
//!    `token_count_info_null_preserves_prior_context` regression
//!    test pins this).
//!
//! 2. **Unknown event tags fall through to `#[serde(other)]`** —
//!    silently ignored, never an Err. The pre-A-status `Value`-based
//!    matcher had the same behavior; the test
//!    `unknown_event_type_ignored_without_error` pins it.

use serde::Deserialize;

use super::super::serde_helpers::{lenient_f64, lenient_object, lenient_string, lenient_u64};
use crate::agent::adapter::types::StatusSnapshot;
#[cfg(test)]
use crate::agent::adapter::types::{stamp_snapshot, ParsedStatus};
use crate::agent::types::{
    ContextWindowStatus, CostMetrics, CurrentUsage, RateLimitInfo, RateLimits,
};

/// Test-only convenience wrapper that pairs `parse_rollout_snapshot`
/// with `stamp_snapshot` so existing tests can keep asserting on the
/// full `AgentStatusEvent` shape (including `session_id`). Production
/// code (the `StateDecoder` impl on `CodexAdapter`) calls
/// `parse_rollout_snapshot` directly and lets the runtime stamp the
/// session id via `stamp_snapshot`.
#[cfg(test)]
pub(crate) fn parse_rollout(session_id: &str, raw: &str) -> Result<ParsedStatus, String> {
    let snapshot = parse_rollout_snapshot(Some(session_id), raw)?;
    Ok(ParsedStatus {
        event: stamp_snapshot(session_id, snapshot),
    })
}

/// Step B' decoder entry point — session-id-free *output*. Used by
/// the new [`crate::agent::adapter::traits::StateDecoder`] impl on
/// `CodexAdapter`. The runtime composes
/// `AgentStatusEvent { session_id, ...snapshot }` after the decoder
/// returns; for now the session-id-stamping `parse_rollout` wrapper
/// above is what `AgentAdapter::parse_status` still calls.
///
/// `session_id` is **diagnostic-only**: it's used to label the
/// per-line `log::warn!` for malformed JSONL lines so multi-session
/// debugging can correlate the warning to the affected PTY session.
/// It does NOT influence the returned `StatusSnapshot` (R2.2 invariant
/// — output is identity-free). PR #261 cycle 2 review flagged that
/// the pre-B' parser logged `for sid={session_id}` and the refactor
/// stripped that context; this parameter restores it without
/// re-introducing the id into the output type.
pub(crate) fn parse_rollout_snapshot(
    session_id: Option<&str>,
    raw: &str,
) -> Result<StatusSnapshot, String> {
    let mut state = CodexFoldState::default();
    let lines: Vec<&str> = raw.split('\n').collect();
    let trailing_complete = raw.ends_with('\n');

    for (idx, line) in lines.iter().enumerate() {
        let is_last = idx + 1 == lines.len();
        if line.is_empty() {
            continue;
        }
        if is_last && !trailing_complete {
            continue;
        }

        // Per-line deserialize: a single malformed line is logged and
        // skipped; the rest of the document continues. Test pin:
        // `malformed_mid_line_skipped_with_warn`.
        match serde_json::from_str::<CodexRolloutLine>(line) {
            Ok(parsed) => fold_event(&mut state, parsed),
            Err(_) => log::warn!(
                "codex: skipping malformed rollout line (sid={})",
                session_id.unwrap_or("?"),
            ),
        }
    }

    Ok(state.into_snapshot())
}

// -------------------------- DTO layer --------------------------

/// One JSONL line from the rollout stream. The outer
/// `#[serde(tag = "type")]` dispatches on the `"type"` discriminator;
/// `#[serde(other)]` catches unknown / forward-compatible tags
/// without erroring.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexRolloutLine {
    SessionMeta {
        #[serde(default)]
        payload: Option<SessionMetaPayloadDto>,
    },
    TurnContext {
        #[serde(default)]
        payload: Option<TurnContextPayloadDto>,
    },
    EventMsg {
        #[serde(default)]
        payload: Option<EventMsgPayloadDto>,
    },
    /// Forward-compatible catch-all for unknown event kinds (and the
    /// existing `response_item`, `user_message`, etc. shapes the
    /// status decoder doesn't fold). `#[serde(other)]` requires a
    /// unit variant — by design.
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Default)]
struct SessionMetaPayloadDto {
    #[serde(default, deserialize_with = "lenient_string")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    cli_version: Option<String>,
}

#[derive(Deserialize, Default)]
struct TurnContextPayloadDto {
    #[serde(default, deserialize_with = "lenient_string")]
    model: Option<String>,
}

/// `event_msg.payload` is itself an internally-tagged variant. The
/// status decoder cares about three of these (`task_started`,
/// `task_complete`, `token_count`); everything else falls through to
/// `Unknown`.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EventMsgPayloadDto {
    TaskStarted {
        #[serde(default, deserialize_with = "lenient_u64")]
        model_context_window: Option<u64>,
    },
    TaskComplete {
        #[serde(default, deserialize_with = "lenient_u64")]
        duration_ms: Option<u64>,
    },
    TokenCount {
        /// Critical for the v4-frozen plan's R2.4 invariant: when this
        /// is `None` (either field missing OR JSON null OR wrong-typed),
        /// the fold MUST NOT clear `state.last_token_count_info`. The
        /// regression test `token_count_info_null_preserves_prior_context`
        /// fails if a future contributor changes the fold to overwrite
        /// on `None`. `lenient_object` (round-1 fix) widens "None" to
        /// also include the wrong-typed case (e.g. `"info": 1`) — a
        /// single malformed sub-block no longer kills the whole
        /// `event_msg` line. Connector P2 round-1 finding.
        #[serde(default, deserialize_with = "lenient_object")]
        info: Option<TokenCountInfoDto>,
        #[serde(default, deserialize_with = "lenient_object")]
        rate_limits: Option<RateLimitsPayloadDto>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Default)]
struct TokenCountInfoDto {
    #[serde(default, deserialize_with = "lenient_object")]
    last_token_usage: Option<LastTokenUsageDto>,
    #[serde(default, deserialize_with = "lenient_u64")]
    model_context_window: Option<u64>,
}

#[derive(Deserialize, Default)]
struct LastTokenUsageDto {
    #[serde(default, deserialize_with = "lenient_u64")]
    input_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    output_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    cached_input_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    total_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct RateLimitsPayloadDto {
    #[serde(default, deserialize_with = "lenient_object")]
    primary: Option<RateLimitWindowDto>,
    #[serde(default, deserialize_with = "lenient_object")]
    secondary: Option<RateLimitWindowDto>,
}

#[derive(Deserialize, Default)]
struct RateLimitWindowDto {
    #[serde(default, deserialize_with = "lenient_f64")]
    used_percent: Option<f64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    resets_at: Option<u64>,
}

// -------------------------- Fold state --------------------------

#[derive(Default)]
struct CodexFoldState {
    agent_session_id: String,
    cli_version: String,
    model: String,
    last_task_started_context_window: Option<u64>,
    last_token_count_info: Option<TokenCountInfo>,
    last_rate_limits: Option<RateLimits>,
    total_duration_ms: u64,
}

#[derive(Clone)]
struct TokenCountInfo {
    last_input_tokens: u64,
    last_output_tokens: u64,
    last_cached_input_tokens: u64,
    last_total_tokens: u64,
    model_context_window: u64,
}

impl CodexFoldState {
    /// Step B': renamed from `into_event(session_id)` to
    /// `into_snapshot()` — the session-id stamp moved out of the
    /// decoder per the v4-frozen plan's R2.2 invariant. Composition
    /// happens in `stamp_snapshot` (called directly by
    /// `parse_rollout` for the still-existing
    /// `AgentAdapter::parse_status` path).
    fn into_snapshot(self) -> StatusSnapshot {
        let context_window_size = self
            .last_token_count_info
            .as_ref()
            .map(|info| info.model_context_window)
            .or(self.last_task_started_context_window)
            .unwrap_or(0);

        let used_percentage = self.last_token_count_info.as_ref().and_then(|info| {
            if context_window_size == 0 {
                None
            } else {
                Some(clamp_percentage(
                    (info.last_total_tokens as f64 / context_window_size as f64) * 100.0,
                ))
            }
        });

        let remaining_percentage = used_percentage
            .map(|used| clamp_percentage(100.0 - used))
            .unwrap_or(100.0);

        let context_window = ContextWindowStatus {
            used_percentage,
            remaining_percentage,
            context_window_size,
            total_input_tokens: self
                .last_token_count_info
                .as_ref()
                .map(|info| info.last_input_tokens)
                .unwrap_or(0),
            total_output_tokens: self
                .last_token_count_info
                .as_ref()
                .map(|info| info.last_output_tokens)
                .unwrap_or(0),
            current_usage: self
                .last_token_count_info
                .as_ref()
                .map(|info| CurrentUsage {
                    input_tokens: info.last_input_tokens,
                    output_tokens: info.last_output_tokens,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: info.last_cached_input_tokens,
                }),
        };

        let cost = CostMetrics {
            total_cost_usd: None,
            total_duration_ms: self.total_duration_ms,
            total_api_duration_ms: 0,
            total_lines_added: 0,
            total_lines_removed: 0,
        };

        let rate_limits = self.last_rate_limits.unwrap_or(RateLimits {
            five_hour: RateLimitInfo {
                used_percentage: 0.0,
                resets_at: 0,
            },
            seven_day: None,
        });

        let (model_id, model_display_name) = if self.model.is_empty() {
            ("unknown".to_string(), "unknown".to_string())
        } else {
            (self.model.clone(), self.model)
        };

        StatusSnapshot {
            agent_session_id: self.agent_session_id,
            model_id,
            model_display_name,
            version: self.cli_version,
            context_window,
            cost,
            rate_limits,
            // Network-fetched-usage is a kimi-only concept.
            usage_fetched: false,
        }
    }
}

fn fold_event(state: &mut CodexFoldState, line: CodexRolloutLine) {
    match line {
        CodexRolloutLine::SessionMeta { payload } => {
            if let Some(p) = payload {
                if let Some(id) = p.id.filter(|id| !id.is_empty()) {
                    if !state.agent_session_id.is_empty() && state.agent_session_id != id {
                        *state = CodexFoldState::default();
                    }
                    state.agent_session_id = id;
                }
                if let Some(version) = p.cli_version {
                    state.cli_version = version;
                }
            }
        }
        CodexRolloutLine::TurnContext { payload } => {
            if let Some(p) = payload {
                if let Some(model) = p.model {
                    state.model = model;
                }
            }
        }
        CodexRolloutLine::EventMsg { payload } => match payload {
            Some(EventMsgPayloadDto::TaskStarted {
                model_context_window,
            }) => {
                if let Some(size) = model_context_window {
                    state.last_task_started_context_window = Some(size);
                }
            }
            Some(EventMsgPayloadDto::TaskComplete { duration_ms }) => {
                if let Some(ms) = duration_ms {
                    state.total_duration_ms = state.total_duration_ms.saturating_add(ms);
                }
            }
            Some(EventMsgPayloadDto::TokenCount { info, rate_limits }) => {
                // R2.4 invariant: Some-only update. A `token_count`
                // event whose `info` is null / missing preserves prior
                // state. Test pin:
                // `token_count_info_null_preserves_prior_context`.
                if let Some(info) = info {
                    state.last_token_count_info = Some(token_count_info_from_dto(info));
                }
                if let Some(rl) = rate_limits {
                    state.last_rate_limits = Some(rate_limits_from_dto(rl));
                }
            }
            Some(EventMsgPayloadDto::Unknown) | None => {}
        },
        CodexRolloutLine::Unknown => {}
    }
}

fn token_count_info_from_dto(dto: TokenCountInfoDto) -> TokenCountInfo {
    let last = dto.last_token_usage.unwrap_or_default();
    TokenCountInfo {
        last_input_tokens: last.input_tokens.unwrap_or(0),
        last_output_tokens: last.output_tokens.unwrap_or(0),
        last_cached_input_tokens: last.cached_input_tokens.unwrap_or(0),
        last_total_tokens: last.total_tokens.unwrap_or(0),
        model_context_window: dto.model_context_window.unwrap_or(0),
    }
}

fn rate_limits_from_dto(dto: RateLimitsPayloadDto) -> RateLimits {
    // Pre-A-status default behavior: missing/null `primary` yields a
    // (0.0, 0) `RateLimitInfo`. The DTO field is `Option<...>` so
    // `unwrap_or_default()` gives us the same shape.
    let primary = dto.primary.unwrap_or_default();
    let five_hour = RateLimitInfo {
        used_percentage: primary.used_percent.unwrap_or(0.0),
        resets_at: primary.resets_at.unwrap_or(0),
    };

    // `secondary` distinguishes null/missing → `None` from
    // present-with-fields → `Some(_)`. The DTO's
    // `secondary: Option<RateLimitWindowDto>` does that directly.
    let seven_day = dto.secondary.map(|s| RateLimitInfo {
        used_percentage: s.used_percent.unwrap_or(0.0),
        resets_at: s.resets_at.unwrap_or(0),
    });

    RateLimits {
        five_hour,
        seven_day,
    }
}

fn clamp_percentage(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex")
            .join(name);

        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {}", path.display(), e))
    }

    #[test]
    fn parses_minimal_single_turn() {
        let raw = fixture("rollout-minimal.jsonl");
        let parsed = parse_rollout("pty-test", &raw).expect("happy path");
        let event = parsed.event;

        assert_eq!(event.session_id, "pty-test");
        assert_eq!(
            event.agent_session_id,
            "019defd8-15a1-7401-9f4f-40fe52a1c590"
        );
        assert_eq!(event.model_id, "gpt-5.4");
        assert_eq!(event.model_display_name, "gpt-5.4");
        assert_eq!(event.version, "0.128.0");
        assert_eq!(event.context_window.context_window_size, 258400);
        assert_eq!(event.context_window.total_input_tokens, 52685);
        assert_eq!(event.context_window.total_output_tokens, 2177);

        let used = event
            .context_window
            .used_percentage
            .expect("used percentage");
        assert!(
            (used - 21.23).abs() < 0.5,
            "used_percentage near 21%, got {}",
            used
        );

        let current_usage = event
            .context_window
            .current_usage
            .expect("current usage should be present");
        assert_eq!(current_usage.cache_read_input_tokens, 51584);
        assert_eq!(current_usage.cache_creation_input_tokens, 0);

        assert_eq!(event.cost.total_cost_usd, None);
        assert_eq!(event.cost.total_duration_ms, 208025);
        assert_eq!(event.cost.total_api_duration_ms, 0);
        assert!((event.rate_limits.five_hour.used_percentage - 8.0).abs() < f64::EPSILON);
        assert!(
            (event
                .rate_limits
                .seven_day
                .expect("seven day limit")
                .used_percentage
                - 26.0)
                .abs()
                < f64::EPSILON
        );
        // Step 0c: `transcript_path` was removed from `ParsedStatus`.
        // The path is now reached via `TranscriptPathSource::static_hint`
        // — exercised by the adapter-level tests in `codex/mod.rs`.
    }

    #[test]
    fn long_session_uses_last_token_usage_not_lifetime() {
        let raw = fixture("rollout-long-session.jsonl");
        let parsed = parse_rollout("pty-long", &raw).expect("long session");
        let event = parsed.event;
        let used = event
            .context_window
            .used_percentage
            .expect("used percentage");

        assert!(
            (used - 20.94).abs() < 0.5,
            "used_percentage must reflect last_token_usage, got {}",
            used
        );
        assert!(used < 100.0);
        assert_eq!(event.context_window.total_input_tokens, 52000);
        assert_eq!(event.context_window.total_output_tokens, 2100);
    }

    #[test]
    fn token_count_info_null_preserves_prior_context() {
        let raw = fixture("rollout-info-null.jsonl");
        let parsed = parse_rollout("pty-info-null", &raw).expect("info-null");
        let event = parsed.event;

        assert_eq!(event.context_window.total_input_tokens, 1000);
        assert_eq!(event.context_window.total_output_tokens, 200);
        assert!((event.rate_limits.five_hour.used_percentage - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn new_session_meta_resets_run_scoped_status() {
        let raw = r#"{"timestamp":"...","type":"session_meta","payload":{"id":"old-run","cli_version":"0.139.0"}}
{"timestamp":"...","type":"turn_context","payload":{"model":"gpt-old"}}
{"timestamp":"...","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":9000,"output_tokens":1000,"cached_input_tokens":7000,"total_tokens":10000},"model_context_window":20000}}}
{"timestamp":"...","type":"event_msg","payload":{"type":"task_complete","duration_ms":5000}}
{"timestamp":"...","type":"session_meta","payload":{"id":"new-run","cli_version":"0.139.0"}}
{"timestamp":"...","type":"turn_context","payload":{"model":"gpt-new"}}
{"timestamp":"...","type":"event_msg","payload":{"type":"task_started","model_context_window":258000}}
"#;

        let parsed = parse_rollout("pty-clear", raw).expect("clear reset");
        let event = parsed.event;

        assert_eq!(event.agent_session_id, "new-run");
        assert_eq!(event.model_id, "gpt-new");
        assert_eq!(event.context_window.context_window_size, 258000);
        assert_eq!(event.context_window.total_input_tokens, 0);
        assert_eq!(event.context_window.total_output_tokens, 0);
        assert!(event.context_window.current_usage.is_none());
        assert_eq!(event.context_window.used_percentage, None);
        assert_eq!(event.cost.total_duration_ms, 0);
    }

    #[test]
    fn multi_turn_sums_durations() {
        let raw = fixture("rollout-multi-turn.jsonl");
        let parsed = parse_rollout("pty-multi", &raw).expect("multi-turn");
        assert_eq!(parsed.event.cost.total_duration_ms, 60000);
    }

    #[test]
    fn incomplete_trailing_line_dropped_silently() {
        let raw = fixture("rollout-incomplete-trail.jsonl");
        let parsed = parse_rollout("pty-trail", &raw).expect("incomplete trail");

        assert_eq!(parsed.event.cost.total_duration_ms, 0);
        assert_eq!(parsed.event.model_id, "gpt-5.4");
    }

    #[test]
    fn malformed_mid_line_skipped_with_warn() {
        let raw = fixture("rollout-malformed-mid.jsonl");
        let parsed = parse_rollout("pty-malformed", &raw).expect("malformed mid");

        assert_eq!(parsed.event.agent_session_id, "sess-malformed");
        assert_eq!(parsed.event.model_id, "gpt-5.4");
    }

    /// Direct coverage for the production decoder entry point
    /// (`parse_rollout_snapshot`) — distinct from `parse_rollout`'s
    /// session-id-stamping wrapper. PR #261 cycle 13 review F37:
    /// without this test, a regression that re-introduces a
    /// `session_id` field on `StatusSnapshot` (violating R2.2) would
    /// only be caught by integration tests; the parser unit suite
    /// would still pass because everything else went through the
    /// `parse_rollout` wrapper that adds the id back on top.
    #[test]
    fn parse_rollout_snapshot_returns_session_id_free_status() {
        let raw = fixture("rollout-minimal.jsonl");
        let snapshot =
            parse_rollout_snapshot(Some("pty-direct"), &raw).expect("snapshot should parse");
        // R2.2: snapshot carries `agent_session_id` (from JSONL
        // payload) but NO Vimeflow session_id field — that's stamped
        // by the runtime via `stamp_snapshot`.
        assert_eq!(
            snapshot.agent_session_id,
            "019defd8-15a1-7401-9f4f-40fe52a1c590"
        );
        assert_eq!(snapshot.model_id, "gpt-5.4");
    }

    /// Direct coverage for the malformed-line skip path on
    /// `parse_rollout_snapshot` with `session_id = None`. PR #261
    /// cycle 13 review F37 — the `session_id: Option<&str>` parameter
    /// is diagnostic-only (cycle 3 F7); `None` is a legitimate
    /// production call shape (e.g., a hypothetical future caller that
    /// hasn't been wired through the runtime yet). Pins that the
    /// fold-with-skip contract works regardless of session_id.
    #[test]
    fn parse_rollout_snapshot_skips_malformed_with_no_session_id() {
        let raw = fixture("rollout-malformed-mid.jsonl");
        let snapshot = parse_rollout_snapshot(None, &raw)
            .expect("malformed mid skip works with session_id=None");
        assert_eq!(snapshot.agent_session_id, "sess-malformed");
        assert_eq!(snapshot.model_id, "gpt-5.4");
    }

    #[test]
    fn task_started_fallback_for_context_window_size() {
        let raw = r#"{"timestamp":"...","type":"event_msg","payload":{"type":"task_started","model_context_window":128000}}
"#;
        let parsed = parse_rollout("pty-fallback", raw).expect("task_started fallback");
        assert_eq!(parsed.event.context_window.context_window_size, 128000);
    }

    #[test]
    fn empty_rollout_returns_defaults() {
        let parsed = parse_rollout("pty-empty", "").expect("empty rollout");
        assert_eq!(parsed.event.model_id, "unknown");
        assert_eq!(parsed.event.context_window.context_window_size, 0);
        assert!(parsed.event.context_window.used_percentage.is_none());
        assert_eq!(parsed.event.cost.total_cost_usd, None);
    }

    #[test]
    fn unknown_event_type_ignored_without_error() {
        let raw = r#"{"timestamp":"...","type":"future_event_kind","payload":{"hello":"world"}}
"#;
        let parsed = parse_rollout("pty-unknown", raw).expect("unknown event kind");
        assert_eq!(parsed.event.model_id, "unknown");
    }

    /// Step A-status: the inner `EventMsgPayloadDto::Unknown` branch
    /// (`#[serde(other)]`) covers forward-compatible `event_msg`
    /// sub-types. This test pins the BEHAVIOR (an unknown-payload line
    /// doesn't poison the rest of the document) — paired with the
    /// DTO-level
    /// `inner_event_msg_unknown_payload_deserializes_to_unknown_variant`
    /// below which pins the MECHANISM (deserialization goes through
    /// `EventMsgPayloadDto::Unknown`, not through the per-line
    /// deserialize-skip catch).
    ///
    /// Codex review round 2 (LOW): this test alone is NOT enough —
    /// removing `#[serde(other)]` would make the first line fail to
    /// deserialize, `parse_rollout` would skip it, and the second
    /// line's `task_complete` would still set `total_duration_ms ==
    /// 5000`. The DTO-level test below closes that loophole.
    #[test]
    fn unknown_event_msg_payload_type_ignored_without_error() {
        let raw = r#"{"timestamp":"...","type":"event_msg","payload":{"type":"future_payload_kind","extra":42}}
{"timestamp":"...","type":"event_msg","payload":{"type":"task_complete","duration_ms":5000}}
"#;
        let parsed = parse_rollout("pty-unknown-payload", raw)
            .expect("unknown event_msg payload type folds as no-op");
        assert_eq!(parsed.event.cost.total_duration_ms, 5000);
    }

    /// Step A-status round 2: pin the OUTER `CodexRolloutLine::Unknown`
    /// branch at the DTO level. If `#[serde(other)]` is removed from
    /// the outer enum, `from_str::<CodexRolloutLine>` errors and the
    /// `.expect(...)` below panics — making the regression loud rather
    /// than letting it hide behind `parse_rollout`'s per-line skip.
    #[test]
    fn outer_unknown_type_deserializes_to_unknown_variant() {
        let line = r#"{"timestamp":"...","type":"future_event_kind","payload":{"hello":"world"}}"#;
        let parsed: CodexRolloutLine =
            serde_json::from_str(line).expect("outer #[serde(other)] catches unknown type");
        assert!(
            matches!(parsed, CodexRolloutLine::Unknown),
            "expected CodexRolloutLine::Unknown for an unknown outer type"
        );
    }

    /// Step A-status round 2: pin the INNER `EventMsgPayloadDto::Unknown`
    /// branch at the DTO level. If `#[serde(other)]` is removed from
    /// the inner enum, `from_str::<CodexRolloutLine>` errors when it
    /// reaches the `payload`, and the `.expect(...)` below panics —
    /// distinguishing the `#[serde(other)]` catch from the per-line
    /// deserialize-skip catch in `parse_rollout`.
    #[test]
    fn inner_event_msg_unknown_payload_deserializes_to_unknown_variant() {
        let line = r#"{"timestamp":"...","type":"event_msg","payload":{"type":"future_payload_kind","extra":42}}"#;
        let parsed: CodexRolloutLine =
            serde_json::from_str(line).expect("inner #[serde(other)] catches unknown payload type");
        match parsed {
            CodexRolloutLine::EventMsg {
                payload: Some(EventMsgPayloadDto::Unknown),
            } => {}
            other => panic!(
                "expected EventMsg with EventMsgPayloadDto::Unknown payload, got: {:?}",
                std::mem::discriminant(&other),
            ),
        }
    }

    /// Round-1 upsource fix (codex connector P2): a `token_count`
    /// event with a wrong-typed `info` or `rate_limits` sub-block
    /// must NOT cause `parse_rollout` to drop the whole event_msg
    /// line. Before `lenient_object` landed, a strict struct
    /// deserialize on `"info": 1` would fail the line and
    /// `parse_rollout` would `log::warn!` + skip it, losing whatever
    /// token-state or rate-limit-state context the line still
    /// carried.
    ///
    /// Setup: feed (1) a known-good `token_count` to seed state, then
    /// (2) a `token_count` with wrong-typed `info` paired with a
    /// VALID `rate_limits`. The valid `rate_limits` sub-block must
    /// still be folded; the prior `last_token_count_info` must be
    /// preserved (Some-only invariant).
    #[test]
    fn wrong_typed_token_count_info_preserves_prior_state_and_sibling_rate_limits() {
        let raw = r#"{"timestamp":"...","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"output_tokens":200,"cached_input_tokens":0,"total_tokens":1200},"model_context_window":200000}}}
{"timestamp":"...","type":"event_msg","payload":{"type":"token_count","info":1,"rate_limits":{"primary":{"used_percent":42.5,"resets_at":1776000000}}}}
"#;
        let parsed = parse_rollout("pty-wrong-info", raw)
            .expect("wrong-typed sub-block must NOT drop the line");
        // Prior token-state preserved (the wrong-typed `info` was
        // treated as None by `lenient_object`, then ignored by the
        // Some-only fold).
        assert_eq!(parsed.event.context_window.total_input_tokens, 1000);
        assert_eq!(parsed.event.context_window.total_output_tokens, 200);
        // Sibling rate_limits sub-block on the SAME malformed line
        // still folded — proving per-field degradation, not
        // whole-line drop.
        assert!(
            (parsed.event.rate_limits.five_hour.used_percentage - 42.5).abs() < f64::EPSILON,
            "rate_limits.primary.used_percent should have folded; got {}",
            parsed.event.rate_limits.five_hour.used_percentage
        );
        assert_eq!(parsed.event.rate_limits.five_hour.resets_at, 1776000000);
    }

    // Step 0c: the former `includes_transcript_path_when_provided` test
    // was removed — `parse_rollout` no longer takes (or surfaces) a
    // transcript path. The adapter-level
    // `static_hint_returns_static_transcript_hint_from_located` test in
    // `codex/mod.rs::adapter_tests` pins the new contract: the rollout
    // path reaches the watcher via
    // `TranscriptPathSource::static_hint(&LocatedStatusSource)`.
}
