//! Provider-neutral agent watcher orchestration.
//!
//! This module intentionally keeps a narrow external surface while splitting
//! the runtime internals into smaller files. Adapter implementations provide
//! provider-specific hooks; this layer owns watcher lifecycle, transcript
//! lifecycle, diagnostics, and path-trust enforcement.

mod diagnostics;
mod path_security;
mod transcript_state;
mod transcript_tail_service;
mod watcher_runtime;

pub(crate) use path_security::{ensure_trusted, TrustedLocatedSource};
pub use transcript_state::{TranscriptHandle, TranscriptStartStatus, TranscriptState};
pub(crate) use transcript_tail_service::{
    for_each_bounded_line, TranscriptDecoder, TranscriptTailService,
};
pub(crate) use watcher_runtime::start_watching;
// `RecordingDecoder` stays module-private to `transcript_tail_service` (only its
// own tests use it); 2.3's Claude end-to-end test imports these two.
#[cfg(test)]
pub(crate) use transcript_tail_service::{ScriptedReader, Step};
pub use watcher_runtime::{AgentWatcherState, WatcherHandle};
