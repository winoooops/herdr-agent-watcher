//! Provider-hook types used by `AgentAdapter` implementations.

use std::path::PathBuf;

use crate::agent::types::{AgentStatusEvent, ContextWindowStatus, CostMetrics, RateLimits};

/// Raw, untrusted, not-yet-validated transcript path emitted by either
/// the locator at attach time (via
/// `TranscriptPathSource::static_hint`) or the statusline decoder at
/// each update (via `TranscriptPathSource::dynamic_hint`).
/// Validation through `AgentAdapter::validate_transcript` converts a
/// `RawPath` into a canonicalized `PathBuf` before any transcript
/// tailing happens.
///
/// Type alias rather than newtype because every caller already works
/// with `String`; a newtype would add ceremony without strengthening
/// the safety boundary, which lives at `validate_transcript`.
pub type RawPath = String;

/// Located statusline source — what
/// `AgentAdapter::located_status_source` returns at attach time.
///
/// Step 0c rename of the former `StatusSource`. The new
/// `static_transcript_hint` field carries Codex's locator-known rollout
/// path so the runtime can ask `TranscriptPathSource::static_hint`
/// for it without depending on the adapter-private `Mutex` side channel
/// (deprecated in 0c, kept in place for back-compat; targeted for
/// removal in a later step).
#[derive(Debug, Clone)]
pub struct LocatedStatusSource {
    /// Filesystem path of the statusline file the watcher will observe.
    pub status_path: PathBuf,
    /// Directory used as the trust root by `path_security` checks when
    /// validating any transcript path paired with this statusline.
    pub trust_root: PathBuf,
    /// Transcript path known at attach time. `Some(_)` for Codex (the
    /// locator resolves the rollout file before any statusline update);
    /// `None` for Claude (the path is dynamic, arriving inside every
    /// statusline JSON update — see
    /// `TranscriptPathSource::dynamic_hint`).
    ///
    /// Held here so the runtime can pass a `&LocatedStatusSource` into
    /// `static_hint` instead of threading the path through
    /// `parse_status` as a side channel.
    pub static_transcript_hint: Option<RawPath>,
    /// Adapter-internal session identifier known at attach time —
    /// `Some(thread_id)` for Codex (the locator returns it alongside
    /// the rollout path), `None` for Claude / NoOp adapters.
    ///
    /// Held here so the lifecycle layer can wire codex's
    /// `session_index.jsonl` title-sync watcher (which keys on this id
    /// to find the matching row) without a side-channel back into the
    /// locator (PR #302 codex review F5 — the previous refactor
    /// dropped this wiring, parking the `agent-session-title` emit path
    /// referenced in `events.rs`). Distinct from
    /// `AgentStatusEvent.session_id` — that one is the Vimeflow PTY
    /// session id, stamped by the runtime; this one is the agent's own
    /// internal id.
    pub agent_session_id: Option<String>,
    /// Workspace directory resolved by the adapter locator at attach time.
    ///
    /// OpenCode records the project directory in its bridge index but does not
    /// currently emit OSC 7, so the PTY cwd can remain stuck at the terminal's
    /// spawn directory. When present, the runtime should prefer this value for
    /// the initial transcript cwd.
    pub resolved_directory: Option<PathBuf>,
}

/// Decoder output — provider-neutral status state, **session-id-free**
/// and **transcript-path-free**.
///
/// Defined in step 0c per the v4-frozen plan; reserved for the
/// state-decoder split (Step B') that moves status decoding off the
/// `AgentAdapter` trait. For 0c the type exists but no caller consumes
/// it directly — `parse_status` continues to return [`ParsedStatus`]
/// (a session-id-stamped envelope built by runtime code), which has
/// lost its `transcript_path` field in this step.
///
/// Distinct from `ParsedStatus`/`AgentStatusEvent` in two structural
/// ways:
///
/// 1. No `session_id` — the Vimeflow PTY session id is a runtime fact,
///    not a decoder output. Future Step B' will have the decoder
///    return this type, and the runtime composes
///    `AgentStatusEvent { session_id, ..snapshot }` afterwards.
/// 2. No transcript path — that lookup is fully owned by
///    `TranscriptPathSource`; the decoder never surfaces it.
///
/// Field set mirrors the non-`session_id` fields of
/// [`AgentStatusEvent`]. Step B' wired the decoder to return this
/// type via `crate::agent::adapter::traits::StateDecoder` — the
/// session-id stamp now happens in the runtime composition layer
/// (`parse_statusline` / `parse_rollout` test wrappers for now,
/// `AgentAdapter::parse_status` until B''/D' migrate it).
#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    /// Agent's internal session id (distinct from the Vimeflow PTY
    /// session id, which lives on `AgentStatusEvent.session_id` and is
    /// stamped by runtime code, not by the decoder).
    pub agent_session_id: String,
    pub model_id: String,
    pub model_display_name: String,
    pub version: String,
    pub context_window: ContextWindowStatus,
    pub cost: CostMetrics,
    pub rate_limits: RateLimits,
    /// Whether `rate_limits` carries a real network-fetched value rather than a
    /// placeholder default. Only kimi fetches usage over the network, so this is
    /// `false` for claude/codex (they read local files) and for a kimi session
    /// that hasn't fetched yet — the kimi usage gate uses it to tell LOADING
    /// from ON without guessing from zeroed values.
    pub usage_fetched: bool,
}

/// Statusline-parser output as consumed by `base/watcher_runtime`.
///
/// Step 0c removed the `transcript_path: Option<String>` field — the
/// runtime now resolves transcript paths via `TranscriptPathSource`
/// instead of via this side channel.
#[derive(Debug, Clone)]
pub struct ParsedStatus {
    pub event: AgentStatusEvent,
}

