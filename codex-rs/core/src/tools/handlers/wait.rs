use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::protocol::CollabWaitLifecycleState;
use codex_protocol::protocol::CollabWaitTargetEvent;
use codex_protocol::protocol::CollabWaitingBeginEvent;
use codex_protocol::protocol::CollabWaitingEndEvent;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use tokio::time::Instant;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::collab_inbox;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct WaitHandler;

const MAX_WAIT_TIMEOUT_MS: i64 = 60 * 60 * 1000; // 1 hour
const WAIT_TIMEOUT_STREAK_HALT_THRESHOLD: u8 = 5;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    #[serde(default)]
    timeout_ms: Option<i64>,
    #[serde(default)]
    target_agent_id: Option<String>,
    #[serde(default)]
    target_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
struct WaitTarget {
    agent_name: String,
    message_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WaitResult {
    timed_out: bool,
    timeout_streak: u8,
    halt_required: bool,
    elapsed_ms: i64,
    satisfied_targets: Vec<WaitTarget>,
    unsatisfied_targets: Vec<WaitTarget>,
    pending_reply_targets: Vec<WaitTarget>,
    matched_expected_reply: bool,
    stale_or_unrelated_message: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_reply_to_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stale_reply_warning: Option<String>,
    notified_wait_target: Option<WaitNoticeTarget>,
    messages: Vec<collab_inbox::InboxMessage>,
    suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
struct WaitNoticeTarget {
    agent_id: String,
    agent_name: String,
    thread_id: ThreadId,
    message_id: String,
}

#[derive(Debug, Clone)]
struct ResolvedWaitTarget {
    sender_filter: String,
    notice: Option<WaitNoticeTarget>,
}

fn wait_timeout_streaks() -> &'static Mutex<HashMap<ThreadId, u8>> {
    static WAIT_TIMEOUT_STREAKS: OnceLock<Mutex<HashMap<ThreadId, u8>>> = OnceLock::new();
    WAIT_TIMEOUT_STREAKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn update_timeout_streak(sender_thread_id: ThreadId, timed_out: bool) -> u8 {
    let mut guard = wait_timeout_streaks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if timed_out {
        let next = guard
            .get(&sender_thread_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        guard.insert(sender_thread_id, next);
        next
    } else {
        guard.remove(&sender_thread_id);
        0
    }
}

fn resolve_receiver_thread_id(
    session: &crate::codex::Session,
    sender_agent_name: &str,
) -> ThreadId {
    if let Some(thread_id) = session
        .services
        .agent_control
        .thread_id_for_agent_name(sender_agent_name)
    {
        return thread_id;
    }
    if let Ok(thread_id) = ThreadId::from_string(sender_agent_name) {
        return thread_id;
    }
    session.conversation_id
}

fn resolve_target_agent_id(
    session: &crate::codex::Session,
    agent_id: &str,
) -> Result<ThreadId, FunctionCallError> {
    if let Some(thread_id) = session
        .services
        .agent_control
        .thread_id_for_agent_name(agent_id)
    {
        return Ok(thread_id);
    }

    ThreadId::from_string(agent_id).map_err(|_| {
        FunctionCallError::RespondToModel(format!("target_agent_id not found: {agent_id}"))
    })
}

fn resolve_optional_wait_target(
    session: &crate::codex::Session,
    target_id: &str,
) -> Result<Option<ThreadId>, FunctionCallError> {
    if let Some(thread_id) = session
        .services
        .agent_control
        .thread_id_for_agent_name(target_id)
    {
        return Ok(Some(thread_id));
    }
    if let Ok(thread_id) = ThreadId::from_string(target_id) {
        return Ok(Some(thread_id));
    }
    if target_id.contains('-') {
        return Err(FunctionCallError::RespondToModel(format!(
            "target_id not found: {target_id}"
        )));
    }
    Ok(None)
}

fn wait_notice_message_id(wait_call_id: &str) -> String {
    format!("wait-notice-{wait_call_id}")
}

fn wait_notice_content(sender_agent_name: &str, sender_thread_id: ThreadId) -> String {
    format!(
        "[WAIT NOTICE] {sender_agent_name} ({sender_thread_id}) is currently waiting for your result. Please return your result as soon as possible using `call` with the appropriate `reply_to_message_id` if this wait is for a delegated request."
    )
}

async fn notify_wait_target(
    session: &crate::codex::Session,
    target_agent_id: &str,
    target_thread_id: ThreadId,
    sender_agent_name: &str,
    call_id: &str,
) -> Result<WaitNoticeTarget, FunctionCallError> {
    if target_thread_id == session.conversation_id {
        return Err(FunctionCallError::RespondToModel(
            "target_agent_id must not refer to the waiting agent itself".to_string(),
        ));
    }

    let target_agent_name = session
        .services
        .agent_control
        .agent_name_for_thread(target_thread_id)
        .unwrap_or_else(|| crate::agent::UNNAMED_AGENT_NAME.to_string());
    let message_id = wait_notice_message_id(call_id);
    let content = wait_notice_content(sender_agent_name, session.conversation_id);

    session
        .services
        .agent_control
        .send_input(
            target_thread_id,
            vec![UserInput::Text {
                text: content.clone(),
                text_elements: Vec::new(),
            }],
        )
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to notify target_agent_id `{target_agent_id}`: {err}"
            ))
        })?;

    collab_inbox::append_message(
        target_thread_id,
        sender_agent_name.to_string(),
        message_id.clone(),
        None,
        content,
    );

    Ok(WaitNoticeTarget {
        agent_id: target_agent_id.to_string(),
        agent_name: target_agent_name,
        thread_id: target_thread_id,
        message_id,
    })
}

