use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Mutex;

use serde_json::Value;

pub const SUBSCRIBER_BUFFER: usize = 256;
const TOOL_CALL_RING: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardState {
    #[default]
    Idle,
    Running,
    Attention,
    Finished,
    Error,
}

/// §2.7: control characters and tabs become single spaces, the ends are
/// trimmed, and interior whitespace is preserved — `printf 'a  b'` is a
/// different command from `printf 'a b'`, and `agent  one` is a different
/// directory from `agent one`, which §2.1's disambiguation depends on.
fn sanitise_fields(value: &mut Value, keys: &[&str]) {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(Value::as_str) {
            value[*key] = Value::String(crate::sidebar::format::sanitise(text));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum CwdSource {
    #[default]
    None,
    PaneList,
    Transcript,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PaneTelemetry {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Not serialized: provenance is daemon-internal (§1.2a).
    #[serde(skip)]
    cwd_source: CwdSource,
    /// The workspace herdr reports for this pane, from the reconcile loop.
    /// `None` means "not placed yet", which the sidebar shows rather than
    /// hides (§3.2).
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub tool_counts: BTreeMap<String, u64>,
    #[serde(default)]
    pub card_state: CardState,
    #[serde(default)]
    pub status: Option<Value>,
    #[serde(default)]
    pub lifecycle: Option<Value>,
    #[serde(default)]
    pub title: Option<Value>,
    #[serde(default)]
    pub tool_calls: VecDeque<Value>,
    #[serde(default)]
    pub tool_call_total: u64,
    #[serde(default)]
    pub updated_seq: u64,
    #[serde(skip)]
    open_tools: HashSet<String>,
    #[serde(skip)]
    settled_tools: HashSet<String>,
}

impl PaneTelemetry {
    /// Fixture constructor. The private replay-dedup fields make a struct literal
    /// impossible outside this module, and `default()` followed by field
    /// assignment trips `clippy::field_reassign_with_default` — which Task 16's
    /// clippy warns on in every file this plan creates. Every fixture starts here.
    pub fn with_agent(agent: &str) -> Self {
        Self {
            agent: Some(agent.to_string()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Update {
    pub seq: u64,
    pub pane_id: String,
    pub telemetry: Option<PaneTelemetry>,
}

#[derive(Default)]
pub struct TelemetryStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    panes: HashMap<String, PaneTelemetry>,
    seq: u64,
    next_sub_id: u64,
    subscribers: Vec<(u64, SyncSender<Update>)>,
}

impl TelemetryStore {
    pub fn set_agent(&self, pane_id: &str, agent: &str) {
        self.mutate_creating(pane_id, |entry| {
            let changed = entry.agent.as_deref() != Some(agent);
            entry.agent = Some(agent.to_string());
            changed
        });
    }

    /// Fallback cwd from herdr's pane list. Never overwrites a transcript value
    /// and never creates an entry (§1.2a, §1.3a).
    pub fn set_pane_list_cwd(&self, pane_id: &str, cwd: Option<String>) {
        let Some(cwd) = cwd else { return };
        let cwd = crate::sidebar::format::sanitise(&cwd);
        if cwd.is_empty() {
            return;
        }
        self.mutate_existing(pane_id, |entry| {
            if entry.cwd_source > CwdSource::PaneList {
                return false;
            }
            if entry.cwd.as_deref() == Some(cwd.as_str()) && entry.cwd_source == CwdSource::PaneList
            {
                return false;
            }
            entry.cwd = Some(cwd);
            entry.cwd_source = CwdSource::PaneList;
            true
        });
    }

    /// Never creates an entry: a pane with no agent has no card, and a
    /// workspace alone is not a reason to make one. Empty maps to `None` —
    /// `PaneInfo::workspace_id` is `#[serde(default)]` (`herdr/api.rs:22`), so
    /// an absent field arrives as `""`, and `Some("")` would defeat the
    /// distinction this field exists to draw.
    pub fn set_pane_workspace(&self, pane_id: &str, workspace: Option<String>) {
        let workspace = workspace.filter(|id| !id.is_empty());
        self.mutate_existing(pane_id, |entry| {
            if entry.workspace_id == workspace {
                return false;
            }
            entry.workspace_id = workspace;
            true
        });
    }

    /// Atomic session swap (§1.4). Carries only the previous cwd, demoted so the
    /// fallback can refresh it until the new session reports its own.
    pub fn replace_session(&self, pane_id: &str, agent: &str) {
        let mut inner = self.inner.lock().expect("store poisoned");
        let carried = inner.panes.get(pane_id).and_then(|e| e.cwd.clone());
        let mut fresh = PaneTelemetry::with_agent(agent);
        fresh.cwd = carried;
        fresh.cwd_source = if fresh.cwd.is_some() {
            CwdSource::PaneList
        } else {
            CwdSource::None
        };
        let seq = inner.seq + 1;
        inner.seq = seq;
        fresh.updated_seq = seq;
        inner.panes.insert(pane_id.to_string(), fresh.clone());
        let update = Update {
            seq,
            pane_id: pane_id.to_string(),
            telemetry: Some(fresh),
        };
        Self::broadcast(&mut inner, update);
    }

    pub fn record(&self, pane_id: &str, event: &str, payload: Value) {
        let mut payload = payload;
        normalise_event(event, &mut payload);

        self.mutate_existing(pane_id, |entry| match event {
            "agent-status" => {
                let changed = entry.status.as_ref() != Some(&payload);
                entry.status = Some(payload);
                changed
            }
            "agent-session-title" => {
                let changed = entry.title.as_ref() != Some(&payload);
                entry.title = Some(payload);
                changed
            }
            "agent-lifecycle" => {
                let before = entry.card_state;
                match payload["phase"].as_str() {
                    Some("running") => entry.card_state = CardState::Running,
                    Some("idle") if entry.card_state == CardState::Running => {
                        entry.card_state = CardState::Idle;
                    }
                    _ => {}
                }
                let changed =
                    before != entry.card_state || entry.lifecycle.as_ref() != Some(&payload);
                entry.lifecycle = Some(payload);
                changed
            }
            "agent-attention" => {
                let changed = entry.card_state != CardState::Attention;
                entry.card_state = CardState::Attention;
                changed
            }
            "agent-notification" => match payload["reason"].as_str() {
                Some("turn-complete") => {
                    let changed = entry.card_state != CardState::Finished;
                    entry.card_state = CardState::Finished;
                    changed
                }
                Some("agent-error") => {
                    let changed = entry.card_state != CardState::Error;
                    entry.card_state = CardState::Error;
                    changed
                }
                _ => false,
            },
            "agent-cwd" => match payload["cwd"].as_str() {
                Some(cwd) => {
                    if entry.cwd.as_deref() == Some(cwd)
                        && entry.cwd_source == CwdSource::Transcript
                    {
                        return false;
                    }
                    entry.cwd = Some(cwd.to_string());
                    entry.cwd_source = CwdSource::Transcript;
                    true
                }
                None => false,
            },
            "agent-tool-call" => match payload["status"].as_str() {
                Some("running") => {
                    if let Some(id) = payload["toolUseId"].as_str() {
                        if !entry.settled_tools.contains(id) {
                            entry.open_tools.insert(id.to_string());
                        }
                    }
                    false
                }
                Some("done") | Some("failed") => {
                    let id = payload["toolUseId"].as_str().unwrap_or_default();
                    if id.is_empty() || !entry.settled_tools.insert(id.to_string()) {
                        return false;
                    }
                    entry.open_tools.remove(id);
                    let tool = payload["tool"]
                        .as_str()
                        .filter(|t| !t.is_empty())
                        .unwrap_or("unknown")
                        .to_string();
                    entry.tool_calls.push_back(payload);
                    while entry.tool_calls.len() > TOOL_CALL_RING {
                        entry.tool_calls.pop_front();
                    }
                    entry.tool_call_total += 1;
                    *entry.tool_counts.entry(tool).or_insert(0) += 1;
                    true
                }
                _ => false,
            },
            "agent-replay-summary" => {
                let mut changed = false;
                if let Some(total) = payload["toolCallTotal"].as_u64() {
                    if entry.tool_call_total != total {
                        entry.tool_call_total = total;
                        changed = true;
                    }
                }
                if let Some(map) = payload["toolCallByType"].as_object() {
                    let counts: BTreeMap<String, u64> = map
                        .iter()
                        .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
                        .collect();
                    if entry.tool_counts != counts {
                        entry.tool_counts = counts;
                        changed = true;
                    }
                }
                if let Some(recent) = payload["recentToolCalls"].as_array() {
                    let calls: VecDeque<Value> =
                        recent.iter().take(TOOL_CALL_RING).rev().cloned().collect();
                    if entry.tool_calls != calls {
                        entry.tool_calls = calls;
                        changed = true;
                    }
                    entry.settled_tools.extend(
                        entry
                            .tool_calls
                            .iter()
                            .filter_map(|call| call["toolUseId"].as_str().map(str::to_string)),
                    );
                }
                if let Some(cwd) = payload["cwd"].as_str() {
                    if entry.cwd.as_deref() != Some(cwd)
                        || entry.cwd_source != CwdSource::Transcript
                    {
                        entry.cwd = Some(cwd.to_string());
                        entry.cwd_source = CwdSource::Transcript;
                        changed = true;
                    }
                }
                changed
            }
            _ => false,
        });
    }

    pub fn remove(&self, pane_id: &str) {
        let mut inner = self.inner.lock().expect("store poisoned");
        if inner.panes.remove(pane_id).is_some() {
            inner.seq += 1;
            let update = Update {
                seq: inner.seq,
                pane_id: pane_id.to_string(),
                telemetry: None,
            };
            Self::broadcast(&mut inner, update);
        }
    }

    pub fn subscribe_with_snapshot(
        &self,
    ) -> (Receiver<Update>, u64, HashMap<String, PaneTelemetry>) {
        let (_, receiver, seq, panes) = self.subscribe_full();
        (receiver, seq, panes)
    }

    pub fn subscribe_with_snapshot_id(&self) -> (u64, Receiver<Update>) {
        let (id, receiver, _, _) = self.subscribe_full();
        (id, receiver)
    }

    pub(crate) fn subscribe_full(
        &self,
    ) -> (u64, Receiver<Update>, u64, HashMap<String, PaneTelemetry>) {
        let mut inner = self.inner.lock().expect("store poisoned");
        let (sender, receiver) = sync_channel(SUBSCRIBER_BUFFER);
        inner.next_sub_id += 1;
        let id = inner.next_sub_id;
        inner.subscribers.push((id, sender));
        (id, receiver, inner.seq, inner.panes.clone())
    }

    pub fn unsubscribe(&self, id: u64) {
        self.inner
            .lock()
            .expect("store poisoned")
            .subscribers
            .retain(|(subscriber_id, _)| *subscriber_id != id);
    }

    pub fn snapshot(&self) -> (u64, HashMap<String, PaneTelemetry>) {
        let inner = self.inner.lock().expect("store poisoned");
        (inner.seq, inner.panes.clone())
    }

    pub fn subscriber_count(&self) -> usize {
        self.inner.lock().expect("store poisoned").subscribers.len()
    }

    /// Applies `apply` to an EXISTING entry only. Advances the sequence and
    /// broadcasts only when the closure reports a change (§1.2). Never creates:
    /// a late event after cleanup must not resurrect a card (§1.3a).
    fn mutate_existing(&self, pane_id: &str, apply: impl FnOnce(&mut PaneTelemetry) -> bool) {
        let mut inner = self.inner.lock().expect("store poisoned");
        let Some(entry) = inner.panes.get_mut(pane_id) else {
            return;
        };
        if !apply(entry) {
            return;
        }
        let seq = inner.seq + 1;
        inner.seq = seq;
        let entry = inner.panes.get_mut(pane_id).expect("checked above");
        entry.updated_seq = seq;
        let update = Update {
            seq,
            pane_id: pane_id.to_string(),
            telemetry: Some(entry.clone()),
        };
        Self::broadcast(&mut inner, update);
    }

    /// Creates the entry if absent, then applies `apply` — under ONE lock, so no
    /// snapshot and no concurrent event can observe a half-initialised card.
    /// Reachable only from the bind path (§1.3a).
    fn mutate_creating(&self, pane_id: &str, apply: impl FnOnce(&mut PaneTelemetry) -> bool) {
        let mut inner = self.inner.lock().expect("store poisoned");
        let created = !inner.panes.contains_key(pane_id);
        let entry = inner.panes.entry(pane_id.to_string()).or_default();
        let changed = apply(entry) || created;
        if !changed {
            return;
        }
        let seq = inner.seq + 1;
        inner.seq = seq;
        let entry = inner.panes.get_mut(pane_id).expect("just inserted");
        entry.updated_seq = seq;
        let update = Update {
            seq,
            pane_id: pane_id.to_string(),
            telemetry: Some(entry.clone()),
        };
        Self::broadcast(&mut inner, update);
    }

    fn broadcast(inner: &mut Inner, update: Update) {
        inner
            .subscribers
            .retain(|(_, sender)| sender.try_send(update.clone()).is_ok());
    }
}

/// §2.7: normalise the dynamic strings in an event payload. `pub(crate)` because
/// the SINK must run it before it does anything at all — it reports
/// `modelDisplayName` and the session title to herdr as pane metadata from the
/// same object it hands the store, and sanitising inside `record()` alone would
/// leave those two consumers reading the raw text. Idempotent, so `record()`
/// runs it too for callers that never pass through the sink.
pub(crate) fn normalise_event(event: &str, payload: &mut Value) {
    match event {
        "agent-status" => sanitise_fields(payload, &["modelDisplayName"]),
        "agent-session-title" => sanitise_fields(payload, &["title"]),
        "agent-cwd" => sanitise_fields(payload, &["cwd"]),
        "agent-tool-call" => sanitise_fields(payload, &["tool", "args"]),
        "agent-replay-summary" => {
            sanitise_fields(payload, &["cwd"]);
            if let Some(calls) = payload
                .get_mut("recentToolCalls")
                .and_then(Value::as_array_mut)
            {
                for call in calls {
                    sanitise_fields(call, &["tool", "args"]);
                }
            }
            if let Some(map) = payload
                .get_mut("toolCallByType")
                .and_then(Value::as_object_mut)
            {
                let mut folded = serde_json::Map::new();
                for (key, value) in map.iter() {
                    let key = crate::sidebar::format::sanitise(key);
                    if key.is_empty() {
                        continue;
                    }
                    let n = value.as_u64().unwrap_or(0);
                    let slot = folded.entry(key).or_insert_with(|| Value::from(0u64));
                    *slot = Value::from(slot.as_u64().unwrap_or(0) + n);
                }
                *map = folded;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_workspace_is_recorded_and_an_empty_one_is_not() {
        let store = TelemetryStore::default();
        store.set_agent("w4:p1", "claude");
        store.set_pane_workspace("w4:p1", Some("w4".into()));
        assert_eq!(
            store.snapshot().1["w4:p1"].workspace_id.as_deref(),
            Some("w4")
        );

        store.set_agent("w4:p2", "codex");
        store.set_pane_workspace("w4:p2", Some(String::new()));
        assert_eq!(store.snapshot().1["w4:p2"].workspace_id, None);

        store.set_pane_workspace("w4:p3", Some("w4".into()));
        assert!(
            !store.snapshot().1.contains_key("w4:p3"),
            "a workspace alone does not create a card"
        );
    }

    #[test]
    fn folds_tool_calls_by_id_and_counts_terminals_once() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.record(
            "p1",
            "agent-tool-call",
            json!({"sessionId":"p1","toolUseId":"t1","tool":"Bash","status":"running"}),
        );
        store.record(
            "p1",
            "agent-tool-call",
            json!({"sessionId":"p1","toolUseId":"t1","tool":"Bash","status":"done"}),
        );
        store.record(
            "p1",
            "agent-tool-call",
            json!({"sessionId":"p1","toolUseId":"t1","tool":"Bash","status":"done"}),
        );
        let pane = &store.snapshot().1["p1"];
        assert_eq!(pane.tool_call_total, 1);
        assert_eq!(pane.tool_calls.len(), 1);
        assert_eq!(pane.tool_calls[0]["tool"], "Bash");
    }

    #[test]
    fn replay_summary_seeds_total_ring_and_settlement() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.record(
            "p1",
            "agent-replay-summary",
            json!({
                "sessionId": "p1",
                "toolCallTotal": 12,
                "recentToolCalls": [
                    {"tool":"newest","toolUseId":"t_new"},
                    {"tool":"older","toolUseId":"t_old"}
                ]
            }),
        );
        let pane = &store.snapshot().1["p1"];
        assert_eq!(pane.tool_call_total, 12);
        assert_eq!(pane.tool_calls.back().unwrap()["tool"], "newest");

        store.record(
            "p1",
            "agent-tool-call",
            json!({"sessionId":"p1","toolUseId":"t_new","tool":"newest","status":"done"}),
        );
        let pane = &store.snapshot().1["p1"];
        assert_eq!(pane.tool_call_total, 12);
        assert_eq!(pane.tool_calls.len(), 2);
    }

    #[test]
    fn card_state_transitions_clear_sticky_states() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.record("p1", "agent-attention", json!({"ptyId":"p1"}));
        assert_eq!(store.snapshot().1["p1"].card_state, CardState::Attention);
        store.record(
            "p1",
            "agent-lifecycle",
            json!({"sessionId":"p1","phase":"running"}),
        );
        assert_eq!(store.snapshot().1["p1"].card_state, CardState::Running);
        store.record(
            "p1",
            "agent-lifecycle",
            json!({"sessionId":"p1","phase":"idle"}),
        );
        assert_eq!(store.snapshot().1["p1"].card_state, CardState::Idle);
        store.record(
            "p1",
            "agent-lifecycle",
            json!({"sessionId":"p1","phase":"running"}),
        );
        store.record(
            "p1",
            "agent-notification",
            json!({"ptyId":"p1","reason":"turn-complete"}),
        );
        assert_eq!(store.snapshot().1["p1"].card_state, CardState::Finished);
        store.record(
            "p1",
            "agent-lifecycle",
            json!({"sessionId":"p1","phase":"idle"}),
        );
        assert_eq!(store.snapshot().1["p1"].card_state, CardState::Finished);
        store.record(
            "p1",
            "agent-notification",
            json!({"ptyId":"p1","reason":"agent-error"}),
        );
        assert_eq!(store.snapshot().1["p1"].card_state, CardState::Error);
        store.record(
            "p1",
            "agent-lifecycle",
            json!({"sessionId":"p1","phase":"running"}),
        );
        assert_eq!(store.snapshot().1["p1"].card_state, CardState::Running);
    }

    #[test]
    fn awaiting_is_a_no_op() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.record("p1", "agent-attention", json!({"ptyId":"p1"}));
        store.record(
            "p1",
            "agent-lifecycle",
            json!({"sessionId":"p1","phase":"awaiting"}),
        );
        assert_eq!(store.snapshot().1["p1"].card_state, CardState::Attention);
    }

    #[test]
    fn duplicate_terminal_after_settle_does_not_double_count() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.record(
            "p1",
            "agent-tool-call",
            json!({"sessionId":"p1","toolUseId":"t1","tool":"Bash","status":"done"}),
        );
        store.record(
            "p1",
            "agent-tool-call",
            json!({"sessionId":"p1","toolUseId":"t1","tool":"Bash","status":"running"}),
        );
        store.record(
            "p1",
            "agent-tool-call",
            json!({"sessionId":"p1","toolUseId":"t1","tool":"Bash","status":"done"}),
        );
        let pane = &store.snapshot().1["p1"];
        assert_eq!(pane.tool_call_total, 1);
        assert_eq!(pane.tool_calls.len(), 1);
    }

    #[test]
    fn subscribe_with_snapshot_is_seq_consistent_and_removal_advances_seq() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.record(
            "p1",
            "agent-lifecycle",
            json!({"sessionId":"p1","phase":"running"}),
        );
        let (receiver, snapshot_seq, panes) = store.subscribe_with_snapshot();
        assert!(panes.contains_key("p1"));
        store.remove("p1");
        let update = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(update.seq > snapshot_seq);
        assert!(update.telemetry.is_none());
    }

    #[test]
    fn unsubscribe_removes_without_needing_another_event() {
        let store = TelemetryStore::default();
        let (id, _receiver) = store.subscribe_with_snapshot_id();
        assert_eq!(store.subscriber_count(), 1);
        store.unsubscribe(id);
        assert_eq!(store.subscriber_count(), 0);
    }

    #[test]
    fn dynamic_strings_are_normalised_at_the_store_boundary() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.record(
            "p1",
            "agent-tool-call",
            json!({"status": "done", "toolUseId": "t1",
                   "tool": "Ba\nsh", "args": "cargo\ttest  x "}),
        );
        store.record(
            "p1",
            "agent-replay-summary",
            json!({"toolCallByType": {"Ba\nsh": 2, "Ba\tsh": 3}}),
        );
        assert_eq!(
            store.snapshot().1["p1"].tool_counts.get("Ba sh"),
            Some(&5),
            "two spellings that normalise to one name are summed, never dropped"
        );

        store.record(
            "p1",
            "agent-status",
            json!({"modelDisplayName": "Sonnet\t4.5 "}),
        );
        assert_eq!(
            store.snapshot().1["p1"].status.as_ref().unwrap()["modelDisplayName"],
            "Sonnet 4.5",
            "the model name is line 3 of the expanded card and is rebroadcast as-is"
        );
        let pane = store.snapshot().1["p1"].clone();
        assert_eq!(
            pane.tool_counts.keys().collect::<Vec<_>>(),
            vec!["Ba sh"],
            "a newline in a tool name would break the card's line count"
        );
        assert_eq!(
            pane.tool_calls[0]["args"], "cargo test  x",
            "tabs become spaces and the ends trim, but interior spacing is data"
        );
    }

    #[test]
    fn a_replay_that_reports_what_we_already_hold_does_not_advance_the_sequence() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        let summary = json!({"toolCallTotal": 2,
                             "toolCallByType": {"Read": 1, "Bash": 1}});
        store.record("p1", "agent-replay-summary", summary.clone());
        let after_first = store.snapshot().1["p1"].updated_seq;
        store.record("p1", "agent-replay-summary", summary);
        assert_eq!(
            store.snapshot().1["p1"].updated_seq,
            after_first,
            "an identical replay must not make the pane look freshly active (§1.2)"
        );
    }

    #[test]
    fn cwd_precedence_transcript_beats_pane_list_and_provenance_upgrades() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");

        store.set_pane_list_cwd("p1", Some("/shell/dir".into()));
        assert_eq!(store.snapshot().1["p1"].cwd.as_deref(), Some("/shell/dir"));

        store.record(
            "p1",
            "agent-cwd",
            json!({"sessionId":"p1","cwd":"/work/tree"}),
        );
        assert_eq!(store.snapshot().1["p1"].cwd.as_deref(), Some("/work/tree"));

        store.set_pane_list_cwd("p1", Some("/shell/dir".into()));
        assert_eq!(store.snapshot().1["p1"].cwd.as_deref(), Some("/work/tree"));

        store.set_pane_list_cwd("p1", None);
        assert_eq!(store.snapshot().1["p1"].cwd.as_deref(), Some("/work/tree"));
    }

    #[test]
    fn provenance_only_change_is_a_real_mutation() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.set_pane_list_cwd("p1", Some("/same".into()));
        let before = store.snapshot().0;

        store.record("p1", "agent-cwd", json!({"sessionId":"p1","cwd":"/same"}));
        assert!(
            store.snapshot().0 > before,
            "provenance upgrade advances seq"
        );

        store.set_pane_list_cwd("p1", Some("/other".into()));
        assert_eq!(store.snapshot().1["p1"].cwd.as_deref(), Some("/same"));
    }

    #[test]
    fn unchanged_write_advances_nothing_and_broadcasts_nothing() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.set_pane_list_cwd("p1", Some("/dir".into()));
        let (rx_id, rx) = store.subscribe_with_snapshot_id();
        let before = store.snapshot().0;

        store.set_pane_list_cwd("p1", Some("/dir".into()));

        assert_eq!(store.snapshot().0, before, "no-op must not advance seq");
        assert!(rx.try_recv().is_err(), "no-op must not broadcast");
        store.unsubscribe(rx_id);
    }

    #[test]
    fn tool_counts_seeded_by_replay_then_advanced_by_settlement() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.record(
            "p1",
            "agent-replay-summary",
            json!({
                "sessionId": "p1",
                "toolCallTotal": 12,
                "toolCallByType": { "Edit": 8, "Bash": 4 },
                "recentToolCalls": []
            }),
        );
        assert_eq!(store.snapshot().1["p1"].tool_counts.get("Edit"), Some(&8));

        store.record(
            "p1",
            "agent-tool-call",
            json!({"sessionId":"p1","toolUseId":"t9","tool":"Edit","status":"done"}),
        );
        assert_eq!(store.snapshot().1["p1"].tool_counts.get("Edit"), Some(&9));
        assert_eq!(store.snapshot().1["p1"].tool_call_total, 13);
    }

    #[test]
    fn events_for_unknown_panes_never_create_entries() {
        let store = TelemetryStore::default();
        store.record("ghost", "agent-status", json!({"sessionId":"ghost"}));
        store.set_pane_list_cwd("ghost", Some("/dir".into()));
        assert!(
            store.snapshot().1.is_empty(),
            "only bind may create an entry"
        );
    }

    #[test]
    fn replace_session_resets_everything_but_carries_cwd() {
        let store = TelemetryStore::default();
        store.set_agent("p1", "claude");
        store.record("p1", "agent-cwd", json!({"sessionId":"p1","cwd":"/work"}));
        store.record(
            "p1",
            "agent-tool-call",
            json!({"sessionId":"p1","toolUseId":"t1","tool":"Bash","status":"done"}),
        );

        store.replace_session("p1", "codex");

        let pane = &store.snapshot().1["p1"];
        assert_eq!(pane.agent.as_deref(), Some("codex"));
        assert_eq!(pane.cwd.as_deref(), Some("/work"), "cwd carries across");
        assert_eq!(pane.tool_call_total, 0, "session-scoped state resets");
        assert!(pane.tool_counts.is_empty());

        store.set_pane_list_cwd("p1", Some("/shell".into()));
        assert_eq!(store.snapshot().1["p1"].cwd.as_deref(), Some("/shell"));
    }
}
