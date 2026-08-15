//! Which cards are shown, and in what order (§3.3, §3.4). Scope first, then
//! idle, then sort, so the hidden count is known before the viewport is
//! measured and counts only panes this sidebar would have shown.

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

/// `scope` is `Some(workspace)` to list only that workspace, `None` to list
/// everything. A pane with no workspace is kept either way: the daemon has not
/// placed it yet, and a card that appears a second late beats one that never
/// appears and cannot be explained.
///
/// The scope filter runs before the idle count, so `hidden_idle` counts only
/// panes this sidebar would otherwise have shown.
pub fn visible(
    panes: &HashMap<String, PaneTelemetry>,
    sort: Sort,
    hide_idle: bool,
    scope: Option<&str>,
) -> Visible {
    let mut kept: Vec<(String, PaneTelemetry)> = Vec::new();
    let mut hidden_idle = 0;
    for (id, t) in panes {
        if let (Some(scope), Some(workspace)) = (scope, t.workspace_id.as_deref()) {
            if scope != workspace {
                continue;
            }
        }
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

    /// Same shape as `pane` above, plus a workspace.
    fn placed(state: CardState, seq: u64, workspace: Option<&str>) -> PaneTelemetry {
        let mut t = pane("claude", state, seq);
        t.workspace_id = workspace.map(str::to_string);
        t
    }

    fn scoped_fixture() -> HashMap<String, PaneTelemetry> {
        HashMap::from([
            (
                "mine".to_string(),
                placed(CardState::Running, 3, Some("w4")),
            ),
            (
                "theirs".to_string(),
                placed(CardState::Running, 2, Some("w9")),
            ),
            (
                "unplaced".to_string(),
                placed(CardState::Running, 1, None),
            ),
        ])
    }

    #[test]
    fn scope_none_lists_every_workspace() {
        let out = visible(&scoped_fixture(), Sort::Smart, false, None);
        assert_eq!(ids(&out), vec!["mine", "theirs", "unplaced"]);
    }

    #[test]
    fn a_scoped_list_keeps_its_own_workspace_and_the_unplaced() {
        let out = visible(&scoped_fixture(), Sort::Smart, false, Some("w4"));
        assert_eq!(ids(&out), vec!["mine", "unplaced"]);
    }

    #[test]
    fn the_hidden_idle_count_ignores_other_workspaces() {
        let mut m = scoped_fixture();
        m.insert(
            "mine_idle".to_string(),
            placed(CardState::Idle, 9, Some("w4")),
        );
        m.insert(
            "their_idle".to_string(),
            placed(CardState::Idle, 8, Some("w9")),
        );
        let out = visible(&m, Sort::Smart, true, Some("w4"));
        assert_eq!(ids(&out), vec!["mine", "unplaced"]);
        assert_eq!(
            out.hidden_idle, 1,
            "an idle pane in another workspace is out of scope, not hidden"
        );
    }

    #[test]
    fn smart_ranks_by_state_then_recency() {
        let out = visible(&fixture(), Sort::Smart, false, None);
        assert_eq!(ids(&out), vec!["p2", "p4", "p3", "p1"]);
    }

    #[test]
    fn group_gathers_agents_then_smart_within() {
        let out = visible(&fixture(), Sort::Group, false, None);
        assert_eq!(ids(&out), vec!["p3", "p1", "p2", "p4"]);
    }

    #[test]
    fn hide_idle_filters_and_reports_the_count() {
        let out = visible(&fixture(), Sort::Smart, true, None);
        assert_eq!(out.hidden_idle, 1);
        assert!(!ids(&out).contains(&"p1"));
    }

    #[test]
    fn ordering_is_total_so_frames_do_not_flicker() {
        let mut m = HashMap::new();
        m.insert("b".to_string(), pane("claude", CardState::Idle, 5));
        m.insert("a".to_string(), pane("claude", CardState::Idle, 5));
        assert_eq!(ids(&visible(&m, Sort::Smart, false, None)), vec!["a", "b"]);
    }

    fn ids(v: &Visible) -> Vec<&str> {
        v.panes.iter().map(|(id, _)| id.as_str()).collect()
    }
}
