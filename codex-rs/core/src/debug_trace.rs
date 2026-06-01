use crate::agent::PRIMARY_AGENT_NAME;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::BackgroundEventEvent;
use codex_protocol::protocol::CollabWaitLifecycleState;
use codex_protocol::protocol::CollabWaitTargetEvent;
use codex_protocol::protocol::DebugTraceAgentKind;
use codex_protocol::protocol::DebugTraceContextChannel;
use codex_protocol::protocol::DebugTraceContextSection;
use codex_protocol::protocol::DebugTraceEntry;
use codex_protocol::protocol::DebugTraceLane;
use codex_protocol::protocol::DebugTraceLanePosition;
use codex_protocol::protocol::DebugTraceRole;
use codex_protocol::protocol::DebugTraceSnapshot;
use codex_protocol::protocol::DebugTraceSnapshotEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::user_input::UserInput;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use tracing::warn;

pub(crate) const DEBUG_TRACE_SCHEMA_VERSION: &str = "debug_trace_v1";
pub(crate) const DEBUG_TRACE_ENABLE_ENV: &str = "CODEX_DEBUG_TRACE";
pub(crate) const DEBUG_TRACE_DIR_ENV: &str = "CODEX_DEBUG_TRACE_DIR";

const DEBUG_TRACE_ROOT_SUBDIR: &str = "debug/conversations";
const DEBUG_TRACE_HISTORY_FILE: &str = "history.latest.json";
const DEBUG_TRACE_METADATA_FILE: &str = "metadata.json";
const WAIT_TARGET_PREVIEW_LIMIT: usize = 80;

#[derive(Clone, Debug)]
pub(crate) struct DebugTraceContext {
    pub(crate) conversation_id: ThreadId,
    pub(crate) turn_id: String,
    pub(crate) session_source: SessionSource,
    pub(crate) collaboration_mode_kind: ModeKind,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) base_instructions: Option<String>,
    pub(crate) initial_context_items: Option<Vec<ResponseItem>>,
    pub(crate) developer_instructions: Option<String>,
    pub(crate) user_instructions: Option<String>,
    pub(crate) agent_name: Option<String>,
    pub(crate) shared_blackboard_path: Option<PathBuf>,
}

#[derive(Default)]
struct DebugTraceRuntime {
    by_root: HashMap<ThreadId, ConversationTraceState>,
    root_by_thread: HashMap<ThreadId, ThreadId>,
    thread_source_by_thread: HashMap<ThreadId, SessionSource>,
}

#[derive(Default)]
struct ConversationTraceState {
    next_entry_seq: u64,
    entries: Vec<DebugTraceEntry>,
    context_sections: Vec<DebugTraceContextSection>,
}

#[derive(Serialize)]
struct DebugTraceMetadata {
    schema_version: String,
    conversation_id: ThreadId,
    updated_at_unix_sec: i64,
    updated_at_rfc3339_sec: String,
    entry_count: usize,
    context_section_count: usize,
    lane_count: usize,
    agent_names: Vec<String>,
}

#[derive(Clone)]
struct PendingEntry {
    agent_thread_id: ThreadId,
    agent_name: String,
    agent_kind: DebugTraceAgentKind,
    role: DebugTraceRole,
    source_event: String,
    content: String,
}

fn runtime() -> &'static Mutex<DebugTraceRuntime> {
    static RUNTIME: OnceLock<Mutex<DebugTraceRuntime>> = OnceLock::new();
    RUNTIME.get_or_init(|| Mutex::new(DebugTraceRuntime::default()))
}

pub(crate) fn trace_output_root(codex_home: &Path) -> Option<PathBuf> {
    let Ok(raw) = std::env::var(DEBUG_TRACE_ENABLE_ENV) else {
        return None;
    };
    let enabled = !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    );
    if !enabled {
        return None;
    }

    match std::env::var(DEBUG_TRACE_DIR_ENV) {
        Ok(path) if !path.trim().is_empty() => Some(PathBuf::from(path.trim())),
        _ => Some(codex_home.join(DEBUG_TRACE_ROOT_SUBDIR)),
    }
}

pub(crate) fn record_event(
    output_root: &Path,
    context: &DebugTraceContext,
    event: &EventMsg,
) -> Option<DebugTraceSnapshotEvent> {
    if matches!(event, EventMsg::DebugTraceSnapshot(_)) {
        return None;
    }

    let now_unix = Utc::now().timestamp();
    let now_rfc3339_sec = format_rfc3339_seconds(now_unix);
    let root_conversation_id;
    let snapshot;
    let metadata;

    {
        let mut guard = runtime()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        root_conversation_id = resolve_root_conversation(&mut guard, context);
        guard
            .thread_source_by_thread
            .insert(context.conversation_id, context.session_source.clone());

        let pending_entries = entries_for_event(&guard, context, event);
        if pending_entries.is_empty() {
            return None;
        }
        let state = guard.by_root.entry(root_conversation_id).or_default();

        let mut added_entries = 0usize;
        for pending in pending_entries {
            if should_skip_pending_entry(state.entries.last(), &pending) {
                continue;
            }

            state.next_entry_seq = state.next_entry_seq.saturating_add(1);
            state.entries.push(DebugTraceEntry {
                conversation_id: root_conversation_id,
                entry_seq: state.next_entry_seq,
                timestamp_unix_sec: now_unix,
                timestamp_rfc3339_sec: now_rfc3339_sec.clone(),
                agent_thread_id: pending.agent_thread_id,
                agent_name: pending.agent_name,
                agent_kind: pending.agent_kind,
                role: pending.role,
                source_event: pending.source_event,
                content: pending.content,
                lane_index: None,
            });
            added_entries = added_entries.saturating_add(1);
        }

        if added_entries == 0 {
            return None;
        }

        let lanes = build_lanes(&state.entries);
        let lane_map = lanes
            .iter()
            .map(|lane| (lane.agent_thread_id, lane.lane_index))
            .collect::<HashMap<ThreadId, u64>>();
        for entry in &mut state.entries {
            entry.lane_index = lane_map.get(&entry.agent_thread_id).copied();
        }

        snapshot = DebugTraceSnapshot {
            schema_version: DEBUG_TRACE_SCHEMA_VERSION.to_string(),
            conversation_id: root_conversation_id,
            generated_at_unix_sec: now_unix,
            generated_at_rfc3339_sec: now_rfc3339_sec.clone(),
            lanes: lanes.clone(),
            entries: state.entries.clone(),
            context_sections: state.context_sections.clone(),
        };
        metadata = DebugTraceMetadata {
            schema_version: DEBUG_TRACE_SCHEMA_VERSION.to_string(),
            conversation_id: root_conversation_id,
            updated_at_unix_sec: now_unix,
            updated_at_rfc3339_sec: now_rfc3339_sec,
            entry_count: snapshot.entries.len(),
            context_section_count: snapshot.context_sections.len(),
            lane_count: snapshot.lanes.len(),
            agent_names: lanes.into_iter().map(|lane| lane.agent_name).collect(),
        };
    }

    if let Err(err) = persist_snapshot(output_root, &snapshot, &metadata) {
        warn!(%err, "failed to persist debug trace snapshot");
    }

    Some(DebugTraceSnapshotEvent {
        conversation_id: root_conversation_id,
        snapshot,
    })
}

