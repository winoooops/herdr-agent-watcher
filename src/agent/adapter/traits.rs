//! The four new trait splits introduced by Step B' of the v4-frozen
//! refactor plan (#246). Step 0c already extracted
//! [`crate::agent::adapter::types::TranscriptPathSource`] into the
//! types module; this file collects the four remaining splits in one
//! place so a reader can see the post-B' adapter contract at a
//! glance:
//!
//! - [`StatusSourceLocator`] — "where is the statusline file for this
//!   session" + the attach-time transcript hint (former
//!   `AgentAdapter::located_status_source`).
//! - [`StateDecoder`] — "given raw statusline content, what's the
//!   current snapshot" (former `AgentAdapter::parse_status`, now
//!   returning [`StatusSnapshot`] without `session_id`; the runtime
//!   stamps the session id when composing `AgentStatusEvent`).
//! - [`TranscriptPathValidator`] — "is this raw path safe to tail"
//!   (former `AgentAdapter::validate_transcript`).
//! - [`TranscriptStreamer`] — "spawn a thread that emits events from
//!   this JSONL" (former `AgentAdapter::tail_transcript`).
//!
//! All four are `pub(crate)` per frozen constraint #3. As of step
//! B'', `TranscriptState::start_or_replace` takes
//! `Arc<dyn TranscriptStreamer>` directly (B' had it on the
//! transitional `Arc<dyn AgentAdapter>` façade). The `AgentAdapter`
//! trait stays alive only as the not-yet-removed façade — D' deletes
//! it with the `AgentWatcherService` boundary. Until then, each
//! adapter struct implements both `AgentAdapter` and the split traits
//! side by side.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::base::TranscriptHandle;
use super::types::{LocatedStatusSource, StatusSnapshot, ValidateTranscriptError};
use crate::runtime::EventSink;

/// Discover the statusline file + the attach-time transcript hint
/// for one PTY session.
///
/// Replaces `AgentAdapter::located_status_source` in the post-B'
/// world. The trait error type is `String` because providers vary in
/// what they classify as a hard failure: Claude's locator is
/// infallible (it just joins a path), Codex's locator does
/// retry+fallback across multiple strategies and folds an internal
/// `LocatorError` into a `String` at this boundary.
pub(crate) trait StatusSourceLocator: Send + Sync {
    fn locate(
        &self,
        cwd: &std::path::Path,
        session_id: &str,
    ) -> Result<LocatedStatusSource, String>;
}

/// Decode raw statusline content into a provider-neutral
/// [`StatusSnapshot`]. Replaces `AgentAdapter::parse_status` in the
/// post-B' world.
///
/// The decoder output (`StatusSnapshot`) is deliberately
/// session-id-free — the runtime composes
/// `AgentStatusEvent { session_id, ...snapshot }` after the decoder
/// returns. This was R2.2 of the v4-frozen plan: "decoder
/// session-id-free, runtime stamps the id".
///
/// The `session_id: Option<&str>` parameter is **diagnostic-only**:
/// providers may attach it to per-line log messages so multi-session
/// debugging can correlate "malformed rollout line" warnings to the
/// affected PTY session. It does NOT appear in the returned
/// `StatusSnapshot` and MUST NOT influence decoding semantics — same
/// raw bytes + different session_id must yield the same snapshot. R2.2
/// is therefore preserved (output identity-free; input is annotated
/// for observability only). PR #261 cycle 2 review flagged the
/// information loss: the pre-B' parser logged `for sid={session_id}`,
/// and the refactor stripped that context.
///
/// **`Err` semantics — provider-specialized**. The watcher maps any
/// `Err` to `TxOutcome::ParseError` and logs at warn level, but
/// implementations are NOT required to return `Err` on malformed
/// input. Two valid shapes:
///
/// - **Atomic decoders** (Claude statusline — one JSON object per
///   read): `Err` on invalid JSON / wrong top-level shape. Every
///   read is all-or-nothing.
/// - **Streaming / line-folded decoders** (Codex rollout JSONL —
///   accumulate across many lines): `Ok` always. A single malformed
///   line is logged with session-id context (see the
///   `Option<&str>` parameter rationale above) and skipped; the
///   rest of the document folds normally. A fully-corrupt file
///   returns `Ok` with default-initialized fields (`model_id =
///   "unknown"`, `context_window_size = 0`).
///
/// Monitoring keyed on `tx_status=parse_error` therefore observes
/// Claude corruption but not Codex corruption. Operators detecting
/// Codex JSONL corruption need a different signal (the per-line
/// `log::warn!("codex: skipping malformed rollout line (sid=...)")`
/// or model-id-equals-"unknown" alerts on emitted
/// `AgentStatusEvent`s). PR #261 cycle 8 review F23 — the
/// distinction was implicit and led to a false "decoders must
/// `Err`" expectation; this clause makes it explicit.
pub(crate) trait StateDecoder: Send + Sync {
    fn decode(&self, session_id: Option<&str>, raw: &str) -> Result<StatusSnapshot, String>;
}

/// Validate a raw transcript path against path-security policy
/// before any file I/O. Replaces `AgentAdapter::validate_transcript`
/// in the post-B' world.
pub(crate) trait TranscriptPathValidator: Send + Sync {
    fn validate(&self, raw: &str) -> Result<PathBuf, ValidateTranscriptError>;
}

/// Spawn a background thread that tails the given transcript JSONL
/// and emits per-event `AgentToolCall` / `AgentTurn` / `TestRun`
/// events through the supplied [`EventSink`]. Replaces
/// `AgentAdapter::tail_transcript` in the post-B' world.
///
/// `TranscriptHandle` (defined in `base::transcript_state`) is the
/// per-stream stop / generation handle the watcher already owns.
/// As of step B'', `TranscriptState::start_or_replace` takes
/// `Arc<dyn TranscriptStreamer>` directly (it previously went through
/// the transitional `Arc<dyn AgentAdapter>` façade).
pub(crate) trait TranscriptStreamer: Send + Sync {
    fn tail(
        &self,
        events: Arc<dyn EventSink>,
        session_id: String,
        cwd: Option<PathBuf>,
        transcript_path: PathBuf,
    ) -> Result<TranscriptHandle, String>;

    /// Read completed replies for the requested correlation nonces without
    /// starting or replacing the live transcript tailer.
    fn recover_replies(
        &self,
        _session_id: &str,
        _transcript_path: &Path,
        _nonces: &HashSet<String>,
    ) -> Result<Vec<crate::agent::types::AgentReplyEvent>, String> {
        Ok(Vec::new())
    }

    /// Read completed delegated reviews for the requested correlation nonces
    /// without starting or replacing the live transcript tailer.
    fn recover_reviews(
        &self,
        _session_id: &str,
        _transcript_path: &Path,
        _nonces: &HashSet<String>,
    ) -> Result<Vec<crate::agent::types::AgentReviewEvent>, String> {
        Ok(Vec::new())
    }
}
