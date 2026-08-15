//! Transcript tailer registry used by the watcher runtime.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::agent::adapter::traits::TranscriptStreamer;
use crate::runtime::EventSink;

/// Internal lifecycle type — created by `TranscriptStreamer::tail`
/// implementations (e.g., `claude_code::transcript::start_tailing` via
/// `TranscriptHandle::new`, which is `pub(crate)`) and owned by
/// `TranscriptState`'s internal `TranscriptWatcher` map. The type
/// itself must remain `pub` because it appears in the
/// `TranscriptStreamer::tail` trait signature (and, until D' removes
/// it, the transitional `AgentAdapter::tail_transcript` façade), which
/// is visible from `agent::adapter`. Construction is gated to the
/// crate via `pub(crate) fn new`; do not bypass that path.
#[doc(hidden)]
pub struct TranscriptHandle {
    stop_flag: Arc<AtomicBool>,
    join_handle: Option<std::thread::JoinHandle<()>>,
    aux_stop: Option<Arc<AtomicBool>>,
    aux_join: Option<std::thread::JoinHandle<()>>,
}

impl TranscriptHandle {
    pub(crate) fn new(
        stop_flag: Arc<AtomicBool>,
        join_handle: std::thread::JoinHandle<()>,
    ) -> Self {
        Self {
            stop_flag,
            join_handle: Some(join_handle),
            aux_stop: None,
            aux_join: None,
        }
    }

    /// Attach a sidecar watcher to this handle.
    pub fn attach_aux_join(
        &mut self,
        stop: Arc<AtomicBool>,
        join: std::thread::JoinHandle<()>,
    ) -> Result<(), &'static str> {
        if self.aux_stop.is_some() || self.aux_join.is_some() {
            log::error!("attach_aux_join called twice");
            stop.store(true, Ordering::Release);
            let _ = join.join();

            return Err("attach_aux_join called twice");
        }

        self.aux_stop = Some(stop);
        self.aux_join = Some(join);

        Ok(())
    }

    /// Signal the background thread to stop and wait for it to finish.
    pub fn stop(mut self) {
        // Release pairs with the Acquire load in the tail loop so the
        // stop signal is observed promptly even on weakly-ordered
        // architectures (Claude review on PR #152, F12 — consistency
        // with the F8 fix that already promoted `WatcherHandle`'s
        // stop_flag to Release/Acquire).
        self.stop_flag.store(true, Ordering::Release);
        if let Some(stop) = self.aux_stop.take() {
            stop.store(true, Ordering::Release);
        }
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.aux_join.take() {
            let _ = handle.join();
        }
    }

    /// Signal the background thread(s) to start winding down WITHOUT
    /// joining. The caller can drop the handle later to perform the
    /// join. Used by `stop_with_held_gate` so the per-session gate
    /// only needs to be held long enough to flip the stop flag — the
    /// ~500ms thread-join can then happen outside the gate, reducing
    /// IPC latency for concurrent gate waiters (PR #302 cycle 11 F2).
    pub(crate) fn signal_stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(stop) = &self.aux_stop {
            stop.store(true, Ordering::Release);
        }
    }
}

impl Drop for TranscriptHandle {
    fn drop(&mut self) {
        // See `stop()` above — Release for cross-thread visibility.
        self.stop_flag.store(true, Ordering::Release);
        if let Some(stop) = self.aux_stop.take() {
            stop.store(true, Ordering::Release);
        }
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.aux_join.take() {
            let _ = handle.join();
        }
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptStartStatus {
    Started,
    Replaced,
    AlreadyRunning,
}

/// Typed error returned by [`TranscriptState::start_or_replace`]. The
/// variant is the routing discriminant: `Displaced` is an expected
/// restart-time condition (`debug!` log + `TxOutcome::Displaced`);
/// `Failed` is a genuine spawn failure (`warn!` log + `TxOutcome::
/// StartFailed`).
///
/// PR #302 cycle 16 F1 (Claude post-cycle-15 review): replaced the
/// cycle-13 `DISPLACED_ERR_PREFIX` string-sentinel that
/// `maybe_start_transcript` used via `starts_with` — a typo or i18n
/// edit to the prefix would have silently routed every restart's
/// expected-condition Err into the generic warn arm (false-positive
/// alerts). The enum makes the discriminant structural: the producer
/// constructs the right variant and the consumer pattern-matches on
/// the variant. The wrapped `String` keeps the human-readable message
/// for logs.
#[doc(hidden)]
#[derive(Debug)]
pub enum StartError {
    /// Caller's `WatcherHandle` was displaced between the callback
    /// dispatch and `start_or_replace`'s gate-protected alive check.
    /// Expected normal restart-time condition; consumer should log
    /// at `debug` and emit `TxOutcome::Displaced`.
    Displaced(String),
    /// Spawn of the new tail thread failed (fs error, inotify
    /// exhaustion, streamer adapter returned `Err`). Unexpected;
    /// consumer should log at `warn` and emit
    /// `TxOutcome::StartFailed`.
    Failed(String),
}

impl StartError {
    /// True iff this error is a displaced-watcher short-circuit (the
    /// expected restart-time condition).
    pub(crate) fn is_displaced(&self) -> bool {
        matches!(self, Self::Displaced(_))
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Displaced(s) | Self::Failed(s) => f.write_str(s),
        }
    }
}

impl From<StartError> for String {
    fn from(e: StartError) -> Self {
        e.to_string()
    }
}

/// Typed handle to a per-session start gate. Constructed only via
/// `TranscriptState::session_gate(session_id)` — owns an
/// `Arc<Mutex<()>>` clone of the registered gate and remembers the
/// `session_id` it was issued for. `.lock()` returns a
/// `SessionGateGuard<'_>` that carries the same `session_id`.
///
/// PR #302 cycle 15 F1 (Claude post-cycle-13 review): strengthens
/// cycle-13 F3's compile-time witness from "some `Mutex<()>` is held"
/// to "the per-session start gate for THIS `session_id` is held".
/// The private constructor (only `TranscriptState::session_gate` can
/// build one) closes the "any random `Mutex<()>` works" loophole; the
/// `session_id` stored inside lets `stop_with_held_gate` runtime-check
/// in debug builds that the guard belongs to the session being torn
/// down.
pub(crate) struct SessionGate<'a> {
    session_id: &'a str,
    /// Identity Arc of the issuing `TranscriptState`'s `start_gates`
    /// map. Stored as a cheap clone (atomic refcount bump) for
    /// `Arc::ptr_eq` identity comparison only — never locked or
    /// dereferenced through this clone. Used by `stop_with_held_gate`
    /// to debug-assert the guard was issued by the same
    /// `TranscriptState` instance that's now being mutated, closing
    /// the cycle-15 retry-1 gap codex flagged: "guard from a different
    /// `TranscriptState` with the same session_id would pass the
    /// session_id check but not be holding `self`'s lock."
    start_gates: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    arc: Arc<Mutex<()>>,
}

