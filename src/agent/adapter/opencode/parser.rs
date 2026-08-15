//! Parser for opencode bridge JSONL files.
//!
//! [`parse_bridge_snapshot`] folds a `<sessionID>.jsonl` (written by the
//! agent-watcher-opencode-bridge plugin) into a [`StatusSnapshot`]. The fold
//! is tolerant: a malformed or partial-trailing line is silently skipped;
//! no line ever panics the whole document. This mirrors the shape of
//! `codex/parser.rs::parse_rollout_snapshot`.
//!
//! ## Field sources
//!
//! Live opencode does NOT put the model id or real token counts on the
//! `session.*` events — `session.created/updated` carry `info.model =
//! {"providerID":"opencode"}` (no id) and `info.tokens` all-zero, even mid-run.
//! The model id lives on the **user** `message.updated` (`info.model.modelID`)
//! and the running token usage lives on each `step-finish` part. The fold below
//! reads those primary sources and keeps the session-event reads as lower-
//! priority fallbacks for older/synthetic transcripts that do carry them.
//!
//! | StatusSnapshot field           | Bridge line                                      |
//! |-------------------------------|--------------------------------------------------|
//! | `agent_session_id`             | latest `session.created/updated` → `info.id`     |
//! | `model_id` / `model_display_name` | latest non-empty `message.updated` `info.model.modelID` (fallback `info.modelID`); else `session.*` `info.model.{modelID,id}` |
//! | `version`                      | `info.version`                                   |
//! | `context_window.total_input_tokens`  | latest `step-finish` → `data.part.tokens.input` (running context-window usage); fallback `session.*` `info.tokens.input` |
//! | `context_window.total_output_tokens` | latest `step-finish` → `data.part.tokens.output`; fallback `session.*` `info.tokens.output` |
//! | `cost.total_cost_usd`          | `info.cost`                                      |
//! | `context_window.current_usage` | latest `step-finish` → `data.part.tokens`        |
//! | `context_window_size`          | injected resolver `(providerID, modelID) -> tokens` — opencode's models.dev cache (`model_catalog`); `0` = unknown |
//! | `rate_limits`                  | safe default (`five_hour: 0.0 / 0`)              |
//! | `usage_fetched`                | always `false`                                   |

use serde_json::Value;

use crate::agent::adapter::opencode::transcript_dto::{
    OpencodeEventType, OpencodeKind, OpencodeLineDto,
};
use crate::agent::adapter::types::StatusSnapshot;
use crate::agent::types::{
    ContextWindowStatus, CostMetrics, CurrentUsage, RateLimitInfo, RateLimits,
};

// ─── fold accumulator ──────────────────────────────────────────────────────

#[derive(Default)]
struct OpencodeFoldState {
    /// Agent's internal session id (from `info.id`).
    agent_session_id: String,
    /// Latest non-empty model id read from a `message.updated`
    /// (`info.model.modelID`, fallback `info.modelID`). This is the primary
    /// source live — the assistant `message.updated` carries `model:{}` and
    /// must NOT clobber the user message's id, so only non-empty values are
    /// recorded.
    model_id_from_message: String,
    /// Latest non-empty model id read from a `session.created/updated`
    /// (`info.model.modelID`, fallback `info.model.id`). Live opencode session
    /// events carry no id, so this only fires for older/synthetic transcripts;
    /// it is the lower-priority fallback behind `model_id_from_message`.
    model_id_from_session: String,
    /// Latest non-empty provider id from a `message.updated`
    /// (`info.model.providerID`). Pairs with `model_id_from_message` for the
    /// `(providerID, modelID)` context-window lookup; the model-id priority rule
    /// applies identically (message wins over session, empty never clobbers).
    provider_id_from_message: String,
    /// Lower-priority provider id from a `session.created/updated`
    /// (`info.model.providerID`).
    provider_id_from_session: String,
    /// Version string (from `info.version`).
    version: String,
    /// Lifetime total input tokens. Live this is the LATEST `step-finish`
    /// `input` (the running context-window usage); the session-event
    /// `info.tokens.input` is a fallback for transcripts that carry it.
    total_input_tokens: u64,
    /// Lifetime total output tokens. Live this is the LATEST `step-finish`
    /// `output`; session-event `info.tokens.output` is the fallback.
    total_output_tokens: u64,
    /// True once a `step-finish` has supplied the token totals, so later
    /// session-event token reads do not overwrite the running step values.
    totals_from_step_finish: bool,
    /// Total cost USD (from latest `session.created/updated` → `info.cost`).
    cost_usd: Option<f64>,
    /// Current-step usage from the latest `step-finish` part.
    step_usage: Option<StepUsage>,
}

