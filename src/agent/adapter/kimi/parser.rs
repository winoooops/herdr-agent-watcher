//! Parser for kimi-code `wire.jsonl` files.
//!
//! Streaming, line-folded decoder (mirrors `codex/parser.rs`): each line
//! deserializes through the lenient [`KimiLineDto`], the fold tracks the
//! latest model + cumulative usage, and `into_snapshot` composes the
//! provider-neutral [`StatusSnapshot`]. `Ok` always — a partial trailing
//! line is dropped, a malformed line is logged + skipped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use super::transcript_dto::{KimiLineDto, KimiRecordType, KimiUsageDto};
use super::types::KIMI_CONTEXT_WINDOW_SIZE;
use crate::agent::adapter::types::StatusSnapshot;
#[cfg(test)]
use crate::agent::adapter::types::{stamp_snapshot, ParsedStatus};
use crate::agent::types::{
    ContextWindowStatus, CostMetrics, CurrentUsage, RateLimitInfo, RateLimits,
};

/// Test-only wrapper pairing `parse_wire_snapshot` with `stamp_snapshot`
/// so tests can assert on the full `AgentStatusEvent` shape. Mirrors
/// `codex::parser::parse_rollout`.
#[cfg(test)]
pub(crate) fn parse_wire(session_id: &str, raw: &str) -> Result<ParsedStatus, String> {
    let snapshot = parse_wire_snapshot(Some(session_id), raw)?;
    Ok(ParsedStatus {
        event: stamp_snapshot(session_id, snapshot),
    })
}

/// Decode a kimi-code `wire.jsonl` snapshot into a session-id-free
/// [`StatusSnapshot`]. `session_id` is diagnostic-only (labels the
/// malformed-line warn). Tolerates a partial trailing line and skips
/// malformed lines.
pub(crate) fn parse_wire_snapshot(
    session_id: Option<&str>,
    raw: &str,
) -> Result<StatusSnapshot, String> {
    let mut state = KimiFoldState::default();
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

        match serde_json::from_str::<KimiLineDto>(line) {
            Ok(parsed) => fold_line(&mut state, parsed),
            Err(_) => log::warn!(
                "kimi: skipping malformed wire line (sid={})",
                session_id.unwrap_or("?"),
            ),
        }
    }

    Ok(state.into_snapshot())
}

/// Aggregate a kimi session across its agents: the model/version come from
/// the `main` agent (the session identity), the context window from the
/// MOST-RECENTLY-ACTIVE agent wire (so a `/init`-style run delegating to a
/// sub-agent shows the sub-agent's live token usage, not main's empty
/// shell). Returns `None` when `state.json` / its wires can't be read, so
/// the caller falls back to the single main-wire decode.
pub(crate) fn parse_session_aggregate(session_dir: &Path) -> Option<StatusSnapshot> {
    let wires = read_agent_wires(session_dir)?;
    let main_wire = wires.iter().find(|w| w.is_main).map(|w| &w.wire)?;
    let main_raw = std::fs::read_to_string(main_wire).ok()?;
    let mut snapshot = parse_wire_snapshot(None, &main_raw).ok()?;

    let active_wire = wires
        .iter()
        .max_by_key(|w| {
            std::fs::metadata(&w.wire)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        })
        .map(|w| &w.wire)?;
    let active_raw = std::fs::read_to_string(active_wire).ok()?;
    snapshot.context_window = parse_wire_snapshot(None, &active_raw).ok()?.context_window;
    // Carry kimi's own `session_*` id so the frontend can distinguish runs.
    snapshot.agent_session_id = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_default();
    Some(snapshot)
}

/// Count MAIN-agent user turns whose usage has SETTLED — a user `turn.prompt`
/// followed by a later `usage.record` — in the session's main wire. Drives the
/// usage refresh from turn COMPLETION (after the provider call records its
/// tokens) rather than turn start, so the fetched plan-usage reflects the turn
/// that just finished instead of lagging one behind. Sub-agent wires are never
/// read, so a delegation's turns can't drive the (network) fetch. `0` when the
/// main wire is unreadable or the latest turn hasn't produced output yet.
pub(crate) fn main_settled_turn_count(session_dir: &Path) -> u64 {
    let Some(wires) = read_agent_wires(session_dir) else {
        return 0;
    };
    let Some(main) = wires.iter().find(|wire| wire.is_main) else {
        return 0;
    };
    let Ok(raw) = std::fs::read_to_string(&main.wire) else {
        return 0;
    };
    count_settled_user_turns(&raw)
}