impl<'a> SessionGate<'a> {
    /// Lock the per-session gate. Returns a `SessionGateGuard<'a>`
    /// whose `session_id()` matches this `SessionGate`'s session_id
    /// and whose owner identity matches the issuing `TranscriptState`.
    pub(crate) fn lock(&'a self) -> SessionGateGuard<'a> {
        SessionGateGuard {
            session_id: self.session_id,
            start_gates: self.start_gates.clone(),
            _guard: self
                .arc
                .lock()
                .expect("failed to lock per-session start gate"),
        }
    }
}

/// Guard proving the per-session start gate for `session_id()` is
/// currently locked. Constructed only by `SessionGate::lock`. The
/// inner `MutexGuard` is private — possessing a `SessionGateGuard`
/// proves both:
///   - SOME `Arc<Mutex<()>>` is locked (compile-time, via the
///     embedded `MutexGuard`'s lifetime), AND
///   - the lock chain went through
///     `TranscriptState::session_gate(session_id).lock()` (compile-
///     time, via the private constructor + private field).
///
/// `stop_with_held_gate` `debug_assert_eq!`s its `session_id` argument
/// against `gate.session_id()` to catch a future contributor passing
/// the wrong session's guard in debug builds. Release builds incur no
/// runtime overhead beyond the underlying `MutexGuard`.
pub(crate) struct SessionGateGuard<'a> {
    session_id: &'a str,
    /// See `SessionGate::start_gates`. Owner identity for
    /// `Arc::ptr_eq` against the operating `TranscriptState`.
    start_gates: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    _guard: std::sync::MutexGuard<'a, ()>,
}

impl SessionGateGuard<'_> {
    /// The `session_id` this guard was issued for.
    pub(crate) fn session_id(&self) -> &str {
        self.session_id
    }
}

struct TranscriptWatcher {
    transcript_path: PathBuf,
    cwd: Option<PathBuf>,
    handle: TranscriptHandle,
}

#[derive(Clone)]
struct TranscriptRecoverySource {
    transcript_path: PathBuf,
    streamer: Arc<dyn TranscriptStreamer>,
}

