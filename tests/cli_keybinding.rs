mod support;

use std::path::PathBuf;

const DEFAULTS: &str = include_str!("fixtures/herdr-default-config.toml");

struct Env {
    _dir: tempfile::TempDir,
    herdr_config: PathBuf,
    plugin_config: PathBuf,
    state: PathBuf,
    fake: support::fake_herdr::FakeHerdr,
}

impl Env {
    /// `herdr_config` is the operator's file; `None` leaves it absent.
    /// `plugin_config` is the body of the plugin's own config.toml.
    /// `check` is what the fake `herdr config check` prints.
    fn new(herdr_config: Option<&str>, plugin_config: &str, check: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = support::fake_herdr::install(dir.path(), DEFAULTS, check);
        Self::with_fake(herdr_config, plugin_config, dir, fake)
    }

    fn with_fake(
        herdr_config: Option<&str>,
        plugin_config: &str,
        dir: tempfile::TempDir,
        fake: support::fake_herdr::FakeHerdr,
    ) -> Self {
        let config = dir.path().join("config.toml");
        if let Some(body) = herdr_config {
            std::fs::write(&config, body).expect("write herdr config");
        }
        let plugin = dir.path().join("plugin-config");
        std::fs::create_dir_all(&plugin).expect("plugin config dir");
        std::fs::write(plugin.join("config.toml"), plugin_config).expect("write plugin config");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).expect("state dir");
        Self {
            _dir: dir,
            herdr_config: config,
            plugin_config: plugin,
            state,
            fake,
        }
    }

    fn run(&self, action: &str) -> std::process::Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_herdr-agent-watcher"))
            .arg(action)
            .env("HERDR_BIN_PATH", &self.fake.bin)
            .env("HERDR_CONFIG_PATH", &self.herdr_config)
            .env("HERDR_PLUGIN_CONFIG_DIR", &self.plugin_config)
            .env("HERDR_PLUGIN_STATE_DIR", &self.state)
            // These tests write config files. REMOVING XDG_CONFIG_HOME would
            // be backwards: `herdr_config_path` then falls back to
            // `dirs::home_dir()/.config`, so a regression that ignored
            // HERDR_CONFIG_PATH would write the operator's real config.
            // Point both at the temp tree instead.
            .env("XDG_CONFIG_HOME", self._dir.path().join("xdg"))
            .env("HOME", self._dir.path().join("home"))
            .output()
            .expect("run the action")
    }

    fn config(&self) -> String {
        std::fs::read_to_string(&self.herdr_config).unwrap_or_default()
    }
}

#[test]
fn bind_writes_the_block_and_reloads() {
    let env = Env::new(Some("[ui]\nx = 1\n"), "", "config: ok");
    let out = env.run("bind-sidebar-key");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let text = env.config();
    assert!(text.starts_with("[ui]\nx = 1\n"), "{text}");
    assert!(text.contains("key = \"prefix+a\""), "{text}");
    assert!(
        env.fake.argv().contains("server reload-config"),
        "the binding is not live until the server reloads: {}",
        env.fake.argv()
    );
    assert!(env.state.join("keybinding-install.json").exists());
}

#[test]
fn bind_twice_is_already_bound() {
    let env = Env::new(Some("[ui]\nx = 1\n"), "", "config: ok");
    assert!(env.run("bind-sidebar-key").status.success());
    let after_first = env.config();

    let second = env.run("bind-sidebar-key");
    assert!(second.status.success());
    assert_eq!(env.config(), after_first, "the block must not be duplicated");
    assert!(String::from_utf8_lossy(&second.stdout).contains("already bound"));
}

#[test]
fn a_taken_key_is_refused_and_names_the_action() {
    // toggle_sidebar holds prefix+b in Herdr's defaults.
    let env = Env::new(
        Some("[ui]\nx = 1\n"),
        "[keys]\nopen_sidebar = \"prefix+b\"\n",
        "config: ok",
    );
    let out = env.run("bind-sidebar-key");
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("toggle_sidebar"), "{text}");
    assert_eq!(env.config(), "[ui]\nx = 1\n", "nothing was written");
}

#[test]
fn a_range_default_is_refused_too() {
    // switch_tab is "prefix+1..9", so prefix+1 is taken by a range.
    let env = Env::new(
        Some("[ui]\nx = 1\n"),
        "[keys]\nopen_sidebar = \"prefix+1\"\n",
        "config: ok",
    );
    let out = env.run("bind-sidebar-key");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("switch_tab"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_invalid_key_never_reaches_the_operators_file() {
    let env = Env::new(
        Some("[ui]\nx = 1\n"),
        "[keys]\nopen_sidebar = \"nope\"\n",
        "config: issues found\ninvalid keybinding: keys.command[0].key = \"nope\"; disabling binding",
    );
    let out = env.run("bind-sidebar-key");
    assert!(!out.status.success());
    assert_eq!(env.config(), "[ui]\nx = 1\n");
}

#[test]
fn an_ambiguous_action_is_refused_and_names_the_other_plugin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = support::fake_herdr::install_with_owners(
        dir.path(),
        DEFAULTS,
        "config: ok",
        &["herdr-agent-watcher", "someone-elses-plugin"],
    );
    let env = Env::with_fake(Some("[ui]\nx = 1\n"), "", dir, fake);

    let out = env.run("bind-sidebar-key");
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("someone-elses-plugin"), "{text}");
    assert_eq!(env.config(), "[ui]\nx = 1\n");
    assert!(!env.state.join("keybinding-install.json").exists());
}

#[test]
fn a_failing_herdr_is_a_failure_not_an_empty_default_list() {
    let env = Env::new(Some("[ui]\nx = 1\n"), "", "config: ok");
    env.fake.fail();
    let out = env.run("bind-sidebar-key");
    assert!(
        !out.status.success(),
        "an empty default list makes every key look free"
    );
    assert_eq!(env.config(), "[ui]\nx = 1\n");
}

#[test]
fn reload_diagnostics_are_reported_as_failure() {
    let env = Env::new(Some("[ui]\nx = 1\n"), "", "config: ok");
    env.fake.reload_diagnostics("[\"something is off\"]");
    let out = env.run("bind-sidebar-key");
    assert!(
        !out.status.success(),
        "the binding is not live, so this is not success"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("diagnostic"));
}

#[test]
fn an_unwritable_record_leaves_the_config_alone() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new(Some("[ui]\nx = 1\n"), "", "config: ok");
    std::fs::set_permissions(&env.state, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = env.run("bind-sidebar-key");
    std::fs::set_permissions(&env.state, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!out.status.success());
    assert_eq!(
        env.config(),
        "[ui]\nx = 1\n",
        "a config changed with no record is a binding nothing can remove"
    );
}

#[test]
fn a_record_for_another_config_is_refused_not_overwritten() {
    let env = Env::new(Some("[ui]\nx = 1\n"), "", "config: ok");
    std::fs::write(
        env.state.join("keybinding-install.json"),
        r#"{"config_path":"/somewhere/else.toml","appended":"x","created_file":false,"key":"prefix+a"}"#,
    )
    .unwrap();
    let out = env.run("bind-sidebar-key");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("/somewhere/else.toml"));
    assert_eq!(env.config(), "[ui]\nx = 1\n");
}
