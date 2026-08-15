//! Debug diagnostics for the status watcher runtime.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Outcome of a transcript-start attempt. Returned so event diagnostics can
/// report an accurate status without re-walking the transcript path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TxOutcome {
    Started,
    Replaced,
    AlreadyRunning,
    Missing,
    OutsidePath,
    NotFile,
    /// Transcript path failed validation BEFORE filesystem resolution
    /// — currently triggered by null-byte detection in the input
    /// string. Distinct from `NotFile` (path canonicalised but isn't
    /// regular) and `OutsidePath` (canonical path escaped the trust
    /// root) so log scrapers / SIEM rules can flag potentially
    /// adversarial input separately from ordinary filesystem-state
    /// misclassifications. Claude review on PR #153.
    InvalidPath,
    StartFailed,
    NoPath,
    ParseError,
    /// `start_or_replace` short-circuited because the caller's
    /// WatcherHandle has been displaced (`alive` flag set to false by
    /// `AgentWatcherState::insert` under the per-session gate). This
    /// is a normal, expected condition that fires on every restart for
    /// any callbacks still in-flight from the displaced handle — it is
    /// NOT a tail-spawn failure and MUST be distinguishable from
    /// `StartFailed` so monitoring rules alerting on
    /// `WARN.*Failed to start transcript tailing` don't fire on every
    /// restart (PR #302 cycle 13 F1 — cycle-10's alive check
    /// originally reused the catch-all Err arm in
    /// `maybe_start_transcript`, which logged at warn level
    /// indistinguishable from real failures).
    Displaced,
}

impl TxOutcome {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Replaced => "replaced",
            Self::AlreadyRunning => "already_running",
            Self::Missing => "missing",
            Self::OutsidePath => "outside_path",
            Self::NotFile => "not_file",
            Self::InvalidPath => "invalid_path",
            Self::StartFailed => "start_failed",
            Self::NoPath => "no_path",
            Self::ParseError => "parse_error",
            Self::Displaced => "displaced",
        }
    }
}

/// Per-source timing state. Notify and poll each keep their own timing so the
/// logs can show which source is firing and how far apart events are.
#[derive(Default)]
pub(super) struct EventTiming {
    last_event_at: Option<Instant>,
}

/// Cross-source transcript-path history. This is shared by inline, notify,
/// and poll so a speculative path observed by one source and a resolved path
/// observed by another still records a path change.
#[derive(Default)]
pub(super) struct PathHistory {
    last_tx_path: Option<String>,
    same_path_repeat: u32,
}

impl PathHistory {
    pub(super) fn observe(&mut self, tx_path: Option<&str>) -> Option<String> {
        match (tx_path, self.last_tx_path.as_deref()) {
            (Some(new), Some(old)) if new != old => {
                let old_owned = old.to_string();
                self.last_tx_path = Some(new.to_string());
                self.same_path_repeat = 1;
                Some(old_owned)
            }
            (Some(new), None) => {
                self.last_tx_path = Some(new.to_string());
                self.same_path_repeat = 1;
                None
            }
            (Some(_), Some(_)) => {
                self.same_path_repeat = self.same_path_repeat.saturating_add(1);
                None
            }
            (None, _) => {
                self.last_tx_path = None;
                self.same_path_repeat = 0;
                None
            }
        }
    }
}

pub(super) fn short_sid(sid: &str) -> &str {
    sid.get(..8).unwrap_or(sid)
}

fn short_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.get(..8).unwrap_or(s).to_string())
        .unwrap_or_else(|| "?".to_string())
}

