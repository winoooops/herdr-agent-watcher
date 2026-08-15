//! Auto-install + path resolution for the vendored opencode bridge plugin.
//!
//! The bridge plugin (`plugin/agent-watcher-opencode-bridge.ts`) is embedded into
//! the backend binary via `include_str!` and idempotently, version-gatedly
//! written into the user's opencode plugins directory on attach. The bridge
//! directory it writes its JSONL into is derived by [`bridge_dir`], whose rule
//! is **byte-identical** to the plugin's own `bridgeDir()` so both sides agree
//! on the path independently.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The bridge plugin source, embedded at build time. The same bytes are written
/// to disk by [`ensure_bridge_installed`] and parsed for the version header by
/// [`embedded_version`].
pub(crate) const BRIDGE_PLUGIN_SOURCE: &str =
    include_str!("plugin/agent-watcher-opencode-bridge.ts");

/// File name of the installed plugin inside the opencode plugins directory.
pub(crate) const BRIDGE_PLUGIN_FILENAME: &str = "agent-watcher-opencode-bridge.ts";

const LEGACY_BRIDGE_PLUGIN_FILENAME: &str = "vimeflow-opencode-bridge.ts";

/// Result of an [`ensure_bridge_installed`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallOutcome {
    /// The plugin was (re)written because it was absent or out of date.
    Installed,
    /// The installed plugin already carries the embedded version — no write.
    UpToDate,
}

/// Bridge directory shared by the Rust adapter and the TS plugin.
///
/// Rule (**must stay byte-identical to the plugin's `bridgeDir()`**):
/// `$AGENT_WATCHER_OPENCODE_BRIDGE_DIR` else
/// `${XDG_DATA_HOME:-$HOME/.local/share}/agent-watcher/opencode-bridge`.
pub(crate) fn bridge_dir() -> PathBuf {
    if let Some(override_dir) = non_empty_env("AGENT_WATCHER_OPENCODE_BRIDGE_DIR") {
        return PathBuf::from(override_dir);
    }

    let data_home = match non_empty_env("XDG_DATA_HOME") {
        Some(value) => PathBuf::from(value),
        None => home_dir().join(".local").join("share"),
    };

    data_home.join("agent-watcher").join("opencode-bridge")
}

/// Directory the bridge plugin is installed into.
///
/// Rule: `$AGENT_WATCHER_OPENCODE_PLUGINS_DIR` else `$HOME/.config/opencode/plugins`.
/// The env override is the test seam (so dispatch tests never write to the real
/// user config).
pub(crate) fn opencode_plugins_dir() -> PathBuf {
    if let Some(override_dir) = non_empty_env("AGENT_WATCHER_OPENCODE_PLUGINS_DIR") {
        return PathBuf::from(override_dir);
    }

    home_dir().join(".config").join("opencode").join("plugins")
}

/// Idempotently install the embedded bridge plugin into `plugins_dir`.
///
/// Reads the `// agent-watcher-bridge-version: N` header from the embedded source
/// and from any existing installed file. Writes (atomically: temp file in the
/// same dir, mode `0600`, then `rename` over the target) when the file is
/// absent, unparsable, or carries a different version; otherwise returns
/// [`InstallOutcome::UpToDate`]. Creates `plugins_dir` if missing.
pub(crate) fn ensure_bridge_installed(plugins_dir: &Path) -> io::Result<InstallOutcome> {
    let embedded = embedded_version().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "embedded opencode bridge plugin has no `// agent-watcher-bridge-version: N` header",
        )
    })?;

    let target = plugins_dir.join(BRIDGE_PLUGIN_FILENAME);

    if let Ok(existing) = fs::read_to_string(&target) {
        if parse_version(&existing) == Some(embedded) {
            remove_legacy_bridge(plugins_dir)?;
            return Ok(InstallOutcome::UpToDate);
        }
    }

    fs::create_dir_all(plugins_dir)?;
    atomic_write(plugins_dir, &target, BRIDGE_PLUGIN_SOURCE.as_bytes())?;
    remove_legacy_bridge(plugins_dir)?;

    Ok(InstallOutcome::Installed)
}

/// Parse the version from the embedded source.
pub(crate) fn embedded_version() -> Option<u32> {
    parse_version(BRIDGE_PLUGIN_SOURCE)
}

/// Parse `// agent-watcher-bridge-version: N` from any plugin text. Returns the first
/// match; `None` if absent or non-numeric.
fn parse_version(source: &str) -> Option<u32> {
    const MARKER: &str = "agent-watcher-bridge-version:";

    for line in source.lines() {
        if let Some(idx) = line.find(MARKER) {
            let rest = &line[idx + MARKER.len()..];

            return rest.trim().parse::<u32>().ok();
        }
    }

    None
}