/// Stamp a session-id-free [`StatusSnapshot`] with a Vimeflow PTY
/// session id, producing the [`AgentStatusEvent`] that the rest of
/// the runtime consumes.
///
/// Centralizes the eight-field copy that was originally duplicated
/// across three call sites in `base/watcher_runtime`,
/// `claude_code/statusline`, and the test-only `codex/parser` (PR #261
/// cycle 1 review F2 introduced this helper; later cycles deleted the
/// intermediate `compose_event` / `snapshot_to_event` shims so all
/// three sites now call this helper directly). Future
/// `AgentStatusEvent` field additions only need to update this one
/// mapping.
pub(crate) fn stamp_snapshot(session_id: &str, snapshot: StatusSnapshot) -> AgentStatusEvent {
    AgentStatusEvent {
        session_id: session_id.to_string(),
        agent_session_id: snapshot.agent_session_id,
        model_id: snapshot.model_id,
        model_display_name: snapshot.model_display_name,
        version: snapshot.version,
        context_window: snapshot.context_window,
        cost: snapshot.cost,
        rate_limits: snapshot.rate_limits,
        usage_fetched: snapshot.usage_fetched,
    }
}

/// Where transcript paths come from. Step 0c extracted this from
/// `AgentAdapter`; step B' narrows visibility from `pub` to
/// `pub(crate)` so it lines up with the other 4 split traits in
/// `crate::agent::adapter::traits` (frozen constraint #3 — all five
/// adapter-concern traits stay internal).
///
/// Per-provider contract:
///
/// - **Claude:** `static_hint` returns `None`; `dynamic_hint(raw)`
///   extracts `transcript_path` from the statusline JSON on every
///   update.
/// - **Codex:** `static_hint(&located)` returns
///   `located.static_transcript_hint.clone()` (the locator-known
///   rollout path captured at attach time); `dynamic_hint` returns
///   `None`.
///
/// The runtime asks `dynamic_hint` first (fresh-per-update Claude
/// path) and falls back to `static_hint` (steady-state Codex path).
/// If both return `None`, no transcript is tailed.
pub(crate) trait TranscriptPathSource: Send + Sync {
    /// Transcript path known at attach time. Default returns `None`;
    /// Codex overrides to surface the rollout path stored in the
    /// supplied [`LocatedStatusSource`].
    fn static_hint(&self, located: &LocatedStatusSource) -> Option<RawPath> {
        let _ = located;
        None
    }

    /// Transcript path extracted from the raw statusline content on
    /// every update. Default returns `None`; Claude overrides to parse
    /// the JSON's `transcript_path` field.
    fn dynamic_hint(&self, raw: &str) -> Option<RawPath> {
        let _ = raw;
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidateTranscriptError {
    NotFound(PathBuf),
    OutsideRoot { path: PathBuf, root: PathBuf },
    NotAFile(PathBuf),
    InvalidPath(String),
    Other(String),
}

impl std::fmt::Display for ValidateTranscriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(path) => write!(f, "transcript path not found: {}", path.display()),
            Self::OutsideRoot { path, root } => write!(
                f,
                "transcript path is outside Claude directory: {} not under {}",
                path.display(),
                root.display()
            ),
            Self::NotAFile(path) => write!(f, "not a transcript file: {}", path.display()),
            // `InvalidPath` carries a structural prefix so log scrapers
            // and SIEM rules keying on Display output can distinguish
            // potentially-adversarial input (currently null-byte
            // injection) from generic validation failures, even if the
            // inner message wording changes (Claude review on PR #153,
            // F7). The matching `TxOutcome::InvalidPath` variant in
            // `diagnostics.rs` provides the structured signal at the
            // tx_status= layer; this prefix complements that for
            // free-text-only consumers.
            Self::InvalidPath(message) => write!(f, "invalid transcript path: {}", message),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ValidateTranscriptError {}

#[cfg(test)]
mod display_tests {
    use super::*;
    use std::path::PathBuf;

    /// Pinned-format regression test for the `Display` impl. Cycle-3 of
    /// PR #153 introduced a stable `invalid transcript path: ` prefix
    /// for `InvalidPath` so SIEM rules and log scrapers can distinguish
    /// security-relevant validation failures from generic ones (F7).
    /// The prefix is therefore part of the de-facto logging contract;
    /// future edits to `Display` should preserve it.
    #[test]
    fn display_invalid_path_has_stable_security_prefix() {
        let e =
            ValidateTranscriptError::InvalidPath("transcript path contains null byte".to_string());
        assert_eq!(
            e.to_string(),
            "invalid transcript path: transcript path contains null byte"
        );
    }

    #[test]
    fn display_other_remains_bare_message() {
        let e = ValidateTranscriptError::Other("something else went wrong".to_string());
        assert_eq!(e.to_string(), "something else went wrong");
    }

    #[test]
    fn display_invalid_path_and_other_are_structurally_distinguishable() {
        let invalid = ValidateTranscriptError::InvalidPath("msg".to_string());
        let other = ValidateTranscriptError::Other("msg".to_string());
        assert_ne!(
            invalid.to_string(),
            other.to_string(),
            "InvalidPath and Other must produce different Display output \
             so log-only consumers can tell them apart"
        );
    }

    #[test]
    fn display_not_found_and_outside_root_unchanged() {
        let nf = ValidateTranscriptError::NotFound(PathBuf::from("/tmp/missing.jsonl"));
        assert!(nf.to_string().starts_with("transcript path not found: "));

        let outside = ValidateTranscriptError::OutsideRoot {
            path: PathBuf::from("/etc/passwd"),
            root: PathBuf::from("/home/user/.claude"),
        };
        let s = outside.to_string();
        assert!(s.starts_with("transcript path is outside Claude directory: "));
        assert!(s.contains(" not under "));
    }
}
