//! kimi-code session locator.
//!
//! Resolves the attach cwd to a `wire.jsonl` for the DETECTED kimi
//! process, in priority order:
//!
//! 1. proc-fd (Linux): if a wire flush is in progress, read the kimi process's
//!    open `agents/main/wire.jsonl` from `<proc_root>/<pid>/fd/*`, keep trusted
//!    main-wire candidates, and choose the most recently active one. Kimi may
//!    close the file between flushes, so an empty fd scan falls through.
//! 2. proc-environ (Linux): read the kimi process's own
//!    `KIMI_CODE_HOME` from `<proc_root>/<pid>/environ` so a per-process
//!    `KIMI_CODE_HOME=/tmp/kimi kimi` is honored.
//! 3. index fallback: `<kimi_home>/session_index.jsonl` workDir match.
//!    A `workDir` matches when it equals `cwd` OR is a component-boundary
//!    ancestor of it (kimi normalizes a worktree / subdirectory cwd to the
//!    repo root). Among matches the longest (deepest) workDir wins, then the
//!    process-owned session. A resumed session is identified by
//!    the `session resume` diagnostic written at process startup because its
//!    original wire creation time predates the process. When the platform
//!    process start is unavailable, same-workDir ties use newest on-disk
//!    session activity so a resumed session keeps winning after kimi appends a
//!    later empty index row.
//! 4. exact-bucket sha256 scan: last-resort newest `session_*` under
//!    this cwd's `wd_<basename>_<hex>` bucket, gated on a `pty_start`
//!    mtime freshness check (no append ordering to tell current from
//!    stale, so a stale same-cwd bucket session must not win there).
//!
//! On macOS (no `/proc`, `proc_root == None`) steps 1-2 cleanly skip and
//! resolution falls through to the index / bucket fallbacks.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::agent::adapter::traits::StatusSourceLocator;
use crate::agent::adapter::types::LocatedStatusSource;
use crate::agent::types::RateLimits;

// Kimi attach/locate observability — routes through the app's debug_log
// so traces land in vimeflow-debug.log alongside the pty/bridge logs.
pub(crate) fn kdbg(msg: &str) {
    crate::debug::debug_log("kimi", msg)
}

pub(crate) const KIMI_BIND_RETRY_INTERVAL_MS: u64 = 100;
pub(crate) const KIMI_BIND_RETRY_MAX_ATTEMPTS: u32 = 5;

// Slack subtracted from `pty_start` before the index freshness check, so a
// session whose wire.jsonl was created a moment before the PTY clock read
// still counts as fresh.
const KIMI_INDEX_FRESHNESS_SLACK: Duration = Duration::from_secs(3);

// Upper bound of the window after the process start in which a session may
// have been created BY this process (session creation lags the fork slightly).
// Paired with `KIMI_INDEX_FRESHNESS_SLACK` as the lower (clock-skew) bound.
const KIMI_OWN_WINDOW: Duration = Duration::from_secs(30);
const KIMI_RESUME_LOG_TAIL_BYTES: u64 = 64 * 1024;
const MAX_RECOVERY_PATHS: usize = 256;

/// One `session_index.jsonl` line.
#[derive(Deserialize)]
struct SessionIndexEntry {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "sessionDir")]
    session_dir: Option<String>,
    #[serde(rename = "workDir")]
    work_dir: Option<String>,
}

struct IndexCandidate {
    work_len: usize,
    created: Option<SystemTime>,
    activity: Option<SystemTime>,
    index_order: usize,
    session_dir: String,
    entry: SessionIndexEntry,
}

#[derive(Clone, Copy)]
struct ProcessStartEvidence {
    at: SystemTime,
    resume_floor: SystemTime,
}

pub(crate) struct KimiLocator {
    kimi_home: PathBuf,
    validator_root: PathBuf,
    agent_pid: u32,
    pty_start: SystemTime,
    // `Some("/proc")` on Linux (or a tempdir in tests); `None` on macOS,
    // where the proc-fd / proc-environ fast-paths skip themselves.
    proc_root: Option<PathBuf>,
    // The session dir of the LAST successful `locate`, shared with the
    // decoder (same Arc) so it can read sibling `agents/agent-*` wires.
    resolved_session_dir: Arc<Mutex<Option<PathBuf>>>,
    // The kimi process's real cwd (`/proc/<pid>/cwd`) from the last
    // `locate`, shared with the tailer so it can emit `agent-cwd` from the
    // project the agent is actually in, not the stale spawn cwd.
    resolved_cwd: Arc<Mutex<Option<PathBuf>>>,
    // Plan-usage (`/usages`) state, shared with the decoder. The fetch is
    // gated on user consent and runs at most once per new main-agent turn.
    usage: Arc<Mutex<UsageState>>,
    resume_evidence: Arc<Mutex<HashMap<String, SystemTime>>>,
    process_start: Arc<Mutex<Option<ProcessStartEvidence>>>,
    recovery_paths: Arc<Mutex<Vec<PathBuf>>>,
}

/// Out-of-band kimi plan-usage state behind the locator's shared Arc. The
/// only network-fed agent state in the backend, so it is turn-debounced
/// (`fetched_turn`) and self-throttled (`in_flight`). Consent is the separate,
/// app-global `kimi_usage_consent` flag, checked before any fetch.
#[derive(Default)]
struct UsageState {
    // The main-agent settled-turn count a fetch was last kicked for. `None`
    // means "no fetch since consent turned on" — so the first poll after
    // consent is enabled (or after attach) catches up with one fetch even with
    // no new turn; thereafter only a newly-settled turn re-fetches. Reset to
    // `None` while consent is OFF so a later enable re-catches-up.
    fetched_turn: Option<u64>,
    // A fetch is running; suppresses overlap if turns arrive faster than the
    // request completes.
    in_flight: bool,
    // Last successful fetch, merged into the snapshot by `decode`.
    cached: Option<RateLimits>,
}

impl UsageState {
    /// Whether a NEW fetch should start, ignoring consent (checked separately
    /// against the app-global flag): none is in flight, and either nothing has
    /// been fetched since consent turned on (catch-up) or the main-agent
    /// settled-turn count has advanced past the last fetched turn.
    fn fetch_due(&self, settled_turn_count: u64) -> bool {
        if self.in_flight {
            return false;
        }
        match self.fetched_turn {
            None => true,
            Some(last) => settled_turn_count > last,
        }
    }
}

impl KimiLocator {
    #[cfg(test)]
    pub(crate) fn new(
        kimi_home: PathBuf,
        agent_pid: u32,
        pty_start: SystemTime,
        proc_root: Option<PathBuf>,
    ) -> Self {
        Self::with_proc_env_home(kimi_home, agent_pid, pty_start, proc_root, true)
    }