fn remove_legacy_bridge(plugins_dir: &Path) -> io::Result<()> {
    match fs::remove_file(plugins_dir.join(LEGACY_BRIDGE_PLUGIN_FILENAME)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Atomically write `bytes` to `target`: write to a uniquely-named temp file in
/// `dir` (mode `0600` on unix), flush, then replace `target`. Unix keeps the
/// atomic rename-over-existing behavior; Windows removes an existing target
/// first because `std::fs::rename` does not replace files there.
fn atomic_write(dir: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        BRIDGE_PLUGIN_FILENAME,
        std::process::id()
    ));

    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let write_result = (|| -> io::Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;

        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp);

        return Err(e);
    }

    if let Err(e) = replace_file(&tmp, target) {
        let _ = fs::remove_file(&tmp);

        return Err(e);
    }

    Ok(())
}

#[cfg(windows)]
fn replace_file(tmp: &Path, target: &Path) -> io::Result<()> {
    match fs::remove_file(target) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    fs::rename(tmp, target)
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, target: &Path) -> io::Result<()> {
    fs::rename(tmp, target)
}

/// `$HOME` (or a relative fallback for headless/service sessions where home is
/// unknown). Mirrors the `dirs::home_dir()` precedence used elsewhere. Shared
/// with `model_catalog` for the models.dev cache-home resolution.
pub(super) fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Read an env var, mapping unset / empty to `None`. Shared with
/// `model_catalog` so both sides derive XDG paths with identical semantics.
pub(super) fn non_empty_env(key: &str) -> Option<std::ffi::OsString> {
    match std::env::var_os(key) {
        Some(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::adapter::opencode::OpencodeEnvGuard;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn embedded_version_is_five() {
        assert_eq!(embedded_version(), Some(5));
    }

    #[test]
    fn parse_version_reads_first_marker_and_rejects_garbage() {
        assert_eq!(
            parse_version("// agent-watcher-bridge-version: 7\nfoo"),
            Some(7)
        );
        assert_eq!(parse_version("no header here"), None);
        assert_eq!(
            parse_version("// agent-watcher-bridge-version: notanumber"),
            None
        );
    }

    #[test]
    fn install_writes_then_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugins = dir.path().join("plugins");

        // First call installs into a not-yet-existing plugins dir.
        let first = ensure_bridge_installed(&plugins).expect("install");
        assert_eq!(first, InstallOutcome::Installed);

        let target = plugins.join(BRIDGE_PLUGIN_FILENAME);
        let written = fs::read_to_string(&target).expect("read installed");
        assert_eq!(written, BRIDGE_PLUGIN_SOURCE);

        // Second call sees the up-to-date version and does not rewrite.
        let second = ensure_bridge_installed(&plugins).expect("install");
        assert_eq!(second, InstallOutcome::UpToDate);
    }

    #[test]
    fn install_replaces_older_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugins = dir.path().join("plugins");
        fs::create_dir_all(&plugins).expect("mkdir plugins");

        let target = plugins.join(BRIDGE_PLUGIN_FILENAME);
        // Seed an older-version stub.
        fs::write(&target, "// agent-watcher-bridge-version: 0\n// stale").expect("seed");

        let outcome = ensure_bridge_installed(&plugins).expect("install");
        assert_eq!(outcome, InstallOutcome::Installed);

        let written = fs::read_to_string(&target).expect("read installed");
        assert_eq!(written, BRIDGE_PLUGIN_SOURCE);
        assert_eq!(parse_version(&written), Some(5));
    }

    #[test]
    fn install_replaces_unparsable_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugins = dir.path().join("plugins");
        fs::create_dir_all(&plugins).expect("mkdir plugins");

        let target = plugins.join(BRIDGE_PLUGIN_FILENAME);
        fs::write(&target, "garbage with no header").expect("seed");

        let outcome = ensure_bridge_installed(&plugins).expect("install");
        assert_eq!(outcome, InstallOutcome::Installed);
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            BRIDGE_PLUGIN_SOURCE
        );
    }

    #[test]
    fn install_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugins = dir.path().join("plugins");

        ensure_bridge_installed(&plugins).expect("install");

        let leftovers: Vec<_> = fs::read_dir(&plugins)
            .expect("read plugins")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic temp file should be renamed away"
        );
    }

    #[test]
    fn install_removes_legacy_plugin_filename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugins = dir.path().join("plugins");
        fs::create_dir_all(&plugins).expect("mkdir plugins");
        let legacy = plugins.join(LEGACY_BRIDGE_PLUGIN_FILENAME);
        fs::write(&legacy, "legacy").expect("seed legacy plugin");

        ensure_bridge_installed(&plugins).expect("install");

        assert!(!legacy.exists());
        assert!(plugins.join(BRIDGE_PLUGIN_FILENAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn installed_file_is_mode_0600() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugins = dir.path().join("plugins");
        ensure_bridge_installed(&plugins).expect("install");

        let target = plugins.join(BRIDGE_PLUGIN_FILENAME);
        let mode = fs::metadata(&target).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "installed plugin must be 0600");
    }

    #[test]
    fn bridge_dir_honors_explicit_override() {
        let _guard = OpencodeEnvGuard::acquire();
        std::env::set_var("AGENT_WATCHER_OPENCODE_BRIDGE_DIR", "/custom/bridge");
        assert_eq!(bridge_dir(), PathBuf::from("/custom/bridge"));
    }

    #[test]
    fn bridge_dir_uses_xdg_data_home_when_no_override() {
        let _guard = OpencodeEnvGuard::acquire();
        std::env::remove_var("AGENT_WATCHER_OPENCODE_BRIDGE_DIR");
        std::env::set_var("XDG_DATA_HOME", "/xdg/data");
        assert_eq!(
            bridge_dir(),
            PathBuf::from("/xdg/data/agent-watcher/opencode-bridge")
        );
    }

    #[test]
    fn bridge_dir_falls_back_to_home_local_share() {
        let _guard = OpencodeEnvGuard::acquire();
        std::env::remove_var("AGENT_WATCHER_OPENCODE_BRIDGE_DIR");
        std::env::remove_var("XDG_DATA_HOME");
        std::env::set_var("HOME", "/home/tester");

        let expected = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("share")
            .join("agent-watcher")
            .join("opencode-bridge");
        assert_eq!(bridge_dir(), expected);
        // And the literal default components are present.
        assert!(bridge_dir().ends_with("agent-watcher/opencode-bridge"));
    }

    #[test]
    fn opencode_plugins_dir_honors_override() {
        let _guard = OpencodeEnvGuard::acquire();
        std::env::set_var("AGENT_WATCHER_OPENCODE_PLUGINS_DIR", "/custom/plugins");
        assert_eq!(opencode_plugins_dir(), PathBuf::from("/custom/plugins"));
    }

    #[test]
    fn opencode_plugins_dir_defaults_to_config_opencode_plugins() {
        let _guard = OpencodeEnvGuard::acquire();
        std::env::remove_var("AGENT_WATCHER_OPENCODE_PLUGINS_DIR");
        std::env::set_var("HOME", "/home/tester");

        let expected = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("opencode")
            .join("plugins");
        assert_eq!(opencode_plugins_dir(), expected);
        assert!(opencode_plugins_dir().ends_with(".config/opencode/plugins"));
    }

    /// Embed-integrity: the `include_str!`'d plugin must carry the version
    /// header, the three registered hook names, and the SAME bridge-dir rule
    /// components / env-var name as the Rust `bridge_dir()`.
    #[test]
    fn embedded_plugin_matches_rust_contract() {
        let src = BRIDGE_PLUGIN_SOURCE;
        assert!(src.contains("agent-watcher-bridge-version: 5"));

        // Registered hooks.
        assert!(src.contains("event:"), "event hook");
        assert!(
            src.contains("'tool.execute.before'"),
            "tool.execute.before hook"
        );
        assert!(
            src.contains("'tool.execute.after'"),
            "tool.execute.after hook"
        );

        // Bridge-dir rule parity with Rust.
        assert!(src.contains("AGENT_WATCHER_OPENCODE_BRIDGE_DIR"));
        assert!(src.contains("XDG_DATA_HOME"));
        assert!(src.contains("'agent-watcher'"));
        assert!(src.contains("'opencode-bridge'"));
        assert!(src.contains("'.local'") && src.contains("'share'"));
    }

    #[test]
    fn embedded_plugin_redacts_sensitive_tool_arg_fields() {
        let src = BRIDGE_PLUGIN_SOURCE;

        assert!(src.contains("SENSITIVE_ARG_FIELDS"));
        assert!(src.contains("isSensitiveArgField(key)"));
        assert!(src.contains("preview[key] = '[redacted]'"));
        assert!(src.contains("'authorization'"));
        assert!(src.contains("'password'"));
        assert!(src.contains("'token'"));
    }
}
