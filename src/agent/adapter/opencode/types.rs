//! Private opencode-internal types and home resolution.
//!
//! This is **registry plumbing**, distinct from the bridge directory. The
//! bridge dir (the adapter's `trust_root`) is owned by [`super::install::bridge_dir`]
//! and derived from `$AGENT_WATCHER_OPENCODE_BRIDGE_DIR` / XDG — NOT from the
//! opencode home computed here. `default_opencode_home` only feeds the
//! detector/registry's `provider_home` chain (see `bindings.rs`); the v1
//! filesystem locator never reads it.

use std::path::PathBuf;

/// Re-export the bridge-dir rule so callers can reach it through `types::`
/// without re-deriving it — M2's `install::bridge_dir` remains the single
/// source of truth for the `$AGENT_WATCHER_OPENCODE_BRIDGE_DIR ?? XDG` rule. The
/// first prod caller lands in M5's `bindings.rs` arm; until then the re-export
/// is "dead" in the non-test build, so suppress the unused-import warning the
/// way M2 suppresses its staged items (`#[allow(dead_code)]` in `mod.rs`).
#[allow(unused_imports)]
pub(crate) use super::install::bridge_dir;

/// opencode context-window "unknown" sentinel. opencode does not emit a
/// context-window size into the bridge JSONL; the decoder resolves it from
/// opencode's models.dev cache by `(providerID, modelID)` (see
/// [`super::model_catalog`]). This `0` is what the lookup returns when the
/// cache is absent or the model is unlisted — the frontend then renders the
/// bar without a denominator. Mirrors the `KIMI_CONTEXT_WINDOW_SIZE` shape.
pub(crate) const OPENCODE_CONTEXT_WINDOW_SIZE: u64 = 0;

/// opencode home fallback (registry plumbing only — NOT the bridge dir).
/// Honors `$OPENCODE_HOME` FIRST (non-empty), then
/// `dirs::home_dir().join(".local/share/opencode")` (the XDG data dir
/// opencode installs into — verified from a live process; NOT `~/.opencode`,
/// which is the binary install), then the relative
/// `.local/share/opencode` when home is unknown (headless / service
/// sessions). Mirrors `default_kimi_home`'s env → home → relative fallback
/// shape.
///
/// Genuinely test-only in M5: the v1 filesystem locator ignores
/// `provider_home` entirely (the bridge root is XDG-derived via
/// `install::bridge_dir`), so the bindings arm never consults this. It exists
/// for the detector/registry's `provider_home` chain wired in M1's
/// `config.rs::AGENT_SPECS` (`home_subdir = ".local/share/opencode"`); until
/// that lands it has no production caller. Keep the narrow allow rather than a
/// blanket module-level one (per M5: drop the blanket `#[allow(dead_code)]`,
/// scope it to the genuinely-staged item).
#[allow(dead_code)]
pub(crate) fn default_opencode_home() -> PathBuf {
    if let Some(env_home) = std::env::var_os("OPENCODE_HOME") {
        if !env_home.is_empty() {
            return PathBuf::from(env_home);
        }
    }
    dirs::home_dir()
        .map(|home| home.join(".local").join("share").join("opencode"))
        .unwrap_or_else(|| PathBuf::from(".local/share/opencode"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::adapter::opencode::OpencodeEnvGuard;

    #[test]
    fn default_opencode_home_prefers_env_var() {
        // Guard serializes env mutation + restores the prior value on drop.
        let _guard = OpencodeEnvGuard::acquire();
        std::env::set_var("OPENCODE_HOME", "/custom/opencode");
        assert_eq!(default_opencode_home(), PathBuf::from("/custom/opencode"));
    }

    #[test]
    fn default_opencode_home_falls_back_to_local_share_subdir() {
        let _guard = OpencodeEnvGuard::acquire();
        std::env::remove_var("OPENCODE_HOME");
        let home = default_opencode_home();
        assert!(home.ends_with(".local/share/opencode"));
    }

    #[test]
    fn default_opencode_home_ignores_empty_env_var() {
        let _guard = OpencodeEnvGuard::acquire();
        std::env::set_var("OPENCODE_HOME", "");
        // Empty must NOT win — falls through to the home/relative default.
        assert!(default_opencode_home().ends_with(".local/share/opencode"));
    }

    #[test]
    fn opencode_context_window_size_is_zero_unknown() {
        assert_eq!(OPENCODE_CONTEXT_WINDOW_SIZE, 0);
    }
}
