pub mod state_wire;
pub mod store;

#[cfg(all(feature = "runtime", unix))]
pub mod config;
#[cfg(all(feature = "runtime", unix))]
pub(crate) mod routes;

/// Unix-only by construction (it names a socket) and only ever called from
/// runtime code. Gated with its callers, kept exactly as it is.
#[cfg(all(feature = "runtime", unix))]
pub(crate) fn state_socket_path() -> std::path::PathBuf {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("herdr-agent-watcher-state.sock")
}

#[cfg(all(feature = "runtime", unix))]
pub mod consent;
#[cfg(all(feature = "runtime", unix))]
pub mod reconcile;
#[cfg(all(feature = "runtime", unix))]
pub mod run;
#[cfg(all(feature = "runtime", unix))]
pub mod singleton;
#[cfg(all(feature = "runtime", unix))]
pub mod sink;
#[cfg(all(feature = "runtime", unix))]
pub mod state_server;