/// Runtime-managed registry of in-flight transcript tailers, one per
/// session. Constructed once as part of `BackendState` and passed to the
/// watcher runtime and `TranscriptStreamer::tail` impls. `pub` supports
/// direct instantiation in `#[cfg(test)]` integration tests under
/// `crates/backend/tests/transcript_*.rs`; do not construct ad hoc instances
/// in production code paths.
#[doc(hidden)]
#[derive(Default, Clone)]
pub struct TranscriptState {
    watchers: Arc<Mutex<HashMap<String, TranscriptWatcher>>>,
    /// Last successfully tailed, already-validated source for each PTY. Kept
    /// when a watcher stops so pane reactivation can perform a read-only,
    /// nonce-scoped recovery scan without restarting the tailer.
    recovery_sources: Arc<Mutex<HashMap<String, TranscriptRecoverySource>>>,
    /// Per-session "start gate" — held across `tail` so the
    /// notify callback and the 3s poll thread can't both pass the
    /// AlreadyRunning check, both spawn, and both emit duplicate
    /// `agent-tool-call` / `agent-turn` events from byte 0 of the
    /// JSONL before the loser's thread is stopped (Claude review on
    /// PR #152, F2). Different sessions still spawn concurrently
    /// because each session has its own `Arc<Mutex<()>>`.
    start_gates: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl TranscriptState {
    pub fn new() -> Self {
        Self {
            watchers: Arc::new(Mutex::new(HashMap::new())),
            recovery_sources: Arc::new(Mutex::new(HashMap::new())),
            start_gates: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Start tailing when none is active, or switch to a newer transcript
    /// path or workspace cwd.
    ///
    /// Step B'' narrowed the parameter from `Arc<dyn AgentAdapter>` to
    /// `Arc<dyn TranscriptStreamer>` — `base` only ever needed the
    /// `tail` spawn method, never the full adapter façade — and
    /// narrowed visibility to `pub(crate)` (the only callers are the
    /// watcher runtime + same-crate tests). The shared
    /// `Arc<CompositeLocator>` plumbing from B' cycle 11 means
    /// `bindings.streamer` already references the same allocation the
    /// watcher's locator does.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_or_replace(
        &self,
        streamer: Arc<dyn TranscriptStreamer>,
        events: Arc<dyn EventSink>,
        session_id: String,
        transcript_path: PathBuf,
        cwd: Option<PathBuf>,
        claim_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
        // PR #302 cycle 10 — `alive: Option<Arc<AtomicBool>>` is the
        // per-WatcherHandle alive token. Notify and poll callbacks
        // pass `Some(alive.clone())`; inline-init / direct test
        // callers pass `None`. Checked UNDER the per-session gate
        // BEFORE any mutation: if `alive` is `Some(false)`, the
        // caller's handle has been displaced and any claim it would
        // make is stale — return `Err("watcher displaced")` early so
        // no mutation happens and `claim_flag` is NOT set. Closes the
        // cycle-9 residual race where an already-dispatched displaced
        // callback could acquire the gate after `insert` released it
        // and reclaim the entry with stale data (codex-connector P2
        // round 10).
        alive: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<TranscriptStartStatus, StartError> {
        // Acquire (or lazily create) the per-session start gate so only
        // one start_or_replace call per session can be inside the
        // check + spawn + insert critical section at a time. Without
        // this, the notify callback and 3s poll thread can both pass
        // the AlreadyRunning check, both call streamer.tail,
        // and both emit events from byte 0 of the JSONL during the
        // tens-of-ms thread-spawn window before the loser's handle is
        // stopped (Claude review on PR #152, F2).
        let gate = {
            let mut gates = self.start_gates.lock().expect("failed to lock start_gates");
            gates
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _gate_guard = gate.lock().expect("failed to lock per-session start gate");

        // PR #302 cycle 10 — alive check INSIDE the gate, BEFORE any
        // mutation. If the caller's handle has been displaced (the
        // gate-protected window that AgentWatcherState::insert uses to
        // set alive = false), short-circuit with Err — no mutation, no
        // claim flag set. Closes the residual cycle-9 race where an
        // already-dispatched displaced callback could race past the
        // _watcher drop and reclaim the entry.
        //
        // PR #302 cycle 16 F1 (Claude post-cycle-15 review):
        // returns the typed `StartError::Displaced` variant so
        // `maybe_start_transcript` can pattern-match on the
        // discriminant rather than substring-match a string sentinel.
        // The wrapped String is the human-readable message (logs only;
        // not part of the routing contract). Pre-cycle-16 used a
        // `DISPLACED_ERR_PREFIX` string sentinel — a typo or i18n
        // edit to the prefix would have silently routed every
        // restart's expected-condition Err into the generic warn arm
        // (false-positive "Failed to start transcript tailing"
        // alerts). The enum makes the discriminant structural.
        if let Some(alive_flag) = &alive {
            if !alive_flag.load(std::sync::atomic::Ordering::Acquire) {
                return Err(StartError::Displaced(
                    "watcher displaced before start_or_replace gate".to_string(),
                ));
            }
        }

        // PR #302 cycle 9 — close the cycle-8 TOCTOU race. The
        // `claim_flag` is set BEFORE returning so the gate-held write
        // is visible to any later gate-holder (specifically
        // `AgentWatcherState::insert`'s gate-protected read). The
        // pre-cycle-9 code wrote the flag AFTER `maybe_start_transcript`
        // returned — outside the gate — so insert could acquire the
        // gate between `start_or_replace`'s gate release and the
        // caller's flag store, reading a stale `false`. Setting under
        // the gate makes the race structurally impossible.
        //
        // Closure runs at every successful early-return + at function
        // tail. Failure paths (streamer.tail returning `Err`) DO NOT
        // set the flag — start_or_replace removed the old entry but
        // didn't establish a new one, so the new handle hasn't truly
        // claimed anything; the displaced handle should retain
        // ownership.
        let mark_claimed = || {
            if let Some(flag) = &claim_flag {
                flag.store(true, std::sync::atomic::Ordering::Release);
            }
        };

        // Extract any old watcher BEFORE spawning the new tail thread, so
        // the old thread is fully joined before the new one starts emitting
        // events for this session_id. The previous order (spawn → insert
        // (capturing old) → stop old outside the lock) created a
        // ~POLL_INTERVAL (500 ms) overlap window during which both tail
        // threads were live; on the cwd-change Replaced path (same
        // transcript_path, different cwd) the new thread replays all events
        // from byte 0 while the old thread still drains its read buffer,
        // producing duplicate `agent-tool-call` and `agent-turn` events that
        // inflate `recentToolCalls` and aggregate counters in the frontend
        // (which has no toolUseId-level dedup). Claude review on PR #152, F19.
        //
        // Lock-ordering remains gate → watchers; the watchers mutex is never
        // held across the blocking `handle.stop()` join. Between extracting
        // and re-inserting the entry the map has no row for this session_id
        // for ~500 ms, but the per-session gate ensures no concurrent
        // `start_or_replace` or `stop()` for this session can observe that
        // gap (both acquire the gate first, F4).
        //
        // Trade-off: if `streamer.tail` fails AFTER the old
        // watcher is stopped, the caller gets the error AND the session is
        // left with no active watcher. Previously (spawn-first order) a
        // tail failure preserved the old watcher. The new
        // behaviour is intentional for the Replaced path: a cwd change
        // means the old cwd is no longer the correct routing context, so a
        // failed swap should fail loudly rather than silently keep a
        // stale-cwd tailer alive.
        let old_handle = {
            let mut watchers = self.watchers.lock().expect("failed to lock watchers");
            if let Some(current) = watchers.get(&session_id) {
                if current.transcript_path == transcript_path && current.cwd == cwd {
                    // AlreadyRunning is a successful claim — the new
                    // handle has adopted the existing entry. Mark the
                    // flag while still holding the gate so the
                    // gate-protected reader sees it.
                    mark_claimed();
                    return Ok(TranscriptStartStatus::AlreadyRunning);
                }
            }
            watchers.remove(&session_id).map(|watcher| watcher.handle)
        };

        let had_old = old_handle.is_some();
        if let Some(handle) = old_handle {
            handle.stop();
        }

        let new_handle = streamer
            .tail(
                events,
                session_id.clone(),
                cwd.clone(),
                transcript_path.clone(),
            )
            .map_err(StartError::Failed)?;

        // The per-session gate guarantees no concurrent `start_or_replace`
        // can have inserted between the check above and this insert.
        // `stop()` also acquires the gate (Claude review on PR #152, F4),
        // so it cannot have removed an entry mid-flight either. Outcome is
        // determined by whether the pre-spawn extract found an entry:
        // present → Replaced (different identity, since the early-return
        // above handled the same-identity case under the same gate);
        // absent → Started.
        {
            let mut watchers = self.watchers.lock().expect("failed to lock watchers");
            watchers.insert(
                session_id.clone(),
                TranscriptWatcher {
                    transcript_path: transcript_path.clone(),
                    cwd,
                    handle: new_handle,
                },
            );
        }
        self.recovery_sources
            .lock()
            .expect("failed to lock recovery_sources")
            .insert(
                session_id,
                TranscriptRecoverySource {
                    transcript_path,
                    streamer,
                },
            );

        // Started / Replaced are both successful claims — the new
        // handle has established the entry. Mark the flag under the
        // still-held gate (cycle 9 TOCTOU close).
        mark_claimed();

        Ok(if had_old {
            TranscriptStartStatus::Replaced
        } else {
            TranscriptStartStatus::Started
        })
    }

    /// Check if a session already has an active transcript watcher.
    #[allow(dead_code)]
    pub fn contains(&self, session_id: &str) -> bool {
        let watchers = self.watchers.lock().expect("failed to lock watchers");
        watchers.contains_key(session_id)
    }

    pub(crate) fn recover_replies(
        &self,
        session_id: &str,
        nonces: &HashSet<String>,
    ) -> Result<Vec<crate::agent::types::AgentReplyEvent>, String> {
        self.recover(session_id, |streamer, path| {
            streamer.recover_replies(session_id, path, nonces)
        })
    }

    pub(crate) fn recover_reviews(
        &self,
        session_id: &str,
        nonces: &HashSet<String>,
    ) -> Result<Vec<crate::agent::types::AgentReviewEvent>, String> {
        self.recover(session_id, |streamer, path| {
            streamer.recover_reviews(session_id, path, nonces)
        })
    }

    fn recover<T>(
        &self,
        session_id: &str,
        scan: impl Fn(&dyn TranscriptStreamer, &std::path::Path) -> Result<Vec<T>, String>,
    ) -> Result<Vec<T>, String> {
        let source = self
            .recovery_sources
            .lock()
            .expect("failed to lock recovery_sources")
            .get(session_id)
            .cloned();
        let Some(source) = source else {
            return Ok(Vec::new());
        };

        scan(source.streamer.as_ref(), &source.transcript_path)
    }

    pub(crate) fn forget_recovery_source(&self, session_id: &str) {
        self.recovery_sources
            .lock()
            .expect("failed to lock recovery_sources")
            .remove(session_id);
    }

    /// Internal: return the per-session start gate's
    /// `Arc<Mutex<()>>` (creating it lazily on first access). Used
    /// directly by `start_or_replace` and `stop` (which already live
    /// in this module) and indirectly by the public `session_gate`
    /// constructor that wraps the result in a typed `SessionGate`.
    fn session_gate_arc(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut gates = self.start_gates.lock().expect("failed to lock start_gates");
        gates
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Return a typed `SessionGate<'a>` bound to `session_id`.
    /// Callers `.lock()` the gate to serialize their critical section
    /// against any in-flight `start_or_replace` or `stop` on the same
    /// session. The resulting `SessionGateGuard<'a>` carries the
    /// `session_id` so gate-aware callees (`stop_with_held_gate`) can
    /// runtime-verify the guard belongs to the session being
    /// operated on.
    ///
    /// Used by `AgentWatcherState::insert` to gate-protect its
    /// claim-flag read + map-mutation + (when the new handle DIDN'T
    /// claim) the under-gate teardown of any orphaned old entry
    /// against a pre-register notify callback's in-flight
    /// `start_or_replace` (PR #302 cycle 9 — closes the cycle-8 TOCTOU
    /// race by making the claim-flag write happen INSIDE
    /// `start_or_replace`'s gate, and insert's read + teardown happen
    /// INSIDE a re-acquisition of the same gate).
    ///
    /// **Lock-ordering invariant**: gate → watchers, matching
    /// `start_or_replace` and `stop`. Callers MUST release the gate
    /// before any operation that could re-acquire it transitively
    /// (specifically: any path that calls `stop` or `start_or_replace`
    /// for the same session). The displaced `WatcherHandle`'s Drop's
    /// `transcript_state.stop` re-acquires the same gate, so insert
    /// uses `stop_with_held_gate` under its own gate and CLEARS the
    /// displaced handle's `owns_transcript` to make the Drop a no-op
    /// for transcript state — no Drop-time gate re-acquisition.
    pub(crate) fn session_gate<'a>(&self, session_id: &'a str) -> SessionGate<'a> {
        SessionGate {
            session_id,
            start_gates: self.start_gates.clone(),
            arc: self.session_gate_arc(session_id),
        }
    }

    /// Same as `stop` but assumes the CALLER already holds the
    /// per-session start gate (via `session_gate(sid).lock()`).
    /// Removes the entry from `watchers` under the watchers lock and
    /// signals the displaced `TranscriptHandle`'s stop flag UNDER the
    /// gate so its tail thread starts winding down immediately.
    /// Returns the displaced handle so the caller can drop it OUTSIDE
    /// the gate — the ~500ms tail-thread join then happens without
    /// holding the gate, so concurrent gate waiters (notify callbacks,
    /// future `start_or_replace` calls) don't stall on IPC for that
    /// duration (PR #302 cycle 9 retry-1 established the under-gate
    /// teardown pattern; cycle 11 F2 split the signal-vs-join so the
    /// join can happen outside the gate). Used by
    /// `AgentWatcherState::insert` to tear down an orphaned old
    /// transcript entry.
    ///
    /// **Caller must hold the gate** for the duration of any work
    /// that relies on the entry being absent (e.g., the new handle's
    /// claim-decision or further state mutations).
    ///
    /// **Race window vs the join-outside-gate decision:** between
    /// the gate release and the OLD tail thread actually observing
    /// the stop flag (at most one POLL_INTERVAL ≈ 500ms later), a
    /// concurrent `start_or_replace` can acquire the gate and spawn
    /// a fresh tail for the same session. Briefly there are two
    /// threads emitting events; the OLD thread exits at its next
    /// stop-flag check (within ≤ one poll iteration), so the overlap
    /// is bounded to a couple of duplicate events at most. The
    /// frontend has no per-tool-call dedup; brief duplicates are an
    /// acceptable cost vs holding the gate for the full join.
    pub(crate) fn stop_with_held_gate(
        &self,
        session_id: &str,
        // PR #302 cycle 15 F1 (Claude post-cycle-13 review):
        // strengthened from cycle-13's `&MutexGuard<'_, ()>` to
        // `&SessionGateGuard<'_>`. The cycle-13 witness only proved
        // that SOME `Mutex<()>` was held; a future contributor could
        // pass any unrelated `MutexGuard<()>` and bypass the intended
        // serialization guarantee. The `SessionGateGuard` newtype
        // closes both layers:
        //   - Compile-time: the only public constructor is
        //     `SessionGate::lock`, and `SessionGate` itself is only
        //     built by `TranscriptState::session_gate(...)`. Future
        //     contributors physically cannot pass a `Mutex<()>` from
        //     a different subsystem.
        //   - Debug-runtime: the `debug_assert_eq!` below catches a
        //     contributor who holds the gate for the wrong session
        //     (e.g., `ts.session_gate(other_id).lock()` then calls
        //     `stop_with_held_gate(this_id, ...)`). Release builds
        //     incur no overhead.
        //
        // Closes the cycle-13 F3 "only proves no-gate-held, not
        // right-gate-held" gap from Claude's post-cycle-13 review.
        gate: &SessionGateGuard<'_>,
    ) -> Option<TranscriptHandle> {
        // PR #302 cycle 15 retry-1 (codex verify): also check the
        // guard belongs to THIS `TranscriptState` instance. The
        // session_id-only assert below would pass for a guard issued
        // by some OTHER `TranscriptState` with the same session_id —
        // and that other state's lock provides no serialization on
        // `self.watchers`. The `Arc::ptr_eq` on `start_gates` is the
        // identity test (atomic refcount bump on construction; no
        // dereference here). Closes codex's MEDIUM finding (0.86
        // conf) flagged on cycle 15.
        debug_assert!(
            Arc::ptr_eq(&gate.start_gates, &self.start_gates),
            "stop_with_held_gate: gate belongs to a different TranscriptState instance; \
             caller is holding the wrong state's gate",
        );
        debug_assert_eq!(
            gate.session_id(),
            session_id,
            "stop_with_held_gate: gate.session_id() ({}) does not match the session_id argument ({}); \
             caller is holding the wrong session's gate",
            gate.session_id(),
            session_id,
        );
        let removed = {
            let mut watchers = self.watchers.lock().expect("failed to lock watchers");
            watchers.remove(session_id)
        };
        // Signal stop_flag IMMEDIATELY under the gate so the tail
        // thread starts winding down without waiting for the caller's
        // later drop. Caller drops the returned handle outside the
        // gate, which then performs the actual thread-join.
        if let Some(w) = &removed {
            w.handle.signal_stop();
        }
        removed.map(|w| w.handle)
    }

    /// Stop tailing for the given session.
    pub fn stop(&self, session_id: &str) -> Result<(), String> {
        // Acquire the per-session start gate before touching `watchers`
        // (Claude review on PR #152, F4). Without this, an in-flight
        // `start_or_replace` could be between its drop-watchers-lock /
        // tail-spawn / re-acquire-watchers steps when stop()
        // ran concurrently and removed the entry. `start_or_replace`
        // would then insert the freshly-spawned T1 as `Started` even
        // though `stop()` already ran — leaving T1 as a zombie tail
        // thread with no owning `WatcherHandle` (the original handle
        // whose Drop called us has already been dropped). Acquiring
        // the gate here forces stop() to wait for any in-flight start
        // to finish, so the entry stop() removes is exactly the entry
        // start_or_replace just inserted — there is no zombie.
        //
        // Lock ordering: gate → watchers, matching `start_or_replace`.
        //
        // Intentionally do NOT remove this session's entry from
        // `start_gates`. If we deleted the gate after releasing it, a
        // subsequent `start_or_replace` would lookup the empty map
        // slot, create a NEW gate, and enter `tail`
        // concurrently with another already-in-flight start that still
        // holds a clone of the OLD gate. Gates are ~56 bytes each
        // (`String` key + `Arc<Mutex<()>>` value); leaving them for
        // the session_id's lifetime is small enough that periodic
        // cleanup isn't worth the lock-ordering complexity.
        let gate = {
            let mut gates = self.start_gates.lock().expect("failed to lock start_gates");
            gates
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _gate_guard = gate.lock().expect("failed to lock per-session start gate");

        let handle = {
            let mut watchers = self.watchers.lock().expect("failed to lock watchers");
            watchers.remove(session_id)
        };
        match handle {
            Some(watcher) => {
                watcher.handle.stop();
                Ok(())
            }
            None => Err(format!("No transcript watcher for session: {}", session_id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::FakeEventSink;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn spawn_loop(stop: Arc<AtomicBool>, counter: Arc<AtomicUsize>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                counter.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    }

    #[test]
    fn transcript_state_contains_empty() {
        let state = TranscriptState::new();
        assert!(!state.contains("any-session"));
    }

    #[test]
    fn recovery_source_survives_stop_without_restarting_the_tailer() {
        struct RecoveringStreamer {
            tail_calls: Arc<AtomicUsize>,
            reply_calls: Arc<AtomicUsize>,
            review_calls: Arc<AtomicUsize>,
        }

        impl TranscriptStreamer for RecoveringStreamer {
            fn tail(
                &self,
                _events: Arc<dyn EventSink>,
                _session_id: String,
                _cwd: Option<PathBuf>,
                _transcript_path: PathBuf,
            ) -> Result<TranscriptHandle, String> {
                self.tail_calls.fetch_add(1, Ordering::Relaxed);
                let stop = Arc::new(AtomicBool::new(false));
                Ok(TranscriptHandle::new(stop, std::thread::spawn(|| {})))
            }

            fn recover_replies(
                &self,
                _session_id: &str,
                _transcript_path: &std::path::Path,
                _nonces: &std::collections::HashSet<String>,
            ) -> Result<Vec<crate::agent::types::AgentReplyEvent>, String> {
                self.reply_calls.fetch_add(1, Ordering::Relaxed);
                Ok(Vec::new())
            }

            fn recover_reviews(
                &self,
                _session_id: &str,
                _transcript_path: &std::path::Path,
                _nonces: &std::collections::HashSet<String>,
            ) -> Result<Vec<crate::agent::types::AgentReviewEvent>, String> {
                self.review_calls.fetch_add(1, Ordering::Relaxed);
                Ok(Vec::new())
            }
        }

        let tail_calls = Arc::new(AtomicUsize::new(0));
        let reply_calls = Arc::new(AtomicUsize::new(0));
        let review_calls = Arc::new(AtomicUsize::new(0));
        let streamer: Arc<dyn TranscriptStreamer> = Arc::new(RecoveringStreamer {
            tail_calls: tail_calls.clone(),
            reply_calls: reply_calls.clone(),
            review_calls: review_calls.clone(),
        });
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("temp dir");
        let transcript_path = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript_path, "").expect("write transcript");
        let state = TranscriptState::new();

        state
            .start_or_replace(
                streamer,
                sink,
                "pty-1".to_string(),
                transcript_path,
                None,
                None,
                None,
            )
            .expect("start tailer");
        state.stop("pty-1").expect("stop tailer");
        state
            .recover_replies(
                "pty-1",
                &std::collections::HashSet::from(["nonce-1".to_string()]),
            )
            .expect("recover replies");
        state
            .recover_reviews(
                "pty-1",
                &std::collections::HashSet::from(["nonce-1".to_string()]),
            )
            .expect("recover reviews");

        assert_eq!(tail_calls.load(Ordering::Relaxed), 1);
        assert_eq!(reply_calls.load(Ordering::Relaxed), 1);
        assert_eq!(review_calls.load(Ordering::Relaxed), 1);
        assert!(!state.contains("pty-1"));

        state.forget_recovery_source("pty-1");
        state
            .recover_replies(
                "pty-1",
                &std::collections::HashSet::from(["nonce-1".to_string()]),
            )
            .expect("missing source returns empty");
        state
            .recover_reviews(
                "pty-1",
                &std::collections::HashSet::from(["nonce-1".to_string()]),
            )
            .expect("missing source returns empty");
        assert_eq!(reply_calls.load(Ordering::Relaxed), 1);
        assert_eq!(review_calls.load(Ordering::Relaxed), 1);
    }

    /// PR #302 cycle 15 F1 — `SessionGate::lock` returns a
    /// `SessionGateGuard` whose `session_id()` matches the
    /// `session_id` passed to `TranscriptState::session_gate(...)`.
    #[test]
    fn session_gate_guard_carries_its_session_id() {
        let state = TranscriptState::new();
        let sid = "session-witness";
        let gate = state.session_gate(sid);
        let guard = gate.lock();
        assert_eq!(guard.session_id(), sid);
    }

    /// PR #302 cycle 15 F1 — `stop_with_held_gate` debug-asserts the
    /// guard's `session_id` matches its `session_id` argument. Passing
    /// the wrong session's guard panics in debug builds, catching a
    /// future contributor who borrows the convenient nearby guard
    /// instead of the right one.
    ///
    /// `cfg(debug_assertions)`-gated because release builds skip the
    /// assertion (no panic, only an undefined teardown — production
    /// is debug-built for safety; release builds still benefit from
    /// the compile-time guarantee that the guard came from
    /// `TranscriptState::session_gate`).
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "caller is holding the wrong session's gate")]
    fn stop_with_held_gate_panics_on_wrong_session_in_debug() {
        let state = TranscriptState::new();
        let wrong_gate = state.session_gate("other-session");
        let wrong_guard = wrong_gate.lock();
        // Pretend to tear down "target-session" while holding
        // "other-session"'s gate — debug_assert_eq! must fire.
        state.stop_with_held_gate("target-session", &wrong_guard);
    }

    /// PR #302 cycle 15 retry-1 (codex verify) — guard from a
    /// DIFFERENT `TranscriptState` instance with the same session_id
    /// would pass the session_id check but isn't actually holding
    /// `self`'s `start_gates[session_id]` lock. The `Arc::ptr_eq` on
    /// `start_gates` catches this in debug builds.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "caller is holding the wrong state's gate")]
    fn stop_with_held_gate_panics_on_wrong_state_in_debug() {
        let state_a = TranscriptState::new();
        let state_b = TranscriptState::new();
        let sid = "shared-session-id";
        // Acquire the gate on state_b (wrong state) for the same sid.
        let foreign_gate = state_b.session_gate(sid);
        let foreign_guard = foreign_gate.lock();
        // Pretend to tear down sid on state_a while holding state_b's
        // gate — Arc::ptr_eq on start_gates must fire.
        state_a.stop_with_held_gate(sid, &foreign_guard);
    }

    #[test]
    fn transcript_state_replaces_changed_path() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let first_path = tmp.path().join("first.jsonl");
        let second_path = tmp.path().join("second.jsonl");
        std::fs::write(&first_path, "").expect("failed to write first transcript");
        std::fs::write(&second_path, "").expect("failed to write second transcript");

        let state = TranscriptState::new();
        let session_id = "session-1".to_string();
        let adapter: Arc<dyn TranscriptStreamer> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);

        let first_status = state
            .start_or_replace(
                adapter.clone(),
                sink.clone(),
                session_id.clone(),
                first_path.clone(),
                None,
                None,
                None,
            )
            .expect("failed to start first transcript watcher");
        assert_eq!(first_status, TranscriptStartStatus::Started);

        let duplicate_status = state
            .start_or_replace(
                adapter.clone(),
                sink.clone(),
                session_id.clone(),
                first_path,
                None,
                None,
                None,
            )
            .expect("failed to check duplicate transcript watcher");
        assert_eq!(duplicate_status, TranscriptStartStatus::AlreadyRunning);

        let replaced_status = state
            .start_or_replace(
                adapter,
                sink.clone(),
                session_id.clone(),
                second_path,
                None,
                None,
                None,
            )
            .expect("failed to replace transcript watcher");
        assert_eq!(replaced_status, TranscriptStartStatus::Replaced);

        state.stop(&session_id).expect("failed to stop watcher");
    }

    #[test]
    fn transcript_state_threads_cwd_through() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let transcript_path = tmp.path().join("t.jsonl");
        std::fs::write(&transcript_path, "").expect("failed to write transcript");
        let cwd = tmp.path().to_path_buf();

        let state = TranscriptState::new();
        let adapter: Arc<dyn TranscriptStreamer> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);

        let status = state
            .start_or_replace(
                adapter,
                sink.clone(),
                "session-cwd".to_string(),
                transcript_path,
                Some(cwd),
                None,
                None,
            )
            .expect("failed to start watcher with cwd");
        assert_eq!(status, TranscriptStartStatus::Started);

        state.stop("session-cwd").expect("failed to stop watcher");
    }

    #[test]
    fn transcript_state_replaces_when_only_cwd_changes() {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let transcript_path = tmp.path().join("t.jsonl");
        std::fs::write(&transcript_path, "").expect("failed to write transcript");
        let cwd_a = tempfile::tempdir().expect("failed to create cwd_a");
        let cwd_b = tempfile::tempdir().expect("failed to create cwd_b");

        let state = TranscriptState::new();
        let session_id = "session-cwd-change".to_string();
        let adapter: Arc<dyn TranscriptStreamer> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);

        let first = state
            .start_or_replace(
                adapter.clone(),
                sink.clone(),
                session_id.clone(),
                transcript_path.clone(),
                Some(cwd_a.path().to_path_buf()),
                None,
                None,
            )
            .expect("failed to start with cwd_a");
        assert_eq!(first, TranscriptStartStatus::Started);

        let same = state
            .start_or_replace(
                adapter.clone(),
                sink.clone(),
                session_id.clone(),
                transcript_path.clone(),
                Some(cwd_a.path().to_path_buf()),
                None,
                None,
            )
            .expect("failed to detect already-running");
        assert_eq!(same, TranscriptStartStatus::AlreadyRunning);

        let replaced = state
            .start_or_replace(
                adapter.clone(),
                sink.clone(),
                session_id.clone(),
                transcript_path.clone(),
                Some(cwd_b.path().to_path_buf()),
                None,
                None,
            )
            .expect("failed to replace on cwd change");
        assert_eq!(replaced, TranscriptStartStatus::Replaced);

        let replaced_to_none = state
            .start_or_replace(
                adapter,
                sink.clone(),
                session_id.clone(),
                transcript_path,
                None,
                None,
                None,
            )
            .expect("failed to replace on cwd to None transition");
        assert_eq!(replaced_to_none, TranscriptStartStatus::Replaced);

        state.stop(&session_id).expect("failed to stop watcher");
    }

