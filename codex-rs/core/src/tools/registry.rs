use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::client_common::tools::ToolSpec;
use crate::features::Feature;
use crate::function_tool::FunctionCallError;
use crate::protocol::SandboxPolicy;
use crate::sandbox_tags::sandbox_tag;
use crate::tools::TOOL_SUMMARY_ARGUMENT;
use crate::tools::TOOL_SUMMARY_ARGUMENT_DESCRIPTION;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::spec::JsonSchema;
use async_trait::async_trait;
use codex_hooks::HookEvent;
use codex_hooks::HookEventAfterToolUse;
use codex_hooks::HookPayload;
use codex_hooks::HookToolInput;
use codex_hooks::HookToolInputLocalShell;
use codex_hooks::HookToolKind;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::AgentWorkSummaryEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_utils_readiness::Readiness;
use serde_json::Value as JsonValue;
use tracing::warn;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolKind {
    Function,
    Mcp,
}

#[async_trait]
pub trait ToolHandler: Send + Sync {
    fn kind(&self) -> ToolKind;

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(
            (self.kind(), payload),
            (ToolKind::Function, ToolPayload::Function { .. })
                | (ToolKind::Mcp, ToolPayload::Mcp { .. })
        )
    }

    /// Returns `true` if the [ToolInvocation] *might* mutate the environment of the
    /// user (through file system, OS operations, ...).
    /// This function must remains defensive and return `true` if a doubt exist on the
    /// exact effect of a ToolInvocation.
    async fn is_mutating(&self, _invocation: &ToolInvocation) -> bool {
        false
    }

    /// Perform the actual [ToolInvocation] and returns a [ToolOutput] containing
    /// the final output to return to the model.
    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError>;
}

pub struct ToolRegistry {
    handlers: HashMap<String, Arc<dyn ToolHandler>>,
}

impl ToolRegistry {
    pub fn new(handlers: HashMap<String, Arc<dyn ToolHandler>>) -> Self {
        Self { handlers }
    }

    pub fn handler(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.handlers.get(name).map(Arc::clone)
    }

    // TODO(jif) for dynamic tools.
    // pub fn register(&mut self, name: impl Into<String>, handler: Arc<dyn ToolHandler>) {
    //     let name = name.into();
    //     if self.handlers.insert(name.clone(), handler).is_some() {
    //         warn!("overwriting handler for tool {name}");
    //     }
    // }

