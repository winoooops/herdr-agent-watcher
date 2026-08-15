//! Kimi Code adapter implementation.

mod locator;
mod parser;
mod transcript;
mod transcript_dto;
mod types;
mod usage;
mod usage_fetch;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::adapter::base::TranscriptHandle;
use crate::agent::adapter::traits::{
    StateDecoder, StatusSourceLocator as _, TranscriptPathValidator, TranscriptStreamer,
};
use crate::agent::adapter::types::{
    stamp_snapshot, LocatedStatusSource, ParsedStatus, RawPath, StatusSnapshot,
    TranscriptPathSource, ValidateTranscriptError,
};
use crate::agent::adapter::AgentAdapter;
use crate::agent::types::AgentType;
use crate::runtime::EventSink;

pub(crate) use self::locator::kdbg;
pub(crate) use self::locator::KimiLocator;
pub(crate) use self::locator::{KIMI_BIND_RETRY_INTERVAL_MS, KIMI_BIND_RETRY_MAX_ATTEMPTS};
pub(crate) use self::types::default_kimi_home;

/// Adapter for the kimi-code CLI. Holds a shared `Arc<KimiLocator>` so
/// `AgentBindings::for_attach` can share one locator between
/// `bindings.locator` and the adapter's decoder/validator/streamer views.
pub struct KimiAdapter {
    locator: Arc<KimiLocator>,
}

impl KimiAdapter {
    pub(crate) fn with_locator(locator: Arc<KimiLocator>) -> Self {
        Self { locator }
    }

    fn locator(&self) -> &KimiLocator {
        &self.locator
    }

    fn recovery_paths(&self, current: &Path) -> Vec<PathBuf> {
        let mut paths = self.locator.recovery_paths();
        if !paths.iter().any(|path| path == current) {
            paths.insert(0, current.to_path_buf());
        }
        paths
    }
}

impl TranscriptPathSource for KimiAdapter {
    /// kimi-code's transcript path is the same `wire.jsonl` the locator
    /// resolves at attach time; surface it via the static hint.
    fn static_hint(&self, located: &LocatedStatusSource) -> Option<RawPath> {
        located.static_transcript_hint.clone()
    }

    // `dynamic_hint` defaults to `None` — kimi never writes the transcript
    // path inside the status stream.
}

impl StateDecoder for KimiAdapter {
    fn decode(&self, session_id: Option<&str>, raw: &str) -> Result<StatusSnapshot, String> {
        // When the locator has resolved a session dir, aggregate across the
        // session's agents (context from the active sub-agent); otherwise
        // decode the single main wire the watcher handed us.
        let session_dir = self.locator.resolved_session_dir();
        let mut snapshot = session_dir
            .as_deref()
            .and_then(parser::parse_session_aggregate)
            .map(Ok)
            .unwrap_or_else(|| parser::parse_wire_snapshot(session_id, raw))?;
        // Merge the last fetched plan-usage (`None` when consent is OFF or
        // nothing has been fetched). The fetch itself is driven by the
        // transcript supervisor's poll, not here, so it also reaches idle
        // sessions the main-wire watcher never re-decodes. `usage_fetched`
        // records whether a real value landed, so the gate tells LOADING from a
        // genuine zero-usage ON without guessing from the values.
        let cached = self.locator.cached_rate_limits();
        snapshot.usage_fetched = cached.is_some();
        if let Some(rate_limits) = cached {
            snapshot.rate_limits = rate_limits;
        }
        kdbg(&format!(
            "DECODE: model={} ctx_size={} input={} output={}",
            snapshot.model_id,
            snapshot.context_window.context_window_size,
            snapshot.context_window.total_input_tokens,
            snapshot.context_window.total_output_tokens
        ));
        Ok(snapshot)
    }
}

impl TranscriptPathValidator for KimiAdapter {
    fn validate(&self, raw: &str) -> Result<PathBuf, ValidateTranscriptError> {
        // Validate against the SAME effective home the locator resolved
        // (per-process `KIMI_CODE_HOME`), not a recomputed default root.
        transcript::validate_transcript_path_with_root(raw, self.locator.validator_root())
    }
}

impl TranscriptStreamer for KimiAdapter {
    fn tail(
        &self,
        events: Arc<dyn EventSink>,
        session_id: String,
        cwd: Option<PathBuf>,
        transcript_path: PathBuf,
    ) -> Result<TranscriptHandle, String> {
        self.locator.remember_recovery_path(transcript_path.clone());
        // Prefer the locator's resolved process cwd over the watcher's stale
        // spawn cwd so the emitted `agent-cwd` points at the real project.
        let cwd = self.locator.resolved_cwd().or(cwd);
        // Hand the supervisor the locator so its status poll merges the fetched
        // plan-usage (and pushes it to idle sessions).
        transcript::start_tailing(
            events,
            session_id,
            transcript_path,
            cwd,
            self.locator.clone(),
        )
    }

    fn recover_replies(
        &self,
        session_id: &str,
        transcript_path: &Path,
        nonces: &std::collections::HashSet<String>,
    ) -> Result<Vec<crate::agent::types::AgentReplyEvent>, String> {
        let paths = self.recovery_paths(transcript_path);
        transcript::recover_replies(&paths, self.locator.validator_root(), session_id, nonces)
    }

    fn recover_reviews(
        &self,
        session_id: &str,
        transcript_path: &Path,
        nonces: &std::collections::HashSet<String>,
    ) -> Result<Vec<crate::agent::types::AgentReviewEvent>, String> {
        let paths = self.recovery_paths(transcript_path);
        transcript::recover_reviews(&paths, self.locator.validator_root(), session_id, nonces)
    }
}

