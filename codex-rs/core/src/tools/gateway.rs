use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use serde::Deserialize;
use serde::Serialize;

use crate::function_tool::FunctionCallError;
use crate::office::GatewayDecision;
use crate::office::ToolCapabilityPolicy;
use crate::prompt_paths::prompt_cwd_for_gateway;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolRegistry;

const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Middleware boundary between Agent Runtime tool intents and execution backends.
///
/// The first production slice deliberately delegates to the existing registry so
/// Swarm/tool behavior remains stable. Future HTTP/Docker routing, capability
/// checks, and connector policies should be added here instead of leaking Docker
/// or sandbox details back into Agent Runtime.
pub struct ToolGateway {
    capability_policy: ToolCapabilityPolicy,
    backend: ToolGatewayBackend,
    registry: ToolRegistry,
}

impl ToolGateway {
    pub fn new(registry: ToolRegistry) -> Self {
        Self::with_capability_policy(registry, ToolCapabilityPolicy::allow_all())
    }

    pub fn from_env(registry: ToolRegistry) -> Self {
        Self::from_env_values(
            registry,
            std::env::var("CODEX_TOOL_GATEWAY_URL").ok(),
            std::env::var("CODEX_TOOL_GATEWAY_BEARER_TOKEN").ok(),
            std::env::var("CODEX_TOOL_GATEWAY_TIMEOUT_MS").ok(),
        )
    }

    fn from_env_values(
        registry: ToolRegistry,
        endpoint: Option<String>,
        bearer_token: Option<String>,
        timeout_ms: Option<String>,
    ) -> Self {
        match endpoint
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            Some(endpoint) => {
                let mut backend = HttpToolGatewayBackend::new(endpoint);
                if let Some(token) = bearer_token
                    && !token.trim().is_empty()
                {
                    backend = backend.with_bearer_token(token);
                }
                if let Some(timeout_ms) = timeout_ms
                    && let Ok(timeout_ms) = timeout_ms.parse::<u64>()
                {
                    backend = backend.with_timeout(Duration::from_millis(timeout_ms));
                }
                Self::with_http_backend(registry, ToolCapabilityPolicy::allow_all(), backend)
            }
            None => Self::new(registry),
        }
    }

    pub fn with_capability_policy(
        registry: ToolRegistry,
        capability_policy: ToolCapabilityPolicy,
    ) -> Self {
        Self {
            backend: ToolGatewayBackend::Local,
            capability_policy,
            registry,
        }
    }

    pub fn with_http_backend(
        registry: ToolRegistry,
        capability_policy: ToolCapabilityPolicy,
        backend: HttpToolGatewayBackend,
    ) -> Self {
        Self {
            backend: ToolGatewayBackend::Http(Arc::new(backend)),
            capability_policy,
            registry,
        }
    }

    pub async fn dispatch(
        &self,
        invocation: ToolInvocation,
    ) -> Result<ResponseInputItem, FunctionCallError> {
        if let GatewayDecision::Denied { reason } =
            self.capability_policy.check(invocation.tool_name.as_str())
        {
            return Err(FunctionCallError::RespondToModel(reason));
        }
        match &self.backend {
            ToolGatewayBackend::Local => self.registry.dispatch(invocation).await,
            ToolGatewayBackend::Http(backend) if uses_execution_gateway(&invocation) => {
                backend.dispatch(invocation).await
            }
            ToolGatewayBackend::Http(_) => self.registry.dispatch(invocation).await,
        }
    }
}

enum ToolGatewayBackend {
    Local,
    Http(Arc<HttpToolGatewayBackend>),
}

fn uses_execution_gateway(invocation: &ToolInvocation) -> bool {
    uses_execution_gateway_tool(invocation.tool_name.as_str())
}

fn uses_execution_gateway_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "shell"
            | "container.exec"
            | "local_shell"
            | "shell_command"
            | "exec_command"
            | "apply_patch"
            | "read_file"
            | "list_dir"
            | "grep_files"
    )
}

#[derive(Clone, Debug)]
pub struct HttpToolGatewayBackend {
    endpoint: String,
    bearer_token: Option<String>,
    timeout: Duration,
    client: reqwest::Client,
}

