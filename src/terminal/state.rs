use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use super::types::SessionId;

static GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct PaneRuntime {
    pid: Option<u32>,
    cwd: Option<String>,
    started_at: SystemTime,
    generation: u64,
}

#[cfg(test)]
pub struct RingBuffer {
    capacity: usize,
}

#[cfg(test)]
impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self { capacity }
    }
}

#[cfg(test)]
pub struct ManagedSession {
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub cwd: String,
    pub bridge_dir: Option<String>,
    pub shim_dir: Option<String>,
    pub generation: u64,
    pub ring: Arc<Mutex<RingBuffer>>,
    pub cancelled: Arc<AtomicBool>,
    pub started_at: SystemTime,
}

#[cfg(test)]
#[derive(Debug)]
pub enum TryInsertError {
    AlreadyExists,
    CapReached,
}

#[derive(Clone, Default)]
pub struct PtyState {
    panes: Arc<Mutex<HashMap<SessionId, PaneRuntime>>>,
    #[cfg(test)]
    test_sessions: Arc<Mutex<HashMap<SessionId, ManagedSession>>>,
}

impl PtyState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert_pane(&self, pane_id: &str, pid: Option<u32>, cwd: Option<String>) {
        let mut panes = self.panes.lock().expect("pane registry poisoned");
        match panes.get_mut(pane_id) {
            Some(pane) => {
                if pid.is_some() {
                    pane.pid = pid;
                }
                if cwd.is_some() {
                    pane.cwd = cwd;
                }
            }
            None => {
                panes.insert(
                    pane_id.to_string(),
                    PaneRuntime {
                        pid,
                        cwd,
                        started_at: SystemTime::now(),
                        generation: GENERATION.fetch_add(1, Ordering::Relaxed) + 1,
                    },
                );
            }
        }
    }

    pub fn remove_pane(&self, pane_id: &str) {
        self.panes
            .lock()
            .expect("pane registry poisoned")
            .remove(pane_id);
    }

    pub fn get_pid(&self, id: &SessionId) -> Option<u32> {
        if let Some(pid) = self
            .panes
            .lock()
            .expect("pane registry poisoned")
            .get(id)
            .and_then(|pane| pane.pid)
        {
            return Some(pid);
        }
        #[cfg(test)]
        return self
            .test_sessions
            .lock()
            .expect("test session registry poisoned")
            .get(id)
            .and_then(|session| session.child.process_id());
        #[cfg(not(test))]
        None
    }

    pub fn get_cwd(&self, id: &SessionId) -> Option<String> {
        if let Some(cwd) = self
            .panes
            .lock()
            .expect("pane registry poisoned")
            .get(id)
            .and_then(|pane| pane.cwd.clone())
        {
            return Some(cwd);
        }
        #[cfg(test)]
        return self
            .test_sessions
            .lock()
            .expect("test session registry poisoned")
            .get(id)
            .map(|session| session.cwd.clone());
        #[cfg(not(test))]
        None
    }

    pub fn get_started_at(&self, id: &SessionId) -> Option<SystemTime> {
        if let Some(started_at) = self
            .panes
            .lock()
            .expect("pane registry poisoned")
            .get(id)
            .map(|pane| pane.started_at)
        {
            return Some(started_at);
        }
        #[cfg(test)]
        return self
            .test_sessions
            .lock()
            .expect("test session registry poisoned")
            .get(id)
            .map(|session| session.started_at);
        #[cfg(not(test))]
        None
    }

    pub fn generation(&self, id: &SessionId) -> Option<u64> {
        if let Some(generation) = self
            .panes
            .lock()
            .expect("pane registry poisoned")
            .get(id)
            .map(|pane| pane.generation)
        {
            return Some(generation);
        }
        #[cfg(test)]
        return self
            .test_sessions
            .lock()
            .expect("test session registry poisoned")
            .get(id)
            .map(|session| session.generation);
        #[cfg(not(test))]
        None
    }

    #[cfg(test)]
    pub fn try_insert(
        &self,
        session_id: SessionId,
        session: ManagedSession,
        max: usize,
    ) -> Result<(), (TryInsertError, ManagedSession)> {
        let mut sessions = self
            .test_sessions
            .lock()
            .expect("test session registry poisoned");
        if sessions.contains_key(&session_id) {
            return Err((TryInsertError::AlreadyExists, session));
        }
        if sessions.len() >= max {
            return Err((TryInsertError::CapReached, session));
        }
        sessions.insert(session_id, session);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_then_read_back() {
        let state = PtyState::new();
        state.upsert_pane("p1", Some(4242), Some("/tmp/wt".into()));
        assert_eq!(state.get_pid(&"p1".to_string()), Some(4242));
        assert_eq!(
            state.get_cwd(&"p1".to_string()),
            Some("/tmp/wt".to_string())
        );
        assert!(state.get_started_at(&"p1".to_string()).is_some());
        let first = state
            .generation(&"p1".to_string())
            .expect("known pane has a generation");
        state.remove_pane("p1");
        assert_eq!(state.get_pid(&"p1".to_string()), None);
        assert_eq!(state.generation(&"p1".to_string()), None);
        state.upsert_pane("p1", Some(4243), None);
        assert!(state.generation(&"p1".to_string()).unwrap() > first);
    }
}
