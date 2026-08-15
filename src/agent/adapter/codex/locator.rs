//! Codex session locator.

use super::types::BindContext;
use crate::agent::types::{RateLimitInfo, RateLimits};
use chrono::{Datelike, Duration as ChronoDuration, Local};
use rusqlite::{named_params, Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct RolloutLocation {
    pub rollout_path: PathBuf,
    #[allow(dead_code)]
    pub thread_id: String,
    #[allow(dead_code)]
    pub state_updated_at_ms: i64,
}

#[derive(Debug, Clone)]
pub enum LocatorError {
    NotYetReady,
    Unresolved(String),
    Fatal(String),
}

/// Shared prefix tying the `SqliteFirstLocator` schema-drift producer
/// sites to the `CompositeLocator::resolve_rollout` fallback-dispatch
/// guard. Two producers (`"threads table not found"` and
/// `"logs table not found"`) prepend this string; the consumer checks
/// `reason.starts_with(SCHEMA_DRIFT_ERROR_PREFIX)`. Centralizing here
/// gives the build a compile-time contract — renaming the constant
/// updates all three sites in one diff (PR #261 cycle 15 review F40 —
/// the prior bare-string-literal form left the consumer guard 350
/// lines from the producers and trivially breakable by a rename).
pub(super) const SCHEMA_DRIFT_ERROR_PREFIX: &str = "schema drift: ";

impl std::fmt::Display for LocatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotYetReady => f.write_str("locator: not yet ready"),
            Self::Unresolved(reason) => write!(f, "locator: unresolved - {}", reason),
            Self::Fatal(reason) => write!(f, "locator: fatal - {}", reason),
        }
    }
}

impl std::error::Error for LocatorError {}

pub trait CodexSessionLocator {
    fn resolve_rollout(&self, ctx: &BindContext<'_>) -> Result<RolloutLocation, LocatorError>;
}

pub(super) fn discover_db(
    codex_home: &Path,
    target_table: &str,
) -> Result<Option<PathBuf>, std::io::Error> {
    let mut candidates: Vec<(PathBuf, u32, SystemTime)> = Vec::new();

    for entry in std::fs::read_dir(codex_home)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };

        // `*.sqlite-wal` and `*.sqlite-shm` are SQLite WAL sidecars, but
        // they don't end with `.sqlite`, so this single suffix check
        // already excludes them.
        if !name.ends_with(".sqlite") {
            continue;
        }

        let conn = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => conn,
            Err(_) => continue,
        };

        let has_table = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 LIMIT 1",
                [target_table],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !has_table {
            continue;
        }

        let suffix = extract_numeric_suffix(name);
        let mtime = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((path, suffix, mtime));
    }

    if candidates.is_empty() {
        return Ok(None);
    }

    candidates.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
    Ok(Some(candidates.remove(0).0))
}

fn extract_numeric_suffix(name: &str) -> u32 {
    let without_ext = name.strip_suffix(".sqlite").unwrap_or(name);
    let suffix = without_ext
        .rsplit_once('_')
        .map(|(_, value)| value)
        .unwrap_or("");
    suffix.parse::<u32>().unwrap_or(0)
}

/// Provider of the set of rollout files a process currently holds open.
///
/// `Err` (the provider itself failed — `/proc` read error, `lsof` timeout,
/// permission denied) is **distinct from `Ok(∅)`** (the provider ran fine and
/// the process has no rollout file open). This distinction is load-bearing for
/// the resolver: a failed provider must not be silently treated as "no open
/// rollout" and allowed to fall back to possibly-stale argv/sqlite. VIM-189
/// introduces the seam behavior-preservingly — `resolve_from_proc_fds` still
/// treats `Err` as "no FD" for now; VIM-191 makes `Err` authoritative.
///
/// Implementations must never panic and never block unboundedly.
pub(super) trait OpenRolloutFds: Send + Sync {
    fn open_rollout_paths(&self, pid: u32) -> io::Result<HashSet<PathBuf>>;
}

/// Linux `/proc/<pid>/fd` open-FD provider: reads the process's fd symlinks and
/// keeps those pointing at `codex_home/sessions/rollout-*.jsonl`.
pub(super) struct ProcOpenRolloutFds {
    proc_root: PathBuf,
    codex_sessions_root: PathBuf,
}

impl ProcOpenRolloutFds {
    pub(super) fn new(proc_root: PathBuf, codex_home: &Path) -> Self {
        Self {
            codex_sessions_root: codex_home.join("sessions"),
            proc_root,
        }
    }
}

impl OpenRolloutFds for ProcOpenRolloutFds {
    fn open_rollout_paths(&self, pid: u32) -> io::Result<HashSet<PathBuf>> {
        let fd_dir = self.proc_root.join(pid.to_string()).join("fd");
        let mut rollout_paths = HashSet::new();

        // A failed directory read is a provider error (`Err`), *not* an empty
        // result: `/proc/<pid>/fd` is unreadable when the pid is gone or access
        // is denied, and callers must be able to distinguish that from "the
        // process has nothing open" (VIM-191). Individual unreadable entries
        // are skipped, mirroring the previous `entries.flatten()` behavior.
        for entry in std::fs::read_dir(&fd_dir)? {
            let Ok(entry) = entry else {
                continue;
            };
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            if !target.starts_with(&self.codex_sessions_root) {
                continue;
            }
            let Some(name) = target.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                rollout_paths.insert(target);
            }
        }

        Ok(rollout_paths)
    }
}

/// No-op provider for platforms without a usable open-FD source (Windows;
/// `/proc` is Linux-only and `lsof` covers the other Unixes). Reports `Ok(∅)` —
/// "ran fine, found no open rollout" — so the resolver falls through to the
/// argv/sqlite strategies exactly as it does today when no FD source exists.
///
/// Compiled only where it is actually used — non-Unix production
/// (`default_non_proc_open_fds`) and tests (as an injected null provider) — so
/// Unix builds, which always have `/proc` or `lsof`, carry no dead code.
#[cfg(any(test, not(unix)))]
pub(super) struct NullOpenRolloutFds;

#[cfg(any(test, not(unix)))]
impl OpenRolloutFds for NullOpenRolloutFds {
    fn open_rollout_paths(&self, _pid: u32) -> io::Result<HashSet<PathBuf>> {
        Ok(HashSet::new())
    }
}

/// Runs `lsof -p <pid> -Fn` and returns its stdout, mapping missing `lsof` to
/// empty output and timeout / non-zero exit to `Err`. Behind a trait so the
/// parser and the `Ok(∅)` / `Err` mapping are unit-tested without spawning
/// `lsof` (the real spawn only runs on Unix in production; VIM-190).
#[cfg(unix)]
trait LsofRunner: Send + Sync {
    fn run(&self, pid: u32) -> io::Result<String>;
}

/// `lsof`-based open-FD provider for the Unixes where `/proc` is unavailable
/// (macOS/BSD). The open rollout FD is the only reliable per-PID signal for an
/// in-session `resume`, so without this the relocation fix is inert on macOS
/// (VIM-188/VIM-190).
#[cfg(unix)]
pub(super) struct LsofOpenRolloutFds {
    codex_sessions_root: PathBuf,
    runner: Box<dyn LsofRunner>,
}

#[cfg(unix)]
impl LsofOpenRolloutFds {
    pub(super) fn new(codex_home: &Path) -> Self {
        Self {
            codex_sessions_root: codex_home.join("sessions"),
            runner: Box::new(RealLsofRunner {
                timeout: Duration::from_secs(2),
            }),
        }
    }

    #[cfg(test)]
    fn with_runner(codex_home: &Path, runner: Box<dyn LsofRunner>) -> Self {
        Self {
            codex_sessions_root: codex_home.join("sessions"),
            runner,
        }
    }
}

#[cfg(unix)]
impl OpenRolloutFds for LsofOpenRolloutFds {
    fn open_rollout_paths(&self, pid: u32) -> io::Result<HashSet<PathBuf>> {
        // Runner `Err` (timeout / non-zero exit) propagates as a provider
        // failure — distinct from `Ok(∅)` (lsof ran, the process has no rollout
        // file open). A missing `lsof` binary means the platform signal is
        // unavailable, so fall back to the pre-lsof behavior instead of blocking
        // all attach strategies on macOS/BSD.
        let stdout = match self.runner.run(pid) {
            Ok(stdout) => stdout,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err),
        };
        Ok(parse_lsof_rollout_names(&stdout, &self.codex_sessions_root))
    }
}

/// Parse `lsof -Fn` field output: records are newline-separated fields, where a
/// line beginning with `n` carries a file name. Keep the names that live under
/// `codex_home/sessions/` and look like `rollout-*.jsonl`. The prefix match is
/// lexical — the same shared, documented limitation as the `/proc` provider
/// (a symlinked `codex_home` could spell the prefix differently).
#[cfg(unix)]
fn parse_lsof_rollout_names(stdout: &str, codex_sessions_root: &Path) -> HashSet<PathBuf> {
    let mut rollout_paths = HashSet::new();
    for line in stdout.lines() {
        let Some(name) = line.strip_prefix('n') else {
            continue;
        };
        let path = PathBuf::from(name);
        if !path.starts_with(codex_sessions_root) {
            continue;
        }
        let Some(file) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if file.starts_with("rollout-") && file.ends_with(".jsonl") {
            rollout_paths.insert(path);
        }
    }
    rollout_paths
}

/// Production `LsofRunner`: spawns `lsof` with a bounded wall-clock timeout so a
/// hung/slow `lsof` can never block watcher startup. stdout is drained on a
/// helper thread so a full pipe cannot deadlock the wait loop.
#[cfg(unix)]
struct RealLsofRunner {
    timeout: Duration,
}

#[cfg(unix)]
impl LsofRunner for RealLsofRunner {
    fn run(&self, pid: u32) -> io::Result<String> {
        use std::process::{Command, Stdio};

        let mut child = match Command::new("lsof")
            .args(["-p", &pid.to_string(), "-Fn"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
            Err(err) => return Err(err),
        };

        // Drain stdout on a thread: lsof output for one pid is small, but a
        // full pipe would otherwise deadlock the try_wait loop below.
        let mut child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("lsof child stdout was not captured"))?;
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = child_stdout.read_to_string(&mut buf);
            buf
        });

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = reader.join();
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "lsof timed out"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(e);
                }
            }
        };

        let stdout = reader
            .join()
            .map_err(|_| io::Error::other("lsof stdout reader panicked"))?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "lsof exited unsuccessfully: {status}"
            )));
        }
        Ok(stdout)
    }
}

/// Open-FD provider selected when `/proc` is unavailable. `lsof` covers the
/// Unixes (it is the only per-PID open-rollout signal on macOS/BSD; VIM-190);
/// Windows has no usable source, so it stays null. On Linux this arm is reached
/// only by tests — production threads a real `/proc` root through
/// `with_proc_root` and uses `ProcOpenRolloutFds`.
fn default_non_proc_open_fds(codex_home: &Path) -> Box<dyn OpenRolloutFds> {
    #[cfg(unix)]
    {
        Box::new(LsofOpenRolloutFds::new(codex_home))
    }
    #[cfg(not(unix))]
    {
        let _ = codex_home;
        Box::new(NullOpenRolloutFds)
    }
}

