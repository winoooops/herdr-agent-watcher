//! opencode session locator (filesystem, pid-primary, fresh-in-cwd fallback).
//!
//! Resolves a PTY attach to the `<sessionID>.jsonl` the vimeflow bridge plugin
//! is writing for the opencode session Vimeflow detected. Resolution reads the
//! bridge dir's `index.jsonl` (each row `{sessionID, pid, directory, slug,
//! time}`) on EVERY `locate` call.
//!
//! **pid is the primary key.** The bridge plugin runs inside the opencode
//! server process and writes `process.pid` on each index row — exactly the pid
//! Vimeflow's process-tree scan reports as `agent_pid`. So among fresh rows
//! (`time` epoch-ms `>= pty_start − SLACK`), the one whose `pid == agent_pid`
//! is THIS attach's session; bind it directly. The equality is verified against
//! live data (the detected `agent_pid` matched the bridge row's `pid`). pid
//! also disambiguates two opencode panes that share a project dir, and (via a
//! fresh locator built by reattach) picks the active session after `/clear` —
//! the newest pid-matched row.
//!
//! **cwd is the fallback, not the key.** Vimeflow's tracked cwd comes from
//! OSC 7, but opencode's TUI does not emit OSC 7, so the tracked cwd stays
//! frozen at the pane's spawn dir (e.g. `~`) while opencode actually runs in a
//! project subdir the bridge records as `directory`. A `directory == cwd` match
//! therefore silently fails to bind the live session (the bug this resolver was
//! re-shaped to fix). It is used only when no fresh index row carries the
//! detected `agent_pid` (pid detection drifted to a wrapper/child); there it
//! still binds only an unambiguous single fresh same-cwd session. The freshness
//! floor rejects a stale row left by an earlier run in either path.
//!
//! **Re-resolution.** Each `locate` re-reads the index and re-resolves so a
//! re-invoked `locate` (reattach, Part 2) can notice a current, unambiguous
//! session. `self.resolved` is also an identity guard: once a locator has bound
//! a session, a later same-cwd row for a different session must not make that
//! watcher jump panes. Ambiguous same-cwd rows therefore return the cached
//! session if there is one, or `Err` so the runtime retries instead of tailing
//! the wrong transcript.
//!
//! Unlike the Kimi locator there is no `/proc` fast-path: the bridge dir is a
//! Vimeflow-owned XDG path, so resolution is pure filesystem on every OS. A
//! missing `index.jsonl` or zero usable rows (with no cached fallback) is the
//! startup-window signal — the locator returns `Err` and the runtime retries,
//! exactly like Kimi returns `Err` before its session dir is written. The
//! chosen `<sessionID>.jsonl`'s existence is the tail engine's concern, not the
//! locator's (matches Kimi's fallback path / data-file split).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::adapter::traits::StatusSourceLocator;
use crate::agent::adapter::types::{LocatedStatusSource, ValidateTranscriptError};

use super::transcript_dto::OpencodeIndexRowDto;

/// Slack (ms) subtracted from `pty_start` before the cwd-fallback freshness
/// check, so a session whose index row was written a moment before the PTY
/// clock was read still counts as fresh. Mirrors Kimi's freshness-slack idea
/// (3s there); 5s here gives the bridge's `session.created` → `index.jsonl`
/// append a slightly wider window over the OSC-detected attach.
const SLACK_MS: i64 = 5_000;

/// Filesystem pid-primary opencode locator. Holds the last successful
/// resolve `(sessionID, status_path, resolved_directory)` behind an
/// `Arc<Mutex<…>>` (the Kimi shared-state pattern) as a transient-empty-read
/// FALLBACK — `locate` re-reads the index and re-resolves on every call. M5's
/// validator delegates to [`OpenCodeLocator::effective_bridge_root`].
pub(crate) struct OpenCodeLocator {
    /// The bridge dir (`trust_root`) — `index.jsonl` and every
    /// `<sessionID>.jsonl` live directly under it.
    bridge_root: PathBuf,
    /// The detected opencode process's pid — the PRIMARY binding key. The bridge
    /// plugin writes `process.pid`, which equals this; the fresh index row
    /// carrying it names this attach's session (see module docs).
    agent_pid: u32,
    /// PTY start instant; both resolve paths reject rows older than
    /// `pty_start − SLACK`.
    pty_start: SystemTime,
    /// `(sessionID, status_path, resolved_directory)` of the last successful
    /// `locate`. `None` until
    /// the first resolve. NOT a short-circuit — used only as a fallback when a
    /// re-`locate`'s index read yields no qualifying row, so a transient empty
    /// read doesn't drop a live binding. Overwritten on every fresh resolve.
    resolved: Arc<Mutex<Option<(String, PathBuf, Option<PathBuf>)>>>,
}

