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

/// The rows the settings panel owns, in the order it shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Sort,
    Scope,
    HideIdle,
    AutoExpand,
    ToolCalls,
    TraceLines,
    Theme,
    AgentMark,
}

pub const SETTINGS: [Setting; 8] = [
    Setting::Sort,
    Setting::Scope,
    Setting::HideIdle,
    Setting::AutoExpand,
    Setting::ToolCalls,
    Setting::TraceLines,
    Setting::Theme,
    Setting::AgentMark,
];

impl Setting {
    pub fn label(self) -> &'static str {
        match self {
            Setting::Sort => "sort",
            Setting::Scope => "scope",
            Setting::HideIdle => "hide idle",
            Setting::AutoExpand => "auto expand",
            Setting::ToolCalls => "tool calls",
            Setting::TraceLines => "trace lines",
            Setting::Theme => "theme",
            Setting::AgentMark => "agent mark",
        }
    }
}

impl Live {
    pub fn value(&self, setting: Setting) -> String {
        match setting {
            Setting::Sort => match self.sort {
                Sort::Position => "position",
                Sort::Smart => "smart",
                Sort::Group => "group",
            }
            .into(),
            Setting::Scope => match self.scope {
                Scope::All => "all",
                Scope::Workspace => "workspace",
            }
            .into(),
            Setting::HideIdle => if self.hide_idle { "yes" } else { "no" }.into(),
            Setting::AutoExpand => match self.auto_expand {
                AutoExpand::None => "none",
                AutoExpand::All => "all",
            }
            .into(),
            Setting::ToolCalls => match self.tool_calls {
                ToolCallStyle::Bars => "bars",
                ToolCallStyle::Jar => "jar",
            }
            .into(),
            Setting::TraceLines => self.trace_lines.to_string(),
            Setting::Theme => match self.theme {
                Theme::Inherit => "inherit",
                Theme::Lumon => "lumon",
            }
            .into(),
            Setting::AgentMark => match self.agent_mark {
                AgentMark::Dot => "dot",
                AgentMark::Initial => "initial",
                AgentMark::Symbol => "symbol",
            }
            .into(),
        }
    }

    pub fn cycle(&mut self, setting: Setting, workspace: Option<&str>) {
        match setting {
            Setting::Sort => {
                self.sort = match self.sort {
                    Sort::Position => Sort::Smart,
                    Sort::Smart => Sort::Group,
                    Sort::Group => Sort::Position,
                }
            }
            // The one setting that needs something from outside itself. Taking
            // it as a parameter keeps this module pure and its tests free of
            // the process environment.
            Setting::Scope => match (self.scope, workspace) {
                (Scope::All, Some(id)) => {
                    self.scope = Scope::Workspace;
                    self.workspace = Some(id.to_string());
                }
                // Same reasoning as `resolve_scope`: an empty sidebar reads as
                // broken rather than misconfigured.
                (Scope::All, None) => {}
                (Scope::Workspace, _) => {
                    self.scope = Scope::All;
                    self.workspace = None;
                }
            },
            Setting::HideIdle => self.hide_idle = !self.hide_idle,
            Setting::AutoExpand => {
                self.auto_expand = match self.auto_expand {
                    AutoExpand::None => AutoExpand::All,
                    AutoExpand::All => AutoExpand::None,
                }
            }
            Setting::ToolCalls => {
                self.tool_calls = match self.tool_calls {
                    ToolCallStyle::Bars => ToolCallStyle::Jar,
                    ToolCallStyle::Jar => ToolCallStyle::Bars,
                }
            }
            // Clamped, not wrapped: a held key must not jump 20 → 1.
            Setting::TraceLines => self.trace_lines = (self.trace_lines + 1).min(20),
            Setting::Theme => {
                self.theme = match self.theme {
                    Theme::Inherit => Theme::Lumon,
                    Theme::Lumon => Theme::Inherit,
                }
            }
            Setting::AgentMark => {
                self.agent_mark = match self.agent_mark {
                    AgentMark::Dot => AgentMark::Initial,
                    AgentMark::Initial => AgentMark::Symbol,
                    AgentMark::Symbol => AgentMark::Dot,
                }
            }
        }
    }

    /// Only `trace_lines` has a meaningful reverse; everything else wraps, so
    /// stepping back is stepping forward.
    pub fn cycle_back(&mut self, setting: Setting, workspace: Option<&str>) {
        match setting {
            Setting::TraceLines => self.trace_lines = self.trace_lines.saturating_sub(1).max(1),
            other => self.cycle(other, workspace),
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

    #[test]
    fn cycling_wraps_for_enumerations() {
        let mut live = Live::from(&Loaded::from_missing());
        assert_eq!(live.sort, Sort::Position);
        live.cycle(Setting::Sort, None);
        assert_eq!(live.sort, Sort::Smart);
        live.cycle(Setting::Sort, None);
        assert_eq!(live.sort, Sort::Group);
        live.cycle(Setting::Sort, None);
        assert_eq!(live.sort, Sort::Position, "and back round");
    }

    /// A wrap here turns a held key into a silent jump from 20 to 1.
    #[test]
    fn trace_lines_clamps_rather_than_wrapping() {
        let mut live = Live::from(&Loaded::from_missing());
        live.trace_lines = 19;
        live.cycle(Setting::TraceLines, None);
        assert_eq!(live.trace_lines, 20);
        live.cycle(Setting::TraceLines, None);
        assert_eq!(live.trace_lines, 20, "clamped at the top");

        live.trace_lines = 1;
        live.cycle_back(Setting::TraceLines, None);
        assert_eq!(live.trace_lines, 1, "and at the bottom");
    }

    /// From a DEFAULT Live, which is what the sidebar starts with when the
    /// config says nothing: `Loaded` holds no workspace while the scope is
    /// `All`, so cycling into workspace scope has to resolve one or the filter
    /// silently stays off.
    #[test]
    fn cycling_into_workspace_scope_resolves_the_workspace() {
        let mut live = Live::from(&Loaded::from_missing());
        assert_eq!(live.workspace_filter(), None);

        live.cycle(Setting::Scope, Some("w4"));
        assert_eq!(live.scope, Scope::Workspace);
        assert_eq!(
            live.workspace_filter(),
            Some("w4"),
            "seeded from the environment"
        );

        live.cycle(Setting::Scope, Some("w4"));
        assert_eq!(live.scope, Scope::All);
        assert_eq!(live.workspace_filter(), None);
        assert_eq!(live.workspace, None, "and cleared, as the spec says");
    }

    #[test]
    fn workspace_scope_with_no_workspace_stays_all() {
        let mut live = Live::from(&Loaded::from_missing());
        live.cycle(Setting::Scope, None);
        assert_eq!(
            live.scope,
            Scope::All,
            "filtering against an unknown workspace matches nothing, and an \
             empty sidebar reads as broken rather than misconfigured"
        );
    }
}