#[derive(Clone)]
struct StepUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
}

impl OpencodeFoldState {
    /// `resolve_window(providerID, modelID) -> tokens` (0 = unknown) supplies the
    /// context-window denominator. Injected so production reads opencode's
    /// models.dev cache while tests pass a deterministic stub.
    fn into_snapshot(self, resolve_window: impl Fn(&str, &str) -> u64) -> StatusSnapshot {
        // Resolve model + provider up front (message wins over session for both —
        // live opencode only supplies the message form; an empty value never
        // clobbers a prior non-empty one during the fold).
        let resolved_model_id = if !self.model_id_from_message.is_empty() {
            self.model_id_from_message
        } else {
            self.model_id_from_session
        };
        let resolved_provider_id = if !self.provider_id_from_message.is_empty() {
            self.provider_id_from_message
        } else {
            self.provider_id_from_session
        };

        // Context-window size comes from opencode's models.dev cache, keyed by
        // (providerID, modelID). 0 = unknown (no model, cache absent, or model
        // unlisted) — the frontend then renders the bar without a denominator.
        let context_window_size = if resolved_model_id.is_empty() {
            0
        } else {
            resolve_window(&resolved_provider_id, &resolved_model_id)
        };

        // Context occupancy = the FULL prompt sent on the latest step, not just
        // the fresh delta. opencode (like Anthropic / kimi) reports `input` as
        // only the uncached tokens once prompt caching engages, parking the bulk
        // of the conversation in `cache.read`; the running totals therefore read
        // small (e.g. 152) even when 24k tokens of context are in play. So the
        // gauge numerator sums fresh input + output + cache-read + cache-write —
        // mirroring `kimi::parser`'s `used` — and the frontend reconstructs the
        // token count from this percentage (it deliberately ignores
        // total_input/output, which exclude cache reads).
        let (cache_read, cache_write) = self
            .step_usage
            .as_ref()
            .map(|su| (su.cache_read_tokens, su.cache_write_tokens))
            .unwrap_or((0, 0));

        // With context_window_size == 0 we cannot compute a meaningful percentage.
        let used_percentage: Option<f64> = if context_window_size == 0 {
            None
        } else {
            // Saturating: a malformed / future transcript with huge token counts
            // must not panic (debug) or wrap (release) — this parser is tolerant
            // of bad lines, and kimi's numerator saturates for the same reason.
            let used = self
                .total_input_tokens
                .saturating_add(self.total_output_tokens)
                .saturating_add(cache_read)
                .saturating_add(cache_write);
            Some((used as f64 / context_window_size as f64 * 100.0).clamp(0.0, 100.0))
        };

        let remaining_percentage = used_percentage
            .map(|u| (100.0_f64 - u).clamp(0.0, 100.0))
            .unwrap_or(100.0);

        let current_usage = self.step_usage.map(|su| CurrentUsage {
            input_tokens: su.input_tokens,
            output_tokens: su.output_tokens,
            cache_creation_input_tokens: su.cache_write_tokens,
            cache_read_input_tokens: su.cache_read_tokens,
        });

        let context_window = ContextWindowStatus {
            used_percentage,
            remaining_percentage,
            context_window_size,
            total_input_tokens: self.total_input_tokens,
            total_output_tokens: self.total_output_tokens,
            current_usage,
        };

        let cost = CostMetrics {
            total_cost_usd: self.cost_usd,
            total_duration_ms: 0,
            total_api_duration_ms: 0,
            total_lines_added: 0,
            total_lines_removed: 0,
        };

        let rate_limits = RateLimits {
            five_hour: RateLimitInfo {
                used_percentage: 0.0,
                resets_at: 0,
            },
            seven_day: None,
        };

        // Fall back to "unknown" when the fold saw no model id.
        let (model_id, model_display_name) = if resolved_model_id.is_empty() {
            ("unknown".to_string(), "unknown".to_string())
        } else {
            (resolved_model_id.clone(), resolved_model_id)
        };

        StatusSnapshot {
            agent_session_id: self.agent_session_id,
            model_id,
            model_display_name,
            version: self.version,
            context_window,
            cost,
            rate_limits,
            usage_fetched: false,
        }
    }
}

// ─── fold helpers ──────────────────────────────────────────────────────────

/// Extract a string leaf from a `serde_json::Value` via a dot-separated path.
/// Returns `None` if any segment is missing or the final value is not a string.
fn value_str<'v>(v: &'v Value, path: &[&str]) -> Option<&'v str> {
    let mut cur = v;
    for seg in path {
        cur = cur.get(seg)?;
    }
    cur.as_str()
}