    pub(crate) fn with_proc_env_home(
        kimi_home: PathBuf,
        agent_pid: u32,
        pty_start: SystemTime,
        proc_root: Option<PathBuf>,
        honor_proc_env_home: bool,
    ) -> Self {
        let kimi_home = if honor_proc_env_home {
            proc_root
                .as_deref()
                .and_then(|root| kimi_home_from_proc_environ(root, agent_pid))
                .unwrap_or(kimi_home)
        } else {
            kimi_home
        };
        let validator_root =
            std::fs::canonicalize(&kimi_home).unwrap_or_else(|_| kimi_home.clone());
        Self {
            kimi_home,
            validator_root,
            agent_pid,
            pty_start,
            proc_root,
            resolved_session_dir: Arc::new(Mutex::new(None)),
            resolved_cwd: Arc::new(Mutex::new(None)),
            usage: Arc::new(Mutex::new(UsageState::default())),
            resume_evidence: Arc::new(Mutex::new(HashMap::new())),
            process_start: Arc::new(Mutex::new(None)),
            recovery_paths: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn remember_recovery_path(&self, path: PathBuf) {
        let mut paths = self.recovery_paths.lock().expect("recovery_paths lock");
        paths.retain(|existing| existing != &path);
        paths.push(path);
        let excess = paths.len().saturating_sub(MAX_RECOVERY_PATHS);
        if excess > 0 {
            paths.drain(..excess);
        }
    }

    pub(crate) fn recovery_paths(&self) -> Vec<PathBuf> {
        self.recovery_paths
            .lock()
            .expect("recovery_paths lock")
            .clone()
    }

    /// The session dir (`.../sessions/wd_*/session_*`) of the last
    /// successful `locate`, used by the decoder to enumerate sub-agent
    /// wires. `None` until the first resolve.
    pub(crate) fn resolved_session_dir(&self) -> Option<PathBuf> {
        self.resolved_session_dir
            .lock()
            .expect("resolved_session_dir lock")
            .clone()
    }

    /// The kimi process's resolved cwd from the last `locate`. `None` until
    /// the first resolve.
    pub(crate) fn resolved_cwd(&self) -> Option<PathBuf> {
        self.resolved_cwd.lock().expect("resolved_cwd lock").clone()
    }

    /// The last successfully fetched plan-usage limits, for `decode` to merge
    /// into the snapshot. `None` until a fetch lands (so the snapshot keeps its
    /// zeroed default) and `None` whenever consent is OFF — so a revoke hides
    /// the bars rather than leaving stale plan data merged from the cache.
    pub(crate) fn cached_rate_limits(&self) -> Option<RateLimits> {
        if !self.usage_consented() {
            return None;
        }
        self.usage.lock().expect("usage lock").cached.clone()
    }

    /// Seed the usage cache directly in tests (bypasses the network fetch).
    #[cfg(test)]
    pub(crate) fn set_cached_rate_limits_for_test(&self, rate_limits: RateLimits) {
        self.usage.lock().expect("usage lock").cached = Some(rate_limits);
    }

    /// Whether plan-usage consent is ON (the app-global flag). `decode` checks
    /// this before doing the (otherwise wasted) main-wire turn count, so an OFF
    /// session — the default — pays nothing for the usage path.
    pub(crate) fn usage_consented(&self) -> bool {
        crate::agent::kimi_usage_consent::usage_consent_enabled()
    }

    /// Re-arm the catch-up while consent is OFF — no wire read, no call — so a
    /// later enable fetches once. The supervisor calls this on the default
    /// opt-out path instead of computing the (full main-wire parse) turn count.
    pub(crate) fn disarm_usage(&self) {
        self.usage.lock().expect("usage lock").fetched_turn = None;
    }

    /// Kick a background `/usages` fetch when due (a catch-up after consent was
    /// just enabled / the session attached, or a new main-agent turn). The
    /// caller (supervisor poll) has already confirmed consent is ON, so an idle
    /// session and a just-enabled consent both fetch without a fresh prompt.
    /// Non-blocking: spawns a detached, timeout-bounded thread, so the local
    /// status path is never delayed by the network. Skips when a fetch is
    /// already in flight; the usage fetcher resolves a Kimi-shaped User-Agent
    /// version even when the transcript omits `metadata.app_version`. The
    /// worker RE-CHECKS consent at the last moment, so a revoke after the gate
    /// sends no request and writes no cache.
    pub(crate) fn maybe_refresh_usage(&self, settled_turn_count: u64, version: &str) {
        {
            let mut usage = self.usage.lock().expect("usage lock");
            if !usage.fetch_due(settled_turn_count) {
                return;
            }
            usage.fetched_turn = Some(settled_turn_count);
            usage.in_flight = true;
        }
        let home = self.effective_home();
        let version = version.to_string();
        let usage = Arc::clone(&self.usage);
        std::thread::spawn(move || {
            // A revoke between the gate above and here must cancel the call (no
            // key sent) and the cache write (no bars after opt-out).
            let fetched = crate::agent::kimi_usage_consent::usage_consent_enabled()
                .then(|| super::usage_fetch::fetch_rate_limits(&home, &version))
                .flatten();
            let mut state = usage.lock().expect("usage lock");
            if fetched.is_some() && crate::agent::kimi_usage_consent::usage_consent_enabled() {
                state.cached = fetched;
            }
            state.in_flight = false;
        });
    }

    /// Record (then pass through) a resolved source's session dir —
    /// `status_path` is `.../session_*/agents/main/wire.jsonl`, so the
    /// session dir is three components up.
    fn remember(&self, located: LocatedStatusSource) -> LocatedStatusSource {
        if let Some(session_dir) = located
            .status_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
        {
            *self
                .resolved_session_dir
                .lock()
                .expect("resolved_session_dir lock") = Some(session_dir.to_path_buf());
        }
        located
    }

    /// Effective kimi home for THIS process: the kimi process's own
    /// `KIMI_CODE_HOME` from `<proc_root>/<pid>/environ` when present,
    /// else the constructor `kimi_home`. Shared with the validator so the
    /// trust root is resolved from one source.
    pub(crate) fn effective_home(&self) -> PathBuf {
        self.kimi_home.clone()
    }

    pub(crate) fn validator_root(&self) -> &Path {
        &self.validator_root
    }

    fn session_index_path(&self, home: &Path) -> PathBuf {
        home.join("session_index.jsonl")
    }

    /// The detected kimi process's actual cwd (`/proc/<pid>/cwd`) when available —
    /// authoritative because kimi derives its session workDir from its own cwd, and
    /// the PTY-supplied cwd can be the stale spawn-time cwd. Falls back to `passed`
    /// when proc_root is None (e.g. macOS) or the link can't be read.
    fn process_cwd(&self, passed: &Path) -> PathBuf {
        self.proc_root
            .as_deref()
            .and_then(|root| {
                std::fs::read_link(root.join(self.agent_pid.to_string()).join("cwd")).ok()
            })
            .unwrap_or_else(|| passed.to_path_buf())
    }

    /// Re-run the locator for the cwd discovered during the initial attach.
    ///
    /// Kimi can create the real `session_*` after the PTY process is already
    /// attached, so the transcript supervisor uses this to move from an older
    /// same-cwd session to the one that actually starts receiving main-agent
    /// writes. OS-specific details stay inside the locator: proc-fd wins on
    /// Linux, while macOS/no-proc falls back through index/activity ranking.
    pub(crate) fn refresh_located_source(&self) -> Option<LocatedStatusSource> {
        let cwd = self.resolved_cwd()?;
        let home = self.effective_home();
        self.try_resolve_from_proc_fds(&home)
            .or_else(|| self.try_resolve_from_index(&home, &cwd))
            .or_else(|| self.try_resolve_fallback(&home, &cwd))
            .map(|located| self.remember(located))
    }

    /// Wall-clock start time of the detected kimi process, from
    /// `<proc_root>/<pid>/stat` field 22 (`starttime`, ticks since boot)
    /// anchored on the system boot epoch (`btime` in `<proc_root>/stat`).
    /// On macOS, derives the same value from `ps etime`. `None` when the
    /// platform process source is unavailable or cannot be parsed.
    fn process_start(&self) -> Option<ProcessStartEvidence> {
        if let Some(cached) = *self.process_start.lock().expect("process_start lock") {
            return Some(cached);
        }

        let resolved = self.resolve_process_start()?;
        *self.process_start.lock().expect("process_start lock") = Some(resolved);
        Some(resolved)
    }

    fn resolve_process_start(&self) -> Option<ProcessStartEvidence> {
        if let Some(proc_root) = self.proc_root.as_deref() {
            let stat =
                std::fs::read_to_string(proc_root.join(self.agent_pid.to_string()).join("stat"))
                    .ok()?;
            // `comm` (field 2) is parenthesized and may contain spaces or
            // parens, so parse the fields AFTER the last ')'. There, index 19 is
            // overall field 22 (`starttime`): fields 3.. start at index 0.
            let starttime_ticks: u64 = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(19)?
                .parse()
                .ok()?;
            let since_boot_ms = starttime_ticks.checked_mul(1000)? / clock_ticks_per_sec();
            let at = SystemTime::UNIX_EPOCH
                + Duration::from_secs(read_btime(proc_root)?)
                + Duration::from_millis(since_boot_ms);
            return Some(ProcessStartEvidence {
                at,
                resume_floor: at,
            });
        }

        #[cfg(target_os = "macos")]
        {
            // `ps etime` is second-granularity, so the estimated start can be
            // up to one second after the real start.
            let output = Command::new("ps")
                .arg("-p")
                .arg(self.agent_pid.to_string())
                .args(["-o", "etime="])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            return process_start_from_elapsed(
                String::from_utf8_lossy(&output.stdout).trim(),
                SystemTime::now(),
            )
            .map(ProcessStartEvidence::from_second_granularity);
        }

        #[cfg(not(target_os = "macos"))]
        None
    }

    /// proc-fd primary: while Kimi is flushing its session's
    /// `agents/main/wire.jsonl`, a `<proc_root>/<pid>/fd/*` symlink resolves to
    /// it. If multiple main wires are open during a session handoff, choose the
    /// most recently active trusted session. Kimi may close the file between
    /// flushes; `None` then allows index resolution to run. Skips itself when
    /// `proc_root` is `None` (macOS).
    fn try_resolve_from_proc_fds(&self, home: &Path) -> Option<LocatedStatusSource> {
        let proc_root = self.proc_root.as_deref()?;
        // Anchor the trust root to THIS process's effective home and require
        // the fd target to live under it: a detected (or spoofed) process must
        // not steer the watcher at a wire.jsonl tree outside the kimi home.
        // Canonicalize both sides so symlinked homes (NFS mounts, Docker
        // volumes, $KIMI_CODE_HOME through a symlink) don't lose the binding.
        let sessions_root = home.join("sessions");
        let trusted_root = std::fs::canonicalize(&sessions_root).unwrap_or(sessions_root);
        let mut newest: Option<(SystemTime, PathBuf)> = None;
        for wire in open_wire_paths_from_proc(proc_root, self.agent_pid) {
            let trusted_wire = std::fs::canonicalize(&wire).unwrap_or_else(|_| wire.clone());
            if !trusted_wire.starts_with(&trusted_root) {
                continue;
            }
            let activity = wire
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .and_then(|session_dir| session_dir.to_str())
                .and_then(session_activity_mtime)
                .or_else(|| modified_at(&wire))
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if newest.as_ref().map_or(true, |(seen, _)| activity > *seen) {
                newest = Some((activity, wire));
            }
        }
        let wire = newest.map(|(_, wire)| wire)?;
        let session_dir = wire.parent()?.parent()?.parent()?;
        let agent_session_id = session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned);
        Some(LocatedStatusSource {
            static_transcript_hint: wire.to_str().map(str::to_owned),
            status_path: wire,
            trust_root: home.to_path_buf(),
            agent_session_id,
            resolved_directory: None,
        })
    }

    /// Resolve the best `session_index.jsonl` entry whose `workDir` matches
    /// `cwd` (equal, or a component-boundary ancestor — kimi normalizes a
    /// worktree / subdirectory cwd to the repo root), binding it
    /// unconditionally. Among all matching entries the winner is the LONGEST
    /// `workDir` (most specific / deepest ancestor beats a shallower one),
    /// then by process ownership when the platform exposes a start time. When
    /// process ownership is unavailable, same-`workDir` ties use newest on-disk
    /// session activity rather than raw append order; kimi resume can keep
    /// writing to an earlier index row after appending a later idle row.
    ///
    /// No freshness gate runs here: the authoritative newest-index match
    /// must never be rejected just because it is idle (the bug that left
    /// the agent-status panel blank for a fresh-but-idle session). The
    /// codex-pass-3 stale-race guard is preserved only on the bucket-scan
    /// fallback (`try_resolve_fallback`), which has no append ordering to
    /// distinguish current from stale. Returns `None` when the chosen
    /// entry's `wire.jsonl` does not exist yet, so the caller retries / falls
    /// through rather than binding a path that has no transcript.
    fn try_resolve_from_index(&self, home: &Path, cwd: &Path) -> Option<LocatedStatusSource> {
        let raw = std::fs::read_to_string(self.session_index_path(home)).ok()?;

        // Gather all cwd-matching entries, then choose: PREFER a session this
        // kimi process owns — created at/after the process started (codex's
        // per-process discriminator, resolved via the session's metadata
        // `created_at`) — so a frozen same-cwd session left by an EARLIER run
        // can't win the tie-break. When the process start is unknown, nothing
        // is "owned", so selection falls back to newest activity among
        // matches.
        let process_start = self.process_start();
        let mut matches: Vec<IndexCandidate> = Vec::new();
        for (index_order, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(line) else {
                continue;
            };
            let Some(work_dir) = entry.work_dir.as_deref() else {
                continue;
            };
            if !cwd_matches_workdir(cwd, work_dir) {
                continue;
            }
            let work_len = work_dir.len();
            // Only consider entries whose sessionDir canonically resolves under
            // the kimi home — an index row with a missing, `..`/symlinked, or
            // otherwise untrusted sessionDir must not win selection on macOS
            // (where the process-start discriminator is unavailable) or steer
            // reads outside the trusted home.
            let Some(session_dir) = entry
                .session_dir
                .clone()
                .filter(|dir| path_under(dir, home))
            else {
                continue;
            };
            let created = if process_start.is_some() {
                session_created_at(&session_dir)
            } else {
                None
            };
            let activity = if process_start.is_none() {
                session_activity_mtime(&session_dir)
            } else {
                None
            };
            matches.push(IndexCandidate {
                work_len,
                created,
                activity,
                index_order,
                session_dir,
                entry,
            });
        }

        // Per-process discriminator: when the process start is KNOWN, choose
        // the closest evidence in the narrow ownership window. A new session
        // provides wire `created_at`; a resumed session keeps its old creation
        // time but emits a startup `session resume` diagnostic. The latter
        // still works when Kimi closes wire.jsonl after each flush and proc-fd
        // is empty. Without a platform process start, retain newest activity
        // ranking.
        let (entry, session_dir) = match process_start {
            Some(start) => matches
                .into_iter()
                .filter_map(|candidate| {
                    let owned_at = candidate
                        .created
                        .filter(|created| created_in_own_window(*created, start.at))
                        .or_else(|| self.session_resume_at(&candidate.session_dir, start))?;
                    Some((
                        candidate.work_len,
                        owned_at,
                        candidate.session_dir,
                        candidate.entry,
                    ))
                })
                .min_by_key(|(work_len, owned_at, _, _)| {
                    (abs_duration(*owned_at, start.at), usize::MAX - *work_len)
                })
                .map(|(_, _, session_dir, entry)| (entry, session_dir)),
            None => matches
                .into_iter()
                .max_by_key(|candidate| {
                    (
                        candidate.work_len,
                        candidate.activity.unwrap_or(SystemTime::UNIX_EPOCH),
                        candidate.index_order,
                    )
                })
                .map(|candidate| (candidate.entry, candidate.session_dir)),
        }?;
        let status_path = PathBuf::from(&session_dir)
            .join("agents")
            .join("main")
            .join("wire.jsonl");

        // Bind only when the transcript exists; a missing wire is the
        // fresh-attach race, so the caller retries / falls through.
        if !status_path.exists() {
            return None;
        }

        Some(LocatedStatusSource {
            static_transcript_hint: status_path.to_str().map(str::to_owned),
            status_path,
            trust_root: home.to_path_buf(),
            agent_session_id: entry.session_id,
            resolved_directory: None,
        })
    }

    /// Best-effort fallback when the index has no fresh matching
    /// `workDir`: newest `session_*` (by mtime) whose
    /// `agents/main/wire.jsonl` is FRESH, scoped to this cwd's EXACT bucket
    /// `wd_<basename>_<hex>` (`<hex>` = `sha256(cwd)[:12]`). The exact
    /// bucket — not a basename prefix — stops a same-basename session
    /// from another project's cwd binding here.
    fn try_resolve_fallback(&self, home: &Path, cwd: &Path) -> Option<LocatedStatusSource> {
        let bucket_path = home.join("sessions").join(cwd_bucket_name(cwd)?);
        if !bucket_path.is_dir() {
            return None;
        }
        let process_start = self.process_start();
        let mut newest: Option<(SystemTime, PathBuf)> = None;

        let sessions = std::fs::read_dir(&bucket_path).ok()?;
        for session in sessions.flatten() {
            let name = session.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with("session_") {
                continue;
            }
            let session_path = session.path();
            let wire = session_path.join("agents").join("main").join("wire.jsonl");
            // Same per-process discriminator as the index path: when the
            // process start is known, only a session this process created may
            // bind; otherwise fall back to the pty-start freshness gate so a
            // stale same-cwd bucket session can't bind.
            let fresh = match process_start {
                Some(start) => session_path
                    .to_str()
                    .and_then(session_created_at)
                    .is_some_and(|created| created_in_own_window(created, start.at)),
                None => wire_is_fresh(&wire, self.pty_start),
            };
            if !fresh {
                continue;
            }
            let mtime = session
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if newest.as_ref().map_or(true, |(seen, _)| mtime > *seen) {
                newest = Some((mtime, session_path));
            }
        }

        let (_, session_path) = newest?;
        let status_path = session_path.join("agents").join("main").join("wire.jsonl");
        let agent_session_id = session_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned);
        Some(LocatedStatusSource {
            static_transcript_hint: status_path.to_str().map(str::to_owned),
            status_path,
            trust_root: home.to_path_buf(),
            agent_session_id,
            resolved_directory: None,
        })
    }