pub(crate) fn record_request_context(
    output_root: &Path,
    context: &DebugTraceContext,
    request_input: &[ResponseItem],
) -> Option<DebugTraceSnapshotEvent> {
    let now_unix = Utc::now().timestamp();
    let now_rfc3339_sec = format_rfc3339_seconds(now_unix);
    let root_conversation_id;
    let snapshot;
    let metadata;

    {
        let mut guard = runtime()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        root_conversation_id = resolve_root_conversation(&mut guard, context);
        guard
            .thread_source_by_thread
            .insert(context.conversation_id, context.session_source.clone());

        let sections =
            context_sections_for_request_input(context, root_conversation_id, request_input);
        if sections.is_empty() {
            return None;
        }

        let state = guard.by_root.entry(root_conversation_id).or_default();
        state.context_sections.retain(|section| {
            section.turn_id != context.turn_id || section.agent_thread_id != context.conversation_id
        });
        state.context_sections.extend(sections);

        let lanes = build_lanes(&state.entries);
        let lane_map = lanes
            .iter()
            .map(|lane| (lane.agent_thread_id, lane.lane_index))
            .collect::<HashMap<ThreadId, u64>>();
        for entry in &mut state.entries {
            entry.lane_index = lane_map.get(&entry.agent_thread_id).copied();
        }

        snapshot = DebugTraceSnapshot {
            schema_version: DEBUG_TRACE_SCHEMA_VERSION.to_string(),
            conversation_id: root_conversation_id,
            generated_at_unix_sec: now_unix,
            generated_at_rfc3339_sec: now_rfc3339_sec.clone(),
            lanes: lanes.clone(),
            entries: state.entries.clone(),
            context_sections: state.context_sections.clone(),
        };
        metadata = DebugTraceMetadata {
            schema_version: DEBUG_TRACE_SCHEMA_VERSION.to_string(),
            conversation_id: root_conversation_id,
            updated_at_unix_sec: now_unix,
            updated_at_rfc3339_sec: now_rfc3339_sec,
            entry_count: snapshot.entries.len(),
            context_section_count: snapshot.context_sections.len(),
            lane_count: snapshot.lanes.len(),
            agent_names: lanes.into_iter().map(|lane| lane.agent_name).collect(),
        };
    }

    if let Err(err) = persist_snapshot(output_root, &snapshot, &metadata) {
        warn!(%err, "failed to persist debug trace request context snapshot");
    }

    Some(DebugTraceSnapshotEvent {
        conversation_id: root_conversation_id,
        snapshot,
    })
}

fn resolve_root_conversation(
    runtime: &mut DebugTraceRuntime,
    context: &DebugTraceContext,
) -> ThreadId {
    let current = context.conversation_id;
    let root = match &context.session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) => runtime
            .root_by_thread
            .get(parent_thread_id)
            .copied()
            .unwrap_or(*parent_thread_id),
        _ => current,
    };

    runtime.root_by_thread.insert(current, root);
    root
}

fn entries_for_event(
    runtime: &DebugTraceRuntime,
    context: &DebugTraceContext,
    event: &EventMsg,
) -> Vec<PendingEntry> {
    let agent_name = resolve_agent_name(context);
    let agent_kind = resolve_agent_kind(&agent_name, &context.session_source);
    let agent_thread_id = context.conversation_id;

    match event {
        EventMsg::TurnStarted(payload) => {
            entries_for_turn_started(context, payload, agent_thread_id, agent_name, agent_kind)
        }
        EventMsg::ItemCompleted(payload) => {
            entries_for_item_completed(payload, agent_thread_id, agent_name, agent_kind)
        }
        EventMsg::McpToolCallBegin(payload) => vec![tool_entry(
            agent_thread_id,
            agent_name,
            agent_kind,
            "mcp_tool_call_begin",
            format!(
                "call_id={} server={} tool={}",
                payload.call_id, payload.invocation.server, payload.invocation.tool
            ),
        )],
        EventMsg::McpToolCallEnd(payload) => vec![tool_entry(
            agent_thread_id,
            agent_name,
            agent_kind,
            "mcp_tool_call_end",
            format!(
                "call_id={} server={} tool={} success={} duration_ms={}",
                payload.call_id,
                payload.invocation.server,
                payload.invocation.tool,
                payload.is_success(),
                payload.duration.as_millis()
            ),
        )],
        EventMsg::ExecCommandBegin(payload) => vec![tool_entry(
            agent_thread_id,
            agent_name,
            agent_kind,
            "exec_command_begin",
            format!(
                "call_id={} turn_id={} source={:?} cwd={} command={}",
                payload.call_id,
                payload.turn_id,
                payload.source,
                payload.cwd.display(),
                payload.command.join(" ")
            ),
        )],
        EventMsg::ExecCommandEnd(payload) => vec![tool_entry(
            agent_thread_id,
            agent_name,
            agent_kind,
            "exec_command_end",
            format!(
                "call_id={} turn_id={} source={:?} status={:?} exit_code={} duration_ms={} command={}",
                payload.call_id,
                payload.turn_id,
                payload.source,
                payload.status,
                payload.exit_code,
                payload.duration.as_millis(),
                payload.command.join(" ")
            ),
        )],
        EventMsg::ViewImageToolCall(payload) => vec![tool_entry(
            agent_thread_id,
            agent_name,
            agent_kind,
            "view_image_tool_call",
            format!(
                "call_id={} path={}",
                payload.call_id,
                payload.path.display()
            ),
        )],
        EventMsg::CollabAgentSpawnBegin(payload) => vec![tool_entry(
            payload.sender_thread_id,
            agent_name,
            agent_kind,
            "collab_agent_spawn_begin",
            format!(
                "call_id={} sender={} prompt={}",
                payload.call_id, payload.sender_thread_id, payload.prompt
            ),
        )],
        EventMsg::CollabAgentSpawnEnd(payload) => vec![tool_entry(
            payload.sender_thread_id,
            agent_name,
            agent_kind,
            "collab_agent_spawn_end",
            format!(
                "call_id={} sender={} new_thread_id={:?} status={:?}",
                payload.call_id, payload.sender_thread_id, payload.new_thread_id, payload.status
            ),
        )],
        EventMsg::CollabAgentInteractionBegin(payload) => vec![tool_entry(
            payload.sender_thread_id,
            agent_name,
            agent_kind,
            "collab_agent_interaction_begin",
            format!(
                "call_id={} sender={} receiver={} prompt={}",
                payload.call_id,
                payload.sender_thread_id,
                payload.receiver_thread_id,
                payload.prompt
            ),
        )],
        EventMsg::CollabAgentInteractionEnd(payload) => vec![tool_entry(
            payload.sender_thread_id,
            agent_name,
            agent_kind,
            "collab_agent_interaction_end",
            format!(
                "call_id={} sender={} receiver={} status={:?}",
                payload.call_id,
                payload.sender_thread_id,
                payload.receiver_thread_id,
                payload.status
            ),
        )],
        EventMsg::CollabWaitingBegin(payload) => vec![tool_entry(
            payload.sender_thread_id,
            payload.sender_agent_name.clone(),
            resolve_agent_kind_for_thread(
                runtime,
                payload.sender_thread_id,
                &payload.sender_agent_name,
                &context.session_source,
            ),
            "collab_waiting_begin",
            format!(
                "call_id={} target_count={} targets={}",
                payload.call_id,
                payload.targets.len(),
                summarize_wait_targets(&payload.targets)
            ),
        )],
        EventMsg::CollabWaitingEnd(payload) => vec![tool_entry(
            payload.sender_thread_id,
            payload.sender_agent_name.clone(),
            resolve_agent_kind_for_thread(
                runtime,
                payload.sender_thread_id,
                &payload.sender_agent_name,
                &context.session_source,
            ),
            "collab_waiting_end",
            format!(
                "call_id={} timed_out={} target_count={} targets={}",
                payload.call_id,
                payload.timed_out,
                payload.targets.len(),
                summarize_wait_targets(&payload.targets)
            ),
        )],
        EventMsg::CollabCloseBegin(payload) => vec![tool_entry(
            payload.sender_thread_id,
            agent_name,
            agent_kind,
            "collab_close_begin",
            format!(
                "call_id={} sender={} receiver={}",
                payload.call_id, payload.sender_thread_id, payload.receiver_thread_id
            ),
        )],
        EventMsg::CollabCloseEnd(payload) => vec![tool_entry(
            payload.sender_thread_id,
            agent_name,
            agent_kind,
            "collab_close_end",
            format!(
                "call_id={} sender={} receiver={} status={:?}",
                payload.call_id,
                payload.sender_thread_id,
                payload.receiver_thread_id,
                payload.status
            ),
        )],
        EventMsg::CollabResumeBegin(payload) => vec![tool_entry(
            payload.sender_thread_id,
            agent_name,
            agent_kind,
            "collab_resume_begin",
            format!(
                "call_id={} sender={} receiver={}",
                payload.call_id, payload.sender_thread_id, payload.receiver_thread_id
            ),
        )],
        EventMsg::CollabResumeEnd(payload) => vec![tool_entry(
            payload.sender_thread_id,
            agent_name,
            agent_kind,
            "collab_resume_end",
            format!(
                "call_id={} sender={} receiver={} status={:?}",
                payload.call_id,
                payload.sender_thread_id,
                payload.receiver_thread_id,
                payload.status
            ),
        )],
        EventMsg::AgentWorkSummary(payload) => vec![tool_entry(
            payload.thread_id,
            payload.agent_name.clone(),
            resolve_agent_kind_for_thread(
                runtime,
                payload.thread_id,
                &payload.agent_name,
                &context.session_source,
            ),
            "agent_work_summary",
            payload.summary.clone(),
        )],
        EventMsg::BackgroundEvent(BackgroundEventEvent {
            message,
            stage,
            elapsed_ms,
            timeout_ms,
            outcome,
        }) => {
            let content = if stage.is_none()
                && elapsed_ms.is_none()
                && timeout_ms.is_none()
                && outcome.is_none()
            {
                message.clone()
            } else {
                let mut parts = vec![message.clone()];
                if let Some(stage) = stage.as_deref() {
                    parts.push(format!("stage={stage}"));
                }
                if let Some(elapsed_ms) = elapsed_ms {
                    parts.push(format!("elapsed_ms={elapsed_ms}"));
                }
                if let Some(timeout_ms) = timeout_ms {
                    parts.push(format!("timeout_ms={timeout_ms}"));
                }
                if let Some(outcome) = outcome.as_deref() {
                    parts.push(format!("outcome={outcome}"));
                }
                parts.join(" | ")
            };
            vec![PendingEntry {
                agent_thread_id,
                agent_name,
                agent_kind,
                role: DebugTraceRole::System,
                source_event: "background_event".to_string(),
                content,
            }]
        }
        _ => Vec::new(),
    }
}

