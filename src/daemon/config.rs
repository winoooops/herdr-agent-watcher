//! The daemon's slice of the plugin config (§2). It parses `[daemon]` and
//! nothing else: the sidebar's loader is sidebar-shaped, the two run in
//! separate processes, and neither can read for the other.
//!
//! Problems are returned, not written. `config-problems.log` belongs to the
//! sidebar, which truncates it when there are problems and DELETES it when
//! there are none — a second writer would erase the other's diagnostics.
//! `doctor` is what surfaces these.

use std::time::Duration;

use toml::Value;

pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub interval: Duration,
    /// How long a session directory may go unwritten before the sweep takes
    /// it. `None` turns the sweep off entirely, which is a different decision
    /// from keeping things for a long time.
    pub prune_after: Option<Duration>,
    pub problems: Vec<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            prune_after: Some(Duration::from_secs(
                u64::from(crate::daemon::prune::DEFAULT_RETENTION_DAYS) * 86_400,
            )),
            problems: Vec::new(),
        }
    }
}

impl DaemonConfig {
    pub fn load() -> Self {
        let Some(path) = crate::sidebar::config::config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_toml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => Self {
                problems: vec![format!("config.toml unreadable: {e}")],
                ..Self::default()
            },
        }
    }

    /// Tolerant by construction (§4): every path ends in a default, so a bad
    /// config cannot stop the daemon starting — and the daemon starting is
    /// what makes the problem visible.
    pub fn from_toml(text: &str) -> Self {
        let mut out = Self::default();
        let root: Value = match text.parse() {
            Ok(v) => v,
            Err(e) => {
                out.problems.push(format!("config.toml unparseable: {e}"));
                return out;
            }
        };
        let Some(table) = root.as_table() else {
            out.problems.push("config.toml is not a table".into());
            return out;
        };
        let Some(daemon) = table.get("daemon") else {
            return out;
        };
        let Some(daemon) = daemon.as_table() else {
            out.problems.push("[daemon] is not a table".into());
            return out;
        };
        for (key, value) in daemon {
            match key.as_str() {
                "interval_ms" => match value.as_integer() {
                    Some(ms) if ms > 0 => out.interval = Duration::from_millis(ms as u64),
                    Some(ms) => out.problems.push(format!(
                        "daemon.interval_ms must be positive, found {ms}; using 1000"
                    )),
                    None => out.problems.push(format!(
                        "daemon.interval_ms must be an integer, found {value}; using 1000"
                    )),
                },
                // Days, not milliseconds: the unit a reader would answer
                // "how long do you keep these?" in.
                "prune_after_days" => match value.as_integer() {
                    Some(0) => out.prune_after = None,
                    Some(days) if days > 0 => {
                        out.prune_after = Some(Duration::from_secs(days as u64 * 86_400))
                    }
                    Some(days) => out.problems.push(format!(
                        "daemon.prune_after_days cannot be negative, found {days}; using {}",
                        crate::daemon::prune::DEFAULT_RETENTION_DAYS
                    )),
                    None => out.problems.push(format!(
                        "daemon.prune_after_days must be an integer, found {value}; using {}",
                        crate::daemon::prune::DEFAULT_RETENTION_DAYS
                    )),
                },
                other => out.problems.push(format!("unknown key daemon.{other}")),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::with_env;
    use std::time::Duration;

    const DEFAULT: Duration = Duration::from_millis(1000);

    #[test]
    fn an_interval_is_read_from_the_daemon_table() {
        let c = DaemonConfig::from_toml("[daemon]\ninterval_ms = 5000\n");
        assert_eq!(c.interval, Duration::from_millis(5000));
        assert!(c.problems.is_empty());
    }

    #[test]
    fn an_absent_file_or_key_is_the_default_and_no_problem() {
        for text in ["", "[daemon]\n"] {
            let c = DaemonConfig::from_toml(text);
            assert_eq!(c.interval, DEFAULT, "{text:?}");
            assert!(c.problems.is_empty(), "{text:?}");
        }
    }

    #[test]
    fn a_rejected_interval_names_the_key_and_the_fallback() {
        for text in [
            "[daemon]\ninterval_ms = 0\n",
            "[daemon]\ninterval_ms = -1\n",
            "[daemon]\ninterval_ms = \"fast\"\n",
        ] {
            let c = DaemonConfig::from_toml(text);
            assert_eq!(c.interval, DEFAULT, "{text}");
            assert_eq!(c.problems.len(), 1, "{text}");
            let message = &c.problems[0];
            assert!(message.contains("daemon.interval_ms"), "{message}");
            assert!(message.contains("1000"), "{message}");
        }
    }

    #[test]
    fn a_broken_document_is_the_default_and_one_problem() {
        for text in ["not toml {{{", "[daemon]\nnope = 1\n"] {
            let c = DaemonConfig::from_toml(text);
            assert_eq!(c.interval, DEFAULT, "{text}");
            assert_eq!(c.problems.len(), 1, "{text}");
        }
    }

    #[test]
    fn other_tables_are_not_the_daemons_problem() {
        let c = DaemonConfig::from_toml(
            "[appearance]\ntheme = \"lumon\"\n[list]\nscope = \"workspace\"\n",
        );
        assert_eq!(c.interval, DEFAULT);
        assert!(c.problems.is_empty(), "{:?}", c.problems);
    }

    #[test]
    fn load_reads_the_plugin_config_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[daemon]\ninterval_ms = 2500\n",
        )
        .expect("write");
        with_env(
            &[("HERDR_PLUGIN_CONFIG_DIR", Some(dir.path().into()))],
            || assert_eq!(DaemonConfig::load().interval, Duration::from_millis(2500)),
        );
    }

    #[test]
    fn an_unreadable_file_is_the_default_and_one_problem() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("config.toml")).expect("mkdir");
        with_env(
            &[("HERDR_PLUGIN_CONFIG_DIR", Some(dir.path().into()))],
            || {
                let c = DaemonConfig::load();
                assert_eq!(c.interval, DEFAULT);
                assert_eq!(c.problems.len(), 1);
            },
        );
    }
    #[test]
    fn a_retention_is_read_in_days() {
        let c = DaemonConfig::from_toml("[daemon]\nprune_after_days = 30\n");
        assert_eq!(c.prune_after, Some(Duration::from_secs(30 * 86_400)));
        assert!(c.problems.is_empty(), "{:?}", c.problems);
    }

    /// Zero is the off switch, not a retention of no time at all -- which
    /// would sweep every session directory the moment it stopped being
    /// written to.
    #[test]
    fn zero_turns_the_sweep_off_rather_than_deleting_everything() {
        let c = DaemonConfig::from_toml("[daemon]\nprune_after_days = 0\n");
        assert_eq!(c.prune_after, None);
        assert!(c.problems.is_empty(), "{:?}", c.problems);
    }

    #[test]
    fn a_retention_that_makes_no_sense_keeps_the_default_and_says_so() {
        for text in [
            "[daemon]\nprune_after_days = -1\n",
            "[daemon]\nprune_after_days = \"a week\"\n",
        ] {
            let c = DaemonConfig::from_toml(text);
            assert_eq!(
                c.prune_after,
                DaemonConfig::default().prune_after,
                "{text:?} should fall back"
            );
            assert_eq!(c.problems.len(), 1, "{text:?} should say why");
        }
    }

    #[test]
    fn the_default_keeps_a_week() {
        let c = DaemonConfig::default();
        assert_eq!(c.prune_after, Some(Duration::from_secs(7 * 86_400)));
    }
}