pub struct SqliteFirstLocator {
    pub codex_home: PathBuf,
    /// `Some(path)` on Linux (and in test harnesses that inject a
    /// tempdir-based fake proc); `None` on macOS/Windows where the
    /// `/proc` filesystem does not exist. Drives the resume-argv fast-path
    /// (`resume_thread_id_from_proc`, which SKIPs itself when this is `None`
    /// rather than probing a nonexistent `/proc/<pid>/cmdline`) and selects
    /// the open-FD provider built in `with_proc_root` — `ProcOpenRolloutFds`
    /// on Linux, `LsofOpenRolloutFds` on macOS/BSD, `NullOpenRolloutFds` on
    /// Windows (PR #302 Claude review F1; the `OpenRolloutFds` seam, VIM-189;
    /// the macOS `lsof` provider, VIM-190).
    proc_root: Option<PathBuf>,
    /// Open rollout-FD provider consulted by `resolve_from_proc_fds`. Built
    /// from `proc_root` in `with_proc_root`; the test-only `with_fds`
    /// constructor injects a fake so the open-FD resolver paths stay
    /// platform-independent (VIM-189).
    fds: Box<dyn OpenRolloutFds>,
}

impl SqliteFirstLocator {
    /// Test constructor with **inert** proc fast-paths — no real `/proc` and a
    /// null open-FD provider — so logs / threads / schema-drift tests are
    /// deterministic and never depend on whether the fake test pid happens to
    /// exist in the host's real `/proc` (under VIM-191 a missing `/proc/<pid>`
    /// is a provider `Err` → `NotYetReady`, which would mask the path under
    /// test). Tests that exercise the proc fast-paths use `with_proc_root`
    /// (fake proc root) or `with_fds` (injected provider). Production callers go
    /// through `CompositeLocator::new` → `with_proc_root`.
    #[cfg(test)]
    pub fn new(codex_home: PathBuf) -> Self {
        Self::with_fds(codex_home, None, Box::new(NullOpenRolloutFds))
    }

    /// Explicit `proc_root` constructor. `pub(super)` so
    /// `CompositeLocator::new` can thread an `AttachContext`-derived
    /// `Option<PathBuf>` through. `None` disables the proc-backed
    /// fast-paths (the locator falls through to the logs / FS-scan
    /// strategies); `Some(path)` enables them rooted at the given
    /// directory (PR #261 cycle 8 F22 added the proc_root parameter,
    /// PR #302 Claude review F1 widened it to `Option` so non-Linux
    /// production callers stop probing nonexistent `/proc` paths).
    pub(super) fn with_proc_root(codex_home: PathBuf, proc_root: Option<PathBuf>) -> Self {
        let fds: Box<dyn OpenRolloutFds> = match &proc_root {
            Some(root) => Box::new(ProcOpenRolloutFds::new(root.clone(), &codex_home)),
            None => default_non_proc_open_fds(&codex_home),
        };
        Self {
            codex_home,
            proc_root,
            fds,
        }
    }

    /// Test-only constructor injecting an arbitrary open-FD provider so the
    /// open-FD resolver paths run without a real `/proc` (VIM-189; consumed by
    /// the VIM-191 relocation test).
    #[cfg(test)]
    pub(super) fn with_fds(
        codex_home: PathBuf,
        proc_root: Option<PathBuf>,
        fds: Box<dyn OpenRolloutFds>,
    ) -> Self {
        Self {
            codex_home,
            proc_root,
            fds,
        }
    }

    fn query_logs_thread_id(
        &self,
        path: &Path,
        pid: u32,
        pty_secs: i64,
        pty_nanos: i64,
    ) -> Result<String, LocatorError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| LocatorError::Fatal(format!("open logs db: {}", e)))?;
        let mut stmt = conn
            .prepare(
                "SELECT thread_id FROM logs
                 WHERE process_uuid LIKE :pid
                   AND thread_id IS NOT NULL
                   AND (ts > :pty_start_secs
                        OR (ts = :pty_start_secs AND ts_nanos >= :pty_start_nanos))
                 ORDER BY ts DESC, ts_nanos DESC
                 LIMIT 1",
            )
            .map_err(|e| LocatorError::Fatal(format!("prepare logs query: {}", e)))?;
        let pid_pattern = format!("pid:{}:%", pid);
        let mut rows = stmt
            .query(named_params! {
                ":pid": pid_pattern,
                ":pty_start_secs": pty_secs,
                ":pty_start_nanos": pty_nanos,
            })
            .map_err(|e| LocatorError::Fatal(format!("execute logs query: {}", e)))?;

        match rows.next() {
            Ok(Some(row)) => row
                .get::<_, String>(0)
                .map_err(|e| LocatorError::Fatal(format!("read thread_id: {}", e))),
            Ok(None) => Err(LocatorError::NotYetReady),
            Err(e) => Err(LocatorError::Fatal(format!("step logs query: {}", e))),
        }
    }

    fn query_thread_row(
        &self,
        path: &Path,
        thread_id: &str,
    ) -> Result<RolloutLocation, LocatorError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| LocatorError::Fatal(format!("open state db: {}", e)))?;
        let mut stmt = conn
            .prepare("SELECT rollout_path, updated_at_ms FROM threads WHERE id = :thread_id")
            .map_err(|e| LocatorError::Fatal(format!("prepare threads query: {}", e)))?;
        let mut rows = stmt
            .query(named_params! { ":thread_id": thread_id })
            .map_err(|e| LocatorError::Fatal(format!("execute threads query: {}", e)))?;

        match rows.next() {
            Ok(Some(row)) => Ok(RolloutLocation {
                rollout_path: PathBuf::from(
                    row.get::<_, String>(0)
                        .map_err(|e| LocatorError::Fatal(format!("read rollout_path: {}", e)))?,
                ),
                thread_id: thread_id.to_string(),
                state_updated_at_ms: row.get::<_, i64>(1).unwrap_or(0),
            }),
            Ok(None) => Err(LocatorError::NotYetReady),
            Err(e) => Err(LocatorError::Fatal(format!("step threads query: {}", e))),
        }
    }

    fn query_thread_by_rollout_path(
        &self,
        path: &Path,
        rollout_path: &Path,
    ) -> Result<Option<RolloutLocation>, LocatorError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| LocatorError::Fatal(format!("open state db: {}", e)))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, updated_at_ms
                 FROM threads
                 WHERE rollout_path = :rollout_path
                 LIMIT 1",
            )
            .map_err(|e| LocatorError::Fatal(format!("prepare rollout lookup: {}", e)))?;
        let rollout_path = rollout_path.to_string_lossy().to_string();
        let mut rows = stmt
            .query(named_params! { ":rollout_path": &rollout_path })
            .map_err(|e| LocatorError::Fatal(format!("execute rollout lookup: {}", e)))?;

        match rows.next() {
            Ok(Some(row)) => Ok(Some(RolloutLocation {
                rollout_path: PathBuf::from(rollout_path),
                thread_id: row
                    .get::<_, String>(0)
                    .map_err(|e| LocatorError::Fatal(format!("read rollout thread id: {}", e)))?,
                state_updated_at_ms: row.get::<_, i64>(1).unwrap_or(0),
            })),
            Ok(None) => Ok(None),
            Err(e) => Err(LocatorError::Fatal(format!("step rollout lookup: {}", e))),
        }
    }

    fn query_candidate_rows(
        &self,
        path: &Path,
        updated_since_ms: Option<i64>,
        rollout_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<StateCandidate>, LocatorError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| LocatorError::Fatal(format!("open state db: {}", e)))?;

        // Non-capturing → `Copy`, so it can back both query_map calls below.
        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<StateCandidate> {
            Ok(StateCandidate {
                thread_id: row.get::<_, String>(0)?,
                rollout_path: PathBuf::from(row.get::<_, String>(1)?),
                cwd: row.get::<_, String>(2).unwrap_or_default(),
                updated_at_ms: row.get::<_, i64>(3).unwrap_or(0),
            })
        };

        let mut candidates = Vec::new();
        let rows = match rollout_paths {
            // Open-FD resolver: filter by the open rollout paths IN SQL and drop
            // the global recency `LIMIT` — otherwise a valid but older open
            // rollout row could be truncated out of the 64 most-recent threads
            // in a large history, making the open-FD resolver wrongly report
            // "not yet ready" (codex review VIM-191 P2).
            Some(paths) => {
                if paths.is_empty() {
                    return Ok(candidates);
                }
                let placeholders = vec!["?"; paths.len()].join(", ");
                let sql = format!(
                    "SELECT id, rollout_path, cwd, updated_at_ms
                     FROM threads
                     WHERE rollout_path IN ({})",
                    placeholders
                );
                let mut stmt = conn
                    .prepare(&sql)
                    .map_err(|e| LocatorError::Fatal(format!("prepare candidate query: {}", e)))?;
                let path_params: Vec<String> = paths.iter().cloned().collect();
                let mapped = stmt
                    .query_map(rusqlite::params_from_iter(path_params.iter()), map_row)
                    .map_err(|e| LocatorError::Fatal(format!("execute candidate query: {}", e)))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| LocatorError::Fatal(format!("read candidate row: {}", e)))?;
                mapped
            }
            // Recent-state heuristic: bounded recency window over all threads.
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, rollout_path, cwd, updated_at_ms
                         FROM threads
                         ORDER BY updated_at_ms DESC
                         LIMIT 64",
                    )
                    .map_err(|e| LocatorError::Fatal(format!("prepare candidate query: {}", e)))?;
                let mapped = stmt
                    .query_map([], map_row)
                    .map_err(|e| LocatorError::Fatal(format!("execute candidate query: {}", e)))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| LocatorError::Fatal(format!("read candidate row: {}", e)))?;
                mapped
            }
        };

        for candidate in rows {
            if let Some(min_updated_at_ms) = updated_since_ms {
                if candidate.updated_at_ms < min_updated_at_ms {
                    continue;
                }
            }
            candidates.push(candidate);
        }

        Ok(candidates)
    }

    fn resolve_recent_state_candidate(
        &self,
        state_path: &Path,
        ctx: &BindContext<'_>,
    ) -> Result<RolloutLocation, LocatorError> {
        let pty_start_ms = pty_start_to_millis(ctx.pty_start)?;
        let candidates = self.query_candidate_rows(state_path, Some(pty_start_ms), None)?;
        match choose_state_candidate(&candidates, ctx)? {
            Some(candidate) => Ok(candidate.into_rollout_location()),
            None => Err(LocatorError::NotYetReady),
        }
    }

    fn resolve_from_resume_arg(
        &self,
        state_path: &Path,
        ctx: &BindContext<'_>,
    ) -> Result<Option<RolloutLocation>, LocatorError> {
        // No proc root on this platform — the resume-argv fast-path is a
        // Linux-only probe. Caller falls through to the FS-scan / logs path
        // (PR #302 Claude review F1).
        let Some(proc_root) = self.proc_root.as_deref() else {
            return Ok(None);
        };
        let Some(thread_id) = resume_thread_id_from_proc(proc_root, ctx.pid) else {
            return Ok(None);
        };

        match self.query_thread_row(state_path, &thread_id) {
            Ok(location) => {
                log::debug!(
                    "codex locator: using resume argv fast-path pid={} thread_id={}",
                    ctx.pid,
                    thread_id
                );
                Ok(Some(location))
            }
            Err(LocatorError::NotYetReady) => Ok(None),
            Err(other) => Err(other),
        }
    }

    /// Resolve via the per-PID open rollout FD — the **authoritative** signal
    /// (VIM-191, Option A). Outcomes:
    ///
    /// - `Ok(Some)` — an open rollout FD resolved to a thread row. This wins
    ///   over a stale `resume`-argv (the original bug: an in-session `resume`
    ///   keeps the same cmdline, so argv-recency would bind the *old* thread).
    /// - `Ok(None)` — the provider ran and found **no** open rollout FD
    ///   (`Ok(∅)`): the early window before codex opens the rollout, or a
    ///   `codex resume <id>` launch. The caller falls back to the resume-argv /
    ///   logs / recency strategies.
    /// - `Err(NotYetReady)` — the provider **failed** (lsof error / timeout /
    ///   denied; or `/proc/<pid>` gone), **or** an FD is open but its thread
    ///   row is not written yet. Either way this is a not-yet / failed
    ///   relocation that must **not** silently fall back to possibly-stale
    ///   argv/sqlite (the false-success trap from codex's review). `retry_locator`
    ///   retries it, and persistent failure fails safe — the watcher keeps its
    ///   prior binding and the UI keeps "needs reattach".
    fn resolve_from_open_fds(
        &self,
        state_path: &Path,
        ctx: &BindContext<'_>,
    ) -> Result<Option<RolloutLocation>, LocatorError> {
        let rollout_paths = match self.fds.open_rollout_paths(ctx.pid) {
            Ok(paths) => paths,
            Err(err) => {
                // Provider `Err` != "no FD". Do not fall back to stale
                // argv/sqlite; surface as not-yet-ready so the bind retries and
                // ultimately fails safe rather than reporting a false relocation.
                log::debug!(
                    "codex locator resolve: decision=provider-err pid={} err={} -> NotYetReady",
                    ctx.pid,
                    err
                );
                return Err(LocatorError::NotYetReady);
            }
        };

        if rollout_paths.is_empty() {
            // `Ok(∅)`: no open rollout FD — fall back to the early-window
            // strategies (resume-argv / logs / recency).
            log::debug!(
                "codex locator resolve: decision=empty-fallback pid={} (no open rollout FD)",
                ctx.pid
            );
            return Ok(None);
        }

        if rollout_paths.len() == 1 {
            let rollout_path = rollout_paths.iter().next().cloned().expect("len checked");
            match self.query_thread_by_rollout_path(state_path, &rollout_path)? {
                Some(location) => {
                    log::debug!(
                        "codex locator resolve: decision=single-fd pid={} picked_rollout={} picked_thread={} picked_updated_at_ms={}",
                        ctx.pid,
                        location.rollout_path.display(),
                        location.thread_id,
                        location.state_updated_at_ms
                    );
                    Ok(Some(location))
                }
                // FD open but the thread row is not written yet — wait for it
                // rather than falling back to a possibly-stale candidate.
                None => {
                    log::debug!(
                        "codex locator resolve: decision=single-fd-no-row pid={} open_fd={} -> NotYetReady",
                        ctx.pid,
                        rollout_path.display()
                    );
                    Err(LocatorError::NotYetReady)
                }
            }
        } else {
            // codex keeps OLD rollout FDs open after `/clear` and in-session
            // `resume` (it does not close them), so a single pid can have
            // several rollouts open at once. They share the same cwd, so cwd
            // disambiguation cannot pick the active one — choose the MOST
            // RECENTLY UPDATED open rollout (the live conversation is the one
            // being written). Erroring on same-cwd ambiguity here (as
            // `choose_state_candidate` does) strands the panel after every
            // `/clear`/`resume` — the actual VIM-188 freeze, confirmed by `lsof`
            // showing one codex pid holding 3 rollout files open.
            let rollout_path_strings: HashSet<String> = rollout_paths
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect();
            let candidates =
                self.query_candidate_rows(state_path, None, Some(&rollout_path_strings))?;

            // One decision line per multi-FD resolve (debug, off by default) so
            // `RUST_LOG=vimeflow_lib=debug` reveals which rollout was bound.
            let Some(max_updated_at_ms) = candidates.iter().map(|c| c.updated_at_ms).max() else {
                // Open FDs present but none has a thread row yet — wait for it.
                log::debug!(
                    "codex locator resolve: decision=no-rows pid={} open_fds={} -> NotYetReady",
                    ctx.pid,
                    rollout_paths.len()
                );
                return Err(LocatorError::NotYetReady);
            };
            let mut newest = candidates
                .into_iter()
                .filter(|c| c.updated_at_ms == max_updated_at_ms);
            let candidate = newest.next().expect("a max implies at least one row");
            if newest.next().is_some() {
                // Two open rollouts share the newest `updated_at_ms` — we can't
                // tell which is active (input order is non-deterministic), so
                // fail safe rather than bind arbitrarily. The live rollout
                // advances its row and breaks the tie on the next retry.
                log::debug!(
                    "codex locator resolve: decision=tie-not-yet-ready pid={} max_updated_at_ms={} -> NotYetReady",
                    ctx.pid,
                    max_updated_at_ms
                );
                return Err(LocatorError::NotYetReady);
            }
            log::debug!(
                "codex locator resolve: decision=unique-newest pid={} picked_thread={} picked_rollout={} picked_updated_at_ms={} open_fds={}",
                ctx.pid,
                candidate.thread_id,
                candidate.rollout_path.display(),
                max_updated_at_ms,
                rollout_paths.len()
            );
            Ok(Some(candidate.into_rollout_location()))
        }
    }
}

