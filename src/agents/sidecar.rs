use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::adapter;
use crate::agent::notification::{NotificationProvider, NotificationWatcherService};
use crate::agents::{AgentAdapter, BoundPane};
use crate::runtime::EventSink;
use crate::terminal::PtyState;

pub struct SidecarAdapter {
    pty_state: PtyState,
    watcher_state: adapter::AgentWatcherState,
    transcript_state: adapter::base::TranscriptState,
    notifications: NotificationWatcherService,
    events: Arc<dyn EventSink>,
    runtime: tokio::runtime::Handle,
    app_data_dir: std::path::PathBuf,
}

impl SidecarAdapter {
    pub fn new(
        pty_state: PtyState,
        events: Arc<dyn EventSink>,
        runtime: tokio::runtime::Handle,
        app_data_dir: std::path::PathBuf,
    ) -> Self {
        let watcher_state = adapter::AgentWatcherState::default();
        let transcript_state = adapter::base::TranscriptState::default();
        let notifications = NotificationWatcherService::new(
            pty_state.clone(),
            watcher_state.clone(),
            transcript_state.clone(),
            events.clone(),
        );
        Self {
            pty_state,
            watcher_state,
            transcript_state,
            notifications,
            events,
            runtime,
            app_data_dir,
        }
    }

    fn seed_claude_status(&self, pane: &BoundPane) -> Result<(), String> {
        if !matches!(pane.agent_id.as_str(), "claude" | "claude-code") {
            return Ok(());
        }
        let cwd = self
            .pty_state
            .get_cwd(&pane.pane_id)
            .map(PathBuf::from)
            .ok_or_else(|| format!("Claude pane {} has no cwd", pane.pane_id))?;
        let status_path = crate::agent::adapter::claude_code::bridge::session_status_file(
            &self.app_data_dir,
            &cwd,
            &pane.pane_id,
        );
        if status_has_session(&status_path, &pane.agent_session) {
            return Ok(());
        }
        let claude_root = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
            .ok_or_else(|| "Claude config directory is unavailable".to_string())?;
        let Some(transcript) = find_file(
            &claude_root.join("projects"),
            &format!("{}.jsonl", pane.agent_session),
        ) else {
            log::debug!(
                "[sidecar-adapter] no Claude transcript for declared session {}",
                pane.agent_session
            );
            return Ok(());
        };
        std::fs::create_dir_all(status_path.parent().expect("status path has parent"))
            .map_err(|error| format!("create Claude status directory: {error}"))?;
        std::fs::write(
            &status_path,
            serde_json::to_vec(&serde_json::json!({
                "session_id": pane.agent_session,
                "transcript_path": transcript,
                "model": { "id": "unknown", "display_name": "Claude Code" },
            }))
            .expect("serialize Claude status seed"),
        )
        .map_err(|error| format!("write Claude status seed: {error}"))
    }

    fn seed_opencode_index(&self, pane: &BoundPane) -> Result<(), String> {
        if pane.agent_id != "opencode" {
            return Ok(());
        }
        let pid = self
            .pty_state
            .get_pid(&pane.pane_id)
            .ok_or_else(|| format!("OpenCode pane {} has no pid", pane.pane_id))?;
        let cwd = self
            .pty_state
            .get_cwd(&pane.pane_id)
            .map(PathBuf::from)
            .ok_or_else(|| format!("OpenCode pane {} has no cwd", pane.pane_id))?;
        seed_opencode_index_at(
            &crate::agent::adapter::opencode::install::bridge_dir(),
            pane,
            pid,
            &cwd,
        )
    }
}

impl AgentAdapter for SidecarAdapter {
    fn ids(&self) -> &[&str] {
        crate::sidebar::agent_ids::ACCEPTED_IDS
    }