    pub async fn dispatch(
        &self,
        mut invocation: ToolInvocation,
    ) -> Result<ResponseInputItem, FunctionCallError> {
        let tool_name = invocation.tool_name.clone();
        let call_id_owned = invocation.call_id.clone();
        let otel = invocation.turn.otel_manager.clone();
        let metric_tags = metric_tags(&invocation);

        let handler = match self.handler(tool_name.as_ref()) {
            Some(handler) => handler,
            None => {
                let message =
                    unsupported_tool_call_message(&invocation.payload, tool_name.as_ref());
                otel.tool_result_with_tags(
                    tool_name.as_ref(),
                    &call_id_owned,
                    invocation.payload.log_payload().as_ref(),
                    Duration::ZERO,
                    false,
                    &message,
                    &metric_tags,
                );
                return Err(FunctionCallError::RespondToModel(message));
            }
        };

        if !handler.matches_kind(&invocation.payload) {
            let message = format!("tool {tool_name} invoked with incompatible payload");
            otel.tool_result_with_tags(
                tool_name.as_ref(),
                &call_id_owned,
                invocation.payload.log_payload().as_ref(),
                Duration::ZERO,
                false,
                &message,
                &metric_tags,
            );
            return Err(FunctionCallError::Fatal(message));
        }

        capture_work_summary_and_strip_payload(&mut invocation).await?;
        let payload_for_response = invocation.payload.clone();
        let log_payload = payload_for_response.log_payload();
        let is_mutating = handler.is_mutating(&invocation).await;
        let output_cell = tokio::sync::Mutex::new(None);
        let invocation_for_tool = invocation.clone();

        let started = Instant::now();
        let result = otel
            .log_tool_result_with_tags(
                tool_name.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                &metric_tags,
                || {
                    let handler = handler.clone();
                    let output_cell = &output_cell;
                    async move {
                        if is_mutating {
                            tracing::trace!("waiting for tool gate");
                            invocation_for_tool.turn.tool_call_gate.wait_ready().await;
                            tracing::trace!("tool gate released");
                        }
                        match handler.handle(invocation_for_tool).await {
                            Ok(output) => {
                                let preview = output.log_preview();
                                let success = output.success_for_logging();
                                let mut guard = output_cell.lock().await;
                                *guard = Some(output);
                                Ok((preview, success))
                            }
                            Err(err) => Err(err),
                        }
                    }
                },
            )
            .await;
        let duration = started.elapsed();
        let (output_preview, success) = match &result {
            Ok((preview, success)) => (preview.clone(), *success),
            Err(err) => (err.to_string(), false),
        };
        dispatch_after_tool_use_hook(AfterToolUseHookDispatch {
            invocation: &invocation,
            output_preview,
            success,
            executed: true,
            duration,
            mutating: is_mutating,
        })
        .await;

        match result {
            Ok(_) => {
                let mut guard = output_cell.lock().await;
                let output = guard.take().ok_or_else(|| {
                    FunctionCallError::Fatal("tool produced no output".to_string())
                })?;
                Ok(output.into_response(&call_id_owned, &payload_for_response))
            }
            Err(err) => Err(err),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfiguredToolSpec {
    pub spec: ToolSpec,
    pub supports_parallel_tool_calls: bool,
}

impl ConfiguredToolSpec {
    pub fn new(spec: ToolSpec, supports_parallel_tool_calls: bool) -> Self {
        Self {
            spec,
            supports_parallel_tool_calls,
        }
    }
}

pub struct ToolRegistryBuilder {
    handlers: HashMap<String, Arc<dyn ToolHandler>>,
    specs: Vec<ConfiguredToolSpec>,
}

impl ToolRegistryBuilder {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            specs: Vec::new(),
        }
    }

    pub fn push_spec(&mut self, spec: ToolSpec) {
        self.push_spec_with_parallel_support(spec, false);
    }

    pub fn push_spec_with_parallel_support(
        &mut self,
        spec: ToolSpec,
        supports_parallel_tool_calls: bool,
    ) {
        self.specs.push(ConfiguredToolSpec::new(
            add_required_summary_parameter(spec),
            supports_parallel_tool_calls,
        ));
    }

    pub fn register_handler(&mut self, name: impl Into<String>, handler: Arc<dyn ToolHandler>) {
        let name = name.into();
        if self
            .handlers
            .insert(name.clone(), handler.clone())
            .is_some()
        {
            warn!("overwriting handler for tool {name}");
        }
    }

    // TODO(jif) for dynamic tools.
    // pub fn register_many<I>(&mut self, names: I, handler: Arc<dyn ToolHandler>)
    // where
    //     I: IntoIterator,
    //     I::Item: Into<String>,
    // {
    //     for name in names {
    //         let name = name.into();
    //         if self
    //             .handlers
    //             .insert(name.clone(), handler.clone())
    //             .is_some()
    //         {
    //             warn!("overwriting handler for tool {name}");
    //         }
    //     }
    // }

    pub fn build(self) -> (Vec<ConfiguredToolSpec>, ToolRegistry) {
        let registry = ToolRegistry::new(self.handlers);
        (self.specs, registry)
    }
}

fn metric_tags(invocation: &ToolInvocation) -> [(&'static str, &'static str); 2] {
    [
        (
            "sandbox",
            sandbox_tag(
                &invocation.turn.sandbox_policy,
                invocation.turn.windows_sandbox_level,
                invocation
                    .turn
                    .features
                    .enabled(Feature::UseLinuxSandboxBwrap),
            ),
        ),
        (
            "sandbox_policy",
            sandbox_policy_tag(&invocation.turn.sandbox_policy),
        ),
    ]
}

async fn capture_work_summary_and_strip_payload(
    invocation: &mut ToolInvocation,
) -> Result<(), FunctionCallError> {
    let (summary, sanitized_payload) = match &invocation.payload {
        ToolPayload::Function { arguments } => {
            let (summary, sanitized_arguments) = parse_summary_and_strip(arguments)?;
            (
                Some(summary),
                ToolPayload::Function {
                    arguments: sanitized_arguments,
                },
            )
        }
        ToolPayload::Mcp {
            server,
            tool,
            raw_arguments,
        } => {
            let (summary, sanitized_arguments) = parse_summary_and_strip(raw_arguments)?;
            (
                Some(summary),
                ToolPayload::Mcp {
                    server: server.clone(),
                    tool: tool.clone(),
                    raw_arguments: sanitized_arguments,
                },
            )
        }
        _ => (None, invocation.payload.clone()),
    };

    invocation.payload = sanitized_payload;

    let Some(summary) = summary else {
        return Ok(());
    };

    ensure_current_agent_has_name(invocation);
    let thread_id = invocation.session.conversation_id;
    invocation
        .session
        .services
        .agent_control
        .record_agent_work_summary(thread_id, summary.clone());

    let agent_name = invocation
        .session
        .services
        .agent_control
        .agent_name_for_thread(thread_id)
        .unwrap_or_else(|| crate::agent::UNNAMED_AGENT_NAME.to_string());
    invocation
        .session
        .send_event(
            &invocation.turn,
            EventMsg::AgentWorkSummary(AgentWorkSummaryEvent {
                thread_id,
                agent_name,
                summary,
            }),
        )
        .await;

    Ok(())
}

fn ensure_current_agent_has_name(invocation: &ToolInvocation) {
    if invocation
        .session
        .services
        .agent_control
        .agent_name_for_thread(invocation.session.conversation_id)
        .is_some()
    {
        return;
    }

    if !matches!(invocation.turn.session_source, SessionSource::SubAgent(_)) {
        let _ = invocation
            .session
            .services
            .agent_control
            .register_agent_name(
                invocation.session.conversation_id,
                crate::agent::PRIMARY_AGENT_NAME,
            );
    }
}

fn parse_summary_and_strip(arguments: &str) -> Result<(String, String), FunctionCallError> {
    let value: JsonValue = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
    })?;

    let Some(mut obj) = value.as_object().cloned() else {
        return Err(FunctionCallError::RespondToModel(
            "failed to parse function arguments: expected JSON object".to_string(),
        ));
    };

    let summary_value = obj.remove(TOOL_SUMMARY_ARGUMENT).ok_or_else(|| {
        FunctionCallError::RespondToModel(format!("{TOOL_SUMMARY_ARGUMENT} is required"))
    })?;
    let JsonValue::String(summary) = summary_value else {
        return Err(FunctionCallError::RespondToModel(format!(
            "{TOOL_SUMMARY_ARGUMENT} must be a string"
        )));
    };
    let summary = summary.trim().to_string();
    if summary.is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "{TOOL_SUMMARY_ARGUMENT} must be non-empty"
        )));
    }
    if summary.chars().count() > crate::tools::TOOL_SUMMARY_MAX_CHARS {
        return Err(FunctionCallError::RespondToModel(format!(
            "{TOOL_SUMMARY_ARGUMENT} is too long (max {} characters)",
            crate::tools::TOOL_SUMMARY_MAX_CHARS
        )));
    }

    let sanitized_arguments = serde_json::to_string(&JsonValue::Object(obj)).map_err(|err| {
        FunctionCallError::Fatal(format!("failed to serialize function arguments: {err}"))
    })?;
    Ok((summary, sanitized_arguments))
}