impl CodexSessionLocator for SqliteFirstLocator {
    fn resolve_rollout(&self, ctx: &BindContext<'_>) -> Result<RolloutLocation, LocatorError> {
        let state_db = discover_db(&self.codex_home, "threads").map_err(|e| {
            LocatorError::Fatal(format!("scan {}: {}", self.codex_home.display(), e))
        })?;
        let logs_db = discover_db(&self.codex_home, "logs").map_err(|e| {
            LocatorError::Fatal(format!("scan {}: {}", self.codex_home.display(), e))
        })?;

        let Some(state_path) = state_db else {
            return Err(LocatorError::Unresolved(format!(
                "{}threads table not found",
                SCHEMA_DRIFT_ERROR_PREFIX
            )));
        };

        // Option A (VIM-191): the per-PID open rollout FD is authoritative, so
        // it is consulted FIRST. Only on `Ok(None)` (provider ran, no open
        // rollout FD — the early window before codex opens the rollout, or a
        // `codex resume <id>` launch) do we fall back to the resume-argv / logs
        // / recency strategies. A provider failure or an FD-without-a-row-yet
        // returns `NotYetReady` from `resolve_from_open_fds` and never falls
        // back to possibly-stale data. (Before this, resume-argv ran first and
        // an in-session `resume` — same cmdline — bound the OLD thread forever.)
        if let Some(location) = self.resolve_from_open_fds(&state_path, ctx)? {
            return Ok(location);
        }
        if let Some(location) = self.resolve_from_resume_arg(&state_path, ctx)? {
            return Ok(location);
        }

        let Some(logs_path) = logs_db else {
            return Err(LocatorError::Unresolved(format!(
                "{}logs table not found",
                SCHEMA_DRIFT_ERROR_PREFIX
            )));
        };

        let (pty_secs, pty_nanos) = pty_start_to_secs_nanos(ctx.pty_start)?;
        match self.query_logs_thread_id(&logs_path, ctx.pid, pty_secs, pty_nanos) {
            Ok(thread_id) => self.query_thread_row(&state_path, &thread_id),
            Err(LocatorError::NotYetReady) => self.resolve_recent_state_candidate(&state_path, ctx),
            Err(other) => Err(other),
        }
    }
}

fn pty_start_to_secs_nanos(t: SystemTime) -> Result<(i64, i64), LocatorError> {
    let duration = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| LocatorError::Fatal(format!("pty_start before epoch: {}", e)))?;
    Ok((duration.as_secs() as i64, duration.subsec_nanos() as i64))
}

fn pty_start_to_millis(t: SystemTime) -> Result<i64, LocatorError> {
    let duration = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| LocatorError::Fatal(format!("pty_start before epoch: {}", e)))?;
    Ok(duration.as_millis() as i64)
}

#[derive(Debug, Clone)]
struct StateCandidate {
    thread_id: String,
    rollout_path: PathBuf,
    cwd: String,
    updated_at_ms: i64,
}

impl StateCandidate {
    fn into_rollout_location(self) -> RolloutLocation {
        RolloutLocation {
            rollout_path: self.rollout_path,
            thread_id: self.thread_id,
            state_updated_at_ms: self.updated_at_ms,
        }
    }
}

fn choose_state_candidate(
    candidates: &[StateCandidate],
    ctx: &BindContext<'_>,
) -> Result<Option<StateCandidate>, LocatorError> {
    if candidates.is_empty() {
        return Ok(None);
    }

    let cwd = ctx.cwd.to_string_lossy();
    let cwd_matches: Vec<&StateCandidate> = candidates.iter().filter(|c| c.cwd == cwd).collect();
    if cwd_matches.len() == 1 {
        return Ok(Some((*cwd_matches[0]).clone()));
    }
    if cwd_matches.len() > 1 {
        return Err(LocatorError::Unresolved(
            "multiple codex session candidates matched cwd".to_string(),
        ));
    }
    if candidates.len() == 1 {
        return Ok(Some(candidates[0].clone()));
    }

    Err(LocatorError::Unresolved(
        "multiple codex session candidates remained after bind heuristics".to_string(),
    ))
}

fn resume_thread_id_from_proc(proc_root: &Path, pid: u32) -> Option<String> {
    let args = read_cmdline_args(proc_root, pid)?;
    let resume_index = args.iter().position(|arg| arg == "resume")?;
    let session_arg = args.get(resume_index + 1)?;
    if session_arg.starts_with('-') {
        return None;
    }
    Some(session_arg.to_string())
}

fn read_cmdline_args(proc_root: &Path, pid: u32) -> Option<Vec<String>> {
    let path = proc_root.join(pid.to_string()).join("cmdline");
    let content = std::fs::read(path).ok()?;
    if content.is_empty() {
        return None;
    }

    let args: Vec<String> = content
        .split(|&b| b == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| String::from_utf8_lossy(chunk).to_string())
        .collect();
    if args.is_empty() {
        None
    } else {
        Some(args)
    }
}

pub struct FsScanFallback {
    pub codex_home: PathBuf,
}

impl FsScanFallback {
    pub fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    fn scan_today_and_yesterday(&self, ctx: &BindContext<'_>) -> Vec<PathBuf> {
        let mut paths = Vec::new();

        for offset in 0..=1 {
            let date = Local::now().date_naive() - ChronoDuration::days(offset);
            let dir = self
                .codex_home
                .join("sessions")
                .join(format!("{:04}", date.year()))
                .join(format!("{:02}", date.month()))
                .join(format!("{:02}", date.day()));

            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or("");
                    if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                        paths.push(path);
                    }
                }
            }
        }

        paths.retain(|path| {
            std::fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .map(|mtime| mtime >= ctx.pty_start)
                .unwrap_or(false)
        });
        paths
    }
}

