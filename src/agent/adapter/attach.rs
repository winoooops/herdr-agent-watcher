//! Attach-time and runtime context for the agent adapter.
//!
//! Two related types live here so the SNAPSHOT-vs-LIVE distinction is
//! visible at a glance:
//!
//! - [`AttachContext`] — the immutable facts known at the moment an agent
//!   adapter is attached to a PTY session. Built once in
//!   `start_agent_watcher_inner`; threaded through the adapter-binding /
//!   locator / decoder / streamer chain by later refactor steps.
//! - [`SessionRuntimeContext`] — a live handle to `PtyState` scoped to one
//!   session, exposing accessors (currently just `live_cwd()`) that
//!   re-query `PtyState` on every call. Used by callers that need the
//!   CURRENT workspace cwd (which can change mid-session via the user's
//!   shell `cd`), not the attach-time snapshot.
//!
//! Tracking: [#246](https://github.com/winoooops/vimeflow/issues/246)
//! (Steps 0a + 0b of the `refactor/agent-adapter` v4-frozen plan).
//!
//! **Constraint (v4-frozen #5):** `AttachContext.initial_cwd` is a
//! SNAPSHOT taken at attach time. Runtime callers that need the live
//! workspace cwd (transcript replace, test-runner cwd resolution) MUST
//! read it from `PtyState` via [`SessionRuntimeContext`], NOT from
//! `AttachContext`. Future refactor steps (B''/D') will migrate the
//! call site at `base/watcher_runtime.rs::maybe_start_transcript` from
//! `&PtyState` + `session_id` to a `&SessionRuntimeContext` so the
//! invariant is structural rather than convention-only.
//!
//! **Provider-neutral schema:** [`AttachContext`] intentionally has no
//! per-agent field names (no `codex_home`, no `claude_home`). All
//! per-agent metadata lives in [`crate::agent::config`], the central
//! registry; `provider_home` is resolved at attach time via
//! `config::spec_for(agent_type).provider_home()`. Adding a new agent
//! touches the registry only, not this struct.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::agent::types::AgentType;
use crate::terminal::PtyState;

/// Immutable attach-time facts. Built once per agent attach by
/// `start_agent_watcher_inner` and consumed by adapter-binding code.
///
/// `provider_home` and `proc_root` are populated up-front but unused
/// until later refactor steps (B', C). The struct-level
/// `#[allow(dead_code)]` covers that interim — remove once every field
/// has at least one reader.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct AttachContext {
    /// Vimeflow PTY session id.
    pub(crate) session_id: String,
    /// CWD known at attach time. SNAPSHOT — see module-level note.
    pub(crate) initial_cwd: PathBuf,
    /// PID of the shell process the agent runs under.
    pub(crate) shell_pid: u32,
    /// PID of the detected agent process (distinct from `shell_pid`).
    pub(crate) agent_pid: u32,
    /// `SystemTime` when the PTY session started — used by Codex's
    /// locator to filter stale rows out of `logs.sqlite` / `state.sqlite`.
    pub(crate) pty_start: SystemTime,
    /// Which CLI agent is attached.
    pub(crate) agent_type: AgentType,
    /// Vimeflow-owned app data directory. Runtime bridge/status files live
    /// under this root instead of the user's project tree.
    pub(crate) app_data_dir: PathBuf,
    /// Home directory for the attached agent (e.g. `~/.codex` for Codex,
    /// `~/.claude` for ClaudeCode). `None` when the active agent has no
    /// canonical home (Aider / Generic today). Resolved at attach time
    /// via [`crate::agent::config::AgentSpec::provider_home`]; adding a
    /// new provider home only touches that registry, not this struct.
    pub(crate) provider_home: Option<PathBuf>,
    /// Explicit provider-home override used by hermetic E2E helpers.
    /// When set, it takes precedence over both the registry-resolved
    /// `provider_home` and any agent-specific environment variable
    /// (`KIMI_CODE_HOME`) so tests can seed fixtures under a temporary
    /// directory without mutating the process-wide environment across
    /// an await.
    pub(crate) provider_home_override: Option<PathBuf>,
    /// Filesystem path to `/proc`. `Some(/proc)` on Linux; `None` on
    /// platforms where `/proc` doesn't exist (macOS, Windows). Codex's
    /// `/proc`-based fast-paths (`resume_thread_id_from_proc`,
    /// `open_rollout_paths_from_proc`) gate on this; consumers fall
    /// through to the FS-scan strategy when `None`. Populated via
    /// [`crate::agent::config::default_proc_root`].
    pub(crate) proc_root: Option<PathBuf>,
}