impl HttpToolGatewayBackend {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            bearer_token: None,
            timeout: DEFAULT_HTTP_TIMEOUT,
            client: reqwest::Client::new(),
        }
    }

    pub fn with_bearer_token(mut self, bearer_token: impl Into<String>) -> Self {
        self.bearer_token = Some(bearer_token.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn dispatch(
        &self,
        invocation: ToolInvocation,
    ) -> Result<ResponseInputItem, FunctionCallError> {
        let request = GatewayToolRequest::from_invocation(&invocation, true)?;
        let mut builder = self
            .client
            .post(&self.endpoint)
            .timeout(self.timeout)
            .json(&request);
        if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token);
        }

        let response = builder.send().await.map_err(|err| {
            FunctionCallError::RespondToModel(format!("middleware gateway request failed: {err}"))
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "middleware gateway response read failed: {err}"
            ))
        })?;

        if !status.is_success() {
            return Err(FunctionCallError::RespondToModel(format!(
                "middleware gateway returned HTTP {status}: {body}"
            )));
        }

        serde_json::from_str::<GatewayToolResponse>(&body)
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "middleware gateway response was invalid JSON: {err}"
                ))
            })
            .and_then(|response| response.into_response_input_item(&request.call_id))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GatewayToolPayload {
    Function {
        arguments: String,
    },
    Custom {
        input: String,
    },
    LocalShell {
        command: Vec<String>,
        workdir: Option<String>,
        timeout_ms: Option<u64>,
        sandbox_permissions: Option<String>,
        prefix_rule: Option<Vec<String>>,
        justification: Option<String>,
    },
    Mcp {
        server: String,
        tool: String,
        raw_arguments: String,
    },
}