impl CodexSessionLocator for FsScanFallback {
    fn resolve_rollout(&self, ctx: &BindContext<'_>) -> Result<RolloutLocation, LocatorError> {
        let mut matches: Vec<(PathBuf, String)> = Vec::new();

        for path in self.scan_today_and_yesterday(ctx) {
            let Ok(file) = std::fs::File::open(&path) else {
                continue;
            };
            let mut first_line = String::new();
            let mut reader = std::io::BufReader::new(file);
            use std::io::BufRead;
            if reader.read_line(&mut first_line).is_err() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(first_line.trim()) else {
                continue;
            };

            let cwd_match = value
                .pointer("/payload/cwd")
                .and_then(Value::as_str)
                .map(|cwd| cwd == ctx.cwd.to_string_lossy())
                .unwrap_or(false);
            if !cwd_match {
                continue;
            }

            let id = value
                .pointer("/payload/id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            matches.push((path, id));
        }

        match matches.len() {
            0 => Err(LocatorError::NotYetReady),
            1 => {
                let (rollout_path, thread_id) = matches.remove(0);
                Ok(RolloutLocation {
                    rollout_path,
                    thread_id,
                    state_updated_at_ms: 0,
                })
            }
            _ => Err(LocatorError::Unresolved(
                "multiple rollout candidates after FS scan".to_string(),
            )),
        }
    }
}

pub struct CompositeLocator {
    primary: SqliteFirstLocator,
    fallback: FsScanFallback,
    /// Codex home directory (`~/.codex` by default). Held here so the
    /// `StatusSourceLocator` impl can use it as the `trust_root` for
    /// the returned `LocatedStatusSource` without an extra clone from
    /// the adapter.
    codex_home: PathBuf,
    /// PID of the detected `codex` process inside the PTY session.
    /// Step B' moved this out of `CodexAdapter` and into the locator
    /// per frozen constraint #2: the `StatusSourceLocator` impl
    /// (below) needs to build the `BindContext` it forwards to the
    /// SQLite-first / FS-scan strategies, and that context wants
    /// pid + pty_start.
    pid: u32,
    /// `SystemTime` when the PTY session was spawned. Used by the
    /// SQLite filter to exclude stale rollout rows from earlier
    /// processes.
    pty_start: SystemTime,
    /// Lazy cache for the discovered logs database path.
    /// `discover_db` enumerates and open-checks sqlite files in
    /// `codex_home` on every call; the logs DB location is stable
    /// for a locator/session lifetime, so caching avoids repeated
    /// filesystem I/O on the status-poll hot path (PR #408 review
    /// finding — logs database discovery runs on every status decode).
    /// Only successful discoveries are cached; misses are retried on
    /// subsequent calls so that early polling before the DB exists
    /// does not permanently freeze account rate-limit reporting.
    logs_db_cache: std::sync::OnceLock<PathBuf>,
}

impl CompositeLocator {
    pub(super) fn latest_account_rate_limits(&self, thread_id: &str) -> Option<RateLimits> {
        if thread_id.is_empty() {
            return None;
        }

        let logs_db = match self.logs_db_cache.get() {
            Some(path) => path,
            None => {
                let path = discover_db(&self.codex_home, "logs").ok().flatten()?;
                let _ = self.logs_db_cache.set(path);
                self.logs_db_cache.get().expect("just set")
            }
        };
        let conn = Connection::open_with_flags(logs_db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
        let mut stmt = conn
            .prepare(
                "SELECT feedback_log_body
                 FROM logs
                 WHERE thread_id = :thread_id
                   AND feedback_log_body IS NOT NULL
                   AND feedback_log_body LIKE '%x-codex-primary-used-percent%'
                 ORDER BY ts DESC, ts_nanos DESC
                 LIMIT 1",
            )
            .ok()?;
        let body: String = stmt
            .query_row(named_params! { ":thread_id": thread_id }, |row| row.get(0))
            .ok()?;

        account_rate_limits_from_log_body(&body)
    }
}

fn account_rate_limits_from_log_body(body: &str) -> Option<RateLimits> {
    let five_hour = RateLimitInfo {
        used_percentage: header_f64(body, "x-codex-primary-used-percent")?,
        resets_at: header_u64(body, "x-codex-primary-reset-at").unwrap_or(0),
    };

    let seven_day = match (
        header_f64(body, "x-codex-secondary-used-percent"),
        header_u64(body, "x-codex-secondary-reset-at"),
    ) {
        (Some(used_percentage), Some(resets_at)) => Some(RateLimitInfo {
            used_percentage,
            resets_at,
        }),
        (Some(used_percentage), None) => Some(RateLimitInfo {
            used_percentage,
            resets_at: 0,
        }),
        _ => None,
    };

    Some(RateLimits {
        five_hour,
        seven_day,
    })
}

fn header_f64(body: &str, name: &str) -> Option<f64> {
    header_value(body, name)?.parse::<f64>().ok()
}

fn header_u64(body: &str, name: &str) -> Option<u64> {
    header_value(body, name)?.parse::<u64>().ok()
}

fn header_value<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("\"{}\"", name);
    let mut rest = body;

    loop {
        let pos = rest.find(&key)?;
        rest = &rest[pos + key.len()..];

        // Skip whitespace after key
        rest = rest.trim_start();

        // Expect colon
        if !rest.starts_with(':') {
            continue;
        }
        rest = &rest[1..];

        // Skip whitespace after colon
        rest = rest.trim_start();

        // Expect opening quote for value
        if !rest.starts_with('"') {
            continue;
        }
        rest = &rest[1..];

        let end = match rest.find('"') {
            Some(end) => end,
            None => continue,
        };
        return Some(&rest[..end]);
    }
}

// ----- Step B' retry budget (moved here from `codex/mod.rs`) -----
//
// Per frozen constraint #2: "Codex retry lives inside
// `CompositeLocator::resolve_rollout`'s `StatusSourceLocator` impl,
// NOT around individual SQLite/FS strategies." The retry sits at
// `<CompositeLocator as StatusSourceLocator>::locate` below — one
// wrapper around the strategy chain, so a transient
// `NotYetReady` from EITHER strategy gets retried, but each strategy
// runs only once per retry iteration.

const CODEX_BIND_RETRY_INTERVAL_MS: u64 = 100;
const CODEX_BIND_RETRY_MAX_ATTEMPTS: u32 = 5;

impl CompositeLocator {
    pub(super) fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    /// `proc_root` lets `AttachContext` inject the platform's proc
    /// directory (`Some("/proc")` on Linux), a tempdir-based fake proc
    /// for tests, or `None` on macOS / Windows where the `/proc`
    /// filesystem does not exist (PR #261 cycle 8 F22 added the
    /// parameter; PR #302 Claude review F1 widened it to `Option` so
    /// non-Linux callers stop hardcoding a nonexistent path). Production
    /// sites pass `ctx.proc_root.clone()` directly from
    /// `AgentBindings::for_attach`; the proc-backed fast-paths inside
    /// `SqliteFirstLocator` skip themselves when this is `None`, so the
    /// locator falls through cleanly to the logs / FS-scan strategies.
    pub fn new(
        codex_home: PathBuf,
        pid: u32,
        pty_start: SystemTime,
        proc_root: Option<PathBuf>,
    ) -> Self {
        // No log here. PR #261 cycle 4 F13 / cycle 11 F31:
        // `AgentBindings::for_attach` historically constructed two
        // `CompositeLocator`s per Codex attach (one outer, one inside
        // `CodexAdapter`). Cycle 11 fixed that by sharing a single
        // `Arc<CompositeLocator>` between `bindings.locator` and
        // `CodexAdapter::with_locator`, but the log site still belongs
        // here-or-the-caller decision stands: the attach-once
        // observability lives in `for_attach` so it fires exactly once
        // per attach regardless of how many adapters / consumers
        // clone the resulting `Arc`.
        Self {
            primary: SqliteFirstLocator::with_proc_root(codex_home.clone(), proc_root),
            fallback: FsScanFallback::new(codex_home.clone()),
            codex_home,
            pid,
            pty_start,
            logs_db_cache: std::sync::OnceLock::new(),
        }
    }
}

impl CodexSessionLocator for CompositeLocator {
    /// Two-strategy dispatch with an INTENTIONALLY narrow fallback
    /// gate. `FsScanFallback` runs only when `SqliteFirstLocator`
    /// reported `Unresolved(reason)` that **starts** with the
    /// `"schema drift: "` prefix — i.e. the SQLite tables themselves
    /// are missing or renamed.
    ///
    /// Ambiguity-class `Unresolved` errors (`"multiple codex session
    /// candidates matched cwd"` / `"multiple ... remained after bind
    /// heuristics"`) propagate to the caller and bubble up as
    /// `"codex bind ambiguous: ..."` without consulting the fallback.
    /// This is the **mutually-exclusive** design from the original
    /// design pass: the two strategies read different data sources
    /// (SQLite `threads.cwd` vs. JSONL `/payload/cwd`), and an
    /// ambiguity in one is overwhelmingly likely to also be an
    /// ambiguity in the other; dispatching the fallback would mostly
    /// add latency and either find the same N candidates or pick a
    /// different one for non-principled reasons (PR #261 cycle 12
    /// review F34 — reviewer flagged this as a potential recovery
    /// path but rated the suggestion 72% confidence with an explicit
    /// "the 'mutually exclusive' design rationale is legitimate"
    /// caveat; documenting the intent so future reviewers don't
    /// re-flag the same design choice).
    fn resolve_rollout(&self, ctx: &BindContext<'_>) -> Result<RolloutLocation, LocatorError> {
        match self.primary.resolve_rollout(ctx) {
            Ok(location) => Ok(location),
            // `starts_with(SCHEMA_DRIFT_ERROR_PREFIX)` rather than
            // `contains(...)` because the prefix is the structurally
            // significant part. Both producer sites in
            // `SqliteFirstLocator` build their message via
            // `format!("{}...", SCHEMA_DRIFT_ERROR_PREFIX)`, so a
            // rename of the constant updates both producers AND this
            // consumer in one diff — compile-time contract between
            // the three sites (PR #261 cycle 13 F35 introduced the
            // prefix match; cycle 15 F40 added the shared constant
            // so the guard can't silently desync from the producers).
            Err(LocatorError::Unresolved(reason))
                if reason.starts_with(SCHEMA_DRIFT_ERROR_PREFIX) =>
            {
                self.fallback.resolve_rollout(ctx)
            }
            Err(other) => Err(other),
        }
    }
}

// ----------- Step B' new trait surface -----------
//
// `StatusSourceLocator::locate` wraps `resolve_rollout` with the
// codex bind retry budget (5 attempts × 100ms inter-attempt sleeps).
// Per frozen constraint #2, the retry lives at THIS boundary — one
// outer loop around the full primary→fallback chain, not around
// individual strategies.

impl crate::agent::adapter::traits::StatusSourceLocator for CompositeLocator {
    fn locate(
        &self,
        cwd: &std::path::Path,
        _session_id: &str,
    ) -> Result<crate::agent::adapter::types::LocatedStatusSource, String> {
        let ctx = BindContext {
            cwd,
            pid: self.pid,
            pty_start: self.pty_start,
        };
        let location = retry_locator(|| self.resolve_rollout(&ctx))?;
        // `to_str()` rejects non-UTF-8 paths by returning `None`,
        // producing a clean `TxOutcome::NoPath` downstream. The
        // previous `to_string_lossy().into_owned()` silently replaced
        // invalid bytes with U+FFFD, yielding a "valid" String that
        // then failed `validate_transcript_path` on every watcher tick
        // — invisible at attach-time, noisy per-update warn spam.
        // PR #261 cycle 6 review F18 — Codex paths under `~/.codex/`
        // are ASCII in practice but the defensive None handles the
        // edge case (Windows home dirs with non-UTF-8 bytes, etc.).
        let static_transcript_hint = location.rollout_path.to_str().map(str::to_owned);
        if static_transcript_hint.is_none() {
            // Attach-time anchor so operators correlating "AgentToolCall
            // stream is empty for this session" have a single log line
            // to grep for rather than per-tick `TxOutcome::NoPath`
            // sentinels (PR #261 cycle 12 review F33). Fires at most
            // once per attach.
            log::warn!(
                "codex: rollout_path contains non-UTF-8 bytes — transcript tailing disabled (path={:?})",
                location.rollout_path,
            );
        }
        Ok(crate::agent::adapter::types::LocatedStatusSource {
            status_path: location.rollout_path,
            trust_root: self.codex_home.clone(),
            static_transcript_hint,
            // Codex's `thread_id` IS its agent_session_id — the same value
            // that appears as `id` in `session_index.jsonl`. Surfaces here
            // so `SessionLifecycle` can wire the codex title-sync watcher
            // back into production (PR #302 codex review F5 — the previous
            // refactor dropped this wiring, parking `agent-session-title`
            // emits for live Codex sessions).
            agent_session_id: Some(location.thread_id),
            resolved_directory: None,
        })
    }
}

/// Retry a codex locator resolution up to the bind budget.
///
/// Moved from `codex/mod.rs` to live with the locator that uses it.
/// Private — the only caller is `StatusSourceLocator::locate`
/// above and same-file `retry_locator_tests`. Cycle 5 (F15)
/// narrowed `pub(crate)` → `pub(super)`; cycle 9 (F25) further
/// narrowed to plain `fn` since `codex/mod.rs` (the only sibling
/// module) has no caller. Closes the accidental surface so future
/// code can't bypass the trait and reintroduce ad-hoc retry chains.
fn retry_locator<F>(mut resolve: F) -> Result<RolloutLocation, String>
where
    F: FnMut() -> Result<RolloutLocation, LocatorError>,
{
    let started = std::time::Instant::now();
    let mut last_reason = String::from("no attempts");

    for attempt in 0..CODEX_BIND_RETRY_MAX_ATTEMPTS {
        match resolve() {
            Ok(location) => return Ok(location),
            Err(LocatorError::NotYetReady) => {
                last_reason = format!("not yet ready (attempt {})", attempt + 1);
                if attempt + 1 < CODEX_BIND_RETRY_MAX_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(
                        CODEX_BIND_RETRY_INTERVAL_MS,
                    ));
                }
            }
            // Distinct prefixes so incident triage + log scraping can
            // tell "no unique candidate" (ambiguous) apart from
            // "filesystem / DB really broken" (fatal). The split also
            // lines up with the `AttachError::LocatorAmbiguous` vs
            // `LocatorFatal` enum split in `error.rs` (PR #261
            // Claude review cycle 2, F4).
            Err(LocatorError::Unresolved(reason)) => {
                return Err(format!("codex bind ambiguous: {}", reason));
            }
            Err(LocatorError::Fatal(reason)) => {
                return Err(format!("codex bind fatal: {}", reason));
            }
        }
    }

    log::warn!(
        "codex bind retry exhausted after {} attempts (elapsed={:?})",
        CODEX_BIND_RETRY_MAX_ATTEMPTS,
        started.elapsed()
    );
    Err(format!("codex bind retry exhausted: {}", last_reason))
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    fn make_db(path: &Path, table: &str) {
        let conn = Connection::open(path).expect("open test db");
        conn.execute(
            &format!("CREATE TABLE {} (id INTEGER PRIMARY KEY)", table),
            [],
        )
        .expect("create test table");
    }

    #[test]
    fn picks_db_with_target_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_db(&dir.path().join("logs_1.sqlite"), "logs");
        make_db(&dir.path().join("state_1.sqlite"), "threads");

        let logs = discover_db(dir.path(), "logs").expect("discover logs");
        let state = discover_db(dir.path(), "threads").expect("discover state");

        assert!(logs.expect("logs db").ends_with("logs_1.sqlite"));
        assert!(state.expect("state db").ends_with("state_1.sqlite"));
    }

    #[test]
    fn returns_none_when_no_db_has_target_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_db(&dir.path().join("logs_1.sqlite"), "logs");

        let result = discover_db(dir.path(), "threads").expect("schema drift result");
        assert!(result.is_none());
    }

    #[test]
    fn highest_numeric_suffix_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_db(&dir.path().join("logs_1.sqlite"), "logs");
        make_db(&dir.path().join("logs_3.sqlite"), "logs");
        make_db(&dir.path().join("logs_2.sqlite"), "logs");

        let picked = discover_db(dir.path(), "logs")
            .expect("discover logs")
            .expect("logs db");
        assert!(picked.ends_with("logs_3.sqlite"));
    }

    #[test]
    fn skips_wal_and_shm_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_db(&dir.path().join("logs_1.sqlite"), "logs");
        std::fs::write(dir.path().join("logs_1.sqlite-wal"), b"").expect("touch wal");
        std::fs::write(dir.path().join("logs_1.sqlite-shm"), b"").expect("touch shm");

        let picked = discover_db(dir.path(), "logs")
            .expect("discover logs")
            .expect("logs db");
        assert!(picked.ends_with("logs_1.sqlite"));
    }
}