    fn session_resume_at(
        &self,
        session_dir: &str,
        process_start: ProcessStartEvidence,
    ) -> Option<SystemTime> {
        if let Some(cached) = self
            .resume_evidence
            .lock()
            .expect("resume_evidence lock")
            .get(session_dir)
            .copied()
        {
            return Some(cached);
        }

        let resumed = session_resume_at(session_dir, process_start)?;
        self.resume_evidence
            .lock()
            .expect("resume_evidence lock")
            .insert(session_dir.to_string(), resumed);
        Some(resumed)
    }
}

impl ProcessStartEvidence {
    fn from_second_granularity(at: SystemTime) -> Self {
        Self {
            at,
            resume_floor: at.checked_sub(Duration::from_secs(1)).unwrap_or(at),
        }
    }
}

impl StatusSourceLocator for KimiLocator {
    fn locate(&self, cwd: &Path, _session_id: &str) -> Result<LocatedStatusSource, String> {
        let home = self.effective_home();
        kdbg(&format!(
            "LOCATE start: passed_cwd={} agent_pid={} proc_root={:?} home={}",
            cwd.display(),
            self.agent_pid,
            self.proc_root,
            home.display()
        ));
        // The PTY-supplied `cwd` can be the stale spawn-time cwd (OSC 7 `cd`
        // updates never reach PtyState). Prefer the kimi process's real
        // `/proc/<pid>/cwd`, since kimi derives its session workDir from there.
        let cwd = self.process_cwd(cwd);
        *self.resolved_cwd.lock().expect("resolved_cwd lock") = Some(cwd.clone());
        kdbg(&format!("LOCATE process_cwd={}", cwd.display()));

        // proc-fd is authoritative and unambiguous — try it first.
        let proc_fd = self.try_resolve_from_proc_fds(&home);
        kdbg(&format!(
            "LOCATE proc_fd={:?}",
            proc_fd
                .as_ref()
                .map(|l| l.status_path.display().to_string())
        ));
        if let Some(located) = proc_fd {
            kdbg(&format!(
                "LOCATE => OK status_path={} sid={:?}",
                located.status_path.display(),
                located.agent_session_id
            ));
            return Ok(self.remember(located));
        }
        // Then the newest same-cwd index match (binds even when idle).
        let index = self.try_resolve_from_index(&home, &cwd);
        kdbg(&format!(
            "LOCATE index={:?}",
            index.as_ref().map(|l| l.status_path.display().to_string())
        ));
        if let Some(located) = index {
            kdbg(&format!(
                "LOCATE => OK status_path={} sid={:?}",
                located.status_path.display(),
                located.agent_session_id
            ));
            return Ok(self.remember(located));
        }

        // Neither proc-fd nor a fresh index match — best-effort fallback.
        if let Some(located) = self.try_resolve_fallback(&home, &cwd) {
            kdbg(&format!(
                "LOCATE => OK status_path={} sid={:?}",
                located.status_path.display(),
                located.agent_session_id
            ));
            return Ok(self.remember(located));
        }

        let err = format!(
            "kimi locator: no fresh session for cwd={} (pid={}) and no fallback session under {}",
            cwd.display(),
            self.agent_pid,
            home.join("sessions").display(),
        );
        kdbg(&format!("LOCATE => ERR {}", err));
        Err(err)
    }
}

/// System boot time (epoch seconds) from `<proc_root>/stat`'s `btime` line.
fn read_btime(proc_root: &Path) -> Option<u64> {
    std::fs::read_to_string(proc_root.join("stat"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.trim().parse().ok())
}

/// Kernel tick rate (`sysconf(_SC_CLK_TCK)`, 100 on typical Linux). `libc`
/// is a Unix-only dependency; non-Unix has no `/proc` (so `process_start`
/// already returns `None`) and just takes the 100 fallback.
fn clock_ticks_per_sec() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: _SC_CLK_TCK takes no pointers and only returns a kernel
        // constant (or -1 on unknown constant); the return value is checked.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if hz > 0 {
            return hz as u64;
        }
    }
    100
}

/// True when `candidate` canonically resolves under `root` (both sides
/// canonicalized so `..` / symlinks can't escape). Refuses metadata reads
/// from an index `sessionDir` outside the trusted kimi home.
fn path_under(candidate: &str, root: &Path) -> bool {
    let (Ok(candidate), Ok(root)) = (
        std::fs::canonicalize(candidate),
        std::fs::canonicalize(root),
    ) else {
        return false;
    };
    candidate.starts_with(root)
}