pub(super) fn record_event_diag(
    timing: Option<&Mutex<EventTiming>>,
    path_history: &Mutex<PathHistory>,
    source: &'static str,
    sid: &str,
    total: Duration,
    outcome: TxOutcome,
    tx_path: Option<&str>,
) {
    if !cfg!(debug_assertions) {
        return;
    }

    let dt_label = match timing {
        Some(t) => {
            let now = Instant::now();
            let mut t = t.lock().expect("watcher timing lock");
            let dt = t
                .last_event_at
                .map(|prev| now.duration_since(prev))
                .unwrap_or(Duration::ZERO);
            t.last_event_at = Some(now);
            format!("dt={}ms", dt.as_millis())
        }
        None => "dt=n/a".to_string(),
    };

    let (path_change, repeat) = {
        let mut h = path_history.lock().expect("watcher path-history lock");
        let path_change = h.observe(tx_path);
        (path_change, h.same_path_repeat)
    };

    if let Some(old) = path_change {
        log::info!(
            "watcher.tx_path_change session={} from={} to={}",
            short_sid(sid),
            short_path(&old),
            tx_path.map(short_path).unwrap_or_else(|| "(none)".into()),
        );
    }

    let total_ms = total.as_millis();
    let tx_path_short = tx_path.map(short_path).unwrap_or_else(|| "(none)".into());
    if total_ms > 50 {
        log::warn!(
            "watcher.slow_event source={} session={} {} total={}ms tx_status={} tx_path={} repeat={}",
            source,
            short_sid(sid),
            dt_label,
            total_ms,
            outcome.label(),
            tx_path_short,
            repeat,
        );
    } else {
        log::info!(
            "watcher.event source={} session={} {} total={}ms tx_status={} tx_path={} repeat={}",
            source,
            short_sid(sid),
            dt_label,
            total_ms,
            outcome.label(),
            tx_path_short,
            repeat,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_history_first_observation_sets_path_and_repeat_1() {
        let mut h = PathHistory::default();
        let r = h.observe(Some("path-a"));
        assert!(
            r.is_none(),
            "first observation must not report a path change"
        );
        assert_eq!(h.last_tx_path.as_deref(), Some("path-a"));
        assert_eq!(h.same_path_repeat, 1);
    }

    #[test]
    fn path_history_repeat_increments_on_same_path() {
        let mut h = PathHistory::default();
        h.observe(Some("path-a"));
        h.observe(Some("path-a"));
        assert_eq!(h.same_path_repeat, 2);

        let r = h.observe(Some("path-a"));
        assert!(
            r.is_none(),
            "same-path observation must not report a path change"
        );
        assert_eq!(h.same_path_repeat, 3);
    }

    #[test]
    fn path_history_path_change_returns_old_and_resets_repeat() {
        let mut h = PathHistory::default();
        h.observe(Some("path-a"));
        h.observe(Some("path-a"));
        assert_eq!(h.same_path_repeat, 2);

        let r = h.observe(Some("path-b"));
        assert_eq!(
            r.as_deref(),
            Some("path-a"),
            "path change must return the previous value"
        );
        assert_eq!(h.last_tx_path.as_deref(), Some("path-b"));
        assert_eq!(
            h.same_path_repeat, 1,
            "streak counter resets to 1 on a fresh path"
        );
    }

    #[test]
    fn path_history_no_path_resets_streak_after_repeat() {
        let mut h = PathHistory::default();
        h.observe(Some("path-a"));
        h.observe(Some("path-a"));
        assert_eq!(h.same_path_repeat, 2);

        let r = h.observe(None);
        assert!(r.is_none());
        assert_eq!(h.last_tx_path, None, "no-path must clear the path cache");
        assert_eq!(h.same_path_repeat, 0, "no-path must reset the streak");

        let r = h.observe(Some("path-a"));
        assert!(
            r.is_none(),
            "the next path-bearing event after a no-path is treated as fresh"
        );
        assert_eq!(h.same_path_repeat, 1, "streak starts at 1, not 3");
    }

    #[test]
    fn path_history_no_path_when_already_no_path_is_idempotent() {
        let mut h = PathHistory::default();
        let r = h.observe(None);
        assert!(r.is_none());
        assert_eq!(h.last_tx_path, None);
        assert_eq!(h.same_path_repeat, 0);
    }

    #[test]
    fn short_sid_truncates_long_to_8_chars() {
        assert_eq!(short_sid("abcdefghijklmnop"), "abcdefgh");
    }

    #[test]
    fn short_sid_returns_input_unchanged_when_short() {
        assert_eq!(short_sid("abc"), "abc");
        assert_eq!(short_sid(""), "");
    }

    #[test]
    fn short_sid_handles_uuid_form() {
        assert_eq!(
            short_sid("ddb8d9f1-30b1-43dc-a1a2-405aaaf95e14"),
            "ddb8d9f1"
        );
    }

    #[test]
    fn short_path_extracts_basename_without_extension() {
        assert_eq!(
            short_path("/home/x/projects/abcdefghijklm.jsonl"),
            "abcdefgh"
        );
        assert_eq!(short_path("/x/y/short.jsonl"), "short");
    }

    #[test]
    fn short_path_handles_input_without_directory() {
        assert_eq!(short_path("nofileonly.jsonl"), "nofileon");
    }

    #[test]
    fn short_path_returns_question_mark_when_no_basename() {
        assert_eq!(short_path("/"), "?");
    }

    #[test]
    fn tx_outcome_label_covers_every_variant() {
        // Compile-time enforcement: `expected_label` below is an
        // exhaustive match. Adding a new `TxOutcome` variant without
        // an arm here produces a compile error, so the contributor
        // is forced to acknowledge the new variant and supply its
        // expected label. Rust has no built-in "iter all variants of
        // enum X" without a proc-macro like `strum::EnumIter`, so the
        // `outcomes` list below remains manually populated — but the
        // exhaustive match is enough to put a new variant on the
        // contributor's radar at compile time. They will then add the
        // variant to `outcomes` so it gets exercised by the assertion
        // loop (cycle-4 retry-1 of Claude review on PR #153, F8).
        fn expected_label(outcome: TxOutcome) -> &'static str {
            match outcome {
                TxOutcome::Started => "started",
                TxOutcome::Replaced => "replaced",
                TxOutcome::AlreadyRunning => "already_running",
                TxOutcome::Missing => "missing",
                TxOutcome::OutsidePath => "outside_path",
                TxOutcome::NotFile => "not_file",
                TxOutcome::InvalidPath => "invalid_path",
                TxOutcome::StartFailed => "start_failed",
                TxOutcome::NoPath => "no_path",
                TxOutcome::ParseError => "parse_error",
                TxOutcome::Displaced => "displaced",
            }
        }

        let outcomes = [
            TxOutcome::Started,
            TxOutcome::Replaced,
            TxOutcome::AlreadyRunning,
            TxOutcome::Missing,
            TxOutcome::OutsidePath,
            TxOutcome::NotFile,
            TxOutcome::InvalidPath,
            TxOutcome::StartFailed,
            TxOutcome::NoPath,
            TxOutcome::ParseError,
            TxOutcome::Displaced,
        ];

        for outcome in outcomes.iter().copied() {
            assert_eq!(
                outcome.label(),
                expected_label(outcome),
                "label() for {:?} should be {:?}",
                outcome,
                expected_label(outcome)
            );
        }
    }
}