#[cfg(test)]
mod rate_limit_header_tests {
    use super::*;

    const LOG_BODY: &str = r#"Request completed method=POST headers={"x-codex-primary-used-percent": "10", "x-codex-secondary-used-percent": "50", "x-codex-primary-reset-at": "1781020167", "x-codex-secondary-reset-at": "1781144090", "x-codex-bengalfox-primary-used-percent": "0", "x-codex-bengalfox-secondary-used-percent": "0"}"#;

    #[test]
    fn parses_account_rate_limits_from_codex_response_headers() {
        let rate_limits =
            account_rate_limits_from_log_body(LOG_BODY).expect("account headers parse");

        assert_eq!(rate_limits.five_hour.used_percentage, 10.0);
        assert_eq!(rate_limits.five_hour.resets_at, 1781020167);

        let seven_day = rate_limits.seven_day.expect("weekly limit");
        assert_eq!(seven_day.used_percentage, 50.0);
        assert_eq!(seven_day.resets_at, 1781144090);
    }

    #[test]
    fn returns_none_when_account_headers_are_absent() {
        let body = r#"headers={"x-codex-bengalfox-primary-used-percent": "0"}"#;

        assert!(account_rate_limits_from_log_body(body).is_none());
    }

    #[test]
    fn latest_account_rate_limits_reads_newest_thread_log_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logs_path = dir.path().join("logs_1.sqlite");
        let conn = Connection::open(&logs_path).expect("open logs db");
        conn.execute_batch(
            "CREATE TABLE logs (
                id INTEGER PRIMARY KEY,
                ts INTEGER NOT NULL,
                ts_nanos INTEGER NOT NULL,
                thread_id TEXT,
                feedback_log_body TEXT
            );",
        )
        .expect("logs schema");
        conn.execute(
            "INSERT INTO logs (ts, ts_nanos, thread_id, feedback_log_body)
             VALUES (1, 0, 'thread-A', ?1)",
            rusqlite::params![r#"headers={"x-codex-primary-used-percent": "1", "x-codex-secondary-used-percent": "2", "x-codex-primary-reset-at": "100", "x-codex-secondary-reset-at": "200"}"#],
        )
        .expect("insert old row");
        conn.execute(
            "INSERT INTO logs (ts, ts_nanos, thread_id, feedback_log_body)
             VALUES (2, 0, 'thread-B', ?1)",
            rusqlite::params![r#"headers={"x-codex-primary-used-percent": "90", "x-codex-secondary-used-percent": "91", "x-codex-primary-reset-at": "900", "x-codex-secondary-reset-at": "910"}"#],
        )
        .expect("insert other thread row");
        conn.execute(
            "INSERT INTO logs (ts, ts_nanos, thread_id, feedback_log_body)
             VALUES (3, 0, 'thread-A', ?1)",
            rusqlite::params![LOG_BODY],
        )
        .expect("insert newest row");

        let locator =
            CompositeLocator::new(dir.path().to_path_buf(), 123, SystemTime::UNIX_EPOCH, None);
        let rate_limits = locator
            .latest_account_rate_limits("thread-A")
            .expect("latest headers should parse");

        assert_eq!(rate_limits.five_hour.used_percentage, 10.0);
        assert_eq!(
            rate_limits.seven_day.expect("weekly limit").used_percentage,
            50.0
        );
    }

    #[test]
    fn parses_varied_json_whitespace() {
        // compact — no spaces
        let body_compact = r#"headers={"x-codex-primary-used-percent":"75","x-codex-primary-reset-at":"1781020167"}"#;
        let rate_limits =
            account_rate_limits_from_log_body(body_compact).expect("compact headers parse");
        assert_eq!(rate_limits.five_hour.used_percentage, 75.0);
        assert_eq!(rate_limits.five_hour.resets_at, 1781020167);

        // extra spaces around colon
        let body_spaced = r#"headers={"x-codex-primary-used-percent" : "60", "x-codex-primary-reset-at" : "1000"}"#;
        let rate_limits =
            account_rate_limits_from_log_body(body_spaced).expect("spaced headers parse");
        assert_eq!(rate_limits.five_hour.used_percentage, 60.0);
        assert_eq!(rate_limits.five_hour.resets_at, 1000);
    }

    #[test]
    fn preserves_usage_when_reset_header_is_absent() {
        let body = r#"headers={"x-codex-primary-used-percent": "80"}"#;

        let rate_limits =
            account_rate_limits_from_log_body(body).expect("usage without reset parses");

        assert_eq!(rate_limits.five_hour.used_percentage, 80.0);
        assert_eq!(rate_limits.five_hour.resets_at, 0);
        assert!(rate_limits.seven_day.is_none());
    }
}

#[cfg(test)]
mod sqlite_first_tests {
    use super::*;
    use std::time::Duration;