fn add_required_summary_parameter(spec: ToolSpec) -> ToolSpec {
    let ToolSpec::Function(mut function) = spec else {
        return spec;
    };
    let JsonSchema::Object {
        properties,
        required,
        ..
    } = &mut function.parameters
    else {
        return ToolSpec::Function(function);
    };

    properties
        .entry(TOOL_SUMMARY_ARGUMENT.to_string())
        .or_insert_with(|| JsonSchema::String {
            description: Some(TOOL_SUMMARY_ARGUMENT_DESCRIPTION.to_string()),
        });

    let required = required.get_or_insert_with(Vec::new);
    if !required.iter().any(|name| name == TOOL_SUMMARY_ARGUMENT) {
        required.push(TOOL_SUMMARY_ARGUMENT.to_string());
    }

    ToolSpec::Function(function)
}

fn unsupported_tool_call_message(payload: &ToolPayload, tool_name: &str) -> String {
    match payload {
        ToolPayload::Custom { .. } => format!("unsupported custom tool call: {tool_name}"),
        _ => format!("unsupported call: {tool_name}"),
    }
}

fn sandbox_policy_tag(policy: &SandboxPolicy) -> &'static str {
    match policy {
        SandboxPolicy::ReadOnly { .. } => "read-only",
        SandboxPolicy::WorkspaceWrite { .. } => "workspace-write",
        SandboxPolicy::DangerFullAccess => "danger-full-access",
        SandboxPolicy::ExternalSandbox { .. } => "external-sandbox",
    }
}

