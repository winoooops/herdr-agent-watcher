//! opencode adapter (observability v1).
//!
//! M2 lands the opencode-side bridge plugin + its auto-installer and the wire
//! DTOs the later milestones consume. M3 adds the filesystem locator + the
//! per-process types + the transcript-path validator. M4 adds the snapshot
//! decoder (parser). M5 wires the transcript streamer + the [`OpenCodeAdapter`]
//! that assembles the five split traits, dispatched from `bindings.rs`.

// M5 wired the production callers (the `AgentType::Opencode` arm in
// `bindings.rs` builds an `OpenCodeAdapter` over these modules). `install`
// still carries items only the bindings arm + tests reach; `transcript_dto`'s
// `OpencodeIndexRowDto` and `parser`'s nothing-extra are reached transitively.
// Everything `OpenCodeAdapter` reaches is live; the residual test-only /
// staged surface keeps a narrow `#[allow(dead_code)]`.
pub(crate) mod install;
pub(crate) mod locator;
pub(crate) mod model_catalog;
pub(crate) mod parser;
pub(crate) mod transcript;
pub(crate) mod transcript_dto;
pub(crate) mod types;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::adapter::base::TranscriptHandle;
use crate::agent::adapter::traits::{StateDecoder, TranscriptPathValidator, TranscriptStreamer};
use crate::agent::adapter::types::{
    stamp_snapshot, LocatedStatusSource, ParsedStatus, RawPath, StatusSnapshot,
    TranscriptPathSource, ValidateTranscriptError,
};
use crate::agent::adapter::AgentAdapter;
use crate::agent::types::AgentType;
use crate::runtime::EventSink;

use self::locator::OpenCodeLocator;
use crate::agent::adapter::traits::StatusSourceLocator as _;

/// Adapter for the opencode CLI. Holds a shared `Arc<OpenCodeLocator>` so
/// `AgentBindings::for_attach` can share one locator between `bindings.locator`
/// and the adapter's decoder/validator/streamer views (the cycle-11 F31
/// single-allocation invariant — mirror `KimiAdapter`/`CodexAdapter`).
pub(crate) struct OpenCodeAdapter {
    locator: Arc<OpenCodeLocator>,
}

impl OpenCodeAdapter {
    /// Construct an `OpenCodeAdapter` that shares the supplied
    /// `Arc<OpenCodeLocator>` with the caller. Used by
    /// `AgentBindings::for_attach` so `bindings.locator` and the adapter's
    /// streamer/validator reference the same `Arc<OpenCodeLocator>` instance.
    pub(crate) fn with_locator(locator: Arc<OpenCodeLocator>) -> Self {
        Self { locator }
    }

    fn locator(&self) -> &OpenCodeLocator {
        &self.locator
    }
}

impl TranscriptPathSource for OpenCodeAdapter {
    /// opencode's transcript path is the `<sessionID>.jsonl` the locator
    /// resolves at attach time; surface it via the static hint.
    fn static_hint(&self, located: &LocatedStatusSource) -> Option<RawPath> {
        located.static_transcript_hint.clone()
    }

    // `dynamic_hint` defaults to `None` — opencode never writes the transcript
    // path inside the bridge JSONL stream.
}

impl StateDecoder for OpenCodeAdapter {
    fn decode(&self, session_id: Option<&str>, raw: &str) -> Result<StatusSnapshot, String> {
        // The bridge stream carries `{providerID, modelID}` but not the model's
        // context-window size; resolve it from opencode's models.dev cache.
        Ok(parser::parse_bridge_snapshot(
            session_id,
            raw,
            model_catalog::context_window,
        ))
    }
}

impl TranscriptPathValidator for OpenCodeAdapter {
    fn validate(&self, raw: &str) -> Result<PathBuf, ValidateTranscriptError> {
        // Validate against the SAME bridge root the locator resolved, so both
        // sides scope the transcript path from one source.
        locator::validate_transcript_path_with_root(raw, self.locator.effective_bridge_root())
    }
}

impl TranscriptStreamer for OpenCodeAdapter {
    fn tail(
        &self,
        events: Arc<dyn EventSink>,
        session_id: String,
        cwd: Option<PathBuf>,
        transcript_path: PathBuf,
    ) -> Result<TranscriptHandle, String> {
        transcript::start_tailing(
            events,
            session_id,
            cwd,
            transcript_path,
            self.locator.clone(),
        )
    }
}

// Transitional `AgentAdapter` façade. Every method delegates to the matching
// split-trait impl above via UFCS, exactly like `KimiAdapter`/`CodexAdapter`.
impl AgentAdapter for OpenCodeAdapter {
    fn agent_type(&self) -> AgentType {
        AgentType::Opencode
    }

    fn located_status_source(
        &self,
        _app_data_dir: &Path,
        cwd: &Path,
        session_id: &str,
    ) -> Result<LocatedStatusSource, String> {
        self.locator().locate(cwd, session_id)
    }

    fn parse_status(&self, session_id: &str, raw: &str) -> Result<ParsedStatus, String> {
        let snapshot = <Self as StateDecoder>::decode(self, Some(session_id), raw)?;
        Ok(ParsedStatus {
            event: stamp_snapshot(session_id, snapshot),
        })
    }

    fn validate_transcript(&self, raw: &str) -> Result<PathBuf, ValidateTranscriptError> {
        <Self as TranscriptPathValidator>::validate(self, raw)
    }

    fn tail_transcript(
        &self,
        events: Arc<dyn EventSink>,
        session_id: String,
        cwd: Option<PathBuf>,
        transcript_path: PathBuf,
    ) -> Result<TranscriptHandle, String> {
        <Self as TranscriptStreamer>::tail(self, events, session_id, cwd, transcript_path)
    }
}

