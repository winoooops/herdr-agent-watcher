//! Attach-level error type for the agent adapter.
//!
//! Step B' of the v4-frozen refactor plan (#246). One of three error
//! enums in the adapter stack (frozen constraint #4):
//!
//! - [`crate::agent::adapter::codex::locator::LocatorError`] —
//!   provider-local (Codex), lives inside the locator impl.
//! - [`AttachError`] — attach-level. **In B' the only wired path is
//!   [`AgentBindings::for_attach`] returning `Result<Self, AttachError>`,
//!   which `start_agent_watcher_inner` maps to `String`.** As of PR
//!   #261 codex round-4 fix, `for_attach` is infallible — the former
//!   `LocatorFatal` for missing `provider_home` was replaced by a
//!   `default_codex_home()` fallback so headless / service sessions
//!   without a resolvable `$HOME` keep attaching. Locator failures
//!   raised by `StatusSourceLocator::locate` still surface as
//!   `String` through `base::start_for`. All six variants
//!   (`NoAgentDetected` / `UnsupportedAgent` / `LocatorExhausted` /
//!   `LocatorFatal` / `LocatorAmbiguous` / `ValidatorRejected`) are
//!   reserved per #246 acceptance for the D' `AgentWatcherService`
//!   boundary, which will retype the locator / validator path.
//! - [`crate::agent::adapter::types::ValidateTranscriptError`] — stays
//!   separate because it feeds `TxOutcome` diagnostics in the runtime,
//!   not attach failures.
//!
//! [`AgentBindings::for_attach`]: crate::agent::adapter::bindings::AgentBindings::for_attach

use std::fmt;

use crate::agent::adapter::types::ValidateTranscriptError;
use crate::agent::types::AgentType;

/// Errors that can happen while binding an attach context to an
/// agent's runtime hooks.
///
/// The variant set is the minimum from #246's Step B' acceptance
/// criteria. Future steps (D') may add variants as new failure modes
/// become observable from the service facade.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum AttachError {
    /// `AttachContext` was built without an agent type set — no agent
    /// was detected in the PTY session's process tree. Currently
    /// surfaced at upstream detection rather than at `for_attach`, but
    /// retained as a defined variant per the acceptance enum.
    NoAgentDetected,
    /// `AttachContext.agent_type` is a variant the adapter stack
    /// doesn't have a real implementation for (e.g. `Aider` /
    /// `Generic` today). The bindings can still be constructed via
    /// `NoOpAdapter`, so this variant is reserved for the future case
    /// where `for_attach` opts to refuse rather than no-op.
    UnsupportedAgent(AgentType),
    /// Locator retry budget exhausted without producing a candidate.
    /// Wraps the provider-local `LocatorError::NotYetReady` chain.
    LocatorExhausted(String),
    /// Locator hit a non-recoverable error (filesystem permission,
    /// SQLite corruption, schema drift). Wraps the provider-local
    /// `LocatorError::Fatal` / `LocatorError::Unresolved` paths.
    LocatorFatal(String),
    /// Locator returned multiple candidates with no unique winner.
    /// Currently unused by the Codex composite locator (the
    /// SqliteFirst/FsScan strategies are mutually exclusive), but
    /// reserved per the acceptance enum so a future locator that
    /// surfaces ambiguity has a typed variant to use.
    LocatorAmbiguous(String),
    /// `TranscriptPathValidator` rejected a path produced by the
    /// `TranscriptPathSource`. Currently the validator runs lazily
    /// inside the watcher, not at `for_attach`, so this variant is
    /// reserved for D' where an eager pre-flight validation may run.
    ValidatorRejected(ValidateTranscriptError),
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAgentDetected => f.write_str("no agent detected in PTY session"),
            Self::UnsupportedAgent(at) => write!(f, "unsupported agent type: {:?}", at),
            Self::LocatorExhausted(reason) => write!(f, "locator retry exhausted: {}", reason),
            Self::LocatorFatal(reason) => write!(f, "locator fatal: {}", reason),
            Self::LocatorAmbiguous(reason) => write!(f, "locator ambiguous: {}", reason),
            Self::ValidatorRejected(inner) => write!(f, "validator rejected: {}", inner),
        }
    }
}

impl std::error::Error for AttachError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Display contract — the strings are not currently parsed by any
    /// caller, but they are how `AttachError → String` mapping at the
    /// D' boundary will look. Pin the prefixes so future variant
    /// additions don't accidentally re-shape the existing ones.
    #[test]
    fn display_carries_stable_prefixes() {
        assert!(AttachError::NoAgentDetected
            .to_string()
            .starts_with("no agent detected"),);
        assert!(AttachError::UnsupportedAgent(AgentType::Aider)
            .to_string()
            .starts_with("unsupported agent type:"),);
        assert!(AttachError::LocatorExhausted("ran out of retries".into())
            .to_string()
            .starts_with("locator retry exhausted:"),);
        assert!(AttachError::LocatorFatal("sqlite is corrupt".into())
            .to_string()
            .starts_with("locator fatal:"),);
        assert!(AttachError::LocatorAmbiguous("two candidates tied".into())
            .to_string()
            .starts_with("locator ambiguous:"),);
        assert!(
            AttachError::ValidatorRejected(ValidateTranscriptError::NotFound(PathBuf::from(
                "/tmp/missing.jsonl"
            )))
            .to_string()
            .starts_with("validator rejected:"),
        );
    }

    /// Six variants — pin the variant count so adding a new one
    /// requires the contributor to consider whether each consumer
    /// (Display, D' mapping, future telemetry) needs an update.
    #[test]
    fn variant_count_check() {
        let _ = |e: AttachError| match e {
            AttachError::NoAgentDetected => 1,
            AttachError::UnsupportedAgent(_) => 2,
            AttachError::LocatorExhausted(_) => 3,
            AttachError::LocatorFatal(_) => 4,
            AttachError::LocatorAmbiguous(_) => 5,
            AttachError::ValidatorRejected(_) => 6,
        };
    }
}
