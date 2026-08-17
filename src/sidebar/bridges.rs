//! Session-directory inventory and manual pruning for the sidebar.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub pane_id: String,
    pub workspace_id: String,
    pub quiet_for: Option<Duration>,
    pub bytes: u64,
    pub live: bool,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub entries: Vec<Entry>,
    pub total_bytes: u64,
    pub stale_count: usize,
    pub removed: Option<(usize, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Reading,
    Pruning,
    Ready(Snapshot),
    Failed(String),
}

pub fn state_root() -> Result<PathBuf, String> {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "HERDR_PLUGIN_STATE_DIR is not set".to_string())
}

fn snapshot(
    root: &std::path::Path,
    retention: Option<Duration>,
    live_panes: &HashSet<String>,
    now: SystemTime,
    removed: Option<(usize, u64)>,
) -> Snapshot {
    let stale: HashSet<PathBuf> = retention
        .map(|retention| crate::daemon::prune::stale(root, retention, now))
        .unwrap_or_default()
        .into_iter()
        .collect();
    let entries: Vec<Entry> = crate::daemon::prune::inventory(root, now)
        .into_iter()
        .map(|session| Entry {
            live: live_panes.contains(&session.pane_id),
            stale: stale.contains(&session.path),
            pane_id: crate::sidebar::format::sanitise(&session.pane_id),
            workspace_id: crate::sidebar::format::sanitise(&session.workspace_id),
            quiet_for: session.quiet_for,
            bytes: session.bytes,
        })
        .collect();
    Snapshot {
        total_bytes: entries.iter().map(|entry| entry.bytes).sum(),
        stale_count: stale.len(),
        entries,
        removed,
    }
}

pub fn read(
    root: Result<PathBuf, String>,
    retention: Option<Duration>,
    live_panes: HashSet<String>,
) -> Receiver<View> {
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        let view = match root {
            Ok(root) => View::Ready(snapshot(
                &root,
                retention,
                &live_panes,
                SystemTime::now(),
                None,
            )),
            Err(error) => View::Failed(error),
        };
        let _ = sender.send(view);
    });
    receiver
}

pub fn prune(
    root: Result<PathBuf, String>,
    retention: Duration,
    live_panes: HashSet<String>,
) -> Receiver<View> {
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        let view = match root {
            Ok(root) => {
                let now = SystemTime::now();
                let removed = crate::daemon::prune::sweep(&root, retention, now);
                View::Ready(snapshot(
                    &root,
                    Some(retention),
                    &live_panes,
                    SystemTime::now(),
                    Some(removed),
                ))
            }
            Err(error) => View::Failed(error),
        };
        let _ = sender.send(view);
    });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(root: &std::path::Path, workspace: &str, pane: &str, bytes: usize) {
        let dir = root
            .join("runtime")
            .join("workspaces")
            .join(workspace)
            .join("sessions")
            .join(pane);
        std::fs::create_dir_all(&dir).expect("session dir");
        std::fs::write(dir.join("status.json"), "x".repeat(bytes)).expect("status");
    }

    #[test]
    fn the_summary_counts_exactly_what_stale_would_take() {
        let root = tempfile::tempdir().expect("tempdir");
        session(root.path(), "w1", "old", 10);
        std::thread::sleep(Duration::from_millis(60));
        session(root.path(), "w1", "new", 20);
        let now = SystemTime::now();
        let retention = Duration::from_millis(30);

        let found = snapshot(root.path(), Some(retention), &HashSet::new(), now, None);
        assert_eq!(
            found.stale_count,
            crate::daemon::prune::stale(root.path(), retention, now).len()
        );
        assert_eq!(found.total_bytes, 30);
    }

    #[test]
    fn a_pane_in_the_snapshot_is_live_and_an_absent_one_is_dead() {
        let root = tempfile::tempdir().expect("tempdir");
        session(root.path(), "w1", "live-pane", 1);
        session(root.path(), "w2", "dead-pane", 1);
        let found = snapshot(
            root.path(),
            None,
            &HashSet::from(["live-pane".to_string()]),
            SystemTime::now(),
            None,
        );
        assert!(found.entries.iter().any(|entry| entry.live));
        assert!(found.entries.iter().any(|entry| !entry.live));
    }
}