/// True when `created` falls in the window a session created by a process
/// started at `start` would: from a touch before (clock skew) through
/// `KIMI_OWN_WINDOW` after (creation lags the fork).
fn created_in_own_window(created: SystemTime, start: SystemTime) -> bool {
    let lower = start
        .checked_sub(KIMI_INDEX_FRESHNESS_SLACK)
        .unwrap_or(start);
    created >= lower && created <= start + KIMI_OWN_WINDOW
}

fn resumed_in_own_window(resumed: SystemTime, start: ProcessStartEvidence) -> bool {
    resumed >= start.resume_floor && resumed <= start.at + KIMI_OWN_WINDOW
}

/// Absolute distance between two instants, regardless of order.
fn abs_duration(a: SystemTime, b: SystemTime) -> Duration {
    a.duration_since(b)
        .unwrap_or_else(|_| b.duration_since(a).unwrap_or_default())
}

/// Convert `ps -o etime=` (`[[dd-]hh:]mm:ss`) into a wall-clock start time.
fn process_start_from_elapsed(raw: &str, now: SystemTime) -> Option<SystemTime> {
    let (days, time) = raw
        .split_once('-')
        .map(|(days, time)| (days.parse::<u64>().ok(), time))
        .unwrap_or((Some(0), raw));
    let days = days?;
    let parts = time
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] => (0, *minutes, *seconds),
        [hours, minutes, seconds] => (*hours, *minutes, *seconds),
        _ => return None,
    };
    if hours >= 24 || minutes >= 60 || seconds >= 60 {
        return None;
    }
    let elapsed = days
        .checked_mul(24)?
        .checked_add(hours)?
        .checked_mul(60)?
        .checked_add(minutes)?
        .checked_mul(60)?
        .checked_add(seconds)?;
    now.checked_sub(Duration::from_secs(elapsed))
}

/// A session's creation time, read from the first `metadata` event in its
/// `agents/main/wire.jsonl` (`created_at`, epoch ms). `None` when absent —
/// the caller then treats the session as not provably owned by this process.
///
/// This helper stops after the first metadata line and uses buffered line I/O
/// so attach latency does not scale with the full transcript size.
fn session_created_at(session_dir: &str) -> Option<SystemTime> {
    let wire = PathBuf::from(session_dir)
        .join("agents")
        .join("main")
        .join("wire.jsonl");
    let file = std::fs::File::open(wire).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("metadata") {
            let ms = value
                .get("created_at")
                .and_then(serde_json::Value::as_u64)?;
            return Some(SystemTime::UNIX_EPOCH + Duration::from_millis(ms));
        }
    }
    None
}

/// Find the `session resume` diagnostic emitted for this process startup.
/// Resumed sessions retain their original wire creation timestamp, while Kimi
/// opens the wire only for each flush, so this log entry is the immediate
/// ownership signal when proc-fd is empty.
fn session_resume_at(session_dir: &str, process_start: ProcessStartEvidence) -> Option<SystemTime> {
    let log = PathBuf::from(session_dir)
        .join("logs")
        .join("kimi-code.log");
    let mut file = std::fs::File::open(log).ok()?;
    let len = file.metadata().ok()?.len();
    let tail_start = len.saturating_sub(KIMI_RESUME_LOG_TAIL_BYTES);
    if tail_start > 0 {
        file.seek(SeekFrom::Start(tail_start)).ok()?;
    }

    let mut reader = BufReader::new(file);
    if tail_start > 0 {
        let mut partial = Vec::new();
        reader.read_until(b'\n', &mut partial).ok()?;
    }

    reader
        .lines()
        .map_while(Result::ok)
        .filter(|line| line.contains(" session resume "))
        .filter_map(|line| {
            let raw = line.split_whitespace().next()?;
            let timestamp = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
            let millis = u64::try_from(timestamp.timestamp_millis()).ok()?;
            let resumed = SystemTime::UNIX_EPOCH + Duration::from_millis(millis);
            resumed_in_own_window(resumed, process_start).then_some(resumed)
        })
        .min_by_key(|resumed| abs_duration(*resumed, process_start.at))
}

/// Best-effort activity timestamp for macOS/no-proc index tie-breaking.
/// Looks at `state.json` and every `agents/*/wire.jsonl`; a resumed kimi
/// session keeps updating these files even when a later index row is idle.
fn session_activity_mtime(session_dir: &str) -> Option<SystemTime> {
    let session_dir = PathBuf::from(session_dir);
    let mut newest = modified_at(&session_dir.join("state.json"));

    if let Ok(agent_dirs) = std::fs::read_dir(session_dir.join("agents")) {
        for agent_dir in agent_dirs.flatten() {
            if let Some(mtime) = modified_at(&agent_dir.path().join("wire.jsonl")) {
                if newest.map_or(true, |seen| mtime > seen) {
                    newest = Some(mtime);
                }
            }
        }
    }

    newest
}

fn modified_at(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Read `<proc_root>/<pid>/fd/*` and return symlink targets that look like
/// kimi main-session wires: they end with `agents/main/wire.jsonl` and live
/// under a `.../sessions/wd_*/session_*/` path.
fn open_wire_paths_from_proc(proc_root: &Path, pid: u32) -> Vec<PathBuf> {
    let fd_dir = proc_root.join(pid.to_string()).join("fd");
    let Ok(entries) = std::fs::read_dir(fd_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .filter(|target| is_kimi_session_wire(target))
        .collect()
}

/// True when `path` ends with `agents/main/wire.jsonl` and sits under a
/// `.../sessions/wd_*/session_*/` layout — the shape a kimi session's
/// open transcript fd resolves to.
fn is_kimi_session_wire(path: &Path) -> bool {
    if !path.ends_with(Path::new("agents/main/wire.jsonl")) {
        return false;
    }
    let Some(session_dir) = path.parent().and_then(Path::parent).and_then(Path::parent) else {
        return false;
    };
    let session_named = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("session_"));
    let wd_named = session_dir
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("wd_"));
    let under_sessions = session_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .is_some_and(|n| n == "sessions");
    session_named && wd_named && under_sessions
}