/// Number of MAIN-agent user turns whose consumption has landed — a
/// `turn.prompt origin.kind == "user"` followed later by a `usage.record`. A
/// turn still in flight (prompt written, no record yet) is not counted, so the
/// count advances at turn completion; multiple records in one turn credit it
/// once. Malformed or partial lines fail to parse and are skipped.
fn count_settled_user_turns(raw: &str) -> u64 {
    let mut settled: u64 = 0;
    let mut pending: u64 = 0;
    for dto in raw
        .lines()
        .filter_map(|line| serde_json::from_str::<KimiLineDto>(line).ok())
    {
        if is_user_turn_prompt(&dto) {
            pending += 1;
        } else if dto.record_type() == KimiRecordType::UsageRecord {
            settled += pending;
            pending = 0;
        }
    }
    settled
}

/// A `turn.prompt` record whose origin is a real user (not a delegated /
/// agent-originated prompt).
fn is_user_turn_prompt(dto: &KimiLineDto) -> bool {
    dto.record_type() == KimiRecordType::TurnPrompt
        && dto
            .origin
            .as_ref()
            .and_then(|origin| origin.kind.as_deref())
            == Some("user")
}

pub(super) struct AgentWire {
    pub(super) agent_id: String,
    pub(super) is_main: bool,
    pub(super) wire: PathBuf,
}

/// Enumerate the `agents/*/wire.jsonl` of a session from its `state.json`
/// `agents{}` map. Each wire is required to live UNDER `session_dir` so a
/// tampered `homedir` can't redirect reads outside the trusted session.
/// Shared by the status aggregator and the transcript supervisor.
pub(super) fn read_agent_wires(session_dir: &Path) -> Option<Vec<AgentWire>> {
    let raw = std::fs::read_to_string(session_dir.join("state.json")).ok()?;
    let state: KimiStateDto = serde_json::from_str(&raw).ok()?;
    let canonical_session = std::fs::canonicalize(session_dir).ok()?;
    let mut wires = Vec::new();
    for (agent_id, entry) in &state.agents {
        let wire = PathBuf::from(&entry.homedir).join("wire.jsonl");
        // Canonicalize (resolves `..` and symlinks) and require the result to
        // stay under the canonical session dir — a lexical `starts_with` would
        // let a tampered `homedir` escape via `..`/a symlinked subdir.
        let Ok(wire) = std::fs::canonicalize(&wire) else {
            continue;
        };
        if wire.starts_with(&canonical_session) && wire.is_file() {
            wires.push(AgentWire {
                agent_id: agent_id.clone(),
                is_main: entry.agent_type.as_deref() == Some("main"),
                wire,
            });
        }
    }
    (!wires.is_empty()).then_some(wires)
}

/// `<sessionDir>/state.json` — only the `agents{}` map matters here.
#[derive(Deserialize)]
struct KimiStateDto {
    #[serde(default)]
    agents: HashMap<String, KimiAgentDto>,
}

#[derive(Deserialize)]
struct KimiAgentDto {
    homedir: String,
    #[serde(rename = "type")]
    agent_type: Option<String>,
}

#[derive(Default)]
struct KimiFoldState {
    model: String,
    version: String,
    latest_usage: Option<KimiUsageDto>,
}

