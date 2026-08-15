//! File watcher runtime for agent status sources.
//!
//! Watches adapter-provided status files for changes and emits backend events
//! when they update. Uses the `notify` crate plus a polling fallback for
//! environments where file-system notifications are unreliable.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use std::time::Instant;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::super::bindings::AgentBindings;
#[cfg(debug_assertions)]
use super::diagnostics::short_sid;
use super::diagnostics::{record_event_diag, EventTiming, PathHistory, TxOutcome};
use super::transcript_state::{TranscriptHandle, TranscriptStartStatus, TranscriptState};
use crate::agent::types::AgentType;
// `TranscriptPathValidator` and `TranscriptStreamer` are referenced as
// `Arc<dyn ...>` in `maybe_start_transcript`'s signature, so both must be
// in scope. `StateDecoder` is consumed only via method dispatch on
// `Arc<dyn StateDecoder>` (vtable), so it does not need to appear here
// (PR #261 cycle 2 review F9). Step B'' added `TranscriptStreamer` here
// when `start_or_replace` migrated off `Arc<dyn AgentAdapter>`.
use super::super::traits::{TranscriptPathValidator, TranscriptStreamer};
use super::TrustedLocatedSource;
use crate::agent::adapter::types::{
    stamp_snapshot, LocatedStatusSource, RawPath, TranscriptPathSource, ValidateTranscriptError,
};
use crate::agent::events::emit_agent_status;
use crate::runtime::EventSink;
use crate::terminal::PtyState;

/// Step 0c: resolve the transcript path by asking the adapter's
/// `TranscriptPathSource` for a dynamic hint (Claude, fresh per
/// update) first, then falling back to the static hint (Codex, fixed
/// at attach time and stored on the [`LocatedStatusSource`]).
///
/// Replaces the former `parsed.transcript_path` side channel on
/// [`crate::agent::adapter::types::ParsedStatus`]. The shape is the
/// same `Option<RawPath>` the previous field carried; the difference
/// is that `TranscriptPathSource` is the single typed origin point
/// the watcher now consults.
fn resolve_transcript_path(
    transcript_paths: &Arc<dyn TranscriptPathSource>,
    raw: &str,
    located: &LocatedStatusSource,
) -> Option<RawPath> {
    transcript_paths
        .dynamic_hint(raw)
        .or_else(|| transcript_paths.static_hint(located))
}

/// Handle to a running watcher — dropping it stops the watcher and polling thread
pub struct WatcherHandle {
    /// The status source (Codex: rollout) path this handle is watching.
    /// Read via `AgentWatcherState::current_status_path` so the relocate
    /// sequence can skip a no-op re-spawn when a fresh locate returns the
    /// same path (the drift tick calls `start_agent_watcher` every few
    /// seconds; re-tailing a 20-114MB rollout on every tick is wasteful).
    status_path: PathBuf,
    _watcher: Option<RecommendedWatcher>,
    /// Signals the polling fallback thread to exit
    poll_stop: Arc<(Mutex<bool>, Condvar)>,
    /// Polling fallback thread. Stored so Drop can join after signalling
    /// stop, rather than leaving the thread briefly detached.
    join_handle: Option<std::thread::JoinHandle<()>>,
    /// Cloned handle to the runtime-managed `TranscriptState` — `Drop`
    /// calls `transcript_state.stop(&self.session_id)` to cascade
    /// transcript-tail teardown, replacing today's two-step
    /// frontend-driven stop courtesy.
    transcript_state: TranscriptState,
    /// Used by the `Drop` cascade AND the debug-build diagnostic log.
    /// (Earlier revisions had a separate `session_id_for_log: String`
    /// gated on `#[cfg(debug_assertions)]` — but the comment claiming
    /// this saved a `String::clone` in release builds was wrong, since
    /// `session_id` itself is always cloned into the struct. Removed
    /// in cycle 6 per Claude review F13.)
    session_id: String,
    /// Which agent runs in this pty session. Stored here so
    /// `AgentWatcherState::agent_type_for_pty` can resolve under the
    /// single `watchers` mutex, instead of a separate `agent_types`
    /// map that would expose an inconsistency window between insert /
    /// remove and the lookup (PR #302 Claude review F2 — the pre-fix
    /// split-mutex design let the rename / title-sync IPC see "agent
    /// type present, watcher absent" and the reverse during the gap
    /// between the two critical sections).
    agent_type: AgentType,
    /// Optional codex title-sync watcher stop flag. `Some` only for
    /// Codex sessions where the locator surfaced an `agent_session_id`;
    /// `None` for Claude / NoOp / Codex-without-thread-id. `Drop`
    /// signals stop before joining `session_index_join` so the
    /// poll-and-emit thread exits in bounded time (PR #302 codex
    /// review F5 — re-wires the title-sync path that the pre-fix
    /// refactor dropped).
    session_index_stop: Option<Arc<AtomicBool>>,
    /// Optional codex title-sync watcher join handle. Paired with
    /// `session_index_stop`; `Drop` joins after signalling stop so the
    /// thread is reaped instead of left detached (matches the existing
    /// `join_handle` pattern for the polling fallback).
    session_index_join: Option<std::thread::JoinHandle<()>>,
    /// Optional Claude semantic-hook stream, bound to this watcher's lifetime.
    /// `WatcherHandle::drop` signals this flag when the session watcher is
    /// removed or replaced, before waiting for the tail thread to exit.
    attention_stop: Option<Arc<AtomicBool>>,
    /// Claude attention tail thread paired with `attention_stop`.
    /// `WatcherHandle::drop` joins it after sending the stop signal so a closed
    /// or replaced Claude session cannot leave a detached file watcher behind.
    attention_join: Option<std::thread::JoinHandle<()>>,
    /// True if this handle owns the per-session entry in
    /// `transcript_state` and should tear it down on Drop. Cleared to
    /// `false` by `AgentWatcherState::insert` ONLY when this handle is
    /// displaced by a NEW handle that reached
    /// `TranscriptState::start_or_replace` at least once before the
    /// `insert` call (signal carried by the new handle's
    /// `claimed_transcript: Arc<AtomicBool>` — set by inline-init AND
    /// the pre-register notify callback). When the new handle never
    /// engaged with `transcript_state`, the old handle retains
    /// ownership and its Drop properly tears down the orphaned old
    /// tail (PR #302 codex review cycle 5 P1 established the
    /// ownership-transfer pattern; cycle 8 P2 narrowed the trigger
    /// from "every restart" to "only when the new handle claimed";
    /// cycle 8 retry-2 widened the claim signal to include
    /// pre-register notify-callback claims).
    owns_transcript: bool,
    /// Per-handle "alive" token. Set to `false` by
    /// `AgentWatcherState::insert` UNDER the per-session gate when
    /// this handle is displaced, BEFORE dropping `_watcher` (the
    /// notify backend) and BEFORE the under-gate orphan teardown.
    /// Notify and poll callbacks pass `Some(alive.clone())` to
    /// `maybe_start_transcript`; `start_or_replace` checks the flag
    /// UNDER the per-session gate and short-circuits with Err if
    /// false — preventing already-dispatched displaced callbacks
    /// from claiming the entry with stale data after the OS-level
    /// `_watcher` disconnect (PR #302 cycle 10 — closes the residual
    /// in-flight-dispatch race codex-connector flagged in round 10).
    ///
    /// Inline-init (synchronous, can't be displaced mid-flight)
    /// passes `None` for alive.
    alive: Arc<AtomicBool>,
    /// `Arc<AtomicBool>` set to `true` by ANY pre-register
    /// `maybe_start_transcript` call that actually reached
    /// `TranscriptState::start_or_replace`. The flag is written
    /// INSIDE `start_or_replace`'s per-session gate (PR #302 cycle 9
    /// — closes the cycle-8 TOCTOU race by serializing the
    /// claim-flag write with `AgentWatcherState::insert`'s
    /// claim-flag read; insert acquires the same gate around its
    /// snapshot, so a concurrent pre-register notify callback's
    /// `start_or_replace` must complete — including its gate-held
    /// flag write — before insert can read).
    ///
    /// Covers both the inline-init path AND the pre-register notify-
    /// callback path: both pass `Some(claimed_transcript.clone())`
    /// down through `maybe_start_transcript` to `start_or_replace`,
    /// which sets the flag from its mark_claimed closure on every
    /// successful outcome (Started / Replaced / AlreadyRunning).
    /// Validation early-exits in `maybe_start_transcript` never
    /// reach `start_or_replace` and so don't set the flag (correct:
    /// no claim made).
    ///
    /// **Lock-ordering invariant** (TranscriptState's start_gate →
    /// watchers): the same `start_or_replace` and `stop` ordering
    /// applies to `AgentWatcherState::insert`'s gate acquisition. See
    /// the comment block in `insert` for the matching Drop-order
    /// invariant (gate guard MUST drop before the displaced handle
    /// drops, since the displaced handle's Drop's
    /// `transcript_state.stop` re-acquires the same gate).
    ///
    /// Read once at `AgentWatcherState::insert` time via `Acquire`
    /// load — after that, later claims by the poll thread / live
    /// notify callbacks don't influence ownership transfer (they
    /// happen long after the displaced handle has been dropped). The
    /// poll thread sleeps ~3s before its first iteration, so it
    /// doesn't race with insert and intentionally does not write to
    /// this flag.
    ///
    /// Field is `Arc`-wrapped (rather than a bare `AtomicBool`) so
    /// the notify-callback closure can capture a clone independently
    /// of the handle's eventual move into `AgentWatcherState`.
    ///
    /// PR #302 cycle 5 P1 introduced unconditional ownership
    /// transfer; cycle 8 P2 narrowed it; cycle 8 retry-2 expanded
    /// the signal from inline-init-only to "any pre-register claim"
    /// after codex verify flagged the notify-callback race.
    claimed_transcript: Arc<AtomicBool>,
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        drop(self._watcher.take());
        // PR #302 cycle 6 — signal BOTH background threads to stop
        // BEFORE joining EITHER, so the two ~500ms sleep budgets race
        // toward exit in parallel instead of accumulating sequentially.
        // Cuts worst-case Drop latency from ~1s (poll-sleep +
        // session_index-sleep) to ~500ms (max of the two). Drop runs
        // outside the watchers mutex, but slimming it reduces stop-IPC
        // and restart latency observed by the caller.
        let (lock, wake) = &*self.poll_stop;
        {
            let mut stopped = lock.lock().expect("failed to lock poll stop flag");
            *stopped = true;
            wake.notify_one();
        }
        // Stop signal for the codex title-sync watcher (if any). Always
        // unconditional: `session_index::spawn_watch` is called
        // per-handle in `start_watching` (not via an idempotent
        // start-or-replace), so the new handle has its own thread and
        // the old thread is a redundant resource that must be reaped
        // (NOT inherited the way transcript-state is). PR #302 cycle 6
        // hoisted this above the `join_handle.join()` below so both
        // threads receive their stop signals before either join blocks.
        if let Some(stop) = self.session_index_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Release);
        }
        // Claude-only attention tail: session removal/replacement drops this
        // WatcherHandle, which stops the per-session tail before it is joined.
        if let Some(stop) = self.attention_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Release);
        }
        // Now join both threads. Order doesn't matter for correctness;
        // total time is bounded by the slower of the two (~500ms).
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.session_index_join.take() {
            let _ = handle.join();
        }
        // Reap the Claude attention tail after the stop signal above.
        if let Some(handle) = self.attention_join.take() {
            let _ = handle.join();
        }
        // Only tear down the transcript-state entry if THIS handle
        // currently owns it. When a new handle for the same session
        // displaces this one via `AgentWatcherState::insert`, the new
        // handle's `start_watching` has already adopted / replaced /
        // started the per-session transcript entry, and `insert` cleared
        // this flag before letting the displaced handle drop — calling
        // `stop` here would otherwise tear down the new tail (PR #302
        // codex review cycle 5 P1). Clean stops via
        // `AgentWatcherState::remove` leave the flag set, so this
        // line runs and the tail is correctly stopped.
        if self.owns_transcript {
            let _ = self.transcript_state.stop(&self.session_id);
        }
        // `#[cfg(...)]` (attribute) on a statement, NOT `if cfg!(...)`
        // (runtime). The attribute physically removes the entire
        // `log::info!(...)` call in release builds, so even the
        // `short_sid` slice into `self.session_id` is compiled away.
        #[cfg(debug_assertions)]
        log::info!(
            "watcher.handle.dropped session={}",
            short_sid(&self.session_id)
        );
    }
}

