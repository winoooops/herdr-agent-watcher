//! Parse the kimi `/usages` plan-usage response into provider-neutral
//! `RateLimits`. Loose by design — kimi's own parser tolerates string-vs-number
//! values and `resetTime` / `resetAt` / `reset_at` spelling drift across
//! versions. Maps the weekly `usage` summary to `seven_day` and the 5-hour
//! window from `limits[]` to `five_hour`.

use serde_json::Value;

use crate::agent::types::{RateLimitInfo, RateLimits};

// A 5-hour window in seconds. The `limits[]` entry whose window spans this
// drives the `five_hour` bar; the top-level `usage` drives the weekly bar.
const FIVE_HOURS_SECS: u64 = 5 * 3600;

/// Map a `/usages` payload to `RateLimits`: weekly ← the top-level `usage`,
/// 5-hour ← the `limits[]` entry whose window is 5h. `None` on unparseable
/// input OR when NEITHER row parses (a valid-JSON error envelope / schema
/// drift), so a successful-but-empty response can't clobber a cached quota
/// with a bogus zero. An absent 5-hour window (but present weekly) still
/// yields a zeroed `five_hour` (the field is required) so weekly renders.
pub(crate) fn parse_usage_payload(raw: &str) -> Option<RateLimits> {
    let root: Value = serde_json::from_str(raw).ok()?;
    let seven_day = root.get("usage").and_then(usage_row);
    let five_hour = root
        .get("limits")
        .and_then(Value::as_array)
        .and_then(|limits| {
            limits
                .iter()
                .find(|item| window_is_five_hours(item.get("window")))
                .and_then(|item| usage_row(item.get("detail").unwrap_or(item)))
        });
    // Require at least one real limit row. A body that parses as JSON but
    // carries neither is not a usable quota — returning a zeroed `Some` would
    // let `fetch_rate_limits` overwrite a previously cached real quota with 0%.
    if five_hour.is_none() && seven_day.is_none() {
        return None;
    }
    Some(RateLimits {
        five_hour: five_hour.unwrap_or(RateLimitInfo {
            used_percentage: 0.0,
            resets_at: 0,
        }),
        seven_day,
    })
}

/// One usage row: `used_percentage` from `used` / `limit` (or `limit -
/// remaining`), `resets_at` from the reset timestamp. Values may be JSON
/// strings or numbers.
fn usage_row(raw: &Value) -> Option<RateLimitInfo> {
    let limit = number(raw.get("limit"))?;
    let used = number(raw.get("used"))
        .or_else(|| number(raw.get("remaining")).map(|rem| (limit - rem).max(0.0)))?;
    let used_percentage = if limit > 0.0 {
        ((used / limit) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let resets_at = raw
        .get("resetTime")
        .or_else(|| raw.get("resetAt"))
        .or_else(|| raw.get("reset_at"))
        .and_then(Value::as_str)
        .and_then(reset_to_epoch)
        .unwrap_or(0);
    Some(RateLimitInfo {
        used_percentage,
        resets_at,
    })
}

/// True when `window` spans 5 hours (`duration` × `timeUnit`). kimi uses
/// `{ duration: 300, timeUnit: "TIME_UNIT_MINUTE" }` for the 5-hour window.
fn window_is_five_hours(window: Option<&Value>) -> bool {
    let Some(window) = window else {
        return false;
    };
    let Some(duration) = number(window.get("duration")) else {
        return false;
    };
    let unit_secs = match window.get("timeUnit").and_then(Value::as_str) {
        Some(u) if u.contains("SECOND") => 1.0,
        Some(u) if u.contains("MINUTE") => 60.0,
        Some(u) if u.contains("HOUR") => 3600.0,
        Some(u) if u.contains("DAY") => 86400.0,
        _ => return false,
    };
    (duration * unit_secs) as u64 == FIVE_HOURS_SECS
}

/// A string or numeric JSON value as `f64` — kimi sends limits as `"100"`.
fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

/// ISO-8601 reset timestamp → Unix epoch seconds.
fn reset_to_epoch(raw: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp().max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The live api.kimi.com/coding/v1/usages response shape (2026-06-14):
    // weekly `usage` + a 300-minute (5h) window in `limits[]`.
    const LIVE: &str = r#"{
        "user": {"userId":"x","region":"REGION_CN"},
        "usage": {"limit":"100","used":"40","remaining":"60","resetTime":"2026-06-17T10:00:46.439476Z"},
        "limits": [
            {"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
             "detail":{"limit":"100","used":"17","remaining":"83","resetTime":"2026-06-14T08:00:46.439476Z"}}
        ],
        "totalQuota": {"limit":"100","remaining":"99"}
    }"#;

    #[test]
    fn maps_weekly_and_five_hour_from_live_shape() {
        let limits = parse_usage_payload(LIVE).expect("parses");
        // weekly ← usage: 40/100 = 40%
        let weekly = limits.seven_day.expect("weekly present");
        assert_eq!(weekly.used_percentage, 40.0);
        assert_eq!(weekly.resets_at, 1781690446); // 2026-06-17T10:00:46Z
                                                  // 5-hour ← the 300-minute window: 17/100 = 17%
        assert_eq!(limits.five_hour.used_percentage, 17.0);
        assert_eq!(limits.five_hour.resets_at, 1781424046); // 2026-06-14T08:00:46Z
    }

    #[test]
    fn used_derives_from_remaining_when_used_absent() {
        let row = usage_row(&serde_json::json!({"limit":"100","remaining":"30"})).expect("row");
        assert_eq!(row.used_percentage, 70.0);
    }

    #[test]
    fn accepts_numeric_values_too() {
        let row = usage_row(&serde_json::json!({"limit":200,"used":50})).expect("row");
        assert_eq!(row.used_percentage, 25.0);
    }

    #[test]
    fn no_five_hour_window_yields_zeroed_five_hour_but_keeps_weekly() {
        let raw = r#"{"usage":{"limit":"100","used":"10"},"limits":[]}"#;
        let limits = parse_usage_payload(raw).expect("parses");
        assert_eq!(limits.five_hour.used_percentage, 0.0);
        assert_eq!(limits.seven_day.expect("weekly").used_percentage, 10.0);
    }

    #[test]
    fn malformed_input_is_none() {
        assert!(parse_usage_payload("not json").is_none());
    }

    #[test]
    fn valid_json_without_any_limit_row_is_none() {
        // An application error envelope: valid JSON, but neither a usage
        // summary nor a 5-hour window. Must NOT pass as a zeroed success
        // (which would overwrite a cached real quota with 0%).
        assert!(parse_usage_payload(r#"{"error":"quota_exceeded","limits":[]}"#).is_none());
        // `usage` present but unparseable (no `limit`) + empty `limits`: still
        // no real row, so still None.
        assert!(parse_usage_payload(r#"{"usage":{"foo":"bar"},"limits":[]}"#).is_none());
    }
}
