use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::agent::adapter;

/// A status-line write this daemon turned away because the session offering it
/// is not the session bound to that pane. Both sides are kept: on its own,
/// "refused" does not tell the reader that the pane's session was replaced.
pub(crate) type Refusals = HashMap<String, crate::daemon::state_wire::Refusal>;

#[derive(Clone, Default)]
pub(crate) struct BridgeRoutes {
    sessions: Arc<Mutex<HashMap<String, String>>>,
    watchers: adapter::AgentWatcherState,
    /// One entry per pane, overwritten. A refused status line renders again
    /// every second for as long as the pane is open, so anything that grows
    /// per refusal grows without bound.
    refusals: Arc<Mutex<Refusals>>,
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
        self.refusals
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
            // Recorded ONLY here. A pane that is unbound, or bound with no
            // watcher yet, also resolves to nothing, but neither means the
            // session was replaced -- and the remedy for this one is to
            // reopen the pane, which is the wrong advice for those.
            self.refusals.lock().expect("bridge routes poisoned").insert(
                pane_id.to_string(),
                crate::daemon::state_wire::Refusal {
                    offered: agent_session.to_string(),
                    bound,
                },
            );
            return None;
        }
        let path = self.watchers.current_status_path(pane_id);
        if path.is_some() {
            // Reporting again: whatever was wrong before is over, and a stale
            // refusal would send the reader to reopen a working pane.
            self.refusals
                .lock()
                .expect("bridge routes poisoned")
                .remove(pane_id);
        }
        path
    }

    pub(crate) fn refusals(&self) -> Refusals {
        self.refusals
            .lock()
            .expect("bridge routes poisoned")
            .clone()
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
    fn a_refused_session_is_recorded_with_both_sides() {
        let routes = BridgeRoutes::default();
        with_watcher(&routes, "w1:p1");
        routes.bind("w1:p1", "session-b");
        assert_eq!(routes.resolve("w1:p1", "session-a"), None);

        let refused = routes.refusals();
        let entry = refused.get("w1:p1").expect("the refusal is recorded");
        assert_eq!(entry.offered, "session-a");
        assert_eq!(entry.bound, "session-b");
    }

    #[test]
    fn only_a_session_mismatch_is_a_refusal() {
        // An unbound pane and a bound pane with no watcher both resolve to
        // nothing, but neither means "your session was replaced" -- recording
        // them would send the reader to reopen a pane that is fine.
        let routes = BridgeRoutes::default();
        with_watcher(&routes, "w1:p1");
        assert_eq!(routes.resolve("w1:p1", "session-a"), None);
        assert!(routes.refusals().is_empty(), "unbound is not a refusal");

        let routes = BridgeRoutes::default();
        routes.bind("w1:p1", "session-a");
        assert_eq!(routes.resolve("w1:p1", "session-a"), None);
        assert!(routes.refusals().is_empty(), "no watcher is not a refusal");
    }

    #[test]
    fn a_later_success_clears_the_refusal() {
        let routes = BridgeRoutes::default();
        with_watcher(&routes, "w1:p1");
        routes.bind("w1:p1", "session-b");
        assert_eq!(routes.resolve("w1:p1", "session-a"), None);
        assert!(!routes.refusals().is_empty());

        routes.bind("w1:p1", "session-a");
        assert!(routes.resolve("w1:p1", "session-a").is_some());
        assert!(
            routes.refusals().is_empty(),
            "a pane that is reporting again must not still be reported as refused"
        );
    }

    #[test]
    fn one_entry_per_pane_however_often_it_is_refused() {
        let routes = BridgeRoutes::default();
        with_watcher(&routes, "w1:p1");
        routes.bind("w1:p1", "session-b");
        for _ in 0..500 {
            let _ = routes.resolve("w1:p1", "session-a");
        }
        assert_eq!(routes.refusals().len(), 1, "a status line renders forever");
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
