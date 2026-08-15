//! Built-in agent registry — the single source of truth for per-agent
//! metadata used at attach time and during process detection.
//!
//! **The intent:** adding a new coding agent should touch ONE place: this
//! file. The existing `AgentType` enum at
//! [`crate::agent::types::AgentType`] is the registry key; this module
//! supplies the declarative metadata per variant (display name, binary
//! names for `/proc/<pid>/cmdline` detection, home subdir).
//!
//! Tracking: [#246](https://github.com/winoooops/vimeflow/issues/246)
//! (Step 0a of the `refactor/agent-adapter` v4-frozen plan, expanded
//! from the original Step 0a per PR #247 review).
//!
//! **YAGNI note:** Step 0a holds the minimum field set needed for
//! `detector` + `AttachContext` to work coherently as a whole. Later
//! steps (B' / C') may extend `AgentSpec` with adapter-routing, status-
//! source-kind, or transcript-root fields — defer until those steps
//! actually consume them.

use std::path::PathBuf;

use crate::agent::types::AgentType;

/// Declarative metadata for a built-in agent. Construct only as `&'static`
/// entries in [`AGENT_SPECS`]; no runtime instances.
#[derive(Debug)]
#[allow(dead_code)] // display_name reserved for future UI / log surfaces.
pub(crate) struct AgentSpec {
    /// Stable identifier — matches the `AgentType` enum variant. Used as
    /// the lookup key in [`spec_for`].
    pub(crate) agent_type: AgentType,
    /// Human-readable label (e.g. for logs, future UI status pills).
    pub(crate) display_name: &'static str,
    /// Process-binary names recognised by [`crate::agent::detector`] for
    /// this agent. First match wins. Empty means "never detected by
    /// binary name" (e.g. `Generic`).
    pub(crate) binary_names: &'static [&'static str],
    /// Subdirectory under the user's home that holds the agent's
    /// per-user data (e.g. `.codex`, `.claude`). `None` means the agent
    /// has no canonical home directory.
    pub(crate) home_subdir: Option<&'static str>,
}

impl AgentSpec {
    /// Resolve the absolute provider home for this agent on the current
    /// host. Returns `None` if the agent has no home subdir or if
    /// `dirs::home_dir()` cannot resolve a user home.
    pub(crate) fn provider_home(&self) -> Option<PathBuf> {
        self.home_subdir
            .and_then(|subdir| dirs::home_dir().map(|home| home.join(subdir)))
    }
}

/// The full set of built-in agents Vimeflow knows about. Adding a new
/// CLI coding agent = adding one entry here (plus its `AgentType` enum
/// variant and adapter impl).
pub(crate) const AGENT_SPECS: &[AgentSpec] = &[
    AgentSpec {
        agent_type: AgentType::ClaudeCode,
        display_name: "Claude Code",
        binary_names: &["claude"],
        home_subdir: Some(".claude"),
    },
    AgentSpec {
        agent_type: AgentType::Codex,
        display_name: "Codex",
        binary_names: &["codex"],
        home_subdir: Some(".codex"),
    },
    AgentSpec {
        agent_type: AgentType::Kimi,
        display_name: "Kimi",
        // A running kimi rewrites argv0 to "kimi-code" (process.title); match both.
        binary_names: &["kimi", "kimi-code"],
        home_subdir: Some(".kimi-code"),
    },
    AgentSpec {
        agent_type: AgentType::Opencode,
        display_name: "opencode",
        binary_names: &["opencode"],
        // TODO(M6): opencode uses the platform data directory
        // (`dirs::data_dir().join("opencode")`), not a home-relative
        // dotdir. Leave this unresolved until the opencode adapter consumes
        // provider_home so non-Linux builds do not receive a bad path.
        home_subdir: None,
    },
    AgentSpec {
        agent_type: AgentType::Aider,
        display_name: "Aider",
        binary_names: &["aider"],
        home_subdir: None,
    },
    AgentSpec {
        agent_type: AgentType::Generic,
        display_name: "Generic",
        binary_names: &[],
        home_subdir: None,
    },
];

/// Look up the spec for a given agent type. Panics if the registry has
/// drifted away from [`AgentType`] — the registry must cover every enum
/// variant. The panic fires in both debug and release builds; treat it
/// as a programming-error guard, not a recoverable runtime error.
pub(crate) fn spec_for(agent_type: AgentType) -> &'static AgentSpec {
    AGENT_SPECS
        .iter()
        .find(|spec| spec.agent_type == agent_type)
        .unwrap_or_else(|| {
            // Programming error: a new AgentType variant was added without
            // a matching AGENT_SPECS entry. Surface immediately rather
            // than returning a synthetic fallback that would mask the
            // drift.
            panic!(
                "agent::config: no AgentSpec registered for {:?} — \
                 update AGENT_SPECS",
                agent_type
            )
        })
}

/// Detector-facing lookup: map a process binary name (e.g. `"claude"`,
/// `"codex"`) to the agent type that owns it. `None` for unrecognised
/// binaries.
pub(crate) fn agent_type_for_binary(binary_name: &str) -> Option<AgentType> {
    AGENT_SPECS
        .iter()
        .find(|spec| spec.binary_names.contains(&binary_name))
        .map(|spec| spec.agent_type)
}

