use async_trait::async_trait;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::protocol::CollabAgentInteractionBeginEvent;
use codex_protocol::protocol::CollabAgentInteractionEndEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SpawnedAgentType;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use encoding_rs::BIG5;
use encoding_rs::EUC_KR;
use encoding_rs::Encoding;
use encoding_rs::GBK;
use encoding_rs::SHIFT_JIS;
use encoding_rs::WINDOWS_1252;
use serde_json::Value as JsonValue;

use crate::agent::AgentStatus;
use crate::agent::PRIMARY_AGENT_NAME;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::error::CodexErr;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::collab_inbox;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

/// Implements correlated `call` semantics on top of Codex sub-agent messaging.
///
/// The caller always continues after dispatch. Use the separate `wait` tool to block on correlated
/// replies.
pub struct CallHandler;

/// Avoid sending arbitrarily large payloads through the multi-agent channel.
const MAX_CONTENT_CHARS: usize = 32_768;
const LOSSY_MARKERS: [char; 2] = ['\u{FFFD}', '?'];
const UTF8_RETRY_ENCODINGS: [&Encoding; 5] = [GBK, BIG5, SHIFT_JIS, EUC_KR, WINDOWS_1252];
const MOJIBAKE_HINT_CHARS: [char; 7] = [
    '\u{00C3}', '\u{00C2}', '\u{00E2}', '\u{00D0}', '\u{00D1}', '\u{00E6}', '\u{00E5}',
];

const FORBIDDEN_CALL_FIELDS: [&str; 3] = ["status_control", "timeout_ms", "context_data"];

const CALL_DISPATCHED_TEXT: &str = "call dispatched successfully; continue without waiting";
const CALL_FAILED_TEXT: &str = "call failed; continue without waiting";

#[derive(Debug)]
struct CallArgs {
    target_id: String,
    content: String,
    message_id: String,
    need_reply: bool,
    reply_to_message_id: Option<String>,
}

fn call_output(success: bool, message: &str) -> Result<ToolOutput, FunctionCallError> {
    Ok(ToolOutput::Function {
        body: FunctionCallOutputBody::Text(message.to_string()),
        success: Some(success),
    })
}

fn parse_call_args(arguments: &str) -> Result<CallArgs, FunctionCallError> {
    let value: JsonValue = match serde_json::from_str(arguments) {
        Ok(value) => value,
        Err(parse_err) => {
            let mut recovered_value = None;
            for encoding in UTF8_RETRY_ENCODINGS {
                let Some(candidate_json) = transcode_to_utf8(arguments, encoding) else {
                    continue;
                };
                if candidate_json == arguments {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<JsonValue>(&candidate_json) {
                    tracing::warn!(
                        encoding = encoding.name(),
                        "call arguments parse failed once; recovered after transcoding retry"
                    );
                    recovered_value = Some(value);
                    break;
                }
            }
            recovered_value.ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "failed to parse function arguments: {parse_err}"
                ))
            })?
        }
    };

    let Some(obj) = value.as_object() else {
        return Err(FunctionCallError::RespondToModel(
            "failed to parse function arguments: expected JSON object".to_string(),
        ));
    };

    let forbidden = FORBIDDEN_CALL_FIELDS
        .iter()
        .copied()
        .filter(|key| obj.contains_key(*key))
        .collect::<Vec<_>>();
    if !forbidden.is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "unsupported call fields: {}",
            forbidden.join(", ")
        )));
    }

    let target_id = obj
        .get("target_id")
        .or_else(|| obj.get("target_agent_name"))
        .and_then(JsonValue::as_str)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "target_id or target_agent_name is required".to_string(),
            )
        })?
        .to_string();
    let content = obj
        .get("content")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| FunctionCallError::RespondToModel("content is required".to_string()))?
        .to_string();
    let message_id_value = obj
        .get("message_id")
        .ok_or_else(|| FunctionCallError::RespondToModel("message_id is required".to_string()))?;
    let message_id = match message_id_value {
        JsonValue::String(value) => value.clone(),
        JsonValue::Number(value) => value.to_string(),
        other => {
            return Err(FunctionCallError::RespondToModel(format!(
                "message id must be a string or number; got {other:?}"
            )));
        }
    };
    let reply_to_message_id = match obj.get("reply_to_message_id") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::String(value)) => Some(value.clone()),
        Some(JsonValue::Number(value)) => Some(value.to_string()),
        Some(other) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "message id must be a string or number; got {other:?}"
            )));
        }
    };
    let mut args = CallArgs {
        target_id,
        content,
        message_id,
        need_reply: obj
            .get("need_reply")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false),
        reply_to_message_id,
    };

    let normalized_content = normalize_call_content_utf8(&args.content);
    if normalized_content != args.content {
        tracing::warn!("call content looked mis-decoded; applied UTF-8 transcoding retry");
        args.content = normalized_content;
    }

    Ok(args)
}