/// Live runtime handle for one PTY session.
///
/// Pairs a session id with a `PtyState` handle so callers can read the
/// CURRENT workspace cwd on demand. Distinct from [`AttachContext`],
/// whose `initial_cwd` is frozen at attach time.
///
/// Cheap to clone — `PtyState` internally wraps `Arc<Mutex<...>>`, so
/// cloning this struct only bumps refcounts.
///
/// The `PtyState` handle is intentionally private: future refactor
/// callers (transcript replace, test-runner snapshot building) receive
/// `&SessionRuntimeContext` instead of `&PtyState` + `session_id`, and
/// the only cwd path they can take is [`Self::live_cwd`]. That keeps
/// v4-frozen constraint #5 — "live cwd comes from `PtyState` via
/// `SessionRuntimeContext`, never from `AttachContext`" — structural.
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct SessionRuntimeContext {
    /// Vimeflow PTY session id this runtime context is scoped to.
    session_id: String,
    /// Live PTY-state handle. Queried each call to [`Self::live_cwd`]
    /// so a mid-session `cd` in the user's shell is observed
    /// immediately.
    pty_state: PtyState,
}

#[allow(dead_code)]
impl SessionRuntimeContext {
    /// Build a runtime context for `session_id` against the live
    /// `PtyState`.
    pub(crate) fn new(session_id: String, pty_state: PtyState) -> Self {
        Self {
            session_id,
            pty_state,
        }
    }

