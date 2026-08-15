//! Tolerant config loading (§4). A derive would reject the whole document on
//! one bad enum; this walks the tree key by key so a mistake costs one key.

use std::path::PathBuf;

use toml::Value;

use crate::sidebar::agent_ids;
use crate::sidebar::format;
use crate::sidebar::select::Sort;
use crate::sidebar::style::{AgentAppearances, ConfigStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Inherit,
    Lumon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMark {
    #[default]
    Dot,
    Initial,
    Symbol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoExpand {
    #[default]
    None,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolCallStyle {
    #[default]
    Bars,
    Jar,
}

pub struct Loaded {
    pub theme: Theme,
    pub agent_mark: AgentMark,
    pub auto_expand: AutoExpand,
    pub tool_calls: ToolCallStyle,
    pub trace_lines: u8,
    pub sort: Sort,
    pub hide_idle: bool,
    pub appearances: AgentAppearances,
    pub status: ConfigStatus,
    pub problem_details: Vec<String>,
}

impl Default for Loaded {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            agent_mark: AgentMark::default(),
            auto_expand: AutoExpand::default(),
            tool_calls: ToolCallStyle::default(),
            trace_lines: 5,
            sort: Sort::default(),
            hide_idle: false,
            appearances: builtin_appearances(),
            status: ConfigStatus {
                problems: 0,
                log_written: false,
            },
            problem_details: Vec::new(),
        }
    }
}

fn builtin_appearances() -> AgentAppearances {
    agent_ids::CANONICAL_IDS
        .iter()
        .filter_map(|id| agent_ids::appearance(id).map(|a| ((*id).to_string(), a)))
        .collect()
}

pub fn config_path() -> Option<PathBuf> {
    std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").map(|d| PathBuf::from(d).join("config.toml"))
}

impl Loaded {
    pub fn from_missing() -> Self {
        Self::default()
    }

    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::from_missing();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_toml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::from_missing(),
            Err(e) => {
                let mut out = Self::default();
                out.problem(format!("config.toml unreadable: {e}"));
                out
            }
        }
    }

    fn problem(&mut self, detail: String) {
        self.status.problems += 1;
        self.problem_details.push(detail);
    }

    /// Best-effort detail log (§4.3). A missing, read-only, or full state
    /// directory changes only the notice wording; it is not a config problem.
    pub fn write_problem_log(&mut self) {
        let dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR");

        if self.status.problems == 0 {
            if let Some(dir) = dir {
                let path = PathBuf::from(dir).join("config-problems.log");
                if path.exists() {
                    let _ = std::fs::remove_file(&path).or_else(|_| std::fs::write(&path, ""));
                }
            }
            return;
        }

        let Some(dir) = dir else {
            self.status.log_written = false;
            return;
        };
        let path = PathBuf::from(dir).join("config-problems.log");
        let body = self.problem_details.join("\n") + "\n";
        self.status.log_written = std::fs::write(&path, body).is_ok();
    }

    pub fn from_toml(text: &str) -> Self {
        let mut out = Self::default();
        let root: Value = match text.parse() {
            Ok(v) => v,
            Err(e) => {
                out.problem(format!("config.toml unparseable: {e}"));
                return out;
            }
        };
        let Some(table) = root.as_table() else {
            out.problem("config.toml is not a table".into());
            return out;
        };

        for (key, value) in table {
            match key.as_str() {
                "appearance" => out.read_appearance(value),
                "cards" => out.read_cards(value),
                "list" => out.read_list(value),
                "agent" => out.read_agents(value),
                other => out.problem(format!("unknown table [{other}]")),
            }
        }
        out.validate_marks();
        out
    }

    fn validate_marks(&mut self) {
        if self.agent_mark != AgentMark::Initial {
            return;
        }
        let bad: Vec<String> = self
            .appearances
            .iter()
            .filter(|(_, a)| initial_mark(&a.label).is_none())
            .map(|(id, _)| id.clone())
            .collect();
        for id in bad {
            self.problem(format!(
                "agent_mark = initial cannot render for {id}; using ●"
            ));
        }
    }

    fn read_appearance(&mut self, v: &Value) {
        let Some(t) = v.as_table() else {
            return self.problem("[appearance] is not a table".into());
        };
        for (k, val) in t {
            match (k.as_str(), val.as_str()) {
                ("theme", Some("inherit")) => self.theme = Theme::Inherit,
                ("theme", Some("lumon")) => self.theme = Theme::Lumon,
                ("agent_mark", Some("dot")) => self.agent_mark = AgentMark::Dot,
                ("agent_mark", Some("initial")) => self.agent_mark = AgentMark::Initial,
                ("agent_mark", Some("symbol")) => self.agent_mark = AgentMark::Symbol,
                ("theme", _) | ("agent_mark", _) => {
                    self.problem(format!("invalid value for appearance.{k}"))
                }
                _ => self.problem(format!("unknown key appearance.{k}")),
            }
        }
    }

    fn read_cards(&mut self, v: &Value) {
        let Some(t) = v.as_table() else {
            return self.problem("[cards] is not a table".into());
        };
        for (k, val) in t {
            match k.as_str() {
                "auto_expand" => match val.as_str() {
                    Some("none") => self.auto_expand = AutoExpand::None,
                    Some("all") => self.auto_expand = AutoExpand::All,
                    _ => self.problem("invalid value for cards.auto_expand".into()),
                },
                "tool_calls" => match val.as_str() {
                    Some("bars") => self.tool_calls = ToolCallStyle::Bars,
                    Some("jar") => self.tool_calls = ToolCallStyle::Jar,
                    _ => self.problem("invalid value for cards.tool_calls".into()),
                },
                "trace_lines" => match val.as_integer() {
                    Some(n) if (1..=20).contains(&n) => self.trace_lines = n as u8,
                    Some(n) => {
                        self.trace_lines = n.clamp(1, 20) as u8;
                        self.problem(format!("cards.trace_lines {n} clamped to 1..=20"));
                    }
                    None => self.problem("invalid value for cards.trace_lines".into()),
                },
                other => self.problem(format!("unknown key cards.{other}")),
            }
        }
    }

    fn read_list(&mut self, v: &Value) {
        let Some(t) = v.as_table() else {
            return self.problem("[list] is not a table".into());
        };
        for (k, val) in t {
            match k.as_str() {
                "sort" => match val.as_str() {
                    Some("smart") => self.sort = Sort::Smart,
                    Some("group") => self.sort = Sort::Group,
                    _ => self.problem("invalid value for list.sort".into()),
                },
                "hide_idle" => match val.as_bool() {
                    Some(b) => self.hide_idle = b,
                    None => self.problem("invalid value for list.hide_idle".into()),
                },
                other => self.problem(format!("unknown key list.{other}")),
            }
        }
    }

    fn read_agents(&mut self, v: &Value) {
        let Some(t) = v.as_table() else {
            return self.problem("[agent] is not a table".into());
        };
        let mut winner: std::collections::BTreeMap<&'static str, (&String, &Value)> =
            std::collections::BTreeMap::new();
        for (id, val) in t {
            let Some(canonical) = agent_ids::canonical(id) else {
                self.problem(format!("unknown agent id [agent.{id}]"));
                continue;
            };
            match winner.get(canonical) {
                None => {
                    winner.insert(canonical, (id, val));
                }
                Some((held, _)) => {
                    self.problem(format!(
                        "[agent.{id}] also configures {canonical}; the canonical table wins"
                    ));
                    if id.as_str() == canonical && held.as_str() != canonical {
                        winner.insert(canonical, (id, val));
                    }
                }
            }
        }

        for (canonical, (id, val)) in winner {
            let Some(fields) = val.as_table() else {
                self.problem(format!("[agent.{id}] is not a table"));
                continue;
            };
            for (k, fv) in fields {
                match k.as_str() {
                    "color" => match fv.as_str().and_then(parse_hex) {
                        Some(rgb) => {
                            if let Some(a) = self.appearances.get_mut(canonical) {
                                a.rgb = rgb;
                            }
                        }
                        None => self.problem(format!("invalid colour for [agent.{id}]")),
                    },
                    "label" => {
                        let clean = format::sanitise(fv.as_str().unwrap_or(""));
                        if clean.is_empty() {
                            self.problem(format!("empty label for [agent.{id}]"));
                        } else if let Some(a) = self.appearances.get_mut(canonical) {
                            a.label = clean;
                        }
                    }
                    "symbol" => {
                        let clean = format::sanitise(fv.as_str().unwrap_or(""));
                        if format::width(&clean) == 1 {
                            if let Some(a) = self.appearances.get_mut(canonical) {
                                a.symbol = Some(clean);
                            }
                        } else {
                            self.problem(format!("symbol for [agent.{id}] must be one cell"));
                        }
                    }
                    other => self.problem(format!("unknown key agent.{id}.{other}")),
                }
            }
        }
    }
}