impl OpenCodeLocator {
    pub(crate) fn new(bridge_root: PathBuf, agent_pid: u32, pty_start: SystemTime) -> Self {
        Self {
            bridge_root,
            agent_pid,
            pty_start,
            resolved: Arc::new(Mutex::new(None)),
        }
    }

    /// The trust root the validator and tailer scope every transcript path to.
    /// M5's `validate_transcript_path_with_root` delegates here so both sides
    /// resolve the root from one source.
    pub(crate) fn effective_bridge_root(&self) -> &Path {
        &self.bridge_root
    }

    fn index_path(&self) -> PathBuf {
        self.bridge_root.join("index.jsonl")
    }

    /// `<bridge_root>/<session_id>.jsonl`.
    fn status_path_for(&self, session_id: &str) -> PathBuf {
        self.bridge_root.join(format!("{session_id}.jsonl"))
    }

    /// Build (and cache) a [`LocatedStatusSource`] for a resolved session id.
    fn locate_session(
        &self,
        session_id: String,
        resolved_directory: Option<PathBuf>,
    ) -> LocatedStatusSource {
        let status_path = self.status_path_for(&session_id);
        *self.resolved.lock().expect("opencode resolved lock") = Some((
            session_id.clone(),
            status_path.clone(),
            resolved_directory.clone(),
        ));
        located_from(
            status_path,
            self.bridge_root.clone(),
            session_id,
            resolved_directory,
        )
    }

    /// All usable index rows (a `sessionID` is required; everything else is
    /// optional). Malformed lines are skipped. `Err` when `index.jsonl` is
    /// absent OR has zero usable rows — the not-ready / startup-window signal.
    fn read_index_rows(&self) -> Result<Vec<OpencodeIndexRowDto>, String> {
        let index_path = self.index_path();
        let raw = std::fs::read_to_string(&index_path).map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                format!(
                    "opencode index not ready: {} does not exist yet",
                    index_path.display()
                )
            } else {
                format!(
                    "opencode index not ready: failed to read {}: {}",
                    index_path.display(),
                    e
                )
            }
        })?;

        let rows: Vec<OpencodeIndexRowDto> = raw
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            // Lenient parse: skip a malformed line rather than poison the read.
            .filter_map(|line| serde_json::from_str::<OpencodeIndexRowDto>(line).ok())
            // A row with no sessionID can't name a `<sessionID>.jsonl`.
            .filter(|row| row.session_id.is_some())
            .collect();

        if rows.is_empty() {
            return Err(format!(
                "opencode index not ready: no usable rows in {}",
                index_path.display()
            ));
        }
        Ok(rows)
    }

    /// The fresh sessionID carrying `pid == agent_pid`, if any — the PRIMARY
    /// resolution path. Among rows fresh by `time >= pty_start − SLACK` whose
    /// `pid` equals the detected `agent_pid`:
    ///
    /// * if this locator already bound one of them, keep it (identity guard —
    ///   don't jump sessions mid-life; reattach owns rotation);
    /// * otherwise the newest by `time` is the active session. Distinct sessions
    ///   under one pid are sequential sessions of the same opencode server (one
    ///   pane), so newest-wins is the live one, not a cross-pane guess.
    ///
    /// `None` when no fresh row carries `agent_pid`, or when a cache exists but
    /// is no longer pid-matched, so the cwd fallback is tried first;
    /// `cached_or_err` preserves the binding only if cwd also finds nothing.
    fn resolve_by_pid(&self, rows: &[OpencodeIndexRowDto]) -> Option<String> {
        let floor = self.freshness_floor_ms();
        let candidates: Vec<&OpencodeIndexRowDto> = rows
            .iter()
            .filter(|row| row.time.is_some_and(|time| time >= floor))
            .filter(|row| row.pid == Some(self.agent_pid as u64))
            .collect();

        if candidates.is_empty() {
            return None;
        }

        if let Some(cached_session_id) = self.cached_session_id() {
            if candidates
                .iter()
                .any(|row| row.session_id.as_deref() == Some(cached_session_id.as_str()))
            {
                return Some(cached_session_id);
            }
            return None;
        }

        candidates
            .iter()
            .max_by_key(|row| row.time.expect("filtered to Some above"))
            .and_then(|row| row.session_id.clone())
    }

    /// The fresh same-cwd sessionID, if unambiguous. A row qualifies when its
    /// canonicalized `directory == cwd` (string-eq fallback if either side fails
    /// to canonicalize) AND its `time >= pty_start − SLACK`.
    ///
    /// Multiple distinct sessionIDs in the same cwd are ambiguous: recency alone
    /// cannot tell two same-project panes apart. If the locator already has a
    /// cached binding and that session is still present, keep it; otherwise fail
    /// closed so the runtime retries rather than rebinding to a different pane.
    fn resolve_by_cwd(
        &self,
        rows: &[OpencodeIndexRowDto],
        cwd: &Path,
    ) -> Result<Option<String>, String> {
        let floor = self.freshness_floor_ms();
        let candidates: Vec<&OpencodeIndexRowDto> = rows
            .iter()
            .filter(|row| row.time.is_some_and(|time| time >= floor))
            .filter(|row| {
                row.directory
                    .as_deref()
                    .is_some_and(|dir| same_directory(dir, cwd))
            })
            .collect();

        if let Some(cached_session_id) = self.cached_session_id() {
            if candidates
                .iter()
                .any(|row| row.session_id.as_deref() == Some(cached_session_id.as_str()))
            {
                return Ok(Some(cached_session_id));
            }
            return Ok(None);
        }

        let mut distinct_session_ids: Vec<&str> = Vec::new();
        for row in &candidates {
            if let Some(session_id) = row.session_id.as_deref() {
                if !distinct_session_ids.contains(&session_id) {
                    distinct_session_ids.push(session_id);
                }
            }
        }

        if distinct_session_ids.len() > 1 {
            return Err(format!(
                "opencode index not ready: {} fresh sessions share cwd={} in {}; refusing recency-only binding",
                distinct_session_ids.len(),
                cwd.display(),
                self.index_path().display(),
            ));
        }

        Ok(candidates
            .iter()
            // Same sessionID only: newest `time`. Tiebreaker: `pid == agent_pid`.
            .max_by_key(|row| {
                (
                    row.time.expect("filtered to Some above"),
                    row.pid == Some(self.agent_pid as u64),
                )
            })
            .and_then(|row| row.session_id.clone()))
    }

    /// `pty_start − SLACK` as epoch-ms; the cwd-fallback freshness floor.
    /// Saturates at `i64::MIN` for a pre-epoch / unreadable clock so the gate
    /// never wraps into rejecting everything.
    fn freshness_floor_ms(&self) -> i64 {
        let start_ms = self
            .pty_start
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(i64::MIN);
        start_ms.saturating_sub(SLACK_MS)
    }
}