/// Thread-safe state for managing active agent watchers per session.
///
/// PR #302 Claude review F2 collapsed the previous parallel `agent_types`
/// map into a field on `WatcherHandle`, so `agent_type_for_pty` and
/// `contains` / `active_count` now read from the SAME `watchers` mutex
/// — closing the inconsistency window where the rename / title-sync IPC
/// could observe an agent type without a live watcher (or vice versa)
/// during a concurrent insert / remove.
#[derive(Default, Clone)]
pub struct AgentWatcherState {
    watchers: Arc<Mutex<HashMap<String, WatcherHandle>>>,
}

impl AgentWatcherState {
    /// Create a new empty watcher state
    pub fn new() -> Self {
        Self {
            watchers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Insert a watcher for a session, stopping any existing watcher.
    ///
    /// Stamps the handle's `agent_type` from the caller-supplied value
    /// so the watchers map is the single source of truth for both the
    /// watcher presence AND the agent type — no split-mutex
    /// inconsistency window (PR #302 Claude review F2).
    ///
    /// Scope the lock guard to a nested block so the evicted
    /// `WatcherHandle` (if any) drops AFTER the watchers mutex is
    /// released. `WatcherHandle::Drop` joins the polling thread, which
    /// can sleep up to 3 seconds — holding the mutex across that wait
    /// would block any concurrent `insert` / `remove` / `active_count`
    /// for the same duration. Same fix that was already in
    /// `TranscriptState::stop` (Claude review on PR #152, F7).
    /// Quiesce any existing watcher for `session_id` BEFORE a new
    /// watcher's `start_watching` runs inline-init. Called from
    /// `SessionLifecycle::run_watch_sequence` between
    /// `ensure_trust` and `spawn_watch`.
    ///
    /// **Why this is necessary (PR #302 cycle 16 — codex P2):**
    /// Without quiesce, the OLD handle's notify or poll callback can
    /// fire AFTER the new handle's inline-init has reached
    /// `start_or_replace` and set `NEW.claimed_transcript = true`,
    /// but BEFORE `insert` displaces the OLD handle (i.e., before
    /// `OLD.alive.store(false)`). In that window the OLD callback's
    /// `start_or_replace` passes its alive check (OLD.alive still
    /// `true`), takes the per-session gate, and overwrites the
    /// just-claimed transcript entry with the OLD path/cwd.
    /// `insert` later sees `new_claimed == true` and skips
    /// orphan-teardown, leaving a stale OLD-path tail attached
    /// under the NEW handle.
    ///
    /// The fix is to set `OLD.alive = false` and drop `OLD._watcher`
    /// UNDER the per-session gate BEFORE inline-init runs. After
    /// this returns:
    ///   - OS-level notify backend stops dispatching new callbacks
    ///     from OLD's watcher (`_watcher.take()` → `Drop`).
    ///   - In-flight OLD callbacks already past dispatch will,
    ///     when they reach `start_or_replace`'s gate-protected
    ///     alive check, observe `OLD.alive == false` and
    ///     short-circuit with `StartError::Displaced` — no
    ///     mutation, transcript entry unaffected.
    ///   - New handle's inline-init re-acquires the gate cleanly
    ///     and races nothing.
    ///
    /// **Lock-ordering invariant:** gate → watchers, matching
    /// `start_or_replace`, `stop`, and `insert`. Caller MUST NOT
    /// hold either lock when calling this method.
    ///
    /// **Idempotency:** no-op when no existing watcher for
    /// `session_id`. Safe to call multiple times (e.g., concurrent
    /// `start_agent_watcher` requests for the same session) —
    /// later calls observe an already-quiesced handle and re-store
    /// `false` (harmless).
    ///
    /// **What this does NOT do:** does not remove the OLD entry
    /// from the watchers map (`insert` does that atomically later)
    /// and does not signal the OLD handle's stop flag or join its
    /// background threads (the displaced `WatcherHandle::Drop`
    /// handles that AFTER `insert` evicts it).
    ///
    /// **Deadlock avoidance (cycle-16 retry-1, codex HIGH):** the
    /// `_watcher: Option<RecommendedWatcher>` is *moved out* under
    /// the gate but *dropped after the gate is released*. On macOS
    /// `RecommendedWatcher` is `FsEventWatcher`, whose `Drop` stops
    /// and joins the FSEvents runloop — if an in-flight OLD notify
    /// callback is blocked on this same per-session gate waiting to
    /// run, dropping the watcher under the gate would deadlock the
    /// restart: the gate-holder would wait for the runloop to
    /// drain, and the runloop would wait for the gate-holding
    /// thread to release before the in-flight callback could
    /// complete. The `alive=false` store under the gate is the
    /// load-bearing race-fix; the watcher-drop is a defensive
    /// belt-and-suspenders that stops fresh dispatches from the OS
    /// backend but doesn't need to be under the gate to remain
    /// correct (any callback that slips out between the gate
    /// release and the drop will still observe `alive == false` at
    /// `start_or_replace`'s gate-protected check).
    pub fn quiesce_existing(&self, session_id: &str, transcript_state: &TranscriptState) {
        let removed_watcher: Option<notify::RecommendedWatcher> = {
            let gate = transcript_state.session_gate(session_id);
            let _gate_guard = gate.lock();
            let mut watchers = self.watchers.lock().expect("failed to lock watchers");
            if let Some(handle) = watchers.get_mut(session_id) {
                handle
                    .alive
                    .store(false, std::sync::atomic::Ordering::Release);
                handle._watcher.take()
            } else {
                None
            }
        };
        // Drop the RecommendedWatcher OUTSIDE the gate so its
        // OS-level join doesn't wait on an in-flight callback that's
        // itself blocked on the gate. See the deadlock-avoidance
        // note in the docstring.
        drop(removed_watcher);
    }

    pub fn insert(&self, session_id: String, mut handle: WatcherHandle, agent_type: AgentType) {
        handle.agent_type = agent_type;

        // PR #302 cycle 9 (with cycle-9 retry-1 refinement) — close
        // the cycle-8 TOCTOU race + the residual race that cycle-9's
        // initial fix left behind.
        //
        // Acquire `TranscriptState`'s per-session start gate AROUND the
        // claim-flag read + watchers-map mutation + (when the new
        // handle DIDN'T claim) the under-gate teardown of any orphaned
        // old transcript entry. The flag write in `start_or_replace`
        // happens INSIDE that same gate (cycle 9 fix), so any in-flight
        // pre-register notify callback's start_or_replace must complete
        // (including its gate-held flag write) before insert can
        // acquire the gate and read the flag.
        //
        // **No-claim teardown.** If `new_claimed` is false, we
        // explicitly tear down the old transcript entry via
        // `stop_with_held_gate` BEFORE releasing the gate. Without
        // this, a notify callback could acquire the gate AFTER insert
        // released it but BEFORE the displaced handle's Drop ran stop
        // — successfully adopting the entry — and then displaced's
        // Drop would tear down the adopted entry. With under-gate
        // teardown, the entry is removed atomically with the
        // ownership-transfer decision; any subsequent notify callback
        // acquires a fresh gate and sees an empty `transcript_state`
        // (start_or_replace returns Started, registering the new
        // handle as owner).
        //
        // **Always-clear displaced.owns_transcript.** The displaced
        // handle's Drop is now ALWAYS a no-op for transcript state —
        // either the new handle owns the entry (claim transferred) or
        // we tore it down right here under the gate. Drop only joins
        // background threads (poll + codex session-index), which
        // happen outside any lock.
        //
        // **Lock-ordering invariant**: gate → watchers, matching
        // `start_or_replace` and `stop`. `stop_with_held_gate` does NOT
        // re-acquire the gate — that's the whole point — so no
        // deadlock from holding the gate across a teardown.
        //
        // **Drop-order invariant**: gate guard MUST drop before the
        // displaced `WatcherHandle` drops. With `owns_transcript = false`
        // always set, displaced's Drop no longer calls
        // `transcript_state.stop` and never re-acquires the gate, so
        // technically the deadlock concern is gone — but we still
        // scope the gate to the inner block for clarity and minimum
        // hold time.
        let ts = handle.transcript_state.clone();
        let gate = ts.session_gate(&session_id);
        // PR #302 cycle 11 F2: the orphan-teardown's tail-thread
        // JOIN must happen OUTSIDE the gate to keep the gate-hold
        // short (cycle-9 retry-1 originally held the gate across the
        // ~500ms join, stalling all concurrent gate waiters for that
        // duration). `stop_with_held_gate` now signals stop_flag
        // under the gate and returns the displaced `TranscriptHandle`
        // for the caller to drop after gate release. The handle
        // binding lives in the OUTER scope (this `let mut` here) so
        // the Drop happens at end-of-function, well after the inner
        // gate-scope ends.
        let mut removed_transcript: Option<TranscriptHandle> = None;
        // PR #302 cycle 17 F1 (Claude post-cycle-16 review HIGH 90%) —
        // moved-out displaced notify watcher, dropped OUTSIDE the
        // gate-scope. Mirrors the deadlock-avoidance fix in
        // `quiesce_existing` (cycle 16 retry-1): on macOS
        // `RecommendedWatcher = FsEventWatcher`, whose `Drop` joins
        // the FSEvents runloop. If an in-flight OLD callback is
        // blocked on the per-session gate, dropping the watcher
        // under the gate would ABBA-deadlock (gate-holder waits for
        // runloop drain; runloop waits for in-flight callback to
        // complete; callback waits for gate). Cycle 9 retry-2
        // introduced the under-gate drop as part of the
        // ownership-transfer fix; the comment claimed the deadlock
        // concern was gone, but that referred to the
        // `transcript_state.stop` gate re-entry, NOT the FSEvents
        // runloop join — the two concerns are orthogonal.
        let mut _removed_watcher_for_drop: Option<notify::RecommendedWatcher> = None;
        let _displaced: Option<WatcherHandle> = {
            let _gate_guard = gate.lock();

            let new_claimed = handle
                .claimed_transcript
                .load(std::sync::atomic::Ordering::Acquire);
            let mut displaced = {
                let mut watchers = self.watchers.lock().expect("failed to lock watchers");
                watchers.insert(session_id.clone(), handle)
            };
            if let Some(d) = displaced.as_mut() {
                // PR #302 cycle 10 — mark displaced alive=false UNDER
                // the gate, BEFORE any other cleanup. Already-
                // dispatched displaced callbacks that are blocked
                // waiting for this gate will, upon acquiring it,
                // observe alive=false and short-circuit out of
                // start_or_replace before any mutation. Closes the
                // in-flight-dispatch race codex-connector flagged in
                // round 10.
                d.alive.store(false, std::sync::atomic::Ordering::Release);

                // Move the displaced notify watcher OUT of the
                // handle under the gate (cycle 9 retry-2 — stops the
                // OS file-system backend from dispatching fresh
                // callbacks). The actual `Drop` happens at end-of-
                // function when `_removed_watcher_for_drop` falls out
                // of scope, well after the gate is released — see
                // the cycle-17 F1 note above the outer let-binding
                // for the macOS deadlock rationale.
                _removed_watcher_for_drop = d._watcher.take();

                if !new_claimed {
                    // Under-the-gate teardown: signal stop_flag on
                    // the orphaned old transcript entry while no
                    // concurrent start_or_replace can be in-flight
                    // for this session (cycle-9 retry-1). The
                    // returned TranscriptHandle is captured in
                    // `removed_transcript` (outer scope) so its
                    // Drop's thread-join happens OUTSIDE the gate
                    // (cycle 11 F2 — reduces gate-hold time from
                    // ~500ms to ~µs).
                    //
                    // PR #302 cycle 15 F1 (Claude post-cycle-13
                    // review): `_gate_guard` is now a typed
                    // `SessionGateGuard<'_>` issued by
                    // `ts.session_gate(&session_id).lock()` above;
                    // its `session_id()` is identical to
                    // `session_id` here. `stop_with_held_gate`
                    // debug-asserts the match so a future
                    // contributor accidentally passing the wrong
                    // session's guard fails fast in debug builds.
                    removed_transcript = ts.stop_with_held_gate(&session_id, &_gate_guard);
                }
                // Always clear: either the new handle owns the entry
                // (claim transferred) or we just tore it down — either
                // way displaced's Drop must not touch transcript_state.
                d.owns_transcript = false;
            }
            displaced
            // `_gate_guard` drops here, releasing the per-session gate.
        };
        // Explicit `drop(removed_transcript)` joins the displaced
        // tail thread (~500ms) HERE, OUTSIDE the gate. Moving this
        // earlier than Rust's default reverse-declaration drop order
        // is intentional — it lets the tail thread start winding
        // down before the OS-level work in `_displaced::Drop` runs.
        //
        // PR #302 cycle 19 F3 (Claude post-cycle-18 review LOW 87% +
        // codex-verify retry-1 correction LOW 95%): the pre-cycle-19
        // comment incorrectly grouped `_displaced`'s drop into this
        // same statement; the cycle-19 first attempt then mis-stated
        // the relative order. The actual order at function exit
        // (after this explicit `drop`) follows Rust's reverse-
        // declaration order. `_displaced` was declared LAST (line
        // ~434) so it drops FIRST; `_removed_watcher_for_drop` was
        // declared SECOND so it drops SECOND. Both happen at
        // end-of-function, both OUTSIDE the gate.
        // `_displaced::Drop` joins the poll thread + codex
        // session-index thread (if any) and SKIPS
        // `transcript_state.stop` (owns_transcript = false). The
        // safety property (every join happens outside the gate)
        // holds regardless of relative ordering of the three drops;
        // if a future change ever requires a different order, use an
        // explicit `drop(...)` call to force it.
        drop(removed_transcript);
        // At end-of-function (reverse-declaration order): `_displaced`
        // drops first, then `_removed_watcher_for_drop`. See cycle-17
        // F1 and the outer-scope binding comment above for why
        // `_removed_watcher_for_drop` is dropped outside the gate
        // (FsEventWatcher runloop join hazard on macOS).
    }

    /// Remove and stop a watcher for a session.
    ///
    /// Same lock-vs-Drop concern as `insert` — scope the guard so the
    /// removed `WatcherHandle` drops outside the mutex (Claude review
    /// on PR #152, F7).
    pub fn remove(&self, session_id: &str) -> bool {
        let handle = {
            let mut watchers = self.watchers.lock().expect("failed to lock watchers");
            watchers.remove(session_id)
        };
        handle.is_some()
        // `handle: Option<WatcherHandle>` drops at end of function,
        // after the lock guard above has already gone out of scope.
    }

    /// Check if a session has an active watcher
    #[allow(dead_code)]
    pub fn contains(&self, session_id: &str) -> bool {
        let watchers = self.watchers.lock().expect("failed to lock watchers");
        watchers.contains_key(session_id)
    }

    /// Number of active watchers across all sessions. Used for the
    /// diagnostic "active_watchers=N" log line — surfaces leaked
    /// watchers from prior sessions that are still polling old
    /// status.json files in the background.
    pub(crate) fn active_count(&self) -> usize {
        let watchers = self.watchers.lock().expect("failed to lock watchers");
        watchers.len()
    }

    /// Resolve which agent runs in a given pty/session — used by the rename /
    /// title-sync IPC (main #265). Reads from the same `watchers` mutex
    /// that gates `contains` / `active_count`, so callers observe
    /// agent-type and watcher-presence atomically (PR #302 Claude review
    /// F2 collapsed the previous separate `agent_types` map).
    pub fn agent_type_for_pty(&self, pty_id: &str) -> Option<AgentType> {
        let watchers = self.watchers.lock().expect("failed to lock watchers");
        watchers.get(pty_id).map(|handle| handle.agent_type)
    }

    /// The status source path the live watcher for `session_id` is
    /// currently tailing, or `None` when no watcher is registered. Reads
    /// the same `watchers` mutex as `contains` / `agent_type_for_pty`, so
    /// the relocate sequence sees a consistent (path, presence) pair. Used
    /// by `run_watch_sequence` to skip a no-op re-spawn when a fresh locate
    /// resolves the same path the handle already watches (drift-tick churn
    /// guard, VIM-192).
    pub(crate) fn current_status_path(&self, session_id: &str) -> Option<PathBuf> {
        let watchers = self.watchers.lock().expect("failed to lock watchers");
        watchers
            .get(session_id)
            .map(|handle| handle.status_path.clone())
    }

    /// Transfer Kimi's relocation-following transcript tail to the durable
    /// notification registration before the foreground watcher is removed.
    pub(crate) fn transfer_kimi_transcript(&self, session_id: &str) -> bool {
        let mut watchers = self.watchers.lock().expect("failed to lock watchers");
        let Some(handle) = watchers.get_mut(session_id) else {
            return false;
        };
        if handle.agent_type != AgentType::Kimi {
            return false;
        }
        handle.owns_transcript = false;
        true
    }

    /// Test-only seam to set a pty's agent type without going through a
    /// real watcher startup. Builds a stub `WatcherHandle::new_for_test`
    /// with a fresh `TranscriptState` (whose `stop` is a no-op for an
    /// unknown session id, matching the production handle Drop
    /// cascade), then inserts via the public `insert` API so the
    /// single-mutex invariant is preserved (PR #302 Claude review F2).
    /// PR #302 cycle 18 (Claude post-cycle-17 review LOW 90%): takes
    /// the *shared* `TranscriptState` so the stub's `insert`-driven
    /// gate acquisition and `stop_with_held_gate` call operate on the
    /// same state any real transcript tail was registered against.
    /// Previously this constructed a fresh `TranscriptState::new()`,
    /// which meant `insert`'s teardown branch looked for the tail in
    /// an isolated map (always empty) and silently no-op'd — leaking
    /// any real tail thread the production state still owned, with
    /// possible cross-test event contamination. The
    /// `Arc::ptr_eq(&gate.start_gates, &self.start_gates)`
    /// debug-assert inside `stop_with_held_gate` doesn't catch this
    /// because both sides come from the same stub state — they match
    /// each other, not the production state. Threading the real
    /// state is the only structural fix.
    #[cfg(any(test, feature = "e2e-test"))]
    pub(crate) fn insert_agent_type_for_test(
        &self,
        transcript_state: TranscriptState,
        pty_id: String,
        agent_type: AgentType,
    ) {
        let stub = WatcherHandle::new_for_test(transcript_state, pty_id.clone());
        self.insert(pty_id, stub, agent_type);
    }
}

/// Try to start transcript tailing, switching files if Claude reports a new path.
///
/// `cwd` is queried fresh from PtyState at every call rather than captured
/// by the outer `start_watching` closures. The user can `cd` mid-session
/// without restarting the agent watcher; we want the test-runner parser to
/// pick up the new workspace immediately. Combined with
/// `TranscriptState::start_or_replace`'s (transcript_path, cwd) identity
/// check, a cwd change triggers a Replace of the tail thread.
#[allow(clippy::too_many_arguments)]
fn maybe_start_transcript(
    validator: &Arc<dyn TranscriptPathValidator>,
    streamer: &Arc<dyn TranscriptStreamer>,
    events: Arc<dyn EventSink>,
    pty_state: &PtyState,
    transcript_state: &TranscriptState,
    session_id: &str,
    transcript_path: &str,
    located: &LocatedStatusSource,
    // PR #302 cycle 9 — the `claim_flag` is threaded all the way down
    // to `TranscriptState::start_or_replace`, which sets it while
    // STILL HOLDING the per-session gate. That gate-held write is
    // visible to any subsequent gate acquirer (notably
    // `AgentWatcherState::insert`'s gate-protected read), so the
    // cycle-8 TOCTOU race — where the post-return store could be
    // missed by a concurrent insert — is structurally closed.
    // Validation early-exits return BEFORE start_or_replace and DO
    // NOT set the flag; only outcomes that reach start_or_replace
    // (Started, Replaced, AlreadyRunning) claim ownership.
    claim_flag: Option<Arc<AtomicBool>>,
    // PR #302 cycle 10 — per-WatcherHandle alive token. Notify and
    // poll callbacks pass `Some(alive.clone())`; inline-init passes
    // `None` (synchronous, can't be displaced mid-flight).
    // `start_or_replace` checks this UNDER the per-session gate and
    // short-circuits if the handle has been displaced, preventing
    // already-dispatched displaced callbacks from claiming the entry
    // with stale data after the OS-level _watcher disconnect.
    alive: Option<Arc<AtomicBool>>,
) -> TxOutcome {
    let canonical = match validator.validate(transcript_path) {
        Ok(path) => path,
        Err(e) => {
            log::warn!(
                "Skipping transcript tailing for session {}: {}",
                session_id,
                e
            );
            return match e {
                ValidateTranscriptError::NotFound(_) => TxOutcome::Missing,
                ValidateTranscriptError::OutsideRoot { .. } => TxOutcome::OutsidePath,
                // `InvalidPath` is potentially-adversarial input
                // (currently null-byte injection), so it must NOT be
                // collapsed into `NotFile`. `NotFile` covers the
                // residual category of validation failures where the
                // path is structurally well-formed but the resolved
                // file isn't usable: `NotAFile` (canonical path is a
                // directory or special file) and `Other` (e.g., the
                // home-directory probe failed, or the path resolved to
                // something we couldn't classify). Operators grepping
                // logs for `tx_status=invalid_path` (see the
                // diagnostics emitter format string) get the
                // null-byte signal directly. Claude review on PR #153.
                ValidateTranscriptError::InvalidPath(_) => TxOutcome::InvalidPath,
                ValidateTranscriptError::NotAFile(_) | ValidateTranscriptError::Other(_) => {
                    TxOutcome::NotFile
                }
            };
        }
    };

    let cwd = located.resolved_directory.clone().or_else(|| {
        pty_state
            .get_cwd(&session_id.to_string())
            .map(PathBuf::from)
    });

    // Step B'': `TranscriptState::start_or_replace` now takes
    // `Arc<dyn TranscriptStreamer>` directly (was `Arc<dyn AgentAdapter>`
    // through B'). `bindings.streamer` is the concrete adapter's
    // streamer view — for Codex it shares the same `Arc<CompositeLocator>`
    // the watcher's locator uses (B' cycle 11), so there's no second
    // locator allocation.
    match transcript_state.start_or_replace(
        streamer.clone(),
        events,
        session_id.to_string(),
        canonical.clone(),
        cwd,
        claim_flag,
        alive,
    ) {
        Ok(TranscriptStartStatus::Started) => {
            log::info!(
                "Started transcript tailing for session {}: {}",
                session_id,
                canonical.display()
            );
            TxOutcome::Started
        }
        Ok(TranscriptStartStatus::Replaced) => {
            log::info!(
                "Switched transcript tailing for session {}: {}",
                session_id,
                canonical.display()
            );
            TxOutcome::Replaced
        }
        Ok(TranscriptStartStatus::AlreadyRunning) => TxOutcome::AlreadyRunning,
        Err(e) => {
            // PR #302 cycle 16 F1 (Claude post-cycle-15 review):
            // route by the typed `StartError` discriminant instead of
            // substring-matching a string sentinel. Pre-cycle-16 this
            // used `starts_with(DISPLACED_ERR_PREFIX)` — a typo or
            // i18n edit to the prefix would silently route every
            // restart's expected-condition Err into the generic warn
            // arm. With a structural enum the compiler enforces
            // both arms are handled and the producer/consumer
            // contract is strongly typed.
            if e.is_displaced() {
                log::debug!(
                    "transcript: displaced-watcher short-circuit for session {}: {}",
                    session_id,
                    e
                );
                TxOutcome::Displaced
            } else {
                log::warn!(
                    "Failed to start transcript tailing for session {}: {}",
                    session_id,
                    e
                );
                TxOutcome::StartFailed
            }
        }
    }
}

/// Start watching a statusline file for changes.
///
/// Watches the parent directory and filters for events on the target file.
/// Debounces at 100ms to avoid redundant processing.
///
/// CWD is intentionally NOT captured here. `maybe_start_transcript` queries
/// PtyState fresh on every invocation so a `cd` mid-session updates the
/// workspace seen by the test-runner parser.
///
/// Step 0c: the parameter shape changed from a bare
/// `status_file_path: PathBuf` to the full
/// [`LocatedStatusSource`] so the per-update transcript-path lookup
/// (which now goes through `TranscriptPathSource`)
/// can consult `static_transcript_hint` for Codex. The file watched
/// is still `located.status_path`; the rest of the struct is cloned
/// into each callback so they can resolve transcript paths via the
/// new trait without re-reading from the adapter.
/// `pre_inline_init` runs AFTER the fallible notify setup
/// (`recommended_watcher` + `watch(parent_dir)`) succeeds and BEFORE
/// the inline-init block reads the status file. It is the hook used
/// by `SessionLifecycle` to call
/// `AgentWatcherState::quiesce_existing` (PR #302 cycle 16 retry-1):
///   - Placing quiesce inside this hook (rather than before
///     `spawn_watch`) preserves the spawn-failure rollback invariant
///     — if any fallible step before this hook fails, we return
///     `Err` and the OLD watcher is untouched.
///   - Placing it BEFORE inline-init closes the codex P2 race —
///     inline-init's `start_or_replace` runs in a quiesced world
///     (OLD.alive already false; any in-flight OLD callback
///     short-circuits under the per-session gate).
pub(crate) fn start_watching(
    bindings: AgentBindings,
    events: Arc<dyn EventSink>,
    pty_state: PtyState,
    transcript_state: TranscriptState,
    session_id: String,
    located: TrustedLocatedSource,
    pre_inline_init: impl FnOnce(),
) -> Result<WatcherHandle, String> {
    let located = located.into_inner();
    let status_file_path = located.status_path.clone();
    // Retained for the `WatcherHandle` so the relocate sequence can compare a
    // fresh locate against the path this handle is already watching.
    let handle_status_path = status_file_path.clone();
    let target_path = status_file_path.clone();
    let sid = session_id.clone();
    let last_processed = Arc::new(Mutex::new(Instant::now()));
    let poll_stop = Arc::new((Mutex::new(false), Condvar::new()));
    let transcript_state_for_handle = transcript_state.clone();

    // Diagnostic state — per-source timing for sources that fire
    // repeatedly (notify, poll), and a SHARED path history so the
    // speculative→resolved transcript-path flip is detected even when
    // it spans sources (e.g. inline saw the speculative path, notify
    // sees the resolved one). The inline-init source has NO timing
    // mutex: it's one-shot and `dt` would be structurally zero —
    // record_event_diag accepts `Option<&Mutex<EventTiming>>` and
    // logs `dt=n/a` when None.
    let notify_timing = Arc::new(Mutex::new(EventTiming::default()));
    let poll_timing = Arc::new(Mutex::new(EventTiming::default()));
    let path_history = Arc::new(Mutex::new(PathHistory::default()));

    // Step B': pull the trait views out of bindings up front. Each
    // is `Arc<dyn ...>` — cheap to clone into closures and threads.
    //
    // Two `AgentBindings` fields are intentionally NOT destructured
    // here and drop with the function-local move:
    //
    // - `bindings.locator` — already used by `start_for` before this
    //   function runs (it borrowed `&bindings.locator` to call
    //   `locate(...)` and produce the `LocatedStatusSource` passed in
    //   as `located`). The `Arc` itself wasn't consumed there — it
    //   stayed alive on `bindings` and drops with this function-local
    //   move now (PR #261 cycle 11 review F32). The watcher itself
    //   doesn't need the locator again.
    // - `bindings.agent_type` — now CONSUMED here too: stamped onto
    //   the returned `WatcherHandle.agent_type` so
    //   `AgentWatcherState::agent_type_for_pty` can resolve under the
    //   same mutex as `contains` / `active_count`, and used below to
    //   gate the codex `session_index.jsonl` title-sync spawn (PR #302
    //   Claude review F2 + codex review F5).
    //
    // Step B'' (this PR): `bindings.streamer` is now CONSUMED here and
    // threaded into `TranscriptState::start_or_replace` (which migrated
    // off `Arc<dyn AgentAdapter>` onto `Arc<dyn TranscriptStreamer>`).
    // The former transitional `bindings.adapter_for_transcript_state`
    // field was removed in this step — the cycle-11 shared-`Arc`
    // guarantee now lives entirely in `bindings.streamer` (for Codex it
    // is the `Arc<CodexAdapter>` that shares the same
    // `Arc<CompositeLocator>` the locator uses).
    let agent_type = bindings.agent_type;
    let decoder = bindings.decoder;
    let validator = bindings.validator;
    let transcript_paths = bindings.transcript_paths;
    let streamer = bindings.streamer;

    // Shared "this handle has reached start_or_replace" signal.
    // Updated by both inline-init AND the notify callback below (which
    // can fire before `register` runs in the caller, since
    // `watcher.watch(...)` activates the notify backend before this
    // function returns). Stored on the `WatcherHandle` so
    // `AgentWatcherState::insert` reads it via `Acquire` load to gate
    // ownership transfer. PR #302 cycle 8 retry-2.
    let claimed_transcript = Arc::new(AtomicBool::new(false));
    // Per-handle alive token (cycle 10). True until
    // `AgentWatcherState::insert` displaces this handle. Notify and
    // poll callbacks pass this to `maybe_start_transcript` so
    // `start_or_replace` can short-circuit under its gate when the
    // handle is no longer current — closes the in-flight dispatch
    // race after _watcher.take() is called.
    let alive = Arc::new(AtomicBool::new(true));

    let notify_timing_for_cb = notify_timing.clone();
    let path_history_for_cb = path_history.clone();
    let decoder_for_cb = decoder.clone();
    let validator_for_cb = validator.clone();
    let transcript_paths_for_cb = transcript_paths.clone();
    let streamer_for_cb = streamer.clone();
    let last_processed_for_cb = last_processed.clone();
    let events_for_cb = events.clone();
    let pty_state_for_cb = pty_state.clone();
    let transcript_state_for_cb = transcript_state.clone();
    let claimed_transcript_for_cb = claimed_transcript.clone();
    let alive_for_cb = alive.clone();
    // Step 0c: each watcher callback (notify / inline-init / poll)
    // needs the full `LocatedStatusSource` so it can ask the adapter's
    // `TranscriptPathSource::static_hint` for Codex's attach-time
    // rollout path. Cheap — it's two `PathBuf`s + `Option<String>`.
    let located_for_cb = located.clone();
    // The notify closure consumes `sid` directly. Cycle 3 introduced a
    // redundant `sid = sid.clone()` for the new
    // `decoder.decode(Some(&sid), ...)` argument while the
    // legacy log/record_event_diag sites still used bare `sid`, leaving
    // two captures of the same value inside one closure (PR #261 cycle
    // 6 review F19). The closure now uses `sid` uniformly.

    // Debounce interval — ignore events within 100ms of the last processed one
    let debounce_ms = 100;

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        let event = match res {
            Ok(ev) => ev,
            Err(e) => {
                log::error!("Watcher error for session {}: {}", sid, e);
                return;
            }
        };

        // Only react to data modifications or file creation
        let dominated = matches!(
            event.kind,
            EventKind::Modify(notify::event::ModifyKind::Data(_))
                | EventKind::Create(notify::event::CreateKind::File)
        );
        if !dominated {
            return;
        }

        // Filter: only process events for the target status file
        let is_target = event.paths.iter().any(|p| p == &target_path);
        if !is_target {
            return;
        }

        // Debounce: skip if processed too recently
        {
            let mut last = last_processed_for_cb
                .lock()
                .expect("failed to lock debounce");
            let now = Instant::now();
            if now.duration_since(*last).as_millis() < debounce_ms {
                return;
            }
            *last = now;
        }

        let started = Instant::now();

        // Read and parse the status file
        let contents = match std::fs::read_to_string(&target_path) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Failed to read statusline file for session {}: {}", sid, e);
                return;
            }
        };

