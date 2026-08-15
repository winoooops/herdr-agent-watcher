use std::time::Duration;

use chrono::DateTime;

/// Parse an ISO 8601 string (with or without fractional seconds) to
/// milliseconds since the Unix epoch. Returns None on parse failure.
pub fn parse_iso8601_ms(s: &str) -> Option<u64> {
    let dt = DateTime::parse_from_rfc3339(s).ok()?;
    let ms = dt.timestamp_millis();
    if ms < 0 {
        return None;
    }
    Some(ms as u64)
}

/// Compute the duration between two ISO 8601 timestamps. Falls back to the
/// provided `fallback` duration if either timestamp can't be parsed or the
/// computed range is negative.
pub fn compute_duration_ms(started_at: &str, finished_at: &str, fallback: Duration) -> u64 {
    match (parse_iso8601_ms(started_at), parse_iso8601_ms(finished_at)) {
        (Some(start), Some(end)) if end >= start => end - start,
        _ => {
            log::debug!("Falling back to Instant::elapsed for test-run duration");
            fallback.as_millis() as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso8601_no_fraction() {
        assert_eq!(
            parse_iso8601_ms("2026-04-28T12:00:00Z"),
            Some(1777377600000)
        );
    }

    #[test]
    fn parses_iso8601_with_fraction() {
        assert_eq!(
            parse_iso8601_ms("2026-04-28T12:00:00.500Z"),
            Some(1777377600500)
        );
    }

    #[test]
    fn returns_none_on_garbage() {
        assert_eq!(parse_iso8601_ms("not-a-date"), None);
        assert_eq!(parse_iso8601_ms(""), None);
    }

    #[test]
    fn duration_uses_timestamps_when_valid() {
        assert_eq!(
            compute_duration_ms(
                "2026-04-28T12:00:00Z",
                "2026-04-28T12:00:01.500Z",
                Duration::from_millis(0),
            ),
            1500
        );
    }

    #[test]
    fn duration_falls_back_when_end_before_start() {
        assert_eq!(
            compute_duration_ms(
                "2026-04-28T12:00:01Z",
                "2026-04-28T12:00:00Z",
                Duration::from_millis(42),
            ),
            42
        );
    }

    #[test]
    fn duration_falls_back_when_unparseable() {
        assert_eq!(
            compute_duration_ms("garbage", "garbage", Duration::from_millis(7)),
            7
        );
    }
}
