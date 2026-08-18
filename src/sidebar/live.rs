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
    pub plan_usage: bool,
    pub theme: Theme,
    pub agent_mark: AgentMark,
    /// The daemon's, not the sidebar's. Editable and saveable, but it cannot
    /// take effect here -- the daemon reads it once at startup, so the row
    /// carries a warning rather than pretending the change is live.
    pub interval_ms: u32,
    pub prune_after_days: u32,
}

/// Move one rung, clamped. A value not on the ladder -- hand-written in the
/// file -- snaps to the nearest rung in the direction asked for.
fn step_ladder(ladder: &[u32], current: u32, direction: i32) -> u32 {
    // The strictly-next rung in the direction asked for. Written this way
    // rather than as an index step because a value hand-written into the file
    // need not be on the ladder at all -- and "the rung at or above 1500" is
    // 2000, which is the wrong answer for a decrease.
    if direction > 0 {
        ladder
            .iter()
            .find(|value| **value > current)
            .copied()
            .unwrap_or(ladder[ladder.len() - 1])
    } else {
        ladder
            .iter()
            .rev()
            .find(|value| **value < current)
            .copied()
            .unwrap_or(ladder[0])
    }
}

/// The intervals the panel offers. A ladder rather than free entry: `h`/`l`
/// step a value, and a list of sensible reconciliation periods is what
/// someone is choosing between.
pub const INTERVALS_MS: [u32; 7] = [250, 500, 1000, 2000, 3000, 5000, 10_000];
const PRUNE_AFTER_DAYS: [u32; 4] = [
    0,
    crate::daemon::prune::RETENTIONS_DAYS[0],
    crate::daemon::prune::RETENTIONS_DAYS[1],
    crate::daemon::prune::RETENTIONS_DAYS[2],
];

pub const DEFAULT_INTERVAL_MS: u32 = 1000;

impl Live {
    /// The daemon's environment-overridable interval arrives separately.
    pub fn from_config(cfg: &Loaded, daemon_interval_ms: u32) -> Self {
        Self::build(cfg, daemon_interval_ms)
    }
}

impl From<&Loaded> for Live {
    fn from(cfg: &Loaded) -> Self {
        Self::build(cfg, DEFAULT_INTERVAL_MS)
    }
}

impl Live {
    fn build(cfg: &Loaded, daemon_interval_ms: u32) -> Self {
        Self {
            sort: cfg.sort,
            scope: cfg.scope,
            workspace: cfg.workspace_id.clone(),
            hide_idle: cfg.hide_idle,
            auto_expand: cfg.auto_expand,
            tool_calls: cfg.tool_calls,
            trace_lines: cfg.trace_lines,
            plan_usage: cfg.plan_usage,
            theme: cfg.theme,
            agent_mark: cfg.agent_mark,
            interval_ms: daemon_interval_ms,
            prune_after_days: cfg.prune_after_days,
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
    PlanUsage,
    Theme,
    AgentMark,
    IntervalMs,
    PruneAfterDays,
}

pub const SETTINGS: [Setting; 11] = [
    Setting::Sort,
    Setting::Scope,
    Setting::HideIdle,
    Setting::AutoExpand,
    Setting::ToolCalls,
    Setting::TraceLines,
    Setting::PlanUsage,
    Setting::Theme,
    Setting::AgentMark,
    Setting::IntervalMs,
    Setting::PruneAfterDays,
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
            Setting::PlanUsage => "plan usage",
            Setting::Theme => "theme",
            Setting::AgentMark => "agent mark",
            Setting::IntervalMs => "interval ms",
            Setting::PruneAfterDays => "prune after days",
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
            Setting::PlanUsage => if self.plan_usage { "yes" } else { "no" }.into(),
            Setting::Theme => match self.theme {
                Theme::Inherit => "inherit",
                Theme::Lumon => "lumon",
            }
            .into(),
            Setting::IntervalMs => self.interval_ms.to_string(),
            Setting::PruneAfterDays if self.prune_after_days == 0 => "off".into(),
            Setting::PruneAfterDays => self.prune_after_days.to_string(),
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
            Setting::PlanUsage => self.plan_usage = !self.plan_usage,
            Setting::IntervalMs => {
                self.interval_ms = step_ladder(&INTERVALS_MS, self.interval_ms, 1)
            }
            Setting::PruneAfterDays => {
                self.prune_after_days = step_ladder(&PRUNE_AFTER_DAYS, self.prune_after_days, 1)
            }
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
            Setting::IntervalMs => {
                self.interval_ms = step_ladder(&INTERVALS_MS, self.interval_ms, -1)
            }
            Setting::PruneAfterDays => {
                self.prune_after_days = step_ladder(&PRUNE_AFTER_DAYS, self.prune_after_days, -1)
            }
            other => self.cycle(other, workspace),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::config::{Loaded, Scope};
    use crate::sidebar::select::Sort;

    #[test]
    fn live_is_seeded_from_the_loaded_config() {
        let mut cfg = Loaded::from_missing();
        cfg.sort = Sort::Smart;
        cfg.hide_idle = true;
        cfg.trace_lines = 9;
        cfg.plan_usage = false;
        cfg.scope = Scope::Workspace;
        cfg.workspace_id = Some("w4".into());
        cfg.prune_after_days = 30;

        let live = Live::from(&cfg);
        assert_eq!(live.sort, Sort::Smart);
        assert!(live.hide_idle);
        assert_eq!(live.trace_lines, 9);
        assert!(!live.plan_usage);
        assert_eq!(live.scope, Scope::Workspace);
        assert_eq!(live.workspace.as_deref(), Some("w4"));
        assert_eq!(live.prune_after_days, 30);
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
    fn the_interval_steps_a_ladder_and_clamps_at_both_ends() {
        let mut live = Live::from_config(&Loaded::from_missing(), 1000);
        live.cycle(Setting::IntervalMs, None);
        assert_eq!(live.interval_ms, 2000);
        live.cycle_back(Setting::IntervalMs, None);
        assert_eq!(live.interval_ms, 1000);

        live.interval_ms = 10_000;
        live.cycle(Setting::IntervalMs, None);
        assert_eq!(live.interval_ms, 10_000, "clamped at the top");
        live.interval_ms = 250;
        live.cycle_back(Setting::IntervalMs, None);
        assert_eq!(live.interval_ms, 250, "and at the bottom");
    }

    /// A value hand-written into the file need not be on the ladder.
    #[test]
    fn an_off_ladder_interval_snaps_toward_the_step_asked_for() {
        let mut live = Live::from_config(&Loaded::from_missing(), 1500);
        live.cycle_back(Setting::IntervalMs, None);
        assert_eq!(live.interval_ms, 1000);

        let mut live = Live::from_config(&Loaded::from_missing(), 1500);
        live.cycle(Setting::IntervalMs, None);
        assert_eq!(
            live.interval_ms, 2000,
            "the next rung up, not the one after"
        );
    }

    #[test]
    fn both_ladders_take_the_strictly_next_rung_in_the_direction_asked_for() {
        assert_eq!(step_ladder(&INTERVALS_MS, 1500, -1), 1000);
        assert_eq!(step_ladder(&[0, 7, 14, 30], 10, -1), 7);
        assert_eq!(step_ladder(&[0, 7, 14, 30], 10, 1), 14);
    }

    #[test]
    fn pruning_cycles_to_off_and_names_it() {
        let mut live = Live::from(&Loaded::from_missing());
        live.cycle_back(Setting::PruneAfterDays, None);
        assert_eq!(live.prune_after_days, 0);
        assert_eq!(live.value(Setting::PruneAfterDays), "off");
    }

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