impl StatusSourceLocator for OpenCodeLocator {
    fn locate(&self, cwd: &Path, _session_id: &str) -> Result<LocatedStatusSource, String> {
        // Re-resolve on every call. The index read may transiently yield zero
        // usable rows — fall back to a prior resolve rather than dropping a live
        // binding.
        let rows = match self.read_index_rows() {
            Ok(rows) => rows,
            Err(not_ready) => return self.cached_or_err(not_ready),
        };

        // Primary: the fresh row whose `pid == agent_pid` is THIS attach's
        // session. Binds correctly even when Vimeflow's OSC7-tracked cwd never
        // caught up to opencode's real cwd — which `resolve_by_cwd` cannot.
        if let Some(session_id) = self.resolve_by_pid(&rows) {
            let resolved_directory = resolved_directory_for(&rows, &session_id);

            return Ok(self.locate_session(session_id, resolved_directory));
        }

        // Fallback: an unambiguous fresh same-cwd session, for the rare case
        // where the detected `agent_pid` carries no index row (pid detection
        // drifted to a wrapper/child).
        match self.resolve_by_cwd(&rows, cwd) {
            Ok(Some(session_id)) => {
                let resolved_directory = resolved_directory_for(&rows, &session_id);

                Ok(self.locate_session(session_id, resolved_directory))
            }
            Ok(None) => self.cached_or_err(format!(
                "opencode index not ready: no fresh session for pid={} or cwd={} in {}",
                self.agent_pid,
                cwd.display(),
                self.index_path().display(),
            )),
            Err(not_ready) => self.cached_or_err(not_ready),
        }
    }
}

impl OpenCodeLocator {
    fn cached_session_id(&self) -> Option<String> {
        self.resolved
            .lock()
            .expect("opencode resolved lock")
            .as_ref()
            .map(|(session_id, _, _)| session_id.clone())
    }

    /// Fallback for a `locate` that found no qualifying row: return the last
    /// successful resolve if one is cached, else surface the not-ready error so
    /// the runtime retries. Keeps a live binding alive across a transient empty
    /// index read; the cache is only ever populated by a fresh resolve.
    fn cached_or_err(&self, not_ready: String) -> Result<LocatedStatusSource, String> {
        match self
            .resolved
            .lock()
            .expect("opencode resolved lock")
            .clone()
        {
            Some((session_id, status_path, resolved_directory)) => Ok(located_from(
                status_path,
                self.bridge_root.clone(),
                session_id,
                resolved_directory,
            )),
            None => Err(not_ready),
        }
    }
}