    fn build_logs_db(path: &Path) {
        let conn = Connection::open(path).expect("open logs db");
        conn.execute_batch(
            "CREATE TABLE logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                ts_nanos INTEGER NOT NULL,
                level TEXT NOT NULL,
                target TEXT NOT NULL,
                thread_id TEXT,
                process_uuid TEXT
            );",
        )
        .expect("create logs table");
    }

    fn build_state_db(path: &Path) {
        let conn = Connection::open(path).expect("open state db");
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                cwd TEXT,
                updated_at_ms INTEGER NOT NULL DEFAULT 0
            );",
        )
        .expect("create threads table");
    }

    fn fake_proc_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp proc root")
    }

    fn write_cmdline(proc_root: &Path, pid: u32, args: &[&str]) {
        let pid_dir = proc_root.join(pid.to_string());
        std::fs::create_dir_all(&pid_dir).expect("create fake /proc pid dir");
        // A real /proc/<pid> always has an fd/ dir (every live process holds
        // fds 0/1/2). Create an empty one so the open-FD provider reports
        // `Ok(∅)` (process alive, no rollout open) rather than `Err` (dead pid)
        // — VIM-191's resolver treats those two cases very differently.
        std::fs::create_dir_all(pid_dir.join("fd")).expect("create fake /proc fd dir");
        let mut bytes = Vec::new();
        for arg in args {
            bytes.extend_from_slice(arg.as_bytes());
            bytes.push(0);
        }
        std::fs::write(pid_dir.join("cmdline"), bytes).expect("write fake cmdline");
    }

    fn write_rollout_fd(proc_root: &Path, pid: u32, fd_name: &str, rollout_path: &Path) {
        let fd_dir = proc_root.join(pid.to_string()).join("fd");
        std::fs::create_dir_all(&fd_dir).expect("create fake /proc fd dir");
        let link_path = fd_dir.join(fd_name);
        #[cfg(unix)]
        std::os::unix::fs::symlink(rollout_path, link_path).expect("create fake fd symlink");
    }

    fn insert_log_row(
        path: &Path,
        process_uuid: &str,
        thread_id: Option<&str>,
        ts: i64,
        ts_nanos: i64,
    ) {
        let conn = Connection::open(path).expect("open logs db");
        conn.execute(
            "INSERT INTO logs (ts, ts_nanos, level, target, thread_id, process_uuid)
             VALUES (?, ?, 'INFO', 'test', ?, ?)",
            rusqlite::params![ts, ts_nanos, thread_id, process_uuid],
        )
        .expect("insert log row");
    }

    fn insert_thread(path: &Path, id: &str, rollout: &str, updated_at_ms: i64) {
        let conn = Connection::open(path).expect("open state db");
        conn.execute(
            "INSERT INTO threads (id, rollout_path, cwd, updated_at_ms) VALUES (?, ?, '/tmp', ?)",
            rusqlite::params![id, rollout, updated_at_ms],
        )
        .expect("insert thread row");
    }

    fn touch_thread(path: &Path, id: &str, updated_at_ms: i64) {
        let conn = Connection::open(path).expect("open state db");
        conn.execute(
            "UPDATE threads SET updated_at_ms = ? WHERE id = ?",
            rusqlite::params![updated_at_ms, id],
        )
        .expect("update thread row");
    }

    fn ctx<'a>(cwd: &'a Path, pid: u32, pty_start: SystemTime) -> BindContext<'a> {
        BindContext {
            cwd,
            pid,
            pty_start,
        }
    }

    #[test]
    fn happy_path_logs_then_threads_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logs = dir.path().join("logs_1.sqlite");
        let state = dir.path().join("state_1.sqlite");
        build_logs_db(&logs);
        build_state_db(&state);

        let pty_start = SystemTime::now() - Duration::from_secs(60);
        let pty_secs = pty_start
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("pty_start after epoch")
            .as_secs() as i64;

        insert_log_row(&logs, "pid:12345:abc", Some("thread-A"), pty_secs + 5, 0);
        insert_thread(&state, "thread-A", "/tmp/rollout-A.jsonl", 1000);

        let locator = SqliteFirstLocator::new(dir.path().to_path_buf());
        let result = locator
            .resolve_rollout(&ctx(dir.path(), 12345, pty_start))
            .expect("sqlite bind succeeds");
        assert_eq!(result.thread_id, "thread-A");
        assert_eq!(result.rollout_path, PathBuf::from("/tmp/rollout-A.jsonl"));
        assert_eq!(result.state_updated_at_ms, 1000);
    }

    #[test]
    fn pty_start_filters_out_old_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logs = dir.path().join("logs_1.sqlite");
        let state = dir.path().join("state_1.sqlite");
        build_logs_db(&logs);
        build_state_db(&state);

        let pty_start = SystemTime::now();
        let pty_secs = pty_start
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("pty_start after epoch")
            .as_secs() as i64;

        insert_log_row(
            &logs,
            "pid:12345:old",
            Some("thread-OLD"),
            pty_secs - 3600,
            0,
        );
        insert_thread(&state, "thread-OLD", "/tmp/rollout-OLD.jsonl", 1);

        let locator = SqliteFirstLocator::new(dir.path().to_path_buf());
        let result = locator.resolve_rollout(&ctx(dir.path(), 12345, pty_start));
        assert!(matches!(result, Err(LocatorError::NotYetReady)));
    }

    #[test]
    fn missing_thread_row_is_not_yet_ready() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logs = dir.path().join("logs_1.sqlite");
        let state = dir.path().join("state_1.sqlite");
        build_logs_db(&logs);
        build_state_db(&state);

        let pty_start = SystemTime::now();
        let pty_secs = pty_start
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("pty_start after epoch")
            .as_secs() as i64;

        insert_log_row(
            &logs,
            "pid:12345:abc",
            Some("thread-orphan"),
            pty_secs + 1,
            0,
        );

        let locator = SqliteFirstLocator::new(dir.path().to_path_buf());
        let result = locator.resolve_rollout(&ctx(dir.path(), 12345, pty_start));
        assert!(matches!(result, Err(LocatorError::NotYetReady)));
    }

    #[test]
    fn nanosecond_tuple_comparison_passes_within_same_second() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logs = dir.path().join("logs_1.sqlite");
        let state = dir.path().join("state_1.sqlite");
        build_logs_db(&logs);
        build_state_db(&state);

        let pty_secs = 1_777_900_000_i64;
        let pty_nanos = 500_000_000_i64;
        let pty_start = SystemTime::UNIX_EPOCH + Duration::new(pty_secs as u64, pty_nanos as u32);

        insert_log_row(
            &logs,
            "pid:777:abc",
            Some("thread-NANOS"),
            pty_secs,
            600_000_000,
        );
        insert_thread(&state, "thread-NANOS", "/tmp/rollout-NANOS.jsonl", 1);

        let locator = SqliteFirstLocator::new(dir.path().to_path_buf());
        let result = locator
            .resolve_rollout(&ctx(dir.path(), 777, pty_start))
            .expect("same-second nanos should bind");
        assert_eq!(result.thread_id, "thread-NANOS");
    }

    #[test]
    fn schema_drift_returns_unresolved_for_caller_dispatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);

        let locator = SqliteFirstLocator::new(dir.path().to_path_buf());
        let result = locator.resolve_rollout(&ctx(dir.path(), 1, SystemTime::now()));
        assert!(matches!(result, Err(LocatorError::Unresolved(_))));
    }

    #[test]
    fn resume_cmdline_fast_path_binds_without_logs_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);
        insert_thread(&state, "thread-resume", "/tmp/rollout-resume.jsonl", 44);

        let proc_root = fake_proc_root();
        write_cmdline(
            proc_root.path(),
            4242,
            &["/vendor/codex/codex", "resume", "thread-resume"],
        );

        let locator = SqliteFirstLocator::with_proc_root(
            dir.path().to_path_buf(),
            Some(proc_root.path().into()),
        );
        let result = locator
            .resolve_rollout(&ctx(dir.path(), 4242, SystemTime::now()))
            .expect("resume argv should bind");

        assert_eq!(result.thread_id, "thread-resume");
        assert_eq!(
            result.rollout_path,
            PathBuf::from("/tmp/rollout-resume.jsonl")
        );
    }

    #[test]
    fn proc_fd_fast_path_binds_without_logs_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);

        let rollout_path = dir
            .path()
            .join("sessions/2026/05/04/rollout-2026-05-04T00-00-00-thread-fd.jsonl");
        std::fs::create_dir_all(rollout_path.parent().expect("rollout parent"))
            .expect("create rollout dir");
        std::fs::write(&rollout_path, "").expect("seed rollout");
        insert_thread(&state, "thread-fd", &rollout_path.to_string_lossy(), 55);

        let proc_root = fake_proc_root();
        write_cmdline(proc_root.path(), 5151, &["/vendor/codex/codex"]);
        write_rollout_fd(proc_root.path(), 5151, "30", &rollout_path);

        let locator = SqliteFirstLocator::with_proc_root(
            dir.path().to_path_buf(),
            Some(proc_root.path().into()),
        );
        let result = locator
            .resolve_rollout(&ctx(dir.path(), 5151, SystemTime::now()))
            .expect("open rollout fd should bind");

        assert_eq!(result.thread_id, "thread-fd");
        assert_eq!(result.rollout_path, rollout_path);
    }

    #[test]
    fn proc_root_none_skips_proc_fast_paths_and_falls_through_to_logs() {
        // PR #302 Claude review F1 — non-Linux platforms (macOS / Windows)
        // pass `proc_root: None`, so the resume-argv fast-path
        // (`resume_thread_id_from_proc`) skips itself and the open-FD provider
        // is `NullOpenRolloutFds` (`Ok(∅)`) rather than probing nonexistent
        // `/proc/<pid>/` paths. Pin the contract: with `None`, the locator
        // still binds via the logs-table path.
        let dir = tempfile::tempdir().expect("tempdir");
        let logs = dir.path().join("logs_1.sqlite");
        let state = dir.path().join("state_1.sqlite");
        build_logs_db(&logs);
        build_state_db(&state);

        let pty_start = SystemTime::now() - Duration::from_secs(60);
        let pty_secs = pty_start
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("pty_start after epoch")
            .as_secs() as i64;

        insert_log_row(&logs, "pid:7777:abc", Some("thread-NL"), pty_secs + 5, 0);
        insert_thread(&state, "thread-NL", "/tmp/rollout-NL.jsonl", 1000);

        // Crucially: NO proc root, and inject a null FD provider so the test
        // stays hermetic — on Unix, `None` proc_root otherwise selects the real
        // `lsof` provider (VIM-190). The contract under test is the logs
        // fallback when no open-FD signal is available.
        let locator = SqliteFirstLocator::with_fds(
            dir.path().to_path_buf(),
            None,
            Box::new(NullOpenRolloutFds),
        );
        let result = locator
            .resolve_rollout(&ctx(dir.path(), 7777, pty_start))
            .expect("logs path binds when proc fast-paths are skipped");
        assert_eq!(result.thread_id, "thread-NL");
        assert_eq!(result.rollout_path, PathBuf::from("/tmp/rollout-NL.jsonl"));
    }

    #[test]
    fn recent_state_heuristic_binds_single_candidate_when_logs_are_threadless() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logs = dir.path().join("logs_1.sqlite");
        let state = dir.path().join("state_1.sqlite");
        build_logs_db(&logs);
        build_state_db(&state);

        let pty_start = SystemTime::now();
        let pty_secs = pty_start
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("pty_start after epoch")
            .as_secs() as i64;
        insert_log_row(&logs, "pid:9090:abc", None, pty_secs + 1, 0);
        insert_thread(
            &state,
            "thread-recent",
            "/tmp/rollout-recent.jsonl",
            pty_secs * 1000 + 1_500,
        );

        let proc_root = fake_proc_root();
        write_cmdline(proc_root.path(), 9090, &["/vendor/codex/codex"]);

        let locator = SqliteFirstLocator::with_proc_root(
            dir.path().to_path_buf(),
            Some(proc_root.path().into()),
        );
        let result = locator
            .resolve_rollout(&ctx(dir.path(), 9090, pty_start))
            .expect("recent thread row should bind when logs are threadless");

        assert_eq!(result.thread_id, "thread-recent");
        assert_eq!(
            result.rollout_path,
            PathBuf::from("/tmp/rollout-recent.jsonl")
        );
    }

    // --- OpenRolloutFds seam (VIM-189) ---

    /// Fake provider so the open-FD resolver paths can be driven without a real
    /// `/proc` (and so the VIM-191 relocation test can make the open FD
    /// disagree with stale argv/sqlite).
    struct FakeOpenRolloutFds {
        paths: HashSet<PathBuf>,
        fail: bool,
    }

    impl OpenRolloutFds for FakeOpenRolloutFds {
        fn open_rollout_paths(&self, _pid: u32) -> io::Result<HashSet<PathBuf>> {
            if self.fail {
                return Err(io::Error::other("fake provider failure"));
            }
            Ok(self.paths.clone())
        }
    }

    #[test]
    fn proc_open_fds_lists_open_rollout_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rollout = dir.path().join("sessions").join("rollout-A.jsonl");
        std::fs::create_dir_all(rollout.parent().expect("parent")).expect("mk sessions dir");
        std::fs::write(&rollout, b"{}").expect("write rollout");

        let proc_root = fake_proc_root();
        write_rollout_fd(proc_root.path(), 4242, "7", &rollout);
        // A non-rollout fd (points outside `sessions/`) must be ignored.
        write_rollout_fd(proc_root.path(), 4242, "8", dir.path());

        let provider = ProcOpenRolloutFds::new(proc_root.path().to_path_buf(), dir.path());
        let found = provider.open_rollout_paths(4242).expect("provider runs");
        assert_eq!(found, HashSet::from([rollout]));
    }

    #[test]
    fn proc_open_fds_ok_empty_when_no_rollout_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let proc_root = fake_proc_root();
        // An fd dir exists but holds only a non-rollout symlink.
        write_rollout_fd(proc_root.path(), 4243, "3", dir.path());

        let provider = ProcOpenRolloutFds::new(proc_root.path().to_path_buf(), dir.path());
        let found = provider.open_rollout_paths(4243).expect("provider runs");
        assert!(found.is_empty());
    }

    #[test]
    fn proc_open_fds_errs_when_fd_dir_missing() {
        // The `Err` != `Ok(∅)` contract (VIM-189/191): an unreadable
        // `/proc/<pid>/fd` (pid gone / denied) is a provider failure, NOT
        // "the process has no rollout open".
        let dir = tempfile::tempdir().expect("tempdir");
        let proc_root = fake_proc_root();
        let provider = ProcOpenRolloutFds::new(proc_root.path().to_path_buf(), dir.path());
        let result = provider.open_rollout_paths(999_999);
        assert!(
            result.is_err(),
            "missing fd dir must surface as Err, not Ok(empty)"
        );
    }

    #[test]
    fn null_open_fds_returns_ok_empty() {
        let found = NullOpenRolloutFds
            .open_rollout_paths(1)
            .expect("null provider never fails");
        assert!(found.is_empty());
    }

    #[test]
    fn injected_fds_provider_drives_proc_fd_resolution() {
        // The seam is wired into resolve: with no proc root (resume-argv path
        // skipped) an injected provider's open rollout selects the thread.
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);
        let rollout = "/sessions/rollout-INJECTED.jsonl";
        insert_thread(&state, "thread-INJECTED", rollout, 1000);

        let provider = FakeOpenRolloutFds {
            paths: HashSet::from([PathBuf::from(rollout)]),
            fail: false,
        };
        let locator =
            SqliteFirstLocator::with_fds(dir.path().to_path_buf(), None, Box::new(provider));
        let result = locator
            .resolve_rollout(&ctx(dir.path(), 4444, SystemTime::now()))
            .expect("injected open fd should bind");
        assert_eq!(result.thread_id, "thread-INJECTED");
        assert_eq!(result.rollout_path, PathBuf::from(rollout));
    }

    #[test]
    fn provider_err_surfaces_not_yet_ready() {
        // VIM-191 made the provider `Err` authoritative: it surfaces as
        // `NotYetReady` (retried; persistent failure fails safe) and never
        // silently falls back to stale data. (Was `provider_err_falls_through_
        // for_now` under the VIM-189 behavior-preserving seam.)
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);
        insert_thread(&state, "thread-X", "/sessions/rollout-X.jsonl", 1000);

        let provider = FakeOpenRolloutFds {
            paths: HashSet::new(),
            fail: true,
        };
        let locator =
            SqliteFirstLocator::with_fds(dir.path().to_path_buf(), None, Box::new(provider));
        let result = locator.resolve_rollout(&ctx(dir.path(), 4445, SystemTime::now()));
        assert!(matches!(result, Err(LocatorError::NotYetReady)));
    }

    // --- lsof open-FD provider (VIM-190) ---

    #[cfg(unix)]
    struct FakeLsofRunner {
        result: Result<String, io::ErrorKind>,
    }

    #[cfg(unix)]
    impl LsofRunner for FakeLsofRunner {
        fn run(&self, _pid: u32) -> io::Result<String> {
            self.result
                .clone()
                .map_err(|kind| io::Error::new(kind, "fake lsof failure"))
        }
    }

    #[cfg(unix)]
    #[test]
    fn parse_lsof_keeps_only_session_rollouts() {
        let sessions = PathBuf::from("/home/u/.codex/sessions");
        let out = concat!(
            "p4242\n",
            "fcwd\n",
            "n/home/u/project\n",
            "ftxt\n",
            "n/usr/lib/dyld\n",
            "f3\n",
            "n/home/u/.codex/sessions/2026/06/20/rollout-AAA.jsonl\n",
            "f4\n",
            "nTCP 127.0.0.1:5151\n",
            "f5\n",
            "n/home/u/.codex/sessions/2026/06/20/rollout-BBB.jsonl\n",
            "f6\n",
            "n/home/u/.codex/sessions/notes.txt\n",
        );
        let got = parse_lsof_rollout_names(out, &sessions);
        assert_eq!(
            got,
            HashSet::from([
                PathBuf::from("/home/u/.codex/sessions/2026/06/20/rollout-AAA.jsonl"),
                PathBuf::from("/home/u/.codex/sessions/2026/06/20/rollout-BBB.jsonl"),
            ])
        );
    }

    #[cfg(unix)]
    #[test]
    fn lsof_provider_lists_rollouts_via_runner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rollout = dir
            .path()
            .join("sessions")
            .join("x")
            .join("rollout-Z.jsonl");
        let stdout = format!("p1\nf3\nn{}\n", rollout.display());
        let provider = LsofOpenRolloutFds::with_runner(
            dir.path(),
            Box::new(FakeLsofRunner { result: Ok(stdout) }),
        );
        let got = provider.open_rollout_paths(1).expect("provider runs");
        assert_eq!(got, HashSet::from([rollout]));
    }

    #[cfg(unix)]
    #[test]
    fn lsof_provider_ok_empty_when_no_rollout_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = LsofOpenRolloutFds::with_runner(
            dir.path(),
            Box::new(FakeLsofRunner {
                result: Ok("p1\nf3\nn/usr/lib/dyld\n".to_string()),
            }),
        );
        let got = provider.open_rollout_paths(1).expect("provider runs");
        assert!(got.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn lsof_provider_propagates_runner_err() {
        // The `Err` != `Ok(∅)` contract: a failed lsof (timeout / non-zero exit)
        // must surface as `Err`, not be conflated with "no FD".
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = LsofOpenRolloutFds::with_runner(
            dir.path(),
            Box::new(FakeLsofRunner {
                result: Err(io::ErrorKind::TimedOut),
            }),
        );
        let result = provider.open_rollout_paths(1);
        assert!(result.is_err(), "runner Err must propagate as provider Err");
    }

    #[cfg(unix)]
    #[test]
    fn lsof_provider_treats_missing_lsof_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = LsofOpenRolloutFds::with_runner(
            dir.path(),
            Box::new(FakeLsofRunner {
                result: Err(io::ErrorKind::NotFound),
            }),
        );
        let got = provider
            .open_rollout_paths(1)
            .expect("missing lsof falls back");
        assert!(got.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn real_lsof_runner_runs_bounded_without_panicking() {
        // Smoke test of the real spawn path (constructs RealLsofRunner): it must
        // complete within the bounded timeout and never panic. The exact Ok/Err
        // depends on whether lsof is installed and the pid exists, so only
        // completion is asserted; the Err mapping is covered above.
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = LsofOpenRolloutFds::new(dir.path());
        let _ = provider.open_rollout_paths(u32::MAX);
    }

    // --- resolver Option A: open-FD authoritative (VIM-191) ---

    /// THE relocation regression: an in-session `resume` keeps the same cmdline,
    /// so the stale `resume`-argv would bind the OLD thread (A) forever. The
    /// open rollout FD points at the NEW thread (B). Option A consults the FD
    /// first, so B wins. This fails before the resolver reorder (argv → A) and
    /// passes after (open-FD → B).
    #[cfg(unix)]
    #[test]
    fn open_fd_beats_stale_resume_argv() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        let rollout_a = sessions.join("rollout-A.jsonl");
        let rollout_b = sessions.join("rollout-B.jsonl");
        // A looks "newer" by recency and is named by the stale resume argv …
        insert_thread(&state, "thread-A", rollout_a.to_str().expect("utf8"), 2000);
        // … but B is the one whose rollout FD is actually open.
        insert_thread(&state, "thread-B", rollout_b.to_str().expect("utf8"), 1000);

        let proc_root = fake_proc_root();
        write_cmdline(proc_root.path(), 6001, &["codex", "resume", "thread-A"]);
        write_rollout_fd(proc_root.path(), 6001, "7", &rollout_b);

        let locator = SqliteFirstLocator::with_proc_root(
            dir.path().to_path_buf(),
            Some(proc_root.path().to_path_buf()),
        );
        let result = locator
            .resolve_rollout(&ctx(dir.path(), 6001, SystemTime::now()))
            .expect("open rollout FD should bind");
        assert_eq!(result.thread_id, "thread-B");
        assert_eq!(result.rollout_path, rollout_b);
    }

    #[test]
    fn provider_ok_empty_falls_back_to_resume_argv() {
        // `codex resume <id>` launch: the rollout FD is not open yet, so the
        // provider returns `Ok(∅)` and the resolver falls back to the resume
        // argv (the early-window contract).
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);
        insert_thread(&state, "thread-A", "/s/rollout-A.jsonl", 1000);

        let proc_root = fake_proc_root();
        write_cmdline(proc_root.path(), 6002, &["codex", "resume", "thread-A"]);

        let locator = SqliteFirstLocator::with_fds(
            dir.path().to_path_buf(),
            Some(proc_root.path().to_path_buf()),
            Box::new(FakeOpenRolloutFds {
                paths: HashSet::new(),
                fail: false,
            }),
        );
        let result = locator
            .resolve_rollout(&ctx(dir.path(), 6002, SystemTime::now()))
            .expect("Ok(empty) falls back to resume argv");
        assert_eq!(result.thread_id, "thread-A");
    }

    #[test]
    fn provider_err_does_not_fall_back_to_stale_argv() {
        // The false-success trap: a provider `Err` (lsof failed / `/proc` gone)
        // must NOT silently bind the stale resume-argv thread. It surfaces as
        // `NotYetReady` (retried; persistent failure fails safe).
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);
        insert_thread(&state, "thread-A", "/s/rollout-A.jsonl", 1000);

        let proc_root = fake_proc_root();
        write_cmdline(proc_root.path(), 6003, &["codex", "resume", "thread-A"]);

        let locator = SqliteFirstLocator::with_fds(
            dir.path().to_path_buf(),
            Some(proc_root.path().to_path_buf()),
            Box::new(FakeOpenRolloutFds {
                paths: HashSet::new(),
                fail: true,
            }),
        );
        let result = locator.resolve_rollout(&ctx(dir.path(), 6003, SystemTime::now()));
        assert!(
            matches!(result, Err(LocatorError::NotYetReady)),
            "provider Err must be NotYetReady, never a stale-argv bind"
        );
    }

    #[test]
    fn open_fd_without_thread_row_is_not_yet_ready() {
        // FD authoritative even before its row lands: an open rollout FD whose
        // thread row is not yet written waits (NotYetReady) rather than falling
        // back to the stale resume-argv candidate.
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);
        insert_thread(&state, "thread-A", "/s/rollout-A.jsonl", 1000);

        let proc_root = fake_proc_root();
        write_cmdline(proc_root.path(), 6004, &["codex", "resume", "thread-A"]);

        let locator = SqliteFirstLocator::with_fds(
            dir.path().to_path_buf(),
            Some(proc_root.path().to_path_buf()),
            Box::new(FakeOpenRolloutFds {
                // FD open for a rollout that has no thread row yet.
                paths: HashSet::from([PathBuf::from("/s/rollout-NEW.jsonl")]),
                fail: false,
            }),
        );
        let result = locator.resolve_rollout(&ctx(dir.path(), 6004, SystemTime::now()));
        assert!(
            matches!(result, Err(LocatorError::NotYetReady)),
            "open FD without a row must wait, not bind stale argv"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_fd_multi_finds_rollout_beyond_recency_limit() {
        // Codex review VIM-191 P2: with multiple open rollout FDs, the matching
        // thread row must be found even when it is older than the 64
        // most-recently-updated threads — the old `LIMIT 64` pre-filter would
        // truncate it and wrongly yield `NotYetReady`.
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");

        // 70 newer, unrelated threads push the open one past the recency window.
        for i in 0..70 {
            let rollout = sessions.join(format!("rollout-dummy-{i}.jsonl"));
            insert_thread(
                &state,
                &format!("dummy-{i}"),
                rollout.to_str().expect("utf8"),
                10_000 + i as i64,
            );
        }
        // The actually-open rollout's thread row is the OLDEST.
        let rollout_open = sessions.join("rollout-OPEN.jsonl");
        insert_thread(
            &state,
            "thread-OPEN",
            rollout_open.to_str().expect("utf8"),
            1,
        );

        let proc_root = fake_proc_root();
        write_cmdline(proc_root.path(), 7001, &["codex"]);
        // Two open rollout FDs (multi-FD path); the second has no thread row.
        write_rollout_fd(proc_root.path(), 7001, "10", &rollout_open);
        write_rollout_fd(
            proc_root.path(),
            7001,
            "11",
            &sessions.join("rollout-GHOST.jsonl"),
        );

        let locator = SqliteFirstLocator::with_proc_root(
            dir.path().to_path_buf(),
            Some(proc_root.path().to_path_buf()),
        );
        let result = locator
            .resolve_rollout(&ctx(dir.path(), 7001, SystemTime::now()))
            .expect("multi-fd resolver must find the open rollout beyond the recency limit");
        assert_eq!(result.thread_id, "thread-OPEN");
        assert_eq!(result.rollout_path, rollout_open);
    }

    #[cfg(unix)]
    #[test]
    fn open_fd_multi_picks_newest_when_codex_leaks_old_rollout_fds() {
        // codex keeps OLD rollout FDs open across `/clear` and in-session
        // `resume`, so a single pid holds several same-cwd rollouts open at once
        // (confirmed by `lsof`). The resolver must pick the most recently updated
        // (the active conversation) instead of erroring on same-cwd ambiguity —
        // the latter strands the panel after every `/clear`/`resume` (VIM-188).
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");

        let rollout_old = sessions.join("rollout-OLD.jsonl");
        let rollout_new = sessions.join("rollout-NEW.jsonl");
        // Same cwd ('/tmp', from insert_thread) — only updated_at_ms differs.
        insert_thread(
            &state,
            "thread-OLD",
            rollout_old.to_str().expect("utf8"),
            1000,
        );
        insert_thread(
            &state,
            "thread-NEW",
            rollout_new.to_str().expect("utf8"),
            5000,
        );

        let proc_root = fake_proc_root();
        // Stale resume-argv points at the OLD thread — the authoritative open-FD
        // (newest) must still win, since open-FD is consulted before argv.
        write_cmdline(proc_root.path(), 8001, &["codex", "resume", "thread-OLD"]);
        // The pid leaks BOTH rollouts open (the bug).
        write_rollout_fd(proc_root.path(), 8001, "10", &rollout_old);
        write_rollout_fd(proc_root.path(), 8001, "11", &rollout_new);

        let locator = SqliteFirstLocator::with_proc_root(
            dir.path().to_path_buf(),
            Some(proc_root.path().to_path_buf()),
        );
        // ctx cwd matches the threads' cwd → the old resolver would error on
        // same-cwd ambiguity; the recency pick must still bind the newest.
        let result = locator
            .resolve_rollout(&ctx(Path::new("/tmp"), 8001, SystemTime::now()))
            .expect("same-cwd multi-fd must bind the newest open rollout, not error");
        assert_eq!(result.thread_id, "thread-NEW");
        assert_eq!(result.rollout_path, rollout_new);
    }

    #[cfg(unix)]
    #[test]
    fn open_fd_multi_tied_updated_at_is_not_yet_ready() {
        // Two open rollouts sharing the newest updated_at_ms are genuinely
        // ambiguous (input order is non-deterministic) → fail safe with
        // NotYetReady rather than bind one arbitrarily.
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");

        let rollout_a = sessions.join("rollout-A.jsonl");
        let rollout_b = sessions.join("rollout-B.jsonl");
        insert_thread(&state, "thread-A", rollout_a.to_str().expect("utf8"), 3000);
        insert_thread(&state, "thread-B", rollout_b.to_str().expect("utf8"), 3000);

        let proc_root = fake_proc_root();
        write_cmdline(proc_root.path(), 8002, &["codex"]);
        write_rollout_fd(proc_root.path(), 8002, "10", &rollout_a);
        write_rollout_fd(proc_root.path(), 8002, "11", &rollout_b);

        let locator = SqliteFirstLocator::with_proc_root(
            dir.path().to_path_buf(),
            Some(proc_root.path().to_path_buf()),
        );
        let result = locator.resolve_rollout(&ctx(Path::new("/tmp"), 8002, SystemTime::now()));
        assert!(
            matches!(result, Err(LocatorError::NotYetReady)),
            "a tie on the newest updated_at_ms must be NotYetReady, not an arbitrary bind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_fd_multi_self_heals_when_resumed_idle_rollout_is_written() {
        // The idle-resume signal gap: resuming an OLD conversation does not
        // advance its `updated_at_ms` until the user interacts, so among leaked
        // FDs the resolver structurally binds the previously-active (newest)
        // rollout. There is NO resolver-side signal that can pick the resumed
        // idle rollout earlier (confirmed: sqlite `updated_at` == file mtime,
        // no active-thread column, `session_index.jsonl` is a stale title
        // cache). Recovery is therefore "self-heal on first interaction": once
        // the resumed rollout is written it becomes newest and the resolver
        // binds it. This guards the backend half of that contract; the frontend
        // persistent retry is what re-invokes the resolver until this flips.
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state_1.sqlite");
        build_state_db(&state);
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");

        let rollout_active = sessions.join("rollout-ACTIVE.jsonl");
        let rollout_resumed = sessions.join("rollout-RESUMED.jsonl");
        // ACTIVE was the previously-live conversation (newest); RESUMED is the
        // older conversation the user just resumed to but has not yet typed in.
        insert_thread(
            &state,
            "thread-ACTIVE",
            rollout_active.to_str().expect("utf8"),
            5000,
        );
        insert_thread(
            &state,
            "thread-RESUMED",
            rollout_resumed.to_str().expect("utf8"),
            1000,
        );

        let proc_root = fake_proc_root();
        write_cmdline(proc_root.path(), 8003, &["codex"]);
        // The pid leaks BOTH rollouts open.
        write_rollout_fd(proc_root.path(), 8003, "10", &rollout_active);
        write_rollout_fd(proc_root.path(), 8003, "11", &rollout_resumed);

        let locator = SqliteFirstLocator::with_proc_root(
            dir.path().to_path_buf(),
            Some(proc_root.path().to_path_buf()),
        );

        // Phase 1 — RESUMED is still idle (older): the resolver binds ACTIVE.
        // This is the unavoidable signal-gap miss, not a bug.
        let before = locator
            .resolve_rollout(&ctx(Path::new("/tmp"), 8003, SystemTime::now()))
            .expect("multi-fd binds the newest open rollout");
        assert_eq!(
            before.thread_id, "thread-ACTIVE",
            "before interaction, the idle resumed rollout cannot be distinguished from the active one"
        );

        // Phase 2 — the user interacts with RESUMED, advancing its row past
        // ACTIVE. A re-invoked resolve (what the persistent retry does) now
        // binds RESUMED, so the panel recovers without a manual click.
        touch_thread(&state, "thread-RESUMED", 9000);
        let after = locator
            .resolve_rollout(&ctx(Path::new("/tmp"), 8003, SystemTime::now()))
            .expect("multi-fd re-resolve after the resumed rollout is written");
        assert_eq!(
            after.thread_id, "thread-RESUMED",
            "once the resumed rollout is the newest, the resolver self-heals onto it"
        );
        assert_eq!(after.rollout_path, rollout_resumed);
    }
}

#[cfg(test)]
mod fs_fallback_tests {
    use super::*;
    use std::time::Duration;

    fn write_rollout(dir: &Path, name: &str, cwd: &str, id: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create rollout dir");
        let path = dir.join(name);
        let line = format!(
            r#"{{"timestamp":"...","type":"session_meta","payload":{{"id":"{}","cwd":"{}","cli_version":"0.128.0"}}}}"#,
            id, cwd
        );
        std::fs::write(&path, format!("{}\n", line)).expect("write rollout");
        path
    }

    fn ctx<'a>(cwd: &'a Path, pty_start: SystemTime) -> BindContext<'a> {
        BindContext {
            cwd,
            pid: 0,
            pty_start,
        }
    }

    #[test]
    fn fs_zero_matches_returns_not_yet_ready() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = std::path::Path::new("/tmp/no-rollouts-here");
        let fallback = FsScanFallback::new(dir.path().to_path_buf());
        let result = fallback.resolve_rollout(&ctx(cwd, SystemTime::now()));
        assert!(matches!(result, Err(LocatorError::NotYetReady)));
    }

    #[test]
    fn fs_multi_match_returns_unresolved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let date = Local::now().date_naive();
        let day_dir = dir
            .path()
            .join("sessions")
            .join(format!("{:04}", date.year()))
            .join(format!("{:02}", date.month()))
            .join(format!("{:02}", date.day()));

        let cwd_str = day_dir
            .parent()
            .expect("day dir parent")
            .to_str()
            .expect("utf8 cwd")
            .to_string();
        let cwd_path = std::path::Path::new(&cwd_str);
        let pty_start = SystemTime::now() - Duration::from_secs(10);

        write_rollout(&day_dir, "rollout-A.jsonl", cwd_str.as_str(), "id-A");
        write_rollout(&day_dir, "rollout-B.jsonl", cwd_str.as_str(), "id-B");

        let fallback = FsScanFallback::new(dir.path().to_path_buf());
        let result = fallback.resolve_rollout(&ctx(cwd_path, pty_start));
        assert!(matches!(result, Err(LocatorError::Unresolved(_))));
    }

    #[test]
    fn composite_dispatches_schema_drift_to_fs() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Step B': `CompositeLocator::new` now also takes pid +
        // pty_start so the locator can own them (they used to live on
        // `CodexAdapter`). The dummy values below don't matter for
        // this test — `resolve_rollout` is called with an explicit
        // `BindContext` that supplies its own pid/pty_start.
        // PR #302 F1: `proc_root` is `Option<PathBuf>`. `Some("/proc")`
        // here matches the pre-F1 behavior even though this test never
        // exercises the proc fast-path (the FS scan fires on schema
        // drift).
        let composite = CompositeLocator::new(
            dir.path().to_path_buf(),
            0,
            SystemTime::UNIX_EPOCH,
            Some(PathBuf::from("/proc")),
        );
        let result =
            composite.resolve_rollout(&ctx(std::path::Path::new("/tmp"), SystemTime::now()));
        assert!(matches!(result, Err(LocatorError::NotYetReady)));
    }
}

