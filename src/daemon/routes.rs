use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::agent::adapter;

#[derive(Clone, Default)]
pub(crate) struct BridgeRoutes {
    sessions: Arc<Mutex<HashMap<String, String>>>,
    watchers: adapter::AgentWatcherState,
}

impl BridgeRoutes {
    pub(crate) fn with_watchers(watchers: adapter::AgentWatcherState) -> Self {
        Self {
            watchers,
            ..Self::default()
        }
    }

    pub(crate) fn bind(&self, pane_id: &str, agent_session: &str) {
        self.sessions
            .lock()
            .expect("bridge routes poisoned")
            .insert(pane_id.to_string(), agent_session.to_string());
    }

    pub(crate) fn unbind(&self, pane_id: &str) {
        self.sessions
            .lock()
            .expect("bridge routes poisoned")
            .remove(pane_id);
    }

    pub(crate) fn resolve(&self, pane_id: &str, agent_session: &str) -> Option<PathBuf> {
        let bound = self
            .sessions
            .lock()
            .expect("bridge routes poisoned")
            .get(pane_id)
            .cloned()?;
        if bound != agent_session {
            return None;
        }
        self.watchers.current_status_path(pane_id)
    }

    pub(crate) fn watchers(&self) -> &adapter::AgentWatcherState {
        &self.watchers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_watcher(routes: &BridgeRoutes, pane: &str) {
        use crate::agent::adapter::base::WatcherHandle;
        let handle = WatcherHandle::new_for_test(
            crate::agent::adapter::base::TranscriptState::default(),
            pane.to_string(),
        );
        routes.watchers().insert(
            pane.to_string(),
            handle,
            crate::agent::types::AgentType::ClaudeCode,
        );
    }

    #[test]
    fn a_bound_pane_with_a_live_watcher_resolves_through_the_production_lookup() {
        let routes = BridgeRoutes::default();
        with_watcher(&routes, "w1:p1");
        routes.bind("w1:p1", "session-a");
        assert!(routes.resolve("w1:p1", "session-a").is_some());
    }

    #[test]
    fn a_bound_pane_with_no_watcher_resolves_to_nothing() {
        let routes = BridgeRoutes::default();
        routes.bind("w1:p1", "session-a");
        assert_eq!(routes.resolve("w1:p1", "session-a"), None);
    }

    #[test]
    fn an_unbound_pane_resolves_to_nothing() {
        let routes = BridgeRoutes::default();
        with_watcher(&routes, "w1:p1");
        assert_eq!(routes.resolve("w1:p1", "session-a"), None);
    }

    #[test]
    fn a_session_that_does_not_match_the_binding_resolves_to_nothing() {
        let routes = BridgeRoutes::default();
        with_watcher(&routes, "w1:p1");
        routes.bind("w1:p1", "session-b");
        assert_eq!(routes.resolve("w1:p1", "session-a"), None);
    }

    #[test]
    fn unbinding_forgets_the_pane() {
        let routes = BridgeRoutes::default();
        with_watcher(&routes, "w1:p1");
        routes.bind("w1:p1", "session-a");
        assert!(routes.resolve("w1:p1", "session-a").is_some());
        routes.unbind("w1:p1");
        assert_eq!(routes.resolve("w1:p1", "session-a"), None);
    }
}