fn summarize_wait_targets(targets: &[CollabWaitTargetEvent]) -> String {
    if targets.is_empty() {
        return "[]".to_string();
    }

    let parts = targets
        .iter()
        .map(|target| {
            let mut part = format!(
                "{}:{}:{}",
                target.receiver_agent_name,
                target.message_id,
                wait_state_label(&target.state)
            );
            if let Some(content) = target.callback_content.as_deref() {
                let preview = summarize_wait_callback(content);
                if !preview.is_empty() {
                    part.push_str(&format!(" content={preview:?}"));
                }
            }
            part
        })
        .collect::<Vec<_>>();

    format!("[{}]", parts.join(", "))
}

fn wait_state_label(state: &CollabWaitLifecycleState) -> &'static str {
    match state {
        CollabWaitLifecycleState::Pending => "pending",
        CollabWaitLifecycleState::Running => "running",
        CollabWaitLifecycleState::Completed => "completed",
        CollabWaitLifecycleState::TimedOut => "timed_out",
        CollabWaitLifecycleState::Failed => "failed",
    }
}

fn summarize_wait_callback(content: &str) -> String {
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= WAIT_TARGET_PREVIEW_LIMIT {
        return collapsed;
    }

    let mut truncated = String::new();
    for ch in collapsed.chars() {
        let next_len = truncated.len() + ch.len_utf8();
        if next_len > WAIT_TARGET_PREVIEW_LIMIT.saturating_sub(3) {
            break;
        }
        truncated.push(ch);
    }
    truncated.push_str("...");
    truncated
}

fn entries_for_turn_started(
    context: &DebugTraceContext,
    payload: &TurnStartedEvent,
    agent_thread_id: ThreadId,
    agent_name: String,
    agent_kind: DebugTraceAgentKind,
) -> Vec<PendingEntry> {
    let mut entries = vec![PendingEntry {
        agent_thread_id,
        agent_name: agent_name.clone(),
        agent_kind: agent_kind.clone(),
        role: DebugTraceRole::System,
        source_event: "turn_started".to_string(),
        content: format!(
            "turn_id={} trace_turn_id={} mode={:?} model_context_window={:?} reasoning_effort={:?}",
            payload.turn_id,
            context.turn_id,
            context.collaboration_mode_kind,
            payload.model_context_window,
            context.reasoning_effort
        ),
    }];

    if let Some(base_instructions) = clean_optional_text(context.base_instructions.as_deref()) {
        entries.push(PendingEntry {
            agent_thread_id,
            agent_name: agent_name.clone(),
            agent_kind: agent_kind.clone(),
            role: DebugTraceRole::System,
            source_event: "initial_context.base_instructions".to_string(),
            content: base_instructions,
        });
    }

    if let Some(initial_context_items) = context.initial_context_items.as_deref() {
        entries.extend(entries_for_initial_context_items(
            initial_context_items,
            agent_thread_id,
            agent_name,
            agent_kind,
        ));
    } else {
        if let Some(user_instructions) = clean_optional_text(context.user_instructions.as_deref()) {
            entries.push(PendingEntry {
                agent_thread_id,
                agent_name: agent_name.clone(),
                agent_kind: agent_kind.clone(),
                role: DebugTraceRole::System,
                source_event: "user_instructions".to_string(),
                content: user_instructions,
            });
        }
        if let Some(developer_instructions) =
            clean_optional_text(context.developer_instructions.as_deref())
        {
            entries.push(PendingEntry {
                agent_thread_id,
                agent_name,
                agent_kind,
                role: DebugTraceRole::Developer,
                source_event: "developer_instructions".to_string(),
                content: developer_instructions,
            });
        }
    }

    entries
}

fn entries_for_item_completed(
    payload: &ItemCompletedEvent,
    agent_thread_id: ThreadId,
    agent_name: String,
    agent_kind: DebugTraceAgentKind,
) -> Vec<PendingEntry> {
    match &payload.item {
        TurnItem::UserMessage(item) => vec![PendingEntry {
            agent_thread_id,
            agent_name,
            agent_kind,
            role: DebugTraceRole::User,
            source_event: "item_completed.user_message".to_string(),
            content: render_user_inputs(&item.content),
        }],
        TurnItem::AgentMessage(item) => vec![PendingEntry {
            agent_thread_id,
            agent_name,
            agent_kind,
            role: DebugTraceRole::Assistant,
            source_event: "item_completed.agent_message".to_string(),
            content: item
                .content
                .iter()
                .map(|content| match content {
                    AgentMessageContent::Text { text } => text.as_str(),
                })
                .collect::<Vec<_>>()
                .join(""),
        }],
        TurnItem::WebSearch(item) => vec![tool_entry(
            agent_thread_id,
            agent_name,
            agent_kind,
            "item_completed.web_search",
            format!(
                "id={} query={} action={:?}",
                item.id, item.query, item.action
            ),
        )],
        _ => Vec::new(),
    }
}

