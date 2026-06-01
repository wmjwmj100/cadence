use async_trait::async_trait;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputBody;
use serde::Deserialize;
use serde::Serialize;

use crate::agent::external_state_from_status;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::collab_inbox;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct ReadAgentStatusHandler;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadAgentStatusArgs {
    agent_id: String,
}

#[derive(Debug, Serialize)]
struct AgentStatusResponse {
    agent_id: String,
    status: String,
    lifecycle_status: String,
    waiting_reason: Option<String>,
    recent_summaries: Vec<String>,
}

const MAX_SUMMARIES: usize = 10;

#[async_trait]
impl ToolHandler for ReadAgentStatusHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolPayload::Function { ref arguments } = invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "read_agent_status handler received unsupported payload".to_string(),
            ));
        };

        let args: ReadAgentStatusArgs = parse_arguments(arguments)?;
        let agent_id = args.agent_id.trim();

        if agent_id.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "agent_id must be non-empty".to_string(),
            ));
        }

        let thread_id = resolve_thread_id(&invocation, agent_id)?;
        let status = invocation
            .session
            .services
            .agent_control
            .get_status(thread_id)
            .await;
        let pending_replies =
            collab_inbox::unresolved_required_reply_obligations_from_source(thread_id);
        let waiting_reason = pending_replies.first().map(|pending| {
            let receiver_name = pending
                .receiver_thread_id
                .and_then(|thread_id| {
                    invocation
                        .session
                        .services
                        .agent_control
                        .agent_name_for_thread(thread_id)
                })
                .unwrap_or_else(|| pending.receiver_inbox_id.clone());
            format!(
                "waiting for reply from {receiver_name} to message_id {}",
                pending.obligation.message_id
            )
        });
        let external_state = external_state_from_status(&status, !pending_replies.is_empty());
        let recent_summaries = invocation
            .session
            .services
            .agent_control
            .agent_work_status(thread_id)
            .map(|work_status| {
                latest_summaries(
                    work_status
                        .entries
                        .into_iter()
                        .map(|entry| entry.summary)
                        .collect(),
                )
            })
            .unwrap_or_default();

        let response = AgentStatusResponse {
            agent_id: agent_id.to_string(),
            status: external_state.as_str().to_string(),
            lifecycle_status: format!("{:?}", status),
            waiting_reason,
            recent_summaries,
        };

        let content = serde_json::to_string_pretty(&response).map_err(|err| {
            FunctionCallError::Fatal(format!("failed to serialize response: {err}"))
        })?;

        Ok(ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success: Some(true),
        })
    }
}

fn resolve_thread_id(
    invocation: &ToolInvocation,
    agent_id: &str,
) -> Result<ThreadId, FunctionCallError> {
    if let Some(thread_id) = invocation
        .session
        .services
        .agent_control
        .thread_id_for_agent_name(agent_id)
    {
        return Ok(thread_id);
    }

    ThreadId::from_string(agent_id)
        .map_err(|_| FunctionCallError::RespondToModel(format!("agent_id not found: {agent_id}")))
}

fn latest_summaries(entries: Vec<String>) -> Vec<String> {
    let mut summaries = entries
        .into_iter()
        .rev()
        .take(MAX_SUMMARIES)
        .collect::<Vec<_>>();
    summaries.reverse();
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::codex::make_session_and_context;
    use crate::tools::context::ToolPayload;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn invocation(
        session: Arc<crate::codex::Session>,
        turn: Arc<crate::codex::TurnContext>,
        args: serde_json::Value,
    ) -> ToolInvocation {
        ToolInvocation {
            session,
            turn,
            tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
            tool_name: "read_agent_status".to_string(),
            call_id: "read-status-1".to_string(),
            payload: ToolPayload::Function {
                arguments: args.to_string(),
            },
        }
    }

    #[test]
    fn latest_summaries_keeps_only_ten_most_recent_entries() {
        let entries = (1..=12)
            .map(|idx| format!("entry-{idx}"))
            .collect::<Vec<_>>();

        let summaries = latest_summaries(entries);

        assert_eq!(
            summaries,
            (3..=12)
                .map(|idx| format!("entry-{idx}"))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn read_agent_status_returns_target_agent_recent_summaries() {
        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let target_thread_id = ThreadId::new();
        session
            .services
            .agent_control
            .register_agent_name(target_thread_id, "Alice-worker")
            .expect("register agent name");
        for idx in 1..=12 {
            session
                .services
                .agent_control
                .record_agent_work_summary(target_thread_id, format!("entry-{idx}"));
        }

        let output = ReadAgentStatusHandler
            .handle(invocation(
                session,
                turn,
                json!({
                    "agent_id": "Alice-worker"
                }),
            ))
            .await
            .expect("read_agent_status should succeed");

        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(true));

        let json: Value = serde_json::from_str(&content).expect("valid json");
        assert_eq!(json["status"], "idle");
        assert_eq!(json["lifecycle_status"], "NotFound");
        assert!(json["waiting_reason"].is_null());
        let summaries = json["recent_summaries"]
            .as_array()
            .expect("recent_summaries should be an array")
            .iter()
            .map(|entry| entry.as_str().expect("summary should be string"))
            .collect::<Vec<_>>();
        assert_eq!(
            summaries,
            (3..=12)
                .map(|idx| format!("entry-{idx}"))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn read_agent_status_reports_waiting_without_blocked_state() {
        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let source_thread_id = ThreadId::new();
        let receiver_thread_id = ThreadId::new();
        session
            .services
            .agent_control
            .register_agent_name(source_thread_id, "Source-worker")
            .expect("register source agent name");
        session
            .services
            .agent_control
            .register_agent_name(receiver_thread_id, "Receiver-worker")
            .expect("register receiver agent name");
        collab_inbox::register_required_reply(
            receiver_thread_id,
            "Source-worker".to_string(),
            source_thread_id,
            "source-receiver-topic".to_string(),
            "please reply".to_string(),
        );

        let output = ReadAgentStatusHandler
            .handle(invocation(
                session,
                turn,
                json!({
                    "agent_id": "Source-worker"
                }),
            ))
            .await
            .expect("read_agent_status should succeed");

        let ToolOutput::Function {
            body: FunctionCallOutputBody::Text(content),
            success,
            ..
        } = output
        else {
            panic!("expected function output");
        };
        assert_eq!(success, Some(true));

        let json: Value = serde_json::from_str(&content).expect("valid json");
        assert_eq!(json["status"], "waiting");
        assert!(
            json["waiting_reason"]
                .as_str()
                .unwrap()
                .contains("Receiver-worker")
        );
        assert_ne!(json["status"], "blocked");
    }
}
