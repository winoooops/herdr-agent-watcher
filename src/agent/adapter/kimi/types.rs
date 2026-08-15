//! Private kimi-internal types and home resolution.

use std::path::PathBuf;

/// kimi-code context window size for `kimi-code/kimi-for-coding`.
/// kimi-code does not emit a context-window size in `wire.jsonl`, so the
/// adapter uses this constant (kimi-for-coding `max_context_size`).
pub(super) const KIMI_CONTEXT_WINDOW_SIZE: u64 = 262_144;

/// kimi-code `KIMI_CODE_HOME` fallback. Honors `$KIMI_CODE_HOME` FIRST
/// (kimi-code reads it to override the default home), then
/// `dirs::home_dir().join(".kimi-code")`, then the relative `.kimi-code`
/// when home is unknown (headless / service sessions). Mirrors
/// `codex::default_codex_home` but adds the env-var precedence kimi-code
/// itself honors.
pub(crate) fn default_kimi_home() -> PathBuf {
    if let Some(env_home) = std::env::var_os("KIMI_CODE_HOME") {
        if !env_home.is_empty() {
            return PathBuf::from(env_home);
        }
    }
    dirs::home_dir()
        .map(|home| home.join(".kimi-code"))
        .unwrap_or_else(|| PathBuf::from(".kimi-code"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_kimi_home_prefers_env_var() {
        // Guard serializes env mutation + restores the prior value on drop.
        let _guard = crate::agent::adapter::KimiHomeEnvGuard::acquire();
        std::env::set_var("KIMI_CODE_HOME", "/custom/kimi");
        assert_eq!(default_kimi_home(), PathBuf::from("/custom/kimi"));
    }

    #[test]
    fn default_kimi_home_falls_back_to_home_subdir() {
        let _guard = crate::agent::adapter::KimiHomeEnvGuard::acquire();
        std::env::remove_var("KIMI_CODE_HOME");
        let home = default_kimi_home();
        assert!(home.ends_with(".kimi-code"));
    }
}