impl AgentAdapter for KimiAdapter {
    fn agent_type(&self) -> AgentType {
        AgentType::Kimi
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
mod adapter_tests {
    use super::*;

    fn adapter() -> KimiAdapter {
        KimiAdapter::with_locator(Arc::new(KimiLocator::new(
            PathBuf::from("/tmp/.kimi-code"),
            0,
            std::time::SystemTime::UNIX_EPOCH,
            None,
        )))
    }

    #[test]
    fn agent_type_is_kimi() {
        assert_eq!(
            <KimiAdapter as AgentAdapter>::agent_type(&adapter()),
            AgentType::Kimi
        );
    }

    #[test]
    fn static_hint_surfaces_located_transcript_hint() {
        let adapter = adapter();
        let tps: &dyn TranscriptPathSource = &adapter;

        let located = LocatedStatusSource {
            status_path: PathBuf::from(
                "/home/u/.kimi-code/sessions/wd/session_1/agents/main/wire.jsonl",
            ),
            trust_root: PathBuf::from("/home/u/.kimi-code"),
            static_transcript_hint: Some(
                "/home/u/.kimi-code/sessions/wd/session_1/agents/main/wire.jsonl".to_string(),
            ),
            agent_session_id: Some("session_1".to_string()),
            resolved_directory: None,
        };
        assert_eq!(
            tps.static_hint(&located).as_deref(),
            Some("/home/u/.kimi-code/sessions/wd/session_1/agents/main/wire.jsonl"),
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
    fn decode_parses_wire_into_snapshot() {
        let adapter = adapter();
        let raw = concat!(
            r#"{"type":"config.update","modelAlias":"kimi-code/kimi-for-coding"}"#,
            "\n",
            r#"{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":211,"output":35,"inputCacheRead":16128,"inputCacheCreation":0}}"#,
            "\n",
        );
        let snapshot = <KimiAdapter as StateDecoder>::decode(&adapter, Some("pty"), raw)
            .expect("kimi snapshot decodes");
        assert_eq!(snapshot.model_id, "kimi-code/kimi-for-coding");
        assert_eq!(snapshot.context_window.total_input_tokens, 211);
    }

    #[test]
    fn validate_transcript_rejects_path_outside_kimi_root() {
        let adapter = adapter();
        assert!(
            <KimiAdapter as AgentAdapter>::validate_transcript(&adapter, "/tmp/not-kimi").is_err(),
            "path outside kimi home should be rejected",
        );
    }

    // Write <proc_root>/<pid>/environ from NUL-joined KEY=VALUE entries.
    fn write_environ(proc_root: &Path, pid: u32, entries: &[&str]) {
        let pid_dir = proc_root.join(pid.to_string());
        std::fs::create_dir_all(&pid_dir).expect("create fake pid dir");
        let mut bytes = Vec::new();
        for entry in entries {
            bytes.extend_from_slice(entry.as_bytes());
            bytes.push(0);
        }
        std::fs::write(pid_dir.join("environ"), bytes).expect("write environ");
    }

    /// Fix B: the validator resolves its trust root from the locator's
    /// per-process effective home (proc-environ `KIMI_CODE_HOME`), so a
    /// wire under that home validates even when the sidecar's own
    /// `KIMI_CODE_HOME` (and constructor home) point elsewhere; a path
    /// outside the effective home is still rejected.
    #[test]
    fn validate_transcript_uses_locator_effective_home() {
        let env_home = tempfile::tempdir().expect("env home");
        let wrong_home = tempfile::tempdir().expect("constructor home");
        let proc_root = tempfile::tempdir().expect("proc root");

        let wire = env_home
            .path()
            .join("sessions")
            .join("wd_x")
            .join("session_1")
            .join("agents")
            .join("main")
            .join("wire.jsonl");
        std::fs::create_dir_all(wire.parent().expect("parent")).expect("mkdir wire");
        std::fs::write(&wire, "").expect("write wire");

        let pid = 9191;
        write_environ(
            proc_root.path(),
            pid,
            &[&format!("KIMI_CODE_HOME={}", env_home.path().display())],
        );

        // Constructor + sidecar env both point at the WRONG home; only the
        // proc-environ home contains the wire.
        let _guard = crate::agent::adapter::KimiHomeEnvGuard::acquire();
        std::env::set_var("KIMI_CODE_HOME", wrong_home.path());
        let adapter = KimiAdapter::with_locator(Arc::new(KimiLocator::new(
            wrong_home.path().to_path_buf(),
            pid,
            std::time::SystemTime::UNIX_EPOCH,
            Some(proc_root.path().to_path_buf()),
        )));
        std::fs::remove_file(proc_root.path().join(pid.to_string()).join("environ"))
            .expect("remove process environ after attach");

        let validated = <KimiAdapter as AgentAdapter>::validate_transcript(
            &adapter,
            wire.to_str().expect("utf8 wire"),
        )
        .expect("wire under per-process effective home validates");
        assert_eq!(
            validated,
            std::fs::canonicalize(&wire).expect("canonical wire")
        );

        // A path outside the effective home is still rejected.
        let outside = wrong_home.path().join("wire.jsonl");
        std::fs::write(&outside, "").expect("write outside");
        assert!(
            matches!(
                <KimiAdapter as TranscriptPathValidator>::validate(
                    &adapter,
                    outside.to_str().expect("utf8 outside"),
                ),
                Err(ValidateTranscriptError::OutsideRoot { .. })
            ),
            "path outside the effective home must be rejected",
        );
    }
}