fn entries_for_initial_context_items(
    items: &[ResponseItem],
    agent_thread_id: ThreadId,
    agent_name: String,
    agent_kind: DebugTraceAgentKind,
) -> Vec<PendingEntry> {
    items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            let ResponseItem::Message { role, content, .. } = item else {
                return None;
            };

            let rendered_content = render_content_items(content);
            let content = clean_optional_text(Some(rendered_content.as_str()))?;
            let debug_role = role_from_message_role(role);
            let source_event =
                classify_initial_context_source(role, rendered_content.as_str(), idx);

            Some(PendingEntry {
                agent_thread_id,
                agent_name: agent_name.clone(),
                agent_kind: agent_kind.clone(),
                role: debug_role,
                source_event,
                content,
            })
        })
        .collect()
}

fn classify_initial_context_source(role: &str, content: &str, idx: usize) -> String {
    if role == "developer" && content.contains("<permissions instructions>") {
        return "initial_context.permissions".to_string();
    }
    if role == "developer" && content.contains("<personality_spec>") {
        return "initial_context.personality".to_string();
    }
    if role == "user" && content.starts_with("<environment_context>") {
        return "initial_context.environment_context".to_string();
    }
    if role == "user"
        && (content.starts_with("# AGENTS.md instructions for ")
            || content.starts_with("<user_instructions>"))
    {
        return "initial_context.user_instructions".to_string();
    }
    if role == "user" && content.starts_with("<skill>") {
        return "initial_context.skill".to_string();
    }

    format!("initial_context.message.{idx}")
}

fn context_sections_for_request_input(
    context: &DebugTraceContext,
    root_conversation_id: ThreadId,
    request_input: &[ResponseItem],
) -> Vec<DebugTraceContextSection> {
    let agent_name = resolve_agent_name(context);
    let agent_kind = resolve_agent_kind(&agent_name, &context.session_source);
    let mut sections = Vec::new();

    for (idx, item) in request_input.iter().enumerate() {
        let ResponseItem::Message { role, content, .. } = item else {
            continue;
        };
        let rendered_content = render_content_items(content);
        let debug_role = role_from_message_role(role);
        let request_item_index = Some(idx as u64);

        if role == "developer" && rendered_content.contains("<agent_durable_context") {
            sections.extend(durable_context_sections_from_text(
                context,
                root_conversation_id,
                &agent_name,
                agent_kind.clone(),
                debug_role.clone(),
                request_item_index,
                &rendered_content,
            ));
        }

        if role == "user" && rendered_content.starts_with("<environment_context>") {
            if let Some(content) = clean_optional_text(Some(rendered_content.as_str())) {
                sections.push(context_section(
                    context,
                    root_conversation_id,
                    &agent_name,
                    agent_kind.clone(),
                    DebugTraceContextChannel::RuntimeTailInjection,
                    "turn_context.environment_context",
                    debug_role.clone(),
                    content,
                    None,
                    request_item_index,
                ));
            }
        }

        if role == "user"
            && rendered_content.starts_with("Below are the agents collaborating with you.")
        {
            sections.extend(runtime_tail_sections_from_text(
                context,
                root_conversation_id,
                &agent_name,
                agent_kind.clone(),
                debug_role,
                request_item_index,
                &rendered_content,
            ));
        }
    }

    sections
}

fn durable_context_sections_from_text(
    context: &DebugTraceContext,
    root_conversation_id: ThreadId,
    agent_name: &str,
    agent_kind: DebugTraceAgentKind,
    role: DebugTraceRole,
    request_item_index: Option<u64>,
    text: &str,
) -> Vec<DebugTraceContextSection> {
    [
        (
            "manual_permanent_system_prompt",
            DebugTraceContextChannel::ManualPermanentSystemPrompt,
            "agent_durable_context.manual_permanent_system_prompt",
        ),
        (
            "automatic_updated_prompt",
            DebugTraceContextChannel::AutomaticUpdatedPrompt,
            "agent_durable_context.automatic_updated_prompt",
        ),
    ]
    .into_iter()
    .filter_map(|(tag, channel, source_kind)| {
        extract_tag_with_source(text, tag).and_then(|(source_path, content)| {
            let content = clean_optional_text(Some(content.as_str()))?;
            Some(context_section(
                context,
                root_conversation_id,
                agent_name,
                agent_kind.clone(),
                channel,
                source_kind,
                role.clone(),
                content,
                source_path,
                request_item_index,
            ))
        })
    })
    .collect()
}

fn runtime_tail_sections_from_text(
    context: &DebugTraceContext,
    root_conversation_id: ThreadId,
    agent_name: &str,
    agent_kind: DebugTraceAgentKind,
    role: DebugTraceRole,
    request_item_index: Option<u64>,
    text: &str,
) -> Vec<DebugTraceContextSection> {
    const COLLAB_HEADER: &str = "Collaborating agents and summary lists";
    const BLACKBOARD_HEADER: &str = "Below is the shared blackboard content:\n";

    let mut sections = Vec::new();
    if let Some(collab_start) = text.find(COLLAB_HEADER) {
        let collab_text = match text.find(BLACKBOARD_HEADER) {
            Some(blackboard_start) if blackboard_start > collab_start => {
                &text[collab_start..blackboard_start]
            }
            _ => &text[collab_start..],
        };
        if let Some(content) = clean_optional_text(Some(collab_text)) {
            sections.push(context_section(
                context,
                root_conversation_id,
                agent_name,
                agent_kind.clone(),
                DebugTraceContextChannel::RuntimeTailInjection,
                "agent_control.other_agents_work_status",
                role.clone(),
                content,
                None,
                request_item_index,
            ));
        }
    }

    if let Some((_, blackboard_text)) = text.split_once(BLACKBOARD_HEADER)
        && let Some(content) = clean_optional_text(Some(blackboard_text))
    {
        sections.push(context_section(
            context,
            root_conversation_id,
            agent_name,
            agent_kind,
            DebugTraceContextChannel::RuntimeTailInjection,
            "shared_blackboard.snapshot",
            role,
            content,
            context
                .shared_blackboard_path
                .as_ref()
                .map(|path| path.display().to_string()),
            request_item_index,
        ));
    }

    sections
}

fn context_section(
    context: &DebugTraceContext,
    root_conversation_id: ThreadId,
    agent_name: &str,
    agent_kind: DebugTraceAgentKind,
    channel: DebugTraceContextChannel,
    source_kind: &str,
    role: DebugTraceRole,
    content: String,
    source_path: Option<String>,
    request_item_index: Option<u64>,
) -> DebugTraceContextSection {
    DebugTraceContextSection {
        conversation_id: root_conversation_id,
        turn_id: context.turn_id.clone(),
        agent_thread_id: context.conversation_id,
        agent_name: agent_name.to_string(),
        agent_kind,
        channel,
        source_kind: source_kind.to_string(),
        role,
        content,
        source_path,
        request_item_index,
    }
}

fn extract_tag_with_source(text: &str, tag: &str) -> Option<(Option<String>, String)> {
    let open_prefix = format!("<{tag}");
    let start = text.find(&open_prefix)?;
    let after_start = &text[start..];
    let open_end = after_start.find('>')?;
    let open_tag = &after_start[..=open_end];
    let content_start = start + open_end + 1;
    let close_tag = format!("</{tag}>");
    let close_offset = text[content_start..].find(&close_tag)?;
    let content = text[content_start..content_start + close_offset]
        .trim()
        .to_string();
    let source_path = extract_attr(open_tag, "source").map(|value| unescape_xml_attr(&value));
    Some((source_path, content))
}

fn extract_attr(open_tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let value_start = open_tag.find(&needle)? + needle.len();
    let value_end = open_tag[value_start..].find('"')?;
    Some(open_tag[value_start..value_start + value_end].to_string())
}