    #[test]
    fn transcript_handle_drop_sets_stop_flag() {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let handle = std::thread::spawn(|| {});

        {
            let _handle = TranscriptHandle::new(Arc::clone(&stop_flag), handle);
        }

        assert!(stop_flag.load(Ordering::Relaxed));
    }

    #[test]
    fn drop_joins_both_threads() {
        let stop_a = Arc::new(AtomicBool::new(false));
        let stop_b = Arc::new(AtomicBool::new(false));
        let counter_a = Arc::new(AtomicUsize::new(0));
        let counter_b = Arc::new(AtomicUsize::new(0));

        let mut handle = TranscriptHandle::new(
            Arc::clone(&stop_a),
            spawn_loop(Arc::clone(&stop_a), Arc::clone(&counter_a)),
        );
        handle
            .attach_aux_join(
                Arc::clone(&stop_b),
                spawn_loop(Arc::clone(&stop_b), Arc::clone(&counter_b)),
            )
            .expect("attach aux join");
        std::thread::sleep(Duration::from_millis(30));
        drop(handle);

        let frozen_a = counter_a.load(Ordering::Relaxed);
        let frozen_b = counter_b.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(counter_a.load(Ordering::Relaxed), frozen_a);
        assert_eq!(counter_b.load(Ordering::Relaxed), frozen_b);
    }