#[cfg(test)]
use std::ffi::OsString;
#[cfg(test)]
use std::sync::{Mutex, MutexGuard};

#[cfg(test)]
use once_cell::sync::Lazy;

/// Serializes every test that mutates the process-wide opencode env vars
/// (`AGENT_WATCHER_OPENCODE_BRIDGE_DIR`, `AGENT_WATCHER_OPENCODE_PLUGINS_DIR`,
/// `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `AGENT_WATCHER_OPENCODE_MODELS_JSON`, `HOME`,
/// `OPENCODE_HOME`) so concurrent tests don't observe each other's mutations.
#[cfg(test)]
static OPENCODE_ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// The env vars [`OpencodeEnvGuard`] snapshots + restores. Kept together so the
/// guard always leaves the process environment exactly as it found it.
#[cfg(test)]
const GUARDED_ENV_KEYS: &[&str] = &[
    "AGENT_WATCHER_OPENCODE_BRIDGE_DIR",
    "AGENT_WATCHER_OPENCODE_PLUGINS_DIR",
    "AGENT_WATCHER_OPENCODE_MODELS_JSON",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "HOME",
    "OPENCODE_HOME",
];

/// RAII guard: locks [`OPENCODE_ENV_LOCK`] so env-mutating tests serialize,
/// snapshots the guarded vars, and restores them on drop.
#[cfg(test)]
pub(crate) struct OpencodeEnvGuard {
    _lock: MutexGuard<'static, ()>,
    prev: Vec<(&'static str, Option<OsString>)>,
}

#[cfg(test)]
impl OpencodeEnvGuard {
    pub(crate) fn acquire() -> Self {
        let lock = OPENCODE_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prev = GUARDED_ENV_KEYS
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();

        Self { _lock: lock, prev }
    }
}

#[cfg(test)]
impl Drop for OpencodeEnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.prev.drain(..) {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(test)]
mod adapter_tests {
    use super::*;
    use std::time::SystemTime;

    fn adapter() -> OpenCodeAdapter {
        OpenCodeAdapter::with_locator(Arc::new(OpenCodeLocator::new(
            PathBuf::from("/tmp/opencode-bridge"),
            4242,
            SystemTime::UNIX_EPOCH,
        )))
    }

    #[test]
    fn agent_type_is_opencode() {
        assert_eq!(
            <OpenCodeAdapter as AgentAdapter>::agent_type(&adapter()),
            AgentType::Opencode
        );
    }

    #[test]
    fn static_hint_surfaces_located_transcript_hint() {
        let adapter = adapter();
        let tps: &dyn TranscriptPathSource = &adapter;

        let located = LocatedStatusSource {
            status_path: PathBuf::from("/tmp/opencode-bridge/ses_x.jsonl"),
            trust_root: PathBuf::from("/tmp/opencode-bridge"),
            static_transcript_hint: Some("/tmp/opencode-bridge/ses_x.jsonl".to_string()),
            agent_session_id: Some("ses_x".to_string()),
            resolved_directory: Some(PathBuf::from("/tmp/project")),
        };
        assert_eq!(
            tps.static_hint(&located).as_deref(),
            Some("/tmp/opencode-bridge/ses_x.jsonl"),
        );

        let without = LocatedStatusSource {
            status_path: PathBuf::from("/tmp/x"),
            trust_root: PathBuf::from("/tmp"),
            static_transcript_hint: None,
            agent_session_id: None,
            resolved_directory: None,
        };
        assert_eq!(tps.static_hint(&without), None);
    }

    #[test]
    fn dynamic_hint_is_none_regardless_of_raw() {
        let adapter = adapter();
        let tps: &dyn TranscriptPathSource = &adapter;
        assert_eq!(tps.dynamic_hint(r#"{"transcript_path":"/ignored"}"#), None);
    }

    #[test]
    fn decode_parses_bridge_into_snapshot() {
        let adapter = adapter();
        let raw = concat!(
            r#"{"v":1,"ts":1,"kind":"event","type":"session.created","data":{"info":{"id":"ses_d","version":"1.2.3","model":{"id":"claude-sonnet-4"},"cost":0,"tokens":{"input":700,"output":300,"cache":{"read":0,"write":0}}}}}"#,
            "\n",
        );
        let snapshot = <OpenCodeAdapter as StateDecoder>::decode(&adapter, Some("pty"), raw)
            .expect("opencode snapshot decodes");
        assert_eq!(snapshot.agent_session_id, "ses_d");
        assert_eq!(snapshot.model_id, "claude-sonnet-4");
        assert_eq!(snapshot.context_window.total_input_tokens, 700);
    }

    #[test]
    fn validate_transcript_rejects_path_outside_bridge_root() {
        let adapter = adapter();
        assert!(
            <OpenCodeAdapter as AgentAdapter>::validate_transcript(&adapter, "/tmp/not-opencode")
                .is_err(),
            "path outside the bridge root should be rejected",
        );
    }

    #[test]
    fn validate_transcript_accepts_real_jsonl_under_bridge_root() {
        let bridge = tempfile::tempdir().expect("bridge tempdir");
        let session = bridge.path().join("ses_v.jsonl");
        std::fs::write(&session, "").expect("write session file");
        let adapter = OpenCodeAdapter::with_locator(Arc::new(OpenCodeLocator::new(
            bridge.path().to_path_buf(),
            4242,
            SystemTime::UNIX_EPOCH,
        )));

        let validated = <OpenCodeAdapter as AgentAdapter>::validate_transcript(
            &adapter,
            session.to_str().expect("utf8 path"),
        )
        .expect("real jsonl under bridge root validates");
        assert_eq!(
            validated,
            std::fs::canonicalize(&session).expect("canonical")
        );
    }
}