    /// The session id this context is scoped to. Provided so callers
    /// that already need the id (logging, event composition) don't need
    /// to thread it separately alongside the context.
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Read the LIVE cwd for this session, re-querying `PtyState` on
    /// every call. Returns `None` when the PTY session is gone (killed,
    /// expired, or never existed).
    ///
    /// Mirrors the type returned by the current call site at
    /// `base/watcher_runtime.rs::maybe_start_transcript`
    /// (`Option<PathBuf>`), so that a future step can swap the
    /// `pty_state.get_cwd(...).map(PathBuf::from)` expression for
    /// `runtime.live_cwd()` without touching the caller's downstream
    /// `TranscriptState::start_or_replace(... cwd ...)` signature.
    pub(crate) fn live_cwd(&self) -> Option<PathBuf> {
        self.pty_state.get_cwd(&self.session_id).map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: AttachContext is constructable with the full field set
    /// and field access works as expected. The semantic invariants
    /// (`agent_pid != shell_pid`, `initial_cwd` originates from PtyState,
    /// `provider_home` resolves from the central registry, etc.) are
    /// exercised by `resolve_bind_inputs_*` tests in `adapter::mod`'s
    /// `noop_tests` module.
    #[test]
    fn attach_context_holds_all_attach_facts() {
        let attach = AttachContext {
            session_id: "sid-1".to_string(),
            initial_cwd: PathBuf::from("/workspace"),
            shell_pid: 100,
            agent_pid: 200,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::ClaudeCode,
            app_data_dir: PathBuf::from("/home/u/.config/vimeflow"),
            provider_home: Some(PathBuf::from("/home/u/.claude")),
            provider_home_override: None,
            proc_root: Some(PathBuf::from("/proc")),
        };

        assert_eq!(attach.session_id, "sid-1");
        assert_eq!(attach.initial_cwd, PathBuf::from("/workspace"));
        assert_eq!(attach.shell_pid, 100);
        assert_eq!(attach.agent_pid, 200);
        assert_ne!(attach.shell_pid, attach.agent_pid);
        assert_eq!(attach.pty_start, SystemTime::UNIX_EPOCH);
        assert_eq!(attach.agent_type, AgentType::ClaudeCode);
        assert_eq!(
            attach.app_data_dir,
            PathBuf::from("/home/u/.config/vimeflow")
        );
        assert_eq!(attach.provider_home, Some(PathBuf::from("/home/u/.claude")));
        assert_eq!(attach.proc_root, Some(PathBuf::from("/proc")));
    }

    /// Clone is needed because the attach facts get cloned into the
    /// `tokio::task::spawn_blocking` closure in
    /// `start_agent_watcher_inner`.
    #[test]
    fn attach_context_is_clone() {
        let original = AttachContext {
            session_id: "sid".to_string(),
            initial_cwd: PathBuf::from("/ws"),
            shell_pid: 1,
            agent_pid: 2,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::Codex,
            app_data_dir: PathBuf::from("/tmp/vimeflow-data"),
            provider_home: Some(PathBuf::from(".codex")),
            provider_home_override: None,
            proc_root: None,
        };

        let cloned = original.clone();
        assert_eq!(cloned.session_id, original.session_id);
        assert_eq!(cloned.agent_pid, original.agent_pid);
        assert_eq!(cloned.app_data_dir, original.app_data_dir);
        assert_eq!(cloned.provider_home, original.provider_home);
        assert_eq!(cloned.proc_root, original.proc_root);
    }

    /// `agent_type` is now `Copy` so callers don't need redundant
    /// `.clone()` when passing it to `for_attach`. Pin the invariant so
    /// a future contributor doesn't accidentally remove `Copy` from the
    /// `AgentType` derive list at `crate::agent::types`.
    #[test]
    fn agent_type_is_copy() {
        let attach = AttachContext {
            session_id: "sid".to_string(),
            initial_cwd: PathBuf::from("/ws"),
            shell_pid: 1,
            agent_pid: 2,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::Codex,
            app_data_dir: PathBuf::from("/tmp/vimeflow-data"),
            provider_home: None,
            provider_home_override: None,
            proc_root: None,
        };
        // If AgentType lost Copy, this would consume attach.agent_type.
        let first = attach.agent_type;
        let second = attach.agent_type;
        assert_eq!(first, second);
    }

    /// `SessionRuntimeContext::session_id` round-trips through the
    /// constructor. Data-shape only; the live-cwd semantic test (plus
    /// the cross-clone shared-state assertion) lives in
    /// `adapter::mod::noop_tests` where `make_test_session` builds a
    /// real PTY-backed `ManagedSession`.
    #[test]
    fn session_runtime_context_remembers_session_id() {
        let runtime = SessionRuntimeContext::new("sid-runtime".to_string(), PtyState::new());
        assert_eq!(runtime.session_id(), "sid-runtime");
    }

    /// On an empty `PtyState`, `live_cwd` returns `None` rather than
    /// panicking or constructing an empty `PathBuf`. Pins the
    /// "session-gone" branch — exercised in production when the PTY is
    /// killed between the transcript-tail thread queuing a path
    /// re-check and `PtyState` actually being read.
    #[test]
    fn session_runtime_context_live_cwd_is_none_for_missing_session() {
        let runtime = SessionRuntimeContext::new("sid-missing".to_string(), PtyState::new());
        assert_eq!(runtime.live_cwd(), None);
    }

    /// `Clone` impl exists and produces an identically-shaped handle:
    /// same session id, same `None` cwd on an empty `PtyState`. Two
    /// independent empty states would pass these assertions too, so
    /// this is a smoke test, NOT a proof that clones share the
    /// underlying `Arc`-backed `PtyState`. The shared-state invariant
    /// is asserted by
    /// `session_runtime_context_clone_shares_pty_state` in
    /// `adapter::mod::noop_tests`, where the PTY-backed test helper
    /// `make_test_session` lives.
    #[test]
    fn session_runtime_context_is_clone() {
        let original = SessionRuntimeContext::new("sid-clone".to_string(), PtyState::new());
        let cloned = original.clone();
        assert_eq!(cloned.session_id(), original.session_id());
        assert_eq!(cloned.live_cwd(), original.live_cwd());
    }
}
