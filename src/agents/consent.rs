//! Public doorway to the ported Kimi usage-consent store.

use std::path::{Path, PathBuf};

pub fn consent_path() -> PathBuf {
    crate::daemon::DaemonOptions::from_env().kimi_consent_path()
}

pub fn set_and_persist(path: &Path, enabled: bool) -> std::io::Result<()> {
    crate::agent::kimi_usage_consent::set_and_persist(path, enabled)
}

pub fn load_into_memory(path: &Path) {
    crate::agent::kimi_usage_consent::load_into_memory(path);
}

pub fn enabled() -> bool {
    crate::agent::kimi_usage_consent::usage_consent_enabled()
}

pub fn request_refresh() {
    crate::agent::kimi_usage_consent::request_refresh();
}

#[cfg(test)]
pub(crate) fn test_serial_guard() -> std::sync::MutexGuard<'static, ()> {
    crate::agent::kimi_usage_consent::test_serial_guard()
}

#[cfg(test)]
pub(crate) fn set_for_test(enabled: bool) {
    crate::agent::kimi_usage_consent::set_for_test(enabled);
}