#[cfg(test)]
mod retry_locator_tests {
    //! Step B': these tests moved here from `codex/mod.rs` alongside
    //! `retry_locator` itself, so the helper and its regression suite
    //! live together.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn retries_on_not_yet_ready_then_succeeds() {
        let calls = AtomicUsize::new(0);
        let result = retry_locator(|| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 3 {
                Err(LocatorError::NotYetReady)
            } else {
                Ok(RolloutLocation {
                    rollout_path: PathBuf::from("/tmp/rollout.jsonl"),
                    thread_id: "tid".to_string(),
                    state_updated_at_ms: 0,
                })
            }
        });

        assert!(
            result.is_ok(),
            "expected Ok after 4th attempt: {:?}",
            result
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn returns_err_when_retry_budget_exhausted() {
        let calls = AtomicUsize::new(0);
        let result = retry_locator(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(LocatorError::NotYetReady)
        });

        assert!(result.is_err());
        assert!(
            result.as_ref().unwrap_err().contains("retry exhausted"),
            "expected 'retry exhausted' in: {:?}",
            result
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            CODEX_BIND_RETRY_MAX_ATTEMPTS as usize,
        );
    }

    #[test]
    fn fatal_short_circuits_immediately() {
        let calls = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let result = retry_locator(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(LocatorError::Fatal("permission denied".to_string()))
        });

        assert!(result.is_err());
        assert!(result.as_ref().unwrap_err().contains("codex bind fatal"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "fatal should short-circuit: elapsed {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn unresolved_short_circuits_immediately_with_ambiguous_prefix() {
        let calls = AtomicUsize::new(0);
        let result = retry_locator(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(LocatorError::Unresolved("ambiguous candidates".to_string()))
        });

        assert!(result.is_err());
        let err = result.as_ref().unwrap_err();
        // PR #261 cycle 2: distinct prefix so log triage can tell
        // ambiguous (no unique candidate) apart from fatal (FS / DB
        // broken). Was previously merged under "codex bind fatal".
        assert!(
            err.contains("codex bind ambiguous"),
            "expected ambiguous prefix, got: {}",
            err,
        );
        assert!(
            !err.contains("codex bind fatal"),
            "ambiguous error must NOT emit the fatal prefix, got: {}",
            err,
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
