//! `AgentBindings` — the typed bundle of one session's split-trait
//! views.
//!
//! Step B' of the v4-frozen refactor plan (#246). Replaces the
//! pre-B' practice of constructing an `Arc<dyn AgentAdapter>` and
//! plumbing it through `start_for` / `watcher_runtime`. Production
//! callers now receive `AgentBindings` from
//! [`AgentBindings::for_attach`] and read the trait surface they
//! need:
//!
//! - `bindings.locator` — where the statusline file is.
//! - `bindings.decoder` — raw JSON → status snapshot.
//! - `bindings.transcript_paths` — dynamic / static transcript hints.
//! - `bindings.validator` — raw path → canonical path (security
//!   check).
//! - `bindings.streamer` — spawn the tail thread. Step B'' wired this
//!   directly into `TranscriptState::start_or_replace`
//!   (`Arc<dyn TranscriptStreamer>`), removing the former transitional
//!   `adapter_for_transcript_state: Arc<dyn AgentAdapter>` field that
//!   B' carried only because `start_or_replace` still took the façade.

use std::path::PathBuf;
use std::sync::Arc;

use super::claude_code::ClaudeCodeAdapter;
use super::codex::{default_codex_home, CodexAdapter, CompositeLocator};
use super::error::AttachError;
use super::kimi::{default_kimi_home, KimiAdapter, KimiLocator};
use super::opencode::install::{bridge_dir, ensure_bridge_installed, opencode_plugins_dir};
use super::opencode::locator::OpenCodeLocator;
use super::opencode::OpenCodeAdapter;
use super::traits::{
    StateDecoder, StatusSourceLocator, TranscriptPathValidator, TranscriptStreamer,
};
use super::types::TranscriptPathSource;
use super::{AttachContext, ClaudeStatusFileLocator, NoOpAdapter};
use crate::agent::types::AgentType;

/// One session's typed adapter views, assembled from the
/// [`AttachContext`] by [`AgentBindings::for_attach`].
///
/// **All fields are live producers** consumed by the watcher / lifecycle
/// path (no `#[allow(dead_code)]` on any field). Concrete consumer for
/// each:
///
/// - `agent_type` — **the watcher DOES branch on this**. Two consumers:
///   (1) `session_lifecycle::run_watch_sequence` captures it before
///   `spawn_watch` consumes `bindings`, then forwards it to `register`,
///   which stamps it onto `WatcherHandle.agent_type` so
///   `AgentWatcherState::agent_type_for_pty` resolves under the single
///   watchers mutex (cycle 1 F2 — single-mutex atomicity guarantee).
///   (2) `base::start_watching` reads the captured value and gates the
///   Codex `session_index.jsonl` title-sync thread spawn on
///   `matches!(agent_type, AgentType::Codex)` (cycle 1 F5 — restores
///   the title-sync path the refactor initially dropped). Removing this
///   field, or any of its captures, silently breaks Codex title-sync
///   AND the atomic agent-type lookup.
/// - `locator` — `session_lifecycle::locate` calls
///   `locator.locate(cwd, sid)` to produce the `LocatedStatusSource`.
/// - `decoder` / `transcript_paths` / `validator` — destructured in
///   `start_watching` and passed into the notify / inline-init / poll
///   callbacks.
/// - `streamer` — handed to `TranscriptState::start_or_replace` (B''
///   migrated this off `Arc<dyn AgentAdapter>`; the façade `Arc` is
///   gone with no transitional field surviving).
#[derive(Clone)]
pub(crate) struct AgentBindings {
    pub(crate) agent_type: AgentType,
    pub(crate) locator: Arc<dyn StatusSourceLocator>,
    pub(crate) decoder: Arc<dyn StateDecoder>,
    pub(crate) transcript_paths: Arc<dyn TranscriptPathSource>,
    pub(crate) validator: Arc<dyn TranscriptPathValidator>,
    pub(crate) streamer: Arc<dyn TranscriptStreamer>,
}