// Hooks use a separate wire-facing input type so hook payload JSON stays stable
// and decoupled from core's internal tool runtime representation.
impl From<&ToolPayload> for HookToolInput {
    fn from(payload: &ToolPayload) -> Self {
        match payload {
            ToolPayload::Function { arguments } => HookToolInput::Function {
                arguments: arguments.clone(),
            },
            ToolPayload::Custom { input } => HookToolInput::Custom {
                input: input.clone(),
            },
            ToolPayload::LocalShell { params } => HookToolInput::LocalShell {
                params: HookToolInputLocalShell {
                    command: params.command.clone(),
                    workdir: params.workdir.clone(),
                    timeout_ms: params.timeout_ms,
                    sandbox_permissions: params.sandbox_permissions,
                    prefix_rule: params.prefix_rule.clone(),
                    justification: params.justification.clone(),
                },
            },
            ToolPayload::Mcp {
                server,
                tool,
                raw_arguments,
            } => HookToolInput::Mcp {
                server: server.clone(),
                tool: tool.clone(),
                arguments: raw_arguments.clone(),
            },
        }
    }
}

fn hook_tool_kind(tool_input: &HookToolInput) -> HookToolKind {
    match tool_input {
        HookToolInput::Function { .. } => HookToolKind::Function,
        HookToolInput::Custom { .. } => HookToolKind::Custom,
        HookToolInput::LocalShell { .. } => HookToolKind::LocalShell,
        HookToolInput::Mcp { .. } => HookToolKind::Mcp,
    }
}

struct AfterToolUseHookDispatch<'a> {
    invocation: &'a ToolInvocation,
    output_preview: String,
    success: bool,
    executed: bool,
    duration: Duration,
    mutating: bool,
}