        if contents.trim().is_empty() {
            return;
        }

        // Step B': decode → session-id-stamped event. The decoder is
        // session-id-free per R2.2; the runtime composes the event
        // here (this was the v4-frozen plan's "runtime stamps the
        // session id" move).
        let (outcome, tx_path) = match decoder_for_cb.decode(Some(&sid), &contents) {
            Ok(snapshot) => {
                let event = stamp_snapshot(&sid, snapshot);
                if let Err(e) = emit_agent_status(events_for_cb.as_ref(), &event) {
                    log::error!("Failed to emit agent-status event: {}", e);
                }

                match resolve_transcript_path(&transcript_paths_for_cb, &contents, &located_for_cb)
                {
                    Some(path) => {
                        // PR #302 cycle 9: thread the claim flag down
                        // to `start_or_replace`, which writes it under
                        // the per-session gate. `AgentWatcherState::insert`
                        // reads the flag under the same gate, so a
                        // concurrent notify callback (us) can't race
                        // with insert — the gate serializes the
                        // claim-write and the claim-read.
                        // PR #302 cycle 10: also thread the alive
                        // token so `start_or_replace` can short-circuit
                        // under the gate if THIS handle has been
                        // displaced (closing the post-_watcher.take()
                        // in-flight-dispatch race).
                        let outcome = maybe_start_transcript(
                            &validator_for_cb,
                            &streamer_for_cb,
                            events_for_cb.clone(),
                            &pty_state_for_cb,
                            &transcript_state_for_cb,
                            &sid,
                            &path,
                            &located_for_cb,
                            Some(claimed_transcript_for_cb.clone()),
                            Some(alive_for_cb.clone()),
                        );
                        (outcome, Some(path))
                    }
                    None => (TxOutcome::NoPath, None),
                }
            }
            Err(e) => {
                log::warn!("Failed to parse statusline for session {}: {}", sid, e);
                (TxOutcome::ParseError, None)
            }
        };

