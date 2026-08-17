use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::agents::{AgentRegistry, BoundPane};
use crate::daemon::reconcile::{Action, Bindings};
use crate::daemon::sink::{HerdrSink, LiveHerdrPort};
use crate::herdr::client::{HerdrClient, HerdrClientError};
use crate::runtime::EventSink;
use crate::terminal::PtyState;

/// The interval to run at, and anything wrong with the config, from **one**
/// read of the file (§2.3). Returning both keeps the diagnostics and the
/// running value on the same snapshot, and keeps the whole chain testable: an
/// implementation that never loads the file fails these tests rather than
/// passing every parsing test in `daemon::config`.
///
/// Precedence, highest first (§2.2): `AGENT_WATCHER_INTERVAL_MS`, then
/// `[daemon] interval_ms`, then 1000 ms. The variable outranks the file
/// because an operator who exported it did so deliberately.
fn startup_config() -> (Duration, Vec<String>) {
    let config = crate::daemon::config::DaemonConfig::load();
    let interval = std::env::var("AGENT_WATCHER_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(config.interval);
    (interval, config.problems)
}

/// How often a daemon that keeps running sweeps again.
///
/// The retention is measured in days, so a directory that becomes eligible at
/// noon does not need collecting by one o'clock. Most daemons never reach this
/// -- herdr restarts them, and so does every plugin upgrade -- which makes the
/// sweep at startup the one that does the work and this the backstop for the
/// one that runs for a week.
const SWEEP_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// Sweep on its own thread, forever, until the daemon goes down.
///
/// Not in the reconcile loop: that runs every second and owns pane state,
/// while this walks a directory tree and calls `remove_dir_all` on what it
/// finds -- 402 of them in the tree that motivated this. A slow disk would
/// show up as late pane updates, which is a strange way to learn your disk is
/// slow.
fn spawn_sweeper(
    root: std::path::PathBuf,
    retention: Duration,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let (removed, bytes) =
                crate::daemon::prune::sweep(&root, retention, std::time::SystemTime::now());
            if removed > 0 {
                log::error!(
                    "[daemon] swept {removed} session director{} unwritten for {} days, \
                     freeing {} KB",
                    if removed == 1 { "y" } else { "ies" },
                    retention.as_secs() / 86_400,
                    bytes / 1024
                );
            }
            // In slices, so shutdown does not wait a day to be noticed.
            let wake = std::time::Instant::now() + SWEEP_EVERY;
            while std::time::Instant::now() < wake
                && !stop.load(std::sync::atomic::Ordering::Relaxed)
            {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

fn app_data_dir() -> std::path::PathBuf {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Identity BEFORE the watcher starts, and no half-bound card if it does not (§1.3a).
fn bind_pane(
    adapter: &Arc<dyn crate::agents::AgentAdapter>,
    store: &crate::daemon::store::TelemetryStore,
    pane_id: &str,
    agent: &str,
    session: &str,
) -> Result<(), String> {
    store.set_agent(pane_id, agent);
    let result = adapter.bind(BoundPane {
        pane_id: pane_id.to_string(),
        agent_id: agent.to_string(),
        agent_session: session.to_string(),
    });
    if let Err(bind_error) = &result {
        if let Err(stop_error) = adapter.unbind(pane_id) {
            log::error!(
                "[daemon] {pane_id}: bind failed ({bind_error}) and the rollback \
                 unbind failed too ({stop_error})"
            );
        }
        store.remove(pane_id);
    }
    result
}

/// One rebind, in the only order that cannot leave two watchers on one pane (§1.4).
fn rebind(
    registry: &AgentRegistry,
    bindings: &mut Bindings,
    store: &crate::daemon::store::TelemetryStore,
    pane_id: &str,
    agent: &str,
    session: &str,
) -> Result<(), String> {
    if let Some(previous) = bindings.agent_for(pane_id).map(str::to_string) {
        if let Some(old) = registry.adapter_for(&previous) {
            old.unbind(pane_id)?;
        }
    }
    bindings.forget(pane_id);
    let Some(adapter) = registry.adapter_for(agent) else {
        store.remove(pane_id);
        return Err(format!("no adapter for agent {agent}"));
    };
    store.replace_session(pane_id, agent);
    let bound = adapter.bind(BoundPane {
        pane_id: pane_id.to_string(),
        agent_id: agent.to_string(),
        agent_session: session.to_string(),
    });
    if let Err(bind_error) = &bound {
        if let Err(stop_error) = adapter.unbind(pane_id) {
            log::warn!(
                "[daemon] {pane_id}: rebind failed ({bind_error}) and the rollback \
                 unbind failed too ({stop_error}); the next tick re-binds"
            );
        }
        store.remove(pane_id);
    }
    bound
}

pub fn run() -> i32 {
    let Some(singleton) = crate::daemon::singleton::claim() else {
        return 1;
    };
    let mut consent =
        crate::daemon::consent::ConsentReloader::new(crate::agents::consent::consent_path());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let client = HerdrClient::from_env();
    let store = Arc::new(crate::daemon::store::TelemetryStore::default());
    let routes = crate::daemon::routes::BridgeRoutes::with_watchers(
        crate::agent::adapter::AgentWatcherState::default(),
    );
    let _state_server = match crate::daemon::state_server::StateServer::start(
        &crate::daemon::state_socket_path(),
        store.clone(),
        routes.clone(),
    ) {
        Ok(server) => server,
        Err(error) => {
            log::error!("[daemon] state socket failed: {error}");
            return 1;
        }
    };
    let events: Arc<dyn EventSink> = Arc::new(HerdrSink::with_store(
        Arc::new(LiveHerdrPort(HerdrClient::from_env())),
        store.clone(),
    ));
    let pty_state = PtyState::new();
    let registry = AgentRegistry::new(vec![Arc::new(crate::agents::sidecar::SidecarAdapter::new(
        pty_state.clone(),
        events,
        runtime.handle().clone(),
        app_data_dir(),
        routes,
    ))]);
    let mut bindings = Bindings::default();
    let mut skipped = HashSet::new();
    let (interval, config_problems) = startup_config();
    for problem in &config_problems {
        // Not readable from `herdr plugin log list` while the daemon runs --
        // that reports stderr only for finished actions. `doctor` is the
        // channel that works; this is for running the binary by hand.
        //
        // error!, not warn!: env_logger defaults to the error level, so a
        // warning here would be dropped unless RUST_LOG is set, and the
        // inherited environment is what this change exists to stop depending
        // on.
        log::error!("[daemon] config.toml: {problem}");
    }
    if let Some(retention) = crate::daemon::config::DaemonConfig::load().prune_after {
        spawn_sweeper(app_data_dir(), retention, singleton.shutdown.clone());
    }

    while !singleton
        .shutdown
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        consent.reload_if_changed();
        match client.pane_list() {
            Ok(panes) => {
                let live_skips: HashSet<_> = panes
                    .iter()
                    .filter_map(|pane| {
                        Some((
                            pane.pane_id.clone(),
                            pane.agent.clone()?,
                            pane.session_value()?.to_string(),
                        ))
                    })
                    .collect();
                skipped.retain(|binding| live_skips.contains(binding));

                for pane in &panes {
                    let pid = client.pane_shell_pid(&pane.pane_id).ok().flatten();
                    let cwd = pane.foreground_cwd.clone().or_else(|| pane.cwd.clone());
                    pty_state.upsert_pane(&pane.pane_id, pid, cwd);
                }

                let mut applied = Vec::new();
                for action in bindings.diff(&panes) {
                    let result = match &action {
                        Action::Bind {
                            pane_id,
                            agent,
                            session,
                        } => match registry.adapter_for(agent) {
                            Some(adapter) => bind_pane(adapter, &store, pane_id, agent, session),
                            None => {
                                if skipped.insert((pane_id.clone(), agent.clone(), session.clone()))
                                {
                                    log::debug!("[daemon] no adapter for agent {agent}; skipping");
                                }
                                continue;
                            }
                        },
                        Action::Rebind {
                            pane_id,
                            agent,
                            session,
                        } => rebind(&registry, &mut bindings, &store, pane_id, agent, session),
                        Action::Unbind { pane_id } => {
                            let result = bindings
                                .agent_for(pane_id)
                                .and_then(|agent| registry.adapter_for(agent))
                                .map(|adapter| adapter.unbind(pane_id))
                                .unwrap_or(Ok(()));
                            if result.is_ok() {
                                pty_state.remove_pane(pane_id);
                                store.remove(pane_id);
                            }
                            result
                        }
                    };
                    match result {
                        Ok(()) => applied.push(action),
                        Err(error) => log::warn!("[daemon] {action:?} failed: {error}"),
                    }
                }
                bindings.apply(&applied);

                for (position, pane) in panes.iter().enumerate() {
                    let cwd = pane.foreground_cwd.clone().or_else(|| pane.cwd.clone());
                    store.set_pane_list_cwd(&pane.pane_id, cwd);
                    // Not at `set_agent`: that is inside `bind_pane`, which
                    // receives only pane_id/agent/session and never sees a
                    // `PaneInfo`. This loop does.
                    store.set_pane_workspace(&pane.pane_id, Some(pane.workspace_id.clone()));
                    // The ORDER of this list is the information, not anything
                    // in a `PaneInfo`: herdr returns panes in layout order, so
                    // the index reproduces what its own sidebar shows.
                    store.set_pane_position(&pane.pane_id, position as u32);
                }
            }
            Err(HerdrClientError::Connect { .. }) => {
                log::info!("[daemon] herdr unavailable; exiting");
                return 0;
            }
            Err(error) => log::warn!("[daemon] pane.list failed: {error}"),
        }
        crate::daemon::singleton::sleep_interruptible(&singleton.shutdown, interval);
    }
    log::info!("[daemon] shutdown requested; exiting");
    0
}

#[cfg(test)]
mod startup_config_tests {
    use super::*;
    use crate::test_env::with_env;

    fn config_dir(text: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("config.toml"), text).expect("write");
        dir
    }

    #[test]
    fn the_config_file_sets_the_interval() {
        let dir = config_dir("[daemon]\ninterval_ms = 5000\n");
        with_env(
            &[
                ("HERDR_PLUGIN_CONFIG_DIR", Some(dir.path().into())),
                ("AGENT_WATCHER_INTERVAL_MS", None),
            ],
            || {
                let (interval, problems) = startup_config();
                assert_eq!(interval, Duration::from_millis(5000));
                assert!(problems.is_empty());
            },
        );
    }

    #[test]
    fn the_environment_outranks_the_file() {
        let dir = config_dir("[daemon]\ninterval_ms = 5000\n");
        with_env(
            &[
                ("HERDR_PLUGIN_CONFIG_DIR", Some(dir.path().into())),
                ("AGENT_WATCHER_INTERVAL_MS", Some("250".into())),
            ],
            || assert_eq!(startup_config().0, Duration::from_millis(250)),
        );
    }

    #[test]
    fn a_rejected_file_value_is_the_default_and_still_reported() {
        let dir = config_dir("[daemon]\ninterval_ms = 0\n");
        with_env(
            &[
                ("HERDR_PLUGIN_CONFIG_DIR", Some(dir.path().into())),
                ("AGENT_WATCHER_INTERVAL_MS", None),
            ],
            || {
                let (interval, problems) = startup_config();
                assert_eq!(interval, Duration::from_secs(1));
                assert_eq!(problems.len(), 1);
                assert!(problems[0].contains("daemon.interval_ms"), "{problems:?}");
            },
        );
    }

    #[test]
    fn a_rejected_file_value_is_reported_even_when_the_environment_wins() {
        let dir = config_dir("[daemon]\ninterval_ms = 0\n");
        with_env(
            &[
                ("HERDR_PLUGIN_CONFIG_DIR", Some(dir.path().into())),
                ("AGENT_WATCHER_INTERVAL_MS", Some("250".into())),
            ],
            || {
                let (interval, problems) = startup_config();
                assert_eq!(interval, Duration::from_millis(250));
                assert_eq!(problems.len(), 1, "a losing setting is still a mistake");
            },
        );
    }

    #[test]
    fn a_rejected_environment_value_falls_through_to_the_file() {
        let dir = config_dir("[daemon]\ninterval_ms = 5000\n");
        with_env(
            &[
                ("HERDR_PLUGIN_CONFIG_DIR", Some(dir.path().into())),
                ("AGENT_WATCHER_INTERVAL_MS", Some("0".into())),
            ],
            || assert_eq!(startup_config().0, Duration::from_millis(5000)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::agents::{AgentAdapter, AgentRegistry, BoundPane};
    use crate::daemon::reconcile::{Action, Bindings};
    use crate::daemon::store::TelemetryStore;
    use crate::herdr::api::PaneInfo;

    struct FakeAdapter {
        ids: Vec<&'static str>,
        unbind_fails: bool,
        bind_fails: std::sync::atomic::AtomicBool,
        agent_at_bind: Mutex<Option<String>>,
        store: Mutex<Option<Arc<TelemetryStore>>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeAdapter {
        fn new(id: &'static str) -> Self {
            Self {
                ids: vec![id],
                unbind_fails: false,
                bind_fails: std::sync::atomic::AtomicBool::new(false),
                agent_at_bind: Mutex::new(None),
                store: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn watching(self: &Arc<Self>, store: &Arc<TelemetryStore>) {
            *self.store.lock().unwrap() = Some(store.clone());
        }
    }

    impl AgentAdapter for FakeAdapter {
        fn ids(&self) -> &[&str] {
            &self.ids
        }

        fn bind(&self, pane: BoundPane) -> Result<(), String> {
            if let Some(store) = self.store.lock().unwrap().as_ref() {
                *self.agent_at_bind.lock().unwrap() = store
                    .snapshot()
                    .1
                    .get(&pane.pane_id)
                    .and_then(|t| t.agent.clone());
            }
            self.calls
                .lock()
                .unwrap()
                .push(format!("bind {} {}", pane.agent_id, pane.agent_session));
            if self.bind_fails.load(std::sync::atomic::Ordering::SeqCst) {
                Err("bind failed".into())
            } else {
                Ok(())
            }
        }

        fn unbind(&self, pane_id: &str) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("unbind {pane_id}"));
            if self.unbind_fails {
                Err("watcher will not stop".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn the_entry_exists_before_the_watcher_is_started() {
        let adapter = Arc::new(FakeAdapter::new("claude"));
        let store = Arc::new(TelemetryStore::default());
        adapter.watching(&store);
        let dynamic: Arc<dyn AgentAdapter> = adapter.clone();

        bind_pane(&dynamic, &store, "p1", "claude", "s1").expect("bind");

        assert_eq!(
            adapter.agent_at_bind.lock().unwrap().as_deref(),
            Some("claude"),
            "an event arriving during bind() must find a card to land on (§1.3a)"
        );
    }

    #[test]
    fn a_rollback_that_fails_changes_nothing_about_the_outcome() {
        let mut fake = FakeAdapter::new("claude");
        fake.bind_fails = std::sync::atomic::AtomicBool::new(true);
        fake.unbind_fails = true;
        let adapter = Arc::new(fake);
        let store = Arc::new(TelemetryStore::default());
        let dynamic: Arc<dyn AgentAdapter> = adapter.clone();

        bind_pane(&dynamic, &store, "p1", "claude", "s1").expect_err("the bind failed");

        assert!(
            !store.snapshot().1.contains_key("p1"),
            "§1.3a is absolute: no card without a watcher, even when cleanup failed"
        );
        assert_eq!(
            *adapter.calls.lock().unwrap(),
            vec!["bind claude s1", "unbind p1"],
            "the rollback is attempted; its failure is logged, not encoded in state"
        );
    }

    #[test]
    fn a_failed_initial_bind_leaves_no_card_behind() {
        let mut fake = FakeAdapter::new("claude");
        fake.bind_fails = std::sync::atomic::AtomicBool::new(true);
        let adapter = Arc::new(fake);
        let store = Arc::new(TelemetryStore::default());
        let dynamic: Arc<dyn AgentAdapter> = adapter.clone();

        bind_pane(&dynamic, &store, "p1", "claude", "s1").expect_err("bind failed");

        assert!(
            !store.snapshot().1.contains_key("p1"),
            "a pane with no watcher must not render as an agent"
        );
        assert_eq!(
            *adapter.calls.lock().unwrap(),
            vec!["bind claude s1", "unbind p1"],
            "the half-started watcher is stopped too, not just the card removed"
        );
    }

    fn adapter_bind_should_succeed(adapter: &Arc<FakeAdapter>) {
        adapter
            .bind_fails
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    fn bound(adapter: Arc<FakeAdapter>) -> (AgentRegistry, Bindings, TelemetryStore) {
        let registry = AgentRegistry::new(vec![adapter as Arc<dyn AgentAdapter>]);
        let mut bindings = Bindings::default();
        bindings.apply(&[Action::Bind {
            pane_id: "p1".into(),
            agent: "claude".into(),
            session: "s1".into(),
        }]);
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.set_pane_list_cwd("p1", Some("/w/repo".into()));
        store.record(
            "p1",
            "agent-tool-call",
            serde_json::json!({"status": "done", "toolUseId": "t1", "tool": "Read"}),
        );
        (registry, bindings, store)
    }

    #[test]
    fn a_successful_rebind_stops_the_old_watcher_before_binding_the_new_session() {
        let adapter = Arc::new(FakeAdapter::new("claude"));
        let (registry, mut bindings, store) = bound(adapter.clone());

        rebind(&registry, &mut bindings, &store, "p1", "claude", "s2").expect("rebind");

        assert_eq!(
            *adapter.calls.lock().unwrap(),
            vec!["unbind p1", "bind claude s2"],
            "the old watcher stops BEFORE the replacement starts"
        );
        let pane = store.snapshot().1["p1"].clone();
        assert_eq!(pane.tool_call_total, 0, "session-scoped state is cleared");
        assert!(pane.tool_counts.is_empty());
        assert_eq!(
            pane.cwd.as_deref(),
            Some("/w/repo"),
            "pane-scoped state carries"
        );
    }

    #[test]
    fn an_agent_change_quiesces_the_old_adapter_and_binds_the_new_one() {
        let old = Arc::new(FakeAdapter::new("claude"));
        let new = Arc::new(FakeAdapter::new("codex"));
        let registry = AgentRegistry::new(vec![
            old.clone() as Arc<dyn AgentAdapter>,
            new.clone() as Arc<dyn AgentAdapter>,
        ]);
        let mut bindings = Bindings::default();
        bindings.apply(&[Action::Bind {
            pane_id: "p1".into(),
            agent: "claude".into(),
            session: "s1".into(),
        }]);
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");

        rebind(&registry, &mut bindings, &store, "p1", "codex", "s1").expect("rebind");

        assert_eq!(
            *old.calls.lock().unwrap(),
            vec!["unbind p1"],
            "the OLD adapter stops it"
        );
        assert_eq!(
            *new.calls.lock().unwrap(),
            vec!["bind codex s1"],
            "the NEW one starts it"
        );
        assert_eq!(store.snapshot().1["p1"].agent.as_deref(), Some("codex"));
        bindings.apply(&[Action::Rebind {
            pane_id: "p1".into(),
            agent: "codex".into(),
            session: "s1".into(),
        }]);
        assert_eq!(bindings.agent_for("p1"), Some("codex"));
    }

    #[test]
    fn a_watcher_that_will_not_stop_aborts_the_rebind_and_keeps_the_binding() {
        let mut fake = FakeAdapter::new("claude");
        fake.unbind_fails = true;
        let adapter = Arc::new(fake);
        let (registry, mut bindings, store) = bound(adapter.clone());

        rebind(&registry, &mut bindings, &store, "p1", "claude", "s2")
            .expect_err("a failed quiesce must not proceed");

        assert_eq!(*adapter.calls.lock().unwrap(), vec!["unbind p1"]);
        assert_eq!(bindings.agent_for("p1"), Some("claude"));
        assert_eq!(store.snapshot().1["p1"].tool_call_total, 1);
    }

    #[test]
    fn a_failed_bind_removes_the_provisional_entry_and_clears_the_binding() {
        let mut fake = FakeAdapter::new("claude");
        fake.bind_fails = std::sync::atomic::AtomicBool::new(true);
        let adapter = Arc::new(fake);
        let (registry, mut bindings, store) = bound(adapter.clone());

        rebind(&registry, &mut bindings, &store, "p1", "claude", "s2").expect_err("bind failed");

        assert!(!store.snapshot().1.contains_key("p1"));
        assert_eq!(
            *adapter.calls.lock().unwrap(),
            vec!["unbind p1", "bind claude s2", "unbind p1"]
        );
        assert_eq!(bindings.agent_for("p1"), None);
    }

    fn pane(session: &str) -> PaneInfo {
        serde_json::from_value(serde_json::json!({
            "pane_id": "p1",
            "workspace_id": "w1",
            "agent": "claude",
            "agent_session": {
                "source": "herdr:claude", "agent": "claude",
                "kind": "id", "value": session,
            },
        }))
        .expect("PaneInfo")
    }

    #[test]
    fn the_tick_after_a_failed_rebind_emits_a_plain_bind() {
        let mut fake = FakeAdapter::new("claude");
        fake.bind_fails = std::sync::atomic::AtomicBool::new(true);
        let adapter = Arc::new(fake);
        let (registry, mut bindings, store) = bound(adapter.clone());

        rebind(&registry, &mut bindings, &store, "p1", "claude", "s2").expect_err("bind failed");

        let actions = bindings.diff(&[pane("s2")]);
        assert!(matches!(actions.as_slice(), [Action::Bind { .. }]));

        adapter_bind_should_succeed(&adapter);
        let Action::Bind {
            pane_id,
            agent,
            session,
        } = &actions[0]
        else {
            unreachable!("checked above")
        };
        let dynamic: Arc<dyn AgentAdapter> = adapter.clone();
        bind_pane(&dynamic, &store, pane_id, agent, session).expect("the retry binds");
        bindings.apply(&actions);

        assert_eq!(bindings.agent_for("p1"), Some("claude"));
        assert_eq!(store.snapshot().1["p1"].agent.as_deref(), Some("claude"));
        assert!(bindings.diff(&[pane("s2")]).is_empty());
    }

    #[test]
    fn rebinding_to_an_unsupported_agent_removes_the_card_instead_of_leaving_a_ghost() {
        let adapter = Arc::new(FakeAdapter::new("claude"));
        let (registry, mut bindings, store) = bound(adapter.clone());

        rebind(&registry, &mut bindings, &store, "p1", "kimi", "s2")
            .expect_err("an unsupported pair must not be recorded as bound");

        assert!(!store.snapshot().1.contains_key("p1"));
        assert_eq!(*adapter.calls.lock().unwrap(), vec!["unbind p1"]);
        assert_eq!(bindings.agent_for("p1"), None);
    }
}
