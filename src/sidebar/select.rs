//! Which cards are shown, and in what order (§3.3, §3.4). Filter first, then
//! sort, so the hidden count is known before the viewport is measured.

use std::collections::HashMap;

use crate::daemon::store::{CardState, PaneTelemetry};
use crate::sidebar::agent_ids;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    Smart,
    Group,
}

pub struct Visible {
    pub panes: Vec<(String, PaneTelemetry)>,
    pub hidden_idle: usize,
}

fn rank(state: CardState) -> u8 {
    match state {
        CardState::Error => 0,
        CardState::Attention => 1,
        CardState::Running => 2,
        CardState::Finished => 3,
        CardState::Idle => 4,
    }
}

/// `group` sorts on the CANONICAL agent id, never the display label: a `label`
/// override is presentation and must not silently regroup the list (§3.3).
fn group_key(t: &PaneTelemetry) -> &str {
    t.agent
        .as_deref()
        .and_then(agent_ids::canonical)
        .unwrap_or("")
}

pub fn visible(panes: &HashMap<String, PaneTelemetry>, sort: Sort, hide_idle: bool) -> Visible {
    let mut kept: Vec<(String, PaneTelemetry)> = Vec::new();
    let mut hidden_idle = 0;
    for (id, t) in panes {
        if hide_idle && t.card_state == CardState::Idle {
            hidden_idle += 1;
            continue;
        }
        kept.push((id.clone(), t.clone()));
    }

    kept.sort_by(|(ida, a), (idb, b)| match sort {
        Sort::Smart => rank(a.card_state)
            .cmp(&rank(b.card_state))
            .then(b.updated_seq.cmp(&a.updated_seq))
            .then(ida.cmp(idb)),
        Sort::Group => group_key(a)
            .cmp(group_key(b))
            .then(rank(a.card_state).cmp(&rank(b.card_state)))
            .then(b.updated_seq.cmp(&a.updated_seq))
            .then(ida.cmp(idb)),
    });

    Visible {
        panes: kept,
        hidden_idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::store::{CardState, PaneTelemetry};
    use std::collections::HashMap;

    fn pane(agent: &str, state: CardState, seq: u64) -> PaneTelemetry {
        let mut t = PaneTelemetry::with_agent(agent);
        t.card_state = state;
        t.updated_seq = seq;
        t
    }

    fn fixture() -> HashMap<String, PaneTelemetry> {
        HashMap::from([
            ("p1".to_string(), pane("claude", CardState::Idle, 10)),
            ("p2".to_string(), pane("codex", CardState::Error, 2)),
            ("p3".to_string(), pane("claude", CardState::Running, 9)),
            ("p4".to_string(), pane("kimi", CardState::Attention, 1)),
        ])
    }

    #[test]
    fn smart_ranks_by_state_then_recency() {
        let out = visible(&fixture(), Sort::Smart, false);
        assert_eq!(ids(&out), vec!["p2", "p4", "p3", "p1"]);
    }

    #[test]
    fn group_gathers_agents_then_smart_within() {
        let out = visible(&fixture(), Sort::Group, false);
        assert_eq!(ids(&out), vec!["p3", "p1", "p2", "p4"]);
    }

    #[test]
    fn hide_idle_filters_and_reports_the_count() {
        let out = visible(&fixture(), Sort::Smart, true);
        assert_eq!(out.hidden_idle, 1);
        assert!(!ids(&out).contains(&"p1"));
    }

    #[test]
    fn ordering_is_total_so_frames_do_not_flicker() {
        let mut m = HashMap::new();
        m.insert("b".to_string(), pane("claude", CardState::Idle, 5));
        m.insert("a".to_string(), pane("claude", CardState::Idle, 5));
        assert_eq!(ids(&visible(&m, Sort::Smart, false)), vec!["a", "b"]);
    }

    fn ids(v: &Visible) -> Vec<&str> {
        v.panes.iter().map(|(id, _)| id.as_str()).collect()
    }
}
