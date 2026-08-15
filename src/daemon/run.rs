use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::agents::{AgentRegistry, BoundPane};
use crate::daemon::reconcile::{Action, Bindings};
use crate::daemon::sink::{HerdrSink, LiveHerdrPort};
use crate::herdr::client::{HerdrClient, HerdrClientError};
use crate::runtime::EventSink;
use crate::terminal::PtyState;

fn interval() -> Duration {
    std::env::var("AGENT_WATCHER_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(1))
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

                for pane in &panes {
                    let cwd = pane.foreground_cwd.clone().or_else(|| pane.cwd.clone());
                    store.set_pane_list_cwd(&pane.pane_id, cwd);
                }
            }
            Err(HerdrClientError::Connect { .. }) => {
                log::info!("[daemon] herdr unavailable; exiting");
                return 0;
            }
            Err(error) => log::warn!("[daemon] pane.list failed: {error}"),
        }
        crate::daemon::singleton::sleep_interruptible(&singleton.shutdown, interval());
    }
    log::info!("[daemon] shutdown requested; exiting");
    0
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