fn pending_reply_targets_for_source(
    session: &crate::codex::Session,
    source_thread_id: ThreadId,
) -> Vec<WaitTarget> {
    collab_inbox::unresolved_required_reply_obligations_from_source(source_thread_id)
        .into_iter()
        .map(|pending| WaitTarget {
            agent_name: pending
                .receiver_thread_id
                .and_then(|thread_id| {
                    session
                        .services
                        .agent_control
                        .agent_name_for_thread(thread_id)
                })
                .unwrap_or_else(|| pending.receiver_inbox_id.clone()),
            message_id: pending.obligation.message_id,
        })
        .collect()
}

#[cfg(test)]
fn reset_timeout_streaks_for_tests() {
    let mut guard = wait_timeout_streaks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.clear();
}

#[async_trait]
impl ToolHandler for WaitHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return false;
        };

        super::parse_arguments::<WaitArgs>(arguments)
            .ok()
            .and_then(|args| args.target_id.or(args.target_agent_id))
            .is_some_and(|agent_id| !agent_id.trim().is_empty())
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
                    "wait handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: WaitArgs = super::parse_arguments(&arguments)?;
        let raw_target_id = args.target_id.or(args.target_agent_id);
        let target_agent_id = match raw_target_id.as_deref().map(str::trim) {
            Some("") => {
                return Err(FunctionCallError::RespondToModel(
                    "target_id must be non-empty when set".to_string(),
                ));
            }
            Some(agent_id) => Some(agent_id),
            None => None,
        };
        let timeout_ms = match args.timeout_ms {
            Some(timeout_ms) if timeout_ms > 0 => timeout_ms.min(MAX_WAIT_TIMEOUT_MS),
            Some(_) => {
                return Err(FunctionCallError::RespondToModel(
                    "timeout_ms must be greater than zero".to_string(),
                ));
            }
            None if target_agent_id.is_some() => 7 * 60 * 1000,
            None => {
                return Err(FunctionCallError::RespondToModel(
                    "timeout_ms is required unless target_id is set".to_string(),
                ));
            }
        };

        let sender_agent_name = session
            .services
            .agent_control
            .agent_name_for_thread(session.conversation_id)
            .unwrap_or_else(|| crate::agent::UNNAMED_AGENT_NAME.to_string());
        let wait_target = if let Some(target_agent_id) = target_agent_id {
            let target_thread_id = resolve_optional_wait_target(session.as_ref(), target_agent_id)?;
            let notice = if let Some(target_thread_id) = target_thread_id {
                Some(
                    notify_wait_target(
                        session.as_ref(),
                        target_agent_id,
                        target_thread_id,
                        &sender_agent_name,
                        &call_id,
                    )
                    .await?,
                )
            } else {
                None
            };
            let sender_filter = notice
                .as_ref()
                .map(|target| target.agent_name.clone())
                .unwrap_or_else(|| target_agent_id.to_string());
            Some(ResolvedWaitTarget {
                sender_filter,
                notice,
            })
        } else {
            None
        };
        let notified_wait_target = wait_target
            .as_ref()
            .and_then(|target| target.notice.clone());
        let begin_targets = wait_target
            .as_ref()
            .and_then(|target| target.notice.as_ref())
            .map(|target| {
                vec![CollabWaitTargetEvent {
                    receiver_thread_id: target.thread_id,
                    receiver_agent_name: target.agent_name.clone(),
                    message_id: target.message_id.clone(),
                    state: CollabWaitLifecycleState::Running,
                    callback_content: None,
                }]
            })
            .unwrap_or_default();
        session
            .send_event(
                &turn,
                CollabWaitingBeginEvent {
                    sender_thread_id: session.conversation_id,
                    sender_agent_name: sender_agent_name.clone(),
                    call_id: call_id.clone(),
                    targets: begin_targets,
                }
                .into(),
            )
            .await;

        let pending_before_wait =
            pending_reply_targets_for_source(session.as_ref(), session.conversation_id);
        if !pending_before_wait.is_empty() {
            collab_inbox::mark_required_reply_obligations_observed_by_source_wait(
                session.conversation_id,
            );
        }
        let pending_before_ids = pending_before_wait
            .iter()
            .map(|target| target.message_id.as_str())
            .collect::<std::collections::HashSet<_>>();

        let started = Instant::now();
        let deadline = started + Duration::from_millis(timeout_ms as u64);
        let (_max_seq, notify) = collab_inbox::subscribe(session.conversation_id);

        let target_agent_name = wait_target
            .as_ref()
            .map(|target| target.sender_filter.as_str());

        // Wait for the next available relevant inbox message. Plain wait preserves
        // FIFO semantics. wait(target_id) only consumes messages from that target,
        // so unrelated Agent/Human input remains in the inbox for the proper waiter.
        let consumed_message = loop {
            let notified = notify.notified();
            let (_max_seq, next_message) = if let Some(target_agent_name) = target_agent_name {
                collab_inbox::pop_message_from_sender(session.conversation_id, target_agent_name)
            } else {
                collab_inbox::pop_next_message(session.conversation_id)
            };
            if next_message.is_some() {
                break next_message;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break None;
            }

            tokio::select! {
                _ = notified => {},
                _ = tokio::time::sleep(remaining) => break None,
            }
        };

        let elapsed_ms = started.elapsed().as_millis().min(i64::MAX as u128) as i64;

        let timed_out = consumed_message.is_none();
        let timeout_streak = update_timeout_streak(session.conversation_id, timed_out);
        let halt_required = timed_out && timeout_streak >= WAIT_TIMEOUT_STREAK_HALT_THRESHOLD;
        let suggestion = if timed_out {
            if halt_required {
                Some(format!(
                    "Timed out after {elapsed_ms}ms (consecutive timeout #{timeout_streak}). If you are waiting on a specific agent reply, prefer read_agent_status for that agent before calling wait again. STOP: do not call wait again in this turn. Immediately send a direct assistant message to the user explaining no inbox message has arrived yet and what action is needed next."
                ))
            } else {
                Some(format!(
                    "Timed out after {elapsed_ms}ms (consecutive timeout #{timeout_streak}). If you are waiting on a specific agent reply, prefer read_agent_status for that agent before calling wait again. If no inbox message has arrived yet, you may wait again. Never use wait to fetch subsequent instructions."
                ))
            }
        } else {
            None
        };

        let mut target_events = Vec::new();
        let mut messages = Vec::new();
        let mut satisfied_targets = Vec::new();
        let mut matched_reply_to_message_id = None;
        let mut matched_expected_reply = false;

        if let Some(msg) = consumed_message {
            if let Some(reply_to_message_id) = msg.reply_to_message_id.as_deref()
                && (pending_before_ids.contains(reply_to_message_id)
                    || collab_inbox::has_resolved_required_reply(
                        resolve_receiver_thread_id(session.as_ref(), &msg.sender_agent_name),
                        session.conversation_id,
                        reply_to_message_id,
                    ))
            {
                matched_expected_reply = true;
                matched_reply_to_message_id = Some(reply_to_message_id.to_string());
            }

            let receiver_thread_id =
                resolve_receiver_thread_id(session.as_ref(), &msg.sender_agent_name);
            let receiver_agent_name = session
                .services
                .agent_control
                .agent_name_for_thread(receiver_thread_id)
                .unwrap_or_else(|| {
                    let trimmed = msg.sender_agent_name.trim();
                    if trimmed.is_empty() {
                        receiver_thread_id.to_string()
                    } else {
                        trimmed.to_string()
                    }
                });

            target_events.push(CollabWaitTargetEvent {
                receiver_thread_id,
                receiver_agent_name,
                message_id: msg.message_id.clone(),
                state: CollabWaitLifecycleState::Completed,
                callback_content: Some(msg.content.clone()),
            });
            satisfied_targets.push(WaitTarget {
                agent_name: msg.sender_agent_name.clone(),
                message_id: msg.message_id.clone(),
            });
            messages.push(msg);
        }

        let pending_reply_targets =
            pending_reply_targets_for_source(session.as_ref(), session.conversation_id);
        if !pending_reply_targets.is_empty() {
            collab_inbox::mark_required_reply_obligations_observed_by_source_wait(
                session.conversation_id,
            );
        }
        let stale_or_unrelated_message =
            !timed_out && !pending_reply_targets.is_empty() && !matched_expected_reply;
        let stale_reply_warning = stale_or_unrelated_message.then(|| {
            let expected = pending_reply_targets
                .iter()
                .map(|target| format!("{}:{}", target.agent_name, target.message_id))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Received a FIFO inbox message, but it did not match any currently pending reply obligation initiated by this agent. Pending replies still unresolved: {expected}. Do not treat this message as satisfying final validation unless its reply_to_message_id matches the expected request."
            )
        });
        let suggestion = if let Some(warning) = stale_reply_warning.clone() {
            Some(warning)
        } else {
            suggestion
        };

        session
            .send_event(
                &turn,
                CollabWaitingEndEvent {
                    sender_thread_id: session.conversation_id,
                    sender_agent_name,
                    call_id: call_id.clone(),
                    timed_out,
                    targets: target_events,
                }
                .into(),
            )
            .await;

        let content = if timed_out {
            serde_json::to_string(&WaitResult {
                timed_out,
                timeout_streak,
                halt_required,
                elapsed_ms,
                satisfied_targets,
                unsatisfied_targets: pending_reply_targets.clone(),
                pending_reply_targets,
                matched_expected_reply,
                stale_or_unrelated_message,
                matched_reply_to_message_id,
                stale_reply_warning,
                notified_wait_target,
                messages,
                suggestion,
            })
            .map_err(|err| {
                FunctionCallError::Fatal(format!("failed to serialize wait result: {err}"))
            })?
        } else {
            messages
                .first()
                .map(|message| message.content.clone())
                .unwrap_or_default()
        };

        Ok(ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success: Some(!timed_out),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::CodexAuth;
    use crate::ThreadManager;
    use crate::agent::PRIMARY_AGENT_NAME;
    use crate::built_in_model_providers;
    use crate::codex::Session;
    use crate::codex::TurnContext;
    use crate::codex::make_session_and_context;
    use crate::protocol::Op;
    use crate::tools::context::ToolPayload;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use serial_test::serial;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio::time::timeout;

    fn reset_wait_test_state() {
        collab_inbox::reset_for_tests();
        reset_timeout_streaks_for_tests();
    }

    fn invocation(
        session: Arc<Session>,
        turn: Arc<TurnContext>,
        args: serde_json::Value,
    ) -> ToolInvocation {
        ToolInvocation {
            session,
            turn,
            tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
            tool_name: "wait".to_string(),
            call_id: "wait-1".to_string(),
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

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_accepts_timeout_only_and_returns_message() {
        reset_wait_test_state();

        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let receiver = session.conversation_id;

        collab_inbox::append_message(
            receiver,
            "Alice-worker".to_string(),
            "m1".to_string(),
            Some("1322".to_string()),
            "reply 1322".to_string(),
        );

        let output = WaitHandler
            .handle(invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                json!({ "timeout_ms": 50 }),
            ))
            .await
            .expect("wait should return");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(true));

        assert_eq!(content, "reply 1322");
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_returns_one_message_per_call_in_fifo_order() {
        reset_wait_test_state();

        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let receiver = session.conversation_id;

        collab_inbox::append_message(
            receiver,
            "Alice-worker".to_string(),
            "m1".to_string(),
            None,
            "first".to_string(),
        );
        collab_inbox::append_message(
            receiver,
            "Bob-worker".to_string(),
            "m2".to_string(),
            None,
            "second".to_string(),
        );

        let first_output = WaitHandler
            .handle(invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                json!({ "timeout_ms": 50 }),
            ))
            .await
            .expect("first wait should return");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(first_content),
            success: first_success,
            ..
        } = first_output
        else {
            panic!("expected function output");
        };
        assert_eq!(first_success, Some(true));
        assert_eq!(first_content, "first");

        let second_output = WaitHandler
            .handle(invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                json!({ "timeout_ms": 50 }),
            ))
            .await
            .expect("second wait should return");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(second_content),
            success: second_success,
            ..
        } = second_output
        else {
            panic!("expected function output");
        };
        assert_eq!(second_success, Some(true));
        assert_eq!(second_content, "second");
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_warns_when_fifo_message_does_not_match_pending_reply() {
        reset_wait_test_state();

        let (session, turn) = make_session_and_context().await;
        let receiver = session.conversation_id;
        let target = ThreadId::new();
        session
            .services
            .agent_control
            .register_agent_name(target, "Dakota-worker")
            .expect("register target agent");
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        collab_inbox::register_required_reply(
            target,
            "wmj-assistant".to_string(),
            receiver,
            "main-dakota-finalcheck".to_string(),
            "please validate final artifact".to_string(),
        );
        collab_inbox::append_message(
            receiver,
            "Dakota-worker".to_string(),
            "old-validate-reply".to_string(),
            Some("main-dakota-validate".to_string()),
            "old repair request".to_string(),
        );

        let output = WaitHandler
            .handle(invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                json!({ "timeout_ms": 50 }),
            ))
            .await
            .expect("wait should return");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(true));

        assert_eq!(content, "old repair request");
        assert!(collab_inbox::has_required_reply_obligation(
            target,
            "main-dakota-finalcheck"
        ));
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_marks_reply_to_pending_obligation_as_matching() {
        reset_wait_test_state();

        let (session, turn) = make_session_and_context().await;
        let receiver = session.conversation_id;
        let target = ThreadId::new();
        session
            .services
            .agent_control
            .register_agent_name(target, "Dakota-worker")
            .expect("register target agent");
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        collab_inbox::register_required_reply(
            target,
            "wmj-assistant".to_string(),
            receiver,
            "main-dakota-finalcheck".to_string(),
            "please validate final artifact".to_string(),
        );
        collab_inbox::append_message(
            receiver,
            "Dakota-worker".to_string(),
            "finalcheck-reply".to_string(),
            Some("main-dakota-finalcheck".to_string()),
            "verified".to_string(),
        );

        let output = WaitHandler
            .handle(invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                json!({ "timeout_ms": 50 }),
            ))
            .await
            .expect("wait should return");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(true));

        assert_eq!(content, "verified");
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_unblocks_when_message_arrives_after_wait_starts() {
        reset_wait_test_state();

        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let receiver = session.conversation_id;

        let handler = WaitHandler;
        let wait_fut = handler.handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            json!({ "timeout_ms": 5000 }),
        ));

        collab_inbox::append_message(
            receiver,
            "Alice-worker".to_string(),
            "m-late".to_string(),
            None,
            "late message".to_string(),
        );

        let output = timeout(Duration::from_secs(1), wait_fut)
            .await
            .expect("wait should complete")
            .expect("wait should succeed");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(true));
        assert_eq!(content, "late message");
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_times_out_when_inbox_stays_empty() {
        reset_wait_test_state();

        let (session, turn) = make_session_and_context().await;
        let output = WaitHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({ "timeout_ms": 20 }),
            ))
            .await
            .expect("wait should return");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(false));

        let json: Value = serde_json::from_str(&content).expect("json");
        assert_eq!(json["timedOut"], Value::Bool(true));
        assert_eq!(json["messages"].as_array().map(std::vec::Vec::len), Some(0));
        assert!(
            json["suggestion"]
                .as_str()
                .unwrap_or_default()
                .contains("Timed out after")
        );
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_target_id_only_consumes_messages_from_that_target() {
        reset_wait_test_state();

        let (mut session, turn) = make_session_and_context().await;
        let manager = thread_manager();
        session.services.agent_control = manager.agent_control();
        session
            .services
            .agent_control
            .register_agent_name(session.conversation_id, PRIMARY_AGENT_NAME)
            .expect("register root agent");
        let agent_control = session.services.agent_control.clone();
        let config = turn.config.as_ref().clone();
        let target_thread = manager
            .start_thread(config.clone())
            .await
            .expect("start target thread");
        let unrelated_thread = manager
            .start_thread(config)
            .await
            .expect("start unrelated thread");
        session
            .services
            .agent_control
            .register_agent_name(target_thread.thread_id, "Alice-worker")
            .expect("register target agent");
        session
            .services
            .agent_control
            .register_agent_name(unrelated_thread.thread_id, "Bob-worker")
            .expect("register unrelated agent");
        let receiver = session.conversation_id;

        collab_inbox::append_message(
            receiver,
            "Bob-worker".to_string(),
            "bob-unrelated".to_string(),
            None,
            "this must remain queued".to_string(),
        );
        collab_inbox::append_message(
            receiver,
            "Alice-worker".to_string(),
            "alice-target".to_string(),
            None,
            "target reply".to_string(),
        );

        let output = WaitHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({ "timeout_ms": 50, "target_id": "Alice-worker" }),
            ))
            .await
            .expect("wait should return target message");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(true));

        assert_eq!(content, "target reply");

        let (_seq, remaining) = collab_inbox::pop_next_message(receiver);
        let remaining = remaining.expect("unrelated message should remain queued");
        assert_eq!(remaining.sender_agent_name, "Bob-worker");
        assert_eq!(remaining.message_id, "bob-unrelated");

        let _ = agent_control
            .shutdown_agent(target_thread.thread_id)
            .await
            .expect("shutdown target agent");
        let _ = agent_control
            .shutdown_agent(unrelated_thread.thread_id)
            .await
            .expect("shutdown unrelated agent");
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_target_id_consumes_logical_human_message() {
        reset_wait_test_state();

        let (mut session, turn) = make_session_and_context().await;
        session
            .services
            .agent_control
            .register_agent_name(session.conversation_id, PRIMARY_AGENT_NAME)
            .expect("register root agent");
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let receiver = session.conversation_id.to_string();

        collab_inbox::append_logical_message(
            "user_ceo",
            "user_ceo".to_string(),
            "human-reply-1".to_string(),
            true,
            Some("request-1".to_string()),
            "approved".to_string(),
        );
        collab_inbox::append_logical_message(
            &receiver,
            "agent_a".to_string(),
            "agent-reply-1".to_string(),
            false,
            None,
            "keep me queued".to_string(),
        );

        let output = WaitHandler
            .handle(invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                json!({ "timeout_ms": 50, "target_id": "user_ceo" }),
            ))
            .await
            .expect("wait should return human message");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(true));
        assert_eq!(content, "approved");

        let (_seq, remaining) = collab_inbox::pop_next_logical(&receiver);
        let remaining = remaining.expect("agent message should remain queued");
        assert_eq!(remaining.message_id, "agent-reply-1");
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_with_target_agent_id_notifies_target_agent() {
        reset_wait_test_state();

        let (mut session, turn) = make_session_and_context().await;
        let manager = thread_manager();
        session.services.agent_control = manager.agent_control();
        session
            .services
            .agent_control
            .register_agent_name(session.conversation_id, PRIMARY_AGENT_NAME)
            .expect("register root agent");
        let agent_control = session.services.agent_control.clone();
        let config = turn.config.as_ref().clone();
        let thread = manager.start_thread(config).await.expect("start thread");
        let target_thread_id = thread.thread_id;
        session
            .services
            .agent_control
            .register_agent_name(target_thread_id, "Alice-worker")
            .expect("register target agent");
        let expected_notice = wait_notice_content(PRIMARY_AGENT_NAME, session.conversation_id);

        let output = WaitHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({ "timeout_ms": 20, "target_agent_id": "Alice-worker" }),
            ))
            .await
            .expect("wait should return");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(false));

        let json: Value = serde_json::from_str(&content).expect("json");
        assert_eq!(json["timedOut"], Value::Bool(true));
        assert_eq!(
            json["notifiedWaitTarget"]["agentId"],
            Value::from("Alice-worker")
        );
        assert_eq!(
            json["notifiedWaitTarget"]["agentName"],
            Value::from("Alice-worker")
        );
        assert_eq!(
            json["notifiedWaitTarget"]["threadId"],
            Value::from(target_thread_id.to_string())
        );
        assert_eq!(
            json["notifiedWaitTarget"]["messageId"],
            Value::from("wait-notice-wait-1")
        );

        let sent_notice = manager.captured_ops().into_iter().any(|(id, op)| {
            id == target_thread_id
                && matches!(
                    op,
                    Op::UserInput {
                        items,
                        origin: codex_protocol::protocol::UserInputOrigin::AgentCall,
                        final_output_json_schema: None,
                    } if items == vec![UserInput::Text {
                        text: expected_notice.clone(),
                        text_elements: Vec::new(),
                    }]
                )
        });
        assert!(
            sent_notice,
            "wait should send a user input notice to the target agent"
        );

        let (_seq, inbox_message) = collab_inbox::pop_next_message(target_thread_id);
        let inbox_message = inbox_message.expect("wait notice inbox message");
        assert_eq!(inbox_message.sender_agent_name, PRIMARY_AGENT_NAME);
        assert_eq!(inbox_message.message_id, "wait-notice-wait-1");
        assert_eq!(inbox_message.reply_to_message_id, None);
        assert_eq!(inbox_message.content, expected_notice);

        let _ = agent_control
            .shutdown_agent(target_thread_id)
            .await
            .expect("shutdown agent");
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_rejects_empty_target_agent_id() {
        reset_wait_test_state();

        let (session, turn) = make_session_and_context().await;
        let output = WaitHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({ "timeout_ms": 20, "target_agent_id": "   " }),
            ))
            .await;
        let err = match output {
            Ok(_) => panic!("wait should reject empty target_agent_id"),
            Err(err) => err,
        };
        assert_eq!(
            err,
            FunctionCallError::RespondToModel("target_id must be non-empty when set".to_string())
        );
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_rejects_non_positive_timeout() {
        reset_wait_test_state();

        let (session, turn) = make_session_and_context().await;
        let output = WaitHandler
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                json!({ "timeout_ms": 0 }),
            ))
            .await;
        let err = match output {
            Ok(_) => panic!("wait should reject non-positive timeout"),
            Err(err) => err,
        };
        assert_eq!(
            err,
            FunctionCallError::RespondToModel("timeout_ms must be greater than zero".to_string())
        );
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_requires_halt_after_five_consecutive_timeouts() {
        reset_wait_test_state();

        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        for attempt in 1..=5 {
            let output = WaitHandler
                .handle(invocation(
                    Arc::clone(&session),
                    Arc::clone(&turn),
                    json!({ "timeout_ms": 20 }),
                ))
                .await
                .expect("wait should return");
            let ToolOutput::Function {
                body: FunctionCallOutputBody::Text(content),
                success,
                ..
            } = output
            else {
                panic!("expected function output");
            };
            assert_eq!(success, Some(false));

            let json: Value = serde_json::from_str(&content).expect("json");
            assert_eq!(json["timedOut"], Value::Bool(true));
            assert_eq!(json["timeoutStreak"], Value::from(attempt));
            let halt_required = json["haltRequired"].as_bool().unwrap_or(false);
            if attempt < 5 {
                assert!(!halt_required, "attempt {attempt} should not force halt");
            } else {
                assert!(halt_required, "attempt {attempt} should force halt");
                assert!(
                    json["suggestion"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("STOP: do not call wait again"),
                );
            }
        }
    }

    #[tokio::test]
    #[serial(wait_handler)]
    async fn wait_timeout_streak_resets_after_success() {
        reset_wait_test_state();

        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let receiver = session.conversation_id;

        for _ in 0..3 {
            let output = WaitHandler
                .handle(invocation(
                    Arc::clone(&session),
                    Arc::clone(&turn),
                    json!({ "timeout_ms": 20 }),
                ))
                .await
                .expect("wait should return");
            let ToolOutput::Function {
                body: FunctionCallOutputBody::Text(content),
                ..
            } = output
            else {
                panic!("expected function output");
            };
            let json: Value = serde_json::from_str(&content).expect("json");
            assert_eq!(json["timedOut"], Value::Bool(true));
        }

        collab_inbox::append_message(
            receiver,
            "Alice-worker".to_string(),
            "m1".to_string(),
            None,
            "reply".to_string(),
        );

        let success_output = WaitHandler
            .handle(invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                json!({ "timeout_ms": 50 }),
            ))
            .await
            .expect("wait should return");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(success_content),
            success,
            ..
        } = success_output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(true));
        assert_eq!(success_content, "reply");

        let timeout_output = WaitHandler
            .handle(invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                json!({ "timeout_ms": 20 }),
            ))
            .await
            .expect("wait should return");
        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(timeout_content),
            success,
            ..
        } = timeout_output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(false));
        let timeout_json: Value = serde_json::from_str(&timeout_content).expect("json");
        assert_eq!(timeout_json["timedOut"], Value::Bool(true));
        assert_eq!(timeout_json["timeoutStreak"], Value::from(1));
        assert_eq!(timeout_json["haltRequired"], Value::Bool(false));
    }
}