        record_event_diag(
            Some(&notify_timing_for_cb),
            &path_history_for_cb,
            "notify",
            &sid,
            started.elapsed(),
            outcome,
            tx_path.as_deref(),
        );
    })
    .map_err(|e| format!("failed to create watcher: {}", e))?;
    // Watch the parent directory (notify watches directories, not individual
    // files). The directory is guaranteed to exist and to have passed the
    // canonicalize-and-verify trust-root check by `path_security::
    // ensure_status_source_under_trust_root`, the only path that reaches
    // this function (Claude review on PR #152, F3 — removed a redundant
    // create_dir_all that re-did path_security's work). If `start_watching`
    // is ever promoted to `pub` and gains a caller that bypasses
    // `start_for`, that caller is responsible for invoking
    // `ensure_status_source_under_trust_root` first; do NOT add a defensive
    // create_dir_all here, because it would create the directory without
    // the post-create symlink-race re-canonicalize check that
    // `path_security` performs.
    let parent_dir = status_file_path
        .parent()
        .ok_or_else(|| "status file path has no parent directory".to_string())?;

    watcher
        .watch(parent_dir, RecursiveMode::NonRecursive)
        .map_err(|e| format!("failed to start watching: {}", e))?;

    // PR #302 cycle 16 retry-1 — invoke the pre_inline_init hook
    // AFTER all fallible notify-setup succeeds and BEFORE inline-init
    // claims transcript. Caller uses this to quiesce any displaced
    // predecessor watcher (alive=false + drop _watcher outside the
    // gate), so inline-init's `start_or_replace` runs without racing
    // an in-flight OLD callback. If the steps above this point
    // returned `Err`, the hook never ran and the OLD watcher is
    // intact — spawn-failure rollback invariant preserved.
    pre_inline_init();

    // Read the file immediately in case it was already written before
    // the watcher started (common race: status.json written by statusline.sh
    // before the frontend calls start_agent_watcher).
    //
    // The `claimed_transcript` shared `Arc<AtomicBool>` is updated
    // below (and by the notify callback above) when
    // `maybe_start_transcript` actually reaches
    // `TranscriptState::start_or_replace`. `AgentWatcherState::insert`
    // reads the flag at restart time to gate ownership transfer from a
    // displaced predecessor handle (PR #302 cycle 5 P1 + cycle 8 P2 +
    // cycle 8 retry-2).
    {
        let initial_sid = session_id.clone();
        let initial_path = status_file_path.clone();
        let initial_decoder = decoder.clone();
        let initial_validator = validator.clone();
        let initial_transcript_paths = transcript_paths.clone();
        let initial_streamer = streamer.clone();
        let initial_events = events.clone();
        let initial_pty_state = pty_state.clone();
        let initial_transcript_state = transcript_state.clone();
        let initial_located = located.clone();
        let started = Instant::now();
        let mut outcome = TxOutcome::NoPath;
        let mut inline_tx_path: Option<String> = None;
        if let Ok(contents) = std::fs::read_to_string(&initial_path) {
            if !contents.trim().is_empty() {
                *last_processed.lock().expect("failed to lock debounce") = Instant::now();
                match initial_decoder.decode(Some(&initial_sid), &contents) {
                    Ok(snapshot) => {
                        let event = stamp_snapshot(&initial_sid, snapshot);
                        let _ = emit_agent_status(initial_events.as_ref(), &event);
                        if let Some(path) = resolve_transcript_path(
                            &initial_transcript_paths,
                            &contents,
                            &initial_located,
                        ) {
                            // PR #302 cycle 9: pass the claim flag to
                            // maybe_start_transcript so the flag write
                            // happens INSIDE `start_or_replace`'s
                            // per-session gate. Validation early-exits
                            // never reach start_or_replace and so don't
                            // set the flag (correct: no claim).
                            // PR #302 cycle 10: inline-init is
                            // synchronous within start_watching — can't
                            // be displaced mid-flight, so passes None
                            // for the alive token. Only async callbacks
                            // (notify / poll) need it.
                            outcome = maybe_start_transcript(
                                &initial_validator,
                                &initial_streamer,
                                initial_events.clone(),
                                &initial_pty_state,
                                &initial_transcript_state,
                                &initial_sid,
                                &path,
                                &initial_located,
                                Some(claimed_transcript.clone()),
                                None,
                            );
                            inline_tx_path = Some(path);
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to parse statusline for session {} \
                             (inline-init): {}",
                            initial_sid,
                            e
                        );
                        outcome = TxOutcome::ParseError;
                    }
                }
                record_event_diag(
                    None,
                    &path_history,
                    "inline",
                    &initial_sid,
                    started.elapsed(),
                    outcome,
                    inline_tx_path.as_deref(),
                );
            }
        }
    }

    // Polling fallback — WSL2's inotify can miss events and Claude Code
    // may use atomic writes (rename). The stop_flag is set when the
    // WatcherHandle is dropped, causing the thread to exit cleanly.
    let poll_join_handle = {
        let poll_sid = session_id.clone();
        let poll_path = status_file_path.clone();
        let poll_events = events.clone();
        let poll_pty_state = pty_state.clone();
        let poll_transcript_state = transcript_state.clone();
        let poll_stop = poll_stop.clone();
        let poll_timing_for_thread = poll_timing.clone();
        let path_history_for_poll = path_history.clone();
        let poll_decoder = decoder.clone();
        let poll_validator = validator.clone();
        let poll_transcript_paths = transcript_paths.clone();
        let poll_streamer = streamer.clone();
        // Step 0c: poll thread also routes transcript-path lookups
        // through `TranscriptPathSource` and so needs its own clone of
        // the `LocatedStatusSource`.
        let poll_located = located.clone();
        // PR #302 cycle 10: poll thread can fire maybe_start_transcript
        // long after the handle was displaced (it sleeps 3s before its
        // first iteration, then on a 3s cadence). Passing the alive
        // token lets `start_or_replace` short-circuit under its gate
        // if the handle is no longer current.
        let poll_alive = alive.clone();
        // PR #302 cycle 11 F1: pass the claim flag from the poll thread
        // too — defense-in-depth. The original "3s sleep guarantees
        // ordering with insert" argument only covered iteration 1; on
        // iterations 2+ the poll thread can race with a concurrent
        // insert. Cycle 10's `alive` token already short-circuits the
        // start_or_replace in that race, but passing claim_flag keeps
        // the symmetry with inline-init / notify callbacks and lets
        // any successful start_or_replace from a fresh (non-displaced)
        // poll iteration also count as a pre-register claim.
        let poll_claimed_transcript = claimed_transcript.clone();
        Some(std::thread::spawn(move || {
            // `poll_last` is the per-thread dedup buffer. Originally
            // wrapped in `Arc<Mutex<String>>` by analogy with the other
            // `Arc`-shared state in the spawn closure, but it is only
            // touched here and never escapes the thread (Claude review
            // on PR #152, F11). Plain `String` removes the heap
            // allocation and per-poll-cycle atomic refcount traffic.
            let mut poll_last = String::new();

            loop {
                let (lock, wake) = &*poll_stop;
                // pre_wait_guard is consumed by wait_timeout_while; stop_guard must drop before file I/O.
                let pre_wait_guard = lock.lock().expect("failed to lock poll stop flag");
                let (stop_guard, _) = wake
                    .wait_timeout_while(pre_wait_guard, Duration::from_secs(3), |flag| !*flag)
                    .expect("failed to wait on poll stop flag");
                if *stop_guard {
                    break;
                }
                drop(stop_guard);

                // Capture `started` BEFORE the read so `total` covers file
                // I/O the same way the notify and inline sources do —
                // otherwise WSL2/virtio-fs read latency is silently
                // excluded from poll's number, making cross-source
                // comparison unsound and biasing the freeze diagnosis
                // toward notify. Dedup-skip `continue` paths run before
                // `record_event_diag`, so unchanged-content polls never
                // log a number — no noise.
                let started = Instant::now();

                let contents = match std::fs::read_to_string(&poll_path) {
                    Ok(c) if !c.trim().is_empty() => c,
                    _ => continue,
                };

                if poll_last == contents {
                    continue;
                }
                poll_last = contents.clone();

                let (outcome, tx_path) = match poll_decoder.decode(Some(&poll_sid), &contents) {
                    Ok(snapshot) => {
                        let event = stamp_snapshot(&poll_sid, snapshot);
                        let _ = emit_agent_status(poll_events.as_ref(), &event);
                        // Step 0c: same `TranscriptPathSource` flow as
                        // the notify and inline callbacks.
                        match resolve_transcript_path(
                            &poll_transcript_paths,
                            &contents,
                            &poll_located,
                        ) {
                            Some(path) => {
                                // PR #302 cycle 11 F1: pass both the
                                // claim flag AND the alive token, same
                                // as the inline-init and notify-callback
                                // paths. The "3s sleep guarantees
                                // ordering with insert" argument that
                                // justified None for claim_flag only
                                // applied to iteration 1; iterations
                                // 2+ have no ordering guarantee, so
                                // defense-in-depth says track claims
                                // from this path too.
                                let outcome = maybe_start_transcript(
                                    &poll_validator,
                                    &poll_streamer,
                                    poll_events.clone(),
                                    &poll_pty_state,
                                    &poll_transcript_state,
                                    &poll_sid,
                                    &path,
                                    &poll_located,
                                    Some(poll_claimed_transcript.clone()),
                                    Some(poll_alive.clone()),
                                );
                                (outcome, Some(path))
                            }
                            None => (TxOutcome::NoPath, None),
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to parse statusline for session {} (poll): {}",
                            poll_sid,
                            e
                        );
                        (TxOutcome::ParseError, None)
                    }
                };

                record_event_diag(
                    Some(&poll_timing_for_thread),
                    &path_history_for_poll,
                    "poll",
                    &poll_sid,
                    started.elapsed(),
                    outcome,
                    tx_path.as_deref(),
                );
            }
        }))
    };

    // Codex title-sync watcher (PR #302 codex review F5). Re-wires the
    // `session_index.jsonl` watcher that the pre-fix refactor dropped:
    // when the locator surfaced an `agent_session_id` (Codex's
    // `thread_id`), spawn a watcher that emits `agent-session-title`
    // events as `thread_name` updates land in
    // `<codex_home>/session_index.jsonl`. The `WatcherHandle::Drop`
    // cascade signals stop + joins the thread so the title-sync
    // lifecycle is bound 1:1 to the statusline watcher's lifetime.
    //
    // Gated on (a) `agent_type == Codex` and (b) the locator actually
    // surfaced an agent_session_id — both must hold. Claude / NoOp /
    // Codex-without-thread-id leave the handle's title-sync fields as
    // `None` and Drop becomes a no-op for those branches.
    let (session_index_stop, session_index_join) =
        if matches!(agent_type, AgentType::Codex) && located.agent_session_id.is_some() {
            // PR #302 cycle 2 F4: `located.trust_root` can be the relative
            // `.codex` path when `dirs::home_dir()` returns `None` (headless
            // / container environments). `default_codex_home()` keeps the
            // relative fallback so codex attach itself doesn't hard-fail —
            // but joining `session_index.jsonl` onto a relative trust_root
            // and handing the result to `spawn_watch` would open the wrong
            // file (relative to the sidecar's cwd, NOT the user's home).
            // Status-path flow tolerates this because
            // `ensure_status_source_under_trust_root` canonicalizes early;
            // the title-sync path bypasses that gate, so we add an explicit
            // absoluteness check here. Non-absolute → skip the title-sync
            // spawn and log a warn so operators can correlate "Codex titles
            // not updating" with a missing HOME env.
            if !located.trust_root.is_absolute() {
                log::warn!(
                    "codex title-sync: skipping spawn — trust_root is not absolute (path={}); \
                     check HOME / dirs::home_dir() in this environment",
                    located.trust_root.display(),
                );
                (None, None)
            } else {
                let agent_session_id = located
                    .agent_session_id
                    .clone()
                    .expect("checked is_some above");
                let session_index_path = located.trust_root.join("session_index.jsonl");
                let stop = Arc::new(AtomicBool::new(false));
                let join = super::super::codex::session_index::spawn_watch(
                    session_index_path,
                    agent_session_id,
                    session_id.clone(),
                    events.clone(),
                    stop.clone(),
                );
                (Some(stop), Some(join))
            }
        } else {
            (None, None)
        };

    let (attention_stop, attention_join) = if matches!(agent_type, AgentType::ClaudeCode) {
        let path =
            crate::agent::adapter::claude_code::bridge::session_attention_file(&status_file_path);
        match super::super::claude_code::hooks::start_tailing(
            &path,
            events.clone(),
            session_id.clone(),
        ) {
            Ok(handles) => (Some(handles.0), Some(handles.1)),
            Err(error) => {
                log::warn!("Claude attention hook stream unavailable: {}", error);
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    log::info!(
        "Started watching statusline for session {}: {}",
        session_id,
        status_file_path.display()
    );

    Ok(WatcherHandle {
        status_path: handle_status_path,
        _watcher: Some(watcher),
        poll_stop,
        join_handle: poll_join_handle,
        transcript_state: transcript_state_for_handle,
        session_id: session_id.clone(),
        agent_type,
        session_index_stop,
        session_index_join,
        attention_stop,
        attention_join,
        // Default `true` — this handle owns the transcript-state entry
        // it just established (or inherited) via inline-init or a
        // pre-register notify callback. `AgentWatcherState::insert`
        // clears this to `false` only if a NEW handle later displaces
        // this one AND the new handle reached `start_or_replace` at
        // least once before insert (see `claimed_transcript`).
        owns_transcript: true,
        // Shared Arc with the notify callback closure above and the
        // inline-init block above; both sites set it on a successful
        // pre-register claim. `AgentWatcherState::insert` reads via
        // Acquire load to decide ownership transfer (PR #302 cycle 8
        // retry-2 — widened from inline-init-only after codex verify
        // flagged the pre-register notify-callback race).
        claimed_transcript,
        // Cycle 10: shared with the notify and poll callbacks above
        // so `start_or_replace` can short-circuit under its gate when
        // this handle has been displaced (set false by
        // `AgentWatcherState::insert`).
        alive,
    })
}

#[cfg(any(test, feature = "e2e-test"))]
impl WatcherHandle {
    pub(crate) fn new_for_test(transcript_state: TranscriptState, session_id: String) -> Self {
        // `agent_type` defaults to `ClaudeCode`. Tests that care about
        // the agent type pass the real value through
        // `AgentWatcherState::insert(sid, handle, agent_type)`, which
        // overwrites this field — so the default only matters for
        // tests that never call `agent_type_for_pty` on the stub.
        WatcherHandle {
            status_path: PathBuf::new(),
            _watcher: None,
            poll_stop: Arc::new((Mutex::new(false), Condvar::new())),
            join_handle: None,
            transcript_state,
            session_id,
            agent_type: AgentType::ClaudeCode,
            session_index_stop: None,
            session_index_join: None,
            attention_stop: None,
            attention_join: None,
            owns_transcript: true,
            // Default `false` (no pre-register claim) — matches the
            // "did NOT run start_watching" test seam semantic. Tests
            // that simulate "the displacing handle DID claim transcript
            // ownership" should call `set_claimed_for_test(true)`
            // before passing the handle to `AgentWatcherState::insert`.
            claimed_transcript: Arc::new(AtomicBool::new(false)),
            // Default `true` — fresh test handle is alive. Insert will
            // set it false on displacement just like in production.
            alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Test-only setter for the pre-register claim signal. Stores into
    /// the shared `Arc<AtomicBool>` so tests can simulate either
    /// restart shape:
    ///
    /// - leave default `false` to model "new handle never reached
    ///   start_or_replace before insert" (cycle 8 Case B — displaced
    ///   handle KEEPS ownership)
    /// - set `true` to model "new handle reached start_or_replace via
    ///   inline-init or a pre-register notify callback" (cycle 5 Case
    ///   A — displaced handle transfers ownership)
    ///
    /// Without this seam, tests can't exercise the ownership-transfer
    /// decision independently of running the full inline-init / notify
    /// code path.
    #[cfg(test)]
    pub(crate) fn set_claimed_for_test(&mut self, v: bool) {
        self.claimed_transcript
            .store(v, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::FakeEventSink;

    #[test]
    fn creates_empty_watcher_state() {
        let state = AgentWatcherState::new();
        assert!(!state.contains("test-session"));
    }

    #[test]
    fn remove_returns_false_for_missing_session() {
        let state = AgentWatcherState::new();
        assert!(!state.remove("nonexistent"));
    }

    #[test]
    fn insert_makes_agent_type_and_presence_atomic_under_single_lock() {
        // PR #302 Claude review F2 — `agent_type_for_pty` and `contains`
        // / `active_count` must agree on every snapshot. Before the fix,
        // `agent_types` and `watchers` were separate mutexes and a
        // concurrent reader could see (Some(type), contains=false) or
        // (None, contains=true) between insert's two critical sections.
        // After the fix, the agent type lives on `WatcherHandle` and is
        // resolved from the same map `contains` queries — so the
        // round-trip below is structurally atomic.
        let state = AgentWatcherState::new();
        let sid = "atomic-sid".to_string();
        assert_eq!(state.agent_type_for_pty(&sid), None);
        assert!(!state.contains(&sid));

        let handle = WatcherHandle::new_for_test(TranscriptState::new(), sid.clone());
        state.insert(sid.clone(), handle, AgentType::Codex);

        // Both reads under the same `watchers` mutex — must agree.
        assert_eq!(state.agent_type_for_pty(&sid), Some(AgentType::Codex));
        assert!(state.contains(&sid));

        state.remove(&sid);
        assert_eq!(state.agent_type_for_pty(&sid), None);
        assert!(!state.contains(&sid));
    }

    /// PR #302 codex review cycle 5 P1 — when a NEW handle for the same
    /// session displaces an OLD handle via `insert`, AND the new
    /// handle has reached `start_or_replace` at least once before the
    /// insert (via inline-init or a pre-register notify callback),
    /// the displaced OLD handle's Drop must NOT stop the per-session
    /// transcript entry — otherwise the session ends up with a status
    /// watcher but no transcript tail. This test simulates the cycle-5
    /// happy path: handle B's pre-register claim flag is set explicitly
    /// via `set_claimed_for_test(true)`, so insert should transfer
    /// ownership and A's Drop must NOT stop the entry.
    #[test]
    fn insert_transfers_transcript_ownership_when_new_handle_claimed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript_path = tmp.path().join("t.jsonl");
        std::fs::write(&transcript_path, "").expect("write");

        let transcript_state = TranscriptState::new();
        let sid = "restart-sid".to_string();
        let adapter: Arc<dyn TranscriptStreamer> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);
        let sink = Arc::new(FakeEventSink::new());

        // Seed transcript_state with an entry for `sid` (as if the OLD
        // handle's `start_watching` had done so via start_or_replace).
        let status = transcript_state
            .start_or_replace(
                adapter,
                sink,
                sid.clone(),
                transcript_path.clone(),
                None,
                None,
                None,
            )
            .expect("seed transcript");
        assert_eq!(status, TranscriptStartStatus::Started);

        // OLD handle A — owns the seeded transcript entry.
        let handle_a = WatcherHandle::new_for_test(transcript_state.clone(), sid.clone());
        let state = AgentWatcherState::new();
        state.insert(sid.clone(), handle_a, AgentType::ClaudeCode);
        assert!(transcript_state.contains(&sid));

        // NEW handle B — same session, and it CLAIMED transcript
        // ownership before insert (simulated via the test setter). In
        // production this would be set by `start_watching` when either
        // (a) the inline-init's `maybe_start_transcript` call reaches
        // `start_or_replace`, or (b) the notify callback (registered
        // before `start_watching` returns) fires in time and reaches
        // `start_or_replace` before the caller's `register` call.
        let mut handle_b = WatcherHandle::new_for_test(transcript_state.clone(), sid.clone());
        handle_b.set_claimed_for_test(true);
        state.insert(sid.clone(), handle_b, AgentType::ClaudeCode);

        // Critical assertion: A has been displaced and dropped, but the
        // transcript entry must STILL be tracked because B inherited
        // ownership (the `owns_transcript = false` clearing in
        // `insert`, gated on B's claim flag).
        assert!(
            transcript_state.contains(&sid),
            "transcript tail must survive A's displacement — B owns it now",
        );

        // Clean removal of B drops with owns_transcript = true, so the
        // tail is finally stopped.
        state.remove(&sid);
        assert!(
            !transcript_state.contains(&sid),
            "transcript tail must stop when the final handle is removed",
        );
    }

    /// PR #302 codex review cycle 8 P2 — counter-example to cycle-5
    /// ownership transfer: when the NEW handle's inline-init did NOT
    /// engage with `transcript_state` (status file unreadable / empty
    /// / parse-failed / NoPath), ownership MUST stay with the OLD
    /// handle so its Drop properly tears down the orphaned tail.
    /// Otherwise the previous session's transcript tail keeps streaming
    /// under the new watcher indefinitely (until a later status update
    /// happens to replace it, or the new handle is removed).
    /// This test simulates the cycle-8 Case B path: handle B's
    /// inline-init flag stays at its default `false`, so insert MUST
    /// NOT clear A's owns_transcript flag, and A's Drop MUST stop the
    /// transcript entry.
    #[test]
    fn insert_keeps_displaced_ownership_when_new_handle_did_not_claim() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript_path = tmp.path().join("t.jsonl");
        std::fs::write(&transcript_path, "").expect("write");

        let transcript_state = TranscriptState::new();
        let sid = "case-b-sid".to_string();
        let adapter: Arc<dyn TranscriptStreamer> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);
        let sink = Arc::new(FakeEventSink::new());

        // Seed transcript_state (as if OLD handle's inline-init had
        // established the entry).
        let status = transcript_state
            .start_or_replace(
                adapter,
                sink,
                sid.clone(),
                transcript_path.clone(),
                None,
                None,
                None,
            )
            .expect("seed transcript");
        assert_eq!(status, TranscriptStartStatus::Started);

        // OLD handle A — owns the seeded entry.
        let handle_a = WatcherHandle::new_for_test(transcript_state.clone(), sid.clone());
        let state = AgentWatcherState::new();
        state.insert(sid.clone(), handle_a, AgentType::ClaudeCode);
        assert!(transcript_state.contains(&sid));

        // NEW handle B — same session, but NO pre-register claim
        // (the shared Arc<AtomicBool> stays at its default `false`).
        // This simulates Case B: B's start_watching read the status
        // file but it was unreadable / empty / parse-failed / had no
        // transcript path / validation-failed, so neither inline-init
        // nor any pre-register notify callback reached
        // `start_or_replace`.
        let handle_b = WatcherHandle::new_for_test(transcript_state.clone(), sid.clone());
        assert!(
            !handle_b
                .claimed_transcript
                .load(std::sync::atomic::Ordering::Acquire),
            "test premise: B did not claim transcript ownership before insert",
        );
        state.insert(sid.clone(), handle_b, AgentType::ClaudeCode);

        // Critical assertion: A has been displaced; since B did NOT
        // claim, A retained `owns_transcript = true`; A's Drop called
        // `transcript_state.stop(&sid)`; the orphaned old tail is now
        // gone. Without the cycle-8 gate, A's Drop would have skipped
        // the stop call and this assertion would fail.
        assert!(
            !transcript_state.contains(&sid),
            "displaced handle MUST stop the orphaned tail when the \
             new handle didn't claim transcript ownership",
        );

        // Subsequent removal of B is a no-op for transcript_state
        // (already empty) — pins that B's Drop doesn't panic / error
        // when there's nothing to stop.
        state.remove(&sid);
        assert!(!transcript_state.contains(&sid));
    }

    /// PR #302 cycle 16 (codex P2 + Claude F1) — `quiesce_existing`
    /// flips the existing handle's `alive` flag to `false` and drops
    /// its `_watcher`. After quiesce, an in-flight callback that
    /// later reaches `start_or_replace` with this handle's alive
    /// token will short-circuit with `StartError::Displaced`.
    /// No-op on a missing session.
    #[test]
    fn t_quiesce_existing_flips_alive_and_drops_watcher() {
        let transcript_state = TranscriptState::new();
        let watcher_state = AgentWatcherState::new();
        let sid = "quiesce-test".to_string();

        // No-op on missing — must not panic, must not create a stub.
        watcher_state.quiesce_existing(&sid, &transcript_state);
        assert!(!watcher_state.contains(&sid));

        // Insert a stub handle and capture its `alive` Arc clone so we
        // can observe the flag change after quiesce.
        let handle = WatcherHandle::new_for_test(transcript_state.clone(), sid.clone());
        let alive_observer = handle.alive.clone();
        watcher_state.insert(sid.clone(), handle, AgentType::ClaudeCode);
        assert!(
            alive_observer.load(std::sync::atomic::Ordering::Acquire),
            "fresh handle should be alive before quiesce",
        );

        watcher_state.quiesce_existing(&sid, &transcript_state);
        assert!(
            !alive_observer.load(std::sync::atomic::Ordering::Acquire),
            "quiesce_existing must flip alive=false on the existing handle",
        );

        // Simulate the in-flight-callback short-circuit path:
        // start_or_replace called with the (now-false) alive token
        // returns StartError::Displaced.
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript_path = tmp.path().join("t.jsonl");
        std::fs::write(&transcript_path, "").expect("write");
        let adapter: Arc<dyn TranscriptStreamer> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);
        let sink = Arc::new(FakeEventSink::new());
        let err = transcript_state
            .start_or_replace(
                adapter,
                sink,
                sid.clone(),
                transcript_path,
                None,
                None,
                Some(alive_observer.clone()),
            )
            .expect_err("alive=false must produce StartError::Displaced");
        assert!(err.is_displaced(), "expected Displaced, got: {:?}", err);
    }

    /// PR #302 cycle 16 F1 (Claude post-cycle-15 review) —
    /// `StartError::is_displaced` returns the right discriminant for
    /// both variants. Pins the structural-routing contract that
    /// `maybe_start_transcript` depends on.
    #[test]
    fn t_start_error_discriminant_routes_correctly() {
        let displaced = super::super::transcript_state::StartError::Displaced("x".into());
        let failed = super::super::transcript_state::StartError::Failed("y".into());
        assert!(displaced.is_displaced());
        assert!(!failed.is_displaced());
    }

    #[test]
    fn t_lifecycle_3_watcher_handle_drop_cascades_transcript_stop() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let transcript_path = tmp.path().join("t.jsonl");
        std::fs::write(&transcript_path, "").expect("failed to write transcript");

        let transcript_state = TranscriptState::new();
        let sid = "test-sid".to_string();
        let adapter: Arc<dyn TranscriptStreamer> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);
        let sink = Arc::new(FakeEventSink::new());

        let status = transcript_state
            .start_or_replace(
                adapter,
                sink,
                sid.clone(),
                transcript_path,
                None,
                None,
                None,
            )
            .expect("failed to start transcript watcher");
        assert_eq!(status, TranscriptStartStatus::Started);

        let handle = WatcherHandle::new_for_test(transcript_state.clone(), sid.clone());
        let watcher_state = AgentWatcherState::new();
        watcher_state.insert(sid.clone(), handle, AgentType::ClaudeCode);

        assert!(
            transcript_state.contains(&sid),
            "transcript should be tracked before remove"
        );

        watcher_state.remove(&sid);

        assert!(
            !transcript_state.contains(&sid),
            "transcript should be stopped after handle drop"
        );
    }
}
