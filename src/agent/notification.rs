use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::adapter::base::{for_each_bounded_line, TranscriptState};
use crate::agent::types::{AgentNotificationEvent, AgentNotificationReason, AgentType};
use crate::agent::AgentWatcherState;
use crate::runtime::{serialize_event, EventSink};
use crate::terminal::state::PtyState;

const EVENT_QUEUE_CAPACITY: usize = 1024;
const MAX_PARTIAL_LINE_BYTES: usize = 256 * 1024;
const READ_BUFFER_BYTES: usize = 64 * 1024;
const RECENT_DEDUPE_KEYS: usize = 128;
const RECONCILE_INTERVAL: Duration = Duration::from_secs(3);
const MAX_NOTIFICATION_BODY_CHARS: usize = 80;
/// First-lookup scan bound for Claude rollout transcripts, which can run
/// 20-114MB; the completion body only ever lives near the tail.
const CLAUDE_TRANSCRIPT_INITIAL_TAIL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum NotificationProvider {
    ClaudeCode,
    Codex,
    Kimi,
    OpenCode,
}

impl NotificationProvider {
    pub(crate) fn from_agent_type(agent_type: AgentType) -> Option<Self> {
        match agent_type {
            AgentType::ClaudeCode => Some(Self::ClaudeCode),
            AgentType::Codex => Some(Self::Codex),
            AgentType::Kimi => Some(Self::Kimi),
            AgentType::Opencode => Some(Self::OpenCode),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NotificationDiagnostics {
    pub worker_alive: bool,
    pub active_registrations: usize,
    pub active_by_provider: HashMap<NotificationProvider, usize>,
    pub started_total: u64,
    pub stopped_total: u64,
    pub rebound_total: u64,
    pub bytes_scanned: u64,
    pub records_scanned: u64,
    pub notifications_emitted: HashMap<AgentNotificationReason, u64>,
    pub parse_failures: u64,
    pub replay_suppressions: u64,
    pub duplicate_suppressions: u64,
    pub queue_drops: u64,
    pub last_successful_event_at: Option<u64>,
    pub process_rss_bytes: Option<u64>,
    pub event_queue_capacity: usize,
    pub max_partial_line_bytes: usize,
}

#[derive(Default)]
struct MutableDiagnostics {
    worker_alive: bool,
    active_registrations: usize,
    active_by_provider: HashMap<NotificationProvider, usize>,
    started_total: u64,
    stopped_total: u64,
    rebound_total: u64,
    bytes_scanned: u64,
    records_scanned: u64,
    notifications_emitted: HashMap<AgentNotificationReason, u64>,
    parse_failures: u64,
    replay_suppressions: u64,
    duplicate_suppressions: u64,
    queue_drops: u64,
    last_successful_event_at: Option<u64>,
}

impl MutableDiagnostics {
    fn snapshot(&self) -> NotificationDiagnostics {
        NotificationDiagnostics {
            worker_alive: self.worker_alive,
            active_registrations: self.active_registrations,
            active_by_provider: self.active_by_provider.clone(),
            started_total: self.started_total,
            stopped_total: self.stopped_total,
            rebound_total: self.rebound_total,
            bytes_scanned: self.bytes_scanned,
            records_scanned: self.records_scanned,
            notifications_emitted: self.notifications_emitted.clone(),
            parse_failures: self.parse_failures,
            replay_suppressions: self.replay_suppressions,
            duplicate_suppressions: self.duplicate_suppressions,
            queue_drops: self.queue_drops,
            last_successful_event_at: self.last_successful_event_at,
            process_rss_bytes: process_rss_bytes(),
            event_queue_capacity: EVENT_QUEUE_CAPACITY,
            max_partial_line_bytes: MAX_PARTIAL_LINE_BYTES,
        }
    }
}

#[derive(Clone)]
struct Registration {
    pty_id: String,
    pty_generation: u64,
    provider: NotificationProvider,
    source_path: PathBuf,
    cursor: u64,
    partial_line: Vec<u8>,
    dropping_oversize_line: bool,
    agent_session_id: Option<String>,
    turn_active: bool,
    turn_id: Option<String>,
    notification_body: Option<String>,
    claude_transcript_cursors: HashMap<PathBuf, u64>,
    pending_opencode_idle: Option<PendingOpenCodeIdle>,
    dedupe_keys: HashSet<String>,
    dedupe_order: VecDeque<String>,
}

#[derive(Clone)]
struct PendingOpenCodeIdle {
    occurred_at: u64,
    agent_session_id: Option<String>,
}

impl Registration {
    fn new(
        pty_id: String,
        pty_generation: u64,
        provider: NotificationProvider,
        source_path: PathBuf,
        cursor: u64,
    ) -> Self {
        Self {
            pty_id,
            pty_generation,
            provider,
            source_path,
            cursor,
            partial_line: Vec::new(),
            dropping_oversize_line: false,
            agent_session_id: None,
            turn_active: false,
            turn_id: None,
            notification_body: None,
            claude_transcript_cursors: HashMap::new(),
            pending_opencode_idle: None,
            dedupe_keys: HashSet::new(),
            dedupe_order: VecDeque::new(),
        }
    }

    fn remember_dedupe_key(&mut self, key: &str) -> bool {
        if self.dedupe_keys.contains(key) {
            return false;
        }

        if self.dedupe_order.len() == RECENT_DEDUPE_KEYS {
            if let Some(oldest) = self.dedupe_order.pop_front() {
                self.dedupe_keys.remove(&oldest);
            }
        }
        self.dedupe_keys.insert(key.to_string());
        self.dedupe_order.push_back(key.to_string());
        true
    }
}

enum WorkerCommand {
    Register {
        registration: Registration,
        acknowledge: SyncSender<Result<(), String>>,
    },
    Stop {
        pty_id: String,
        acknowledge: SyncSender<bool>,
    },
    SourceChanged(Vec<PathBuf>),
    #[cfg(feature = "e2e-test")]
    Reconcile(SyncSender<()>),
    Shutdown,
}

struct WorkerHandle {
    sender: SyncSender<WorkerCommand>,
    alive: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

#[derive(Clone)]
pub(crate) struct NotificationWatcherService {
    worker: Arc<Mutex<Option<WorkerHandle>>>,
    diagnostics: Arc<Mutex<MutableDiagnostics>>,
    pty_state: PtyState,
    full_watchers: AgentWatcherState,
    transcripts: TranscriptState,
    events: Arc<dyn EventSink>,
}

impl NotificationWatcherService {
    pub(crate) fn new(
        pty_state: PtyState,
        full_watchers: AgentWatcherState,
        transcripts: TranscriptState,
        events: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            worker: Arc::new(Mutex::new(None)),
            diagnostics: Arc::new(Mutex::new(MutableDiagnostics::default())),
            pty_state,
            full_watchers,
            transcripts,
            events,
        }
    }

    pub(crate) fn register(
        &self,
        pty_id: String,
        provider: NotificationProvider,
        source_path: PathBuf,
    ) -> Result<(), String> {
        let pty_generation = self
            .pty_state
            .generation(&pty_id)
            .ok_or_else(|| format!("session not found: {pty_id}"))?;

        let source_path = std::fs::canonicalize(&source_path)
            .map_err(|error| format!("canonicalize notification source: {error}"))?;
        let metadata = std::fs::metadata(&source_path)
            .map_err(|error| format!("read notification source metadata: {error}"))?;
        if !metadata.is_file() {
            return Err("notification source is not a regular file".to_string());
        }

        let registration = Registration::new(
            pty_id,
            pty_generation,
            provider,
            source_path,
            metadata.len(),
        );
        for _ in 0..2 {
            let sender = self.worker_sender()?;
            let (acknowledge, result) = mpsc::sync_channel(1);
            if sender
                .send(WorkerCommand::Register {
                    registration: registration.clone(),
                    acknowledge,
                })
                .is_err()
            {
                continue;
            }
            if let Ok(result) = result.recv() {
                return result;
            }
        }

        Err("notification worker stopped during register".to_string())
    }

    pub(crate) fn stop(&self, pty_id: &str) {
        let slot = self
            .worker
            .lock()
            .expect("notification worker mutex poisoned");
        let mut stop_transcript = slot.is_none();
        if let Some(worker) = slot.as_ref() {
            let (acknowledge, stopped) = mpsc::sync_channel(1);
            if worker
                .sender
                .send(WorkerCommand::Stop {
                    pty_id: pty_id.to_string(),
                    acknowledge,
                })
                .is_ok()
            {
                stop_transcript = stopped.recv().unwrap_or(true);
            } else {
                stop_transcript = true;
            }
        }
        drop(slot);
        if stop_transcript {
            let _ = self.transcripts.stop(pty_id);
        }
    }

    #[cfg(feature = "e2e-test")]
    pub(crate) fn reconcile(&self) {
        let Some(sender) = self.current_sender() else {
            return;
        };
        let (acknowledge, reconciled) = mpsc::sync_channel(1);
        if sender.send(WorkerCommand::Reconcile(acknowledge)).is_ok() {
            let _ = reconciled.recv();
        }
    }

    pub(crate) fn diagnostics(&self) -> NotificationDiagnostics {
        self.diagnostics
            .lock()
            .expect("notification diagnostics mutex poisoned")
            .snapshot()
    }

    #[cfg(feature = "e2e-test")]
    fn current_sender(&self) -> Option<SyncSender<WorkerCommand>> {
        self.worker
            .lock()
            .expect("notification worker mutex poisoned")
            .as_ref()
            .filter(|worker| worker.alive.load(Ordering::Acquire))
            .map(|worker| worker.sender.clone())
    }

    fn worker_sender(&self) -> Result<SyncSender<WorkerCommand>, String> {
        let mut slot = self
            .worker
            .lock()
            .map_err(|_| "notification worker mutex poisoned".to_string())?;

        if slot
            .as_ref()
            .is_some_and(|worker| !worker.alive.load(Ordering::Acquire))
        {
            if let Some(stale) = slot.take() {
                let _ = stale.join.join();
            }
        }

        if slot.is_none() {
            *slot = Some(start_worker(
                self.diagnostics.clone(),
                self.pty_state.clone(),
                self.full_watchers.clone(),
                self.transcripts.clone(),
                self.events.clone(),
            )?);
        }

        Ok(slot
            .as_ref()
            .expect("worker just initialized")
            .sender
            .clone())
    }
}

impl Drop for NotificationWatcherService {
    fn drop(&mut self) {
        if Arc::strong_count(&self.worker) != 1 {
            return;
        }
        let worker = self
            .worker
            .lock()
            .expect("notification worker mutex poisoned")
            .take();
        if let Some(worker) = worker {
            let _ = worker.sender.send(WorkerCommand::Shutdown);
            let _ = worker.join.join();
        }
    }
}

fn start_worker(
    diagnostics: Arc<Mutex<MutableDiagnostics>>,
    pty_state: PtyState,
    full_watchers: AgentWatcherState,
    transcripts: TranscriptState,
    events: Arc<dyn EventSink>,
) -> Result<WorkerHandle, String> {
    let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
    let (startup, started) = mpsc::sync_channel(1);
    let alive = Arc::new(AtomicBool::new(false));
    let alive_for_thread = alive.clone();
    let callback_sender = sender.clone();
    let callback_diagnostics = diagnostics.clone();

    let join = std::thread::Builder::new()
        .name("agent-notification-watcher".to_string())
        .spawn(move || {
            let watcher =
                notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                    let Ok(event) = result else {
                        return;
                    };
                    if callback_sender
                        .try_send(WorkerCommand::SourceChanged(event.paths))
                        .is_err()
                    {
                        callback_diagnostics
                            .lock()
                            .expect("notification diagnostics mutex poisoned")
                            .queue_drops += 1;
                    }
                });

            let mut watcher = match watcher {
                Ok(watcher) => watcher,
                Err(error) => {
                    let _ = startup.send(Err(format!(
                        "create notification filesystem watcher: {error}"
                    )));
                    return;
                }
            };

            alive_for_thread.store(true, Ordering::Release);
            diagnostics
                .lock()
                .expect("notification diagnostics mutex poisoned")
                .worker_alive = true;
            let _ = startup.send(Ok(()));
            run_worker(
                &mut watcher,
                receiver,
                diagnostics.clone(),
                alive_for_thread.clone(),
                pty_state,
                full_watchers,
                transcripts,
                events,
            );
            alive_for_thread.store(false, Ordering::Release);
            diagnostics
                .lock()
                .expect("notification diagnostics mutex poisoned")
                .worker_alive = false;
        })
        .map_err(|error| format!("spawn notification worker: {error}"))?;

    started
        .recv()
        .map_err(|_| "notification worker exited during startup".to_string())??;

    Ok(WorkerHandle {
        sender,
        alive,
        join,
    })
}

fn run_worker(
    watcher: &mut RecommendedWatcher,
    receiver: Receiver<WorkerCommand>,
    diagnostics: Arc<Mutex<MutableDiagnostics>>,
    alive: Arc<AtomicBool>,
    pty_state: PtyState,
    full_watchers: AgentWatcherState,
    transcripts: TranscriptState,
    events: Arc<dyn EventSink>,
) {
    let mut registrations = HashMap::<String, Registration>::new();
    let mut last_reconcile = Instant::now();

    loop {
        if last_reconcile.elapsed() >= RECONCILE_INTERVAL {
            reconcile_sources(
                watcher,
                &mut registrations,
                &diagnostics,
                &pty_state,
                &full_watchers,
                &transcripts,
                events.as_ref(),
            );
            last_reconcile = Instant::now();
            if registrations.is_empty() {
                alive.store(false, Ordering::Release);
                break;
            }
        }

        let timeout = RECONCILE_INTERVAL.saturating_sub(last_reconcile.elapsed());
        let command = match receiver.recv_timeout(timeout) {
            Ok(command) => Some(command),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        match command {
            Some(WorkerCommand::Register {
                registration,
                acknowledge,
            }) => {
                let pty_id = registration.pty_id.clone();
                let result = if pty_state.generation(&registration.pty_id)
                    == Some(registration.pty_generation)
                {
                    if registration.provider == NotificationProvider::Kimi
                        && !full_watchers.transfer_kimi_transcript(&pty_id)
                    {
                        Err(format!(
                            "Kimi watcher missing during notification ownership transfer: {pty_id}"
                        ))
                    } else {
                        register_source(watcher, &mut registrations, registration)
                    }
                } else {
                    Err("PTY changed before notification source registration".to_string())
                };
                if let Ok(outcome) = result.as_ref() {
                    let mut metrics = diagnostics
                        .lock()
                        .expect("notification diagnostics mutex poisoned");
                    match outcome {
                        RegisterOutcome::Started => metrics.started_total += 1,
                        RegisterOutcome::Rebound => metrics.rebound_total += 1,
                        RegisterOutcome::Noop => {}
                    }
                    drop(metrics);
                    refresh_registration_diagnostics(&registrations, &diagnostics);
                }
                let _ = acknowledge.send(result.map(|_| ()));
            }
            Some(WorkerCommand::Stop {
                pty_id,
                acknowledge,
            }) => {
                let stop_transcript =
                    remove_source(watcher, &mut registrations, &pty_id, &diagnostics)
                        == Some(NotificationProvider::Kimi);
                let empty = registrations.is_empty();
                if empty {
                    alive.store(false, Ordering::Release);
                    diagnostics
                        .lock()
                        .expect("notification diagnostics mutex poisoned")
                        .worker_alive = false;
                }
                let _ = acknowledge.send(stop_transcript);
                if empty {
                    break;
                }
            }
            Some(WorkerCommand::SourceChanged(paths)) => {
                let ids: Vec<String> = registrations
                    .values()
                    .filter(|registration| {
                        registration.provider != NotificationProvider::Kimi
                            && paths.contains(&registration.source_path)
                    })
                    .map(|registration| registration.pty_id.clone())
                    .collect();
                scan_ids(&ids, &mut registrations, &diagnostics, events.as_ref());
            }
            #[cfg(feature = "e2e-test")]
            Some(WorkerCommand::Reconcile(acknowledge)) => {
                reconcile_sources(
                    watcher,
                    &mut registrations,
                    &diagnostics,
                    &pty_state,
                    &full_watchers,
                    &transcripts,
                    events.as_ref(),
                );
                let empty = registrations.is_empty();
                if empty {
                    alive.store(false, Ordering::Release);
                    diagnostics
                        .lock()
                        .expect("notification diagnostics mutex poisoned")
                        .worker_alive = false;
                }
                let _ = acknowledge.send(());
                if empty {
                    break;
                }
            }
            None => {}
            Some(WorkerCommand::Shutdown) => break,
        }
    }
}

enum RegisterOutcome {
    Started,
    Rebound,
    Noop,
}

fn register_source(
    watcher: &mut RecommendedWatcher,
    registrations: &mut HashMap<String, Registration>,
    registration: Registration,
) -> Result<RegisterOutcome, String> {
    if registrations
        .get(&registration.pty_id)
        .is_some_and(|current| {
            current.provider == registration.provider
                && current.source_path == registration.source_path
        })
    {
        return Ok(RegisterOutcome::Noop);
    }

    let pty_id = registration.pty_id.clone();
    let provider = registration.provider;

    let path_already_watched = registrations.values().any(|current| {
        current.provider != NotificationProvider::Kimi
            && current.source_path == registration.source_path
    });
    // Kimi's registration owns its relocation follower; never watch one stale wire.
    if provider != NotificationProvider::Kimi && !path_already_watched {
        watcher
            .watch(&registration.source_path, RecursiveMode::NonRecursive)
            .map_err(|error| format!("watch notification source: {error}"))?;
    }

    let replaced = registrations.insert(pty_id.clone(), registration);
    if let Some(previous) = replaced.as_ref() {
        if previous.provider != NotificationProvider::Kimi
            && !registrations.values().any(|current| {
                current.provider != NotificationProvider::Kimi
                    && current.source_path == previous.source_path
            })
        {
            let _ = watcher.unwatch(&previous.source_path);
        }
    }

    log::info!("notification watcher registered pty={pty_id} provider={provider:?}");

    Ok(if replaced.is_some() {
        RegisterOutcome::Rebound
    } else {
        RegisterOutcome::Started
    })
}

fn remove_source(
    watcher: &mut RecommendedWatcher,
    registrations: &mut HashMap<String, Registration>,
    pty_id: &str,
    diagnostics: &Arc<Mutex<MutableDiagnostics>>,
) -> Option<NotificationProvider> {
    let Some(removed) = registrations.remove(pty_id) else {
        return None;
    };
    if removed.provider != NotificationProvider::Kimi
        && !registrations.values().any(|current| {
            current.provider != NotificationProvider::Kimi
                && current.source_path == removed.source_path
        })
    {
        let _ = watcher.unwatch(&removed.source_path);
    }
    diagnostics
        .lock()
        .expect("notification diagnostics mutex poisoned")
        .stopped_total += 1;
    refresh_registration_diagnostics(registrations, diagnostics);
    log::info!("notification watcher stopped pty={pty_id}");
    Some(removed.provider)
}

fn refresh_registration_diagnostics(
    registrations: &HashMap<String, Registration>,
    diagnostics: &Arc<Mutex<MutableDiagnostics>>,
) {
    let mut next = HashMap::new();
    for registration in registrations.values() {
        *next.entry(registration.provider).or_insert(0) += 1;
    }
    let mut diagnostics = diagnostics
        .lock()
        .expect("notification diagnostics mutex poisoned");
    diagnostics.active_registrations = registrations.len();
    diagnostics.active_by_provider = next;
}

fn reconcile_sources(
    watcher: &mut RecommendedWatcher,
    registrations: &mut HashMap<String, Registration>,
    diagnostics: &Arc<Mutex<MutableDiagnostics>>,
    pty_state: &PtyState,
    full_watchers: &AgentWatcherState,
    transcripts: &TranscriptState,
    events: &dyn EventSink,
) {
    let missing: Vec<String> = registrations
        .values()
        .filter(|registration| {
            pty_state.generation(&registration.pty_id) != Some(registration.pty_generation)
        })
        .map(|registration| registration.pty_id.clone())
        .collect();
    for pty_id in missing {
        if registrations
            .get(&pty_id)
            .is_some_and(|registration| registration.provider == NotificationProvider::Kimi)
        {
            let _ = transcripts.stop(&pty_id);
        }
        let _ = remove_source(watcher, registrations, &pty_id, diagnostics);
        full_watchers.remove(&pty_id);
    }

    let ids: Vec<String> = registrations
        .values()
        .filter(|registration| registration.provider != NotificationProvider::Kimi)
        .map(|registration| registration.pty_id.clone())
        .collect();
    scan_ids(&ids, registrations, diagnostics, events);
}

fn scan_ids(
    ids: &[String],
    registrations: &mut HashMap<String, Registration>,
    diagnostics: &Arc<Mutex<MutableDiagnostics>>,
    events: &dyn EventSink,
) {
    for pty_id in ids {
        if let Some(registration) = registrations.get_mut(pty_id) {
            if let Err(error) = scan_source(registration, diagnostics, events) {
                log::warn!("notification watcher scan failed pty={pty_id}: {error}");
            }
        }
    }
}

fn scan_source(
    registration: &mut Registration,
    diagnostics: &Arc<Mutex<MutableDiagnostics>>,
    events: &dyn EventSink,
) -> Result<(), String> {
    let length = std::fs::metadata(&registration.source_path)
        .map_err(|error| format!("source metadata: {error}"))?
        .len();
    if length < registration.cursor {
        registration.cursor = length;
        registration.partial_line.clear();
        registration.dropping_oversize_line = false;
        registration.turn_active = false;
        registration.turn_id = None;
        registration.pending_opencode_idle = None;
        registration.claude_transcript_cursors.clear();
        diagnostics
            .lock()
            .expect("notification diagnostics mutex poisoned")
            .replay_suppressions += 1;
        return Ok(());
    }
    if length == registration.cursor {
        return Ok(());
    }

    let mut file =
        File::open(&registration.source_path).map_err(|error| format!("open source: {error}"))?;
    file.seek(SeekFrom::Start(registration.cursor))
        .map_err(|error| format!("seek source: {error}"))?;
    let mut buffer = [0_u8; READ_BUFFER_BYTES];

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read source: {error}"))?;
        if read == 0 {
            break;
        }
        registration.cursor += read as u64;
        diagnostics
            .lock()
            .expect("notification diagnostics mutex poisoned")
            .bytes_scanned += read as u64;

        for byte in &buffer[..read] {
            if *byte == b'\n' {
                finish_line(registration, diagnostics, events);
            } else if registration.dropping_oversize_line {
                continue;
            } else if registration.partial_line.len() < MAX_PARTIAL_LINE_BYTES {
                registration.partial_line.push(*byte);
            } else {
                registration.partial_line.clear();
                registration.dropping_oversize_line = true;
            }
        }
    }

    flush_pending_opencode_idle(registration, diagnostics, events);

    Ok(())
}

fn finish_line(
    registration: &mut Registration,
    diagnostics: &Arc<Mutex<MutableDiagnostics>>,
    events: &dyn EventSink,
) {
    if registration.dropping_oversize_line {
        registration.dropping_oversize_line = false;
        diagnostics
            .lock()
            .expect("notification diagnostics mutex poisoned")
            .parse_failures += 1;
        return;
    }

    while registration.partial_line.last() == Some(&b'\r') {
        registration.partial_line.pop();
    }
    if registration.partial_line.is_empty() {
        return;
    }

    let line = std::mem::take(&mut registration.partial_line);
    diagnostics
        .lock()
        .expect("notification diagnostics mutex poisoned")
        .records_scanned += 1;
    let classified = match registration.provider {
        NotificationProvider::Codex => classify_codex(&line),
        NotificationProvider::Kimi => classify_kimi(&line),
        NotificationProvider::OpenCode => classify_opencode(&line),
        NotificationProvider::ClaudeCode => classify_claude(&line),
    };
    match classified {
        Ok(signal) => apply_signal(registration, signal, diagnostics, events),
        Err(()) => {
            diagnostics
                .lock()
                .expect("notification diagnostics mutex poisoned")
                .parse_failures += 1;
        }
    }
}

#[derive(Debug, Deserialize)]
struct CodexEnvelope {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    record_type: String,
    payload: CodexPayload,
}

#[derive(Debug, Deserialize)]
struct CodexPayload {
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    turn_id: Option<String>,
    approval_id: Option<String>,
    call_id: Option<String>,
    request_id: Option<String>,
    name: Option<String>,
    last_agent_message: Option<String>,
}

enum ClassifiedSignal {
    Session(String),
    AssistantText(String),
    OpenCodeStatusIdle {
        occurred_at: u64,
        agent_session_id: Option<String>,
    },
    TurnStarted {
        turn_id: Option<String>,
        agent_session_id: Option<String>,
    },
    TurnEnded,
    Notification {
        reason: AgentNotificationReason,
        occurred_at: u64,
        dedupe_key: Option<String>,
        turn_id: Option<String>,
        agent_session_id: Option<String>,
        body: Option<String>,
        transcript_path: Option<String>,
        requires_active_turn: bool,
        ends_turn: bool,
    },
    Irrelevant,
}

fn classify_codex(line: &[u8]) -> Result<ClassifiedSignal, ()> {
    let envelope: CodexEnvelope = serde_json::from_slice(line).map_err(|_| ())?;
    let occurred_at = timestamp_millis(envelope.timestamp.as_deref());

    if envelope.record_type == "session_meta" {
        return Ok(envelope
            .payload
            .id
            .map(ClassifiedSignal::Session)
            .unwrap_or(ClassifiedSignal::Irrelevant));
    }

    if envelope.record_type == "response_item"
        && envelope.payload.kind.as_deref() == Some("function_call")
        && envelope.payload.name.as_deref() == Some("request_user_input")
    {
        let key = envelope.payload.call_id;
        return Ok(ClassifiedSignal::Notification {
            reason: AgentNotificationReason::QuestionRequested,
            occurred_at,
            dedupe_key: key,
            turn_id: None,
            agent_session_id: None,
            body: None,
            transcript_path: None,
            requires_active_turn: false,
            ends_turn: false,
        });
    }

    if envelope.record_type != "event_msg" {
        return Ok(ClassifiedSignal::Irrelevant);
    }

    match envelope.payload.kind.as_deref() {
        Some("task_started") => Ok(ClassifiedSignal::TurnStarted {
            turn_id: envelope.payload.turn_id,
            agent_session_id: None,
        }),
        Some("task_complete") => {
            let turn_id = envelope.payload.turn_id;
            Ok(ClassifiedSignal::Notification {
                reason: AgentNotificationReason::TurnComplete,
                occurred_at,
                dedupe_key: turn_id.as_ref().map(|id| format!("turn:{id}")),
                turn_id,
                agent_session_id: None,
                body: envelope.payload.last_agent_message,
                transcript_path: None,
                // Registration starts at EOF and can occur after task_started.
                // Any terminal record appended after that boundary is live.
                requires_active_turn: false,
                ends_turn: true,
            })
        }
        Some("turn_aborted") => Ok(ClassifiedSignal::TurnEnded),
        Some("exec_approval_request")
        | Some("apply_patch_approval_request")
        | Some("request_permissions") => {
            let key = envelope.payload.approval_id.or(envelope.payload.call_id);
            Ok(ClassifiedSignal::Notification {
                reason: AgentNotificationReason::ApprovalRequested,
                occurred_at,
                dedupe_key: key,
                turn_id: None,
                agent_session_id: None,
                body: None,
                transcript_path: None,
                requires_active_turn: false,
                ends_turn: false,
            })
        }
        Some("elicitation_request") => {
            let key = envelope.payload.request_id;
            Ok(ClassifiedSignal::Notification {
                reason: AgentNotificationReason::QuestionRequested,
                occurred_at,
                dedupe_key: key,
                turn_id: None,
                agent_session_id: None,
                body: None,
                transcript_path: None,
                requires_active_turn: false,
                ends_turn: false,
            })
        }
        _ => Ok(ClassifiedSignal::Irrelevant),
    }
}

#[derive(Deserialize)]
struct ClaudeEnvelope {
    #[serde(rename = "type")]
    line_type: Option<String>,
    timestamp: Option<String>,
    uuid: Option<String>,
    message: Option<ClaudeMessage>,
    hook_event_name: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    session_id: Option<String>,
    transcript_path: Option<String>,
    #[serde(default)]
    error: Value,
    error_details: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    #[serde(default)]
    content: Value,
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeTranscriptBodyLine {
    #[serde(rename = "type")]
    line_type: Option<String>,
    message: Option<ClaudeMessage>,
}

fn classify_claude(line: &[u8]) -> Result<ClassifiedSignal, ()> {
    let envelope: ClaudeEnvelope = serde_json::from_slice(line).map_err(|_| ())?;
    let occurred_at = timestamp_millis(envelope.timestamp.as_deref());

    if let Some(hook) = envelope.hook_event_name.as_deref() {
        if hook == "UserPromptSubmit" {
            return Ok(ClassifiedSignal::TurnStarted {
                turn_id: None,
                agent_session_id: envelope.session_id,
            });
        }
        let (reason, requires_active_turn, ends_turn) = match hook {
            "Stop" => (AgentNotificationReason::TurnComplete, true, true),
            "PermissionRequest" => (AgentNotificationReason::ApprovalRequested, false, false),
            "PreToolUse" if envelope.tool_name.as_deref() == Some("AskUserQuestion") => {
                (AgentNotificationReason::QuestionRequested, false, false)
            }
            "StopFailure" => (AgentNotificationReason::AgentError, false, true),
            _ => return Ok(ClassifiedSignal::Irrelevant),
        };
        let body = match reason {
            AgentNotificationReason::AgentError => envelope
                .error
                .as_str()
                .or_else(|| envelope.error.get("message").and_then(Value::as_str))
                .or(envelope.error_details.as_deref())
                .map(str::to_string),
            _ => None,
        };
        return Ok(ClassifiedSignal::Notification {
            reason,
            occurred_at,
            dedupe_key: envelope.tool_use_id,
            turn_id: None,
            agent_session_id: envelope.session_id,
            body,
            transcript_path: envelope.transcript_path,
            requires_active_turn,
            ends_turn,
        });
    }

    if envelope.line_type.as_deref() == Some("user")
        && envelope.message.as_ref().is_some_and(|message| {
            crate::agent::adapter::claude_code::transcript::is_user_prompt(&message.content)
        })
    {
        return Ok(ClassifiedSignal::TurnStarted {
            turn_id: None,
            agent_session_id: None,
        });
    }

    let Some(message) = envelope.message else {
        return Ok(ClassifiedSignal::Irrelevant);
    };
    if envelope.line_type.as_deref() != Some("assistant")
        || !matches!(
            message.stop_reason.as_deref(),
            Some("end_turn" | "stop_sequence" | "max_tokens")
        )
    {
        return Ok(ClassifiedSignal::Irrelevant);
    }

    let dedupe_key = envelope
        .uuid
        .or(envelope.timestamp)
        .map(|id| format!("turn:{id}"));
    Ok(ClassifiedSignal::Notification {
        reason: AgentNotificationReason::TurnComplete,
        occurred_at,
        dedupe_key,
        turn_id: None,
        agent_session_id: None,
        body: claude_message_text(&message.content),
        transcript_path: None,
        requires_active_turn: true,
        ends_turn: true,
    })
}

fn claude_hook_body(
    registration: &mut Registration,
    reason: AgentNotificationReason,
    transcript_path: Option<&str>,
) -> Option<String> {
    if !matches!(
        reason,
        AgentNotificationReason::TurnComplete | AgentNotificationReason::AgentError
    ) {
        return None;
    }

    let transcript_path = transcript_path?;
    let path =
        crate::agent::adapter::claude_code::transcript::validate_transcript_path(transcript_path)
            .ok()?;

    let cursor = registration
        .claude_transcript_cursors
        .entry(path.clone())
        .or_default();
    latest_claude_transcript_body_at_path(&path, cursor)
}

fn latest_claude_transcript_body_at_path(path: &Path, cursor: &mut u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    if length < *cursor {
        *cursor = 0;
    }
    let should_skip_partial_initial_line =
        *cursor == 0 && length > CLAUDE_TRANSCRIPT_INITIAL_TAIL_BYTES;
    if should_skip_partial_initial_line {
        *cursor = length.saturating_sub(CLAUDE_TRANSCRIPT_INITIAL_TAIL_BYTES);
    }
    file.seek(SeekFrom::Start(*cursor)).ok()?;
    if should_skip_partial_initial_line {
        let mut byte = [0_u8; 1];
        while file.read(&mut byte).ok()? == 1 {
            *cursor += 1;
            if byte[0] == b'\n' {
                break;
            }
        }
    }

    let mut latest = None;

    for_each_bounded_line(
        BufReader::new(file),
        "Claude notification transcript",
        |line| {
            let Ok(dto) = serde_json::from_str::<ClaudeTranscriptBodyLine>(line) else {
                return;
            };
            if dto.line_type.as_deref() != Some("assistant") {
                return;
            }
            let Some(message) = dto.message else {
                return;
            };
            if !matches!(
                message.stop_reason.as_deref(),
                Some("end_turn" | "stop_sequence" | "max_tokens")
            ) {
                return;
            }
            if let Some(body) = claude_message_text(&message.content) {
                latest = Some(body);
            }
        },
    )
    .ok()?;

    *cursor = length;
    latest.and_then(|body| normalize_notification_body(&body))
}

fn claude_message_text(content: &Value) -> Option<String> {
    let text = content
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");

    (!text.is_empty()).then_some(text)
}

#[derive(Deserialize)]
struct KimiEnvelope {
    #[serde(rename = "type")]
    record_type: String,
    time: Option<u64>,
    origin: Option<KimiOrigin>,
    event: Option<KimiEvent>,
}

#[derive(Deserialize)]
struct KimiOrigin {
    kind: Option<String>,
}

#[derive(Deserialize)]
struct KimiEvent {
    #[serde(rename = "type")]
    kind: Option<String>,
    uuid: Option<String>,
    #[serde(rename = "turnId")]
    turn_id: Option<String>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
    part: Option<KimiPart>,
}

#[derive(Deserialize)]
struct KimiPart {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

fn classify_kimi(line: &[u8]) -> Result<ClassifiedSignal, ()> {
    let envelope: KimiEnvelope = serde_json::from_slice(line).map_err(|_| ())?;
    if envelope.record_type == "turn.prompt"
        && envelope.origin.and_then(|origin| origin.kind).as_deref() == Some("user")
    {
        return Ok(ClassifiedSignal::TurnStarted {
            turn_id: None,
            agent_session_id: None,
        });
    }

    let Some(event) = envelope.event else {
        return Ok(ClassifiedSignal::Irrelevant);
    };
    if envelope.record_type == "context.append_loop_event"
        && event.kind.as_deref() == Some("content.part")
        && event.part.as_ref().and_then(|part| part.kind.as_deref()) == Some("text")
    {
        return Ok(event
            .part
            .and_then(|part| part.text)
            .filter(|text| !text.is_empty())
            .map(ClassifiedSignal::AssistantText)
            .unwrap_or(ClassifiedSignal::Irrelevant));
    }
    if envelope.record_type != "context.append_loop_event"
        || event.kind.as_deref() != Some("step.end")
        || event.finish_reason.as_deref() != Some("end_turn")
    {
        return Ok(ClassifiedSignal::Irrelevant);
    }

    Ok(ClassifiedSignal::Notification {
        reason: AgentNotificationReason::TurnComplete,
        occurred_at: envelope.time.unwrap_or_else(now_millis),
        dedupe_key: event.uuid.map(|id| format!("turn:{id}")),
        turn_id: event.turn_id,
        agent_session_id: None,
        body: None,
        transcript_path: None,
        requires_active_turn: true,
        ends_turn: true,
    })
}

#[derive(Deserialize)]
struct OpenCodeEnvelope {
    ts: Option<i64>,
    kind: Option<String>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    #[serde(default)]
    data: OpenCodeData,
}

#[derive(Default, Deserialize)]
struct OpenCodeData {
    id: Option<String>,
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    status: Option<OpenCodeStatus>,
    text: Option<String>,
    #[serde(default)]
    error: Value,
}

#[derive(Deserialize)]
struct OpenCodeStatus {
    #[serde(rename = "type")]
    kind: Option<String>,
}

fn classify_opencode(line: &[u8]) -> Result<ClassifiedSignal, ()> {
    let envelope: OpenCodeEnvelope = serde_json::from_slice(line).map_err(|_| ())?;
    if envelope.kind.as_deref() != Some("event") {
        return Ok(ClassifiedSignal::Irrelevant);
    }

    let occurred_at = envelope
        .ts
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or_else(now_millis);
    let agent_session_id = envelope.data.session_id;
    let status_kind = envelope.data.status.and_then(|status| status.kind);
    match envelope.event_type.as_deref() {
        Some("session.status") if matches!(status_kind.as_deref(), Some("busy" | "retry")) => {
            Ok(ClassifiedSignal::TurnStarted {
                turn_id: None,
                agent_session_id,
            })
        }
        Some("session.status") if status_kind.as_deref() == Some("idle") => {
            Ok(ClassifiedSignal::OpenCodeStatusIdle {
                occurred_at,
                agent_session_id,
            })
        }
        Some("assistant.text") => Ok(envelope
            .data
            .text
            .filter(|text| !text.is_empty())
            .map(ClassifiedSignal::AssistantText)
            .unwrap_or(ClassifiedSignal::Irrelevant)),
        Some("session.idle") => Ok(opencode_completion(occurred_at, agent_session_id)),
        Some("session.error") => Ok(ClassifiedSignal::Notification {
            reason: AgentNotificationReason::AgentError,
            occurred_at,
            dedupe_key: envelope.data.id.or_else(|| {
                agent_session_id
                    .as_ref()
                    .map(|id| format!("error:{id}:{occurred_at}"))
            }),
            turn_id: None,
            agent_session_id,
            body: envelope
                .data
                .error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string),
            transcript_path: None,
            requires_active_turn: true,
            ends_turn: true,
        }),
        Some("permission.asked") => Ok(ClassifiedSignal::Notification {
            reason: AgentNotificationReason::ApprovalRequested,
            occurred_at,
            dedupe_key: envelope.data.id,
            turn_id: None,
            agent_session_id,
            body: None,
            transcript_path: None,
            requires_active_turn: false,
            ends_turn: false,
        }),
        Some("question.asked") => Ok(ClassifiedSignal::Notification {
            reason: AgentNotificationReason::QuestionRequested,
            occurred_at,
            dedupe_key: envelope.data.id,
            turn_id: None,
            agent_session_id,
            body: None,
            transcript_path: None,
            requires_active_turn: false,
            ends_turn: false,
        }),
        _ => Ok(ClassifiedSignal::Irrelevant),
    }
}

fn opencode_completion(occurred_at: u64, agent_session_id: Option<String>) -> ClassifiedSignal {
    ClassifiedSignal::Notification {
        reason: AgentNotificationReason::TurnComplete,
        occurred_at,
        dedupe_key: agent_session_id
            .as_ref()
            .map(|id| format!("turn:{id}:{occurred_at}")),
        turn_id: None,
        agent_session_id,
        body: None,
        transcript_path: None,
        requires_active_turn: true,
        ends_turn: true,
    }
}

fn apply_signal(
    registration: &mut Registration,
    signal: ClassifiedSignal,
    diagnostics: &Arc<Mutex<MutableDiagnostics>>,
    events: &dyn EventSink,
) {
    match signal {
        ClassifiedSignal::Session(agent_session_id) => {
            registration.agent_session_id = Some(agent_session_id);
        }
        ClassifiedSignal::AssistantText(text) => {
            if let Some(body) = normalize_notification_body(&text) {
                registration.notification_body = Some(body);
                flush_pending_opencode_idle(registration, diagnostics, events);
            }
        }
        ClassifiedSignal::OpenCodeStatusIdle {
            occurred_at,
            agent_session_id,
        } => {
            if agent_session_id.is_some() {
                registration.agent_session_id = agent_session_id.clone();
            }
            registration.pending_opencode_idle = Some(PendingOpenCodeIdle {
                occurred_at,
                agent_session_id,
            });
        }
        ClassifiedSignal::TurnStarted {
            turn_id,
            agent_session_id,
        } => {
            flush_pending_opencode_idle(registration, diagnostics, events);
            if agent_session_id.is_some() {
                registration.agent_session_id = agent_session_id;
            }
            registration.turn_active = true;
            registration.turn_id = turn_id;
            registration.notification_body = None;
        }
        ClassifiedSignal::TurnEnded => {
            registration.turn_active = false;
            registration.turn_id = None;
            registration.notification_body = None;
            registration.pending_opencode_idle = None;
        }
        ClassifiedSignal::Notification {
            reason,
            occurred_at,
            dedupe_key,
            turn_id,
            agent_session_id,
            body,
            transcript_path,
            requires_active_turn,
            ends_turn,
        } => {
            if agent_session_id.is_some() {
                registration.agent_session_id = agent_session_id;
            }
            let mismatched_turn = registration.turn_id.is_some()
                && turn_id.is_some()
                && registration.turn_id != turn_id;
            if mismatched_turn || (requires_active_turn && !registration.turn_active) {
                diagnostics
                    .lock()
                    .expect("notification diagnostics mutex poisoned")
                    .replay_suppressions += 1;
                return;
            }
            if ends_turn {
                registration.turn_active = false;
                registration.turn_id = None;
                registration.pending_opencode_idle = None;
            }

            let buffered_body = if ends_turn {
                registration.notification_body.take()
            } else {
                None
            };
            let transcript_body =
                claude_hook_body(registration, reason, transcript_path.as_deref());
            let body = body
                .as_deref()
                .and_then(normalize_notification_body)
                .or(transcript_body)
                .or(buffered_body)
                .or_else(|| notification_action_body(reason).map(str::to_string));

            if dedupe_key
                .as_deref()
                .is_some_and(|key| !registration.remember_dedupe_key(key))
            {
                diagnostics
                    .lock()
                    .expect("notification diagnostics mutex poisoned")
                    .duplicate_suppressions += 1;
                return;
            }

            let event = AgentNotificationEvent {
                pty_id: registration.pty_id.clone(),
                agent_session_id: registration.agent_session_id.clone(),
                reason,
                title: notification_title(registration.provider, reason).to_string(),
                body,
                occurred_at,
                dedupe_key,
            };
            let emitted = serialize_event(&event)
                .and_then(|payload| events.emit_json("agent-notification", payload));
            if emitted.is_ok() {
                let mut diagnostics = diagnostics
                    .lock()
                    .expect("notification diagnostics mutex poisoned");
                *diagnostics.notifications_emitted.entry(reason).or_insert(0) += 1;
                diagnostics.last_successful_event_at = Some(now_millis());
            } else if let Err(error) = emitted {
                log::warn!("failed to emit agent-notification: {error}");
            }
        }
        ClassifiedSignal::Irrelevant => {}
    }
}

fn flush_pending_opencode_idle(
    registration: &mut Registration,
    diagnostics: &Arc<Mutex<MutableDiagnostics>>,
    events: &dyn EventSink,
) {
    if registration.provider != NotificationProvider::OpenCode {
        return;
    }

    if let Some(pending) = registration.pending_opencode_idle.take() {
        apply_signal(
            registration,
            opencode_completion(pending.occurred_at, pending.agent_session_id),
            diagnostics,
            events,
        );
    }
}

pub(crate) fn normalize_notification_body(message: &str) -> Option<String> {
    let normalized = message
        .lines()
        .map(str::trim)
        .take_while(|line| !line.starts_with("<<<VIMEFLOW_"))
        .find_map(|line| {
            let without_markdown = line
                .trim_matches(|character: char| matches!(character, '#' | '-' | '*' | '>' | '`'))
                .trim();
            let without_controls: String = without_markdown
                .chars()
                .filter(|character| !character.is_control())
                .collect();
            let collapsed = without_controls
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");

            (!collapsed.is_empty()).then_some(collapsed)
        })?;

    let mut characters = normalized.chars();
    let mut body: String = characters
        .by_ref()
        .take(MAX_NOTIFICATION_BODY_CHARS)
        .collect();
    if characters.next().is_some() {
        body.pop();
        body.push('…');
    }

    Some(body)
}

/// Kimi completions are emitted by the relocation-following transcript
/// supervisor rather than a fixed-path wire watcher: a completion written to
/// a relocated wire before the next watcher tick would otherwise be lost.
pub(crate) fn emit_kimi_completion(
    events: &dyn EventSink,
    pty_id: &str,
    agent_session_id: &str,
    occurred_at: Option<u64>,
    event_id: Option<&str>,
    body: Option<String>,
) {
    if agent_session_id.is_empty() {
        return;
    }

    let event = AgentNotificationEvent {
        pty_id: pty_id.to_string(),
        agent_session_id: Some(agent_session_id.to_string()),
        reason: AgentNotificationReason::TurnComplete,
        title: "Kimi finished".to_string(),
        body,
        occurred_at: occurred_at.unwrap_or_else(now_millis),
        dedupe_key: event_id.map(|id| format!("turn:{id}")),
    };
    if let Err(error) =
        serialize_event(&event).and_then(|payload| events.emit_json("agent-notification", payload))
    {
        log::warn!("failed to emit Kimi completion notification: {error}");
    }
}

fn notification_action_body(reason: AgentNotificationReason) -> Option<&'static str> {
    match reason {
        AgentNotificationReason::ApprovalRequested => Some("Awaiting your approval"),
        AgentNotificationReason::QuestionRequested => Some("Awaiting your response"),
        AgentNotificationReason::TurnComplete | AgentNotificationReason::AgentError => None,
    }
}

fn notification_title(
    provider: NotificationProvider,
    reason: AgentNotificationReason,
) -> &'static str {
    match (provider, reason) {
        (NotificationProvider::ClaudeCode, AgentNotificationReason::TurnComplete) => {
            "Claude finished"
        }
        (NotificationProvider::ClaudeCode, AgentNotificationReason::ApprovalRequested) => {
            "Claude needs approval"
        }
        (NotificationProvider::ClaudeCode, AgentNotificationReason::QuestionRequested) => {
            "Claude has a question"
        }
        (NotificationProvider::ClaudeCode, AgentNotificationReason::AgentError) => "Claude failed",
        (NotificationProvider::Codex, AgentNotificationReason::TurnComplete) => "Codex finished",
        (NotificationProvider::Codex, AgentNotificationReason::ApprovalRequested) => {
            "Codex needs approval"
        }
        (NotificationProvider::Codex, AgentNotificationReason::QuestionRequested) => {
            "Codex has a question"
        }
        (NotificationProvider::Codex, AgentNotificationReason::AgentError) => "Codex failed",
        (NotificationProvider::Kimi, AgentNotificationReason::TurnComplete) => "Kimi finished",
        (NotificationProvider::Kimi, AgentNotificationReason::ApprovalRequested) => {
            "Kimi needs approval"
        }
        (NotificationProvider::Kimi, AgentNotificationReason::QuestionRequested) => {
            "Kimi has a question"
        }
        (NotificationProvider::Kimi, AgentNotificationReason::AgentError) => "Kimi failed",
        (NotificationProvider::OpenCode, AgentNotificationReason::TurnComplete) => {
            "OpenCode finished"
        }
        (NotificationProvider::OpenCode, AgentNotificationReason::ApprovalRequested) => {
            "OpenCode needs approval"
        }
        (NotificationProvider::OpenCode, AgentNotificationReason::QuestionRequested) => {
            "OpenCode has a question"
        }
        (NotificationProvider::OpenCode, AgentNotificationReason::AgentError) => "OpenCode failed",
    }
}

