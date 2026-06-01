use std::collections::HashMap;
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Notify;

/// Upper bound to avoid unbounded growth when callers don't drain their inbox.
const MAX_MESSAGES_PER_THREAD: usize = 2_048;
const COLLAB_INBOX_STORE_SUBPATH: &str = "office/collab_inbox.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InboxMessage {
    pub(crate) seq: u64,
    pub(crate) sender_agent_name: String,
    pub(crate) message_id: String,
    #[serde(default)]
    pub(crate) need_reply: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reply_to_message_id: Option<String>,
    pub(crate) content: String,
}

#[derive(Default)]
struct InboxState {
    next_seq: u64,
    messages: VecDeque<InboxMessage>,
    notify: Arc<Notify>,
    required_reply_next_seq: u64,
    required_reply_obligations: HashMap<String, RequiredReplyObligation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RequiredReplyObligation {
    pub(crate) source_agent_name: String,
    pub(crate) source_thread_id: ThreadId,
    #[serde(default)]
    pub(crate) source_inbox_id: String,
    pub(crate) message_id: String,
    pub(crate) content: String,
    pub(crate) first_seen_seq: u64,
    pub(crate) reminder_count: u8,
    pub(crate) observed_by_source_wait: bool,
    #[serde(default)]
    pub(crate) resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingRequiredReplyObligation {
    pub(crate) receiver_inbox_id: String,
    pub(crate) receiver_thread_id: Option<ThreadId>,
    pub(crate) obligation: RequiredReplyObligation,
}

#[derive(Default)]
struct InboxRuntime {
    by_inbox: HashMap<String, InboxState>,
}

fn runtime() -> &'static std::sync::Mutex<InboxRuntime> {
    static RUNTIME: OnceLock<std::sync::Mutex<InboxRuntime>> = OnceLock::new();
    RUNTIME.get_or_init(|| std::sync::Mutex::new(load_runtime_from_disk().unwrap_or_default()))
}

fn store_path_override() -> &'static Mutex<Option<PathBuf>> {
    static STORE_PATH_OVERRIDE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    STORE_PATH_OVERRIDE.get_or_init(|| Mutex::new(None))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct InboxRuntimeSnapshot {
    #[serde(default)]
    by_inbox: HashMap<String, InboxStateSnapshot>,
    #[serde(default)]
    by_thread: HashMap<String, InboxStateSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct InboxStateSnapshot {
    next_seq: u64,
    messages: VecDeque<InboxMessage>,
    required_reply_next_seq: u64,
    required_reply_obligations: HashMap<String, RequiredReplyObligation>,
}

impl InboxRuntime {
    fn snapshot(&self) -> InboxRuntimeSnapshot {
        InboxRuntimeSnapshot {
            by_inbox: self
                .by_inbox
                .iter()
                .map(|(inbox_id, inbox)| {
                    (
                        inbox_id.clone(),
                        InboxStateSnapshot {
                            next_seq: inbox.next_seq,
                            messages: inbox.messages.clone(),
                            required_reply_next_seq: inbox.required_reply_next_seq,
                            required_reply_obligations: inbox.required_reply_obligations.clone(),
                        },
                    )
                })
                .collect(),
            by_thread: HashMap::new(),
        }
    }

    fn from_snapshot(snapshot: InboxRuntimeSnapshot) -> Self {
        let InboxRuntimeSnapshot {
            by_inbox,
            by_thread,
        } = snapshot;

        let mut by_inbox = by_inbox
            .into_iter()
            .map(|(inbox_id, inbox)| {
                (
                    inbox_id,
                    InboxState {
                        next_seq: inbox.next_seq,
                        messages: inbox.messages,
                        notify: Arc::new(Notify::new()),
                        required_reply_next_seq: inbox.required_reply_next_seq,
                        required_reply_obligations: inbox.required_reply_obligations,
                    },
                )
            })
            .collect::<HashMap<_, _>>();

        for (thread_id, inbox) in by_thread {
            by_inbox.entry(thread_id).or_insert_with(|| InboxState {
                next_seq: inbox.next_seq,
                messages: inbox.messages,
                notify: Arc::new(Notify::new()),
                required_reply_next_seq: inbox.required_reply_next_seq,
                required_reply_obligations: inbox.required_reply_obligations,
            });
        }

        Self { by_inbox }
    }

    fn merge_snapshot_preserving_notify(&mut self, snapshot: InboxRuntimeSnapshot) {
        let mut loaded_by_inbox = snapshot.by_inbox;
        for (thread_id, inbox) in snapshot.by_thread {
            loaded_by_inbox.entry(thread_id).or_insert(inbox);
        }

        let mut merged_by_inbox = HashMap::with_capacity(loaded_by_inbox.len());
        for (inbox_id, loaded) in loaded_by_inbox {
            let existing = self.by_inbox.remove(&inbox_id);
            let previous_next_seq = existing
                .as_ref()
                .map(|existing| existing.next_seq)
                .unwrap_or_default();
            let notify = existing
                .map(|existing| existing.notify)
                .unwrap_or_else(|| Arc::new(Notify::new()));
            let loaded_next_seq = loaded.next_seq;
            let has_new_messages = loaded_next_seq > previous_next_seq;
            merged_by_inbox.insert(
                inbox_id,
                InboxState {
                    next_seq: loaded.next_seq,
                    messages: loaded.messages,
                    notify: Arc::clone(&notify),
                    required_reply_next_seq: loaded.required_reply_next_seq,
                    required_reply_obligations: loaded.required_reply_obligations,
                },
            );
            if has_new_messages {
                notify.notify_waiters();
            }
        }

        for (inbox_id, existing) in self.by_inbox.drain() {
            merged_by_inbox.insert(inbox_id, existing);
        }

        self.by_inbox = merged_by_inbox;
    }
}

fn inbox_key_for_thread(thread_id: ThreadId) -> String {
    thread_id.to_string()
}

fn append_to_inbox(
    receiver_inbox_id: &str,
    sender_agent_name: String,
    message_id: String,
    need_reply: bool,
    reply_to_message_id: Option<String>,
    content: String,
) -> u64 {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = state
        .by_inbox
        .entry(receiver_inbox_id.to_string())
        .or_default();
    inbox.next_seq = inbox.next_seq.saturating_add(1);
    let seq = inbox.next_seq;

    inbox.messages.push_back(InboxMessage {
        seq,
        sender_agent_name,
        message_id,
        need_reply,
        reply_to_message_id,
        content,
    });
    while inbox.messages.len() > MAX_MESSAGES_PER_THREAD {
        inbox.messages.pop_front();
    }

    inbox.notify.notify_waiters();
    inbox.notify.notify_one();
    persist_runtime_to_disk(&state);
    seq
}

fn messages_for_inbox(receiver_inbox_id: &str) -> Vec<InboxMessage> {
    let state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state
        .by_inbox
        .get(receiver_inbox_id)
        .map(|inbox| inbox.messages.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default()
}

fn collab_inbox_store_path() -> Option<PathBuf> {
    if let Some(path) = store_path_override()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return Some(path);
    }
    if let Some(path) = env::var_os("AI_OFFICE_COLLAB_INBOX") {
        return Some(PathBuf::from(path));
    }
    crate::config::find_codex_home()
        .ok()
        .map(|codex_home| codex_home.join(COLLAB_INBOX_STORE_SUBPATH))
}

fn load_runtime_from_disk() -> Option<InboxRuntime> {
    let path = collab_inbox_store_path()?;
    let raw = fs::read_to_string(&path).ok()?;
    let snapshot: InboxRuntimeSnapshot = serde_json::from_str(&raw).ok()?;
    Some(InboxRuntime::from_snapshot(snapshot))
}

fn load_snapshot_from_disk() -> Option<InboxRuntimeSnapshot> {
    let path = collab_inbox_store_path()?;
    let raw = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn persist_runtime_to_disk(state: &InboxRuntime) {
    let Some(path) = collab_inbox_store_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let body = match serde_json::to_string_pretty(&state.snapshot()) {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize collab inbox snapshot");
            return;
        }
    };
    let tmp_path = path.with_extension("tmp");
    if fs::write(&tmp_path, body).is_ok() {
        let _ = fs::rename(&tmp_path, &path);
    }
}

pub(crate) fn append_message(
    receiver_thread_id: ThreadId,
    sender_agent_name: String,
    message_id: String,
    reply_to_message_id: Option<String>,
    content: String,
) -> u64 {
    append_to_inbox(
        &inbox_key_for_thread(receiver_thread_id),
        sender_agent_name,
        message_id,
        false,
        reply_to_message_id,
        content,
    )
}

pub(crate) fn append_message_with_reply_requirement(
    receiver_thread_id: ThreadId,
    sender_agent_name: String,
    message_id: String,
    need_reply: bool,
    reply_to_message_id: Option<String>,
    content: String,
) -> u64 {
    append_to_inbox(
        &inbox_key_for_thread(receiver_thread_id),
        sender_agent_name,
        message_id,
        need_reply,
        reply_to_message_id,
        content,
    )
}

pub(crate) fn configure_store_path(path: impl Into<PathBuf>) {
    {
        let mut override_path = store_path_override()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *override_path = Some(path.into());
    }

    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *state = load_runtime_from_disk().unwrap_or_default();
}

pub(crate) fn reload_inbox_from_disk() {
    if let Ok(mut state) = runtime().lock() {
        if let Some(snapshot) = load_snapshot_from_disk() {
            state.merge_snapshot_preserving_notify(snapshot);
        }
    }
}

pub(crate) fn append_logical_message(
    receiver_id: &str,
    sender_agent_name: String,
    message_id: String,
    need_reply: bool,
    reply_to_message_id: Option<String>,
    content: String,
) -> u64 {
    append_to_inbox(
        receiver_id,
        sender_agent_name,
        message_id,
        need_reply,
        reply_to_message_id,
        content,
    )
}

pub(crate) fn subscribe(receiver_thread_id: ThreadId) -> (u64, Arc<Notify>) {
    subscribe_logical(&inbox_key_for_thread(receiver_thread_id))
}

pub(crate) fn subscribe_logical(receiver_id: &str) -> (u64, Arc<Notify>) {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = state.by_inbox.entry(receiver_id.to_string()).or_default();
    (inbox.next_seq, Arc::clone(&inbox.notify))
}

pub(crate) fn pop_next_message(receiver_thread_id: ThreadId) -> (u64, Option<InboxMessage>) {
    pop_message_matching(&inbox_key_for_thread(receiver_thread_id), |_| true)
}

pub(crate) fn pop_message_from_sender(
    receiver_thread_id: ThreadId,
    sender_agent_name: &str,
) -> (u64, Option<InboxMessage>) {
    pop_message_matching(&inbox_key_for_thread(receiver_thread_id), |message| {
        message.sender_agent_name == sender_agent_name
    })
}

pub(crate) fn pop_next_logical(receiver_id: &str) -> (u64, Option<InboxMessage>) {
    pop_message_matching(receiver_id, |_| true)
}

pub(crate) fn pop_logical_message_from_sender(
    receiver_id: &str,
    sender_agent_name: &str,
) -> (u64, Option<InboxMessage>) {
    pop_message_matching(receiver_id, |message| {
        message.sender_agent_name == sender_agent_name
    })
}

fn pop_message_matching(
    receiver_id: &str,
    predicate: impl Fn(&InboxMessage) -> bool,
) -> (u64, Option<InboxMessage>) {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = state.by_inbox.entry(receiver_id.to_string()).or_default();
    let max_seq = inbox.next_seq;
    let Some(index) = inbox.messages.iter().position(predicate) else {
        return (max_seq, None);
    };
    let message = inbox.messages.remove(index);
    persist_runtime_to_disk(&state);
    (max_seq, message)
}

pub(crate) fn register_required_reply(
    receiver_thread_id: ThreadId,
    source_agent_name: String,
    source_thread_id: ThreadId,
    message_id: String,
    content: String,
) {
    register_required_reply_for_inbox(
        &inbox_key_for_thread(receiver_thread_id),
        source_thread_id.to_string(),
        source_agent_name,
        source_thread_id,
        message_id,
        content,
    );
}

pub(crate) fn register_required_reply_logical(
    receiver_id: &str,
    source_agent_name: String,
    source_thread_id: ThreadId,
    message_id: String,
    content: String,
) {
    register_required_reply_for_inbox(
        receiver_id,
        source_thread_id.to_string(),
        source_agent_name,
        source_thread_id,
        message_id,
        content,
    );
}

pub(crate) fn register_required_reply_logical_from_source(
    receiver_id: &str,
    source_inbox_id: String,
    source_agent_name: String,
    source_thread_id: ThreadId,
    message_id: String,
    content: String,
) {
    register_required_reply_for_inbox(
        receiver_id,
        source_inbox_id,
        source_agent_name,
        source_thread_id,
        message_id,
        content,
    );
}

fn register_required_reply_for_inbox(
    receiver_id: &str,
    source_inbox_id: String,
    source_agent_name: String,
    source_thread_id: ThreadId,
    message_id: String,
    content: String,
) {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = state.by_inbox.entry(receiver_id.to_string()).or_default();
    if inbox.required_reply_obligations.contains_key(&message_id) {
        return;
    }

    inbox.required_reply_next_seq = inbox.required_reply_next_seq.saturating_add(1);
    let first_seen_seq = inbox.required_reply_next_seq;
    inbox.required_reply_obligations.insert(
        message_id.clone(),
        RequiredReplyObligation {
            source_agent_name,
            source_thread_id,
            source_inbox_id,
            message_id,
            content,
            first_seen_seq,
            reminder_count: 0,
            observed_by_source_wait: false,
            resolved: false,
        },
    );
    persist_runtime_to_disk(&state);
}

pub(crate) fn has_required_reply_obligation(
    receiver_thread_id: ThreadId,
    message_id: &str,
) -> bool {
    has_required_reply_obligation_logical(&inbox_key_for_thread(receiver_thread_id), message_id)
}

pub(crate) fn has_required_reply_obligation_logical(receiver_id: &str, message_id: &str) -> bool {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = state.by_inbox.entry(receiver_id.to_string()).or_default();
    inbox
        .required_reply_obligations
        .get(message_id)
        .is_some_and(|obligation| !obligation.resolved)
}

pub(crate) fn required_reply_resolved_logical(receiver_id: &str, message_id: &str) -> Option<bool> {
    let state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state
        .by_inbox
        .get(receiver_id)
        .and_then(|inbox| inbox.required_reply_obligations.get(message_id))
        .map(|obligation| obligation.resolved)
}

pub(crate) fn resolve_required_reply(
    receiver_thread_id: ThreadId,
    source_thread_id: ThreadId,
    reply_to_message_id: &str,
) -> bool {
    resolve_required_reply_logical(
        &inbox_key_for_thread(receiver_thread_id),
        source_thread_id,
        reply_to_message_id,
    )
}

pub(crate) fn resolve_required_reply_logical(
    receiver_id: &str,
    source_thread_id: ThreadId,
    reply_to_message_id: &str,
) -> bool {
    resolve_required_reply_logical_from_source_id(
        receiver_id,
        &source_thread_id.to_string(),
        reply_to_message_id,
    )
}

pub(crate) fn resolve_required_reply_logical_from_source_id(
    receiver_id: &str,
    source_inbox_id: &str,
    reply_to_message_id: &str,
) -> bool {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = state.by_inbox.entry(receiver_id.to_string()).or_default();
    let matches = inbox
        .required_reply_obligations
        .get(reply_to_message_id)
        .is_some_and(|obligation| {
            obligation_source_inbox_id(obligation) == source_inbox_id && !obligation.resolved
        });
    if matches {
        if let Some(obligation) = inbox
            .required_reply_obligations
            .get_mut(reply_to_message_id)
        {
            obligation.resolved = true;
        }
        persist_runtime_to_disk(&state);
    }
    matches
}

pub(crate) fn resolve_required_reply_logical_any_source(
    receiver_id: &str,
    reply_to_message_id: &str,
) -> bool {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = state.by_inbox.entry(receiver_id.to_string()).or_default();
    let matches = inbox
        .required_reply_obligations
        .get(reply_to_message_id)
        .is_some_and(|obligation| !obligation.resolved);
    if matches {
        if let Some(obligation) = inbox
            .required_reply_obligations
            .get_mut(reply_to_message_id)
        {
            obligation.resolved = true;
        }
        persist_runtime_to_disk(&state);
    }
    matches
}

pub(crate) fn has_resolved_required_reply(
    receiver_thread_id: ThreadId,
    source_thread_id: ThreadId,
    reply_to_message_id: &str,
) -> bool {
    has_resolved_required_reply_logical(
        &inbox_key_for_thread(receiver_thread_id),
        source_thread_id,
        reply_to_message_id,
    )
}

pub(crate) fn has_resolved_required_reply_logical(
    receiver_id: &str,
    source_thread_id: ThreadId,
    reply_to_message_id: &str,
) -> bool {
    has_resolved_required_reply_logical_from_source_id(
        receiver_id,
        &source_thread_id.to_string(),
        reply_to_message_id,
    )
}

pub(crate) fn has_resolved_required_reply_logical_from_source_id(
    receiver_id: &str,
    source_inbox_id: &str,
    reply_to_message_id: &str,
) -> bool {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = state.by_inbox.entry(receiver_id.to_string()).or_default();
    inbox
        .required_reply_obligations
        .get(reply_to_message_id)
        .is_some_and(|obligation| {
            obligation_source_inbox_id(obligation) == source_inbox_id && obligation.resolved
        })
}

fn obligation_source_inbox_id(obligation: &RequiredReplyObligation) -> &str {
    obligation.source_inbox_id.trim()
}

#[cfg(test)]
pub(crate) fn unresolved_required_reply_obligations(
    receiver_thread_id: ThreadId,
) -> Vec<RequiredReplyObligation> {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = state
        .by_inbox
        .entry(inbox_key_for_thread(receiver_thread_id))
        .or_default();
    let mut obligations = inbox
        .required_reply_obligations
        .values()
        .filter(|obligation| !obligation.resolved)
        .cloned()
        .collect::<Vec<_>>();
    obligations.sort_unstable_by(|left, right| {
        left.first_seen_seq
            .cmp(&right.first_seen_seq)
            .then_with(|| left.message_id.cmp(&right.message_id))
    });
    obligations
}

pub(crate) fn unresolved_required_reply_obligations_from_source(
    source_thread_id: ThreadId,
) -> Vec<PendingRequiredReplyObligation> {
    let state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut obligations = state
        .by_inbox
        .iter()
        .flat_map(|(receiver_inbox_id, inbox)| {
            let receiver_thread_id = ThreadId::from_string(receiver_inbox_id).ok();
            let source_inbox_id = source_thread_id.to_string();
            inbox
                .required_reply_obligations
                .values()
                .filter(move |obligation| {
                    obligation_source_inbox_id(obligation) == source_inbox_id
                        && !obligation.resolved
                })
                .cloned()
                .map(move |obligation| PendingRequiredReplyObligation {
                    receiver_inbox_id: receiver_inbox_id.clone(),
                    receiver_thread_id,
                    obligation,
                })
        })
        .collect::<Vec<_>>();
    obligations.sort_unstable_by(|left, right| {
        left.obligation
            .first_seen_seq
            .cmp(&right.obligation.first_seen_seq)
            .then_with(|| left.obligation.message_id.cmp(&right.obligation.message_id))
            .then_with(|| left.receiver_inbox_id.cmp(&right.receiver_inbox_id))
    });
    obligations
}

pub(crate) fn mark_required_reply_obligations_observed_by_source_wait(
    source_thread_id: ThreadId,
) -> usize {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut marked = 0usize;
    for inbox in state.by_inbox.values_mut() {
        for obligation in inbox.required_reply_obligations.values_mut() {
            if obligation.source_thread_id == source_thread_id
                && !obligation.resolved
                && !obligation.observed_by_source_wait
            {
                obligation.observed_by_source_wait = true;
                marked = marked.saturating_add(1);
            }
        }
    }
    persist_runtime_to_disk(&state);
    marked
}

pub(crate) fn claim_required_reply_reminders_from_source_thread(
    source_thread_id: ThreadId,
    max_reminders: u8,
) -> Vec<PendingRequiredReplyObligation> {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut remindable = state
        .by_inbox
        .iter()
        .flat_map(|(receiver_inbox_id, inbox)| {
            inbox
                .required_reply_obligations
                .iter()
                .filter_map(move |(message_id, obligation)| {
                    (obligation.source_thread_id == source_thread_id
                        && !obligation.resolved
                        && !obligation.observed_by_source_wait
                        && obligation.reminder_count < max_reminders)
                        .then_some((
                            obligation.first_seen_seq,
                            receiver_inbox_id.clone(),
                            message_id.clone(),
                        ))
                })
        })
        .collect::<Vec<_>>();
    remindable.sort_unstable_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.1.cmp(&right.1))
    });

    let mut reminders = Vec::with_capacity(remindable.len());
    for (_, receiver_inbox_id, message_id) in remindable {
        if let Some(inbox) = state.by_inbox.get_mut(&receiver_inbox_id)
            && let Some(obligation) = inbox.required_reply_obligations.get_mut(&message_id)
            && obligation.source_thread_id == source_thread_id
            && !obligation.resolved
            && !obligation.observed_by_source_wait
            && obligation.reminder_count < max_reminders
        {
            obligation.reminder_count = obligation.reminder_count.saturating_add(1);
            reminders.push(PendingRequiredReplyObligation {
                receiver_thread_id: ThreadId::from_string(&receiver_inbox_id).ok(),
                receiver_inbox_id,
                obligation: obligation.clone(),
            });
        }
    }
    persist_runtime_to_disk(&state);
    reminders
}