fn unescape_xml_attr(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn role_from_message_role(role: &str) -> DebugTraceRole {
    match role {
        "developer" => DebugTraceRole::Developer,
        "user" => DebugTraceRole::User,
        "assistant" => DebugTraceRole::Assistant,
        "system" => DebugTraceRole::System,
        _ => DebugTraceRole::System,
    }
}

fn render_content_items(items: &[ContentItem]) -> String {
    items
        .iter()
        .map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => text.clone(),
            ContentItem::InputImage { image_url } => format!("[image:{image_url}]"),
        })
        .collect::<Vec<_>>()
        .join("")
}

fn tool_entry(
    agent_thread_id: ThreadId,
    agent_name: String,
    agent_kind: DebugTraceAgentKind,
    source_event: &str,
    content: String,
) -> PendingEntry {
    PendingEntry {
        agent_thread_id,
        agent_name,
        agent_kind,
        role: DebugTraceRole::Tool,
        source_event: source_event.to_string(),
        content,
    }
}

fn should_skip_pending_entry(last: Option<&DebugTraceEntry>, pending: &PendingEntry) -> bool {
    let Some(last) = last else {
        return false;
    };

    pending.source_event == "item_completed.user_message"
        && last.source_event == pending.source_event
        && last.agent_thread_id == pending.agent_thread_id
        && last.role == pending.role
        && last.content == pending.content
}

fn resolve_agent_name(context: &DebugTraceContext) -> String {
    if let Some(name) = clean_optional_text(context.agent_name.as_deref()) {
        return name;
    }
    match &context.session_source {
        SessionSource::SubAgent(_) => context.conversation_id.to_string(),
        _ => PRIMARY_AGENT_NAME.to_string(),
    }
}

fn resolve_agent_kind(name: &str, source: &SessionSource) -> DebugTraceAgentKind {
    if name.eq_ignore_ascii_case("orchestrator")
        || name.eq_ignore_ascii_case("coordinator")
        || name.eq_ignore_ascii_case("dispatcher")
    {
        return DebugTraceAgentKind::Coordinator;
    }
    match source {
        SessionSource::SubAgent(_) => DebugTraceAgentKind::SubAgent,
        _ => DebugTraceAgentKind::Primary,
    }
}

fn resolve_agent_kind_for_thread(
    runtime: &DebugTraceRuntime,
    thread_id: ThreadId,
    agent_name: &str,
    fallback_source: &SessionSource,
) -> DebugTraceAgentKind {
    if let Some(source) = runtime.thread_source_by_thread.get(&thread_id) {
        return resolve_agent_kind(agent_name, source);
    }
    resolve_agent_kind(agent_name, fallback_source)
}

