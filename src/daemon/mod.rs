pub mod state_wire;
pub mod store;

#[cfg(feature = "runtime")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonOptions {
    pub state_dir: std::path::PathBuf,
}

#[cfg(feature = "runtime")]
impl DaemonOptions {
    pub fn new(state_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os("HERDR_PLUGIN_STATE_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir),
        )
    }

    pub fn singleton_lock_path(&self) -> std::path::PathBuf {
        self.state_dir.join("herdr-agent-watcher.lock")
    }

    pub fn control_socket_path(&self) -> std::path::PathBuf {
        self.state_dir.join("herdr-agent-watcher-control.sock")
    }

    pub fn state_socket_path(&self) -> std::path::PathBuf {
        self.state_dir.join("herdr-agent-watcher-state.sock")
    }

    pub fn kimi_consent_path(&self) -> std::path::PathBuf {
        self.state_dir.join("kimi-usage-consent.json")
    }

    pub fn daemon_data_dir(&self) -> std::path::PathBuf {
        self.state_dir.clone()
    }
}

#[cfg(feature = "runtime")]
pub mod config;
#[cfg(feature = "runtime")]
pub(crate) mod routes;

#[cfg(feature = "runtime")]
pub(crate) fn state_socket_path() -> std::path::PathBuf {
    DaemonOptions::from_env().state_socket_path()
}

#[cfg(feature = "runtime")]
pub mod consent;
pub mod prune;
#[cfg(feature = "runtime")]
pub mod reconcile;
#[cfg(feature = "runtime")]
pub mod run;
#[cfg(feature = "runtime")]
pub use run::{start, DaemonHandle};
#[cfg(feature = "runtime")]
pub mod singleton;
#[cfg(feature = "runtime")]
pub mod sink;
#[cfg(feature = "runtime")]
pub mod state_server;

#[cfg(all(test, feature = "runtime"))]
mod options_tests {
    use super::DaemonOptions;
    use crate::test_env::with_env;

    fn paths(options: &DaemonOptions) -> Vec<std::path::PathBuf> {
        vec![
            options.singleton_lock_path(),
            options.control_socket_path(),
            options.state_socket_path(),
            options.kimi_consent_path(),
            options.daemon_data_dir(),
        ]
    }

    #[test]
    fn different_roots_produce_disjoint_path_sets() {
        let left = DaemonOptions::new("/tmp/watcher-left");
        let right = DaemonOptions::new("/tmp/watcher-right");
        assert!(paths(&left)
            .iter()
            .all(|left| paths(&right).iter().all(|right| left != right)));
    }

    #[test]
    fn environment_options_keep_the_standalone_paths() {
        with_env(
            &[("HERDR_PLUGIN_STATE_DIR", Some("/tmp/watcher-env".into()))],
            || {
                let options = DaemonOptions::from_env();
                assert_eq!(options.state_dir, std::path::Path::new("/tmp/watcher-env"));
                assert_eq!(
                    options.singleton_lock_path(),
                    std::path::Path::new("/tmp/watcher-env/herdr-agent-watcher.lock")
                );
                assert_eq!(
                    options.control_socket_path(),
                    std::path::Path::new("/tmp/watcher-env/herdr-agent-watcher-control.sock")
                );
                assert_eq!(super::state_socket_path(), options.state_socket_path());
                assert_eq!(
                    crate::agents::consent::consent_path(),
                    options.kimi_consent_path()
                );
            },
        );
        with_env(&[("HERDR_PLUGIN_STATE_DIR", None)], || {
            assert_eq!(DaemonOptions::from_env().state_dir, std::env::temp_dir());
        });
    }
}
