//! Reaping session directories nothing has written to in a while.
//!
//! A month of ordinary use left 402 of them behind in the project this was
//! ported from -- 63 MB, every one of them long finished. Sessions are active
//! for hours and then silent forever, so age is the whole of the signal.
//!
//! Deleting one is cheap to be wrong about: the directory holds
//! `attention.jsonl` and `status.json` and nothing else, the scripts live once
//! at the state root, and `append_attention` creates the parent before writing.
//! A directory removed under a session that is still going comes back on its
//! next write, minus the history nobody was reading.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long after the last write a session directory may be removed.
///
/// The ladder the settings panel offers. `0` disables the sweep, which is why
/// it is not one of the rungs -- turning it off is a separate decision from
/// choosing how long to keep.
pub const RETENTIONS_DAYS: [u32; 3] = [7, 14, 30];
pub const DEFAULT_RETENTION_DAYS: u32 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionDir {
    pub path: PathBuf,
    pub workspace_id: String,
    pub pane_id: String,
    pub quiet_for: Option<Duration>,
    pub bytes: u64,
}

/// The newest write anywhere in a session directory.
///
/// Not the directory's own mtime: appending to a file leaves it untouched, and
/// `attention.jsonl` is appended in place. A session that only ever fires
/// attention hooks would look untouched since the day it was created and be
/// swept while it was still running.
fn last_write(dir: &Path) -> Option<SystemTime> {
    let mut newest = std::fs::metadata(dir).ok()?.modified().ok();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return newest;
    };
    for entry in entries.flatten() {
        if let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) {
            if newest.is_none_or(|current| modified > current) {
                newest = Some(modified);
            }
        }
    }
    newest
}

/// The session directories and the facts the sweeper and sidebar share.
pub(crate) fn inventory(root: &Path, now: SystemTime) -> Vec<SessionDir> {
    let mut found = Vec::new();
    let Ok(workspaces) = std::fs::read_dir(root.join("runtime").join("workspaces")) else {
        return found;
    };
    for workspace in workspaces.flatten() {
        let workspace_id = workspace.file_name().to_string_lossy().into_owned();
        let Ok(sessions) = std::fs::read_dir(workspace.path().join("sessions")) else {
            continue;
        };
        for session in sessions.flatten() {
            let path = session.path();
            if !path.is_dir() {
                continue;
            }
            found.push(SessionDir {
                pane_id: session.file_name().to_string_lossy().into_owned(),
                workspace_id: workspace_id.clone(),
                quiet_for: last_write(&path).and_then(|last| now.duration_since(last).ok()),
                bytes: dir_size(&path),
                path,
            });
        }
    }
    found.sort_by(|a, b| (&a.workspace_id, &a.pane_id).cmp(&(&b.workspace_id, &b.pane_id)));
    found
}

/// The session directories under `root` that have gone quiet for longer than
/// `retention`.
///
/// Returns them rather than deleting them so the decision is testable without
/// a filesystem to destroy, and so the caller decides what a failed delete
/// means.
pub fn stale(root: &Path, retention: Duration, now: SystemTime) -> Vec<PathBuf> {
    inventory(root, now)
        .into_iter()
        // A clock that went backwards leaves the directory looking like it
        // was written in the future. Keeping it is the harmless answer.
        .filter(|session| session.quiet_for.is_some_and(|quiet| quiet > retention))
        .map(|session| session.path)
        .collect()
}

/// Remove what `stale` found, and say how much went.
///
/// A directory that will not delete is left alone and counted out: the sweep
/// is housekeeping, and housekeeping that fails should not stop a daemon.
pub fn sweep(root: &Path, retention: Duration, now: SystemTime) -> (usize, u64) {
    let mut removed = 0;
    let mut bytes = 0;
    for dir in stale(root, retention, now) {
        let size = dir_size(&dir);
        if std::fs::remove_dir_all(&dir).is_ok() {
            removed += 1;
            bytes += size;
        }
    }
    (removed, bytes)
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(root: &Path, workspace: &str, pane: &str) -> PathBuf {
        let dir = root
            .join("runtime")
            .join("workspaces")
            .join(workspace)
            .join("sessions")
            .join(pane);
        std::fs::create_dir_all(&dir).expect("create the session dir");
        dir
    }

    /// Ages are made with real time rather than by rewriting mtimes: the
    /// retention is a `Duration`, so a test can use a tenth of a second where
    /// production uses a week, and the same comparison runs either way.
    const GAP: Duration = Duration::from_millis(60);
    const RETENTION: Duration = Duration::from_millis(30);

    fn pause() {
        std::thread::sleep(GAP);
    }

    #[test]
    fn a_session_quiet_for_longer_than_the_retention_is_stale() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = session(tmp.path(), "w1", "p1");
        std::fs::write(old.join("status.json"), "{}").unwrap();
        pause();
        let recent = session(tmp.path(), "w1", "p2");
        std::fs::write(recent.join("status.json"), "{}").unwrap();

        let found = stale(tmp.path(), RETENTION, SystemTime::now());
        assert_eq!(found, vec![old], "only the quiet one");
        assert!(!found.contains(&recent));
    }

    /// The bug this function exists to avoid. Appending to a file does not
    /// touch its directory's mtime, and `attention.jsonl` is appended in
    /// place -- so a session that only fires attention hooks keeps a
    /// directory that looks untouched since the day it was made.
    #[test]
    fn a_fresh_file_saves_a_directory_whose_own_mtime_is_old() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = session(tmp.path(), "w1", "p1");
        pause();
        std::fs::write(dir.join("attention.jsonl"), "{}").unwrap();

        assert!(
            stale(tmp.path(), RETENTION, SystemTime::now()).is_empty(),
            "the file inside was written just now"
        );
    }

    #[test]
    fn an_empty_directory_is_judged_on_its_own_mtime() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = session(tmp.path(), "w1", "p1");
        pause();
        assert_eq!(stale(tmp.path(), RETENTION, SystemTime::now()), vec![dir]);
    }

    /// A directory dated after `now` -- a clock that moved backwards, a tree
    /// copied from another machine -- has no age to compare. Keeping it is
    /// the answer that cannot destroy anything.
    #[test]
    fn a_directory_from_the_future_is_left_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        session(tmp.path(), "w1", "p1");
        assert!(
            stale(tmp.path(), RETENTION, SystemTime::UNIX_EPOCH).is_empty(),
            "everything is in the future when now is 1970"
        );
    }

    #[test]
    fn a_state_directory_with_nothing_in_it_sweeps_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(stale(tmp.path(), RETENTION, SystemTime::now()).is_empty());
        assert_eq!(sweep(tmp.path(), RETENTION, SystemTime::now()), (0, 0));
    }

    #[test]
    fn sweeping_removes_the_stale_and_reports_what_it_freed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = session(tmp.path(), "w1", "p1");
        std::fs::write(old.join("attention.jsonl"), "x".repeat(100)).unwrap();
        pause();
        let keep = session(tmp.path(), "w2", "p1");
        std::fs::write(keep.join("attention.jsonl"), "y").unwrap();

        let (removed, bytes) = sweep(tmp.path(), RETENTION, SystemTime::now());
        assert_eq!(removed, 1);
        assert_eq!(bytes, 100);
        assert!(!old.exists(), "gone");
        assert!(keep.exists(), "kept");
        // The workspace directory stays: the next session under it would only
        // have to make it again.
        assert!(old.parent().expect("sessions dir").exists());
    }
}