/// Extract a `u64` leaf from a `serde_json::Value` via a dot-separated path.
fn value_u64(v: &Value, path: &[&str]) -> Option<u64> {
    let mut cur = v;
    for seg in path {
        cur = cur.get(seg)?;
    }
    cur.as_u64()
}

/// Extract an `f64` leaf from a `serde_json::Value` via a dot-separated path.
fn value_f64(v: &Value, path: &[&str]) -> Option<f64> {
    let mut cur = v;
    for seg in path {
        cur = cur.get(seg)?;
    }
    cur.as_f64()
}

fn fold_session_info(state: &mut OpencodeFoldState, info: &Value) {
    if let Some(id) = value_str(info, &["id"]) {
        if !id.is_empty() {
            state.agent_session_id = id.to_string();
        }
    }
    if let Some(ver) = value_str(info, &["version"]) {
        if !ver.is_empty() {
            state.version = ver.to_string();
        }
    }
    // Lower-priority model source: live opencode session events carry no id
    // (`model:{"providerID":"opencode"}`), so this only fires for older /
    // synthetic transcripts. `modelID` first, then the legacy nested `id`.
    let model_id =
        value_str(info, &["model", "modelID"]).or_else(|| value_str(info, &["model", "id"]));
    if let Some(model_id) = model_id.filter(|value| !value.is_empty()) {
        state.model_id_from_session = model_id.to_string();
    }
    if let Some(provider) = value_str(info, &["model", "providerID"]).filter(|v| !v.is_empty()) {
        state.provider_id_from_session = provider.to_string();
    }
    // Session-event token totals are a fallback only: live they are all-zero,
    // and once a `step-finish` has supplied the running totals we must not let a
    // later (zero) session event overwrite them.
    if !state.totals_from_step_finish {
        if let Some(input) = value_u64(info, &["tokens", "input"]) {
            state.total_input_tokens = input;
        }
        if let Some(output) = value_u64(info, &["tokens", "output"]) {
            state.total_output_tokens = output;
        }
    }
    // `cost` in the bridge is a number (USD); `None` means missing field.
    if let Some(cost) = value_f64(info, &["cost"]) {
        state.cost_usd = Some(cost);
    }
}

/// A `message.updated` carries the model id live: a **user** message has
/// `info.model = {"providerID":...,"modelID":...}`, while the assistant message
/// has `model:{}`. Only a non-empty value is recorded, so the assistant's empty
/// object never clobbers the user-supplied id. `info.model.modelID` is the
/// primary path; a flat `info.modelID` is a defensive fallback.
fn fold_message_info(state: &mut OpencodeFoldState, info: &Value) {
    let model_id = value_str(info, &["model", "modelID"]).or_else(|| value_str(info, &["modelID"]));
    if let Some(model_id) = model_id.filter(|value| !value.is_empty()) {
        state.model_id_from_message = model_id.to_string();
    }
    if let Some(provider) = value_str(info, &["model", "providerID"]).filter(|v| !v.is_empty()) {
        state.provider_id_from_message = provider.to_string();
    }
}