fn transcode_to_utf8(input: &str, encoding: &'static Encoding) -> Option<String> {
    let (encoded, _, had_encode_errors) = encoding.encode(input);
    if had_encode_errors {
        return None;
    }
    Some(std::str::from_utf8(encoded.as_ref()).ok()?.to_string())
}

fn content_repair_score(content: &str) -> (usize, usize, usize) {
    let lossy_marker_count = content
        .chars()
        .filter(|ch| LOSSY_MARKERS.contains(ch))
        .count();
    let c1_control_count = content
        .chars()
        .filter(|ch| {
            let code = *ch as u32;
            (0x80..=0x9F).contains(&code)
        })
        .count();
    let mojibake_hint_count = content
        .chars()
        .filter(|ch| MOJIBAKE_HINT_CHARS.contains(ch))
        .count();
    (lossy_marker_count, c1_control_count, mojibake_hint_count)
}

fn normalize_call_content_utf8(content: &str) -> String {
    let mut best = content.to_string();
    let mut best_score = content_repair_score(content);
    for encoding in UTF8_RETRY_ENCODINGS {
        let Some(candidate) = transcode_to_utf8(content, encoding) else {
            continue;
        };
        let candidate_score = content_repair_score(&candidate);
        if candidate_score < best_score {
            best = candidate;
            best_score = candidate_score;
        }
    }
    best
}