/// Build a [`LocatedStatusSource`] from a resolved status path + session id.
/// `static_transcript_hint` carries the same path so M5's
/// `TranscriptPathSource::static_hint` can surface it at attach.
fn located_from(
    status_path: PathBuf,
    bridge_root: PathBuf,
    session_id: String,
    resolved_directory: Option<PathBuf>,
) -> LocatedStatusSource {
    LocatedStatusSource {
        static_transcript_hint: Some(status_path.to_string_lossy().into_owned()),
        status_path,
        trust_root: bridge_root,
        agent_session_id: Some(session_id),
        resolved_directory,
    }
}

fn resolved_directory_for(rows: &[OpencodeIndexRowDto], session_id: &str) -> Option<PathBuf> {
    rows.iter()
        .filter(|row| row.session_id.as_deref() == Some(session_id))
        .max_by_key(|row| row.time)
        .and_then(|row| row.directory.as_ref())
        .map(PathBuf::from)
}

/// True when index `directory` and the attach `cwd` name the same directory.
/// Canonicalizes both (resolving symlinks / `..`); falls back to a raw
/// `Path`-eq when either side fails to canonicalize (a not-yet-created cwd or
/// a removed `directory`). Equality, not ancestry — opencode records the
/// session's own cwd, so a deeper pane cwd is a different session, not a match.
fn same_directory(directory: &str, cwd: &Path) -> bool {
    let dir_path = Path::new(directory);
    match (std::fs::canonicalize(dir_path), std::fs::canonicalize(cwd)) {
        (Ok(dir), Ok(cwd)) => dir == cwd,
        _ => dir_path == cwd,
    }
}

/// Validate a raw transcript path (NUL check → canonicalize → must be under the
/// canonicalized bridge root → must be a `*.jsonl` file) against the locator's
/// bridge root. Mirrors `kimi/transcript.rs`'s validator for the canonicalize +
/// under-root logic, with the extra `.jsonl` extension gate the spec requires.
pub(crate) fn validate_transcript_path_with_root(
    raw: &str,
    bridge_root: &Path,
) -> Result<PathBuf, ValidateTranscriptError> {
    if raw.bytes().any(|b| b == 0) {
        return Err(ValidateTranscriptError::InvalidPath(
            "transcript path contains null byte".to_string(),
        ));
    }

    let path = PathBuf::from(raw);
    let canonical = std::fs::canonicalize(&path).map_err(|e| {
        let kind = e.kind();
        if kind == ErrorKind::NotFound {
            return ValidateTranscriptError::NotFound(path.clone());
        }
        if kind == ErrorKind::PermissionDenied {
            if let Ok(false) = path.try_exists() {
                return ValidateTranscriptError::NotFound(path.clone());
            }
        }
        ValidateTranscriptError::Other(format!("invalid transcript path '{}': {}", raw, e))
    })?;

    if !canonical.is_file() {
        return Err(ValidateTranscriptError::NotAFile(canonical));
    }

    let bridge_root = std::fs::canonicalize(bridge_root).map_err(|e| {
        ValidateTranscriptError::Other(format!(
            "cannot resolve opencode bridge root '{}': {}",
            bridge_root.display(),
            e
        ))
    })?;

    if !canonical.starts_with(&bridge_root) {
        return Err(ValidateTranscriptError::OutsideRoot {
            path: canonical,
            root: bridge_root,
        });
    }

    // The bridge writes only `<sessionID>.jsonl`; reject anything else under
    // the root (a stray non-transcript file) as not-a-file-shape.
    let is_jsonl = canonical
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));
    if !is_jsonl {
        return Err(ValidateTranscriptError::InvalidPath(format!(
            "transcript path is not a .jsonl file: {}",
            canonical.display()
        )));
    }

    Ok(canonical)
}