pub fn initial_mark(label: &str) -> Option<String> {
    let initial: String = label.chars().next()?.to_uppercase().collect();
    (format::width(&initial) == 1).then_some(initial)
}

fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        u8::from_str_radix(&h[0..2], 16).ok()?,
        u8::from_str_radix(&h[2..4], 16).ok()?,
        u8::from_str_radix(&h[4..6], 16).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_str(s: &str) -> Loaded {
        Loaded::from_toml(s)
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<T>(vars: &[(&str, Option<PathBuf>)], body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(String, Option<std::ffi::OsString>)> = vars
            .iter()
            .map(|(key, _)| ((*key).to_string(), std::env::var_os(key)))
            .collect();
        for (key, value) in vars {
            match value {
                Some(path) => std::env::set_var(key, path),
                None => std::env::remove_var(key),
            }
        }
        let out = body();
        for (key, value) in saved {
            match value {
                Some(value) => std::env::set_var(&key, value),
                None => std::env::remove_var(&key),
            }
        }
        out
    }

    #[test]
    fn a_missing_file_is_all_defaults_and_no_problem() {
        let l = Loaded::from_missing();
        assert_eq!(l.status.problems, 0);
        assert_eq!(l.sort, crate::sidebar::select::Sort::Smart);
        assert!(!l.hide_idle);
    }

    #[test]
    fn an_unparseable_file_is_one_problem_and_all_defaults() {
        let l = load_str("this is not toml {{{");
        assert_eq!(l.status.problems, 1);
        assert_eq!(l.trace_lines, 5);
    }

    #[test]
    fn one_bad_value_defaults_only_that_key() {
        let l = load_str("[list]\nsort = \"Smart\"\nhide_idle = true\n");
        assert_eq!(l.status.problems, 1, "case-folding is not a guess");
        assert_eq!(l.sort, crate::sidebar::select::Sort::Smart);
        assert!(l.hide_idle, "the valid key still applies");
    }

    #[test]
    fn unknown_keys_and_tables_are_counted_not_silently_ignored() {
        let l = load_str("[list]\nsort = \"group\"\nnonsense = 1\n\n[bogus]\nx = 1\n");
        assert_eq!(l.status.problems, 2);
        assert_eq!(l.sort, crate::sidebar::select::Sort::Group);
    }

    #[test]
    fn trace_lines_clamps_and_counts() {
        let l = load_str("[cards]\ntrace_lines = 99\n");
        assert_eq!(l.trace_lines, 20);
        assert_eq!(l.status.problems, 1);
    }

    #[test]
    fn the_problem_log_is_best_effort_and_reports_its_own_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        with_env(
            &[("HERDR_PLUGIN_STATE_DIR", Some(dir.path().to_path_buf()))],
            || {
                let mut l = load_str("[list]\nsort = \"Smart\"\n");
                l.write_problem_log();
                assert!(l.status.log_written);
                assert!(dir.path().join("config-problems.log").exists());
            },
        );

        with_env(
            &[(
                "HERDR_PLUGIN_STATE_DIR",
                Some(dir.path().join("nope/deeper")),
            )],
            || {
                let mut l = load_str("[list]\nsort = \"Smart\"\n");
                let before = l.status.problems;
                l.write_problem_log();
                assert!(!l.status.log_written, "the notice says (log unavailable)");
                assert_eq!(
                    l.status.problems, before,
                    "the count is config mistakes; an unrelated write failure is not one"
                );
            },
        );
    }

    #[test]
    fn a_missing_file_and_an_unreadable_one_are_different_answers() {
        let dir = tempfile::tempdir().expect("tempdir");
        with_env(
            &[("HERDR_PLUGIN_CONFIG_DIR", Some(dir.path().to_path_buf()))],
            || {
                assert_eq!(Loaded::load().status.problems, 0);
                std::fs::create_dir(dir.path().join("config.toml")).expect("mkdir");
                assert_eq!(
                    Loaded::load().status.problems,
                    1,
                    "unreadable is not missing"
                );
            },
        );
    }

    #[test]
    fn a_fixed_config_clears_the_previous_runs_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        with_env(
            &[("HERDR_PLUGIN_STATE_DIR", Some(dir.path().to_path_buf()))],
            || {
                let log = dir.path().join("config-problems.log");
                load_str("[list]\nsort = \"Smart\"\n").write_problem_log();
                assert!(log.exists());
                let mut good = load_str("[list]\nsort = \"smart\"\n");
                assert_eq!(good.status.problems, 0);
                good.write_problem_log();
                assert!(
                    !log.exists(),
                    "a fixed config must stop being accused (§4.3)"
                );
            },
        );
    }

    #[test]
    fn no_state_directory_changes_the_wording_not_the_count() {
        with_env(&[("HERDR_PLUGIN_STATE_DIR", None)], || {
            let mut l = load_str("[list]\nsort = \"Smart\"\n");
            l.write_problem_log();
            assert!(!l.status.log_written, "the notice says (log unavailable)");
            assert_eq!(
                l.status.problems, 1,
                "the same two typos must not read as three problems on a machine \
                 whose state directory happens to be unwritable (§4.3)"
            );

            let mut good = load_str("[list]\nsort = \"smart\"\n");
            good.write_problem_log();
            assert_eq!(good.status.problems, 0);
        });
    }

    #[test]
    fn agent_overrides_normalise_aliases_and_reject_typos() {
        let l = load_str(
            "[agent.claude-code]\ncolor = \"#123456\"\n\n[agent.claud]\ncolor = \"#000000\"\n",
        );
        assert_eq!(l.appearances["claude"].rgb, (0x12, 0x34, 0x56));
        assert_eq!(l.appearances["claude"].ansi, 1, "override changes RGB only");
        assert_eq!(l.status.problems, 1, "the typo is counted");
    }

    #[test]
    fn configuring_both_spellings_counts_the_duplicate_and_the_canonical_wins() {
        let both =
            "[agent.claude-code]\ncolor = \"#111111\"\n\n[agent.claude]\ncolor = \"#222222\"\n";
        let l = load_str(both);
        assert_eq!(l.appearances["claude"].rgb, (0x22, 0x22, 0x22));
        assert_eq!(l.status.problems, 1, "counted, not fatal");

        let reversed =
            "[agent.claude]\ncolor = \"#222222\"\n\n[agent.claude-code]\ncolor = \"#111111\"\n";
        let l = load_str(reversed);
        assert_eq!(l.appearances["claude"].rgb, (0x22, 0x22, 0x22));
        assert_eq!(l.status.problems, 1);
    }

    #[test]
    fn an_invalid_canonical_value_defaults_instead_of_falling_back_to_the_alias() {
        let l = load_str(
            "[agent.claude-code]\ncolor = \"#111111\"\n\n[agent.claude]\ncolor = \"nonsense\"\n",
        );
        assert_eq!(
            l.appearances["claude"].rgb,
            (0xd9, 0x77, 0x57),
            "the BUILT-IN colour: the canonical table won, and its value was invalid"
        );
        assert_eq!(
            l.status.problems, 2,
            "the duplicate table and the bad colour"
        );
    }

    #[test]
    fn an_unrenderable_initial_is_counted_at_load_not_swallowed_at_draw() {
        let l = load_str(
            "[appearance]\nagent_mark = \"initial\"\n\n[agent.claude]\nlabel = \"猫猫\"\n",
        );
        assert_eq!(
            l.status.problems, 1,
            "two cells cannot be the mark, and the user is told"
        );
        assert_eq!(
            l.appearances["claude"].label, "猫猫",
            "the label itself is perfectly valid"
        );
    }

    #[test]
    fn an_unrenderable_initial_costs_nothing_when_that_mark_is_not_in_use() {
        let l = load_str("[agent.claude]\nlabel = \"猫猫\"\n");
        assert_eq!(
            l.status.problems, 0,
            "the default mark is a dot; nothing is wrong"
        );
    }

    #[test]
    fn a_malformed_colour_never_panics() {
        let l = load_str("[agent.claude]\ncolor = \"#猫猫\"\n");
        assert_eq!(
            l.appearances["claude"].rgb,
            (0xd9, 0x77, 0x57),
            "built-in retained"
        );
        assert_eq!(l.status.problems, 1);
    }

    #[test]
    fn an_empty_label_is_invalid_and_falls_back() {
        let l = load_str("[agent.claude]\nlabel = \"   \"\n");
        assert_eq!(l.appearances["claude"].label, "CLAUDE");
        assert_eq!(l.status.problems, 1);
    }

    #[test]
    fn a_two_cell_symbol_is_rejected() {
        let l = load_str("[agent.claude]\nsymbol = \"猫\"\n");
        assert!(l.appearances["claude"].symbol.is_none());
        assert_eq!(l.status.problems, 1);
    }
}
