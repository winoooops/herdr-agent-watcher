//! The settings in force, in one place.
//!
//! They used to be in three: `Interaction.hide_idle`, a `workspace_id` that
//! `view_input` translated into a scope, and the loaded config for the rest.
//! A settings panel that edited the loaded config would have moved nothing on
//! screen for two of its eight rows.

use crate::sidebar::config::{AgentMark, AutoExpand, Loaded, Scope, Theme, ToolCallStyle};
use crate::sidebar::select::Sort;

#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub sort: Sort,
    pub scope: Scope,
    /// The workspace this sidebar is in. Kept even while `scope` is `All`, so
    /// cycling back to `Workspace` does not have to re-resolve it.
    pub workspace: Option<String>,
    pub hide_idle: bool,
    pub auto_expand: AutoExpand,
    pub tool_calls: ToolCallStyle,
    pub trace_lines: u8,
    pub theme: Theme,
    pub agent_mark: AgentMark,
}

impl From<&Loaded> for Live {
    fn from(cfg: &Loaded) -> Self {
        Self {
            sort: cfg.sort,
            scope: cfg.scope,
            workspace: cfg.workspace_id.clone(),
            hide_idle: cfg.hide_idle,
            auto_expand: cfg.auto_expand,
            tool_calls: cfg.tool_calls,
            trace_lines: cfg.trace_lines,
            theme: cfg.theme,
            agent_mark: cfg.agent_mark,
        }
    }
}

impl Live {
    /// What `select::visible` filters on. `None` means every workspace —
    /// derived from the scope rather than from whether a workspace is known,
    /// so returning to `All` actually stops filtering.
    pub fn workspace_filter(&self) -> Option<&str> {
        match self.scope {
            Scope::All => None,
            Scope::Workspace => self.workspace.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::config::{AgentMark, AutoExpand, Loaded, Scope, Theme, ToolCallStyle};
    use crate::sidebar::select::Sort;

    #[test]
    fn live_is_seeded_from_the_loaded_config() {
        let mut cfg = Loaded::from_missing();
        cfg.sort = Sort::Smart;
        cfg.hide_idle = true;
        cfg.trace_lines = 9;
        cfg.scope = Scope::Workspace;
        cfg.workspace_id = Some("w4".into());

        let live = Live::from(&cfg);
        assert_eq!(live.sort, Sort::Smart);
        assert!(live.hide_idle);
        assert_eq!(live.trace_lines, 9);
        assert_eq!(live.scope, Scope::Workspace);
        assert_eq!(live.workspace.as_deref(), Some("w4"));
    }

    #[test]
    fn the_workspace_filter_follows_the_scope() {
        let mut live = Live::from(&Loaded::from_missing());
        assert_eq!(live.workspace_filter(), None, "scope defaults to all");

        live.scope = Scope::Workspace;
        live.workspace = Some("w4".into());
        assert_eq!(live.workspace_filter(), Some("w4"));

        // `resolve_scope` cannot do this: it returns early unless the scope is
        // already Workspace, so it can set the workspace but never clear it.
        live.scope = Scope::All;
        assert_eq!(
            live.workspace_filter(),
            None,
            "returning to all must stop filtering, whatever the workspace field holds"
        );
    }
}