/// Extract a non-empty `KIMI_CODE_HOME` from the kimi process's environ
/// (`<proc_root>/<pid>/environ`, NUL-separated `KEY=VALUE`).
fn kimi_home_from_proc_environ(proc_root: &Path, pid: u32) -> Option<PathBuf> {
    let path = proc_root.join(pid.to_string()).join("environ");
    let content = std::fs::read(path).ok()?;
    for entry in content.split(|&b| b == 0) {
        let entry = String::from_utf8_lossy(entry);
        if let Some(value) = entry.strip_prefix("KIMI_CODE_HOME=") {
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

/// True when `wire` exists and its mtime is `>= pty_start - slack`. A
/// missing file (fresh-attach race) or a too-old mtime (stale same-cwd
/// entry) both return false, so the caller retries / falls through.
fn wire_is_fresh(wire: &Path, pty_start: SystemTime) -> bool {
    let Ok(mtime) = std::fs::metadata(wire).and_then(|m| m.modified()) else {
        return false;
    };
    let floor = pty_start
        .checked_sub(KIMI_INDEX_FRESHNESS_SLACK)
        .unwrap_or(pty_start);
    mtime >= floor
}

/// True when an index `workDir` matches the attach `cwd` by path-component
/// semantics: `cwd == workDir` OR `workDir` is a component-boundary ancestor
/// of `cwd` (kimi normalizes a worktree — and any project subdirectory — cwd
/// to the repo root, so the registered `workDir` is the ancestor of the pane
/// cwd). Canonicalizes both sides (resolves symlinks / `..`); falls back to
/// the raw paths if either side fails to canonicalize. A `starts_with` over
/// `Path` components — not a string prefix — so `/proj` does NOT match
/// `/proj-other` (only a real `/` boundary counts).
fn cwd_matches_workdir(cwd: &Path, work_dir: &str) -> bool {
    let work_path = Path::new(work_dir);
    let canonical_cwd = canonical_or_owned(cwd);
    let canonical_work = canonical_or_owned(work_path);
    canonical_cwd.starts_with(&canonical_work)
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// kimi's exact session-bucket name for a cwd: `wd_<basename>_<hex>` where
/// `<hex>` is the first 12 hex chars of `sha256(cwd)`. Mirrors how kimi-code
/// names `<kimi_home>/sessions/` buckets.
fn cwd_bucket_name(cwd: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let basename = cwd.file_name()?.to_str()?;
    let digest = Sha256::digest(cwd.to_string_lossy().as_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    Some(format!("wd_{basename}_{hex}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    fn write_wire(session_dir: &Path) -> PathBuf {
        let wire = session_dir.join("agents").join("main").join("wire.jsonl");
        std::fs::create_dir_all(wire.parent().expect("parent")).expect("mkdir wire");
        std::fs::write(&wire, b"{\"type\":\"metadata\"}\n").expect("write wire");
        wire
    }

    // Like `write_wire` but stamps the session's creation time into the
    // `metadata` event so the per-process discriminator can read it.
    fn write_wire_created(session_dir: &Path, created_ms: u64) -> PathBuf {
        let wire = session_dir.join("agents").join("main").join("wire.jsonl");
        std::fs::create_dir_all(wire.parent().expect("parent")).expect("mkdir wire");
        std::fs::write(
            &wire,
            format!("{{\"type\":\"metadata\",\"created_at\":{created_ms}}}\n"),
        )
        .expect("write wire");
        wire
    }

    // Fake `<proc_root>/stat` carrying a `btime` line.
    fn write_proc_btime(proc_root: &Path, btime: u64) {
        std::fs::write(proc_root.join("stat"), format!("cpu 0 0\nbtime {btime}\n"))
            .expect("write proc stat");
    }

    // Fake `<proc_root>/<pid>/stat` whose field 22 is `starttime_ticks`
    // (comm parenthesized, 17 filler fields before starttime).
    fn write_proc_stat(proc_root: &Path, pid: u32, starttime_ticks: u64) {
        let pid_dir = proc_root.join(pid.to_string());
        std::fs::create_dir_all(&pid_dir).expect("pid dir");
        let filler = "0 ".repeat(17);
        std::fs::write(
            pid_dir.join("stat"),
            format!("{pid} (kimi) S 1 {filler}{starttime_ticks}\n"),
        )
        .expect("write pid stat");
    }

    fn write_index(kimi_home: &Path, entries: &[(&str, &Path, &Path)]) {
        let path = kimi_home.join("session_index.jsonl");
        let mut file = std::fs::File::create(&path).expect("create index");
        for (session_id, session_dir, work_dir) in entries {
            writeln!(
                file,
                r#"{{"sessionId":"{}","sessionDir":"{}","workDir":"{}"}}"#,
                session_id,
                session_dir.display(),
                work_dir.display(),
            )
            .expect("write index line");
        }
    }

    // A kimi-shaped session dir under <home>/sessions/<wd>/<session>.
    fn session_under(home: &Path, wd: &str, session: &str) -> PathBuf {
        home.join("sessions").join(wd).join(session)
    }

    // Symlink <proc_root>/<pid>/fd/<fd> → target (so proc-fd resolves it).
    fn write_fd(proc_root: &Path, pid: u32, fd: &str, target: &Path) {
        let fd_dir = proc_root.join(pid.to_string()).join("fd");
        std::fs::create_dir_all(&fd_dir).expect("create fake fd dir");
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, fd_dir.join(fd)).expect("symlink fd");
    }

    // Write <proc_root>/<pid>/environ from NUL-joined KEY=VALUE entries.
    fn write_environ(proc_root: &Path, pid: u32, entries: &[&str]) {
        let pid_dir = proc_root.join(pid.to_string());
        std::fs::create_dir_all(&pid_dir).expect("create fake pid dir");
        let mut bytes = Vec::new();
        for entry in entries {
            bytes.extend_from_slice(entry.as_bytes());
            bytes.push(0);
        }
        std::fs::write(pid_dir.join("environ"), bytes).expect("write environ");
    }

    // Force a file's mtime via std `File::set_modified` (no `filetime` dep).
    fn set_mtime(path: &Path, when: SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime");
        file.set_modified(when).expect("set mtime");
    }

    fn locator_with(kimi_home: &Path, pid: u32, pty_start: SystemTime) -> KimiLocator {
        KimiLocator::new(kimi_home.to_path_buf(), pid, pty_start, None)
    }

    fn locator_with_proc(
        kimi_home: &Path,
        pid: u32,
        pty_start: SystemTime,
        proc_root: &Path,
    ) -> KimiLocator {
        KimiLocator::new(
            kimi_home.to_path_buf(),
            pid,
            pty_start,
            Some(proc_root.to_path_buf()),
        )
    }

    #[test]
    fn elapsed_process_time_resolves_start_time() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(400_000);

        assert_eq!(
            process_start_from_elapsed("01:02", now),
            Some(now - Duration::from_secs(62))
        );
        assert_eq!(
            process_start_from_elapsed("02:03:04", now),
            Some(now - Duration::from_secs(7_384))
        );
        assert_eq!(
            process_start_from_elapsed("3-04:05:06", now),
            Some(now - Duration::from_secs(273_906))
        );
        assert_eq!(process_start_from_elapsed("not-a-duration", now), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_process_start_reads_live_proc() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let locator = locator_with_proc(
            kimi_home.path(),
            std::process::id(),
            SystemTime::now(),
            Path::new("/proc"),
        );

        let started = locator.process_start().expect("live process start");
        assert!(started.at <= SystemTime::now());
    }

    #[test]
    fn process_start_is_cached_after_first_resolution() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let proc_root = tempfile::tempdir().expect("proc root");
        let pid = 4242u32;
        let hz = clock_ticks_per_sec();
        let btime = 1_700_000_000u64;
        write_proc_btime(proc_root.path(), btime);
        write_proc_stat(proc_root.path(), pid, 50 * hz);

        let locator = locator_with_proc(kimi_home.path(), pid, SystemTime::now(), proc_root.path());
        let first = locator.process_start().expect("first start");
        write_proc_stat(proc_root.path(), pid, 90 * hz);
        let second = locator.process_start().expect("cached start");

        assert_eq!(second.at, first.at);
        assert_eq!(second.resume_floor, first.resume_floor);
    }

    #[test]
    fn process_start_miss_is_retryable() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let proc_root = tempfile::tempdir().expect("proc root");
        let pid = 4242u32;
        let hz = clock_ticks_per_sec();
        let btime = 1_700_000_000u64;
        write_proc_btime(proc_root.path(), btime);

        let locator = locator_with_proc(kimi_home.path(), pid, SystemTime::now(), proc_root.path());
        assert!(
            locator.process_start().is_none(),
            "missing proc stat should not resolve process start"
        );

        write_proc_stat(proc_root.path(), pid, 50 * hz);
        let retried = locator.process_start().expect("retried process start");

        assert_eq!(
            retried.at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(btime + 50)
        );
        assert_eq!(retried.resume_floor, retried.at);
    }

    #[test]
    fn resolves_status_path_and_session_id_from_index() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let session_dir = session_under(kimi_home.path(), "wd_x", "session_abc");
        let wire = write_wire(&session_dir);
        write_index(
            kimi_home.path(),
            &[("session_abc", &session_dir, work.path())],
        );

        let locator = locator_with(kimi_home.path(), 4242, SystemTime::now());
        let located = locator.locate(work.path(), "pty-1").expect("locate ok");

        assert_eq!(
            located.status_path, wire,
            "status_path points at agents/main/wire.jsonl"
        );
        assert_eq!(located.trust_root, kimi_home.path());
        assert_eq!(located.agent_session_id.as_deref(), Some("session_abc"));
        assert_eq!(located.static_transcript_hint.as_deref(), wire.to_str());
    }

    #[test]
    fn takes_last_matching_workdir_entry() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let old_dir = session_under(kimi_home.path(), "wd_x", "session_old");
        let new_dir = session_under(kimi_home.path(), "wd_x", "session_new");
        write_wire(&old_dir);
        let new_wire = write_wire(&new_dir);
        write_index(
            kimi_home.path(),
            &[
                ("session_old", &old_dir, work.path()),
                ("session_new", &new_dir, work.path()),
            ],
        );

        let locator = locator_with(kimi_home.path(), 4242, SystemTime::now());
        let located = locator.locate(work.path(), "pty-1").expect("locate ok");
        assert_eq!(located.status_path, new_wire);
        assert_eq!(located.agent_session_id.as_deref(), Some("session_new"));
    }

    /// proc-fd is authoritative: even when `session_index.jsonl` has a
    /// DIFFERENT (stale) same-cwd entry, the wire.jsonl the kimi process
    /// holds open wins. Proves P1 (stale same-cwd session) is fixed.
    #[test]
    fn proc_fd_wins_over_stale_same_cwd_index_entry() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let proc_root = tempfile::tempdir().expect("proc root");
        let work = tempfile::tempdir().expect("work dir");

        // The fresh session this process actually holds open.
        let fresh_dir = session_under(kimi_home.path(), "wd_a", "session_abc");
        let fresh_wire = write_wire(&fresh_dir);

        // A stale same-cwd index entry pointing at a DIFFERENT session.
        let stale_dir = session_under(kimi_home.path(), "wd_a", "session_stale");
        write_wire(&stale_dir);
        write_index(
            kimi_home.path(),
            &[("session_stale", &stale_dir, work.path())],
        );

        let pid = 7777;
        write_fd(proc_root.path(), pid, "3", &fresh_wire);

        let locator = locator_with_proc(kimi_home.path(), pid, SystemTime::now(), proc_root.path());
        let located = locator
            .locate(work.path(), "pty-1")
            .expect("proc-fd resolves");

        assert_eq!(
            located.status_path, fresh_wire,
            "proc-fd must bind the process's own session, not the stale index entry"
        );
        assert_eq!(located.agent_session_id.as_deref(), Some("session_abc"));
        assert_eq!(located.trust_root, kimi_home.path());
    }

    #[test]
    fn proc_fd_prefers_newest_active_main_wire_when_multiple_are_open() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let proc_root = tempfile::tempdir().expect("proc root");
        let work = tempfile::tempdir().expect("work dir");
        let pid = 7777;

        let stale_dir = session_under(kimi_home.path(), "wd_a", "session_stale");
        let stale_wire = write_wire(&stale_dir);
        let active_dir = session_under(kimi_home.path(), "wd_a", "session_active");
        let active_wire = write_wire(&active_dir);
        let now = SystemTime::now();
        set_mtime(&stale_wire, now - Duration::from_secs(60));
        set_mtime(&active_wire, now + Duration::from_secs(1));
        write_fd(proc_root.path(), pid, "3", &stale_wire);
        write_fd(proc_root.path(), pid, "4", &active_wire);

        let locator = locator_with_proc(kimi_home.path(), pid, now, proc_root.path());
        let located = locator
            .locate(work.path(), "pty-1")
            .expect("proc-fd resolves");
        assert_eq!(
            located.status_path, active_wire,
            "proc-fd must not treat fd directory order as session freshness"
        );
        assert_eq!(located.agent_session_id.as_deref(), Some("session_active"));
    }

    /// proc-environ P2: the kimi process launched with
    /// `KIMI_CODE_HOME=<tmp>` writes state under `<tmp>`; the locator
    /// reads that env from `<proc>/<pid>/environ` even though its
    /// constructor `kimi_home` points elsewhere.
    #[test]
    fn proc_environ_resolves_per_process_home() {
        let env_home = tempfile::tempdir().expect("env home");
        let wrong_home = tempfile::tempdir().expect("constructor home");
        let proc_root = tempfile::tempdir().expect("proc root");
        let work = tempfile::tempdir().expect("work dir");

        let session_dir = session_under(env_home.path(), "wd_e", "session_env");
        let wire = write_wire(&session_dir);
        write_index(
            env_home.path(),
            &[("session_env", &session_dir, work.path())],
        );

        let pid = 8888;
        write_environ(
            proc_root.path(),
            pid,
            &[&format!("KIMI_CODE_HOME={}", env_home.path().display())],
        );

        // Constructor home is the WRONG dir; only proc-environ points to env_home.
        let locator =
            locator_with_proc(wrong_home.path(), pid, SystemTime::now(), proc_root.path());
        let located = locator
            .locate(work.path(), "pty-1")
            .expect("proc-environ home resolves");

        assert_eq!(located.status_path, wire);
        assert!(
            located.status_path.starts_with(env_home.path()),
            "status_path must resolve under the proc-environ KIMI_CODE_HOME"
        );
        assert_eq!(located.trust_root, env_home.path());
    }

    /// macOS/no-proc regression: kimi can resume an earlier same-cwd session
    /// and then append a later config-only index row. With no `/proc` fd or
    /// process-start discriminator, the active session's newer on-disk
    /// activity must beat the later idle index row so context/cache metrics
    /// keep flowing from the transcript that is actually receiving usage.
    #[test]
    fn no_proc_index_prefers_active_resumed_entry_over_later_idle_row() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");

        let resumed_dir = session_under(kimi_home.path(), "wd_a", "session_resumed");
        let resumed_wire = write_wire(&resumed_dir);
        let idle_dir = session_under(kimi_home.path(), "wd_a", "session_idle");
        let idle_wire = write_wire(&idle_dir);

        let pty_start = SystemTime::now();
        set_mtime(&idle_wire, pty_start - Duration::from_secs(30));
        set_mtime(&resumed_wire, pty_start + Duration::from_secs(1));

        write_index(
            kimi_home.path(),
            &[
                ("session_resumed", &resumed_dir, work.path()),
                ("session_idle", &idle_dir, work.path()),
            ],
        );

        let locator = locator_with(kimi_home.path(), 4242, pty_start);
        let located = locator
            .locate(work.path(), "pty-1")
            .expect("active resumed index entry resolves");
        assert_eq!(
            located.status_path, resumed_wire,
            "active resumed session must beat a later idle index row on macOS"
        );
        assert_eq!(located.agent_session_id.as_deref(), Some("session_resumed"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_index_binds_session_owned_by_live_process() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let locator = locator_with(kimi_home.path(), std::process::id(), SystemTime::now());
        let process_start = locator.process_start().expect("macOS process start");
        let process_start_ms = u64::try_from(
            process_start
                .at
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("process started after epoch")
                .as_millis(),
        )
        .expect("process start fits u64");

        let ours_dir = session_under(kimi_home.path(), "wd_a", "session_ours");
        let ours_wire = write_wire_created(&ours_dir, process_start_ms);
        let other_dir = session_under(kimi_home.path(), "wd_a", "session_other");
        write_wire_created(&other_dir, process_start_ms + 10_000);
        write_index(
            kimi_home.path(),
            &[
                ("session_ours", &ours_dir, work.path()),
                ("session_other", &other_dir, work.path()),
            ],
        );

        let located = locator.locate(work.path(), "pty-1").expect("ours resolves");
        assert_eq!(located.status_path, ours_wire);
        assert_eq!(located.agent_session_id.as_deref(), Some("session_ours"));
    }

    /// Linux regression for Kimi 0.27: resumed sessions keep their original
    /// wire `metadata.created_at`, and Kimi opens `wire.jsonl` only while
    /// flushing. The startup `session resume` diagnostic is therefore the
    /// immediate per-process evidence available when proc-fd is empty.
    #[test]
    fn proc_index_binds_resumed_session_from_startup_log() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let proc_root = tempfile::tempdir().expect("proc root");

        let pid = 4242u32;
        let hz = clock_ticks_per_sec();
        let btime = 1_700_000_000u64;
        let process_start_ms = (btime + 50) * 1000;
        write_proc_btime(proc_root.path(), btime);
        write_proc_stat(proc_root.path(), pid, 50 * hz);

        let resumed_dir = session_under(kimi_home.path(), "wd_a", "session_resumed");
        let resumed_wire = write_wire_created(&resumed_dir, process_start_ms - 3_600_000);
        let log = resumed_dir.join("logs").join("kimi-code.log");
        std::fs::create_dir_all(log.parent().expect("log parent")).expect("mkdir log");
        std::fs::write(
            log,
            b"2023-11-14T22:14:10.000Z INFO  session resume  app_version=0.27.0\n",
        )
        .expect("write session log");

        // A second pane creates another same-cwd session 10 seconds later.
        // Its new wire must not outrank this process's closer resume event.
        let other_dir = session_under(kimi_home.path(), "wd_a", "session_other");
        write_wire_created(&other_dir, process_start_ms + 10_000);
        write_index(
            kimi_home.path(),
            &[
                ("session_resumed", &resumed_dir, work.path()),
                ("session_other", &other_dir, work.path()),
            ],
        );

        let locator = locator_with_proc(kimi_home.path(), pid, SystemTime::now(), proc_root.path());
        let located = locator
            .locate(work.path(), "pty-1")
            .expect("resumed session resolves");
        assert_eq!(located.status_path, resumed_wire);
        assert_eq!(located.agent_session_id.as_deref(), Some("session_resumed"));
    }

    #[test]
    fn proc_index_rejects_prior_process_resume_log_entry() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let proc_root = tempfile::tempdir().expect("proc root");

        let pid = 4242u32;
        let hz = clock_ticks_per_sec();
        let btime = 1_700_000_000u64;
        let process_start_ms = (btime + 50) * 1000;
        write_proc_btime(proc_root.path(), btime);
        write_proc_stat(proc_root.path(), pid, 50 * hz);

        let stale_dir = session_under(kimi_home.path(), "wd_a", "session_stale");
        write_wire_created(&stale_dir, process_start_ms - 3_600_000);
        let log = stale_dir.join("logs").join("kimi-code.log");
        std::fs::create_dir_all(log.parent().expect("log parent")).expect("mkdir log");
        std::fs::write(
            log,
            b"2023-11-14T22:14:08.500Z INFO  session resume  app_version=0.27.0\n",
        )
        .expect("write stale session log");
        write_index(
            kimi_home.path(),
            &[("session_stale", &stale_dir, work.path())],
        );

        let locator = locator_with_proc(kimi_home.path(), pid, SystemTime::now(), proc_root.path());
        assert!(
            locator.locate(work.path(), "pty-1").is_err(),
            "a prior process's resume diagnostic in the clock-skew slack must not bind"
        );
    }

    #[test]
    fn second_granularity_process_start_accepts_subsecond_resume_evidence() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let session_dir = session_under(kimi_home.path(), "wd_a", "session_resumed");
        write_wire_created(&session_dir, 1_700_000_000_000);
        let log = session_dir.join("logs").join("kimi-code.log");
        std::fs::create_dir_all(log.parent().expect("log parent")).expect("mkdir log");
        std::fs::write(
            log,
            b"2023-11-14T22:13:19.250Z INFO  session resume  app_version=0.27.0\n",
        )
        .expect("write session log");

        let estimated_start = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000);
        let process_start = ProcessStartEvidence::from_second_granularity(estimated_start);
        let resumed = session_resume_at(session_dir.to_str().expect("session dir"), process_start)
            .expect("subsecond resume evidence resolves");

        assert_eq!(
            resumed,
            SystemTime::UNIX_EPOCH + Duration::from_millis(1_699_999_999_250)
        );
    }

    #[test]
    fn proc_index_finds_resume_from_bounded_log_tail() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let proc_root = tempfile::tempdir().expect("proc root");

        let pid = 4242u32;
        let hz = clock_ticks_per_sec();
        let btime = 1_700_000_000u64;
        let process_start_ms = (btime + 50) * 1000;
        write_proc_btime(proc_root.path(), btime);
        write_proc_stat(proc_root.path(), pid, 50 * hz);

        let resumed_dir = session_under(kimi_home.path(), "wd_a", "session_resumed");
        let resumed_wire = write_wire_created(&resumed_dir, process_start_ms - 3_600_000);
        let log = resumed_dir.join("logs").join("kimi-code.log");
        std::fs::create_dir_all(log.parent().expect("log parent")).expect("mkdir log");
        let old_prefix =
            "2023-11-14T22:14:08.500Z INFO  session resume  app_version=0.27.0\n".repeat(1200);
        std::fs::write(
            log,
            format!(
                "{old_prefix}2023-11-14T22:14:10.000Z INFO  session resume  app_version=0.27.0\n",
            ),
        )
        .expect("write long session log");
        write_index(
            kimi_home.path(),
            &[("session_resumed", &resumed_dir, work.path())],
        );

        let locator = locator_with_proc(kimi_home.path(), pid, SystemTime::now(), proc_root.path());
        let located = locator
            .locate(work.path(), "pty-1")
            .expect("tail resume evidence resolves");
        assert_eq!(located.status_path, resumed_wire);
        assert_eq!(located.agent_session_id.as_deref(), Some("session_resumed"));
    }

    /// Regression pin for the blank-agent-status bug: an IDLE session whose
    /// `wire.jsonl` mtime is well before `pty_start` (the user opened kimi
    /// but never submitted a prompt) and which is the sole/newest index
    /// entry for the cwd MUST bind, so the status watcher attaches and the
    /// config-only model + context window stream to the panel.
    #[test]
    fn idle_sole_index_entry_binds() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");

        let idle_dir = session_under(kimi_home.path(), "wd_a", "session_idle");
        let idle_wire = write_wire(&idle_dir);
        let pty_start = SystemTime::now();
        // Idle: wire created long before the PTY clock read, never updated.
        set_mtime(&idle_wire, pty_start - Duration::from_secs(3600));
        write_index(
            kimi_home.path(),
            &[("session_idle", &idle_dir, work.path())],
        );

        let locator = locator_with(kimi_home.path(), 4242, pty_start);
        let located = locator
            .locate(work.path(), "pty-1")
            .expect("idle sole index entry must bind");
        assert_eq!(located.status_path, idle_wire);
        assert_eq!(located.agent_session_id.as_deref(), Some("session_idle"));
    }

    /// The "5-minutes-ago" bug / codex P2: two same-`workDir` sessions where
    /// the FROZEN earlier run is listed last. With the live process's start
    /// time known, the locator must bind the session THIS process created
    /// (its `metadata.created_at` is at/after the process start), not the
    /// last index row.
    #[test]
    fn index_binds_session_owned_by_the_live_process() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let proc_root = tempfile::tempdir().expect("proc root");

        let pid = 4242u32;
        let hz = clock_ticks_per_sec();
        let btime = 1_700_000_000u64;
        let starttime_ticks = 50 * hz;
        // process_start = btime + 50s.
        let process_start_ms = (btime + 50) * 1000;
        write_proc_btime(proc_root.path(), btime);
        write_proc_stat(proc_root.path(), pid, starttime_ticks);

        // Owned: created exactly at process start. Stale: created 50s before
        // it (well outside the freshness slack) yet listed LAST in the index.
        let owned_dir = session_under(kimi_home.path(), "wd_a", "session_owned");
        let owned_wire = write_wire_created(&owned_dir, process_start_ms);
        let stale_dir = session_under(kimi_home.path(), "wd_a", "session_stale");
        write_wire_created(&stale_dir, process_start_ms - 50_000);

        write_index(
            kimi_home.path(),
            &[
                ("session_owned", &owned_dir, work.path()),
                ("session_stale", &stale_dir, work.path()),
            ],
        );

        let locator = locator_with_proc(kimi_home.path(), pid, SystemTime::now(), proc_root.path());
        let located = locator
            .locate(work.path(), "pty-1")
            .expect("owned session resolves");
        assert_eq!(
            located.status_path, owned_wire,
            "must bind the session this process created, not the last index row"
        );
        assert_eq!(located.agent_session_id.as_deref(), Some("session_owned"));
    }

    /// codex P2: two sessions share a cwd. The locator (whose process started
    /// at T) must bind the session created CLOSEST to T — its own run — not
    /// the other pane's later session, even though that one is listed last.
    #[test]
    fn closest_created_session_wins_for_concurrent_same_cwd() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let proc_root = tempfile::tempdir().expect("proc root");

        let pid = 4242u32;
        let hz = clock_ticks_per_sec();
        let btime = 1_700_000_000u64;
        let process_start_ms = (btime + 50) * 1000;
        write_proc_btime(proc_root.path(), btime);
        write_proc_stat(proc_root.path(), pid, 50 * hz);

        let ours_dir = session_under(kimi_home.path(), "wd_a", "session_ours");
        let ours_wire = write_wire_created(&ours_dir, process_start_ms);
        let other_dir = session_under(kimi_home.path(), "wd_a", "session_other");
        write_wire_created(&other_dir, process_start_ms + 10_000);

        write_index(
            kimi_home.path(),
            &[
                ("session_ours", &ours_dir, work.path()),
                ("session_other", &other_dir, work.path()),
            ],
        );

        let locator = locator_with_proc(kimi_home.path(), pid, SystemTime::now(), proc_root.path());
        let located = locator.locate(work.path(), "pty-1").expect("ours resolves");
        assert_eq!(located.status_path, ours_wire);
        assert_eq!(located.agent_session_id.as_deref(), Some("session_ours"));
    }

    /// codex follow-up: with the process start known but the live session's
    /// row/wire not landed yet, every same-cwd row is unowned — the resolver
    /// must NOT latch a previous run's stale row; it returns None so locate
    /// retries instead of binding the wrong transcript.
    #[test]
    fn proc_known_without_owned_session_does_not_bind_stale() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let proc_root = tempfile::tempdir().expect("proc root");

        let pid = 4242u32;
        let hz = clock_ticks_per_sec();
        let btime = 1_700_000_000u64;
        let process_start_ms = (btime + 50) * 1000;
        write_proc_btime(proc_root.path(), btime);
        write_proc_stat(proc_root.path(), pid, 50 * hz);

        // Only a stale session exists (created an hour before the process),
        // and it lives under `wd_a` — NOT this cwd's real bucket — so the
        // fallback can't pick it up either.
        let stale_dir = session_under(kimi_home.path(), "wd_a", "session_stale");
        write_wire_created(&stale_dir, process_start_ms - 3_600_000);
        write_index(
            kimi_home.path(),
            &[("session_stale", &stale_dir, work.path())],
        );

        let locator = locator_with_proc(kimi_home.path(), pid, SystemTime::now(), proc_root.path());
        assert!(
            locator.locate(work.path(), "pty-1").is_err(),
            "must not bind a session created before this process started"
        );
    }

    /// Recompute kimi's exact bucket name for a cwd in the test (same
    /// `sha256(cwd)[:12]` the locator derives), so the fixture builds the
    /// bucket the fallback will actually scan.
    fn bucket_for(cwd: &Path) -> String {
        use sha2::{Digest, Sha256};
        let basename = cwd.file_name().and_then(|n| n.to_str()).expect("basename");
        let digest = Sha256::digest(cwd.to_string_lossy().as_bytes());
        let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        format!("wd_{basename}_{hex}")
    }

    /// `sha256("/home/will/projects/vimeflow")[:12]` sanity anchor.
    #[test]
    fn cwd_bucket_name_hex_matches_known_sha() {
        let name = cwd_bucket_name(Path::new("/home/will/projects/vimeflow")).expect("bucket");
        assert_eq!(name, "wd_vimeflow_6650d9cdb25d");
    }

    #[test]
    fn fallback_is_scoped_to_cwd_bucket() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let other_work = tempfile::tempdir().expect("other work");
        let base = work
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .expect("work basename");

        // Session in THIS cwd's EXACT bucket (wd_<basename>_<sha256[:12]>) → must win.
        let scoped = kimi_home
            .path()
            .join("sessions")
            .join(bucket_for(work.path()))
            .join("session_scoped");
        let scoped_wire = write_wire(&scoped);

        // Newer decoy: SAME basename, DIFFERENT hash → must be ignored.
        let decoy = kimi_home
            .path()
            .join("sessions")
            .join(format!("wd_{base}_ffffffffffff"))
            .join("session_decoy");
        write_wire(&decoy);

        // Index entry is for a different workDir, so the index never matches.
        write_index(
            kimi_home.path(),
            &[("session_scoped", &scoped, other_work.path())],
        );

        let locator = locator_with(kimi_home.path(), 4242, SystemTime::now());
        let located = locator
            .locate(work.path(), "pty-1")
            .expect("scoped fallback resolves");
        assert_eq!(
            located.status_path, scoped_wire,
            "fallback must pick the session in this cwd's bucket, not the newer decoy"
        );
        assert_eq!(located.agent_session_id.as_deref(), Some("session_scoped"));
    }

    /// Exact-bucket freshness: a STALE bucket session (wire mtime far
    /// before pty_start) is rejected while a FRESH one wins, even though
    /// the stale dir is newer by directory mtime.
    #[test]
    fn fallback_freshness_prefers_fresh_over_stale_in_bucket() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let bucket = kimi_home
            .path()
            .join("sessions")
            .join(bucket_for(work.path()));

        // FRESH created first, STALE second: the stale dir is newer by mtime,
        // so a naive newest-wins scan would pick it — the gate must skip it.
        let fresh = bucket.join("session_fresh");
        let fresh_wire = write_wire(&fresh);
        let stale = bucket.join("session_stale");
        let stale_wire = write_wire(&stale);

        let pty_start = SystemTime::now();
        set_mtime(&stale_wire, pty_start - Duration::from_secs(3600));
        set_mtime(&fresh_wire, pty_start + Duration::from_secs(1));

        // Index never matches this cwd so resolution falls to the bucket scan.
        write_index(kimi_home.path(), &[]);

        let locator = locator_with(kimi_home.path(), 4242, pty_start);
        let located = locator
            .locate(work.path(), "pty-1")
            .expect("fresh bucket session resolves");
        assert_eq!(
            located.status_path, fresh_wire,
            "fallback must reject the stale bucket session and bind the fresh one"
        );
        assert_eq!(located.agent_session_id.as_deref(), Some("session_fresh"));
    }

    /// A stale-only EXACT bucket (the single session's wire predates
    /// pty_start) must error rather than bind the stale transcript.
    #[test]
    fn fallback_stale_only_bucket_errors() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let bucket = kimi_home
            .path()
            .join("sessions")
            .join(bucket_for(work.path()));
        let stale = bucket.join("session_stale");
        let stale_wire = write_wire(&stale);

        let pty_start = SystemTime::now();
        set_mtime(&stale_wire, pty_start - Duration::from_secs(3600));
        write_index(kimi_home.path(), &[]);

        let locator = locator_with(kimi_home.path(), 4242, pty_start);
        let err = locator
            .locate(work.path(), "pty-1")
            .expect_err("stale-only bucket must not bind");
        assert!(err.contains("kimi locator"), "got: {}", err);
    }

    #[test]
    fn errors_when_no_index_and_no_sessions() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let locator = locator_with(kimi_home.path(), 4242, SystemTime::now());
        let err = locator
            .locate(work.path(), "pty-1")
            .expect_err("empty kimi home should error");
        assert!(err.contains("kimi locator"), "got: {}", err);
    }

    /// `cwd_matches_workdir` component-boundary semantics: equal binds;
    /// an ancestor binds; a sibling that shares a string prefix but no path
    /// boundary (`/proj` vs `/proj-other`) does NOT; an unrelated path does
    /// NOT. Uses real on-disk dirs so canonicalize succeeds on both sides.
    #[test]
    fn cwd_matches_workdir_component_boundary_semantics() {
        let root = tempfile::tempdir().expect("root");
        let proj = root.path().join("proj");
        let nested = proj.join("worktrees").join("wt");
        let sibling = root.path().join("proj-other");
        let unrelated = root.path().join("elsewhere");
        for dir in [&nested, &sibling, &unrelated] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }

        let proj_str = proj.to_string_lossy().into_owned();

        // Equal: workDir == cwd.
        assert!(cwd_matches_workdir(&proj, &proj_str));
        // Ancestor: cwd is under workDir at a real boundary.
        assert!(cwd_matches_workdir(&nested, &proj_str));
        // Sibling sharing a string prefix but no path boundary: must NOT match.
        assert!(!cwd_matches_workdir(&sibling, &proj_str));
        // Unrelated: must NOT match.
        assert!(!cwd_matches_workdir(&unrelated, &proj_str));
    }

    /// Deeper-vs-shallower precedence: when both a deep `workDir` and its
    /// shallower ancestor match the cwd, the LONGEST (most specific) workDir
    /// wins, so the more specific session binds.
    #[test]
    fn index_prefers_deepest_matching_workdir() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let root = tempfile::tempdir().expect("work root");
        let shallow = root.path().to_path_buf();
        let deep = root.path().join("nested");
        let cwd = deep.join("leaf");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");

        let shallow_dir = session_under(kimi_home.path(), "wd_shallow", "session_shallow");
        let shallow_wire = write_wire(&shallow_dir);
        let deep_dir = session_under(kimi_home.path(), "wd_deep", "session_deep");
        let deep_wire = write_wire(&deep_dir);

        // Deep entry listed FIRST so a last-wins tiebreak can't explain a deep win.
        write_index(
            kimi_home.path(),
            &[
                ("session_deep", &deep_dir, &deep),
                ("session_shallow", &shallow_dir, &shallow),
            ],
        );

        let locator = locator_with(kimi_home.path(), 4242, SystemTime::now());
        let located = locator
            .locate(&cwd, "pty-1")
            .expect("deepest workDir resolves");
        assert_eq!(
            located.status_path, deep_wire,
            "the longest (deepest) matching workDir must win over a shallower ancestor"
        );
        assert_eq!(located.agent_session_id.as_deref(), Some("session_deep"));
        assert_ne!(located.status_path, shallow_wire);
    }

    /// Regression for the worktree-cwd bug: kimi registers a worktree (or any
    /// subdirectory) cwd under its GIT-ROOT `workDir`. An index entry with
    /// `workDir=/proj` MUST bind when locating `cwd=/proj/worktrees/wt`
    /// (component-boundary ancestor match), and a sibling cwd `/proj-other`
    /// (string prefix, no path boundary) MUST NOT bind to it.
    #[test]
    fn index_binds_ancestor_workdir_for_worktree_cwd() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let proj_root = tempfile::tempdir().expect("proj root");
        let proj = proj_root.path().join("proj");
        let worktree = proj.join("worktrees").join("wt");
        let sibling = proj_root.path().join("proj-other");
        std::fs::create_dir_all(&worktree).expect("mkdir worktree");
        std::fs::create_dir_all(&sibling).expect("mkdir sibling");

        let session_dir = session_under(kimi_home.path(), "wd_proj", "session_root");
        let wire = write_wire(&session_dir);
        write_index(kimi_home.path(), &[("session_root", &session_dir, &proj)]);

        // Ancestor match: the worktree cwd binds the repo-root workDir session.
        let locator = locator_with(kimi_home.path(), 4242, SystemTime::now());
        let located = locator
            .locate(&worktree, "pty-1")
            .expect("worktree cwd binds its repo-root ancestor session");
        assert_eq!(located.status_path, wire);
        assert_eq!(located.agent_session_id.as_deref(), Some("session_root"));

        // Boundary: a sibling sharing a string prefix must NOT bind, so the
        // ancestor index never matches and the bucket fallback (absent) errors.
        let sibling_locator = locator_with(kimi_home.path(), 4242, SystemTime::now());
        let err = sibling_locator
            .locate(&sibling, "pty-1")
            .expect_err("sibling cwd must not bind the ancestor workDir");
        assert!(err.contains("kimi locator"), "got: {}", err);
    }

    // Symlink <proc_root>/<pid>/cwd → target (so process_cwd resolves it).
    fn write_proc_cwd(proc_root: &Path, pid: u32, target: &Path) {
        let pid_dir = proc_root.join(pid.to_string());
        std::fs::create_dir_all(&pid_dir).expect("create fake pid dir");
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, pid_dir.join("cwd")).expect("symlink cwd");
    }

    /// Root-cause regression: the PTY supplies the STALE spawn-time cwd, but
    /// the kimi process's real cwd (`/proc/<pid>/cwd`) is the project dir its
    /// session was registered under. `process_cwd` must override the passed
    /// (stale) cwd so the index match binds — otherwise `locate` returns Err
    /// and the agent-status panel stays blank.
    #[test]
    fn process_cwd_overrides_stale_passed_cwd_for_index_match() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let proc_root = tempfile::tempdir().expect("proc root");
        // The kimi process's REAL cwd (where it registered its session).
        let real_cwd = tempfile::tempdir().expect("real cwd");

        let session_dir = session_under(kimi_home.path(), "wd_real", "session_real");
        let wire = write_wire(&session_dir);
        write_index(
            kimi_home.path(),
            &[("session_real", &session_dir, real_cwd.path())],
        );

        let pid = 9191;
        write_proc_cwd(proc_root.path(), pid, real_cwd.path());

        let locator = locator_with_proc(kimi_home.path(), pid, SystemTime::now(), proc_root.path());
        // Passed cwd is STALE/wrong — it does not match the index workDir.
        let located = locator
            .locate(Path::new("/some/unrelated/stale/cwd"), "sid")
            .expect("process cwd from /proc/<pid>/cwd must bind the real session");

        assert_eq!(
            located.status_path, wire,
            "must bind via the process cwd, overriding the stale passed cwd"
        );
        assert_eq!(located.agent_session_id.as_deref(), Some("session_real"));
    }

    /// With `proc_root == None` (macOS, no `/proc`), `process_cwd` cannot read
    /// the link, so it falls back to the passed cwd and the index match uses
    /// that. Pins the documented fallback path.
    #[test]
    fn process_cwd_falls_back_to_passed_cwd_without_proc_root() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let session_dir = session_under(kimi_home.path(), "wd_fb", "session_fb");
        let wire = write_wire(&session_dir);
        write_index(
            kimi_home.path(),
            &[("session_fb", &session_dir, work.path())],
        );

        // proc_root is None → process_cwd returns the passed cwd unchanged.
        let locator = locator_with(kimi_home.path(), 4242, SystemTime::now());
        let located = locator
            .locate(work.path(), "sid")
            .expect("passed cwd binds when proc_root is None");
        assert_eq!(located.status_path, wire);
        assert_eq!(located.agent_session_id.as_deref(), Some("session_fb"));
    }

    /// macOS path: with `proc_root == None`, the proc fast-paths skip and
    /// resolution still binds via the (fresh) index match.
    #[test]
    fn no_proc_root_falls_through_to_index() {
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let work = tempfile::tempdir().expect("work dir");
        let session_dir = session_under(kimi_home.path(), "wd_x", "session_mac");
        let wire = write_wire(&session_dir);
        write_index(
            kimi_home.path(),
            &[("session_mac", &session_dir, work.path())],
        );

        let locator = KimiLocator::new(
            kimi_home.path().to_path_buf(),
            4242,
            SystemTime::now() - Duration::from_secs(60),
            None,
        );
        let located = locator.locate(work.path(), "pty-1").expect("index binds");
        assert_eq!(located.status_path, wire);
    }

    /// The fetch-due guard (consent is checked separately): a catch-up fires
    /// when nothing has been fetched yet (`fetched_turn` None), a new turn
    /// re-fetches, the same turn does not, and an in-flight fetch suppresses
    /// overlap.
    #[test]
    fn fetch_due_catches_up_then_debounces_per_turn() {
        // Catch-up: nothing fetched since consent on, even at turn 0.
        let armed = UsageState {
            fetched_turn: None,
            in_flight: false,
            cached: None,
        };
        assert!(armed.fetch_due(0));

        // Same turn already fetched: no re-fetch within the turn.
        let same_turn = UsageState {
            fetched_turn: Some(1),
            in_flight: false,
            cached: None,
        };
        assert!(!same_turn.fetch_due(1));

        // A new turn re-fetches.
        assert!(same_turn.fetch_due(2));

        // A fetch already running: suppress overlap even on a new turn.
        let busy = UsageState {
            fetched_turn: Some(1),
            in_flight: true,
            cached: None,
        };
        assert!(!busy.fetch_due(2));
    }

    /// A revoke hides usage: `cached_rate_limits` returns the cached value only
    /// while consent is ON, and `None` once it is OFF — so `decode` stops
    /// merging stale plan data after the user turns consent off.
    #[test]
    fn cached_rate_limits_hidden_when_consent_off() {
        let _guard = crate::agent::kimi_usage_consent::test_serial_guard();
        let kimi_home = tempfile::tempdir().expect("kimi home");
        let locator = locator_with(kimi_home.path(), 4242, SystemTime::now());
        locator.usage.lock().expect("usage lock").cached = Some(RateLimits {
            five_hour: crate::agent::types::RateLimitInfo {
                used_percentage: 42.0,
                resets_at: 100,
            },
            seven_day: None,
        });

        crate::agent::kimi_usage_consent::set_for_test(true);
        assert!(
            locator.cached_rate_limits().is_some(),
            "consent ON surfaces the cached limits"
        );

        crate::agent::kimi_usage_consent::set_for_test(false);
        assert!(
            locator.cached_rate_limits().is_none(),
            "consent OFF hides the cached limits"
        );

        crate::agent::kimi_usage_consent::set_for_test(false);
    }
}
