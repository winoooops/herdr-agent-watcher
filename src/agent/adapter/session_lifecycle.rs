//! `SessionLifecycle` — the registry facade for agent-watcher
//! lifecycle.
//!
//! Step D' of the v4-frozen refactor plan (#246). Owns clones of the
//! four runtime collaborators (`PtyState`, `AgentWatcherState`,
//! `TranscriptState`, `Arc<dyn EventSink>`) and exposes the two
//! lifecycle verbs the IPC layer needs:
//!
//! - `start(session_id, app_data_dir)` — resolve the [`AttachContext`] from
//!   live `PtyState`, build the typed [`AgentBindings`], and run the verb
//!   sequence on the blocking pool.
//! - `stop(session_id)` — remove the session's watcher from
//!   `AgentWatcherState` (its `Drop` cascades the transcript-tail
//!   teardown).
//!
//! **`AttachError` → `String` boundary.** Per #246's D' acceptance and
//! frozen constraint #4, the typed `AttachError` from
//! `AgentBindings::for_attach` is collapsed to `String` *here* — the
//! post-bindings verb sequence (`run_watch_sequence`) and everything
//! below it speaks `String` errors. This is the single mapping seam;
//! `start_agent_watcher_inner` delegates to this method rather than
//! mapping independently.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::bindings::AgentBindings;
use super::kimi::{KIMI_BIND_RETRY_INTERVAL_MS, KIMI_BIND_RETRY_MAX_ATTEMPTS};
use super::{base, resolve_bind_inputs, AttachContext};
use crate::agent::adapter::types::LocatedStatusSource;
use crate::agent::detector::detect_agent;
use crate::agent::types::AgentType;
use crate::runtime::EventSink;
use crate::terminal::types::SessionId;
use crate::terminal::PtyState;
use base::{AgentWatcherState, TranscriptState, TrustedLocatedSource, WatcherHandle};

/// Registry facade owning clones of the four runtime collaborators.
///
/// Constructed per IPC call from `BackendState`'s long-lived
/// originals (all four are cheap `Arc`-backed clones). `stop` only
/// reads `watcher_state`, but the service carries all four so the
/// facade shape is uniform and a future eager-prevalidation `stop`
/// (or a `restart`) has the inputs it needs without a signature
/// change.
#[derive(Clone)]
pub(crate) struct SessionLifecycle {
    pty_state: PtyState,
    watcher_state: AgentWatcherState,
    transcript_state: TranscriptState,
    events: Arc<dyn EventSink>,
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::SystemTime;

    use super::{AgentBindings, AttachContext, SessionLifecycle};
    use crate::agent::adapter::base::{AgentWatcherState, TranscriptState, WatcherHandle};
    use crate::agent::adapter::make_test_session;
    use crate::agent::adapter::traits::StatusSourceLocator;
    use crate::agent::adapter::types::LocatedStatusSource;
    use crate::agent::types::AgentType;
    use crate::runtime::EventSink;
    use crate::runtime::FakeEventSink;
    use crate::terminal::PtyState;
    use tempfile::TempDir;

    fn make_attach_ctx(cwd: &std::path::Path) -> AttachContext {
        AttachContext {
            session_id: "test-sess".to_string(),
            initial_cwd: cwd.to_path_buf(),
            shell_pid: 1,
            agent_pid: 2,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::ClaudeCode,
            app_data_dir: cwd.join("vimeflow-data"),
            provider_home: Some(PathBuf::from("/home/u/.claude")),
            provider_home_override: None,
            proc_root: None,
        }
    }

    fn write_claude_status(app_data_dir: &Path, cwd: &std::path::Path, sid: &str) {
        let dir =
            crate::agent::adapter::claude_code::bridge::session_bridge_dir(app_data_dir, cwd, sid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("status.json"),
            r#"{"session_id":"sid","model":{}}"#,
        )
        .unwrap();
    }

    fn seeded_fixture(sid: &str) -> (tempfile::TempDir, TranscriptState, AgentWatcherState) {
        let sink = Arc::new(FakeEventSink::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript_path = tmp.path().join("t.jsonl");
        std::fs::write(&transcript_path, "").expect("write");
        let transcript_state = TranscriptState::new();
        let adapter: Arc<dyn crate::agent::adapter::traits::TranscriptStreamer> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);
        transcript_state
            .start_or_replace(
                adapter,
                sink,
                sid.to_string(),
                transcript_path,
                None,
                None,
                None,
            )
            .expect("seed transcript state");
        let watcher_state = AgentWatcherState::new();
        let seed = WatcherHandle::new_for_test(transcript_state.clone(), sid.to_string());
        watcher_state.insert(sid.to_string(), seed, AgentType::ClaudeCode);
        // Return `tmp` so the caller owns its lifetime — dropping the TempDir
        // here would delete the seeded transcript file before the test body
        // runs (a latent trap for any future test that reads it).
        (tmp, transcript_state, watcher_state)
    }

    struct OutsideTrustLocator;

    impl StatusSourceLocator for OutsideTrustLocator {
        fn locate(
            &self,
            cwd: &std::path::Path,
            _session_id: &str,
        ) -> Result<LocatedStatusSource, String> {
            let parent = cwd.parent().unwrap_or(cwd);
            Ok(LocatedStatusSource {
                status_path: parent.join("sibling.json"),
                trust_root: cwd.to_path_buf(),
                static_transcript_hint: None,
                agent_session_id: None,
                resolved_directory: None,
            })
        }
    }