pub(crate) fn clear_required_reply_obligations(receiver_thread_id: ThreadId) -> usize {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(inbox) = state
        .by_inbox
        .get_mut(&inbox_key_for_thread(receiver_thread_id))
    else {
        return 0;
    };

    let cleared = inbox.required_reply_obligations.len();
    inbox.required_reply_obligations.clear();
    persist_runtime_to_disk(&state);
    cleared
}

pub(crate) fn clear_required_reply_obligations_for_source_thread(
    source_thread_id: ThreadId,
) -> usize {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut cleared: usize = 0;
    for inbox in state.by_inbox.values_mut() {
        let before = inbox.required_reply_obligations.len();
        inbox
            .required_reply_obligations
            .retain(|_, obligation| obligation.source_thread_id != source_thread_id);
        cleared =
            cleared.saturating_add(before.saturating_sub(inbox.required_reply_obligations.len()));
    }
    persist_runtime_to_disk(&state);
    cleared
}

#[allow(dead_code)]
pub(crate) fn claim_required_reply_reminders(
    receiver_thread_id: ThreadId,
    max_reminders: u8,
) -> Vec<RequiredReplyObligation> {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = state
        .by_inbox
        .entry(inbox_key_for_thread(receiver_thread_id))
        .or_default();
    let mut remindable_ids = inbox
        .required_reply_obligations
        .iter()
        .filter_map(|(message_id, obligation)| {
            (!obligation.resolved && obligation.reminder_count < max_reminders)
                .then_some((obligation.first_seen_seq, message_id.clone()))
        })
        .collect::<Vec<_>>();
    remindable_ids
        .sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    let mut reminders = Vec::with_capacity(remindable_ids.len());
    for (_, message_id) in remindable_ids {
        if let Some(obligation) = inbox.required_reply_obligations.get_mut(&message_id)
            && !obligation.resolved
            && obligation.reminder_count < max_reminders
        {
            obligation.reminder_count = obligation.reminder_count.saturating_add(1);
            reminders.push(obligation.clone());
        }
    }
    persist_runtime_to_disk(&state);
    reminders
}