    #[test]
    fn stop_method_flips_both_flags_before_joining() {
        let stop_a = Arc::new(AtomicBool::new(false));
        let stop_b = Arc::new(AtomicBool::new(false));
        let counter_a = Arc::new(AtomicUsize::new(0));
        let counter_b = Arc::new(AtomicUsize::new(0));
        let mut handle = TranscriptHandle::new(
            Arc::clone(&stop_a),
            spawn_loop(Arc::clone(&stop_a), Arc::clone(&counter_a)),
        );

        handle
            .attach_aux_join(
                Arc::clone(&stop_b),
                spawn_loop(Arc::clone(&stop_b), Arc::clone(&counter_b)),
            )
            .expect("attach aux join");
        std::thread::sleep(Duration::from_millis(30));
        handle.stop();

        assert!(stop_a.load(Ordering::Acquire));
        assert!(stop_b.load(Ordering::Acquire));
    }

    #[test]
    fn attach_aux_join_rejects_duplicate_without_panicking() {
        let stop_a = Arc::new(AtomicBool::new(false));
        let stop_b = Arc::new(AtomicBool::new(false));
        let counter_a = Arc::new(AtomicUsize::new(0));
        let counter_b = Arc::new(AtomicUsize::new(0));
        let mut handle = TranscriptHandle::new(
            Arc::clone(&stop_a),
            spawn_loop(Arc::clone(&stop_a), Arc::clone(&counter_a)),
        );

        handle
            .attach_aux_join(
                Arc::clone(&stop_b),
                spawn_loop(Arc::clone(&stop_b), Arc::clone(&counter_b)),
            )
            .expect("attach aux join");

        let duplicate_stop = Arc::new(AtomicBool::new(false));
        let result = handle.attach_aux_join(Arc::clone(&duplicate_stop), std::thread::spawn(|| {}));

        assert!(result.is_err());
        assert!(duplicate_stop.load(Ordering::Acquire));
        handle.stop();
    }