fn call_error_details(agent_name: &str, err: CodexErr) -> String {
    match err {
        CodexErr::ThreadNotFound(_) => format!("agent name not found: {agent_name}"),
        CodexErr::InternalAgentDied => format!("agent `{agent_name}` is closed"),
        CodexErr::UnsupportedOperation(_) => "collab manager unavailable".to_string(),
        err => {
            tracing::warn!(?err, %agent_name, "call tool failed");
            "call tool failed".to_string()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallDispatchMode {
    StartNewTurn,
    QueueInRunningTurn,
    LogicalInbox,
}

fn dispatch_mode_for_status(status: &AgentStatus) -> Option<CallDispatchMode> {
    match status {
        AgentStatus::PendingInit
        | AgentStatus::Interrupted
        | AgentStatus::Completed(_)
        | AgentStatus::Errored(_) => Some(CallDispatchMode::StartNewTurn),
        AgentStatus::Running => Some(CallDispatchMode::QueueInRunningTurn),
        AgentStatus::Shutdown | AgentStatus::NotFound => None,
    }
}

fn build_call_message(
    sender_agent_name: &str,
    message_id: &str,
    reply_to_message_id: Option<&str>,
    content: &str,
    need_reply: bool,
) -> Vec<UserInput> {
    let mut body_text = content.trim_end().to_string();

    if need_reply {
        let suffix = format!(
            "\n\nWhen finished, send your result back using `call` with:\n- target_agent_name: \"{sender_agent_name}\"\n- message_id: <a new id for your reply>\n- reply_to_message_id: \"{message_id}\"\n- content: <your final result>.\n\nIf you delegate further using `call`, keep using new message_id values and set reply_to_message_id to the relevant original message_id you are replying to."
        );

        // Best-effort: avoid duplicating the instruction if the caller already included it.
        if !body_text.contains("reply_to_message_id") && !body_text.contains("message_id") {
            body_text.push_str(&suffix);
        }
    }

    let reply_to_message_id = reply_to_message_id.unwrap_or("none");
    let text = format!(
        "call from [{sender_agent_name}],reply_to[{reply_to_message_id}],message_id[{message_id}]\n\n{body_text}"
    );

    vec![UserInput::Text {
        text,
        text_elements: Vec::new(),
    }]
}

async fn augment_call_content_for_verifier(
    session: &Session,
    turn: &TurnContext,
    target_thread_id: ThreadId,
    content: &str,
) -> String {
    if matches!(turn.session_source, SessionSource::SubAgent(_)) {
        return content.to_string();
    }

    let Ok(snapshot) = session
        .services
        .agent_control
        .thread_config_snapshot(target_thread_id)
        .await
    else {
        return content.to_string();
    };

    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        agent_type: Some(SpawnedAgentType::Verifier),
        ..
    }) = snapshot.session_source
    else {
        return content.to_string();
    };

    if parent_thread_id != session.conversation_id {
        return content.to_string();
    }

    let recent_user_inputs = session.recent_real_user_inputs().await;
    if recent_user_inputs.is_empty() {
        return content.to_string();
    }

    let total = recent_user_inputs.len();
    let labeled_inputs = recent_user_inputs
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let ordinal = index + 1;
            let label = if ordinal == total {
                format!("User input {ordinal} (latest)")
            } else {
                format!("User input {ordinal}")
            };
            format!("- {label}:\n{text}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    format!(
        "{}\n\nPlease also consider the following recent real human user inputs from the main agent session. These are preserved verbatim in chronological order and may conflict; use the ordering to resolve that context.\n\n{}",
        content.trim_end(),
        labeled_inputs
    )
}

fn unknown_agent_name_error(session: &Session, agent_name: &str) -> FunctionCallError {
    let known_names = session.services.agent_control.known_agent_names();
    if known_names.is_empty() {
        return FunctionCallError::RespondToModel(format!("agent name not found: {agent_name}"));
    }
    FunctionCallError::RespondToModel(format!(
        "agent name not found: {agent_name}. Known agent names: {}",
        known_names.join(", ")
    ))
}

fn resolve_target_agent(
    session: &Session,
    target_agent_name: &str,
) -> Result<ThreadId, FunctionCallError> {
    if let Some(thread_id) = session
        .services
        .agent_control
        .thread_id_for_agent_name(target_agent_name)
    {
        return Ok(thread_id);
    }
    if let Ok(thread_id) = ThreadId::from_string(target_agent_name) {
        return Ok(thread_id);
    }
    Err(unknown_agent_name_error(session, target_agent_name))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedCallTarget {
    Agent(ThreadId),
    Logical,
}

fn resolve_call_target(session: &Session, target_id: &str) -> ResolvedCallTarget {
    if let Some(thread_id) = session
        .services
        .agent_control
        .thread_id_for_agent_name(target_id)
    {
        return ResolvedCallTarget::Agent(thread_id);
    }
    if let Ok(thread_id) = ThreadId::from_string(target_id) {
        return ResolvedCallTarget::Agent(thread_id);
    }
    ResolvedCallTarget::Logical
}

fn ensure_sender_agent_name(
    session: &Session,
    turn: &TurnContext,
) -> Result<String, FunctionCallError> {
    if let Some(name) = session
        .services
        .agent_control
        .agent_name_for_thread(session.conversation_id)
    {
        return Ok(name);
    }

    if !matches!(turn.session_source, SessionSource::SubAgent(_)) {
        session
            .services
            .agent_control
            .register_agent_name(session.conversation_id, PRIMARY_AGENT_NAME)
            .map_err(FunctionCallError::RespondToModel)?;
        return Ok(PRIMARY_AGENT_NAME.to_string());
    }

    Err(FunctionCallError::RespondToModel(format!(
        "agent name not found for current sender: {}",
        session.conversation_id
    )))
}

fn input_preview(items: &[UserInput]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|item| match item {
            UserInput::Text { text, .. } => text.clone(),
            UserInput::Image { .. } => "[image]".to_string(),
            UserInput::LocalImage { path } => format!("[local_image:{}]", path.display()),
            UserInput::Skill { name, path } => format!("[skill:${name}]({})", path.display()),
            UserInput::Mention { name, path } => format!("[mention:${name}]({path})"),
            _ => "[input]".to_string(),
        })
        .collect();
    parts.join("\n")
}

#[async_trait]
impl ToolHandler for CallHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn is_mutating(&self, _invocation: &ToolInvocation) -> bool {
        true
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "call handler received unsupported payload".to_string(),
                ));
            }
        };

        let args = parse_call_args(&arguments)?;
        if args.target_id.trim().is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "target_id or target_agent_name is required".to_string(),
            ));
        }
        if args.content.trim().is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "content must be non-empty".to_string(),
            ));
        }
        if args.content.chars().count() > MAX_CONTENT_CHARS {
            return Err(FunctionCallError::RespondToModel(format!(
                "content is too large (max {MAX_CONTENT_CHARS} characters)"
            )));
        }
        if args.message_id.trim().is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "message_id must be non-empty".to_string(),
            ));
        }
        if args
            .reply_to_message_id
            .as_deref()
            .is_some_and(|id| id.trim().is_empty())
        {
            return Err(FunctionCallError::RespondToModel(
                "reply_to_message_id must be non-empty when set".to_string(),
            ));
        }

        let target = resolve_call_target(session.as_ref(), &args.target_id);
        if matches!(target, ResolvedCallTarget::Logical) && args.target_id.contains('-') {
            return Err(unknown_agent_name_error(session.as_ref(), &args.target_id));
        }
        let target_inbox_id = match target {
            ResolvedCallTarget::Agent(thread_id) => thread_id.to_string(),
            ResolvedCallTarget::Logical => args.target_id.clone(),
        };
        if turn.collaboration_mode.mode == ModeKind::Swarm
            && collab_inbox::has_required_reply_obligation_logical(
                &target_inbox_id,
                &args.message_id,
            )
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "message_id already has an unresolved need_reply obligation for target `{}`: {}",
                args.target_id, args.message_id
            )));
        }

        let (status_before_send, dispatch_mode) = match target {
            ResolvedCallTarget::Agent(target_thread_id) => {
                let status = session
                    .services
                    .agent_control
                    .get_status(target_thread_id)
                    .await;
                let Some(dispatch_mode) = dispatch_mode_for_status(&status) else {
                    tracing::info!(
                        target_agent_name = %args.target_id,
                        %target_thread_id,
                        ?status,
                        "call rejected"
                    );
                    return call_output(false, CALL_FAILED_TEXT);
                };
                (status, dispatch_mode)
            }
            ResolvedCallTarget::Logical => {
                (AgentStatus::Completed(None), CallDispatchMode::LogicalInbox)
            }
        };

        let sender_agent_name = ensure_sender_agent_name(session.as_ref(), turn.as_ref())?;
        let augmented_content = match target {
            ResolvedCallTarget::Agent(target_thread_id) => {
                augment_call_content_for_verifier(
                    session.as_ref(),
                    turn.as_ref(),
                    target_thread_id,
                    &args.content,
                )
                .await
            }
            ResolvedCallTarget::Logical => args.content.clone(),
        };
        let message = build_call_message(
            &sender_agent_name,
            &args.message_id,
            args.reply_to_message_id.as_deref(),
            &augmented_content,
            args.need_reply,
        );
        let prompt_preview = input_preview(&message);

        if let ResolvedCallTarget::Agent(target_thread_id) = target {
            session
                .send_event(
                    &turn,
                    CollabAgentInteractionBeginEvent {
                        call_id: call_id.clone(),
                        sender_thread_id: session.conversation_id,
                        receiver_thread_id: target_thread_id,
                        prompt: prompt_preview.clone(),
                    }
                    .into(),
                )
                .await;

            let is_self_target = target_thread_id == session.conversation_id;
            let send_result = if is_self_target {
                Ok(CALL_DISPATCHED_TEXT.to_string())
            } else {
                session
                    .services
                    .agent_control
                    .send_input(target_thread_id, message)
                    .await
            };

            let status_after_send = if is_self_target {
                status_before_send.clone()
            } else {
                session
                    .services
                    .agent_control
                    .get_status(target_thread_id)
                    .await
            };

            session
                .send_event(
                    &turn,
                    CollabAgentInteractionEndEvent {
                        call_id: call_id.clone(),
                        sender_thread_id: session.conversation_id,
                        receiver_thread_id: target_thread_id,
                        prompt: prompt_preview,
                        status: status_after_send.clone(),
                    }
                    .into(),
                )
                .await;

            if let Err(err) = send_result {
                let details = call_error_details(&args.target_id, err);
                tracing::info!(
                    target_agent_name = %args.target_id,
                    %target_thread_id,
                    ?dispatch_mode,
                    %details,
                    "call failed to dispatch"
                );
                return call_output(false, CALL_FAILED_TEXT);
            }
        }

        if turn.collaboration_mode.mode == ModeKind::Swarm {
            if args.need_reply {
                collab_inbox::register_required_reply_logical(
                    &target_inbox_id,
                    sender_agent_name.clone(),
                    session.conversation_id,
                    args.message_id.clone(),
                    augmented_content.clone(),
                );
            }
            if let Some(reply_to_message_id) = args.reply_to_message_id.as_deref() {
                collab_inbox::resolve_required_reply_logical_from_source_id(
                    &session.conversation_id.to_string(),
                    &target_inbox_id,
                    reply_to_message_id,
                );
            }
        }

        // Publish the message into the receiver's inbox so `wait` can observe it.
        collab_inbox::append_logical_message(
            &target_inbox_id,
            sender_agent_name,
            args.message_id.clone(),
            args.need_reply,
            args.reply_to_message_id.clone(),
            augmented_content,
        );

        call_output(true, CALL_DISPATCHED_TEXT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::CodexAuth;
    use crate::ThreadManager;
    use crate::built_in_model_providers;
    use crate::codex::make_session_and_context;
    use crate::protocol::Op;
    use crate::tools::context::ToolPayload;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SpawnedAgentType;
    use codex_protocol::protocol::SubAgentSource;
    use codex_protocol::protocol::UserInputOrigin;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn invocation(
        session: Arc<Session>,
        turn: Arc<TurnContext>,
        args: serde_json::Value,
    ) -> ToolInvocation {
        ToolInvocation {
            session,
            turn,
            tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
            tool_name: "call".to_string(),
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: args.to_string(),
            },
        }
    }

    fn thread_manager() -> ThreadManager {
        ThreadManager::with_models_provider_for_tests(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers()["openai"].clone(),
        )
    }

    #[test]
    fn dispatch_mode_for_status_handles_lifecycle_states() {
        assert_eq!(
            dispatch_mode_for_status(&AgentStatus::PendingInit),
            Some(CallDispatchMode::StartNewTurn)
        );
        assert_eq!(
            dispatch_mode_for_status(&AgentStatus::Completed(Some("done".to_string()))),
            Some(CallDispatchMode::StartNewTurn)
        );
        assert_eq!(
            dispatch_mode_for_status(&AgentStatus::Errored("boom".to_string())),
            Some(CallDispatchMode::StartNewTurn)
        );
        assert_eq!(
            dispatch_mode_for_status(&AgentStatus::Interrupted),
            Some(CallDispatchMode::StartNewTurn)
        );
        assert_eq!(
            dispatch_mode_for_status(&AgentStatus::Running),
            Some(CallDispatchMode::QueueInRunningTurn)
        );
        assert_eq!(dispatch_mode_for_status(&AgentStatus::Shutdown), None);
        assert_eq!(dispatch_mode_for_status(&AgentStatus::NotFound), None);
    }

    #[tokio::test]
    async fn call_rejects_removed_fields() {
        let (session, turn) = make_session_and_context().await;
        let invocation = invocation(
            Arc::new(session),
            Arc::new(turn),
            json!({
                "target_agent_name": "Alice-worker",
                "content": "hi",
                "message_id": "m1",
                "status_control": "continue"
            }),
        );
        let err = match CallHandler.handle(invocation).await {
            Ok(_) => panic!("should fail"),
            Err(err) => err,
        };
        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "unsupported call fields: status_control".to_string()
            )
        );
    }

    #[tokio::test]
    async fn call_rejects_empty_target() {
        let (session, turn) = make_session_and_context().await;
        let invocation = invocation(
            Arc::new(session),
            Arc::new(turn),
            json!({
                "target_agent_name": "",
                "content": "hi",
                "message_id": "m1"
            }),
        );
        let err = match CallHandler.handle(invocation).await {
            Ok(_) => panic!("should fail"),
            Err(err) => err,
        };
        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "target_id or target_agent_name is required".to_string()
            )
        );
    }

    #[tokio::test]
    async fn call_rejects_unknown_target_agent_name() {
        let (session, turn) = make_session_and_context().await;
        let invocation = invocation(
            Arc::new(session),
            Arc::new(turn),
            json!({
                "target_agent_name": "unknown-agent",
                "content": "hi",
                "message_id": "m1"
            }),
        );
        let err = match CallHandler.handle(invocation).await {
            Ok(_) => panic!("should fail"),
            Err(err) => err,
        };
        let FunctionCallError::RespondToModel(msg) = err else {
            panic!("expected respond-to-model error");
        };
        assert!(msg.starts_with("agent name not found: unknown-agent"));
    }

    #[tokio::test]
    async fn call_rejects_empty_content() {
        let (session, turn) = make_session_and_context().await;
        let invocation = invocation(
            Arc::new(session),
            Arc::new(turn),
            json!({
                "target_agent_name": "Alice-worker",
                "content": "   ",
                "message_id": "m1"
            }),
        );
        let err = match CallHandler.handle(invocation).await {
            Ok(_) => panic!("should fail"),
            Err(err) => err,
        };
        assert_eq!(
            err,
            FunctionCallError::RespondToModel("content must be non-empty".to_string())
        );
    }

    #[tokio::test]
    async fn call_rejects_empty_message_id() {
        let (session, turn) = make_session_and_context().await;
        let invocation = invocation(
            Arc::new(session),
            Arc::new(turn),
            json!({
                "target_agent_name": "Alice-worker",
                "content": "hi",
                "message_id": "   "
            }),
        );
        let err = match CallHandler.handle(invocation).await {
            Ok(_) => panic!("should fail"),
            Err(err) => err,
        };
        assert_eq!(
            err,
            FunctionCallError::RespondToModel("message_id must be non-empty".to_string())
        );
    }

    #[tokio::test]
    async fn call_dispatches_to_pending_init_agent() {
        collab_inbox::reset_for_tests();
        let (mut session, turn) = make_session_and_context().await;
        let manager = thread_manager();
        session.services.agent_control = manager.agent_control();
        let agent_control = session.services.agent_control.clone();
        let config = turn.config.as_ref().clone();
        let thread = manager.start_thread(config).await.expect("start thread");
        let agent_id = thread.thread_id;
        session
            .services
            .agent_control
            .register_agent_name(agent_id, "Alice-worker")
            .expect("register agent name");

        let output = CallHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({
                    "target_agent_name": "Alice-worker",
                    "content": "hi from caller",
                    "message_id": "m1"
                }),
            ))
            .await
            .expect("call should succeed");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(content, CALL_DISPATCHED_TEXT);
        assert_eq!(success, Some(true));

        let sent_user_input = manager
            .captured_ops()
            .into_iter()
            .any(|(id, op)| id == agent_id && matches!(op, Op::UserInput { .. }));
        assert!(sent_user_input);

        let _ = agent_control
            .shutdown_agent(agent_id)
            .await
            .expect("shutdown agent");
    }

    #[tokio::test]
    async fn swarm_need_reply_call_registers_required_reply_obligation() {
        collab_inbox::reset_for_tests();

        let (mut session, mut turn) = make_session_and_context().await;
        turn.collaboration_mode.mode = ModeKind::Swarm;
        let manager = thread_manager();
        session.services.agent_control = manager.agent_control();
        let agent_control = session.services.agent_control.clone();
        let config = turn.config.as_ref().clone();
        let thread = manager.start_thread(config).await.expect("start thread");
        let agent_id = thread.thread_id;
        session
            .services
            .agent_control
            .register_agent_name(agent_id, "Alice-worker")
            .expect("register agent name");

        let output = CallHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({
                    "target_agent_name": "Alice-worker",
                    "content": "please reply",
                    "message_id": "m-required",
                    "need_reply": true
                }),
            ))
            .await
            .expect("call should succeed");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(content, CALL_DISPATCHED_TEXT);
        assert_eq!(success, Some(true));

        let unresolved = collab_inbox::unresolved_required_reply_obligations(agent_id);
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].message_id, "m-required");
        assert_eq!(unresolved[0].source_agent_name, PRIMARY_AGENT_NAME);
        assert_eq!(unresolved[0].content, "please reply");

        let _ = agent_control
            .shutdown_agent(agent_id)
            .await
            .expect("shutdown agent");
    }

    #[tokio::test]
    async fn swarm_call_to_logical_human_target_queues_human_inbox() {
        collab_inbox::reset_for_tests();

        let (session, mut turn) = make_session_and_context().await;
        turn.collaboration_mode.mode = ModeKind::Swarm;
        session
            .services
            .agent_control
            .register_agent_name(session.conversation_id, PRIMARY_AGENT_NAME)
            .expect("register root agent");
        let sender_thread_id = session.conversation_id;

        let output = CallHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({
                    "target_id": "user_ceo",
                    "content": "需要你批准上线窗口。",
                    "message_id": "ceo-owner-approval",
                    "need_reply": true
                }),
            ))
            .await
            .expect("logical human call should succeed");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(content, CALL_DISPATCHED_TEXT);
        assert_eq!(success, Some(true));

        let messages = collab_inbox::messages_for_logical("user_ceo");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].sender_agent_name, PRIMARY_AGENT_NAME);
        assert_eq!(messages[0].message_id, "ceo-owner-approval");
        assert!(messages[0].need_reply);

        let pending =
            collab_inbox::unresolved_required_reply_obligations_from_source(sender_thread_id);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].receiver_inbox_id, "user_ceo");
        assert_eq!(pending[0].obligation.message_id, "ceo-owner-approval");
    }

    #[tokio::test]
    async fn swarm_call_reply_to_message_id_resolves_required_reply_obligation() {
        collab_inbox::reset_for_tests();

        let (mut session, mut turn) = make_session_and_context().await;
        turn.collaboration_mode.mode = ModeKind::Swarm;
        let manager = thread_manager();
        session.services.agent_control = manager.agent_control();
        let agent_control = session.services.agent_control.clone();
        let config = turn.config.as_ref().clone();
        let thread = manager.start_thread(config).await.expect("start thread");
        let target_thread_id = thread.thread_id;
        session
            .services
            .agent_control
            .register_agent_name(target_thread_id, "Alice-worker")
            .expect("register agent name");
        let sender_thread_id = session.conversation_id;

        collab_inbox::register_required_reply(
            sender_thread_id,
            "Alice-worker".to_string(),
            target_thread_id,
            "pending-msg".to_string(),
            "request body".to_string(),
        );

        let output = CallHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({
                    "target_agent_name": "Alice-worker",
                    "content": "reply body",
                    "message_id": "reply-msg",
                    "reply_to_message_id": "pending-msg"
                }),
            ))
            .await
            .expect("call should succeed");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(content, CALL_DISPATCHED_TEXT);
        assert_eq!(success, Some(true));
        assert!(collab_inbox::unresolved_required_reply_obligations(sender_thread_id).is_empty());

        let _ = agent_control
            .shutdown_agent(target_thread_id)
            .await
            .expect("shutdown agent");
    }

    #[tokio::test]
    async fn default_mode_need_reply_does_not_register_required_reply_obligation() {
        collab_inbox::reset_for_tests();

        let (mut session, turn) = make_session_and_context().await;
        let manager = thread_manager();
        session.services.agent_control = manager.agent_control();
        let agent_control = session.services.agent_control.clone();
        let config = turn.config.as_ref().clone();
        let thread = manager.start_thread(config).await.expect("start thread");
        let agent_id = thread.thread_id;
        session
            .services
            .agent_control
            .register_agent_name(agent_id, "Alice-worker")
            .expect("register agent name");

        let output = CallHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({
                    "target_agent_name": "Alice-worker",
                    "content": "please reply",
                    "message_id": "m-required",
                    "need_reply": true
                }),
            ))
            .await
            .expect("call should succeed");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(content, CALL_DISPATCHED_TEXT);
        assert_eq!(success, Some(true));
        assert!(collab_inbox::unresolved_required_reply_obligations(agent_id).is_empty());

        let _ = agent_control
            .shutdown_agent(agent_id)
            .await
            .expect("shutdown agent");
    }

    #[tokio::test]
    async fn swarm_unmatched_reply_to_message_id_is_noop_for_required_reply_tracker() {
        collab_inbox::reset_for_tests();

        let (mut session, mut turn) = make_session_and_context().await;
        turn.collaboration_mode.mode = ModeKind::Swarm;
        let manager = thread_manager();
        session.services.agent_control = manager.agent_control();
        let agent_control = session.services.agent_control.clone();
        let config = turn.config.as_ref().clone();
        let thread = manager.start_thread(config).await.expect("start thread");
        let target_thread_id = thread.thread_id;
        session
            .services
            .agent_control
            .register_agent_name(target_thread_id, "Alice-worker")
            .expect("register agent name");
        let sender_thread_id = session.conversation_id;
        collab_inbox::register_required_reply(
            sender_thread_id,
            "Alice-worker".to_string(),
            target_thread_id,
            "pending-msg".to_string(),
            "request body".to_string(),
        );

        let output = CallHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({
                    "target_agent_name": "Alice-worker",
                    "content": "reply body",
                    "message_id": "reply-msg",
                    "reply_to_message_id": "other-msg"
                }),
            ))
            .await
            .expect("call should succeed");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(content, CALL_DISPATCHED_TEXT);
        assert_eq!(success, Some(true));
        let unresolved = collab_inbox::unresolved_required_reply_obligations(sender_thread_id);
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].message_id, "pending-msg");

        let _ = agent_control
            .shutdown_agent(target_thread_id)
            .await
            .expect("shutdown agent");
    }

    #[tokio::test]
    async fn swarm_rejects_duplicate_inflight_message_id_for_same_receiver() {
        collab_inbox::reset_for_tests();

        let (session, mut turn) = make_session_and_context().await;
        turn.collaboration_mode.mode = ModeKind::Swarm;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let target_thread_id = ThreadId::new();
        session
            .services
            .agent_control
            .register_agent_name(target_thread_id, "Alice-worker")
            .expect("register agent name");
        collab_inbox::register_required_reply(
            target_thread_id,
            "sender-a".to_string(),
            ThreadId::new(),
            "m-dup".to_string(),
            "first body".to_string(),
        );

        let err = match CallHandler
            .handle(invocation(
                session,
                turn,
                json!({
                    "target_agent_name": "Alice-worker",
                    "content": "second body",
                    "message_id": "m-dup",
                    "need_reply": true
                }),
            ))
            .await
        {
            Ok(_) => panic!("duplicate in-flight message_id should fail"),
            Err(err) => err,
        };

        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "message_id already has an unresolved need_reply obligation for target agent `Alice-worker`: m-dup"
                    .to_string(),
            )
        );
    }

    #[tokio::test]
    async fn call_appends_recent_root_user_inputs_for_direct_verifier_child() {
        collab_inbox::reset_for_tests();

        let (mut session, turn) = make_session_and_context().await;
        let manager = thread_manager();
        session.services.agent_control = manager.agent_control();
        let agent_control = session.services.agent_control.clone();
        let config = turn.config.as_ref().clone();

        session
            .push_recent_real_user_input("First human requirement".to_string())
            .await;
        session
            .push_recent_real_user_input("Second human requirement".to_string())
            .await;
        session
            .push_recent_real_user_input("Latest human requirement".to_string())
            .await;

        let verifier_id = agent_control
            .spawn_agent(
                config,
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: session.conversation_id,
                    depth: 1,
                    agent_type: Some(SpawnedAgentType::Verifier),
                    agent_name_hint: Some("Quinn-verifier".to_string()),
                })),
            )
            .await
            .expect("spawn verifier child");
        agent_control
            .register_agent_name(verifier_id, "Quinn-verifier")
            .expect("register verifier agent name");

        let output = CallHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({
                    "target_agent_name": "Quinn-verifier",
                    "content": "Please verify the implementation.",
                    "message_id": "verify-1"
                }),
            ))
            .await
            .expect("call should succeed");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(content, CALL_DISPATCHED_TEXT);
        assert_eq!(success, Some(true));

        let sent = manager
            .captured_ops()
            .into_iter()
            .find(|(id, op)| *id == verifier_id && matches!(op, Op::UserInput { .. }))
            .expect("captured verifier input");
        let Op::UserInput {
            items,
            origin,
            final_output_json_schema,
        } = sent.1
        else {
            panic!("expected user input op");
        };
        assert_eq!(origin, UserInputOrigin::AgentCall);
        assert_eq!(final_output_json_schema, None);
        assert_eq!(items.len(), 1);
        let UserInput::Text { text, .. } = &items[0] else {
            panic!("expected text input");
        };
        assert!(text.contains("call from [wmj-assistant],reply_to[none],message_id[verify-1]"));
        assert!(text.contains("Please verify the implementation."));
        assert!(text.contains("Please also consider the following recent real human user inputs from the main agent session."));
        assert!(text.contains(
            "- User input 1:
First human requirement"
        ));
        assert!(text.contains(
            "- User input 2:
Second human requirement"
        ));
        assert!(text.contains(
            "- User input 3 (latest):
Latest human requirement"
        ));

        let _ = agent_control
            .shutdown_agent(verifier_id)
            .await
            .expect("shutdown verifier child");
    }

    #[tokio::test]
    async fn call_does_not_append_recent_user_inputs_for_non_verifier_child() {
        collab_inbox::reset_for_tests();

        let (mut session, turn) = make_session_and_context().await;
        let manager = thread_manager();
        session.services.agent_control = manager.agent_control();
        let agent_control = session.services.agent_control.clone();
        let config = turn.config.as_ref().clone();

        session
            .push_recent_real_user_input("Only for verifier".to_string())
            .await;

        let worker_id = agent_control
            .spawn_agent(
                config,
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: session.conversation_id,
                    depth: 1,
                    agent_type: Some(SpawnedAgentType::Worker),
                    agent_name_hint: Some("Wren-worker".to_string()),
                })),
            )
            .await
            .expect("spawn worker child");
        agent_control
            .register_agent_name(worker_id, "Wren-worker")
            .expect("register worker agent name");

        let output = CallHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({
                    "target_agent_name": "Wren-worker",
                    "content": "Please implement the fix.",
                    "message_id": "work-1"
                }),
            ))
            .await
            .expect("call should succeed");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(content, CALL_DISPATCHED_TEXT);
        assert_eq!(success, Some(true));

        let sent = manager
            .captured_ops()
            .into_iter()
            .find(|(id, op)| *id == worker_id && matches!(op, Op::UserInput { .. }))
            .expect("captured worker input");
        let Op::UserInput { items, .. } = sent.1 else {
            panic!("expected user input op");
        };
        let UserInput::Text { text, .. } = &items[0] else {
            panic!("expected text input");
        };
        assert!(text.contains("Please implement the fix."));
        assert!(!text.contains("recent real human user inputs"));
        assert!(!text.contains("Only for verifier"));

        let _ = agent_control
            .shutdown_agent(worker_id)
            .await
            .expect("shutdown worker child");
    }

    #[tokio::test]
    async fn call_returns_failed_output_for_missing_thread_id() {
        let (session, turn) = make_session_and_context().await;
        let invocation = invocation(
            Arc::new(session),
            Arc::new(turn),
            json!({
                "target_agent_name": ThreadId::new().to_string(),
                "content": "hello",
                "message_id": "m1"
            }),
        );
        let output = CallHandler
            .handle(invocation)
            .await
            .expect("missing thread id should produce failed output");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(content, CALL_FAILED_TEXT);
        assert_eq!(success, Some(false));
    }

    #[test]
    fn build_call_message_prepends_correlation_header() {
        let items = build_call_message("Alice-worker", "m-1", Some("m-0"), "hello", false);
        assert_eq!(
            items,
            vec![UserInput::Text {
                text: "call from [Alice-worker],reply_to[m-0],message_id[m-1]\n\nhello".to_string(),
                text_elements: Vec::new(),
            }]
        );
    }

    #[test]
    fn build_call_message_uses_none_reply_when_missing() {
        let items = build_call_message("Alice-worker", "m-1", None, "hello", false);
        assert_eq!(
            items,
            vec![UserInput::Text {
                text: "call from [Alice-worker],reply_to[none],message_id[m-1]\n\nhello"
                    .to_string(),
                text_elements: Vec::new(),
            }]
        );
    }

    #[test]
    fn normalize_call_content_utf8_keeps_clean_text() {
        let clean = "please review swarm flow and report";
        assert_eq!(normalize_call_content_utf8(clean), clean);
    }

    #[test]
    fn normalize_call_content_utf8_reduces_mojibake_markers() {
        let source = "\u{8bf7}\u{805a}\u{7126} Swarm \
                      \u{6a21}\u{5f0f}\u{7684}\u{4f7f}\u{7528}\u{4f53}\u{9a8c}\u{4e0e}\
                      \u{53ef}\u{9a8c}\u{8bc1}\u{6027}";
        let (garbled, _, _) = GBK.decode(source.as_bytes());
        let garbled = garbled.into_owned();
        let repaired = normalize_call_content_utf8(&garbled);

        assert!(content_repair_score(&repaired) <= content_repair_score(&garbled));
    }

    #[test]
    fn parse_call_args_repairs_mojibake_content() {
        let source = "\u{7edf}\u{4e00} call \
                      \u{6d88}\u{606f}\u{94fe}\u{8def}\u{7684} UTF-8 \
                      \u{7f16}\u{7801}/\u{89e3}\u{7801}";
        let (garbled, _, _) = GBK.decode(source.as_bytes());
        let garbled = garbled.into_owned();
        let arguments = json!({
            "target_agent_name": "Alice-worker",
            "content": garbled,
            "message_id": "m-1"
        })
        .to_string();

        let parsed = parse_call_args(&arguments).expect("parse call args");
        assert!(content_repair_score(&parsed.content) <= content_repair_score(&garbled));
    }
}