/// Convert ms-since-the-Unix-epoch to a `SystemTime` for test construction.
#[cfg(test)]
fn epoch_ms(ms: i64) -> SystemTime {
    let duration = std::time::Duration::from_millis(ms.unsigned_abs());
    if ms < 0 {
        UNIX_EPOCH - duration
    } else {
        UNIX_EPOCH + duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A bridge dir under a tempdir, plus a constructor for the locator.
    struct Bridge {
        _tmp: tempfile::TempDir,
        root: PathBuf,
    }

    impl Bridge {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("bridge tempdir");
            let root = tmp.path().to_path_buf();
            Self { _tmp: tmp, root }
        }

        fn write_index(&self, rows: &[serde_json::Value]) {
            let raw = rows
                .iter()
                .map(|row| row.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(self.root.join("index.jsonl"), format!("{raw}\n")).expect("write index");
        }

        /// Touch `<sessionID>.jsonl` so a tailer could open it (not required by
        /// the locator, but real on disk for the validator tests).
        fn write_session_file(&self, session_id: &str) -> PathBuf {
            let path = self.root.join(format!("{session_id}.jsonl"));
            std::fs::write(&path, "").expect("write session file");
            path
        }

        fn locator(&self, agent_pid: u32, pty_start: SystemTime) -> OpenCodeLocator {
            OpenCodeLocator::new(self.root.clone(), agent_pid, pty_start)
        }
    }

    fn index_row(session_id: &str, pid: u64, directory: &str, time: i64) -> serde_json::Value {
        json!({
            "sessionID": session_id,
            "pid": pid,
            "directory": directory,
            "slug": "happy-otter",
            "time": time,
        })
    }

    /// In the cwd FALLBACK path (neither row carries the detected pid), multiple
    /// fresh same-cwd sessionIDs are ambiguous without a per-attach identity
    /// marker. The locator must fail closed instead of choosing the newest row
    /// by recency and potentially tailing another pane's transcript.
    #[test]
    fn ambiguous_fresh_same_cwd_sessions_without_pid_or_cache_is_not_ready_err() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        let cwd_str = cwd.to_string_lossy().into_owned();

        let agent_pid = 4242u32;
        // Neither row's pid matches `agent_pid`, so the pid path yields nothing
        // and the cwd-ambiguity guard is what must fire.
        bridge.write_index(&[
            index_row("ses_a", 9998, &cwd_str, 1_000),
            index_row("ses_b", 9999, &cwd_str, 5_000),
        ]);

        let locator = bridge.locator(agent_pid, epoch_ms(0));
        let err = locator
            .locate(&cwd, "pty-1")
            .expect_err("ambiguous same-cwd sessions must not bind");

        assert!(
            err.contains("refusing recency-only binding"),
            "ambiguity failure explains fail-closed binding: {err}"
        );
    }

    /// The core fix: pid binds THIS attach's session even when Vimeflow's
    /// OSC7-tracked cwd never caught up to opencode's real cwd. The index row's
    /// `directory` is a project subdir; `locate` is called with the stale spawn
    /// cwd (`~`). `directory == cwd` can never match, but `pid == agent_pid`
    /// does — and that is the session the user is looking at.
    #[test]
    fn pid_match_binds_session_despite_cwd_mismatch() {
        let bridge = Bridge::new();
        // opencode's real/project dir (what the bridge records as `directory`).
        let project = bridge._tmp.path().join("projects/rustgo");
        std::fs::create_dir_all(&project).expect("mkdir project");
        let project_str = project.to_string_lossy().into_owned();
        // Vimeflow's stale OSC7 cwd — the pane's spawn dir, NOT the project.
        let tracked_cwd = bridge._tmp.path().to_path_buf();

        let agent_pid = 89074u32;
        bridge.write_index(&[index_row("ses_live", agent_pid as u64, &project_str, 5_000)]);

        let locator = bridge.locator(agent_pid, epoch_ms(0));
        let located = locator
            .locate(&tracked_cwd, "pty")
            .expect("pid match binds despite cwd mismatch");
        assert_eq!(located.agent_session_id.as_deref(), Some("ses_live"));
        assert_eq!(
            located.resolved_directory.as_deref(),
            Some(project.as_path())
        );
    }

    /// Two opencode panes sharing one project dir are NOT ambiguous under
    /// pid-primary: the row whose pid matches the detected `agent_pid` is the
    /// one bound, even though both rows share a cwd.
    #[test]
    fn pid_match_disambiguates_two_same_cwd_sessions() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().join("shared");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        let cwd_str = cwd.to_string_lossy().into_owned();

        let agent_pid = 4242u32;
        bridge.write_index(&[
            // Another pane's session in the same dir — newer, but not our pid.
            index_row("ses_other", 9999, &cwd_str, 9_000),
            // Our session — older row, but its pid matches.
            index_row("ses_ours", agent_pid as u64, &cwd_str, 1_000),
        ]);

        let locator = bridge.locator(agent_pid, epoch_ms(0));
        let located = locator.locate(&cwd, "pty").expect("pid disambiguates");
        assert_eq!(
            located.agent_session_id.as_deref(),
            Some("ses_ours"),
            "pid match wins over a newer same-cwd row from another pane"
        );
    }

    /// One opencode server (one pid) that created several sessions in sequence:
    /// the newest pid-matched row is the active session.
    #[test]
    fn pid_match_picks_newest_across_sequential_sessions_under_one_pid() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().to_path_buf();
        let cwd_str = cwd.to_string_lossy().into_owned();

        let agent_pid = 59730u32;
        bridge.write_index(&[
            index_row("ses_first", agent_pid as u64, &cwd_str, 1_000),
            index_row("ses_second", agent_pid as u64, &cwd_str, 2_000),
            index_row("ses_third", agent_pid as u64, &cwd_str, 3_000),
        ]);

        let locator = bridge.locator(agent_pid, epoch_ms(0));
        let located = locator.locate(&cwd, "pty").expect("locate ok");
        assert_eq!(located.agent_session_id.as_deref(), Some("ses_third"));
    }

    /// A pid-matched row older than `pty_start − SLACK` is rejected; with no
    /// other candidate, locate returns the not-ready/retry signal.
    #[test]
    fn freshness_gate_rejects_stale_pid_matched_row() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().join("work");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        let cwd_str = cwd.to_string_lossy().into_owned();

        // floor = 10_000 − 5_000 = 5_000ms; the only row is at 4_000ms (stale).
        let agent_pid = 321u32;
        bridge.write_index(&[index_row("ses_stale", agent_pid as u64, &cwd_str, 4_000)]);

        let locator = bridge.locator(agent_pid, epoch_ms(10_000));
        let err = locator
            .locate(&cwd, "pty")
            .expect_err("stale pid-matched row must not bind");
        assert!(
            err.contains("not ready"),
            "stale pid-matched row is a retry/not-ready signal: {err}"
        );
    }

    /// Repeated rows for the SAME session remain unambiguous even when opencode
    /// writes drifting pids; the newest row for that session still resolves.
    #[test]
    fn repeated_same_session_rows_resolve_despite_pid_drift() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().to_path_buf();
        let cwd_str = cwd.to_string_lossy().into_owned();
        let agent_pid = 7u32;
        bridge.write_index(&[
            index_row("ses_same", 111, &cwd_str, 100),
            index_row("ses_same", 222, &cwd_str, 900),
        ]);

        let locator = bridge.locator(agent_pid, epoch_ms(0));
        let located = locator.locate(&cwd, "pty").expect("locate ok");
        assert_eq!(located.agent_session_id.as_deref(), Some("ses_same"));
    }

    /// One fresh same-cwd session is chosen.
    #[test]
    fn cwd_freshness_fallback_picks_single_fresh_same_cwd_session() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().join("work");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        let cwd_str = cwd.to_string_lossy().into_owned();

        // pty_start = 10_000ms; SLACK = 5_000 ⇒ floor = 5_000ms.
        let pty_start = epoch_ms(10_000);
        bridge.write_index(&[index_row("ses_fresh", 2, &cwd_str, 6_000)]);

        let locator = bridge.locator(1, pty_start);
        let located = locator.locate(&cwd, "pty").expect("locate ok");
        assert_eq!(located.agent_session_id.as_deref(), Some("ses_fresh"));
    }

    /// A same-cwd row older than `pty_start − SLACK` is rejected — when it is
    /// the only candidate, locate returns Err (retry signal).
    #[test]
    fn freshness_gate_rejects_stale_same_cwd_row() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().join("work");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        let cwd_str = cwd.to_string_lossy().into_owned();

        // floor = 10_000 − 5_000 = 5_000ms; the only row is at 4_000ms (stale).
        let pty_start = epoch_ms(10_000);
        bridge.write_index(&[index_row("ses_stale", 2, &cwd_str, 4_000)]);

        let locator = bridge.locator(1, pty_start);
        let err = locator
            .locate(&cwd, "pty")
            .expect_err("stale-only must not bind");
        assert!(
            err.contains("not ready"),
            "stale-only failure is a retry/not-ready signal: {err}"
        );
    }

    /// A row exactly on the freshness floor is accepted (`>=`, not `>`).
    #[test]
    fn freshness_gate_accepts_row_exactly_on_floor() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().join("work");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        let cwd_str = cwd.to_string_lossy().into_owned();
        let pty_start = epoch_ms(10_000); // floor = 5_000
        bridge.write_index(&[index_row("ses_edge", 2, &cwd_str, 5_000)]);

        let locator = bridge.locator(1, pty_start);
        let located = locator.locate(&cwd, "pty").expect("on-floor row binds");
        assert_eq!(located.agent_session_id.as_deref(), Some("ses_edge"));
    }

    /// Missing `index.jsonl` ⇒ Err (not-ready / retry), not a panic/fatal.
    #[test]
    fn missing_index_is_not_ready_err() {
        let bridge = Bridge::new();
        // No index written.
        let locator = bridge.locator(1, epoch_ms(0));
        let err = locator
            .locate(bridge._tmp.path(), "pty")
            .expect_err("missing index ⇒ Err");
        assert!(
            err.contains("not ready"),
            "missing index is not-ready: {err}"
        );
    }

    /// A present index with no pid-matched row, no qualifying same-cwd row, and
    /// no cached resolve ⇒ Err (not-ready / retry). The only row carries a
    /// different pid AND lives in a DIFFERENT cwd.
    #[test]
    fn no_pid_match_no_same_cwd_row_and_no_cache_is_not_ready_err() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        let other = bridge._tmp.path().join("elsewhere");
        std::fs::create_dir_all(&other).expect("mkdir other");
        let other_str = other.to_string_lossy().into_owned();

        // pid 999 ≠ agent_pid 1, and the row's dir is not `proj`.
        bridge.write_index(&[index_row("ses_elsewhere", 999, &other_str, 1)]);

        let locator = bridge.locator(1, epoch_ms(0));
        let err = locator
            .locate(&cwd, "pty")
            .expect_err("no pid + no same-cwd row + no cache ⇒ Err");
        assert!(err.contains("not ready"), "no-match is not-ready: {err}");
    }

    /// The locator returns the `<sessionID>.jsonl` path even though that file
    /// does not exist yet — the data file's existence is the tailer's concern
    /// (matches Kimi's locator/data-file split).
    #[test]
    fn missing_session_file_is_tolerated_by_locator() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().to_path_buf();
        let cwd_str = cwd.to_string_lossy().into_owned();
        bridge.write_index(&[index_row("ses_only_index", 42, &cwd_str, 1)]);
        // Note: NO `ses_only_index.jsonl` file written.

        let locator = bridge.locator(42, epoch_ms(0));
        let located = locator
            .locate(&cwd, "pty")
            .expect("locate ok despite no file");
        assert_eq!(
            located.status_path,
            bridge.root.join("ses_only_index.jsonl")
        );
        assert!(!located.status_path.exists(), "data file is absent on disk");
    }

    /// A malformed index line is skipped; a usable row after it still resolves.
    #[test]
    fn malformed_index_line_is_skipped() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().to_path_buf();
        let cwd_str = cwd.to_string_lossy().into_owned();
        let raw = format!(
            "not json at all\n{}\n",
            index_row("ses_good", 5, &cwd_str, 1)
        );
        std::fs::write(bridge.root.join("index.jsonl"), raw).expect("write index");

        let locator = bridge.locator(5, epoch_ms(0));
        let located = locator.locate(&cwd, "pty").expect("good row resolves");
        assert_eq!(located.agent_session_id.as_deref(), Some("ses_good"));
    }

    /// Re-resolution still re-reads the index, but it must not jump from an
    /// already-bound session to a different newer same-cwd session. That keeps
    /// an older pane's watcher attached to its own transcript after a newer pane
    /// appends to the shared index.
    #[test]
    fn second_locate_keeps_cached_binding_when_newer_same_cwd_session_appears() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().to_path_buf();
        let cwd_str = cwd.to_string_lossy().into_owned();

        // Session A is the only fresh same-cwd row at first.
        bridge.write_index(&[index_row("ses_A", 111, &cwd_str, 1_000)]);

        let locator = bridge.locator(1, epoch_ms(0));
        let first = locator.locate(&cwd, "pty").expect("first locate binds A");
        assert_eq!(first.agent_session_id.as_deref(), Some("ses_A"));

        // A newer same-cwd session B is appended by another pane.
        bridge.write_index(&[
            index_row("ses_A", 111, &cwd_str, 1_000),
            index_row("ses_B", 222, &cwd_str, 2_000),
        ]);

        let second = locator
            .locate(&cwd, "pty")
            .expect("second locate keeps cache");
        assert_eq!(
            second.agent_session_id.as_deref(),
            Some("ses_A"),
            "re-invoked locate must not rebind to another same-cwd session by recency alone"
        );
        assert_eq!(second.status_path, bridge.root.join("ses_A.jsonl"));
    }

    /// Fallback: after a successful resolve, a later `locate` whose index read
    /// yields NO qualifying row returns the cached previous resolve (not Err) —
    /// a transient empty read must not drop a live binding.
    #[test]
    fn locate_falls_back_to_cached_resolve_on_a_later_empty_read() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().to_path_buf();
        let cwd_str = cwd.to_string_lossy().into_owned();
        bridge.write_index(&[index_row("ses_cached", 1, &cwd_str, 1)]);

        let locator = bridge.locator(1, epoch_ms(0));
        let first = locator.locate(&cwd, "pty").expect("first locate");

        // Remove the index; the read now yields the not-ready error, but the
        // prior resolve is cached and must answer.
        std::fs::remove_file(bridge.root.join("index.jsonl")).expect("rm index");

        let second = locator
            .locate(&cwd, "pty")
            .expect("cached fallback locate ok");
        assert_eq!(first.agent_session_id, second.agent_session_id);
        assert_eq!(first.status_path, second.status_path);
        assert_eq!(second.agent_session_id.as_deref(), Some("ses_cached"));
    }

    /// Fallback: after a successful resolve, a later `locate` whose index is
    /// present but contains no row for the current cwd still returns the cached
    /// previous resolve. This covers the `resolve_by_cwd -> None` fallback path.
    #[test]
    fn locate_falls_back_to_cached_resolve_when_cwd_row_disappears_from_index() {
        let bridge = Bridge::new();
        let cwd = bridge._tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        let cwd_str = cwd.to_string_lossy().into_owned();
        let other = bridge._tmp.path().join("other");
        std::fs::create_dir_all(&other).expect("mkdir other");
        let other_str = other.to_string_lossy().into_owned();

        bridge.write_index(&[index_row("ses_A", 1, &cwd_str, 1)]);

        let locator = bridge.locator(1, epoch_ms(0));
        let first = locator.locate(&cwd, "pty").expect("first locate");

        bridge.write_index(&[index_row("ses_other", 1, &other_str, 2)]);

        let second = locator
            .locate(&cwd, "pty")
            .expect("cached fallback locate ok");
        assert_eq!(first.agent_session_id, second.agent_session_id);
        assert_eq!(first.status_path, second.status_path);
        assert_eq!(second.agent_session_id.as_deref(), Some("ses_A"));
    }

    #[test]
    fn effective_bridge_root_returns_bridge_root() {
        let bridge = Bridge::new();
        let locator = bridge.locator(1, epoch_ms(0));
        assert_eq!(locator.effective_bridge_root(), bridge.root.as_path());
    }

    // ---- validator ----

    #[test]
    fn validator_accepts_real_jsonl_under_bridge_root() {
        let bridge = Bridge::new();
        let path = bridge.write_session_file("ses_x");
        let canonical =
            validate_transcript_path_with_root(path.to_str().expect("utf8 path"), &bridge.root)
                .expect("real jsonl under root validates");
        assert_eq!(canonical, std::fs::canonicalize(&path).expect("canonical"));
    }

    #[test]
    fn validator_rejects_null_byte() {
        let bridge = Bridge::new();
        let result = validate_transcript_path_with_root("/tmp/ses\0.jsonl", &bridge.root);
        assert!(matches!(
            result,
            Err(ValidateTranscriptError::InvalidPath(_))
        ));
    }

    #[test]
    fn validator_rejects_path_outside_bridge_root() {
        let bridge = Bridge::new();
        // A real `.jsonl` file but OUTSIDE the bridge root (sibling tempdir).
        let outside_tmp = tempfile::tempdir().expect("outside tempdir");
        let outside = outside_tmp.path().join("escape.jsonl");
        std::fs::write(&outside, "").expect("write outside");

        let result =
            validate_transcript_path_with_root(outside.to_str().expect("utf8"), &bridge.root);
        assert!(matches!(
            result,
            Err(ValidateTranscriptError::OutsideRoot { .. })
        ));
    }

    #[test]
    fn validator_rejects_traversal_out_of_bridge_root() {
        let bridge = Bridge::new();
        let outside_tmp = tempfile::tempdir().expect("outside tempdir");
        let outside = outside_tmp.path().join("escape.jsonl");
        std::fs::write(&outside, "").expect("write outside");

        // `<bridge>/../<outside-basename>` style traversal — canonicalize
        // collapses it to the real outside path, which fails the under-root gate.
        let traversal = bridge
            .root
            .join("..")
            .join(outside_tmp.path().file_name().expect("name"))
            .join("escape.jsonl");
        let result =
            validate_transcript_path_with_root(traversal.to_str().expect("utf8"), &bridge.root);
        assert!(
            matches!(result, Err(ValidateTranscriptError::OutsideRoot { .. }))
                || matches!(result, Err(ValidateTranscriptError::NotFound(_))),
            "traversal out of root must not validate: {result:?}"
        );
    }

    #[test]
    fn validator_rejects_non_jsonl_file_under_root() {
        let bridge = Bridge::new();
        let txt = bridge.root.join("notes.txt");
        std::fs::write(&txt, "").expect("write txt");
        let result = validate_transcript_path_with_root(txt.to_str().expect("utf8"), &bridge.root);
        assert!(matches!(
            result,
            Err(ValidateTranscriptError::InvalidPath(_))
        ));
    }

    #[test]
    fn validator_reports_not_found_for_missing_path() {
        let bridge = Bridge::new();
        let missing = bridge.root.join("nope.jsonl");
        let result =
            validate_transcript_path_with_root(missing.to_str().expect("utf8"), &bridge.root);
        assert!(matches!(result, Err(ValidateTranscriptError::NotFound(_))));
    }
}
