use std::collections::HashMap;

use crate::herdr::api::PaneInfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Bind {
        pane_id: String,
        agent: String,
        session: String,
    },
    Rebind {
        pane_id: String,
        agent: String,
        session: String,
    },
    Unbind {
        pane_id: String,
    },
}

#[derive(Default)]
pub struct Bindings {
    bound: HashMap<String, (String, String)>,
}

impl Bindings {
    pub fn diff(&self, panes: &[PaneInfo]) -> Vec<Action> {
        let present: std::collections::HashSet<&str> =
            panes.iter().map(|pane| pane.pane_id.as_str()).collect();
        let live: HashMap<&str, (&str, &str)> = panes
            .iter()
            .filter_map(|pane| {
                Some((
                    pane.pane_id.as_str(),
                    (pane.agent.as_deref()?, pane.session_value()?),
                ))
            })
            .collect();
        let mut actions = Vec::new();

        for (pane_id, bound) in &self.bound {
            if !present.contains(pane_id.as_str()) {
                actions.push(Action::Unbind {
                    pane_id: pane_id.clone(),
                });
                continue;
            }
            match live.get(pane_id.as_str()) {
                None => {}
                Some((agent, session))
                    if (bound.0.as_str(), bound.1.as_str()) != (*agent, *session) =>
                {
                    actions.push(Action::Rebind {
                        pane_id: pane_id.clone(),
                        agent: (*agent).to_string(),
                        session: (*session).to_string(),
                    });
                }
                Some(_) => {}
            }
        }
        for (pane_id, (agent, session)) in live {
            if !self.bound.contains_key(pane_id) {
                actions.push(Action::Bind {
                    pane_id: pane_id.to_string(),
                    agent: agent.to_string(),
                    session: session.to_string(),
                });
            }
        }
        actions.sort_by_key(|action| match action {
            Action::Unbind { pane_id } => (0, pane_id.clone()),
            Action::Rebind { pane_id, .. } => (1, pane_id.clone()),
            Action::Bind { pane_id, .. } => (2, pane_id.clone()),
        });
        actions
    }

    pub fn apply(&mut self, actions: &[Action]) {
        for action in actions {
            match action {
                Action::Bind {
                    pane_id,
                    agent,
                    session,
                } => {
                    self.bound
                        .insert(pane_id.clone(), (agent.clone(), session.clone()));
                }
                Action::Rebind {
                    pane_id,
                    agent,
                    session,
                } => {
                    self.bound
                        .insert(pane_id.clone(), (agent.clone(), session.clone()));
                }
                Action::Unbind { pane_id } => {
                    self.bound.remove(pane_id);
                }
            }
        }
    }

    pub fn agent_for(&self, pane_id: &str) -> Option<&str> {
        self.bound.get(pane_id).map(|(agent, _)| agent.as_str())
    }

    pub fn forget(&mut self, pane_id: &str) {
        self.bound.remove(pane_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::api::{AgentSessionInfo, PaneInfo};

    fn pane(id: &str, agent: Option<&str>, session: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            workspace_id: "w1".into(),
            agent: agent.map(Into::into),
            agent_session: session.map(|value| AgentSessionInfo {
                source: None,
                agent: agent.map(Into::into),
                kind: None,
                value: Some(value.into()),
            }),
            cwd: Some("/tmp".into()),
            foreground_cwd: None,
        }
    }

    #[test]
    fn diff_binds_rebinds_unbinds_and_ignores_incomplete_panes() {
        let mut bound = Bindings::default();
        let actions = bound.diff(&[pane("p1", Some("claude"), Some("s1"))]);
        assert_eq!(
            actions,
            vec![Action::Bind {
                pane_id: "p1".into(),
                agent: "claude".into(),
                session: "s1".into(),
            }]
        );
        bound.apply(&actions);
        assert!(bound
            .diff(&[pane("p1", Some("claude"), Some("s1"))])
            .is_empty());

        let actions = bound.diff(&[pane("p1", Some("claude"), Some("s2"))]);
        assert_eq!(
            actions,
            vec![Action::Rebind {
                pane_id: "p1".into(),
                agent: "claude".into(),
                session: "s2".into(),
            }],
            "one action now: Unbind+Bind lost the pane's cwd between them (§1.4)"
        );
        bound.apply(&actions);

        assert_eq!(
            bound.diff(&[pane("p2", Some("claude"), None)]),
            vec![Action::Unbind {
                pane_id: "p1".into()
            }]
        );
    }

    #[test]
    fn an_agent_change_reusing_the_session_still_rebinds() {
        let mut bound = Bindings::default();
        bound.apply(&bound.diff(&[pane("p1", Some("claude"), Some("s1"))]));
        let actions = bound.diff(&[pane("p1", Some("codex"), Some("s1"))]);
        assert_eq!(
            actions,
            vec![Action::Rebind {
                pane_id: "p1".into(),
                agent: "codex".into(),
                session: "s1".into()
            }]
        );
    }

    #[test]
    fn a_failed_rebind_leaves_the_pane_unbound_so_the_next_tick_plain_binds() {
        let mut bound = Bindings::default();
        bound.apply(&bound.diff(&[pane("p1", Some("claude"), Some("s1"))]));
        bound.forget("p1");
        let actions = bound.diff(&[pane("p1", Some("claude"), Some("s2"))]);
        assert!(
            matches!(actions.as_slice(), [Action::Bind { .. }]),
            "a cleared binding must retry as Bind, never loop on Rebind"
        );
    }

    #[test]
    fn a_present_pane_whose_identity_has_not_arrived_yet_is_held_not_unbound() {
        let mut bound = Bindings::default();
        bound.apply(&bound.diff(&[pane("p1", Some("claude"), Some("s1"))]));
        assert!(
            bound.diff(&[pane("p1", None, None)]).is_empty(),
            "a momentary gap is not a departure — unbinding here deletes the card \
             and loses the cwd (§1.4)"
        );
        assert_eq!(
            bound.diff(&[]),
            vec![Action::Unbind {
                pane_id: "p1".into()
            }]
        );
    }

    #[test]
    fn a_session_change_rebinds_rather_than_unbind_then_bind() {
        let mut bound = Bindings::default();
        bound.apply(&bound.diff(&[pane("p1", Some("claude"), Some("s1"))]));
        let actions = bound.diff(&[pane("p1", Some("claude"), Some("s2"))]);
        assert!(matches!(actions.as_slice(), [Action::Rebind { .. }]));
    }
}