pub(crate) fn messages_for(receiver_thread_id: ThreadId) -> Vec<InboxMessage> {
    messages_for_inbox(&inbox_key_for_thread(receiver_thread_id))
}

pub(crate) fn messages_for_logical(receiver_id: &str) -> Vec<InboxMessage> {
    messages_for_inbox(receiver_id)
}

pub(crate) fn unresolved_required_reply_obligations_for_source(
    source_thread_id: ThreadId,
) -> Vec<PendingRequiredReplyObligation> {
    unresolved_required_reply_obligations_from_source(source_thread_id)
}

pub(crate) fn claim_unobserved_required_reply_reminders(
    receiver_thread_id: ThreadId,
    max_reminders: u8,
) -> Vec<RequiredReplyObligation> {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = state
        .by_inbox
        .entry(inbox_key_for_thread(receiver_thread_id))
        .or_default();
    let mut remindable_ids = inbox
        .required_reply_obligations
        .iter()
        .filter_map(|(message_id, obligation)| {
            (!obligation.resolved
                && !obligation.observed_by_source_wait
                && obligation.reminder_count < max_reminders)
                .then_some((obligation.first_seen_seq, message_id.clone()))
        })
        .collect::<Vec<_>>();
    remindable_ids
        .sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    let mut reminders = Vec::with_capacity(remindable_ids.len());
    for (_, message_id) in remindable_ids {
        if let Some(obligation) = inbox.required_reply_obligations.get_mut(&message_id)
            && !obligation.resolved
            && !obligation.observed_by_source_wait
            && obligation.reminder_count < max_reminders
        {
            obligation.reminder_count = obligation.reminder_count.saturating_add(1);
            reminders.push(obligation.clone());
        }
    }
    persist_runtime_to_disk(&state);
    reminders
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut state = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.by_inbox.clear();
    if let Some(path) = collab_inbox_store_path() {
        let _ = fs::remove_file(path);
    }
    let mut override_path = store_path_override()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *override_path = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use serial_test::serial;

    #[test]
    #[serial(collab_inbox)]
    fn required_reply_lifecycle_registers_and_resolves_by_message_id() {
        reset_for_tests();
        let receiver = ThreadId::new();
        let source_thread = ThreadId::new();

        register_required_reply(
            receiver,
            "sender-a".to_string(),
            source_thread,
            "msg-1".to_string(),
            "first".to_string(),
        );
        register_required_reply(
            receiver,
            "sender-b".to_string(),
            ThreadId::new(),
            "msg-2".to_string(),
            "second".to_string(),
        );

        let unresolved = unresolved_required_reply_obligations(receiver);
        assert_eq!(unresolved.len(), 2);
        assert_eq!(unresolved[0].message_id, "msg-1");
        assert_eq!(unresolved[1].message_id, "msg-2");

        assert!(resolve_required_reply(receiver, source_thread, "msg-1"));
        assert!(has_resolved_required_reply(
            receiver,
            source_thread,
            "msg-1"
        ));
        assert!(!has_required_reply_obligation(receiver, "msg-1"));
        let unresolved = unresolved_required_reply_obligations(receiver);
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].message_id, "msg-2");
        assert_eq!(unresolved[0].source_agent_name, "sender-b");
    }

    #[test]
    #[serial(collab_inbox)]
    fn required_reply_resolution_noops_when_source_thread_mismatches() {
        reset_for_tests();
        let receiver = ThreadId::new();
        let source_thread = ThreadId::new();
        register_required_reply(
            receiver,
            "sender-a".to_string(),
            source_thread,
            "msg-1".to_string(),
            "first".to_string(),
        );

        assert!(!resolve_required_reply(receiver, ThreadId::new(), "msg-1"));
        let unresolved = unresolved_required_reply_obligations(receiver);
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].message_id, "msg-1");
    }

    #[test]
    #[serial(collab_inbox)]
    fn has_required_reply_obligation_tracks_registered_ids() {
        reset_for_tests();
        let receiver = ThreadId::new();

        assert!(!has_required_reply_obligation(receiver, "msg-1"));

        register_required_reply(
            receiver,
            "sender-a".to_string(),
            ThreadId::new(),
            "msg-1".to_string(),
            "first".to_string(),
        );

        assert!(has_required_reply_obligation(receiver, "msg-1"));
        assert!(!has_required_reply_obligation(receiver, "msg-2"));
    }

    #[test]
    #[serial(collab_inbox)]
    fn claim_required_reply_reminders_is_bounded() {
        reset_for_tests();
        let receiver = ThreadId::new();

        register_required_reply(
            receiver,
            "sender-a".to_string(),
            ThreadId::new(),
            "msg-1".to_string(),
            "first".to_string(),
        );
        register_required_reply(
            receiver,
            "sender-b".to_string(),
            ThreadId::new(),
            "msg-2".to_string(),
            "second".to_string(),
        );

        let first = claim_required_reply_reminders(receiver, 2);
        assert_eq!(
            first
                .iter()
                .map(|obligation| obligation.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["msg-1", "msg-2"]
        );
        let second = claim_required_reply_reminders(receiver, 2);
        assert_eq!(
            second
                .iter()
                .map(|obligation| obligation.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["msg-1", "msg-2"]
        );
        let third = claim_required_reply_reminders(receiver, 2);
        assert!(third.is_empty());
    }

    #[test]
    #[serial(collab_inbox)]
    fn pop_next_message_preserves_fifo_order() {
        reset_for_tests();
        let receiver = ThreadId::new();
        append_message(
            receiver,
            "agent-a".to_string(),
            "m1".to_string(),
            None,
            "first".to_string(),
        );
        append_message(
            receiver,
            "agent-b".to_string(),
            "m2".to_string(),
            None,
            "second".to_string(),
        );

        let (_, first) = pop_next_message(receiver);
        let (_, second) = pop_next_message(receiver);
        let (_, third) = pop_next_message(receiver);

        assert_eq!(
            first.expect("first message").message_id,
            "m1",
            "expected oldest message first"
        );
        assert_eq!(
            second.expect("second message").message_id,
            "m2",
            "expected next message second"
        );
        assert!(third.is_none(), "queue should be empty after two pops");
    }

    #[tokio::test]
    #[serial(collab_inbox)]
    async fn reload_preserves_wait_notify_handles() {
        reset_for_tests();
        let temp = tempfile::tempdir().expect("tempdir");
        configure_store_path(temp.path().join("collab-inbox.json"));
        let receiver = ThreadId::new();
        let (_seq, notify) = subscribe(receiver);

        reload_inbox_from_disk();
        append_message(
            receiver,
            "agent-c".to_string(),
            "reply-1".to_string(),
            Some("request-1".to_string()),
            "done".to_string(),
        );

        tokio::time::timeout(std::time::Duration::from_millis(200), notify.notified())
            .await
            .expect("existing wait notify should be signalled after reload");
        let (_seq, message) = pop_message_from_sender(receiver, "agent-c");
        let message = message.expect("message should be available after notify");
        assert_eq!(message.message_id, "reply-1");
    }

    #[test]
    #[serial(collab_inbox)]
    fn clear_required_reply_obligations_only_affects_target_receiver() {
        reset_for_tests();
        let receiver_a = ThreadId::new();
        let receiver_b = ThreadId::new();

        register_required_reply(
            receiver_a,
            "sender-a".to_string(),
            ThreadId::new(),
            "msg-a1".to_string(),
            "a1".to_string(),
        );
        register_required_reply(
            receiver_b,
            "sender-b".to_string(),
            ThreadId::new(),
            "msg-b1".to_string(),
            "b1".to_string(),
        );

        let cleared = clear_required_reply_obligations(receiver_a);
        assert_eq!(cleared, 1);
        assert!(unresolved_required_reply_obligations(receiver_a).is_empty());
        assert_eq!(unresolved_required_reply_obligations(receiver_b).len(), 1);
    }

    #[test]
    #[serial(collab_inbox)]
    fn unresolved_required_reply_obligations_from_source_lists_all_targets() {
        reset_for_tests();
        let source = ThreadId::new();
        let other_source = ThreadId::new();
        let receiver_a = ThreadId::new();
        let receiver_b = ThreadId::new();

        register_required_reply(
            receiver_a,
            "sender-a".to_string(),
            source,
            "msg-a1".to_string(),
            "a1".to_string(),
        );
        register_required_reply(
            receiver_b,
            "sender-a".to_string(),
            source,
            "msg-b1".to_string(),
            "b1".to_string(),
        );
        register_required_reply(
            receiver_b,
            "sender-b".to_string(),
            other_source,
            "msg-b2".to_string(),
            "b2".to_string(),
        );

        let unresolved = unresolved_required_reply_obligations_from_source(source);
        assert_eq!(unresolved.len(), 2);
        assert_eq!(unresolved[0].receiver_thread_id, Some(receiver_a));
        assert_eq!(unresolved[0].obligation.message_id, "msg-a1");
        assert_eq!(unresolved[1].receiver_thread_id, Some(receiver_b));
        assert_eq!(unresolved[1].obligation.message_id, "msg-b1");
    }

    #[test]
    #[serial(collab_inbox)]
    fn claim_required_reply_reminders_from_source_thread_is_bounded() {
        reset_for_tests();
        let source = ThreadId::new();
        let other_source = ThreadId::new();
        let receiver_a = ThreadId::new();
        let receiver_b = ThreadId::new();

        register_required_reply(
            receiver_a,
            "sender-a".to_string(),
            source,
            "msg-a1".to_string(),
            "a1".to_string(),
        );
        register_required_reply(
            receiver_b,
            "sender-a".to_string(),
            source,
            "msg-b1".to_string(),
            "b1".to_string(),
        );
        register_required_reply(
            receiver_b,
            "sender-b".to_string(),
            other_source,
            "msg-b2".to_string(),
            "b2".to_string(),
        );

        let reminders = claim_required_reply_reminders_from_source_thread(source, 1);
        assert_eq!(reminders.len(), 2);
        assert_eq!(reminders[0].receiver_thread_id, Some(receiver_a));
        assert_eq!(reminders[0].obligation.message_id, "msg-a1");
        assert_eq!(reminders[0].obligation.reminder_count, 1);
        assert_eq!(reminders[1].receiver_thread_id, Some(receiver_b));
        assert_eq!(reminders[1].obligation.message_id, "msg-b1");
        assert_eq!(reminders[1].obligation.reminder_count, 1);

        assert!(claim_required_reply_reminders_from_source_thread(source, 1).is_empty());

        let unresolved = unresolved_required_reply_obligations_from_source(source);
        assert_eq!(unresolved.len(), 2);
        assert_eq!(unresolved[0].obligation.reminder_count, 1);
        assert_eq!(unresolved[1].obligation.reminder_count, 1);

        let unrelated = unresolved_required_reply_obligations_from_source(other_source);
        assert_eq!(unrelated.len(), 1);
        assert_eq!(unrelated[0].obligation.message_id, "msg-b2");
        assert_eq!(unrelated[0].obligation.reminder_count, 0);
    }

    #[test]
    #[serial(collab_inbox)]
    fn clear_required_reply_obligations_for_source_thread_spans_all_receivers() {
        reset_for_tests();
        let source = ThreadId::new();
        let other_source = ThreadId::new();
        let receiver_a = ThreadId::new();
        let receiver_b = ThreadId::new();

        register_required_reply(
            receiver_a,
            "sender-a".to_string(),
            source,
            "msg-a1".to_string(),
            "a1".to_string(),
        );
        register_required_reply(
            receiver_a,
            "sender-a".to_string(),
            other_source,
            "msg-a2".to_string(),
            "a2".to_string(),
        );
        register_required_reply(
            receiver_b,
            "sender-b".to_string(),
            source,
            "msg-b1".to_string(),
            "b1".to_string(),
        );

        let cleared = clear_required_reply_obligations_for_source_thread(source);
        assert_eq!(cleared, 2);

        let unresolved_a = unresolved_required_reply_obligations(receiver_a);
        assert_eq!(unresolved_a.len(), 1);
        assert_eq!(unresolved_a[0].message_id, "msg-a2");

        assert!(unresolved_required_reply_obligations(receiver_b).is_empty());
    }
}