impl GatewayToolPayload {
    fn from_tool_payload(payload: &ToolPayload) -> Self {
        match payload {
            ToolPayload::Function { arguments } => Self::Function {
                arguments: arguments.clone(),
            },
            ToolPayload::Custom { input } => Self::Custom {
                input: input.clone(),
            },
            ToolPayload::LocalShell { params } => Self::LocalShell {
                command: params.command.clone(),
                workdir: params.workdir.clone(),
                timeout_ms: params.timeout_ms,
                sandbox_permissions: params
                    .sandbox_permissions
                    .map(|permissions| format!("{permissions:?}")),
                prefix_rule: params.prefix_rule.clone(),
                justification: params.justification.clone(),
            },
            ToolPayload::Mcp {
                server,
                tool,
                raw_arguments,
            } => Self::Mcp {
                server: server.clone(),
                tool: tool.clone(),
                raw_arguments: raw_arguments.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GatewayToolRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub company_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub call_id: String,
    pub tool_name: String,
    pub payload: GatewayToolPayload,
}

impl GatewayToolRequest {
    fn from_invocation(
        invocation: &ToolInvocation,
        execution_gateway: bool,
    ) -> Result<Self, FunctionCallError> {
        Ok(Self {
            session_id: invocation.session.conversation_id.to_string(),
            turn_id: invocation.turn.sub_id.clone(),
            cwd: path_to_gateway_string(
                prompt_cwd_for_gateway(&invocation.turn.cwd, execution_gateway).as_path(),
            )?,
            company_id: env_scope_value("CODEX_TOOL_GATEWAY_COMPANY_ID"),
            project_id: env_scope_value("CODEX_TOOL_GATEWAY_PROJECT_ID")
                .or_else(|| project_id_from_cwd(&invocation.turn.cwd)),
            agent_id: Some(agent_id_for_invocation(invocation)),
            call_id: invocation.call_id.clone(),
            tool_name: invocation.tool_name.clone(),
            payload: GatewayToolPayload::from_tool_payload(&invocation.payload),
        })
    }
}

fn env_scope_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn project_id_from_cwd(cwd: &Path) -> Option<String> {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn agent_id_for_invocation(invocation: &ToolInvocation) -> String {
    match &invocation.turn.session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            agent_name_hint: Some(agent_name_hint),
            ..
        }) if !agent_name_hint.trim().is_empty() => agent_name_hint.trim().to_string(),
        _ => invocation
            .session
            .services
            .agent_control
            .agent_name_for_thread(invocation.session.conversation_id)
            .unwrap_or_else(|| crate::agent::PRIMARY_AGENT_NAME.to_string()),
    }
}

fn path_to_gateway_string(path: &Path) -> Result<String, FunctionCallError> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "middleware gateway request contains a non-UTF-8 cwd".to_string(),
        )
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayToolResponse {
    Function {
        output: codex_protocol::models::FunctionCallOutputPayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    Custom {
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    Mcp {
        result: Result<codex_protocol::mcp::CallToolResult, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    Error {
        message: String,
    },
}

impl GatewayToolResponse {
    fn into_response_input_item(
        self,
        fallback_call_id: &str,
    ) -> Result<ResponseInputItem, FunctionCallError> {
        match self {
            Self::Function { output, call_id } => Ok(ResponseInputItem::FunctionCallOutput {
                call_id: call_id.unwrap_or_else(|| fallback_call_id.to_string()),
                output,
            }),
            Self::Custom { output, call_id } => Ok(ResponseInputItem::CustomToolCallOutput {
                call_id: call_id.unwrap_or_else(|| fallback_call_id.to_string()),
                output,
            }),
            Self::Mcp { result, call_id } => Ok(ResponseInputItem::McpToolCallOutput {
                call_id: call_id.unwrap_or_else(|| fallback_call_id.to_string()),
                result,
            }),
            Self::Error { message } => Err(FunctionCallError::RespondToModel(message)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::codex::make_session_and_context;
    use crate::office::ToolCapabilityPolicy;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::registry::ToolRegistry;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::models::ResponseInputItem;
    use serde_json::Value;
    use serde_json::json;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::HttpToolGatewayBackend;
    use super::ToolGateway;
    use super::uses_execution_gateway_tool;

    fn invocation(
        session: crate::codex::Session,
        turn: crate::codex::TurnContext,
        tool_name: &str,
        call_id: &str,
        payload: ToolPayload,
    ) -> ToolInvocation {
        ToolInvocation {
            session: Arc::new(session),
            turn: Arc::new(turn),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            payload,
        }
    }

    #[tokio::test]
    async fn explicit_capability_policy_denies_before_registry_dispatch() {
        let (session, turn) = make_session_and_context().await;
        let gateway = ToolGateway::with_capability_policy(
            ToolRegistry::new(HashMap::new()),
            ToolCapabilityPolicy::default().allow("call"),
        );
        let invocation = invocation(
            session,
            turn,
            "shell",
            "call-1",
            ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        );

        let err = gateway
            .dispatch(invocation)
            .await
            .expect_err("policy should reject shell before registry lookup");
        assert_eq!(
            err.to_string(),
            "tool `shell` is not allowed for this Agent profile"
        );
    }

    #[tokio::test]
    async fn default_gateway_policy_preserves_registry_behavior() {
        let (session, turn) = make_session_and_context().await;
        let gateway = ToolGateway::new(ToolRegistry::new(HashMap::new()));
        let invocation = invocation(
            session,
            turn,
            "missing_tool",
            "call-1",
            ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        );

        let err = gateway
            .dispatch(invocation)
            .await
            .expect_err("registry should still report unsupported tools");
        assert_eq!(err.to_string(), "unsupported call: missing_tool");
    }

    #[tokio::test]
    async fn http_backend_posts_tool_intent_and_returns_function_output() {
        let (session, turn) = make_session_and_context().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/dispatch"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "type": "function",
                "output": "gateway ok"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let gateway = ToolGateway::with_http_backend(
            ToolRegistry::new(HashMap::new()),
            ToolCapabilityPolicy::allow_all(),
            HttpToolGatewayBackend::new(format!("{}/tools/dispatch", server.uri()))
                .with_bearer_token("secret-token")
                .with_timeout(Duration::from_secs(5)),
        );
        let call_id = "http-call-1";
        let output = gateway
            .dispatch(invocation(
                session,
                turn,
                "shell",
                call_id,
                ToolPayload::Function {
                    arguments: r#"{"command":"pwd"}"#.to_string(),
                },
            ))
            .await
            .expect("http gateway should return output");

        match output {
            ResponseInputItem::FunctionCallOutput {
                call_id: got,
                output,
            } => {
                assert_eq!(got, call_id);
                assert_eq!(output.text_content(), Some("gateway ok"));
            }
            other => panic!("expected function output, got {other:?}"),
        }

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).expect("json body");
        assert_eq!(body["call_id"], Value::from(call_id));
        assert_eq!(body["tool_name"], Value::from("shell"));
        assert_eq!(body["payload"]["kind"], Value::from("function"));
        assert_eq!(
            body["payload"]["arguments"],
            Value::from(r#"{"command":"pwd"}"#)
        );
        assert!(
            body["session_id"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert!(
            body["turn_id"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert!(body["cwd"].as_str().is_some_and(|value| !value.is_empty()));
        assert!(
            body["agent_id"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert!(
            body["project_id"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
    }

    #[tokio::test]
    async fn http_backend_surfaces_gateway_error_response_to_model() {
        let (session, turn) = make_session_and_context().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/dispatch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "type": "error",
                "message": "denied by middleware"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let gateway = ToolGateway::with_http_backend(
            ToolRegistry::new(HashMap::new()),
            ToolCapabilityPolicy::allow_all(),
            HttpToolGatewayBackend::new(format!("{}/tools/dispatch", server.uri())),
        );
        let err = gateway
            .dispatch(invocation(
                session,
                turn,
                "shell",
                "http-call-2",
                ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            ))
            .await
            .expect_err("middleware error response should fail the tool call");
        assert_eq!(err.to_string(), "denied by middleware");
    }

    #[tokio::test]
    async fn http_backend_surfaces_http_status_to_model() {
        let (session, turn) = make_session_and_context().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/dispatch"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let gateway = ToolGateway::with_http_backend(
            ToolRegistry::new(HashMap::new()),
            ToolCapabilityPolicy::allow_all(),
            HttpToolGatewayBackend::new(format!("{}/tools/dispatch", server.uri())),
        );
        let err = gateway
            .dispatch(invocation(
                session,
                turn,
                "shell",
                "http-call-3",
                ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            ))
            .await
            .expect_err("HTTP error should fail the tool call");
        assert!(
            err.to_string()
                .contains("middleware gateway returned HTTP 403")
        );
        assert!(err.to_string().contains("forbidden"));
    }

    #[tokio::test]
    async fn env_configured_gateway_uses_http_backend() {
        let (session, turn) = make_session_and_context().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/dispatch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "type": "function",
                "output": "env gateway ok"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let gateway = ToolGateway::from_env_values(
            ToolRegistry::new(HashMap::new()),
            Some(format!("{}/tools/dispatch", server.uri())),
            Some("secret-token".to_string()),
            Some("5000".to_string()),
        );
        let output = gateway
            .dispatch(invocation(
                session,
                turn,
                "shell",
                "env-call-1",
                ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            ))
            .await
            .expect("env gateway should return output");

        match output {
            ResponseInputItem::FunctionCallOutput { output, .. } => {
                assert_eq!(output.text_content(), Some("env gateway ok"));
            }
            other => panic!("expected function output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_backend_only_routes_execution_environment_tools() {
        for tool_name in [
            "shell",
            "container.exec",
            "local_shell",
            "shell_command",
            "exec_command",
            "apply_patch",
            "read_file",
            "list_dir",
            "grep_files",
        ] {
            assert!(
                uses_execution_gateway_tool(tool_name),
                "{tool_name} should use execution gateway"
            );
        }

        for tool_name in [
            "call",
            "wait",
            "update_plan",
            "request_user_input",
            "list_mcp_resources",
            "read_mcp_resource",
            "spawn_agent",
            "send_input",
            "read_agent_status",
        ] {
            assert!(
                !uses_execution_gateway_tool(tool_name),
                "{tool_name} should stay in the runtime registry"
            );
        }
    }

    #[tokio::test]
    async fn http_gateway_falls_back_to_registry_for_runtime_tools() {
        let (session, turn) = make_session_and_context().await;
        let server = MockServer::start().await;
        let gateway = ToolGateway::with_http_backend(
            ToolRegistry::new(HashMap::new()),
            ToolCapabilityPolicy::allow_all(),
            HttpToolGatewayBackend::new(format!("{}/tools/dispatch", server.uri())),
        );

        let err = gateway
            .dispatch(invocation(
                session,
                turn,
                "update_plan",
                "runtime-call-1",
                ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            ))
            .await
            .expect_err("empty local registry should reject runtime tool locally");

        assert_eq!(err.to_string(), "unsupported call: update_plan");
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn http_backend_sends_logical_workspace_cwd() {
        let (session, turn) = make_session_and_context().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/dispatch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "type": "function",
                "output": "ok"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let gateway = ToolGateway::with_http_backend(
            ToolRegistry::new(HashMap::new()),
            ToolCapabilityPolicy::allow_all(),
            HttpToolGatewayBackend::new(format!("{}/tools/dispatch", server.uri())),
        );

        let _ = gateway
            .dispatch(invocation(
                session,
                turn,
                "shell",
                "http-call-cwd",
                ToolPayload::Function {
                    arguments: r#"{"command":["pwd"]}"#.to_string(),
                },
            ))
            .await
            .expect("http gateway should succeed");

        let requests = server.received_requests().await.expect("requests");
        let body: Value = serde_json::from_slice(&requests[0].body).expect("json body");
        assert_eq!(body["cwd"], Value::from("/workspace"));
    }
}