async fn dispatch_after_tool_use_hook(dispatch: AfterToolUseHookDispatch<'_>) {
    let AfterToolUseHookDispatch { invocation, .. } = dispatch;
    let session = invocation.session.as_ref();
    let turn = invocation.turn.as_ref();
    let tool_input = HookToolInput::from(&invocation.payload);
    session
        .hooks()
        .dispatch(HookPayload {
            session_id: session.conversation_id,
            cwd: turn.cwd.clone(),
            triggered_at: chrono::Utc::now(),
            hook_event: HookEvent::AfterToolUse {
                event: HookEventAfterToolUse {
                    turn_id: turn.sub_id.clone(),
                    call_id: invocation.call_id.clone(),
                    tool_name: invocation.tool_name.clone(),
                    tool_kind: hook_tool_kind(&tool_input),
                    tool_input,
                    executed: dispatch.executed,
                    success: dispatch.success,
                    duration_ms: u64::try_from(dispatch.duration.as_millis()).unwrap_or(u64::MAX),
                    mutating: dispatch.mutating,
                    sandbox: sandbox_tag(
                        &turn.sandbox_policy,
                        turn.windows_sandbox_level,
                        turn.features.enabled(Feature::UseLinuxSandboxBwrap),
                    )
                    .to_string(),
                    sandbox_policy: sandbox_policy_tag(&turn.sandbox_policy).to_string(),
                    output_preview: dispatch.output_preview.clone(),
                },
            },
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::client_common::tools::ResponsesApiTool;
    use crate::codex::make_session_and_context;
    use crate::tools::context::ToolPayload;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[test]
    fn parse_summary_and_strip_removes_summary_and_trims_text() {
        let (summary, stripped) =
            parse_summary_and_strip(r#"{"summary":"  working on tests  ","path":"src/main.rs"}"#)
                .expect("summary should parse");

        assert_eq!(summary, "working on tests");
        assert_eq!(stripped, r#"{"path":"src/main.rs"}"#);
    }

    #[test]
    fn parse_summary_and_strip_rejects_missing_summary() {
        let err = parse_summary_and_strip(r#"{"path":"src/main.rs"}"#)
            .expect_err("missing summary should fail");
        assert_eq!(
            err,
            FunctionCallError::RespondToModel("summary is required".to_string())
        );
    }

    #[test]
    fn parse_summary_and_strip_rejects_blank_summary() {
        let err =
            parse_summary_and_strip(r#"{"summary":"   "}"#).expect_err("blank summary should fail");
        assert_eq!(
            err,
            FunctionCallError::RespondToModel("summary must be non-empty".to_string())
        );
    }

    #[test]
    fn parse_summary_and_strip_rejects_over_limit_summary() {
        let summary = "a".repeat(crate::tools::TOOL_SUMMARY_MAX_CHARS + 1);
        let arguments = serde_json::json!({
            "summary": summary,
            "path": "src/main.rs",
        })
        .to_string();

        let err = parse_summary_and_strip(&arguments).expect_err("over-limit summary should fail");
        assert_eq!(
            err,
            FunctionCallError::RespondToModel(format!(
                "summary is too long (max {} characters)",
                crate::tools::TOOL_SUMMARY_MAX_CHARS
            ))
        );
    }

    #[test]
    fn add_required_summary_parameter_marks_summary_required() {
        let spec = ToolSpec::Function(ResponsesApiTool {
            name: "demo".to_string(),
            description: "demo".to_string(),
            strict: false,
            parameters: JsonSchema::Object {
                properties: std::collections::BTreeMap::new(),
                required: None,
                additional_properties: None,
            },
        });

        let ToolSpec::Function(updated) = add_required_summary_parameter(spec) else {
            panic!("expected function tool");
        };
        let JsonSchema::Object {
            properties,
            required,
            ..
        } = updated.parameters
        else {
            panic!("expected object schema");
        };

        assert!(properties.contains_key(TOOL_SUMMARY_ARGUMENT));
        assert_eq!(required, Some(vec![TOOL_SUMMARY_ARGUMENT.to_string()]));
    }

    #[tokio::test]
    async fn capture_work_summary_records_current_agent_and_strips_payload_summary() {
        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let mut invocation = ToolInvocation {
            session: Arc::clone(&session),
            turn,
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-1".to_string(),
            tool_name: "shell_command".to_string(),
            payload: ToolPayload::Function {
                arguments: r#"{"summary":"searching docs","command":"Get-ChildItem"}"#.to_string(),
            },
        };

        capture_work_summary_and_strip_payload(&mut invocation)
            .await
            .expect("capture should succeed");

        let ToolPayload::Function { arguments } = invocation.payload else {
            panic!("expected function payload");
        };
        assert_eq!(arguments, r#"{"command":"Get-ChildItem"}"#);

        let statuses = session
            .services
            .agent_control
            .other_agents_work_status(ThreadId::new());
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].agent_name, crate::agent::PRIMARY_AGENT_NAME);
        assert_eq!(statuses[0].entries.len(), 1);
        assert_eq!(statuses[0].entries[0].summary, "searching docs");
    }
}