fn clean_optional_text(value: Option<&str>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn render_user_inputs(inputs: &[UserInput]) -> String {
    inputs
        .iter()
        .map(|input| match input {
            UserInput::Text { text, .. } => text.clone(),
            UserInput::Image { image_url } => format!("[image:{image_url}]"),
            UserInput::LocalImage { path } => format!("[local_image:{}]", path.display()),
            UserInput::Skill { name, path } => format!("[skill:{name}:{}]", path.display()),
            UserInput::Mention { name, path } => format!("[mention:{name}:{path}]"),
            _ => "[other_input]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("")
}

fn build_lanes(entries: &[DebugTraceEntry]) -> Vec<DebugTraceLane> {
    let mut ordered_agents: Vec<(ThreadId, String, DebugTraceAgentKind)> = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries {
        if seen.insert(entry.agent_thread_id) {
            ordered_agents.push((
                entry.agent_thread_id,
                entry.agent_name.clone(),
                entry.agent_kind.clone(),
            ));
        }
    }

    if ordered_agents.len() == 3
        && let Some(primary_idx) = ordered_agents
            .iter()
            .position(|(_, _, kind)| *kind == DebugTraceAgentKind::Primary)
    {
        let primary = ordered_agents.remove(primary_idx);
        ordered_agents.insert(1, primary);
    }

    let total = ordered_agents.len();
    ordered_agents
        .into_iter()
        .enumerate()
        .map(|(idx, (thread_id, name, _kind))| DebugTraceLane {
            agent_thread_id: thread_id,
            agent_name: name,
            lane_index: idx as u64,
            lane_position: lane_position(total, idx),
        })
        .collect()
}

fn lane_position(total: usize, idx: usize) -> DebugTraceLanePosition {
    match total {
        0 => DebugTraceLanePosition::Scroll,
        1 => DebugTraceLanePosition::Center,
        2 => {
            if idx == 0 {
                DebugTraceLanePosition::Left
            } else {
                DebugTraceLanePosition::Right
            }
        }
        3 => match idx {
            0 => DebugTraceLanePosition::Left,
            1 => DebugTraceLanePosition::Center,
            _ => DebugTraceLanePosition::Right,
        },
        _ => DebugTraceLanePosition::Scroll,
    }
}

fn persist_snapshot(
    output_root: &Path,
    snapshot: &DebugTraceSnapshot,
    metadata: &DebugTraceMetadata,
) -> std::io::Result<()> {
    let conversation_dir = output_root.join(snapshot.conversation_id.to_string());
    std::fs::create_dir_all(&conversation_dir)?;

    let history_json = serde_json::to_string_pretty(snapshot)
        .map_err(|err| std::io::Error::other(format!("serialize snapshot failed: {err}")))?;
    let metadata_json = serde_json::to_string_pretty(metadata)
        .map_err(|err| std::io::Error::other(format!("serialize metadata failed: {err}")))?;

    write_atomic_replace(
        &conversation_dir.join(DEBUG_TRACE_HISTORY_FILE),
        &history_json,
    )?;
    write_atomic_replace(
        &conversation_dir.join(DEBUG_TRACE_METADATA_FILE),
        &metadata_json,
    )?;
    Ok(())
}

fn write_atomic_replace(path: &Path, contents: &str) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path {} has no parent", path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;

    let tmp_path = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("trace"),
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&tmp_path, contents)?;
    match std::fs::rename(&tmp_path, path) {
        Ok(_) => Ok(()),
        Err(first_err) => {
            let _ = std::fs::remove_file(path);
            std::fs::rename(&tmp_path, path).map_err(|second_err| {
                std::io::Error::other(format!(
                    "atomic replace failed: {first_err}; fallback rename failed: {second_err}"
                ))
            })
        }
    }
}

fn format_rfc3339_seconds(unix_sec: i64) -> String {
    DateTime::<Utc>::from_timestamp(unix_sec, 0)
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| unix_sec.to_string())
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut guard = runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = DebugTraceRuntime::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use codex_protocol::items::AgentMessageContent;
    use codex_protocol::items::AgentMessageItem;
    use codex_protocol::items::UserMessageItem;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::AgentWorkSummaryEvent;
    use codex_protocol::protocol::CollabWaitLifecycleState;
    use codex_protocol::protocol::CollabWaitTargetEvent;
    use codex_protocol::protocol::CollabWaitingBeginEvent;
    use codex_protocol::protocol::CollabWaitingEndEvent;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::McpInvocation;
    use codex_protocol::protocol::McpToolCallBeginEvent;
    use codex_protocol::protocol::SubAgentSource;
    use codex_protocol::protocol::TurnStartedEvent;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;
    use serial_test::serial;
    use tempfile::tempdir;

    fn ctx(thread_id: ThreadId, source: SessionSource, name: Option<&str>) -> DebugTraceContext {
        DebugTraceContext {
            conversation_id: thread_id,
            turn_id: "turn-1".to_string(),
            session_source: source,
            collaboration_mode_kind: ModeKind::Swarm,
            reasoning_effort: None,
            base_instructions: Some("base inst".to_string()),
            initial_context_items: Some(vec![
                ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "<permissions instructions>\nallow\n</permissions instructions>"
                            .to_string(),
                    }],
                    end_turn: None,
                    phase: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"
                            .to_string(),
                    }],
                    end_turn: None,
                    phase: None,
                },
            ]),
            developer_instructions: Some("dev inst".to_string()),
            user_instructions: Some("user inst".to_string()),
            agent_name: name.map(str::to_string),
            shared_blackboard_path: None,
        }
    }

    #[test]
    #[serial(debug_trace)]
    fn creates_per_conversation_snapshot_files() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let thread_id = ThreadId::new();
        let snapshot = record_event(
            dir.path(),
            &ctx(thread_id, SessionSource::Cli, Some(PRIMARY_AGENT_NAME)),
            &EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
                call_id: "mcp-1".to_string(),
                invocation: McpInvocation {
                    server: "rmcp".to_string(),
                    tool: "echo".to_string(),
                    arguments: None,
                },
            }),
        )
        .expect("snapshot");

        assert_eq!(snapshot.conversation_id, thread_id);
        let conv_dir = dir.path().join(thread_id.to_string());
        assert!(conv_dir.join(DEBUG_TRACE_HISTORY_FILE).exists());
        assert!(conv_dir.join(DEBUG_TRACE_METADATA_FILE).exists());
    }

    #[test]
    #[serial(debug_trace)]
    fn merges_subagent_entries_into_root_conversation_directory() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let root = ThreadId::new();
        let child = ThreadId::new();

        let _ = record_event(
            dir.path(),
            &ctx(root, SessionSource::Cli, Some(PRIMARY_AGENT_NAME)),
            &EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "root-turn".to_string(),
                model_context_window: Some(100),
                collaboration_mode_kind: ModeKind::Default,
            }),
        )
        .expect("root");

        let child_snapshot = record_event(
            dir.path(),
            &ctx(
                child,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: root,
                    depth: 1,
                    agent_type: None,
                    agent_name_hint: None,
                }),
                Some("worker-a"),
            ),
            &EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: child,
                turn_id: "child-turn".to_string(),
                item: TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: vec![],
                }])),
            }),
        )
        .expect("child");

        assert_eq!(child_snapshot.conversation_id, root);
        assert!(dir.path().join(root.to_string()).exists());
        assert!(!dir.path().join(child.to_string()).exists());
        assert!(
            child_snapshot
                .snapshot
                .entries
                .iter()
                .any(|entry| entry.agent_name == "worker-a"),
        );
    }

    #[test]
    #[serial(debug_trace)]
    fn keeps_distinct_root_sessions_in_separate_directories() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let root_a = ThreadId::new();
        let root_b = ThreadId::new();
        let child_a = ThreadId::new();
        let child_b = ThreadId::new();

        let _ = record_event(
            dir.path(),
            &ctx(root_a, SessionSource::Cli, Some(PRIMARY_AGENT_NAME)),
            &EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "root-a-turn".to_string(),
                model_context_window: Some(100),
                collaboration_mode_kind: ModeKind::Swarm,
            }),
        )
        .expect("root a");
        let _ = record_event(
            dir.path(),
            &ctx(root_b, SessionSource::Cli, Some(PRIMARY_AGENT_NAME)),
            &EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "root-b-turn".to_string(),
                model_context_window: Some(100),
                collaboration_mode_kind: ModeKind::Swarm,
            }),
        )
        .expect("root b");

        let snapshot_a = record_event(
            dir.path(),
            &ctx(
                child_a,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: root_a,
                    depth: 1,
                    agent_type: None,
                    agent_name_hint: None,
                }),
                Some("worker-a"),
            ),
            &EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: child_a,
                turn_id: "child-a-turn".to_string(),
                item: TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
                    text: "hello-a".to_string(),
                    text_elements: vec![],
                }])),
            }),
        )
        .expect("child a");

        let snapshot_b = record_event(
            dir.path(),
            &ctx(
                child_b,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: root_b,
                    depth: 1,
                    agent_type: None,
                    agent_name_hint: None,
                }),
                Some("worker-b"),
            ),
            &EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: child_b,
                turn_id: "child-b-turn".to_string(),
                item: TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
                    text: "hello-b".to_string(),
                    text_elements: vec![],
                }])),
            }),
        )
        .expect("child b");

        assert_eq!(snapshot_a.conversation_id, root_a);
        assert_eq!(snapshot_b.conversation_id, root_b);
        assert_ne!(root_a, root_b);
        assert!(dir.path().join(root_a.to_string()).exists());
        assert!(dir.path().join(root_b.to_string()).exists());
        assert!(!dir.path().join(child_a.to_string()).exists());
        assert!(!dir.path().join(child_b.to_string()).exists());
    }

    #[test]
    #[serial(debug_trace)]
    fn captures_user_assistant_and_tool_roles() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let thread_id = ThreadId::new();
        let context = ctx(thread_id, SessionSource::Cli, Some(PRIMARY_AGENT_NAME));

        let _ = record_event(
            dir.path(),
            &context,
            &EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-1".to_string(),
                model_context_window: Some(100),
                collaboration_mode_kind: ModeKind::Default,
            }),
        )
        .expect("turn started");

        let _ = record_event(
            dir.path(),
            &context,
            &EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: "turn-1".to_string(),
                item: TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
                    text: "u".to_string(),
                    text_elements: vec![],
                }])),
            }),
        )
        .expect("user");

        let assistant_snapshot = record_event(
            dir.path(),
            &context,
            &EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: "turn-1".to_string(),
                item: TurnItem::AgentMessage(AgentMessageItem::new(&[AgentMessageContent::Text {
                    text: "assistant".to_string(),
                }])),
            }),
        )
        .expect("assistant");
        let tool_snapshot = record_event(
            dir.path(),
            &context,
            &EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
                call_id: "mcp-1".to_string(),
                invocation: McpInvocation {
                    server: "rmcp".to_string(),
                    tool: "echo".to_string(),
                    arguments: None,
                },
            }),
        )
        .expect("tool");
        let roles = assistant_snapshot
            .snapshot
            .entries
            .iter()
            .map(|entry| entry.role.clone())
            .collect::<Vec<_>>();
        assert!(roles.contains(&DebugTraceRole::System));
        assert!(roles.contains(&DebugTraceRole::Developer));
        assert!(roles.contains(&DebugTraceRole::User));
        assert!(roles.contains(&DebugTraceRole::Assistant));
        assert!(
            tool_snapshot
                .snapshot
                .entries
                .iter()
                .any(|entry| entry.role == DebugTraceRole::Tool),
        );
    }

    #[test]
    #[serial(debug_trace)]
    fn assigns_monotonic_entry_sequence_for_stable_tie_breaks() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let thread_id = ThreadId::new();
        let context = ctx(thread_id, SessionSource::Cli, Some(PRIMARY_AGENT_NAME));

        let _ = record_event(
            dir.path(),
            &context,
            &EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-1".to_string(),
                model_context_window: Some(100),
                collaboration_mode_kind: ModeKind::Default,
            }),
        )
        .expect("turn started");
        let snapshot = record_event(
            dir.path(),
            &context,
            &EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: "turn-1".to_string(),
                item: TurnItem::AgentMessage(AgentMessageItem::new(&[AgentMessageContent::Text {
                    text: "assistant".to_string(),
                }])),
            }),
        )
        .expect("assistant");

        let seqs = snapshot
            .snapshot
            .entries
            .iter()
            .map(|entry| entry.entry_seq)
            .collect::<Vec<_>>();
        let expected = (1..=seqs.len() as u64).collect::<Vec<_>>();
        assert_eq!(seqs, expected);
    }

    #[test]
    #[serial(debug_trace)]
    fn turn_started_includes_full_initial_prompt_sections() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let thread_id = ThreadId::new();
        let context = ctx(thread_id, SessionSource::Cli, Some(PRIMARY_AGENT_NAME));

        let snapshot = record_event(
            dir.path(),
            &context,
            &EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-1".to_string(),
                model_context_window: Some(100),
                collaboration_mode_kind: ModeKind::Default,
            }),
        )
        .expect("turn started");

        let entries = snapshot.snapshot.entries;
        assert!(entries.iter().any(|entry| {
            entry.source_event == "initial_context.base_instructions"
                && entry.content.contains("base inst")
                && entry.role == DebugTraceRole::System
        }));
        assert!(entries.iter().any(|entry| {
            entry.source_event == "initial_context.permissions"
                && entry.role == DebugTraceRole::Developer
        }));
        assert!(entries.iter().any(|entry| {
            entry.source_event == "initial_context.environment_context"
                && entry.role == DebugTraceRole::User
        }));
    }

    #[test]
    #[serial(debug_trace)]
    fn deduplicates_consecutive_duplicate_user_messages() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let thread_id = ThreadId::new();
        let context = ctx(thread_id, SessionSource::Cli, Some(PRIMARY_AGENT_NAME));

        let first = record_event(
            dir.path(),
            &context,
            &EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: "turn-1".to_string(),
                item: TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
                    text: "dup-user".to_string(),
                    text_elements: vec![],
                }])),
            }),
        )
        .expect("first");

        let second = record_event(
            dir.path(),
            &context,
            &EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: "turn-1".to_string(),
                item: TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
                    text: "dup-user".to_string(),
                    text_elements: vec![],
                }])),
            }),
        );

        // Duplicate event should be ignored and not produce another snapshot event.
        assert!(second.is_none());

        let user_entries = first
            .snapshot
            .entries
            .iter()
            .filter(|entry| entry.source_event == "item_completed.user_message")
            .count();
        assert_eq!(user_entries, 1);
    }

    #[test]
    #[serial(debug_trace)]
    fn collab_waiting_uses_sender_thread_source_for_agent_kind() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let root = ThreadId::new();
        let child = ThreadId::new();

        let child_context = ctx(
            child,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root,
                depth: 1,
                agent_type: None,
                agent_name_hint: None,
            }),
            Some("worker-a"),
        );

        let _ = record_event(
            dir.path(),
            &child_context,
            &EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "child-turn".to_string(),
                model_context_window: Some(100),
                collaboration_mode_kind: ModeKind::Swarm,
            }),
        )
        .expect("child turn started");

        let root_context = ctx(root, SessionSource::Cli, Some(PRIMARY_AGENT_NAME));
        let snapshot = record_event(
            dir.path(),
            &root_context,
            &EventMsg::CollabWaitingBegin(CollabWaitingBeginEvent {
                sender_thread_id: child,
                sender_agent_name: "worker-a".to_string(),
                call_id: "call-1".to_string(),
                targets: Vec::new(),
            }),
        )
        .expect("collab waiting begin");

        let waiting_entry = snapshot
            .snapshot
            .entries
            .iter()
            .find(|entry| entry.source_event == "collab_waiting_begin")
            .expect("waiting entry");
        assert_eq!(waiting_entry.agent_kind, DebugTraceAgentKind::SubAgent);
    }

    #[test]
    #[serial(debug_trace)]
    fn collab_waiting_end_includes_wait_target_details() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let root = ThreadId::new();
        let worker = ThreadId::new();
        let context = ctx(root, SessionSource::Cli, Some(PRIMARY_AGENT_NAME));

        let snapshot = record_event(
            dir.path(),
            &context,
            &EventMsg::CollabWaitingEnd(CollabWaitingEndEvent {
                sender_thread_id: root,
                sender_agent_name: PRIMARY_AGENT_NAME.to_string(),
                call_id: "wait-1".to_string(),
                timed_out: false,
                targets: vec![CollabWaitTargetEvent {
                    receiver_thread_id: worker,
                    receiver_agent_name: "worker-a".to_string(),
                    message_id: "reply-42".to_string(),
                    state: CollabWaitLifecycleState::Completed,
                    callback_content: Some(
                        "Repo path is /tmp/repo\nTask split is ready for execution.".to_string(),
                    ),
                }],
            }),
        )
        .expect("collab waiting end");

        let waiting_entry = snapshot
            .snapshot
            .entries
            .iter()
            .find(|entry| entry.source_event == "collab_waiting_end")
            .expect("waiting entry");
        assert_eq!(
            waiting_entry.content,
            "call_id=wait-1 timed_out=false target_count=1 targets=[worker-a:reply-42:completed content=\"Repo path is /tmp/repo Task split is ready for execution.\"]"
        );
    }

    #[test]
    #[serial(debug_trace)]
    fn dispatcher_work_summary_is_recorded_as_coordinator() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let root = ThreadId::new();
        let context = ctx(root, SessionSource::Cli, Some(PRIMARY_AGENT_NAME));
        let dispatcher_thread =
            ThreadId::from_string("00000000-0000-7000-8000-000000000001").expect("thread id");

        let snapshot = record_event(
            dir.path(),
            &context,
            &EventMsg::AgentWorkSummary(AgentWorkSummaryEvent {
                thread_id: dispatcher_thread,
                agent_name: "dispatcher".to_string(),
                summary: "activation summary".to_string(),
            }),
        )
        .expect("dispatcher summary");

        let entry = snapshot
            .snapshot
            .entries
            .iter()
            .find(|item| item.source_event == "agent_work_summary")
            .expect("summary entry");
        assert_eq!(entry.agent_kind, DebugTraceAgentKind::Coordinator);
        assert_eq!(entry.agent_thread_id, dispatcher_thread);
    }

    #[test]
    #[serial(debug_trace)]
    fn background_event_is_recorded_in_history() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let thread_id = ThreadId::new();
        let context = ctx(thread_id, SessionSource::Cli, Some(PRIMARY_AGENT_NAME));

        let snapshot = record_event(
            dir.path(),
            &context,
            &EventMsg::BackgroundEvent(BackgroundEventEvent {
                message: "Still waiting for first model response event...".to_string(),
                stage: None,
                elapsed_ms: None,
                timeout_ms: None,
                outcome: None,
            }),
        )
        .expect("background event snapshot");

        let entry = snapshot
            .snapshot
            .entries
            .iter()
            .find(|item| item.source_event == "background_event")
            .expect("background event entry");
        assert_eq!(entry.role, DebugTraceRole::System);
        assert_eq!(
            entry.content,
            "Still waiting for first model response event..."
        );
    }

    #[test]
    #[serial(debug_trace)]
    fn request_context_captures_three_context_channels_without_cross_contamination() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let thread_id = ThreadId::new();
        let manual_source = "/codex-home/agents/wmj-assistant/manual_system_prompt.md";
        let automatic_source = "/codex-home/agents/wmj-assistant/automatic_prompt.md";
        let blackboard_path = dir.path().join("shared-blackboard.md");
        let mut context = ctx(thread_id, SessionSource::Cli, Some(PRIMARY_AGENT_NAME));
        context.shared_blackboard_path = Some(blackboard_path.clone());

        let durable_context = format!(
            "<agent_durable_context schema_version=\"1\" agent_id=\"wmj-assistant\" display_name=\"wmj-assistant\">\n  <manual_permanent_system_prompt source=\"{manual_source}\">\nmanual stable prompt only\n  </manual_permanent_system_prompt>\n  <automatic_updated_prompt source=\"{automatic_source}\">\nautomatic stable prompt only\n  </automatic_updated_prompt>\n</agent_durable_context>"
        );
        let runtime_tail = "Below are the agents collaborating with you. You can use the `call` tool to communicate and collaborate with them.\nCurrent time: 2026-05-15 21:36:36 UTC\n\nCollaborating agents and summary lists (first 40 summaries per agent):\n- worker-a: [runtime collaborator summary only]\n\nBelow is the shared blackboard content:\n[wmj-assistant]：runtime blackboard snapshot only";
        let request_input = vec![
            ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: durable_context,
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: runtime_tail.to_string(),
                }],
                end_turn: None,
                phase: None,
            },
        ];

        let snapshot = record_request_context(dir.path(), &context, &request_input)
            .expect("request context snapshot");
        let sections = snapshot.snapshot.context_sections;
        assert_eq!(sections.len(), 4);

        let manual = sections
            .iter()
            .find(|section| {
                section.channel == DebugTraceContextChannel::ManualPermanentSystemPrompt
            })
            .expect("manual section");
        assert_eq!(
            manual.source_kind,
            "agent_durable_context.manual_permanent_system_prompt"
        );
        assert_eq!(manual.role, DebugTraceRole::Developer);
        assert_eq!(manual.source_path.as_deref(), Some(manual_source));
        assert_eq!(manual.request_item_index, Some(0));
        assert!(manual.content.contains("manual stable prompt only"));
        assert!(!manual.content.contains("automatic stable prompt only"));
        assert!(!manual.content.contains("runtime blackboard snapshot only"));

        let automatic = sections
            .iter()
            .find(|section| section.channel == DebugTraceContextChannel::AutomaticUpdatedPrompt)
            .expect("automatic section");
        assert_eq!(
            automatic.source_kind,
            "agent_durable_context.automatic_updated_prompt"
        );
        assert_eq!(automatic.role, DebugTraceRole::Developer);
        assert_eq!(automatic.source_path.as_deref(), Some(automatic_source));
        assert_eq!(automatic.request_item_index, Some(0));
        assert!(automatic.content.contains("automatic stable prompt only"));
        assert!(!automatic.content.contains("manual stable prompt only"));
        assert!(
            !automatic
                .content
                .contains("runtime collaborator summary only")
        );

        let runtime_sections = sections
            .iter()
            .filter(|section| section.channel == DebugTraceContextChannel::RuntimeTailInjection)
            .collect::<Vec<_>>();
        assert_eq!(runtime_sections.len(), 2);
        assert!(
            runtime_sections
                .iter()
                .all(|section| section.role == DebugTraceRole::User)
        );
        assert!(
            runtime_sections
                .iter()
                .all(|section| section.request_item_index == Some(1))
        );
        assert!(
            runtime_sections
                .iter()
                .all(|section| !section.content.contains("manual stable prompt only"))
        );
        assert!(
            runtime_sections
                .iter()
                .all(|section| !section.content.contains("automatic stable prompt only"))
        );

        let collaborator = runtime_sections
            .iter()
            .find(|section| section.source_kind == "agent_control.other_agents_work_status")
            .expect("collaborator runtime section");
        assert!(
            collaborator
                .content
                .contains("runtime collaborator summary only")
        );
        assert!(collaborator.source_path.is_none());

        let blackboard = runtime_sections
            .iter()
            .find(|section| section.source_kind == "shared_blackboard.snapshot")
            .expect("blackboard runtime section");
        assert_eq!(
            blackboard.source_path.as_deref(),
            Some(blackboard_path.display().to_string().as_str())
        );
        assert!(
            blackboard
                .content
                .contains("runtime blackboard snapshot only")
        );

        let history_path = dir
            .path()
            .join(thread_id.to_string())
            .join(DEBUG_TRACE_HISTORY_FILE);
        let persisted = fs::read_to_string(history_path).expect("read history snapshot");
        let persisted: DebugTraceSnapshot = serde_json::from_str(&persisted).expect("history json");
        assert_eq!(persisted.context_sections.len(), 4);
    }

    #[test]
    #[serial(debug_trace)]
    fn snapshot_updates_overwrite_history_latest_file() {
        reset_for_tests();
        let dir = tempdir().expect("tempdir");
        let thread_id = ThreadId::new();
        let context = ctx(thread_id, SessionSource::Cli, Some(PRIMARY_AGENT_NAME));

        let first = record_event(
            dir.path(),
            &context,
            &EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: "turn-1".to_string(),
                item: TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: vec![],
                }])),
            }),
        )
        .expect("first");
        let conversation_dir = dir.path().join(thread_id.to_string());
        let history_path = conversation_dir.join(DEBUG_TRACE_HISTORY_FILE);
        let first_contents = fs::read_to_string(&history_path).expect("read first snapshot");
        let first_json: DebugTraceSnapshot =
            serde_json::from_str(&first_contents).expect("first json");
        assert_eq!(first_json.entries.len(), first.snapshot.entries.len());

        let second = record_event(
            dir.path(),
            &context,
            &EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: "turn-1".to_string(),
                item: TurnItem::AgentMessage(AgentMessageItem::new(&[AgentMessageContent::Text {
                    text: "assistant".to_string(),
                }])),
            }),
        )
        .expect("second");
        let second_contents = fs::read_to_string(&history_path).expect("read second snapshot");
        let second_json: DebugTraceSnapshot =
            serde_json::from_str(&second_contents).expect("second json");
        assert_eq!(second_json.entries.len(), second.snapshot.entries.len());
        assert!(second_json.entries.len() > first_json.entries.len());
        assert!(
            !conversation_dir
                .read_dir()
                .expect("read conversation dir")
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp")),
            "temporary files should not remain after atomic replace",
        );
    }

    #[test]
    fn write_atomic_replace_replaces_existing_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("history.latest.json");

        fs::write(&path, "old").expect("seed file");
        write_atomic_replace(&path, "new").expect("replace");

        let current = fs::read_to_string(&path).expect("read");
        assert_eq!(current, "new");
        assert!(
            !dir.path()
                .read_dir()
                .expect("read dir")
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp")),
            "temporary files should be cleaned up",
        );
    }

    #[test]
    fn lane_layout_for_three_agents_places_primary_center() {
        let primary = ThreadId::new();
        let sub = ThreadId::new();
        let coordinator = ThreadId::new();
        let entries = vec![
            DebugTraceEntry {
                conversation_id: ThreadId::new(),
                entry_seq: 1,
                timestamp_unix_sec: 0,
                timestamp_rfc3339_sec: "0".to_string(),
                agent_thread_id: sub,
                agent_name: "worker".to_string(),
                agent_kind: DebugTraceAgentKind::SubAgent,
                role: DebugTraceRole::Assistant,
                source_event: "x".to_string(),
                content: "x".to_string(),
                lane_index: None,
            },
            DebugTraceEntry {
                conversation_id: ThreadId::new(),
                entry_seq: 2,
                timestamp_unix_sec: 0,
                timestamp_rfc3339_sec: "0".to_string(),
                agent_thread_id: primary,
                agent_name: PRIMARY_AGENT_NAME.to_string(),
                agent_kind: DebugTraceAgentKind::Primary,
                role: DebugTraceRole::Assistant,
                source_event: "x".to_string(),
                content: "x".to_string(),
                lane_index: None,
            },
            DebugTraceEntry {
                conversation_id: ThreadId::new(),
                entry_seq: 3,
                timestamp_unix_sec: 0,
                timestamp_rfc3339_sec: "0".to_string(),
                agent_thread_id: coordinator,
                agent_name: "dispatcher".to_string(),
                agent_kind: DebugTraceAgentKind::Coordinator,
                role: DebugTraceRole::Assistant,
                source_event: "x".to_string(),
                content: "x".to_string(),
                lane_index: None,
            },
        ];
        let lanes = build_lanes(&entries);
        assert_eq!(lanes.len(), 3);
        assert_eq!(lanes[1].agent_name, PRIMARY_AGENT_NAME.to_string());
        assert_eq!(lanes[0].lane_position, DebugTraceLanePosition::Left);
        assert_eq!(lanes[1].lane_position, DebugTraceLanePosition::Center);
        assert_eq!(lanes[2].lane_position, DebugTraceLanePosition::Right);
    }
}
