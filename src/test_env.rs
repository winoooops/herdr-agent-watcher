//! One lock for every test that mutates process environment.
//!
//! Modules with their own locks are not serialised against each other, and
//! `sidebar::config`, `agents::claude_bridge` and `daemon::config` all mutate
//! variables the others read.

use std::ffi::OsString;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sets `vars`, runs `body`, restores what was there before — including
/// restoring "absent" for a variable this call created.
pub(crate) fn with_env<T>(vars: &[(&str, Option<OsString>)], body: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved: Vec<(String, Option<OsString>)> = vars
        .iter()
        .map(|(key, _)| ((*key).to_string(), std::env::var_os(key)))
        .collect();
    for (key, value) in vars {
        match value {
            Some(value) => std::env::set_var(key, value),
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
