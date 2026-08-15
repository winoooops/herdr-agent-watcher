//! A stand-in for the herdr binary. Records every argv line it is given, so a
//! test can assert that `server reload-config` was actually called — asserting
//! on the resulting file alone passes with the reload deleted.

use std::path::{Path, PathBuf};

pub struct FakeHerdr {
    pub bin: PathBuf,
    pub log: PathBuf,
}

/// `default_config` is printed for `--default-config`; `check` is printed for
/// `config check` (use "config: ok" or an issues block); `owners` are the
/// plugin ids that declare `open-sidebar`, so a test can make it ambiguous.
pub fn install(dir: &Path, default_config: &str, check: &str) -> FakeHerdr {
    install_with_owners(dir, default_config, check, &["herdr-agent-watcher"])
}

pub fn install_with_owners(
    dir: &Path,
    default_config: &str,
    check: &str,
    owners: &[&str],
) -> FakeHerdr {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("herdr");
    let log = dir.join("herdr-argv.log");
    let defaults_path = dir.join("default-config.toml");
    std::fs::write(&defaults_path, default_config).unwrap();

    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> {log}
case "$1" in
  --default-config) cat {defaults} ;;
  config)  printf '%s\n' {check} ;;
  server)  printf '%s\n' '{{"result":{{"status":"applied","diagnostics":[]}}}}' ;;
  plugin)  printf '%s\n' {actions} ;;
esac
exit 0
"#,
        // Quoted, like `check` already is: a temp root containing a space
        // breaks an unquoted redirection target and an unquoted `cat` argument.
        log = shell_quote(&log.to_string_lossy()),
        defaults = shell_quote(&defaults_path.to_string_lossy()),
        check = shell_quote(check),
        actions = shell_quote(&format!(
            r#"{{"result":{{"actions":[{}]}}}}"#,
            owners
                .iter()
                .map(|id| format!(r#"{{"action_id":"open-sidebar","plugin_id":"{id}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        )),
    );
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    FakeHerdr { bin, log }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

impl FakeHerdr {
    pub fn argv(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    pub fn fail(&self) {
        let script = std::fs::read_to_string(&self.bin)
            .unwrap()
            .replace("exit 0", "exit 1");
        std::fs::write(&self.bin, script).unwrap();
    }

    pub fn reload_diagnostics(&self, diagnostics: &str) {
        let script = std::fs::read_to_string(&self.bin).unwrap().replace(
            "\"diagnostics\":[]",
            &format!("\"diagnostics\":{diagnostics}"),
        );
        std::fs::write(&self.bin, script).unwrap();
    }
}