impl AgentBindings {
    /// Build a session's bindings from the attach context.
    ///
    /// Dispatches by `ctx.agent_type` and wires each variant to the
    /// concrete adapter + locator. The return type is
    /// `Result<Self, AttachError>` for forward-compatibility with the
    /// D' service boundary, but today's implementation is
    /// infallible — Codex with `provider_home == None` falls back to
    /// `default_codex_home()` rather than failing (PR #261 codex
    /// review F3). The `AttachError` variants are reserved per #246
    /// acceptance for failure modes that become observable when D'
    /// retypes the locator/validator path.
    pub(crate) fn for_attach(ctx: &AttachContext) -> Result<Self, AttachError> {
        match ctx.agent_type {
            AgentType::ClaudeCode => {
                let adapter: Arc<ClaudeCodeAdapter> = Arc::new(ClaudeCodeAdapter);
                Ok(Self {
                    agent_type: ctx.agent_type,
                    locator: Arc::new(ClaudeStatusFileLocator::new(ctx.app_data_dir.clone())),
                    decoder: adapter.clone(),
                    transcript_paths: adapter.clone(),
                    validator: adapter.clone(),
                    streamer: adapter,
                })
            }
            AgentType::Codex => {
                // Codex's locator needs `codex_home` + `pid` +
                // `pty_start`. `ctx.provider_home` carries the typed
                // value from the central config registry when
                // `dirs::home_dir()` resolved successfully; in headless
                // / service sessions where it didn't, fall back to
                // `default_codex_home()` (relative `.codex`) so attach
                // still works — matching the pre-B' behavior of
                // `CodexAdapter::new` (PR #261 codex review F3).
                //
                // The locator is built ONCE here and shared via `Arc`
                // between `bindings.locator` (the outer
                // `Arc<dyn StatusSourceLocator>`) and `bindings.streamer`
                // (the `Arc<CodexAdapter>` whose internal locator field
                // is `Arc<CompositeLocator>`, NOT an owned one).
                // Pre-cycle-11 the two paths each built an independent
                // `CompositeLocator` from the same parameters — a
                // latent double-retry hazard if `<CodexAdapter as
                // AgentAdapter>::located_status_source` (the
                // transitional façade) was ever called downstream. The
                // structural fix (PR #261 cycle 11 F31) shares one
                // `CompositeLocator`; B'' (this step) then consumes the
                // streamer view directly in `start_or_replace`.
                let codex_home = ctx
                    .provider_home_override
                    .clone()
                    .or_else(|| ctx.provider_home.clone())
                    .unwrap_or_else(default_codex_home);
                // `ctx.proc_root` carries `Some("/proc")` on Linux,
                // `None` on non-Linux, and `Some(tempdir)` in test
                // harnesses that inject a fake `/proc`. Pass the
                // `Option` straight through — `CompositeLocator::new`
                // (and `SqliteFirstLocator`) gate the proc fast-paths
                // on `is_some()`, so macOS / Windows production runs
                // stop probing nonexistent `/proc/<pid>/cmdline` (PR
                // #261 cycle 8 F22 added `proc_root`; PR #302 Claude
                // review F1 widened it to `Option` so the platform
                // guard already in `config::default_proc_root()` isn't
                // silently undone by an `unwrap_or_else` here).
                let proc_root = ctx.proc_root.clone();
                // Attach-once observability for production Codex
                // sessions (PR #261 cycle 3 F11 + cycle 4 F13). The
                // log site stays here at the binding boundary so it
                // fires exactly once per attach, regardless of how
                // many consumers hold the shared `Arc<CompositeLocator>`.
                log::info!(
                    "codex adapter: locator initialized (codex_home={}, pid={})",
                    codex_home.display(),
                    ctx.agent_pid,
                );
                let composite_locator: Arc<CompositeLocator> = Arc::new(CompositeLocator::new(
                    codex_home,
                    ctx.agent_pid,
                    ctx.pty_start,
                    proc_root,
                ));
                let locator: Arc<dyn StatusSourceLocator> = composite_locator.clone();
                let adapter: Arc<CodexAdapter> =
                    Arc::new(CodexAdapter::with_locator(composite_locator));
                Ok(Self {
                    agent_type: ctx.agent_type,
                    locator,
                    decoder: adapter.clone(),
                    transcript_paths: adapter.clone(),
                    validator: adapter.clone(),
                    streamer: adapter,
                })
            }
            AgentType::Kimi => {
                // kimi-code's locator reads `<kimi_home>/session_index.jsonl`
                // to resolve the attach cwd to a `wire.jsonl`. An explicit
                // hermetic override (E2E helpers) wins over `$KIMI_CODE_HOME`
                // and the registry-resolved `provider_home`; otherwise
                // `$KIMI_CODE_HOME` stays authoritative to match kimi-code's
                // own home resolution, then `provider_home`, then
                // `default_kimi_home`.
                let kimi_home = ctx
                    .provider_home_override
                    .clone()
                    .or_else(|| {
                        std::env::var_os("KIMI_CODE_HOME")
                            // Ignore an empty `$KIMI_CODE_HOME` so it doesn't root at "" (matches `default_kimi_home`).
                            .filter(|v| !v.is_empty())
                            .map(PathBuf::from)
                    })
                    .or_else(|| ctx.provider_home.clone())
                    .unwrap_or_else(default_kimi_home);
                super::kimi::kdbg(&format!(
                    "ATTACH kimi: agent_pid={} initial_cwd={} provider_home={:?} proc_root={:?} home={}",
                    ctx.agent_pid,
                    ctx.initial_cwd.display(),
                    ctx.provider_home,
                    ctx.proc_root,
                    kimi_home.display()
                ));
                log::info!(
                    "kimi adapter: locator initialized (kimi_home={}, pid={})",
                    kimi_home.display(),
                    ctx.agent_pid,
                );
                // Pass pid + pty_start + proc_root so the locator can read
                // the kimi process's own fds / environ (proc-fd, proc-environ);
                // `kimi_home` stays the env/provider/default fallback home.
                let kimi_locator: Arc<KimiLocator> = Arc::new(KimiLocator::with_proc_env_home(
                    kimi_home,
                    ctx.agent_pid,
                    ctx.pty_start,
                    ctx.proc_root.clone(),
                    ctx.provider_home_override.is_none(),
                ));
                let locator: Arc<dyn StatusSourceLocator> = kimi_locator.clone();
                let adapter: Arc<KimiAdapter> = Arc::new(KimiAdapter::with_locator(kimi_locator));
                Ok(Self {
                    agent_type: ctx.agent_type,
                    locator,
                    decoder: adapter.clone(),
                    transcript_paths: adapter.clone(),
                    validator: adapter.clone(),
                    streamer: adapter,
                })
            }
            AgentType::Opencode => {
                // opencode's locator is filesystem-only: the bridge dir
                // (XDG-derived via `bridge_dir()`) holds `index.jsonl` + every
                // `<sessionID>.jsonl`. Unlike Codex/Kimi it never reads
                // `ctx.provider_home` for resolution — the bridge root is the
                // single source — so we pass `bridge_dir()`, `ctx.agent_pid`
                // (the index's primary disambiguator), and `ctx.pty_start`
                // (the cwd-fallback freshness floor). `proc_root` is unused in
                // v1 (no `/proc` fast-path).
                //
                // The locator is built ONCE here and shared via `Arc` between
                // `bindings.locator` (the outer `Arc<dyn StatusSourceLocator>`)
                // and the `OpenCodeAdapter`'s internal locator field —
                // the cycle-11 F31 single-allocation invariant. Do NOT build
                // two locators.
                let bridge_root = bridge_dir();
                log::info!(
                    "opencode adapter: locator initialized (bridge_root={}, pid={})",
                    bridge_root.display(),
                    ctx.agent_pid,
                );
                let opencode_locator: Arc<OpenCodeLocator> = Arc::new(OpenCodeLocator::new(
                    bridge_root,
                    ctx.agent_pid,
                    ctx.pty_start,
                ));

                // Idempotently install the embedded bridge plugin. A failed /
                // forbidden install MUST NOT fail attach — the adapter can
                // still tail an already-installed bridge; worst case is the
                // documented restart caveat. Log-and-continue on `Err`.
                let plugins_dir = opencode_plugins_dir();
                match ensure_bridge_installed(&plugins_dir) {
                    Ok(outcome) => log::info!(
                        "opencode adapter: bridge plugin {:?} in {}",
                        outcome,
                        plugins_dir.display(),
                    ),
                    Err(e) => log::warn!(
                        "opencode adapter: bridge plugin install skipped (non-fatal) in {}: {}",
                        plugins_dir.display(),
                        e,
                    ),
                }

                let locator: Arc<dyn StatusSourceLocator> = opencode_locator.clone();
                let adapter: Arc<OpenCodeAdapter> =
                    Arc::new(OpenCodeAdapter::with_locator(opencode_locator));
                Ok(Self {
                    agent_type: ctx.agent_type,
                    locator,
                    decoder: adapter.clone(),
                    transcript_paths: adapter.clone(),
                    validator: adapter.clone(),
                    streamer: adapter,
                })
            }
            other => {
                // NoOp adapter for Aider / Generic — covers every
                // non-Claude / non-Codex / non-Kimi / non-opencode variant.
                // `UnsupportedAgent`
                // is reserved per the acceptance enum for a future
                // refusal mode; today's behavior matches the
                // pre-B' `<dyn AgentAdapter>::for_attach` (always
                // returns Ok with a NoOp wrapper).
                let adapter: Arc<NoOpAdapter> =
                    Arc::new(NoOpAdapter::new(other, ctx.app_data_dir.clone()));
                Ok(Self {
                    agent_type: other,
                    locator: adapter.clone(),
                    decoder: adapter.clone(),
                    transcript_paths: adapter.clone(),
                    validator: adapter.clone(),
                    streamer: adapter,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::adapter::types::LocatedStatusSource;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn claude_ctx() -> AttachContext {
        AttachContext {
            session_id: "sid".to_string(),
            initial_cwd: PathBuf::from("/tmp/ws"),
            shell_pid: 1,
            agent_pid: 2,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::ClaudeCode,
            app_data_dir: PathBuf::from("/tmp/vimeflow-data"),
            provider_home: Some(PathBuf::from("/home/u/.claude")),
            provider_home_override: None,
            proc_root: None,
        }
    }

    fn codex_ctx(home: Option<PathBuf>) -> AttachContext {
        AttachContext {
            session_id: "sid".to_string(),
            initial_cwd: PathBuf::from("/tmp/ws"),
            shell_pid: 1,
            agent_pid: 2,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::Codex,
            app_data_dir: PathBuf::from("/tmp/vimeflow-data"),
            provider_home: home,
            provider_home_override: None,
            proc_root: None,
        }
    }

    fn kimi_ctx(home: Option<PathBuf>) -> AttachContext {
        AttachContext {
            session_id: "sid".to_string(),
            initial_cwd: PathBuf::from("/tmp/ws"),
            shell_pid: 1,
            agent_pid: 2,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::Kimi,
            app_data_dir: PathBuf::from("/tmp/vimeflow-data"),
            provider_home: home,
            provider_home_override: None,
            proc_root: None,
        }
    }

    fn opencode_ctx(home: Option<PathBuf>) -> AttachContext {
        AttachContext {
            session_id: "sid".to_string(),
            initial_cwd: PathBuf::from("/tmp/ws"),
            shell_pid: 1,
            agent_pid: 2,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::Opencode,
            app_data_dir: PathBuf::from("/tmp/vimeflow-data"),
            provider_home: home,
            provider_home_override: None,
            proc_root: None,
        }
    }

    /// Acquire the opencode env guard AND point `$AGENT_WATCHER_OPENCODE_PLUGINS_DIR`
    /// at a fresh temp dir so `for_attach`'s `ensure_bridge_installed` never
    /// writes to the real `~/.config/opencode/plugins`. The returned guard +
    /// tempdir must be held for the duration of the `for_attach` call.
    fn opencode_plugins_temp() -> (
        crate::agent::adapter::opencode::OpencodeEnvGuard,
        tempfile::TempDir,
    ) {
        let guard = crate::agent::adapter::opencode::OpencodeEnvGuard::acquire();
        let tmp = tempfile::tempdir().expect("plugins tempdir");
        std::env::set_var("AGENT_WATCHER_OPENCODE_PLUGINS_DIR", tmp.path());
        // Also pin a temp bridge dir so the locator root is hermetic.
        std::env::set_var(
            "AGENT_WATCHER_OPENCODE_BRIDGE_DIR",
            tmp.path().join("bridge"),
        );
        (guard, tmp)
    }

    fn aider_ctx() -> AttachContext {
        AttachContext {
            session_id: "sid".to_string(),
            initial_cwd: PathBuf::from("/tmp/ws"),
            shell_pid: 1,
            agent_pid: 2,
            pty_start: SystemTime::UNIX_EPOCH,
            agent_type: AgentType::Aider,
            app_data_dir: PathBuf::from("/tmp/vimeflow-data"),
            provider_home: None,
            provider_home_override: None,
            proc_root: None,
        }
    }

    /// for_attach dispatches by `ctx.agent_type` — three variants,
    /// three branches. The `agent_type` field on the returned
    /// bindings round-trips, proving the dispatch hit the right arm.
    #[test]
    fn for_attach_dispatches_by_agent_type() {
        let claude = AgentBindings::for_attach(&claude_ctx()).expect("claude binds");
        assert_eq!(claude.agent_type, AgentType::ClaudeCode);

        let codex = AgentBindings::for_attach(&codex_ctx(Some(PathBuf::from("/home/u/.codex"))))
            .expect("codex binds");
        assert_eq!(codex.agent_type, AgentType::Codex);

        let kimi = AgentBindings::for_attach(&kimi_ctx(Some(PathBuf::from("/home/u/.kimi-code"))))
            .expect("kimi binds");
        assert_eq!(kimi.agent_type, AgentType::Kimi);

        let opencode = {
            let (_guard, _tmp) = opencode_plugins_temp();
            AgentBindings::for_attach(&opencode_ctx(Some(PathBuf::from(
                "/home/u/.local/share/opencode",
            ))))
            .expect("opencode binds")
        };
        assert_eq!(opencode.agent_type, AgentType::Opencode);

        let noop = AgentBindings::for_attach(&aider_ctx()).expect("noop binds aider");
        assert_eq!(noop.agent_type, AgentType::Aider);
    }

    /// The shared-Arc invariant (cycle-11 F31): `for_attach` builds exactly
    /// ONE `OpenCodeLocator` and shares it between `bindings.locator` and the
    /// adapter's streamer/validator views. We prove "one instance" via the
    /// `Arc` strong-count on the outer `bindings.locator`: the arm holds
    /// `locator` (the `dyn` view) plus four `adapter.clone()`s of an
    /// `Arc<OpenCodeAdapter>` whose single field is the SAME locator `Arc`.
    /// After `for_attach` returns, the only surviving handle to the locator
    /// allocation reachable from outside is `bindings.locator` itself plus the
    /// one inside the adapter (held by `streamer`/`decoder`/… clones, all the
    /// same `Arc<OpenCodeAdapter>`). A second locator would have been a
    /// distinct allocation; this asserts the dispatch produced a real opencode
    /// adapter that locates under the SAME hermetic bridge root the locator was
    /// built with — a second, independently-built locator would resolve a
    /// different root.
    #[test]
    fn for_attach_opencode_shares_single_locator_root() {
        let (_guard, tmp) = opencode_plugins_temp();
        let bridge_root = tmp.path().join("bridge");
        std::fs::create_dir_all(&bridge_root).expect("mkdir bridge root");

        // Seed an index row keyed by the ctx agent_pid so the locator resolves.
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        std::fs::write(
            bridge_root.join("index.jsonl"),
            format!(
                "{{\"sessionID\":\"ses_shared\",\"pid\":2,\"directory\":\"{}\",\"time\":1}}\n",
                cwd.display()
            ),
        )
        .expect("write index");

        let bindings =
            AgentBindings::for_attach(&opencode_ctx(None)).expect("opencode binds (no home)");
        assert_eq!(bindings.agent_type, AgentType::Opencode);

        // The shared locator resolves the session under the hermetic bridge
        // root we pinned via `$AGENT_WATCHER_OPENCODE_BRIDGE_DIR`. A second,
        // independently-constructed locator pointed at the real (non-temp)
        // bridge dir would NOT find this row.
        let located = bindings
            .locator
            .locate(&cwd, "pty-1")
            .expect("shared locator resolves the seeded session");
        assert_eq!(located.agent_session_id.as_deref(), Some("ses_shared"));
        assert!(
            located.status_path.starts_with(&bridge_root),
            "resolved status path must be under the single shared bridge root, got {}",
            located.status_path.display(),
        );
    }

    /// A read-only / forbidden plugins dir does NOT fail `for_attach` — the
    /// bridge install error is non-fatal (log-and-continue). The bindings still
    /// dispatch to the real opencode adapter.
    #[test]
    fn for_attach_opencode_install_error_is_non_fatal() {
        let guard = crate::agent::adapter::opencode::OpencodeEnvGuard::acquire();
        let tmp = tempfile::tempdir().expect("tempdir");

        // Point the plugins dir at a path UNDER a read-only parent so
        // `ensure_bridge_installed`'s `create_dir_all` / write fails.
        let readonly_parent = tmp.path().join("ro");
        std::fs::create_dir_all(&readonly_parent).expect("mkdir ro parent");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&readonly_parent, std::fs::Permissions::from_mode(0o500))
                .expect("chmod ro parent");
        }
        let plugins_dir = readonly_parent.join("plugins");
        std::env::set_var("AGENT_WATCHER_OPENCODE_PLUGINS_DIR", &plugins_dir);
        std::env::set_var(
            "AGENT_WATCHER_OPENCODE_BRIDGE_DIR",
            tmp.path().join("bridge"),
        );

        let result = AgentBindings::for_attach(&opencode_ctx(None));

        // Restore writable perms BEFORE the tempdir drops (so cleanup succeeds).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&readonly_parent, std::fs::Permissions::from_mode(0o700));
        }
        drop(guard);

        let bindings = result.expect("forbidden install must NOT fail attach");
        assert_eq!(bindings.agent_type, AgentType::Opencode);
    }

    /// `for_attach` honors `$AGENT_WATCHER_OPENCODE_PLUGINS_DIR` — the install lands
    /// in the temp dir, NEVER the real `~/.config/opencode/plugins`.
    #[test]
    fn for_attach_opencode_install_honors_plugins_dir_override() {
        let (_guard, tmp) = opencode_plugins_temp();
        let bindings = AgentBindings::for_attach(&opencode_ctx(None)).expect("opencode binds");
        assert_eq!(bindings.agent_type, AgentType::Opencode);

        // The bridge plugin was written under the override dir.
        let installed = tmp.path().join("agent-watcher-opencode-bridge.ts");
        assert!(
            installed.exists(),
            "bridge plugin must install under $AGENT_WATCHER_OPENCODE_PLUGINS_DIR ({})",
            installed.display(),
        );
    }

    /// Kimi `for_attach` with `provider_home == None` falls back to
    /// `default_kimi_home()` (which itself honors `$KIMI_CODE_HOME`)
    /// rather than failing, matching the Codex fallback contract.
    #[test]
    fn for_attach_kimi_without_provider_home_falls_back_to_default_home() {
        let bindings =
            AgentBindings::for_attach(&kimi_ctx(None)).expect("kimi falls back to default home");
        assert_eq!(bindings.agent_type, AgentType::Kimi);
    }

    /// `$KIMI_CODE_HOME` is authoritative over `ctx.provider_home`: with a
    /// session laid out under the env home and a bogus provider_home, the
    /// built locator resolves under the env home (proving the override
    /// reached the locator, not provider_home).
    #[test]
    fn for_attach_kimi_env_home_overrides_provider_home() {
        let env_home = tempfile::tempdir().expect("env home");
        let work = tempfile::tempdir().expect("work dir");
        let session_dir = env_home
            .path()
            .join("sessions")
            .join("wd_x")
            .join("session_env");
        let wire = session_dir.join("agents").join("main").join("wire.jsonl");
        std::fs::create_dir_all(wire.parent().expect("wire parent")).expect("mkdir wire");
        std::fs::write(&wire, b"{\"type\":\"metadata\"}\n").expect("write wire");
        std::fs::write(
            env_home.path().join("session_index.jsonl"),
            format!(
                "{{\"sessionId\":\"session_env\",\"sessionDir\":\"{}\",\"workDir\":\"{}\"}}\n",
                session_dir.display(),
                work.path().display(),
            ),
        )
        .expect("write index");

        // Guard serializes env mutation + restores the prior value on drop.
        let _guard = crate::agent::adapter::KimiHomeEnvGuard::acquire();
        std::env::set_var("KIMI_CODE_HOME", env_home.path());
        let bindings = AgentBindings::for_attach(&kimi_ctx(Some(PathBuf::from("/bogus/provider"))))
            .expect("kimi binds");
        let located = bindings
            .locator
            .locate(work.path(), "pty-1")
            .expect("locate under env home");

        assert!(
            located.status_path.starts_with(env_home.path()),
            "status_path must resolve under $KIMI_CODE_HOME, got {}",
            located.status_path.display(),
        );
    }

    /// An explicit provider-home override is stronger than the detected
    /// process's own `$KIMI_CODE_HOME`. E2E uses this to keep watcher fixtures
    /// hermetic even when the runner process has a real Kimi home in its
    /// environment.
    #[test]
    fn for_attach_kimi_provider_home_override_beats_proc_environ_home() {
        let env_home = tempfile::tempdir().expect("env home");
        let override_home = tempfile::tempdir().expect("override home");
        let proc_root = tempfile::tempdir().expect("proc root");
        let work = tempfile::tempdir().expect("work dir");
        let proc_pid_dir = proc_root.path().join("2");
        std::fs::create_dir_all(&proc_pid_dir).expect("mkdir proc pid");
        std::fs::write(
            proc_pid_dir.join("environ"),
            format!("KIMI_CODE_HOME={}\0", env_home.path().display()),
        )
        .expect("write proc environ");

        for (home, session_id) in [
            (env_home.path(), "session_env"),
            (override_home.path(), "session_override"),
        ] {
            let session_dir = home.join("sessions").join("wd_x").join(session_id);
            let wire = session_dir.join("agents").join("main").join("wire.jsonl");
            std::fs::create_dir_all(wire.parent().expect("wire parent")).expect("mkdir wire");
            std::fs::write(&wire, b"{\"type\":\"metadata\"}\n").expect("write wire");
            std::fs::write(
                home.join("session_index.jsonl"),
                format!(
                    "{{\"sessionId\":\"{}\",\"sessionDir\":\"{}\",\"workDir\":\"{}\"}}\n",
                    session_id,
                    session_dir.display(),
                    work.path().display(),
                ),
            )
            .expect("write index");
        }

        let mut ctx = kimi_ctx(Some(PathBuf::from("/bogus/provider")));
        ctx.provider_home_override = Some(override_home.path().to_path_buf());
        ctx.proc_root = Some(proc_root.path().to_path_buf());

        let bindings = AgentBindings::for_attach(&ctx).expect("kimi binds");
        let located = bindings
            .locator
            .locate(work.path(), "pty-1")
            .expect("locate under explicit override home");

        assert!(
            located.status_path.starts_with(override_home.path()),
            "status_path must resolve under the explicit override home, got {}",
            located.status_path.display(),
        );
        assert_eq!(
            located.agent_session_id.as_deref(),
            Some("session_override")
        );
    }

    // NOTE: the B' shared-`Arc` regression test
    // (`for_attach_codex_shares_arc_between_streamer_and_facade`, cycle 14
    // F39) was removed in B''. It pinned that `bindings.streamer` and the
    // now-deleted `bindings.adapter_for_transcript_state` were clones of
    // the same `Arc<CodexAdapter>`. With the façade field gone, the
    // cycle-11 F31 invariant ("one `CompositeLocator` per Codex attach,
    // shared between the outer locator and the adapter's internal locator")
    // is pinned at its source instead — see
    // `codex::mod::tests::with_locator_shares_passed_locator_allocation`,
    // which asserts `CodexAdapter::with_locator` stores the exact `Arc`
    // it was handed rather than rebuilding one.

    /// Claude's locator writes Vimeflow-owned status bridge files under
    /// app data rather than the user's project tree, with
    /// `static_transcript_hint == None`.
    #[test]
    fn for_attach_claude_locator_returns_static_path() {
        let ctx = claude_ctx();
        let bindings = AgentBindings::for_attach(&ctx).expect("claude binds");
        let cwd = PathBuf::from("/tmp/ws");
        let located: LocatedStatusSource = bindings
            .locator
            .locate(&cwd, "sess-1")
            .expect("locator infallible for claude");
        assert_eq!(
            located.status_path,
            crate::agent::adapter::claude_code::bridge::session_status_file(
                &ctx.app_data_dir,
                &cwd,
                "sess-1",
            ),
        );
        assert_eq!(located.trust_root, ctx.app_data_dir);
        assert_eq!(located.static_transcript_hint, None);
    }

    /// Codex `for_attach` with `provider_home == None` falls back to
    /// `default_codex_home()` rather than failing. Pin the behavior so
    /// headless / service sessions (where `dirs::home_dir()` returns
    /// `None`) keep attaching, matching the pre-B' path through
    /// `CodexAdapter::new` (PR #261 codex review F3).
    ///
    /// Coverage scope:
    /// - **No panic / no Err**: the successful `expect(...)` proves
    ///   both that the `unwrap_or_else(default_codex_home)` fallback
    ///   fired AND that `CompositeLocator::new` +
    ///   `CodexAdapter::with_locator` accept the resulting `PathBuf`
    ///   without panicking.
    /// - **Dispatch hits the Codex arm**: the `agent_type` round-trip
    ///   confirms the test exercised the `AgentType::Codex` branch
    ///   (a regression that dropped through to `NoOpAdapter` would
    ///   fail this assertion).
    ///
    /// **NOT pinned here**: that `bindings.locator` and
    /// `bindings.streamer`'s internal locator reference the SAME
    /// `Arc<CompositeLocator>` instance (cycle 11 F31). That
    /// single-allocation invariant is pinned at its source in
    /// `codex::adapter_tests::with_locator_shares_passed_locator_allocation`,
    /// which doesn't need the test-only downcast / SQLite-fixture
    /// machinery this bindings-level test would.
    #[test]
    fn for_attach_codex_without_provider_home_falls_back_to_default_home() {
        let bindings =
            AgentBindings::for_attach(&codex_ctx(None)).expect("codex falls back to default home");
        assert_eq!(bindings.agent_type, AgentType::Codex);
    }
}