    #[test]
    fn handle_without_aux_still_works() {
        let stop = Arc::new(AtomicBool::new(false));
        let counter = Arc::new(AtomicUsize::new(0));
        let handle = TranscriptHandle::new(
            Arc::clone(&stop),
            spawn_loop(Arc::clone(&stop), Arc::clone(&counter)),
        );

        std::thread::sleep(Duration::from_millis(30));
        drop(handle);

        assert!(stop.load(Ordering::Acquire));
    }

    /// Regression test for F19 — start_or_replace on the cwd-change
    /// Replaced path must fully stop the old tail thread BEFORE spawning
    /// the new one. The pre-fix order was (spawn-new → insert → stop-old),
    /// which left both threads live for ~POLL_INTERVAL (500 ms) and
    /// produced duplicate `agent-tool-call` / `agent-turn` events on the
    /// frontend.
    ///
    /// The invariant is observed via a custom streamer that records the
    /// order of `tail` calls AND the order of stop-flag flips
    /// on the handles it returns. After two `start_or_replace` calls (cwd
    /// A then cwd B on the same transcript_path), the recorded sequence
    /// must be: `spawn(A)`, `stop(A)`, `spawn(B)` — NOT `spawn(A)`,
    /// `spawn(B)`, `stop(A)`.
    ///
    /// Claude review on PR #152, F19. Step B'' retyped the mock from a
    /// full `AgentAdapter` (4 `unreachable!()` stubs + the real
    /// `tail_transcript`) to a lean `TranscriptStreamer` — the only
    /// trait `start_or_replace` now needs.
    #[test]
    fn replace_on_cwd_change_stops_old_before_spawning_new() {
        use std::sync::Mutex;

        struct OrderingStreamer {
            events: Arc<Mutex<Vec<String>>>,
            stop_flags: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
        }

        impl TranscriptStreamer for OrderingStreamer {
            fn tail(
                &self,
                _events: Arc<dyn crate::runtime::EventSink>,
                _session_id: String,
                cwd: Option<PathBuf>,
                _transcript_path: PathBuf,
            ) -> Result<TranscriptHandle, String> {
                let cwd_label = cwd
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<none>".to_string());
                self.events
                    .lock()
                    .expect("events lock")
                    .push(format!("spawn({})", cwd_label));

                let stop_flag = Arc::new(AtomicBool::new(false));
                let stop_clone = Arc::clone(&stop_flag);
                let events_clone = Arc::clone(&self.events);
                let cwd_for_thread = cwd_label.clone();
                self.stop_flags
                    .lock()
                    .expect("stop_flags lock")
                    .push(Arc::clone(&stop_flag));

                // The mock thread polls the stop flag and records when it
                // observes the stop. Real adapters do real I/O here; the
                // poll cadence below is deliberately tight so the test
                // doesn't pad runtime.
                let join_handle = std::thread::spawn(move || {
                    while !stop_clone.load(Ordering::Acquire) {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    events_clone
                        .lock()
                        .expect("events lock in mock thread")
                        .push(format!("stop({})", cwd_for_thread));
                });

                Ok(TranscriptHandle::new(stop_flag, join_handle))
            }
        }

        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let transcript_path = tmp.path().join("t.jsonl");
        std::fs::write(&transcript_path, "").expect("failed to write transcript");
        let cwd_a = tempfile::tempdir().expect("failed to create cwd_a");
        let cwd_b = tempfile::tempdir().expect("failed to create cwd_b");

        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let stop_flags = Arc::new(Mutex::new(Vec::<Arc<AtomicBool>>::new()));
        let adapter: Arc<dyn TranscriptStreamer> = Arc::new(OrderingStreamer {
            events: Arc::clone(&events),
            stop_flags: Arc::clone(&stop_flags),
        });

        let state = TranscriptState::new();
        let session_id = "session-f19".to_string();

        state
            .start_or_replace(
                adapter.clone(),
                sink.clone(),
                session_id.clone(),
                transcript_path.clone(),
                Some(cwd_a.path().to_path_buf()),
                None,
                None,
            )
            .expect("failed to start with cwd_a");

        state
            .start_or_replace(
                adapter,
                sink.clone(),
                session_id.clone(),
                transcript_path,
                Some(cwd_b.path().to_path_buf()),
                None,
                None,
            )
            .expect("failed to replace with cwd_b");

        state.stop(&session_id).expect("failed to stop watcher");

        // After `state.stop()` returns, the second handle's `stop()` has
        // joined the thread, so both `stop(A)` and `stop(B)` events are
        // guaranteed to be in the log (the mock thread pushes the stop
        // event before exiting; `handle.stop()` waits for that exit).
        let recorded = events.lock().expect("events lock").clone();
        // The invariant: stop(A) appears BEFORE spawn(B). Pre-fix order
        // (spawn-then-stop) would have produced spawn(A), spawn(B),
        // stop(A), stop(B). Post-fix order produces spawn(A), stop(A),
        // spawn(B), stop(B).
        let spawn_b_idx = recorded
            .iter()
            .position(|e| e.starts_with("spawn(") && e.contains(cwd_b.path().to_str().unwrap()))
            .expect("spawn(B) must appear in event log");
        let stop_a_idx = recorded
            .iter()
            .position(|e| e.starts_with("stop(") && e.contains(cwd_a.path().to_str().unwrap()))
            .expect("stop(A) must appear in event log");
        assert!(
            stop_a_idx < spawn_b_idx,
            "F19 regression: expected stop(A) before spawn(B); got events {:?}",
            recorded
        );
    }
}