/// Resolve the path to `/proc` per platform.
///
/// Codex's binding fast-paths (`resume_thread_id_from_proc`,
/// `open_rollout_paths_from_proc`) read `/proc/<pid>/cmdline` and
/// `/proc/<pid>/fd/*`. Those only exist on Linux, so non-Linux platforms
/// get `None` and the locator falls through to its FS-scan strategy.
pub(crate) fn default_proc_root() -> Option<PathBuf> {
    if cfg!(target_os = "linux") {
        Some(PathBuf::from("/proc"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry must cover every `AgentType` variant. Two layered
    /// checks make the contract hold:
    ///
    /// 1. **Compile-time** — the closure below contains a `match` whose
    ///    arms list every known variant. Adding a new variant to
    ///    `AgentType` without updating this arm fails to compile here.
    ///    The closure is never invoked; the exhaustiveness check fires
    ///    at definition time regardless.
    /// 2. **Runtime** — the loop below calls `spec_for` for each known
    ///    variant. If a variant exists in the enum + match arm but
    ///    `AGENT_SPECS` is missing its entry, `spec_for` panics here
    ///    (failing this test) rather than only at production attach.
    ///
    /// The two checks are intentionally redundant: (1) catches "added
    /// variant, forgot test"; (2) catches "added variant + test arm,
    /// forgot `AGENT_SPECS`".
    #[test]
    fn registry_covers_every_agent_type() {
        // (1) Compile-time exhaustiveness — adding a new AgentType
        //     variant without listing it here triggers a non-exhaustive
        //     match error at this match expression.
        let _ = |at: AgentType| match at {
            AgentType::ClaudeCode
            | AgentType::Codex
            | AgentType::Kimi
            | AgentType::Opencode
            | AgentType::Aider
            | AgentType::Generic => spec_for(at),
        };

        // (2) Runtime registry-presence check — must mirror the match
        //     arms above. `spec_for` panics if AGENT_SPECS is missing
        //     the entry, failing the test loudly.
        for agent_type in [
            AgentType::ClaudeCode,
            AgentType::Codex,
            AgentType::Kimi,
            AgentType::Opencode,
            AgentType::Aider,
            AgentType::Generic,
        ] {
            let _ = spec_for(agent_type);
        }
    }

    #[test]
    fn spec_for_returns_matching_entry() {
        let claude = spec_for(AgentType::ClaudeCode);
        assert_eq!(claude.display_name, "Claude Code");
        assert_eq!(claude.home_subdir, Some(".claude"));

        let codex = spec_for(AgentType::Codex);
        assert_eq!(codex.display_name, "Codex");
        assert_eq!(codex.home_subdir, Some(".codex"));
    }

    #[test]
    fn agent_type_for_binary_maps_canonical_names() {
        assert_eq!(agent_type_for_binary("claude"), Some(AgentType::ClaudeCode));
        assert_eq!(agent_type_for_binary("codex"), Some(AgentType::Codex));
        assert_eq!(agent_type_for_binary("kimi"), Some(AgentType::Kimi));
        assert_eq!(agent_type_for_binary("kimi-code"), Some(AgentType::Kimi));
        assert_eq!(agent_type_for_binary("opencode"), Some(AgentType::Opencode));
        assert_eq!(agent_type_for_binary("aider"), Some(AgentType::Aider));
    }

    #[test]
    fn agent_type_for_binary_returns_none_for_unknown() {
        assert_eq!(agent_type_for_binary("bash"), None);
        assert_eq!(agent_type_for_binary(""), None);
        assert_eq!(agent_type_for_binary("python"), None);
    }

    #[test]
    fn provider_home_returns_home_join_subdir_when_resolvable() {
        // dirs::home_dir() is real here; we only assert structural shape
        // (ends with the registered subdir) rather than pinning an
        // exact value that depends on $HOME.
        let codex = spec_for(AgentType::Codex);
        let home = codex
            .provider_home()
            .expect("codex provider home should resolve on this host");
        assert!(home.ends_with(".codex"));
    }

    #[test]
    fn provider_home_is_none_when_subdir_missing() {
        let opencode = spec_for(AgentType::Opencode);
        assert_eq!(opencode.provider_home(), None);

        let aider = spec_for(AgentType::Aider);
        assert_eq!(aider.provider_home(), None);

        let generic = spec_for(AgentType::Generic);
        assert_eq!(generic.provider_home(), None);
    }

    /// Platform-shape regression: `default_proc_root` returns `Some` only
    /// on Linux. Other platforms get `None` so the Codex locator falls
    /// through to its FS-scan strategy without trying to read
    /// `/proc/<pid>/cmdline` on macOS/Windows.
    #[test]
    fn default_proc_root_matches_target_os() {
        let proc_root = default_proc_root();
        if cfg!(target_os = "linux") {
            assert_eq!(proc_root, Some(PathBuf::from("/proc")));
        } else {
            assert_eq!(proc_root, None);
        }
    }
}