fn timestamp_millis(timestamp: Option<&str>) -> u64 {
    timestamp
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .and_then(|value| u64::try_from(value.timestamp_millis()).ok())
        .unwrap_or_else(now_millis)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    kib.checked_mul(1024)
}

#[cfg(not(target_os = "linux"))]
fn process_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::runtime::event_sink::FakeEventSink;

    #[test]
    fn codex_classifier_maps_only_notification_relevant_records() {
        let cases = [
            (
                r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#,
                "started",
            ),
            (
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}"#,
                "turn-complete",
            ),
            (
                r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t1","reason":"interrupted"}}"#,
                "ended",
            ),
            (
                r#"{"type":"event_msg","payload":{"type":"exec_approval_request","approval_id":"a1"}}"#,
                "approval-requested",
            ),
            (
                r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"q1"}}"#,
                "question-requested",
            ),
            (
                r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"e1","exit_code":1}}"#,
                "irrelevant",
            ),
            (
                r#"{"type":"event_msg","payload":{"type":"token_count"}}"#,
                "irrelevant",
            ),
        ];

        for (line, expected) in cases {
            let actual = match classify_codex(line.as_bytes()).expect("valid Codex envelope") {
                ClassifiedSignal::TurnStarted { .. } => "started",
                ClassifiedSignal::Notification { reason, .. } => match reason {
                    AgentNotificationReason::TurnComplete => "turn-complete",
                    AgentNotificationReason::ApprovalRequested => "approval-requested",
                    AgentNotificationReason::QuestionRequested => "question-requested",
                    AgentNotificationReason::AgentError => "agent-error",
                },
                ClassifiedSignal::Irrelevant => "irrelevant",
                ClassifiedSignal::Session(_) => "session",
                ClassifiedSignal::AssistantText(_) => "assistant-text",
                ClassifiedSignal::OpenCodeStatusIdle { .. } => "opencode-status-idle",
                ClassifiedSignal::TurnEnded => "ended",
            };
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn kimi_classifier_maps_real_wire_end_turn() {
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/agent/adapter/kimi/fixtures/sample_wire.jsonl"
        ));
        let mut starts = 0;
        let mut completions = Vec::new();

        for line in fixture.lines() {
            match classify_kimi(line.as_bytes()).expect("valid Kimi wire record") {
                ClassifiedSignal::TurnStarted { .. } => starts += 1,
                ClassifiedSignal::Notification {
                    reason: AgentNotificationReason::TurnComplete,
                    occurred_at,
                    dedupe_key,
                    ..
                } => completions.push((occurred_at, dedupe_key)),
                _ => {}
            }
        }

        assert_eq!(starts, 1);
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].0, 1_781_345_365_638);
        assert_eq!(
            completions[0].1.as_deref(),
            Some("turn:6eeeb415-2667-482d-a5b4-53e283346c94")
        );
    }

    #[test]
    fn opencode_classifier_maps_notification_contract() {
        let cases = [
            (
                r#"{"v":1,"ts":1,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"busy"}}}"#,
                "started",
            ),
            (
                r#"{"v":1,"ts":2,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"idle"}}}"#,
                "turn-complete",
            ),
            (
                r#"{"v":1,"ts":2,"kind":"event","type":"session.idle","data":{"sessionID":"ses1"}}"#,
                "turn-complete",
            ),
            (
                r#"{"v":1,"ts":3,"kind":"event","type":"permission.asked","data":{"id":"permission-1","sessionID":"ses1"}}"#,
                "approval-requested",
            ),
            (
                r#"{"v":1,"ts":4,"kind":"event","type":"question.asked","data":{"id":"question-1","sessionID":"ses1"}}"#,
                "question-requested",
            ),
            (
                r#"{"v":1,"ts":5,"kind":"event","type":"session.error","data":{"id":"error-1","sessionID":"ses1","error":{"name":"ProviderError"}}}"#,
                "agent-error",
            ),
        ];

        for (line, expected) in cases {
            let actual = match classify_opencode(line.as_bytes()).expect("valid OpenCode record") {
                ClassifiedSignal::TurnStarted {
                    agent_session_id, ..
                } => {
                    assert_eq!(agent_session_id.as_deref(), Some("ses1"));
                    "started"
                }
                ClassifiedSignal::Notification {
                    reason,
                    agent_session_id,
                    ..
                } => {
                    assert_eq!(agent_session_id.as_deref(), Some("ses1"));
                    match reason {
                        AgentNotificationReason::TurnComplete => "turn-complete",
                        AgentNotificationReason::ApprovalRequested => "approval-requested",
                        AgentNotificationReason::QuestionRequested => "question-requested",
                        AgentNotificationReason::AgentError => "agent-error",
                    }
                }
                ClassifiedSignal::OpenCodeStatusIdle {
                    agent_session_id, ..
                } => {
                    assert_eq!(agent_session_id.as_deref(), Some("ses1"));
                    "turn-complete"
                }
                _ => "irrelevant",
            };
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn opencode_classifier_maps_real_bridge_completion() {
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/agent/adapter/opencode/fixtures/sample_bridge.jsonl"
        ));
        let signals: Vec<&str> = fixture
            .lines()
            .filter_map(|line| match classify_opencode(line.as_bytes()).ok()? {
                ClassifiedSignal::TurnStarted { .. } => Some("started"),
                ClassifiedSignal::Notification {
                    reason: AgentNotificationReason::TurnComplete,
                    ..
                } => Some("turn-complete"),
                ClassifiedSignal::OpenCodeStatusIdle { .. } => Some("turn-complete"),
                _ => None,
            })
            .collect();

        assert_eq!(signals, ["started", "turn-complete"]);
    }

    #[test]
    fn claude_classifier_maps_transcript_and_hook_contracts() {
        let cases = [
            (
                r#"{"type":"user","timestamp":"2026-04-28T11:00:00Z","message":{"content":"fix the bug"}}"#,
                "started",
            ),
            (
                r#"{"type":"assistant","uuid":"assistant-1","timestamp":"2026-04-28T11:00:01Z","message":{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#,
                "turn-complete",
            ),
            (
                r#"{"hook_event_name":"PermissionRequest","tool_use_id":"approval-1"}"#,
                "approval-requested",
            ),
            (
                r#"{"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"question-1"}"#,
                "question-requested",
            ),
            (
                r#"{"hook_event_name":"StopFailure","tool_use_id":"error-1"}"#,
                "agent-error",
            ),
            (
                r#"{"hook_event_name":"UserPromptSubmit","session_id":"claude-1"}"#,
                "started",
            ),
            (
                r#"{"hook_event_name":"Stop","session_id":"claude-1"}"#,
                "turn-complete",
            ),
        ];

        for (line, expected) in cases {
            let actual = match classify_claude(line.as_bytes()).expect("valid Claude record") {
                ClassifiedSignal::TurnStarted { .. } => "started",
                ClassifiedSignal::Notification { reason, .. } => match reason {
                    AgentNotificationReason::TurnComplete => "turn-complete",
                    AgentNotificationReason::ApprovalRequested => "approval-requested",
                    AgentNotificationReason::QuestionRequested => "question-requested",
                    AgentNotificationReason::AgentError => "agent-error",
                },
                _ => "irrelevant",
            };
            assert_eq!(actual, expected);
        }

        assert!(matches!(
            classify_claude(
                br#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1"}]}}"#
            ),
            Ok(ClassifiedSignal::Irrelevant)
        ));
    }

    #[test]
    fn claude_completion_extracts_body_from_transcript_assistant_text() {
        let line = br###"{"type":"assistant","uuid":"assistant-1","timestamp":"2026-04-28T11:00:01Z","message":{"content":[{"type":"text","text":"## PR #42 is ready for review\n<<<VIMEFLOW_REPLY\nprotocol\nVIMEFLOW_REPLY>>>"}],"stop_reason":"end_turn"}}"###;

        let actual = classify_claude(line).expect("valid Claude record");

        match actual {
            ClassifiedSignal::Notification { body, .. } => {
                assert_eq!(
                    body.as_deref(),
                    Some(
                        "## PR #42 is ready for review\n<<<VIMEFLOW_REPLY\nprotocol\nVIMEFLOW_REPLY>>>"
                    )
                );
            }
            _ => panic!("expected notification"),
        }
    }

    #[test]
    fn claude_stop_hook_body_reads_latest_completed_transcript_text() {
        let temp = tempfile::tempdir().expect("tempdir");
        let transcript = temp.path().join("claude.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"first\"}],\"stop_reason\":\"end_turn\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"latest done\"}],\"stop_reason\":\"end_turn\"}}\n"
            ),
        )
        .expect("write transcript");
        let mut cursor = 0;

        assert_eq!(
            latest_claude_transcript_body_at_path(&transcript, &mut cursor).as_deref(),
            Some("latest done")
        );
        assert_eq!(
            cursor,
            std::fs::metadata(&transcript).expect("metadata").len()
        );
    }

    #[test]
    fn claude_stop_hook_normalizes_transcript_body() {
        let temp = tempfile::tempdir().expect("tempdir");
        let transcript = temp.path().join("claude.jsonl");
        let text = format!("## Finished\u{7} {}", "x".repeat(100));
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "assistant",
                    "message": {
                        "content": [{ "type": "text", "text": text }],
                        "stop_reason": "end_turn",
                    },
                })
            ),
        )
        .expect("write transcript");
        let stop = classify_claude(
            serde_json::json!({
                "hook_event_name": "Stop",
                "transcript_path": transcript,
            })
            .to_string()
            .as_bytes(),
        )
        .expect("valid Stop hook");
        let ClassifiedSignal::Notification {
            transcript_path: Some(_),
            ..
        } = stop
        else {
            panic!("expected Stop notification with transcript path");
        };
        let mut cursor = 0;

        let body = latest_claude_transcript_body_at_path(&transcript, &mut cursor)
            .expect("transcript body");

        assert_eq!(body.chars().count(), MAX_NOTIFICATION_BODY_CHARS);
        assert!(!body.chars().any(char::is_control));
        assert!(body.starts_with("Finished "));
        assert!(body.ends_with('…'));
    }

    #[test]
    fn initial_claude_transcript_body_scan_starts_from_bounded_tail() {
        let temp = tempfile::tempdir().expect("tempdir");
        let transcript = temp.path().join("claude.jsonl");
        let old_line = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}],\"stop_reason\":\"end_turn\"}}}}\n",
            "old".repeat(100_000)
        );
        let latest_line =
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"tail\"}],\"stop_reason\":\"end_turn\"}}\n";
        std::fs::write(&transcript, format!("{old_line}{latest_line}")).expect("write transcript");
        let mut cursor = 0;

        assert_eq!(
            latest_claude_transcript_body_at_path(&transcript, &mut cursor).as_deref(),
            Some("tail")
        );
        assert_eq!(
            cursor,
            std::fs::metadata(&transcript).expect("metadata").len()
        );
    }

    #[test]
    fn claude_stop_hook_body_scans_only_new_transcript_bytes_after_cursor() {
        use std::io::{Seek, SeekFrom, Write};

        let temp = tempfile::tempdir().expect("tempdir");
        let transcript = temp.path().join("claude.jsonl");
        let mut file = std::fs::File::create(&transcript).expect("create transcript");
        writeln!(
            file,
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"first\"}}],\"stop_reason\":\"end_turn\"}}}}"
        )
        .expect("write first");
        file.flush().expect("flush first");
        let mut cursor = 0;

        assert_eq!(
            latest_claude_transcript_body_at_path(&transcript, &mut cursor).as_deref(),
            Some("first")
        );
        let first_cursor = cursor;

        file.seek(SeekFrom::End(0)).expect("seek end");
        writeln!(
            file,
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"second\"}}],\"stop_reason\":\"end_turn\"}}}}"
        )
        .expect("write second");
        file.flush().expect("flush second");

        assert_eq!(
            latest_claude_transcript_body_at_path(&transcript, &mut cursor).as_deref(),
            Some("second")
        );
        assert!(cursor > first_cursor);
    }

    #[test]
    fn timestamp_falls_back_without_retaining_provider_content() {
        assert_eq!(
            timestamp_millis(Some("2026-08-02T15:00:00Z")),
            1_785_682_800_000
        );
        assert!(timestamp_millis(Some("not-a-time")) > 0);
    }

    #[test]
    fn notification_body_caps_unicode_text_at_80_characters_with_ellipsis() {
        let body = normalize_notification_body(&"界".repeat(81)).expect("notification body");

        assert_eq!(body.chars().count(), 80);
        assert_eq!(body, format!("{}…", "界".repeat(79)));
    }

    #[test]
    fn provider_error_notifications_use_bridge_messages() {
        let cases = [
            (
                NotificationProvider::ClaudeCode,
                r#"{"hook_event_name":"StopFailure","error":{"message":"request failed"}}"#,
                "request failed",
            ),
            (
                NotificationProvider::OpenCode,
                r#"{"v":1,"ts":2,"kind":"event","type":"session.error","data":{"sessionID":"ses1","error":{"name":"ProviderError","message":"Model stopped responding"}}}"#,
                "Model stopped responding",
            ),
        ];

        for (provider, line, expected) in cases {
            let temp = tempfile::tempdir().expect("tempdir");
            let mut registration = Registration::new(
                "pty-error".to_string(),
                1,
                provider,
                temp.path().join("events.jsonl"),
                0,
            );
            registration.turn_active = true;
            registration.notification_body = Some("unrelated transcript text".to_string());
            let diagnostics = Arc::new(Mutex::new(MutableDiagnostics::default()));
            let sink = FakeEventSink::new();
            let signal = match provider {
                NotificationProvider::ClaudeCode => classify_claude(line.as_bytes()),
                NotificationProvider::OpenCode => classify_opencode(line.as_bytes()),
                _ => unreachable!("provider error test only covers Claude and OpenCode"),
            }
            .expect("valid provider error record");

            apply_signal(&mut registration, signal, &diagnostics, &sink);

            assert_eq!(sink.count("agent-notification"), 1);
            let event = &sink.recorded()[0].1;
            assert_eq!(event["reason"], "agent-error");
            assert_eq!(event["body"], expected);
        }
    }

    #[test]
    fn incremental_scan_starts_at_eof_and_emits_only_live_notification_events() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("rollout.jsonl");
        std::fs::write(
            &source,
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"old\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"old\"}}\n",
            ),
        )
        .expect("seed rollout");
        let cursor = std::fs::metadata(&source).expect("metadata").len();
        let mut registration = Registration::new(
            "pty-a".to_string(),
            1,
            NotificationProvider::Codex,
            source.clone(),
            cursor,
        );
        let diagnostics = Arc::new(Mutex::new(MutableDiagnostics::default()));
        let sink = FakeEventSink::new();

        scan_source(&mut registration, &diagnostics, &sink).expect("initial scan");
        assert_eq!(sink.count("agent-notification"), 0);

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .expect("open rollout");
        writeln!(
            file,
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"agent-a\"}}}}"
        )
        .expect("append session");
        writeln!(
            file,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"live\"}}}}"
        )
        .expect("append start");
        writeln!(
            file,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\"}}}}"
        )
        .expect("append noise");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "task_complete",
                    "turn_id": "live",
                    "last_agent_message": "## PR #42 is ready for review\n<<<VIMEFLOW_REPLY\nprotocol payload\nVIMEFLOW_REPLY>>>",
                },
            })
        )
        .expect("append complete");
        file.flush().expect("flush rollout");

        scan_source(&mut registration, &diagnostics, &sink).expect("live scan");

        assert_eq!(sink.count("agent-notification"), 1);
        assert_eq!(sink.count("agent-status"), 0);
        let recorded = sink.recorded();
        assert_eq!(recorded[0].1["ptyId"], "pty-a");
        assert_eq!(recorded[0].1["agentSessionId"], "agent-a");
        assert_eq!(recorded[0].1["reason"], "turn-complete");
        assert_eq!(recorded[0].1["body"], "PR #42 is ready for review");
        let metrics = diagnostics.lock().expect("diagnostics");
        assert_eq!(metrics.records_scanned, 4);
        assert_eq!(
            metrics
                .notifications_emitted
                .get(&AgentNotificationReason::TurnComplete),
            Some(&1)
        );
    }

    #[test]
    fn codex_live_terminal_event_after_mid_turn_registration_is_not_suppressed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("rollout.jsonl");
        std::fs::write(
            &source,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"agent-a\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"live\"}}\n",
            ),
        )
        .expect("seed active turn");
        let cursor = std::fs::metadata(&source).expect("metadata").len();
        let mut registration = Registration::new(
            "pty-a".to_string(),
            1,
            NotificationProvider::Codex,
            source.clone(),
            cursor,
        );
        let diagnostics = Arc::new(Mutex::new(MutableDiagnostics::default()));
        let sink = FakeEventSink::new();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(source)
            .expect("open rollout");
        writeln!(
            file,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"live\"}}}}"
        )
        .expect("append terminal event");
        file.flush().expect("flush rollout");

        scan_source(&mut registration, &diagnostics, &sink).expect("scan terminal event");

        assert_eq!(sink.count("agent-notification"), 1);
        assert_eq!(sink.recorded()[0].1["reason"], "turn-complete");
    }

    #[test]
    fn codex_abort_clears_the_turn_without_notifying() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("rollout.jsonl");
        std::fs::write(&source, "").expect("create rollout");
        let mut registration = Registration::new(
            "pty-a".to_string(),
            1,
            NotificationProvider::Codex,
            source,
            0,
        );
        registration.turn_active = true;
        registration.turn_id = Some("live".to_string());
        registration.notification_body = Some("partial answer".to_string());
        registration.pending_opencode_idle = Some(PendingOpenCodeIdle {
            occurred_at: 1,
            agent_session_id: None,
        });
        let diagnostics = Arc::new(Mutex::new(MutableDiagnostics::default()));
        let sink = FakeEventSink::new();

        let signal = classify_codex(
            br#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"live","reason":"interrupted"}}"#,
        )
        .expect("valid Codex abort");
        apply_signal(&mut registration, signal, &diagnostics, &sink);

        assert_eq!(sink.count("agent-notification"), 0);
        assert!(!registration.turn_active);
        assert_eq!(registration.turn_id, None);
        assert_eq!(registration.notification_body, None);
        assert!(registration.pending_opencode_idle.is_none());
    }

    #[test]
    fn claude_hook_stream_emits_one_live_completion() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("attention.jsonl");
        std::fs::write(&source, "").expect("create hook stream");
        let mut registration = Registration::new(
            "pty-claude".to_string(),
            1,
            NotificationProvider::ClaudeCode,
            source.clone(),
            0,
        );
        let diagnostics = Arc::new(Mutex::new(MutableDiagnostics::default()));
        let sink = FakeEventSink::new();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(source)
            .expect("open hook stream");
        writeln!(file, r#"{{"hook_event_name":"UserPromptSubmit"}}"#).expect("append turn start");
        writeln!(file, r#"{{"hook_event_name":"Stop"}}"#).expect("append stop");
        file.flush().expect("flush hook stream");

        scan_source(&mut registration, &diagnostics, &sink).expect("scan hook stream");

        assert_eq!(sink.count("agent-notification"), 1);
        assert_eq!(sink.recorded()[0].1["reason"], "turn-complete");
        assert_eq!(sink.recorded()[0].1["title"], "Claude finished");
        assert!(sink.recorded()[0].1["body"].is_null());
    }

    #[test]
    fn kimi_completion_uses_the_latest_assistant_text_part() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("wire.jsonl");
        std::fs::write(&source, "").expect("create wire");
        let mut registration = Registration::new(
            "pty-kimi".to_string(),
            1,
            NotificationProvider::Kimi,
            source.clone(),
            0,
        );
        let diagnostics = Arc::new(Mutex::new(MutableDiagnostics::default()));
        let sink = FakeEventSink::new();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(source)
            .expect("open wire");
        writeln!(
            file,
            r#"{{"type":"turn.prompt","origin":{{"kind":"user"}}}}"#
        )
        .expect("append prompt");
        writeln!(
            file,
            r#"{{"type":"context.append_loop_event","event":{{"type":"content.part","part":{{"type":"text","text":"I will inspect the repository"}}}}}}"#
        )
        .expect("append text");
        writeln!(
            file,
            r#"{{"type":"context.append_loop_event","event":{{"type":"content.part","part":{{"type":"text","text":"PR #43 is ready for review"}}}}}}"#
        )
        .expect("append final text");
        writeln!(
            file,
            r#"{{"type":"context.append_loop_event","event":{{"type":"step.end","finishReason":"end_turn"}}}}"#
        )
        .expect("append completion");
        file.flush().expect("flush wire");

        scan_source(&mut registration, &diagnostics, &sink).expect("scan wire");

        assert_eq!(sink.count("agent-notification"), 1);
        assert_eq!(sink.recorded()[0].1["body"], "PR #43 is ready for review");
    }

    #[test]
    fn opencode_waits_for_assistant_text_before_completion() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("session.jsonl");
        std::fs::write(&source, "").expect("create bridge stream");
        let mut registration = Registration::new(
            "pty-opencode".to_string(),
            1,
            NotificationProvider::OpenCode,
            source.clone(),
            0,
        );
        let diagnostics = Arc::new(Mutex::new(MutableDiagnostics::default()));
        let sink = FakeEventSink::new();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(source)
            .expect("open bridge stream");
        for line in [
            r#"{"v":1,"ts":1,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"busy"}}}"#,
            r#"{"v":1,"ts":2,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"idle"}}}"#,
            r#"{"v":1,"ts":3,"kind":"event","type":"assistant.text","data":{"sessionID":"ses1","text":"PR #44 is ready for review"}}"#,
            r#"{"v":1,"ts":4,"kind":"event","type":"session.idle","data":{"sessionID":"ses1"}}"#,
        ] {
            writeln!(file, "{line}").expect("append bridge line");
        }
        file.flush().expect("flush bridge stream");

        scan_source(&mut registration, &diagnostics, &sink).expect("scan bridge stream");

        assert_eq!(sink.count("agent-notification"), 1);
        assert_eq!(sink.recorded()[0].1["body"], "PR #44 is ready for review");
    }

    #[test]
    fn opencode_status_idle_completes_without_assistant_text() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("session.jsonl");
        std::fs::write(&source, "").expect("create bridge stream");
        let mut registration = Registration::new(
            "pty-opencode".to_string(),
            1,
            NotificationProvider::OpenCode,
            source.clone(),
            0,
        );
        let diagnostics = Arc::new(Mutex::new(MutableDiagnostics::default()));
        let sink = FakeEventSink::new();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(source)
            .expect("open bridge stream");
        for line in [
            r#"{"v":1,"ts":1,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"busy"}}}"#,
            r#"{"v":1,"ts":2,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"idle"}}}"#,
        ] {
            writeln!(file, "{line}").expect("append bridge line");
        }
        file.flush().expect("flush bridge stream");

        scan_source(&mut registration, &diagnostics, &sink).expect("scan bridge stream");

        assert_eq!(sink.count("agent-notification"), 1);
        assert_eq!(sink.recorded()[0].1["reason"], "turn-complete");
        assert_eq!(sink.recorded()[0].1["body"], serde_json::Value::Null);
    }

    #[test]
    fn opencode_next_busy_flushes_previous_pending_idle() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("session.jsonl");
        std::fs::write(&source, "").expect("create bridge stream");
        let mut registration = Registration::new(
            "pty-opencode".to_string(),
            1,
            NotificationProvider::OpenCode,
            source.clone(),
            0,
        );
        let diagnostics = Arc::new(Mutex::new(MutableDiagnostics::default()));
        let sink = FakeEventSink::new();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(source)
            .expect("open bridge stream");
        for line in [
            r#"{"v":1,"ts":1,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"busy"}}}"#,
            r#"{"v":1,"ts":2,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"idle"}}}"#,
            r#"{"v":1,"ts":3,"kind":"event","type":"session.status","data":{"sessionID":"ses1","status":{"type":"busy"}}}"#,
        ] {
            writeln!(file, "{line}").expect("append bridge line");
        }
        file.flush().expect("flush bridge stream");

        scan_source(&mut registration, &diagnostics, &sink).expect("scan bridge stream");

        assert_eq!(sink.count("agent-notification"), 1);
        assert_eq!(sink.recorded()[0].1["reason"], "turn-complete");
        assert_eq!(sink.recorded()[0].1["body"], serde_json::Value::Null);
    }

    #[test]
    fn semantic_attention_includes_the_next_action() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("rollout.jsonl");
        std::fs::write(&source, "").expect("create rollout");
        let mut registration = Registration::new(
            "pty-codex".to_string(),
            1,
            NotificationProvider::Codex,
            source.clone(),
            0,
        );
        let diagnostics = Arc::new(Mutex::new(MutableDiagnostics::default()));
        let sink = FakeEventSink::new();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(source)
            .expect("open rollout");
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"function_call","name":"request_user_input","call_id":"q1"}}}}"#
        )
        .expect("append question");
        file.flush().expect("flush rollout");

        scan_source(&mut registration, &diagnostics, &sink).expect("scan rollout");

        assert_eq!(sink.count("agent-notification"), 1);
        assert_eq!(sink.recorded()[0].1["body"], "Awaiting your response");
    }
}