impl KimiFoldState {
    fn into_snapshot(self) -> StatusSnapshot {
        let usage = self.latest_usage.unwrap_or_default();
        let input_other = usage.input_other.unwrap_or(0);
        let output = usage.output.unwrap_or(0);
        let input_cache_read = usage.input_cache_read.unwrap_or(0);
        let input_cache_creation = usage.input_cache_creation.unwrap_or(0);

        let used = input_other
            .saturating_add(output)
            .saturating_add(input_cache_read)
            .saturating_add(input_cache_creation);
        let context_window_size = KIMI_CONTEXT_WINDOW_SIZE;

        let used_percentage = if context_window_size == 0 {
            None
        } else {
            Some(clamp_percentage(
                (used as f64 / context_window_size as f64) * 100.0,
            ))
        };
        let remaining_percentage = used_percentage
            .map(|used| clamp_percentage(100.0 - used))
            .unwrap_or(100.0);

        let context_window = ContextWindowStatus {
            used_percentage,
            remaining_percentage,
            context_window_size,
            total_input_tokens: input_other,
            total_output_tokens: output,
            current_usage: Some(CurrentUsage {
                input_tokens: input_other,
                output_tokens: output,
                cache_creation_input_tokens: input_cache_creation,
                cache_read_input_tokens: input_cache_read,
            }),
        };

        let cost = CostMetrics {
            total_cost_usd: None,
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

        let (model_id, model_display_name) = if self.model.is_empty() {
            ("unknown".to_string(), "unknown".to_string())
        } else {
            (self.model.clone(), self.model)
        };

        StatusSnapshot {
            agent_session_id: String::new(),
            model_id,
            model_display_name,
            version: self.version,
            context_window,
            cost,
            rate_limits,
            // The decode / supervisor sets this true once a real fetch lands.
            usage_fetched: false,
        }
    }
}

fn fold_line(state: &mut KimiFoldState, line: KimiLineDto) {
    match line.record_type() {
        KimiRecordType::Metadata => {
            if let Some(version) = line.app_version {
                state.version = version;
            }
        }
        KimiRecordType::ConfigUpdate => {
            if let Some(model) = line.model_alias {
                state.model = model;
            }
        }
        KimiRecordType::UsageRecord => {
            if let Some(model) = line.model {
                state.model = model;
            }
            if let Some(usage) = line.usage {
                state.latest_usage = Some(usage);
            }
        }
        KimiRecordType::AppendLoopEvent => {
            // `step.end` carries a per-step usage snapshot; use it as a
            // fallback when no `usage.record` has been seen yet.
            if let Some(event) = line.event {
                if let Some(usage) = event.usage {
                    state.latest_usage = Some(usage);
                }
            }
        }
        KimiRecordType::TurnPrompt | KimiRecordType::Other => {}
    }
}

fn clamp_percentage(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/agent/adapter/kimi/fixtures/sample_wire.jsonl");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {}", path.display(), e))
    }

    fn write_agent_wire(session_dir: &Path, agent: &str, contents: &str) -> PathBuf {
        let dir = session_dir.join("agents").join(agent);
        std::fs::create_dir_all(&dir).expect("agent dir");
        let wire = dir.join("wire.jsonl");
        std::fs::write(&wire, contents).expect("agent wire");
        wire
    }

    /// `/init`-style session: `main` only carries the model, the `agent-0`
    /// sub-agent carries the live token usage and is the most recently
    /// written wire — the aggregate takes the model from main and the
    /// context window from the active sub-agent.
    #[test]
    fn session_aggregate_takes_context_from_active_subagent() {
        use std::time::Duration;
        let session = tempfile::tempdir().expect("session");
        let dir = session.path();
        let main_wire = write_agent_wire(
            dir,
            "main",
            "{\"type\":\"config.update\",\"modelAlias\":\"kimi-code/kimi-for-coding\"}\n",
        );
        let sub_wire = write_agent_wire(
            dir,
            "agent-0",
            "{\"type\":\"config.update\",\"modelAlias\":\"kimi-code/kimi-for-coding\"}\n\
             {\"type\":\"usage.record\",\"usage\":{\"inputOther\":6492,\"output\":187,\"inputCacheRead\":58624,\"inputCacheCreation\":0}}\n",
        );
        std::fs::write(
            dir.join("state.json"),
            format!(
                "{{\"agents\":{{\"main\":{{\"homedir\":\"{}\",\"type\":\"main\"}},\"agent-0\":{{\"homedir\":\"{}\",\"type\":\"sub\"}}}}}}",
                main_wire.parent().unwrap().display(),
                sub_wire.parent().unwrap().display(),
            ),
        )
        .expect("state.json");

        let now = SystemTime::now();
        set_modified(&main_wire, now - Duration::from_secs(60));
        set_modified(&sub_wire, now);

        let snap = parse_session_aggregate(dir).expect("aggregate resolves");
        assert_eq!(snap.model_id, "kimi-code/kimi-for-coding");
        assert_eq!(snap.context_window.total_input_tokens, 6492);
        assert_eq!(
            snap.context_window
                .current_usage
                .expect("usage")
                .cache_read_input_tokens,
            58624
        );
    }

    /// A wire whose `homedir` points OUTSIDE the session dir is rejected, so
    /// the aggregate can't be steered to read an arbitrary path.
    #[test]
    fn session_aggregate_rejects_homedir_outside_session() {
        let session = tempfile::tempdir().expect("session");
        let outside = tempfile::tempdir().expect("outside");
        let dir = session.path();
        write_agent_wire(dir, "main", "{\"type\":\"config.update\"}\n");
        let escaped = outside.path().join("agents").join("main");
        std::fs::create_dir_all(&escaped).expect("escaped dir");
        std::fs::write(escaped.join("wire.jsonl"), "{\"type\":\"metadata\"}\n")
            .expect("escaped wire");
        std::fs::write(
            dir.join("state.json"),
            format!(
                "{{\"agents\":{{\"main\":{{\"homedir\":\"{}\",\"type\":\"main\"}}}}}}",
                escaped.display(),
            ),
        )
        .expect("state.json");
        assert!(
            parse_session_aggregate(dir).is_none(),
            "must not read a wire outside the session dir"
        );
    }

    /// The main-wire SETTLED user-turn count drives the usage refresh. Only a
    /// MAIN-agent `turn.prompt origin.kind=="user"` that has produced a later
    /// `usage.record` counts — an agent-origin (delegated) main prompt, a user
    /// prompt in a SUB-agent wire, and an in-flight main turn with no record
    /// yet are all excluded, so a delegation can't trigger a fetch and the
    /// count only advances once a turn's consumption has landed.
    #[test]
    fn main_settled_turn_count_counts_completed_user_turns_only() {
        let session = tempfile::tempdir().expect("session");
        let dir = session.path();
        let main_wire = write_agent_wire(
            dir,
            "main",
            "{\"type\":\"config.update\",\"modelAlias\":\"kimi-code/kimi-for-coding\"}\n\
             {\"type\":\"turn.prompt\",\"origin\":{\"kind\":\"user\"}}\n\
             {\"type\":\"usage.record\",\"usage\":{\"inputOther\":10,\"output\":2}}\n\
             {\"type\":\"turn.prompt\",\"origin\":{\"kind\":\"agent\"}}\n\
             {\"type\":\"turn.prompt\",\"origin\":{\"kind\":\"user\"}}\n\
             {\"type\":\"usage.record\",\"usage\":{\"inputOther\":20,\"output\":4}}\n\
             {\"type\":\"turn.prompt\",\"origin\":{\"kind\":\"user\"}}\n",
        );
        let sub_wire = write_agent_wire(
            dir,
            "agent-0",
            "{\"type\":\"turn.prompt\",\"origin\":{\"kind\":\"user\"}}\n\
             {\"type\":\"usage.record\",\"usage\":{\"inputOther\":5,\"output\":1}}\n",
        );
        std::fs::write(
            dir.join("state.json"),
            format!(
                "{{\"agents\":{{\"main\":{{\"homedir\":\"{}\",\"type\":\"main\"}},\"agent-0\":{{\"homedir\":\"{}\",\"type\":\"sub\"}}}}}}",
                main_wire.parent().unwrap().display(),
                sub_wire.parent().unwrap().display(),
            ),
        )
        .expect("state.json");

        // 2 settled user turns in main; the agent-origin main turn, the
        // sub-agent's settled turn, and the final in-flight user turn (no
        // record yet) are all excluded.
        assert_eq!(main_settled_turn_count(dir), 2);
    }

    /// Several `usage.record` lines within one turn credit it once, so the
    /// count is stable mid-turn and the supervisor doesn't re-fetch per step.
    #[test]
    fn settled_count_credits_a_multi_record_turn_once() {
        let raw = "{\"type\":\"turn.prompt\",\"origin\":{\"kind\":\"user\"}}\n\
                   {\"type\":\"usage.record\",\"usage\":{\"output\":1}}\n\
                   {\"type\":\"usage.record\",\"usage\":{\"output\":2}}\n";
        assert_eq!(count_settled_user_turns(raw), 1);
    }

    /// A user prompt with no following `usage.record` is in flight, so it does
    /// not advance the count until its consumption lands.
    #[test]
    fn settled_count_excludes_an_in_flight_turn() {
        let raw = "{\"type\":\"turn.prompt\",\"origin\":{\"kind\":\"user\"}}\n";
        assert_eq!(count_settled_user_turns(raw), 0);
    }

    fn set_modified(path: &Path, when: SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime")
            .set_modified(when)
            .expect("set mtime");
    }

    #[test]
    fn folds_fixture_into_snapshot() {
        let raw = fixture();
        let parsed = parse_wire("pty-test", &raw).expect("fixture folds");
        let event = parsed.event;

        assert_eq!(event.session_id, "pty-test");
        assert_eq!(event.model_id, "kimi-code/kimi-for-coding");
        assert_eq!(event.model_display_name, "kimi-code/kimi-for-coding");
        assert_eq!(event.version, "0.14.2");
        assert_eq!(event.context_window.context_window_size, 262_144);

        // Latest usage.record is the line-19 turn: inputOther=211,
        // output=35, inputCacheRead=16128, inputCacheCreation=0.
        let current = event
            .context_window
            .current_usage
            .expect("current usage present");
        assert_eq!(current.input_tokens, 211);
        assert_eq!(current.output_tokens, 35);
        assert_eq!(current.cache_read_input_tokens, 16128);
        assert_eq!(current.cache_creation_input_tokens, 0);

        assert_eq!(event.context_window.total_input_tokens, 211);
        assert_eq!(event.context_window.total_output_tokens, 35);

        // used = 211 + 35 + 16128 + 0 = 16374 → non-zero, small fraction.
        let used = event
            .context_window
            .used_percentage
            .expect("used percentage");
        assert!(used > 0.0 && used < 100.0, "used in range, got {}", used);

        assert_eq!(event.cost.total_cost_usd, None);
        assert_eq!(event.agent_session_id, "");
    }

    #[test]
    fn tolerates_truncated_trailing_line() {
        let raw = concat!(
            r#"{"type":"config.update","modelAlias":"kimi-code/kimi-for-coding"}"#,
            "\n",
            r#"{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":100,"output":10,"inputCacheRead":0,"inputCacheCreation":0}}"#,
            "\n",
            r#"{"type":"usage.record","model":"kimi-code"#, // partial — no trailing newline
        );
        let parsed = parse_wire("pty-trunc", raw).expect("truncated trail tolerated");
        // The complete second line is the latest folded usage.
        assert_eq!(parsed.event.context_window.total_input_tokens, 100);
        assert_eq!(parsed.event.model_id, "kimi-code/kimi-for-coding");
    }

    #[test]
    fn empty_wire_returns_defaults() {
        let parsed = parse_wire("pty-empty", "").expect("empty wire");
        assert_eq!(parsed.event.model_id, "unknown");
        assert_eq!(parsed.event.context_window.total_input_tokens, 0);
        // size is the constant; used_percentage is Some(0.0) for zero usage.
        assert_eq!(parsed.event.context_window.context_window_size, 262_144);
        assert_eq!(parsed.event.context_window.used_percentage, Some(0.0));
    }

    #[test]
    fn malformed_line_skipped_without_error() {
        let raw = concat!(
            r#"{"type":"config.update","modelAlias":"kimi-code/kimi-for-coding"}"#,
            "\n",
            "{not json\n",
            r#"{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":5,"output":1,"inputCacheRead":0,"inputCacheCreation":0}}"#,
            "\n",
        );
        let parsed = parse_wire("pty-malformed", raw).expect("malformed line skipped");
        assert_eq!(parsed.event.model_id, "kimi-code/kimi-for-coding");
        assert_eq!(parsed.event.context_window.total_input_tokens, 5);
    }

    #[test]
    fn step_end_usage_used_when_no_usage_record() {
        let raw = concat!(
            r#"{"type":"config.update","modelAlias":"kimi-code/kimi-for-coding"}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","usage":{"inputOther":42,"output":7,"inputCacheRead":3,"inputCacheCreation":1},"finishReason":"tool_use"}}"#,
            "\n",
        );
        let parsed = parse_wire("pty-step", raw).expect("step.end usage folds");
        assert_eq!(parsed.event.context_window.total_input_tokens, 42);
        let current = parsed.event.context_window.current_usage.expect("usage");
        assert_eq!(current.cache_creation_input_tokens, 1);
    }
}