    fn _assert_send_sync_static<T: Send + Sync + 'static>() {}

    #[test]
    fn t_lifecycle_4_agent_bindings_is_send_sync_static() {
        _assert_send_sync_static::<AgentBindings>();
    }

    #[test]
    fn t_verb_resolve_attach() {
        let app_data = TempDir::new().unwrap();
        let pty_state = PtyState::new();
        let sid = "sid-verb-resolve".to_string();
        pty_state
            .try_insert(sid.clone(), make_test_session(), 64)
            .unwrap_or_else(|_| panic!("insert session"));

        let lifecycle = SessionLifecycle::new(
            pty_state,
            AgentWatcherState::new(),
            TranscriptState::new(),
            Arc::new(FakeEventSink::new()),
        );

        let attach = lifecycle
            .resolve_attach(&sid, app_data.path(), None, |_pid| {
                Some((AgentType::Codex, 4242))
            })
            .expect("resolve_attach");

        // resolve_attach is a thin delegate to resolve_bind_inputs; this test
        // proves only the wiring — the right session plus the injected
        // detector's (agent_type, pid) flowing through. The exhaustive
        // field→value map is already pinned by
        // resolve_bind_inputs_populates_attach_context_fields, so re-asserting
        // every field here would just duplicate that coverage.
        assert_eq!(attach.session_id, sid);
        assert_eq!(attach.agent_type, AgentType::Codex);
        assert_eq!(attach.agent_pid, 4242);
        assert_eq!(attach.app_data_dir, app_data.path());
    }

    #[test]
    fn t_verb_bind_services() {
        let ctx = AttachContext {
            session_id: "pty-codex".to_string(),
            initial_cwd: PathBuf::from("/tmp/ws"),
            shell_pid: 1,
            agent_pid: 12345,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::Codex,
            app_data_dir: PathBuf::from("/tmp/vimeflow-data"),
            provider_home: Some(PathBuf::from("/home/u/.codex")),
            provider_home_override: None,
            proc_root: None,
        };

        let lifecycle = SessionLifecycle::new(
            PtyState::new(),
            AgentWatcherState::new(),
            TranscriptState::new(),
            Arc::new(FakeEventSink::new()),
        );

        let bindings = lifecycle.bind_services(&ctx).expect("bind_services");
        assert!(matches!(bindings.agent_type, AgentType::Codex));
    }

    #[test]
    fn t_verb_locate_happy_path() {
        let tmp = TempDir::new().unwrap();
        let app_data = TempDir::new().unwrap();
        let sid = "test-sess".to_string();
        let ctx = AttachContext {
            session_id: sid.clone(),
            initial_cwd: tmp.path().to_path_buf(),
            shell_pid: 1,
            agent_pid: 2,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::ClaudeCode,
            app_data_dir: app_data.path().to_path_buf(),
            provider_home: Some(PathBuf::from("/home/u/.claude")),
            provider_home_override: None,
            proc_root: None,
        };
        let bindings = AgentBindings::for_attach(&ctx).expect("for_attach");
        let lifecycle = SessionLifecycle::new(
            PtyState::new(),
            AgentWatcherState::new(),
            TranscriptState::new(),
            Arc::new(FakeEventSink::new()),
        );
        let located = lifecycle
            .locate(&bindings, tmp.path(), &sid)
            .expect("locate happy path");
        assert_eq!(
            located.status_path,
            crate::agent::adapter::claude_code::bridge::session_status_file(
                app_data.path(),
                tmp.path(),
                &sid,
            )
        );
        assert_eq!(located.trust_root, app_data.path());
    }

    #[test]
    #[ignore = "live diagnostic: needs a running kimi; KIMI_LIVE_PID=<pid> cargo test --lib kimi_live_attach_diag -- --ignored --nocapture"]
    fn kimi_live_attach_diag() {
        let agent_pid: u32 = std::env::var("KIMI_LIVE_PID")
            .expect("set KIMI_LIVE_PID")
            .parse()
            .expect("KIMI_LIVE_PID must be u32");
        let stale_cwd =
            std::env::var("KIMI_STALE_CWD").unwrap_or_else(|_| "/home/will".to_string());
        if let Ok(shell_pid) = std::env::var("KIMI_SHELL_PID") {
            let sp: u32 = shell_pid.parse().expect("KIMI_SHELL_PID must be u32");
            eprintln!(
                "[diag] detect_agent(shell_pid={}) = {:?}",
                sp,
                crate::agent::detector::detect_agent(sp)
            );
        }
        let sid = "live-diag".to_string();
        let spec = crate::agent::config::spec_for(AgentType::Kimi);
        let ctx = AttachContext {
            session_id: sid.clone(),
            initial_cwd: PathBuf::from(&stale_cwd),
            shell_pid: 1,
            agent_pid,
            pty_start: SystemTime::now(),
            agent_type: AgentType::Kimi,
            app_data_dir: PathBuf::from("/tmp/vimeflow-data"),
            provider_home: spec.provider_home(),
            provider_home_override: None,
            proc_root: crate::agent::config::default_proc_root(),
        };
        eprintln!(
            "[diag] ctx agent_pid={} stale_cwd={} provider_home={:?} proc_root={:?}",
            agent_pid, stale_cwd, ctx.provider_home, ctx.proc_root
        );
        let bindings = AgentBindings::for_attach(&ctx).expect("for_attach kimi");
        eprintln!("[diag] for_attach OK agent_type={:?}", bindings.agent_type);
        match bindings
            .locator
            .locate(PathBuf::from(&stale_cwd).as_path(), &sid)
        {
            Ok(located) => {
                eprintln!(
                    "[diag] LOCATE OK status_path={} trust_root={}",
                    located.status_path.display(),
                    located.trust_root.display()
                );
                let contents = std::fs::read_to_string(&located.status_path)
                    .expect("read located status_path");
                eprintln!("[diag] wire bytes={}", contents.len());
                match bindings.decoder.decode(Some(&sid), &contents) {
                    Ok(snap) => eprintln!("[diag] DECODE OK {:?}", snap),
                    Err(e) => eprintln!("[diag] DECODE ERR {}", e),
                }
            }
            Err(e) => eprintln!("[diag] LOCATE ERR {}", e),
        }
    }

