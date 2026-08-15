//! Reading metrics out of raw status payloads. Provider differences live here
//! and nowhere else (§2.4).

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Context {
    pub pct: f64,
    pub used: u64,
    pub left: u64,
    pub window: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cache {
    pub pct: u8,
    pub read: u64,
    pub wrote: u64,
    pub fresh: u64,
    /// What `read` is a share OF. The gauge's extreme guards need the ratio, not
    /// the rounded percent, and only this module knows the provider's
    /// denominator (§2.4).
    pub denom: u64,
}

/// Percentage is taken as reported, never recomputed from buckets whose cache
/// semantics differ per adapter.
pub fn context(status: &Value) -> Option<Context> {
    let cw = status.get("contextWindow")?;
    let pct = cw.get("usedPercentage")?.as_f64()?;
    let window = cw.get("contextWindowSize")?.as_u64()?;
    if !pct.is_finite() || pct < 0.0 || window == 0 {
        return None;
    }
    let used = ((pct / 100.0) * window as f64).round().max(0.0) as u64;
    Some(Context {
        pct,
        used,
        left: window.saturating_sub(used),
        window,
    })
}

/// Claude, Kimi and OpenCode report disjoint buckets; Codex reports
/// `cachedInputTokens` as a SUBSET of input. An unrecognised agent renders
/// nothing rather than a plausible wrong number.
pub fn cache(status: &Value, canonical_agent: &str) -> Option<Cache> {
    let usage = status.get("contextWindow")?.get("currentUsage")?;
    let input = usage.get("inputTokens")?.as_u64()?;
    let created = usage
        .get("cacheCreationInputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let read = usage
        .get("cacheReadInputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let (denom, fresh) = match canonical_agent {
        "claude" | "kimi" | "opencode" => (input + created + read, input),
        "codex" => (input, input.saturating_sub(read)),
        _ => return None,
    };
    if denom == 0 {
        return None;
    }
    let pct = ((read as u128 * 100 + denom as u128 / 2) / denom as u128).min(100) as u8;
    Some(Cache {
        pct,
        read,
        wrote: created,
        fresh,
        denom,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_denominator_is_provider_specific() {
        let claude = json!({"contextWindow": {"currentUsage": {
            "inputTokens": 500, "cacheCreationInputTokens": 0, "cacheReadInputTokens": 500}}});
        assert_eq!(
            cache(&claude, "claude"),
            Some(Cache {
                pct: 50,
                read: 500,
                wrote: 0,
                fresh: 500,
                denom: 1000
            })
        );

        let codex = json!({"contextWindow": {"currentUsage": {
            "inputTokens": 9000, "cacheCreationInputTokens": 0, "cacheReadInputTokens": 7000}}});
        let c = cache(&codex, "codex").expect("codex cache");
        assert_eq!(c.pct, 78);
        assert_eq!(
            c.fresh, 2000,
            "fresh = input - cacheRead for a subset provider"
        );
    }

    #[test]
    fn cache_is_unavailable_for_zero_denominators_and_unknown_agents() {
        let zero = json!({"contextWindow": {"currentUsage": {
            "inputTokens": 0, "cacheCreationInputTokens": 0, "cacheReadInputTokens": 0}}});
        assert_eq!(cache(&zero, "claude"), None);
        let ok = json!({"contextWindow": {"currentUsage": {
            "inputTokens": 10, "cacheCreationInputTokens": 0, "cacheReadInputTokens": 5}}});
        assert_eq!(
            cache(&ok, "mystery-agent"),
            None,
            "unverified accounting renders nothing"
        );
    }

    #[test]
    fn a_percentage_above_100_is_reported_not_hidden() {
        let v = json!({"contextWindow": {"usedPercentage": 140.0,
                                         "contextWindowSize": 1_000_000}});
        let c = context(&v).expect("still available");
        assert_eq!(c.used, 1_400_000, "the overshoot is the news");
        assert_eq!(c.left, 0, "saturating, never negative");
        assert_eq!(c.pct, 140.0, "unclamped — percent() renders it as 99+%");
    }

    #[test]
    fn cache_covers_every_provider_and_alias() {
        let disjoint = json!({"contextWindow": {"currentUsage": {
            "inputTokens": 500, "cacheCreationInputTokens": 0,
            "cacheReadInputTokens": 500}}});
        for agent in ["claude", "kimi", "opencode"] {
            let c = cache(&disjoint, agent).unwrap_or_else(|| panic!("{agent}"));
            assert_eq!(c.pct, 50, "{agent} sums its buckets");
            assert_eq!(c.fresh, 500, "{agent}");
            assert_eq!(c.denom, 1000, "{agent}");
        }
        assert_eq!(
            crate::sidebar::agent_ids::canonical("claude-code"),
            Some("claude")
        );
    }

    #[test]
    fn context_is_unavailable_when_the_window_is_zero() {
        let v = json!({"contextWindow": {"usedPercentage": 50.0, "contextWindowSize": 0}});
        assert_eq!(context(&v), None, "0 means unknown, not a real window");
        let v = json!({"contextWindow": {"usedPercentage": f64::NAN, "contextWindowSize": 1000}});
        assert_eq!(context(&v), None);
    }

    #[test]
    fn context_derives_used_and_left_from_the_reported_percentage() {
        let v = json!({"contextWindow": {"usedPercentage": 50.0, "contextWindowSize": 1_000_000}});
        let c = context(&v).expect("context");
        assert_eq!(c.pct, 50.0);
        assert_eq!(c.used, 500_000);
        assert_eq!(c.left, 500_000);
    }
}