    fn bind(&self, pane: BoundPane) -> Result<(), String> {
        log::info!(
            "[sidecar-adapter] binding {}/{} as {}",
            pane.pane_id.clone(),
            pane.agent_session,
            pane.agent_id
        );
        self.seed_claude_status(&pane)?;
        self.seed_opencode_index(&pane)?;
        self.runtime.block_on(adapter::start_agent_watcher_inner(
            self.pty_state.clone(),
            self.watcher_state.clone(),
            self.transcript_state.clone(),
            self.events.clone(),
            self.app_data_dir.clone(),
            pane.pane_id.clone(),
            None,
        ))?;
        let agent_type = self
            .watcher_state
            .agent_type_for_pty(&pane.pane_id)
            .ok_or_else(|| format!("agent watcher missing after start: {}", pane.pane_id))?;
        if let (Some(provider), Some(mut source)) = (
            NotificationProvider::from_agent_type(agent_type),
            self.watcher_state.current_status_path(&pane.pane_id),
        ) {
            if provider == NotificationProvider::ClaudeCode {
                source =
                    crate::agent::adapter::claude_code::bridge::session_attention_file(&source);
            }
            if let Err(error) = self
                .notifications
                .register(pane.pane_id.clone(), provider, source)
            {
                log::warn!(
                    "notification watcher registration failed for {} ({provider:?}): {error}",
                    pane.pane_id,
                );
            }
        }
        Ok(())
    }

    fn unbind(&self, pane_id: &str) -> Result<(), String> {
        self.notifications.stop(pane_id);
        match self.runtime.block_on(adapter::stop_agent_watcher_inner(
            self.pty_state.clone(),
            self.watcher_state.clone(),
            self.transcript_state.clone(),
            self.events.clone(),
            pane_id.to_string(),
        )) {
            Err(error) if error.starts_with("No active watcher") => Ok(()),
            result => result,
        }
    }
}

fn seed_opencode_index_at(
    bridge_dir: &Path,
    pane: &BoundPane,
    pid: u32,
    cwd: &Path,
) -> Result<(), String> {
    if pane.agent_session.is_empty()
        || !pane
            .agent_session
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        || !bridge_dir
            .join(format!("{}.jsonl", pane.agent_session))
            .is_file()
    {
        return Ok(());
    }
    let time = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_err(|error| format!("read clock for OpenCode index seed: {error}"))?
        .as_millis();
    let row = serde_json::json!({
        "sessionID": pane.agent_session,
        "pid": pid,
        "directory": cwd,
        "slug": "agent-watcher",
        "time": time,
    });
    let mut index = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(bridge_dir.join("index.jsonl"))
        .map_err(|error| format!("open OpenCode bridge index: {error}"))?;
    writeln!(index, "{row}").map_err(|error| format!("seed OpenCode bridge index: {error}"))
}

fn status_has_session(path: &Path, session: &str) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value["session_id"].as_str().map(str::to_owned))
        .is_some_and(|value| value == session)
}

fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let file_type = entry.file_type().ok()?;
        if file_type.is_file() && entry.file_name() == name {
            return Some(entry.path());
        }
        if file_type.is_dir() {
            if let Some(path) = find_file(&entry.path(), name) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_exact_claude_transcript_without_following_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("project");
        std::fs::create_dir_all(&nested).unwrap();
        let transcript = nested.join("session-1.jsonl");
        std::fs::write(&transcript, "").unwrap();

        assert_eq!(find_file(tmp.path(), "session-1.jsonl"), Some(transcript));
        assert_eq!(find_file(tmp.path(), "session-2.jsonl"), None);
    }

    #[test]
    fn seeds_existing_opencode_session_for_daemon_restart() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("demo-session.jsonl"), "{}").unwrap();
        let pane = BoundPane {
            pane_id: "demo:opencode".into(),
            agent_id: "opencode".into(),
            agent_session: "demo-session".into(),
        };
        seed_opencode_index_at(tmp.path(), &pane, 4242, Path::new("/workspace/demo")).unwrap();
        let row: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(tmp.path().join("index.jsonl"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(row["sessionID"], "demo-session");
        assert_eq!(row["pid"], 4242);
    }
}