    struct ErrLocator;

    impl StatusSourceLocator for ErrLocator {
        fn locate(
            &self,
            _cwd: &std::path::Path,
            _session_id: &str,
        ) -> Result<LocatedStatusSource, String> {
            Err("locate failed".to_string())
        }
    }

    /// Locator that resolves a caller-chosen status path under a trust root,
    /// for exercising the Codex no-op relocate (idempotency) branch.
    struct StubPathLocator {
        status_path: PathBuf,
        trust_root: PathBuf,
    }

    impl StatusSourceLocator for StubPathLocator {
        fn locate(
            &self,
            _cwd: &std::path::Path,
            _session_id: &str,
        ) -> Result<LocatedStatusSource, String> {
            Ok(LocatedStatusSource {
                status_path: self.status_path.clone(),
                trust_root: self.trust_root.clone(),
                static_transcript_hint: None,
                agent_session_id: None,
                resolved_directory: None,
            })
        }
    }

    fn write_stub_status(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("status path has parent"))
            .expect("create status dir");
        std::fs::write(path, r#"{"session_id":"sid","model":{}}"#).expect("write status");
    }

    fn stub_bindings(
        agent_type: AgentType,
        status_path: PathBuf,
        trust_root: PathBuf,
    ) -> AgentBindings {
        let adapter: Arc<crate::agent::adapter::claude_code::ClaudeCodeAdapter> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);
        AgentBindings {
            agent_type,
            locator: Arc::new(StubPathLocator {
                status_path,
                trust_root,
            }),
            decoder: adapter.clone(),
            transcript_paths: adapter.clone(),
            validator: adapter.clone(),
            streamer: adapter,
        }
    }

    #[test]
    fn t_verb_locate_err_forwarding() {
        let adapter: Arc<crate::agent::adapter::claude_code::ClaudeCodeAdapter> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);
        let bindings = AgentBindings {
            agent_type: AgentType::ClaudeCode,
            locator: Arc::new(ErrLocator),
            decoder: adapter.clone(),
            transcript_paths: adapter.clone(),
            validator: adapter.clone(),
            streamer: adapter,
        };
        let lifecycle = SessionLifecycle::new(
            PtyState::new(),
            AgentWatcherState::new(),
            TranscriptState::new(),
            Arc::new(FakeEventSink::new()),
        );
        let result = lifecycle.locate(&bindings, std::path::Path::new("/tmp"), "sid");
        assert_eq!(result.unwrap_err(), "locate failed");
    }

    #[test]
    fn t_verb_ensure_trust() {
        let tmp = TempDir::new().unwrap();

        // Under trust root -> Ok
        let under_status_path = tmp
            .path()
            .join(".vimeflow")
            .join("sessions")
            .join("s")
            .join("status.json");
        let under = LocatedStatusSource {
            status_path: under_status_path.clone(),
            trust_root: tmp.path().to_path_buf(),
            static_transcript_hint: None,
            agent_session_id: None,
            resolved_directory: None,
        };
        let lifecycle = SessionLifecycle::new(
            PtyState::new(),
            AgentWatcherState::new(),
            TranscriptState::new(),
            Arc::new(FakeEventSink::new()),
        );
        let trusted = lifecycle.ensure_trust(under).expect("under trust root");
        assert_eq!(trusted.status_path(), under_status_path);

        // Outside trust root -> Err
        let outside = LocatedStatusSource {
            status_path: tmp.path().parent().unwrap().join("sibling.json"),
            trust_root: tmp.path().to_path_buf(),
            static_transcript_hint: None,
            agent_session_id: None,
            resolved_directory: None,
        };
        let result = lifecycle.ensure_trust(outside);
        assert!(result.is_err(), "outside trust root should fail");
    }

    #[test]
    fn t_verb_evict_old() {
        let state = AgentWatcherState::new();
        let transcript_state = TranscriptState::new();
        let sid = "test-sid".to_string();
        let handle = WatcherHandle::new_for_test(transcript_state, sid.clone());
        state.insert(sid.clone(), handle, AgentType::ClaudeCode);
        assert!(state.contains(&sid));

        let lifecycle = SessionLifecycle::new(
            PtyState::new(),
            state.clone(),
            TranscriptState::new(),
            Arc::new(FakeEventSink::new()),
        );
        lifecycle.evict_old(&sid);
        assert!(!state.contains(&sid), "evict_old should remove the session");

        // Idempotent on empty/absent registry — must not panic
        lifecycle.evict_old("nonexistent");
    }

    #[test]
    fn t_verb_spawn_watch() {
        let tmp = TempDir::new().unwrap();
        let app_data = TempDir::new().unwrap();
        let sid = "test-sess".to_string();
        let status_path = crate::agent::adapter::claude_code::bridge::session_status_file(
            app_data.path(),
            tmp.path(),
            &sid,
        );
        std::fs::create_dir_all(status_path.parent().unwrap()).unwrap();
        std::fs::write(&status_path, r#"{"session_id":"sid","model":{}}"#).unwrap();

        let ctx = AttachContext {
            session_id: sid.clone(),
            initial_cwd: tmp.path().to_path_buf(),
            shell_pid: 1,
            agent_pid: 2,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::ClaudeCode,
            app_data_dir: app_data.path().to_path_buf(),
            provider_home: Some(PathBuf::from("/home/u/.claude")),
            provider_home_override: None,
            proc_root: None,
        };
        let bindings = AgentBindings::for_attach(&ctx).expect("for_attach");
        let located = LocatedStatusSource {
            status_path,
            trust_root: app_data.path().to_path_buf(),
            static_transcript_hint: None,
            agent_session_id: None,
            resolved_directory: None,
        };

        let lifecycle = SessionLifecycle::new(
            PtyState::new(),
            AgentWatcherState::new(),
            TranscriptState::new(),
            Arc::new(FakeEventSink::new()),
        );

        let trusted = lifecycle
            .ensure_trust(located)
            .expect("ensure_trust should pass");
        let handle = lifecycle
            .spawn_watch(bindings, trusted, sid.clone())
            .expect("spawn_watch should return a handle");
        // Drop the handle to join the spawned thread
        drop(handle);
    }

    #[test]
    fn t_verb_register() {
        let state = AgentWatcherState::new();
        let transcript_state = TranscriptState::new();
        let sid = "test-sid".to_string();
        let handle = WatcherHandle::new_for_test(transcript_state, sid.clone());

        let lifecycle = SessionLifecycle::new(
            PtyState::new(),
            state.clone(),
            TranscriptState::new(),
            Arc::new(FakeEventSink::new()),
        );
        lifecycle.register(sid.clone(), handle, AgentType::ClaudeCode);
        assert!(state.contains(&sid), "register should insert the session");
    }

    #[tokio::test]
    async fn t_verb_stop_empty_registry_returns_err() {
        let lifecycle = SessionLifecycle::new(
            PtyState::new(),
            AgentWatcherState::new(),
            TranscriptState::new(),
            Arc::new(FakeEventSink::new()),
        );
        let result = lifecycle.stop("nonexistent".to_string()).await;
        assert_eq!(
            result.unwrap_err(),
            "No active watcher for session: nonexistent"
        );
    }

    #[tokio::test]
    async fn t_lifecycle_1_start_inner_for_test_happy_path_registers_session() {
        let tmp = TempDir::new().unwrap();
        let sid = "test-sess".to_string();

        let ctx = make_attach_ctx(tmp.path());
        write_claude_status(&ctx.app_data_dir, tmp.path(), &sid);
        let bindings = AgentBindings::for_attach(&ctx).unwrap();
        let events: Arc<dyn EventSink> = Arc::new(FakeEventSink::new());
        let pty_state = PtyState::new();
        let transcript_state = TranscriptState::new();
        let watcher_state = AgentWatcherState::new();

        let lifecycle =
            SessionLifecycle::new(pty_state, watcher_state.clone(), transcript_state, events);
        lifecycle
            .start_inner_for_test(sid.clone(), bindings, tmp.path().to_path_buf())
            .await
            .expect("start_inner_for_test happy path");
        assert!(watcher_state.contains(&sid), "session registered");

        watcher_state.remove(&sid);
    }

    #[tokio::test]
    async fn t_lifecycle_2a_start_inner_for_test_locate_err_preserves_existing_watcher() {
        let sid = "test-sess".to_string();
        let (_tmp, transcript_state, watcher_state) = seeded_fixture(&sid);

        let adapter: Arc<crate::agent::adapter::claude_code::ClaudeCodeAdapter> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);
        let bindings = AgentBindings {
            agent_type: AgentType::ClaudeCode,
            locator: Arc::new(ErrLocator),
            decoder: adapter.clone(),
            transcript_paths: adapter.clone(),
            validator: adapter.clone(),
            streamer: adapter,
        };
        let events: Arc<dyn EventSink> = Arc::new(FakeEventSink::new());
        let pty_state = PtyState::new();

        let lifecycle = SessionLifecycle::new(
            pty_state,
            watcher_state.clone(),
            transcript_state.clone(),
            events,
        );
        let result = lifecycle
            .start_inner_for_test(sid.clone(), bindings, PathBuf::from("/tmp"))
            .await;

        assert!(result.is_err(), "locate failure should return Err");
        assert!(
            watcher_state.contains(&sid),
            "existing watcher should be preserved"
        );
        assert!(
            transcript_state.contains(&sid),
            "transcript state entry should be preserved"
        );

        watcher_state.remove(&sid);
    }

    #[tokio::test]
    async fn t_lifecycle_2b_start_inner_for_test_trust_err_preserves_existing_watcher() {
        let sid = "test-sess".to_string();
        let (_tmp, transcript_state, watcher_state) = seeded_fixture(&sid);

        let adapter: Arc<crate::agent::adapter::claude_code::ClaudeCodeAdapter> =
            Arc::new(crate::agent::adapter::claude_code::ClaudeCodeAdapter);
        let bindings = AgentBindings {
            agent_type: AgentType::ClaudeCode,
            locator: Arc::new(OutsideTrustLocator),
            decoder: adapter.clone(),
            transcript_paths: adapter.clone(),
            validator: adapter.clone(),
            streamer: adapter,
        };
        let events: Arc<dyn EventSink> = Arc::new(FakeEventSink::new());
        let pty_state = PtyState::new();

        let lifecycle = SessionLifecycle::new(
            pty_state,
            watcher_state.clone(),
            transcript_state.clone(),
            events,
        );
        let result = lifecycle
            .start_inner_for_test(sid.clone(), bindings, PathBuf::from("/tmp"))
            .await;

        assert!(result.is_err(), "trust failure should return Err");
        assert!(
            watcher_state.contains(&sid),
            "existing watcher should be preserved"
        );
        assert!(
            transcript_state.contains(&sid),
            "transcript state entry should be preserved"
        );

        watcher_state.remove(&sid);
    }

    #[tokio::test]
    async fn codex_relocate_same_rollout_is_noop_changed_false() {
        // Drift tick re-locates the active Codex pane every few seconds. When
        // the locate resolves the SAME rollout already watched, the relocate
        // must be a no-op (changed=false) so it neither re-tails the (large)
        // rollout nor lets the frontend optimistic-clear red (VIM-192).
        let tmp = TempDir::new().expect("tempdir");
        let trust_root = tmp.path().to_path_buf();
        let status_path = trust_root.join("sessions").join("a").join("status.json");
        write_stub_status(&status_path);
        let sid = "codex-sess".to_string();
        let watcher_state = AgentWatcherState::new();
        let transcript_state = TranscriptState::new();

        let lifecycle = SessionLifecycle::new(
            PtyState::new(),
            watcher_state.clone(),
            transcript_state,
            Arc::new(FakeEventSink::new()),
        );

        // First attach: a real relocate.
        let changed = lifecycle
            .start_inner_for_test(
                sid.clone(),
                stub_bindings(AgentType::Codex, status_path.clone(), trust_root.clone()),
                tmp.path().to_path_buf(),
            )
            .await
            .expect("first codex attach");
        assert!(changed, "first attach must report changed=true");
        assert_eq!(
            watcher_state.current_status_path(&sid).as_deref(),
            Some(status_path.as_path()),
            "handle records the rollout it watches"
        );

        // Second attach, same rollout: idempotent no-op.
        let changed_again = lifecycle
            .start_inner_for_test(
                sid.clone(),
                stub_bindings(AgentType::Codex, status_path.clone(), trust_root.clone()),
                tmp.path().to_path_buf(),
            )
            .await
            .expect("second codex attach (same rollout)");
        assert!(
            !changed_again,
            "re-locating the SAME rollout must be a no-op (changed=false)"
        );
        assert_eq!(
            watcher_state.current_status_path(&sid).as_deref(),
            Some(status_path.as_path()),
            "still watching the same rollout"
        );

        watcher_state.remove(&sid);
    }

    #[tokio::test]
    async fn codex_relocate_changed_rollout_respawns_changed_true() {
        // A genuine switch (resume -> different rollout becomes newest) must
        // relocate and report changed=true so the panel follows it.
        let tmp = TempDir::new().expect("tempdir");
        let trust_root = tmp.path().to_path_buf();
        let path1 = trust_root.join("sessions").join("a").join("status.json");
        let path2 = trust_root.join("sessions").join("b").join("status.json");
        write_stub_status(&path1);
        write_stub_status(&path2);
        let sid = "codex-sess".to_string();
        let watcher_state = AgentWatcherState::new();

        let lifecycle = SessionLifecycle::new(
            PtyState::new(),
            watcher_state.clone(),
            TranscriptState::new(),
            Arc::new(FakeEventSink::new()),
        );

        lifecycle
            .start_inner_for_test(
                sid.clone(),
                stub_bindings(AgentType::Codex, path1.clone(), trust_root.clone()),
                tmp.path().to_path_buf(),
            )
            .await
            .expect("attach path1");

        let changed = lifecycle
            .start_inner_for_test(
                sid.clone(),
                stub_bindings(AgentType::Codex, path2.clone(), trust_root.clone()),
                tmp.path().to_path_buf(),
            )
            .await
            .expect("relocate to path2");
        assert!(changed, "switching rollout must report changed=true");
        assert_eq!(
            watcher_state.current_status_path(&sid).as_deref(),
            Some(path2.as_path()),
            "now watching the new rollout"
        );

        watcher_state.remove(&sid);
    }

    #[tokio::test]
    async fn non_codex_relocate_always_respawns_even_for_same_path() {
        // The no-op is Codex-scoped: Claude/Kimi keep their always-respawn
        // restart semantics, so a same-path re-locate still reports changed=true.
        let tmp = TempDir::new().expect("tempdir");
        let trust_root = tmp.path().to_path_buf();
        let status_path = trust_root.join("sessions").join("a").join("status.json");
        write_stub_status(&status_path);
        let sid = "claude-sess".to_string();
        let watcher_state = AgentWatcherState::new();

        let lifecycle = SessionLifecycle::new(
            PtyState::new(),
            watcher_state.clone(),
            TranscriptState::new(),
            Arc::new(FakeEventSink::new()),
        );

        lifecycle
            .start_inner_for_test(
                sid.clone(),
                stub_bindings(
                    AgentType::ClaudeCode,
                    status_path.clone(),
                    trust_root.clone(),
                ),
                tmp.path().to_path_buf(),
            )
            .await
            .expect("first claude attach");
        let changed_again = lifecycle
            .start_inner_for_test(
                sid.clone(),
                stub_bindings(
                    AgentType::ClaudeCode,
                    status_path.clone(),
                    trust_root.clone(),
                ),
                tmp.path().to_path_buf(),
            )
            .await
            .expect("second claude attach");
        assert!(
            changed_again,
            "non-Codex agents always respawn (changed=true) even for the same path"
        );

        watcher_state.remove(&sid);
    }
}