fn fold_step_finish(state: &mut OpencodeFoldState, part: &Value) {
    // `data.part.tokens` carries per-step usage for a `step-finish` part.
    let tokens = match part.get("tokens") {
        Some(t) if t.is_object() => t,
        _ => return,
    };

    let input = tokens.get("input").and_then(Value::as_u64).unwrap_or(0);
    let output = tokens.get("output").and_then(Value::as_u64).unwrap_or(0);
    let cache_read = tokens
        .get("cache")
        .and_then(|c| c.get("read"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = tokens
        .get("cache")
        .and_then(|c| c.get("write"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    state.step_usage = Some(StepUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    });

    // The LATEST `step-finish` `input` is opencode's running context-window
    // usage (it resets per step rather than accumulating, so we track the most
    // recent value, not a sum). `output` likewise mirrors the latest step. Once
    // any `step-finish` lands, these win over the (zero) session-event tokens.
    state.total_input_tokens = input;
    state.total_output_tokens = output;
    state.totals_from_step_finish = true;
}

fn fold_line(state: &mut OpencodeFoldState, dto: &OpencodeLineDto) {
    match dto.kind() {
        OpencodeKind::Event => match dto.event_type() {
            OpencodeEventType::SessionCreated | OpencodeEventType::SessionUpdated => {
                if let Some(info) = dto.data.get("info") {
                    fold_session_info(state, info);
                }
            }
            OpencodeEventType::MessageUpdated => {
                // Primary model-id source (the user message carries modelID).
                if let Some(info) = dto.data.get("info") {
                    fold_message_info(state, info);
                }
            }
            OpencodeEventType::MessagePartUpdated => {
                // Only fold `step-finish` parts.
                if let Some(part) = dto.data.get("part") {
                    if part.get("type").and_then(Value::as_str) == Some("step-finish") {
                        fold_step_finish(state, part);
                    }
                }
            }
            _ => {} // All other event types are not needed by the snapshot decoder.
        },
        // tool.before / tool.after / Unknown — not needed by the snapshot fold.
        _ => {}
    }
}

// ─── public entry point ────────────────────────────────────────────────────

/// Fold `raw` (the contents of a `<sessionID>.jsonl` bridge file) into a
/// [`StatusSnapshot`].
///
/// Every line is parsed independently. Empty lines and lines that fail to
/// deserialize are silently skipped (the same tolerant behavior as
/// `codex/parser.rs::parse_rollout_snapshot`). The incomplete partial-trailing
/// line that a mid-write tail produces is also dropped.
///
/// The first caller in production is M5's `StateDecoder` impl on
/// `OpenCodeAdapter`.
/// `resolve_window(providerID, modelID) -> tokens` (0 = unknown) is injected so
/// the production decode path passes `model_catalog::context_window` (which
/// reads opencode's models.dev cache) while tests pass a deterministic stub.
#[allow(dead_code)]
pub(crate) fn parse_bridge_snapshot(
    session_id: Option<&str>,
    raw: &str,
    resolve_window: impl Fn(&str, &str) -> u64,
) -> StatusSnapshot {
    let mut state = OpencodeFoldState::default();

    let lines: Vec<&str> = raw.split('\n').collect();
    let trailing_complete = raw.ends_with('\n');

    for (idx, line) in lines.iter().enumerate() {
        let is_last = idx + 1 == lines.len();
        if line.is_empty() {
            continue;
        }
        // Drop the last line if it has no trailing newline — it's a partial write.
        if is_last && !trailing_complete {
            continue;
        }

        match serde_json::from_str::<OpencodeLineDto>(line) {
            Ok(dto) => fold_line(&mut state, &dto),
            Err(err) => log::warn!(
                "opencode: skipping malformed bridge line (sid={}, err={err})",
                session_id.unwrap_or("?"),
            ),
        }
    }

    state.into_snapshot(resolve_window)
}

// ─── tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Window-resolver stub: unit tests never consult the real models.dev cache,
    /// so the window is "unknown" unless a test injects its own resolver.
    fn no_window(_provider_id: &str, _model_id: &str) -> u64 {
        0
    }

    /// Load the authored `sample_bridge.jsonl` fixture (embedded at
    /// compile time so the test works regardless of cwd).
    const SAMPLE_BRIDGE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/agent/adapter/opencode/fixtures/sample_bridge.jsonl"
    ));

    // ── fixture-based tests (REAL live shapes) ────────────────────────
    //
    // The fixture mirrors live opencode: `session.*` carry `model:
    // {"providerID":"opencode"}` (NO id) and all-zero `info.tokens`; the model
    // id lives on the **user** `message.updated` (`info.model.modelID`), and the
    // running token usage lives on the `step-finish` parts. These tests would
    // FAIL on the pre-fix parser (which read model + tokens only from
    // `session.*`).

    #[test]
    fn snapshot_model_comes_from_message_updated_not_session() {
        // Bug B: the model id is on the user `message.updated`
        // (`glm-4-6-fake`); the session events carry `model:{"providerID":
        // "opencode"}` with no id, and the assistant `message.updated` carries
        // `model:{}` — neither must win or clobber.
        let snap = parse_bridge_snapshot(None, SAMPLE_BRIDGE, no_window);
        assert_eq!(snap.model_id, "glm-4-6-fake");
        assert_eq!(snap.model_display_name, "glm-4-6-fake");
        assert_eq!(snap.version, "1.17.8");
    }

    #[test]
    fn snapshot_has_correct_agent_session_id_from_fixture() {
        let snap = parse_bridge_snapshot(None, SAMPLE_BRIDGE, no_window);
        assert_eq!(snap.agent_session_id, "ses_sample001");
    }

    #[test]
    fn snapshot_token_totals_come_from_latest_step_finish() {
        // Bug C: live `session.*` `info.tokens` are all-zero. The running
        // context-window usage is the LATEST `step-finish` `input`
        // (9000 then 12345 ⇒ 12345), not the first and not a sum.
        let snap = parse_bridge_snapshot(None, SAMPLE_BRIDGE, no_window);
        assert_eq!(snap.context_window.total_input_tokens, 12345);
        assert_eq!(snap.context_window.total_output_tokens, 222);
    }

    #[test]
    fn snapshot_has_correct_cost_from_fixture() {
        // The final `session.updated` carries `cost: 0.0042`.
        let snap = parse_bridge_snapshot(None, SAMPLE_BRIDGE, no_window);
        let cost = snap.cost.total_cost_usd.expect("cost should be set");
        assert!(
            (cost - 0.0042).abs() < 1e-9,
            "cost should be 0.0042, got {cost}"
        );
    }

    #[test]
    fn snapshot_current_usage_from_latest_step_finish() {
        // The fixture's LATEST `step-finish` part carries
        // tokens: { input: 12345, output: 222, cache: { read: 1200, write: 600 } }.
        let snap = parse_bridge_snapshot(None, SAMPLE_BRIDGE, no_window);
        let cu = snap
            .context_window
            .current_usage
            .expect("current_usage should be present from step-finish");
        assert_eq!(cu.input_tokens, 12345);
        assert_eq!(cu.output_tokens, 222);
        assert_eq!(cu.cache_read_input_tokens, 1200);
        assert_eq!(cu.cache_creation_input_tokens, 600);
    }

    #[test]
    fn assistant_empty_model_does_not_clobber_user_model() {
        // Order: user message (modelID set) → assistant message (model:{}).
        // The empty assistant object must NOT wipe the recorded id.
        let raw = concat!(
            "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"message.updated\",\"data\":{\"info\":{\"id\":\"msg_u\",\"role\":\"user\",\"model\":{\"providerID\":\"opencode\",\"modelID\":\"deepseek-v4-flash-free\"}}}}\n",
            "{\"v\":1,\"ts\":2,\"kind\":\"event\",\"type\":\"message.updated\",\"data\":{\"info\":{\"id\":\"msg_a\",\"role\":\"assistant\",\"model\":{}}}}\n",
        );
        let snap = parse_bridge_snapshot(None, raw, no_window);
        assert_eq!(snap.model_id, "deepseek-v4-flash-free");
    }

    #[test]
    fn live_session_events_with_zero_tokens_yield_step_finish_totals() {
        // The exact live failure mode: session.* model has no id + zero tokens;
        // user message supplies the model; step-finish supplies real tokens.
        let raw = concat!(
            "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"session.created\",\"data\":{\"info\":{\"id\":\"ses_live\",\"version\":\"1.17.8\",\"model\":{\"providerID\":\"opencode\"},\"cost\":0,\"tokens\":{\"input\":0,\"output\":0,\"reasoning\":0,\"cache\":{\"read\":0,\"write\":0}}}}}\n",
            "{\"v\":1,\"ts\":2,\"kind\":\"event\",\"type\":\"message.updated\",\"data\":{\"info\":{\"id\":\"msg_u\",\"role\":\"user\",\"model\":{\"providerID\":\"opencode\",\"modelID\":\"deepseek-v4-flash-free\"}}}}\n",
            "{\"v\":1,\"ts\":3,\"kind\":\"event\",\"type\":\"message.part.updated\",\"data\":{\"part\":{\"type\":\"step-finish\",\"tokens\":{\"input\":11781,\"output\":98,\"reasoning\":19,\"cache\":{\"read\":0,\"write\":0}}}}}\n",
            "{\"v\":1,\"ts\":4,\"kind\":\"event\",\"type\":\"session.updated\",\"data\":{\"info\":{\"id\":\"ses_live\",\"model\":{\"providerID\":\"opencode\"},\"tokens\":{\"input\":0,\"output\":0,\"reasoning\":0,\"cache\":{\"read\":0,\"write\":0}}}}}\n",
        );
        let snap = parse_bridge_snapshot(None, raw, no_window);
        assert_eq!(snap.model_id, "deepseek-v4-flash-free");
        // The trailing zero session.updated must NOT overwrite the step total.
        assert_eq!(snap.context_window.total_input_tokens, 11781);
        assert_eq!(snap.context_window.total_output_tokens, 98);
    }

    #[test]
    fn context_window_size_is_zero_unknown_when_resolver_returns_zero() {
        let snap = parse_bridge_snapshot(None, SAMPLE_BRIDGE, no_window);
        assert_eq!(snap.context_window.context_window_size, 0);
        // With size == 0 we cannot compute a percentage.
        assert!(snap.context_window.used_percentage.is_none());
    }

    #[test]
    fn context_window_size_and_percentage_come_from_injected_resolver() {
        // user `message.updated` supplies {providerID, modelID}; `step-finish`
        // supplies the running token usage. The resolver maps that model to a
        // 200k-token window (as opencode's models.dev cache does for
        // deepseek-v4-flash-free), so the snapshot reports a real percentage.
        let raw = concat!(
            "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"message.updated\",\"data\":{\"info\":{\"role\":\"user\",\"model\":{\"providerID\":\"opencode\",\"modelID\":\"deepseek-v4-flash-free\"}}}}\n",
            "{\"v\":1,\"ts\":2,\"kind\":\"event\",\"type\":\"message.part.updated\",\"data\":{\"part\":{\"type\":\"step-finish\",\"tokens\":{\"input\":18000,\"output\":2000,\"cache\":{\"read\":0,\"write\":0}}}}}\n",
        );

        let resolve = |provider: &str, model: &str| -> u64 {
            if (provider, model) == ("opencode", "deepseek-v4-flash-free") {
                200_000
            } else {
                0
            }
        };

        let snap = parse_bridge_snapshot(None, raw, resolve);
        assert_eq!(snap.context_window.context_window_size, 200_000);
        // total = 18000 + 2000 = 20000; 20000 / 200000 = 10%.
        let used = snap
            .context_window
            .used_percentage
            .expect("percentage set when window known");
        assert!((used - 10.0).abs() < 1e-9, "used% = {used}");
        assert!(
            (snap.context_window.remaining_percentage - 90.0).abs() < 1e-9,
            "remaining% = {}",
            snap.context_window.remaining_percentage
        );
    }

    #[test]
    fn context_percentage_includes_cache_read_not_just_fresh_input() {
        // Regression for the "context not incrementing" bug: once prompt caching
        // engages, opencode reports `input` as only the fresh delta (152) and
        // parks the conversation in `cache.read` (24064). The gauge must reflect
        // the FULL context (152 + 44 + 24064), not collapse to the fresh delta.
        let raw = concat!(
            "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"message.updated\",\"data\":{\"info\":{\"role\":\"user\",\"model\":{\"providerID\":\"opencode\",\"modelID\":\"deepseek-v4-flash-free\"}}}}\n",
            "{\"v\":1,\"ts\":2,\"kind\":\"event\",\"type\":\"message.part.updated\",\"data\":{\"part\":{\"type\":\"step-finish\",\"tokens\":{\"input\":152,\"output\":44,\"reasoning\":20,\"cache\":{\"read\":24064,\"write\":0}}}}}\n",
        );
        let snap = parse_bridge_snapshot(None, raw, |_, _| 200_000);

        // used = 152 + 44 + 24064 = 24260; 24260 / 200000 = 12.13%.
        let used = snap
            .context_window
            .used_percentage
            .expect("percentage set when window known");
        assert!((used - 12.13).abs() < 0.01, "used% = {used}");
        // The fresh-input total stays the per-step delta (matches kimi); the
        // cache breakdown is surfaced via current_usage.
        assert_eq!(snap.context_window.total_input_tokens, 152);
        let cu = snap.context_window.current_usage.expect("usage present");
        assert_eq!(cu.cache_read_input_tokens, 24064);
    }

    #[test]
    fn resolver_receives_the_message_provider_and_model_ids() {
        // The resolver must be called with the (providerID, modelID) the bridge
        // emitted on the user `message.updated`.
        use std::cell::RefCell;
        let raw =
            "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"message.updated\",\"data\":{\"info\":{\"role\":\"user\",\"model\":{\"providerID\":\"anthropic\",\"modelID\":\"claude-sonnet-4\"}}}}\n";

        let seen = RefCell::new(None);
        let resolve = |provider: &str, model: &str| -> u64 {
            *seen.borrow_mut() = Some((provider.to_string(), model.to_string()));
            1_000_000
        };

        let snap = parse_bridge_snapshot(None, raw, resolve);
        assert_eq!(snap.context_window.context_window_size, 1_000_000);
        assert_eq!(
            seen.into_inner(),
            Some(("anthropic".to_string(), "claude-sonnet-4".to_string()))
        );
    }

    #[test]
    fn resolver_is_not_consulted_when_the_model_is_unknown() {
        // No model id in the stream ⇒ the resolver must not be called and the
        // window stays unknown (0).
        let raw = "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"session.created\",\"data\":{\"info\":{\"id\":\"ses_x\"}}}\n";
        let resolve = |_provider: &str, _model: &str| -> u64 {
            panic!("resolver must not run when the model is unknown");
        };

        let snap = parse_bridge_snapshot(None, raw, resolve);
        assert_eq!(snap.context_window.context_window_size, 0);
        assert!(snap.context_window.used_percentage.is_none());
    }

    #[test]
    fn rate_limits_are_safe_default() {
        let snap = parse_bridge_snapshot(None, SAMPLE_BRIDGE, no_window);
        assert!((snap.rate_limits.five_hour.used_percentage - 0.0).abs() < f64::EPSILON);
        assert_eq!(snap.rate_limits.five_hour.resets_at, 0);
        assert!(snap.rate_limits.seven_day.is_none());
        assert!(!snap.usage_fetched);
    }

    #[test]
    fn snapshot_reads_model_id_from_bridge_emitted_model_id() {
        let raw = concat!(
            "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"session.created\",\"data\":{\"info\":{\"id\":\"ses_model_id\",\"version\":\"1.2.3\",\"model\":{\"providerID\":\"anthropic\",\"modelID\":\"claude-sonnet-4\"},\"cost\":0.25,\"tokens\":{\"input\":700,\"output\":300,\"cache\":{\"read\":10,\"write\":20}}}}}\n",
        );

        let snap = parse_bridge_snapshot(Some("pty-1"), raw, no_window);
        assert_eq!(snap.agent_session_id, "ses_model_id");
        assert_eq!(snap.model_id, "claude-sonnet-4");
        assert_eq!(snap.model_display_name, "claude-sonnet-4");
        assert_eq!(snap.version, "1.2.3");
        assert_eq!(snap.context_window.total_input_tokens, 700);
        assert_eq!(snap.context_window.total_output_tokens, 300);
        let cost = snap.cost.total_cost_usd.expect("cost set");
        assert!((cost - 0.25).abs() < 1e-9);
    }

    #[test]
    fn step_finish_tokens_populate_totals_when_session_tokens_are_absent() {
        let raw = concat!(
            "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"session.created\",\"data\":{\"info\":{\"id\":\"ses_step_only\",\"model\":{\"modelID\":\"claude-haiku\"}}}}\n",
            "{\"v\":1,\"ts\":2,\"kind\":\"event\",\"type\":\"message.part.updated\",\"data\":{\"part\":{\"type\":\"step-finish\",\"tokens\":{\"input\":321,\"output\":123,\"cache\":{\"read\":40,\"write\":50}}}}}\n",
        );

        let snap = parse_bridge_snapshot(None, raw, no_window);
        assert_eq!(snap.model_id, "claude-haiku");
        assert_eq!(snap.context_window.total_input_tokens, 321);
        assert_eq!(snap.context_window.total_output_tokens, 123);
        let cu = snap
            .context_window
            .current_usage
            .expect("current_usage present");
        assert_eq!(cu.cache_read_input_tokens, 40);
        assert_eq!(cu.cache_creation_input_tokens, 50);
    }

    // ── malformed / edge-case tests ────────────────────────────────────

    /// A garbage line in the middle is skipped; the fold succeeds and the
    /// good lines before and after still contribute.
    #[test]
    fn malformed_mid_line_is_tolerated() {
        let raw = concat!(
            "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"session.created\",\"data\":{\"info\":{\"id\":\"ses_tol\",\"version\":\"1.0.0\",\"model\":{\"id\":\"claude-opus\"},\"cost\":0,\"tokens\":{\"input\":100,\"output\":50,\"cache\":{\"read\":0,\"write\":0}}}}}\n",
            "THIS IS NOT JSON AT ALL !!!\n",
            "{\"v\":1,\"ts\":2,\"kind\":\"event\",\"type\":\"session.updated\",\"data\":{\"info\":{\"id\":\"ses_tol\",\"version\":\"1.0.0\",\"model\":{\"id\":\"claude-opus\"},\"cost\":0.001,\"tokens\":{\"input\":200,\"output\":80,\"cache\":{\"read\":0,\"write\":0}}}}}\n",
        );

        let snap = parse_bridge_snapshot(None, raw, no_window);
        assert_eq!(snap.agent_session_id, "ses_tol");
        assert_eq!(snap.model_id, "claude-opus");
        // The second good session.updated (after the garbage line) is applied.
        assert_eq!(snap.context_window.total_input_tokens, 200);
        assert_eq!(snap.context_window.total_output_tokens, 80);
        let cost = snap.cost.total_cost_usd.expect("cost set");
        assert!((cost - 0.001).abs() < 1e-9);
    }

    /// A bridge with no `step-finish` event → current_usage is None, no panic.
    #[test]
    fn no_step_finish_yields_zero_current_usage() {
        let raw = concat!(
            "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"session.created\",\"data\":{\"info\":{\"id\":\"ses_nostep\",\"version\":\"1.0.0\",\"model\":{\"id\":\"gpt-4o\"},\"cost\":0,\"tokens\":{\"input\":0,\"output\":0,\"cache\":{\"read\":0,\"write\":0}}}}}\n",
        );

        let snap = parse_bridge_snapshot(None, raw, no_window);
        assert_eq!(snap.agent_session_id, "ses_nostep");
        assert!(snap.context_window.current_usage.is_none());
        assert_eq!(snap.context_window.total_input_tokens, 0);
    }

    /// An entirely empty input produces a default snapshot without panicking.
    #[test]
    fn empty_input_returns_default_snapshot_no_panic() {
        let snap = parse_bridge_snapshot(None, "", no_window);
        assert_eq!(snap.agent_session_id, "");
        assert_eq!(snap.model_id, "unknown");
        assert_eq!(snap.model_display_name, "unknown");
        assert_eq!(snap.version, "");
        assert_eq!(snap.context_window.total_input_tokens, 0);
        assert_eq!(snap.context_window.total_output_tokens, 0);
        assert!(snap.context_window.current_usage.is_none());
        assert!(snap.cost.total_cost_usd.is_none());
        assert!(!snap.usage_fetched);
    }

    /// A file that ends with a partial (no trailing newline) last line
    /// silently drops it and still folds the rest correctly.
    #[test]
    fn partial_trailing_line_dropped_silently() {
        // The second line has no trailing '\n' — it must be dropped.
        let raw = "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"session.created\",\"data\":{\"info\":{\"id\":\"ses_partial\",\"version\":\"0.9\",\"model\":{\"id\":\"m1\"},\"cost\":0,\"tokens\":{\"input\":10,\"output\":5,\"cache\":{\"read\":0,\"write\":0}}}}}\n{INCOMPLETE";

        let snap = parse_bridge_snapshot(None, raw, no_window);
        assert_eq!(snap.agent_session_id, "ses_partial");
        assert_eq!(snap.model_id, "m1");
    }

    /// The latest `session.updated` wins over an earlier `session.created`
    /// for all session-level fields.
    #[test]
    fn latest_session_event_wins() {
        let raw = concat!(
            "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"session.created\",\"data\":{\"info\":{\"id\":\"ses_win\",\"version\":\"1.0.0\",\"model\":{\"id\":\"old-model\"},\"cost\":0,\"tokens\":{\"input\":0,\"output\":0,\"cache\":{\"read\":0,\"write\":0}}}}}\n",
            "{\"v\":1,\"ts\":2,\"kind\":\"event\",\"type\":\"session.updated\",\"data\":{\"info\":{\"id\":\"ses_win\",\"version\":\"1.17.8\",\"model\":{\"id\":\"new-model\"},\"cost\":0.5,\"tokens\":{\"input\":500,\"output\":100,\"cache\":{\"read\":0,\"write\":0}}}}}\n",
        );

        let snap = parse_bridge_snapshot(None, raw, no_window);
        assert_eq!(snap.model_id, "new-model");
        assert_eq!(snap.version, "1.17.8");
        assert_eq!(snap.context_window.total_input_tokens, 500);
        let cost = snap.cost.total_cost_usd.expect("cost set");
        assert!((cost - 0.5).abs() < 1e-9);
    }

    /// The latest `step-finish` part's tokens win when there are multiple.
    #[test]
    fn latest_step_finish_wins() {
        let raw = concat!(
            "{\"v\":1,\"ts\":1,\"kind\":\"event\",\"type\":\"message.part.updated\",\"data\":{\"part\":{\"type\":\"step-finish\",\"tokens\":{\"input\":100,\"output\":20,\"cache\":{\"read\":0,\"write\":0}}}}}\n",
            "{\"v\":1,\"ts\":2,\"kind\":\"event\",\"type\":\"message.part.updated\",\"data\":{\"part\":{\"type\":\"step-finish\",\"tokens\":{\"input\":300,\"output\":60,\"cache\":{\"read\":50,\"write\":10}}}}}\n",
        );

        let snap = parse_bridge_snapshot(None, raw, no_window);
        let cu = snap
            .context_window
            .current_usage
            .expect("current_usage present");
        assert_eq!(cu.input_tokens, 300);
        assert_eq!(cu.output_tokens, 60);
        assert_eq!(cu.cache_read_input_tokens, 50);
        assert_eq!(cu.cache_creation_input_tokens, 10);
    }
}