impl SessionLifecycle {
    pub(crate) fn new(
        pty_state: PtyState,
        watcher_state: AgentWatcherState,
        transcript_state: TranscriptState,
        events: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            pty_state,
            watcher_state,
            transcript_state,
            events,
        }
    }

    fn resolve_attach<F>(
        &self,
        sid: &SessionId,
        app_data_dir: &Path,
        provider_home_override: Option<PathBuf>,
        detect: F,
    ) -> Result<AttachContext, String>
    where
        F: FnOnce(u32) -> Option<(AgentType, u32)>,
    {
        resolve_bind_inputs(
            &self.pty_state,
            app_data_dir,
            sid,
            provider_home_override,
            detect,
        )
    }

    fn bind_services(&self, ctx: &AttachContext) -> Result<AgentBindings, String> {
        AgentBindings::for_attach(ctx).map_err(|e| format!("agent bindings: {}", e))
    }

    fn locate(
        &self,
        bindings: &AgentBindings,
        cwd: &Path,
        sid: &str,
    ) -> Result<LocatedStatusSource, String> {
        bindings.locator.locate(cwd, sid)
    }

    /// Async wrapper around [`Self::locate`].
    ///
    /// All locator work is dispatched through `tokio::task::spawn_blocking`
    /// because `StatusSourceLocator::locate` performs filesystem I/O. For
    /// kimi, the retry loop lives here (outside the blocking closure) so the
    /// inter-attempt delay is `tokio::time::sleep`, which yields the async
    /// task instead of parking a blocking-pool thread (PR #447 review F1).
    /// Other agents either have their own internal retry inside `locate`
    /// (codex) or are infallible (claude), so they get a single blocking
    /// locate call.
    async fn locate_async(
        &self,
        bindings: &AgentBindings,
        cwd: &Path,
        sid: &str,
    ) -> Result<LocatedStatusSource, String> {
        let lc = self.clone();
        let bindings = bindings.clone();
        let cwd = cwd.to_path_buf();
        let sid = sid.to_string();

        if bindings.agent_type != AgentType::Kimi {
            return tokio::task::spawn_blocking(move || lc.locate(&bindings, &cwd, &sid))
                .await
                .map_err(|e| format!("locate task panicked: {}", e))?;
        }

        let mut last_err = String::from("kimi locate retry exhausted");
        for attempt in 0..KIMI_BIND_RETRY_MAX_ATTEMPTS {
            let lc = lc.clone();
            let bindings = bindings.clone();
            let cwd = cwd.clone();
            let sid = sid.clone();
            match tokio::task::spawn_blocking(move || lc.locate(&bindings, &cwd, &sid))
                .await
                .map_err(|e| format!("locate task panicked: {}", e))?
            {
                Ok(located) => return Ok(located),
                Err(e) => {
                    last_err = e;
                    if attempt + 1 < KIMI_BIND_RETRY_MAX_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(KIMI_BIND_RETRY_INTERVAL_MS))
                            .await;
                    }
                }
            }
        }
        Err(last_err)
    }

    fn ensure_trust(&self, located: LocatedStatusSource) -> Result<TrustedLocatedSource, String> {
        base::ensure_trusted(located)
    }

    /// One of the seven F.4-decomposition verbs, but no longer called
    /// by `run_watch_sequence` after PR #302 cycle 3 — `register` (via
    /// `AgentWatcherState::insert`) does the atomic replace by itself
    /// without the ~3.5s session-absent window that a separate evict
    /// step introduced.
    ///
    /// **Footgun guard:** marked `#[cfg(test)]` so the verb only
    /// exists in test builds (PR #302 cycle 11 F4). In production
    /// builds the method doesn't exist, so a future contributor
    /// browsing autocomplete can't accidentally reintroduce the race
    /// the `start_or_replace` / `register` path was carefully designed
    /// to avoid. The `t_verb_evict_old` test still pins the standalone
    /// semantics under `cfg(test)`, satisfying the F.4 spec mapping in
    /// `docs/superpowers/specs/2026-05-25-transcript-dtos-and-engine-design.md`
    /// without exposing the verb as a production API. If a genuine
    /// "evict without replace" use case appears (e.g. a hard reset
    /// before a new spawn that lives in a different task), revert
    /// this gate AND add a docstring spelling out the rename /
    /// title-sync IPC contract the caller must handle.
    #[cfg(test)]
    fn evict_old(&self, sid: &str) {
        self.watcher_state.remove(sid);
    }

    fn spawn_watch(
        &self,
        bindings: AgentBindings,
        located: TrustedLocatedSource,
        sid: String,
    ) -> Result<WatcherHandle, String> {
        // PR #302 cycle 16 retry-1: the `pre_inline_init` closure
        // quiesces any displaced predecessor watcher AFTER notify
        // setup succeeds (preserving spawn-failure rollback) and
        // BEFORE inline-init runs (closing the codex P2 race). See
        // `start_watching`'s docstring for the full ordering proof.
        let watcher_state = self.watcher_state.clone();
        let transcript_state_for_quiesce = self.transcript_state.clone();
        let sid_for_quiesce = sid.clone();
        base::start_watching(
            bindings,
            self.events.clone(),
            self.pty_state.clone(),
            self.transcript_state.clone(),
            sid,
            located,
            move || {
                watcher_state.quiesce_existing(&sid_for_quiesce, &transcript_state_for_quiesce);
            },
        )
    }

    fn register(&self, sid: String, handle: WatcherHandle, agent_type: AgentType) {
        self.watcher_state.insert(sid, handle, agent_type);
    }

    /// Start (or restart) the agent watcher for `session_id`.
    ///
    /// Resolves the attach inputs from live `PtyState`, builds the
    /// `AgentBindings`, then runs the post-bindings verb sequence
    /// (`locate → ensure_trust → spawn_watch → register`) on the blocking
    /// pool via [`Self::run_watch_sequence`]. The `register` step calls
    /// `AgentWatcherState::insert`, which atomically swaps the old handle
    /// for the new one under a single lock — there is no separate `evict`
    /// step (PR #302 cycle 3 — cycle 2 had inserted an `evict_old` here
    /// that opened a ~3.5s session-absent window during the displaced
    /// handle's thread-join inside `remove`; deleted in this cycle because
    /// `insert`'s own `_displaced` drop already handles atomic replace).
    /// `AttachError` is mapped to `String` at this boundary.
    pub(crate) async fn start(
        &self,
        session_id: String,
        app_data_dir: PathBuf,
        provider_home_override: Option<PathBuf>,
    ) -> Result<bool, String> {
        crate::debug::debug_log("agent-attach", &format!("start session={}", session_id));
        let attach = match self.resolve_attach(
            &session_id,
            &app_data_dir,
            provider_home_override,
            |shell_pid| {
                let detected = detect_agent(shell_pid);

                #[cfg(feature = "e2e-test")]
                {
                    // E2E tests seed the watcher map instead of launching a real
                    // Claude/Codex process, but still exercise the normal watcher
                    // startup and status-file emission path.
                    detected.or_else(|| {
                        self.watcher_state
                            .agent_type_for_pty(&session_id)
                            .map(|agent_type| (agent_type, shell_pid))
                    })
                }

                #[cfg(not(feature = "e2e-test"))]
                {
                    detected
                }
            },
        ) {
            Ok(attach) => attach,
            Err(e) => {
                crate::debug::debug_log(
                    "agent-attach",
                    &format!("resolve_attach ERR session={}: {}", session_id, e),
                );
                return Err(e);
            }
        };
        crate::debug::debug_log(
            "agent-attach",
            &format!(
                "detected session={} agent={:?} agent_pid={} cwd={}",
                session_id,
                attach.agent_type,
                attach.agent_pid,
                attach.initial_cwd.display()
            ),
        );
        let bindings = self.bind_services(&attach)?;
        let cwd = attach.initial_cwd.clone();
        self.run_watch_sequence(session_id, bindings, cwd).await
    }

    /// The post-bindings half of `start`: the verb sequence run on the
    /// blocking pool (`locate → ensure_trust → spawn_watch → register`)
    /// with the two diagnostic log lines at the orchestration site.
    /// Shared by `start` (production) and, in tests,
    /// `start_inner_for_test` — so the migrated T-LIFECYCLE-1/2a/2b tests
    /// exercise the EXACT production sequence rather than a parallel copy.
    ///
    /// **Atomic replace, no separate evict step.** Two cumulative
    /// invariants need to hold across a watcher restart:
    ///
    /// 1. **Spawn failure leaves the old watcher intact.** If
    ///    `spawn_watch` returns `Err` (inotify fd exhaustion, racy
    ///    restart, low-fd container), the closure exits via `?` BEFORE
    ///    `register` runs, so the old `WatcherHandle` is untouched and
    ///    the session keeps observing (PR #302 cycle 2 F3 fixed the
    ///    pre-cycle-2 evict-before-spawn ordering that violated this).
    ///
    /// 2. **The swap from old → new is observable as atomic.**
    ///    `register` calls `AgentWatcherState::insert`, which acquires
    ///    the watchers lock, calls `watchers.insert(sid, new_handle)`
    ///    (returning the displaced `Option<old_handle>`), and releases
    ///    the lock — the displaced `Option<WatcherHandle>` is bound to
    ///    `_displaced` in `insert`'s outer scope and drops AFTER the
    ///    guard, so the long Drop cascade (poll-thread join ≤3s,
    ///    session-index thread join ≤500ms, transcript-tail teardown)
    ///    runs OUTSIDE the lock with the NEW handle already in the
    ///    map. Concurrent `agent_type_for_pty` / `contains` /
    ///    `active_count` always see ONE handle for the session — never
    ///    None, never under-counted by one.
    ///
    /// Cycle 2 of PR #302 originally inserted an `lc.evict_old(&sid)`
    /// call between `spawn_watch` and `register` to make the eviction
    /// "explicit". That call inadvertently broke invariant 2 by routing
    /// the displaced handle's Drop through `AgentWatcherState::remove`'s
    /// in-function drop (which joins the polling thread BEFORE returning
    /// — leaving the map empty for the duration of the join). Cycle 3
    /// deleted the `evict_old` call: `insert` does the atomic replace
    /// it was designed for, and the spawn-failure-rollback property is
    /// preserved by the spawn-before-register ordering alone.
    ///
    /// Returns `Ok(true)` when the watcher was (re)spawned onto the located
    /// path, and `Ok(false)` for a Codex no-op relocate — a fresh locate that
    /// resolved the SAME rollout the live handle already watches. The drift
    /// tick (VIM-192) calls this every few seconds for the active Codex pane, so
    /// skipping the re-spawn avoids re-tailing a 20-114MB rollout on every tick.
    async fn run_watch_sequence(
        &self,
        session_id: String,
        bindings: AgentBindings,
        cwd: std::path::PathBuf,
    ) -> Result<bool, String> {
        // Locate outside the long-running spawn_blocking closure so kimi's
        // retry delay can be an async `tokio::time::sleep` that yields the
        // task. The actual filesystem work still runs on the blocking pool
        // (see `locate_async`).
        let located = self.locate_async(&bindings, &cwd, &session_id).await?;

        let lc = self.clone();
        tokio::task::spawn_blocking(move || {
            let trusted = lc.ensure_trust(located)?;

            // Codex-only no-op: if the fresh locate resolved the SAME rollout
            // the live handle already watches, skip spawn_watch + register
            // entirely. Scoped to Codex because only its locator sets
            // `status_path` to the rollout path (CompositeLocator); Claude /
            // Kimi keep their existing always-respawn restart semantics.
            if bindings.agent_type == AgentType::Codex
                && lc.watcher_state.current_status_path(&session_id).as_deref()
                    == Some(trusted.status_path())
            {
                log::debug!(
                    "codex relocate: no-op (same rollout still watched) session={} path={}",
                    session_id,
                    trusted.status_path().display(),
                );
                return Ok::<bool, String>(false);
            }

            log::debug!(
                "Watcher startup detail: session={}, cwd={}, path={}",
                session_id,
                cwd.display(),
                trusted.status_path().display(),
            );

            log::info!(
                "Starting agent watcher: session={}, path={}, active_watchers={}",
                session_id,
                trusted.status_path().display(),
                lc.watcher_state.active_count(),
            );

            crate::debug::debug_log(
                "agent-attach",
                &format!(
                    "watch session={} path={}",
                    session_id,
                    trusted.status_path().display()
                ),
            );

            // Capture the agent type before `spawn_watch` consumes `bindings`,
            // so `register` records it for the rename / title-sync IPC (main #265).
            let agent_type = bindings.agent_type;
            // Spawn FIRST. On `Err`, `?` short-circuits before any state
            // mutation — the old watcher (if any) is still live and
            // continues polling. PR #302 cycle 2 F3.
            //
            // PR #302 cycle 16 (codex P2 + retry-1): `spawn_watch`
            // takes a quiesce closure that fires AFTER notify setup
            // succeeds and BEFORE inline-init. This ordering
            // preserves cycle-2 F3's rollback invariant (spawn-failure
            // leaves the old watcher intact — quiesce never ran) AND
            // closes the codex P2 race (inline-init runs in a
            // quiesced world — OLD.alive already false, OLD's notify
            // callbacks short-circuit at `start_or_replace`'s
            // alive check).
            let handle = lc.spawn_watch(bindings, trusted, session_id.clone())?;
            // `register` (via `AgentWatcherState::insert`) atomically swaps
            // the old handle for the new one under a single lock; the
            // displaced handle drops outside the lock so its thread-join
            // cascade never blocks readers. See the docstring above for
            // the full invariant statement (PR #302 cycle 3 deleted a
            // separate `evict_old` call that broke this property).
            lc.register(session_id, handle, agent_type);
            Ok::<bool, String>(true)
        })
        .await
        .map_err(|e| format!("start_agent_watcher task panicked: {}", e))?
    }

    /// Test-only entry to the post-bindings sequence with hand-built
    /// `AgentBindings` (bypassing `resolve_attach`, which needs a live
    /// `PtyState` entry). Delegates to the SAME [`Self::run_watch_sequence`]
    /// production `start` runs, so the migrated T-LIFECYCLE-2a/2b
    /// short-circuit tests cover the real orchestration body.
    #[cfg(test)]
    pub(crate) async fn start_inner_for_test(
        &self,
        session_id: String,
        bindings: AgentBindings,
        cwd: std::path::PathBuf,
    ) -> Result<bool, String> {
        self.run_watch_sequence(session_id, bindings, cwd).await
    }

    /// Stop the agent watcher for `session_id`. Removing the
    /// `WatcherHandle` from `AgentWatcherState` runs its `Drop`, which
    /// cascades the transcript-tail teardown.
    ///
    /// PR #302 cycle 19 F1 (Claude post-cycle-18 review MED 90%):
    /// dispatched via `spawn_blocking` because `AgentWatcherState::
    /// remove` drops a `WatcherHandle` inline, and `WatcherHandle::
    /// Drop` joins the transcript-tail thread (~500ms) plus the
    /// poll / session-index threads. Joining OS threads from a
    /// Tokio task starves the executor during that window — on a
    /// single-threaded runtime this can deadlock; on
    /// multi-threaded it spikes IPC latency under concurrent
    /// session churn. The `start` path already uses
    /// `spawn_blocking` for the same reason (see
    /// `run_watch_sequence` above). The async `bool` from the
    /// inner closure propagates back through `JoinError`.
    pub(crate) async fn stop(&self, session_id: String) -> Result<(), String> {
        let watcher_state = self.watcher_state.clone();
        let sid_for_log = session_id.clone();
        let removed = tokio::task::spawn_blocking(move || watcher_state.remove(&session_id))
            .await
            .map_err(|e| format!("stop task panicked: {}", e))?;
        if removed {
            log::info!("Stopped watching statusline for session {}", sid_for_log);
            Ok(())
        } else {
            Err(format!("No active watcher for session: {}", sid_for_log))
        }
    }
}
