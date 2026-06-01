//! Session- and turn-scoped helpers for talking to model provider APIs.
//!
//! `ModelClient` is intended to live for the lifetime of a Codex session and holds the stable
//! configuration and state needed to talk to a provider (auth, provider selection, conversation id,
//! and feature-gated request behavior).
//!
//! Per-turn settings (model selection, reasoning controls, telemetry context, and turn metadata)
//! are passed explicitly to streaming and unary methods so that the turn lifetime is visible at the
//! call site.
//!
//! A [`ModelClientSession`] is created per turn and is used to stream one or more Responses API
//! requests during that turn. It caches a Responses WebSocket connection (opened lazily) and stores
//! per-turn state such as the `x-codex-turn-state` token used for sticky routing.
//!
//! Prewarm is intentionally handshake-only: it may warm a socket and capture sticky-routing
//! state, but the first `response.create` payload is still sent only when a turn starts.
//!
//! Startup prewarm is owned by turn-scoped callers (for example, a pre-created regular task). When
//! a warmed [`ModelClientSession`] is available, turn execution can reuse it; otherwise the turn
//! lazily opens a websocket on first stream call.
//!
//! ## Retry-Budget Tradeoff
//!
//! Startup prewarm is treated as the first websocket connection attempt for the first turn. If
//! it fails, the stream attempt fails and the retry/fallback loop decides whether to retry or fall
//! back. This avoids duplicate handshakes but means a failed prewarm can consume one retry
//! budget slot before any turn payload is sent.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::api_bridge::CoreAuthProvider;
use crate::api_bridge::auth_provider_from_auth;
use crate::api_bridge::map_api_error;
use crate::auth::UnauthorizedRecovery;
use codex_api::ChatCompletionsClient as ApiChatCompletionsClient;
use codex_api::CompactClient as ApiCompactClient;
use codex_api::CompactionInput as ApiCompactionInput;
use codex_api::MemoriesClient as ApiMemoriesClient;
use codex_api::MemorySummarizeInput as ApiMemorySummarizeInput;
use codex_api::MemorySummarizeOutput as ApiMemorySummarizeOutput;
use codex_api::RawMemory as ApiRawMemory;
use codex_api::RequestTelemetry;
use codex_api::ReqwestTransport;
use codex_api::ResponseAppendWsRequest;
use codex_api::ResponseCreateWsRequest;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesClient as ApiResponsesClient;
use codex_api::ResponsesOptions as ApiResponsesOptions;
use codex_api::ResponsesWebsocketClient as ApiWebSocketResponsesClient;
use codex_api::ResponsesWebsocketConnection as ApiWebSocketConnection;
use codex_api::SseTelemetry;
use codex_api::TransportError;
use codex_api::WebsocketTelemetry;
use codex_api::build_conversation_headers;
use codex_api::common::Reasoning;
use codex_api::common::ResponsesWsRequest;
use codex_api::create_text_param_for_request;
use codex_api::error::ApiError;
use codex_api::requests::responses::Compression;
use codex_client::StreamResponse as RawStreamResponse;
use codex_otel::OtelManager;

use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::Verbosity as VerbosityConfig;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::SessionSource;
use eventsource_stream::Event;
use eventsource_stream::EventStreamError;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use http::HeaderMap as ApiHeaderMap;
use http::HeaderValue;
use http::StatusCode as HttpStatusCode;
use reqwest::StatusCode;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Error;
use tokio_tungstenite::tungstenite::Message;
use tracing::trace;
use tracing::warn;

use crate::AuthManager;
use crate::auth::CodexAuth;
use crate::auth::RefreshTokenError;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::default_client::build_reqwest_client;
use crate::error::CodexErr;
use crate::error::Result;
use crate::flags::CODEX_RS_SSE_FIXTURE;
use crate::model_io_recorder::ModelIoRecorder;
use crate::model_provider_info::ModelProviderInfo;
use crate::model_provider_info::WireApi;
use crate::provider_adapter::ChatCompletionsChunkNormalizer;
use crate::provider_adapter::ProviderAdapterKind;
use crate::provider_adapter::responses_request_to_chat_request;
use crate::tools::spec::create_tools_json_for_responses_api;

pub const OPENAI_BETA_HEADER: &str = "OpenAI-Beta";
pub const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-04";
pub const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";
pub const X_CODEX_TURN_METADATA_HEADER: &str = "x-codex-turn-metadata";
pub const X_RESPONSESAPI_INCLUDE_TIMING_METRICS_HEADER: &str =
    "x-responsesapi-include-timing-metrics";
const RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE: &str = "responses_websockets=2026-02-06";
/// Session-scoped state shared by all [`ModelClient`] clones.
///
/// This is intentionally kept minimal so `ModelClient` does not need to hold a full `Config`. Most
/// configuration is per turn and is passed explicitly to streaming/unary methods.
#[derive(Debug)]
struct ModelClientState {
    auth_manager: Option<Arc<AuthManager>>,
    conversation_id: ThreadId,
    provider: ModelProviderInfo,
    session_source: SessionSource,
    model_verbosity: Option<VerbosityConfig>,
    enable_responses_websockets: bool,
    enable_responses_websockets_v2: bool,
    enable_request_compression: bool,
    include_timing_metrics: bool,
    beta_features_header: Option<String>,
    model_io_recorder: Option<ModelIoRecorder>,
    disable_websockets: AtomicBool,
}

/// Resolved API client setup for a single request attempt.
///
/// Keeping this as a single bundle ensures prewarm and normal request paths
/// share the same auth/provider setup flow.
struct CurrentClientSetup {
    auth: Option<CodexAuth>,
    api_provider: codex_api::Provider,
    api_auth: CoreAuthProvider,
}

/// A session-scoped client for model-provider API calls.
///
/// This holds configuration and state that should be shared across turns within a Codex session
/// (auth, provider selection, conversation id, feature-gated request behavior, and transport
/// fallback state).
///
/// WebSocket fallback is session-scoped: once a turn activates the HTTP fallback, subsequent turns
/// will also use HTTP for the remainder of the session.
///
/// Turn-scoped settings (model selection, reasoning controls, telemetry context, and turn
/// metadata) are passed explicitly to the relevant methods to keep turn lifetime visible at the
/// call site.
#[derive(Debug, Clone)]
pub struct ModelClient {
    state: Arc<ModelClientState>,
}

/// A turn-scoped streaming session created from a [`ModelClient`].
///
/// The session establishes a Responses WebSocket connection lazily and reuses it across multiple
/// requests within the turn. It also caches per-turn state:
///
/// - The last full request, so subsequent calls can use `response.append` only when the current
///   request is an incremental extension of the previous one.
/// - The `x-codex-turn-state` sticky-routing token, which must be replayed for all requests within
///   the same turn.
///
/// Create a fresh `ModelClientSession` for each Codex turn. Reusing it across turns would replay
/// the previous turn's sticky-routing token into the next turn, which violates the client/server
/// contract and can cause routing bugs.
pub struct ModelClientSession {
    client: ModelClient,
    connection: Option<ApiWebSocketConnection>,
    websocket_last_request: Option<ResponsesApiRequest>,
    websocket_last_response_rx: Option<oneshot::Receiver<LastResponse>>,
    /// Turn state for sticky routing.
    ///
    /// This is an `OnceLock` that stores the turn state value received from the server
    /// on turn start via the `x-codex-turn-state` response header. Once set, this value
    /// should be sent back to the server in the `x-codex-turn-state` request header for
    /// all subsequent requests within the same turn to maintain sticky routing.
    ///
    /// This is a contract between the client and server: we receive it at turn start,
    /// keep sending it unchanged between turn requests (e.g., for retries, incremental
    /// appends, or continuation requests), and must not send it between different turns.
    turn_state: Arc<OnceLock<String>>,
}

#[derive(Debug, Clone)]
struct LastResponse {
    response_id: String,
    items_added: Vec<ResponseItem>,
    can_append: bool,
}

#[derive(Clone)]
struct ModelIoDebugContext {
    recorder: ModelIoRecorder,
    request_id: u64,
    next_seq: u64,
    request_input_items: Vec<ResponseItem>,
    transport: &'static str,
    request_started_at: std::time::Instant,
}

enum WebsocketStreamOutcome {
    Stream(ResponseStream),
    FallbackToHttp,
}

impl ModelClient {
    #[allow(clippy::too_many_arguments)]
    /// Creates a new session-scoped `ModelClient`.
    ///
    /// All arguments are expected to be stable for the lifetime of a Codex session. Per-turn values
    /// are passed to [`ModelClientSession::stream`] (and other turn-scoped methods) explicitly.
    pub fn new(
        auth_manager: Option<Arc<AuthManager>>,
        conversation_id: ThreadId,
        provider: ModelProviderInfo,
        session_source: SessionSource,
        model_verbosity: Option<VerbosityConfig>,
        enable_responses_websockets: bool,
        enable_responses_websockets_v2: bool,
        enable_request_compression: bool,
        include_timing_metrics: bool,
        beta_features_header: Option<String>,
        model_io_debug_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let model_io_recorder =
            model_io_debug_dir.and_then(|dir| match ModelIoRecorder::new(dir, &conversation_id) {
                Ok(recorder) => Some(recorder),
                Err(err) => {
                    warn!(%err, "failed to initialize model io debug recorder");
                    None
                }
            });
        Self {
            state: Arc::new(ModelClientState {
                auth_manager,
                conversation_id,
                provider,
                session_source,
                model_verbosity,
                enable_responses_websockets,
                enable_responses_websockets_v2,
                enable_request_compression,
                include_timing_metrics,
                beta_features_header,
                model_io_recorder,
                disable_websockets: AtomicBool::new(false),
            }),
        }
    }

    /// Creates a fresh turn-scoped streaming session.
    ///
    /// This constructor does not perform network I/O itself; the session opens a websocket lazily
    /// when the first stream request is issued.
    pub fn new_session(&self) -> ModelClientSession {
        ModelClientSession {
            client: self.clone(),
            connection: None,
            websocket_last_request: None,
            websocket_last_response_rx: None,
            turn_state: Arc::new(OnceLock::new()),
        }
    }

    /// Compacts the current conversation history using the Compact endpoint.
    ///
    /// This is a unary call (no streaming) that returns a new list of
    /// `ResponseItem`s representing the compacted transcript.
    ///
    /// The model selection and telemetry context are passed explicitly to keep `ModelClient`
    /// session-scoped.
    pub async fn compact_conversation_history(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        otel_manager: &OtelManager,
    ) -> Result<Vec<ResponseItem>> {
        if prompt.input.is_empty() {
            return Ok(Vec::new());
        }
        let client_setup = self.current_client_setup().await?;
        let transport = ReqwestTransport::new(build_reqwest_client());
        let request_telemetry = Self::build_request_telemetry(otel_manager);
        let client =
            ApiCompactClient::new(transport, client_setup.api_provider, client_setup.api_auth)
                .with_telemetry(Some(request_telemetry));

        let instructions = prompt.base_instructions.text.clone();
        let payload = ApiCompactionInput {
            model: &model_info.slug,
            input: &prompt.input,
            instructions: &instructions,
        };

        let extra_headers = self.build_subagent_headers();
        client
            .compact_input(&payload, extra_headers)
            .await
            .map_err(map_api_error)
    }

    /// Builds memory summaries for each provided normalized raw memory.
    ///
    /// This is a unary call (no streaming) to `/v1/memories/trace_summarize`.
    ///
    /// The model selection, reasoning effort, and telemetry context are passed explicitly to keep
    /// `ModelClient` session-scoped.
    pub async fn summarize_memories(
        &self,
        raw_memories: Vec<ApiRawMemory>,
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
        otel_manager: &OtelManager,
    ) -> Result<Vec<ApiMemorySummarizeOutput>> {
        if raw_memories.is_empty() {
            return Ok(Vec::new());
        }

        let client_setup = self.current_client_setup().await?;
        let transport = ReqwestTransport::new(build_reqwest_client());
        let request_telemetry = Self::build_request_telemetry(otel_manager);
        let client =
            ApiMemoriesClient::new(transport, client_setup.api_provider, client_setup.api_auth)
                .with_telemetry(Some(request_telemetry));

        let payload = ApiMemorySummarizeInput {
            model: model_info.slug.clone(),
            raw_memories,
            reasoning: effort.map(|effort| Reasoning {
                effort: Some(effort),
                summary: None,
            }),
        };

        client
            .summarize_input(&payload, self.build_subagent_headers())
            .await
            .map_err(map_api_error)
    }

    fn build_subagent_headers(&self) -> ApiHeaderMap {
        let mut extra_headers = ApiHeaderMap::new();
        if let SessionSource::SubAgent(sub) = &self.state.session_source {
            let subagent = match sub {
                crate::protocol::SubAgentSource::Review => "review".to_string(),
                crate::protocol::SubAgentSource::Compact => "compact".to_string(),
                crate::protocol::SubAgentSource::MemoryConsolidation => {
                    "memory_consolidation".to_string()
                }
                crate::protocol::SubAgentSource::ThreadSpawn { .. } => "collab_spawn".to_string(),
                crate::protocol::SubAgentSource::Other(label) => label.clone(),
            };
            if let Ok(val) = HeaderValue::from_str(&subagent) {
                extra_headers.insert("x-openai-subagent", val);
            }
        }
        extra_headers
    }

    /// Builds request telemetry for unary API calls (e.g., Compact endpoint).
    fn build_request_telemetry(otel_manager: &OtelManager) -> Arc<dyn RequestTelemetry> {
        let telemetry = Arc::new(ApiTelemetry::new(otel_manager.clone()));
        let request_telemetry: Arc<dyn RequestTelemetry> = telemetry;
        request_telemetry
    }

    /// Returns whether this session is configured to use Responses-over-WebSocket.
    ///
    /// This combines provider capability and feature gating; both must be true for websocket paths
    /// to be eligible.
    pub fn responses_websocket_enabled(&self, model_info: &ModelInfo) -> bool {
        self.state.provider.supports_websockets
            && (self.state.enable_responses_websockets || model_info.prefer_websockets)
    }

    fn responses_websockets_v2_enabled(&self) -> bool {
        self.state.enable_responses_websockets_v2
    }

    /// Returns whether websocket transport has been permanently disabled for this session.
    ///
    /// Once set by fallback activation, subsequent turns must stay on HTTP transport.
    fn websockets_disabled(&self) -> bool {
        self.state.disable_websockets.load(Ordering::Relaxed)
    }

    /// Returns auth + provider configuration resolved from the current session auth state.
    ///
    /// This centralizes setup used by both prewarm and normal request paths so they stay in
    /// lockstep when auth/provider resolution changes.
    async fn current_client_setup(&self) -> Result<CurrentClientSetup> {
        let auth = match self.state.auth_manager.as_ref() {
            Some(manager) => manager.auth().await,
            None => None,
        };
        let api_provider = self
            .state
            .provider
            .to_api_provider(auth.as_ref().map(CodexAuth::auth_mode))?;
        let api_auth = auth_provider_from_auth(auth.clone(), &self.state.provider)?;
        Ok(CurrentClientSetup {
            auth,
            api_provider,
            api_auth,
        })
    }

    /// Opens a websocket connection using the same header and telemetry wiring as normal turns.
    ///
    /// Both startup prewarm and in-turn `needs_new` reconnects call this path so handshake
    /// behavior remains consistent across both flows.
    async fn connect_websocket(
        &self,
        otel_manager: &OtelManager,
        api_provider: codex_api::Provider,
        api_auth: CoreAuthProvider,
        turn_state: Option<Arc<OnceLock<String>>>,
        turn_metadata_header: Option<&str>,
    ) -> std::result::Result<ApiWebSocketConnection, ApiError> {
        let headers = self.build_websocket_headers(turn_state.as_ref(), turn_metadata_header);
        let websocket_telemetry = ModelClientSession::build_websocket_telemetry(otel_manager);
        ApiWebSocketResponsesClient::new(api_provider, api_auth)
            .connect(
                headers,
                crate::default_client::default_headers(),
                turn_state,
                Some(websocket_telemetry),
            )
            .await
    }

    /// Builds websocket handshake headers for both prewarm and turn-time reconnect.
    ///
    /// Callers should pass the current turn-state lock when available so sticky-routing state is
    /// replayed on reconnect within the same turn.
    fn build_websocket_headers(
        &self,
        turn_state: Option<&Arc<OnceLock<String>>>,
        turn_metadata_header: Option<&str>,
    ) -> ApiHeaderMap {
        let turn_metadata_header = parse_turn_metadata_header(turn_metadata_header);
        let mut headers = build_responses_headers(
            self.state.beta_features_header.as_deref(),
            turn_state,
            turn_metadata_header.as_ref(),
        );
        headers.extend(build_conversation_headers(Some(
            self.state.conversation_id.to_string(),
        )));
        let responses_websockets_beta_header = if self.responses_websockets_v2_enabled() {
            RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE
        } else {
            OPENAI_BETA_RESPONSES_WEBSOCKETS
        };
        headers.insert(
            OPENAI_BETA_HEADER,
            HeaderValue::from_static(responses_websockets_beta_header),
        );
        if self.state.include_timing_metrics {
            headers.insert(
                X_RESPONSESAPI_INCLUDE_TIMING_METRICS_HEADER,
                HeaderValue::from_static("true"),
            );
        }
        headers
    }
}

impl ModelClientSession {
    fn activate_http_fallback(&self, websocket_enabled: bool) -> bool {
        websocket_enabled
            && !self
                .client
                .state
                .disable_websockets
                .swap(true, Ordering::Relaxed)
    }

    fn build_responses_request(
        &self,
        provider: &codex_api::Provider,
        prompt: &Prompt,
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
    ) -> Result<ResponsesApiRequest> {
        let instructions = &prompt.base_instructions.text;
        let input = prompt.get_formatted_input();
        let tools = create_tools_json_for_responses_api(&prompt.tools)?;
        let default_reasoning_effort = model_info.default_reasoning_level;
        let reasoning = if model_info.supports_reasoning_summaries {
            Some(Reasoning {
                effort: effort.or(default_reasoning_effort),
                summary: if summary == ReasoningSummaryConfig::None {
                    None
                } else {
                    Some(summary)
                },
            })
        } else {
            None
        };
        let include = if reasoning.is_some() {
            vec!["reasoning.encrypted_content".to_string()]
        } else {
            Vec::new()
        };
        let verbosity = if model_info.support_verbosity {
            self.client
                .state
                .model_verbosity
                .or(model_info.default_verbosity)
        } else {
            if self.client.state.model_verbosity.is_some() {
                warn!(
                    "model_verbosity is set but ignored as the model does not support verbosity: {}",
                    model_info.slug
                );
            }
            None
        };
        let text = create_text_param_for_request(verbosity, &prompt.output_schema);
        let prompt_cache_key = Some(self.client.state.conversation_id.to_string());
        let tool_choice = if tools.is_empty() {
            "none".to_string()
        } else {
            "auto".to_string()
        };
        let request = ResponsesApiRequest {
            model: model_info.slug.clone(),
            instructions: instructions.clone(),
            input,
            tools,
            tool_choice,
            parallel_tool_calls: prompt.parallel_tool_calls,
            reasoning,
            store: provider.is_azure_responses_endpoint(),
            stream: true,
            include,
            prompt_cache_key,
            text,
        };
        Ok(request)
    }

    #[allow(clippy::too_many_arguments)]
    /// Builds shared Responses API transport options and request-body options.
    ///
    /// Keeping option construction in one place ensures request-scoped headers are consistent
    /// regardless of transport choice.
    fn build_responses_options(
        &self,
        turn_metadata_header: Option<&str>,
        compression: Compression,
    ) -> ApiResponsesOptions {
        let turn_metadata_header = parse_turn_metadata_header(turn_metadata_header);
        let conversation_id = self.client.state.conversation_id.to_string();

        ApiResponsesOptions {
            conversation_id: Some(conversation_id),
            session_source: Some(self.client.state.session_source.clone()),
            extra_headers: build_responses_headers(
                self.client.state.beta_features_header.as_deref(),
                Some(&self.turn_state),
                turn_metadata_header.as_ref(),
            ),
            compression,
            turn_state: Some(Arc::clone(&self.turn_state)),
        }
    }

    /// Builds the extra headers attached to Chat Completions adapter requests.
    ///
    /// Chat-compatible providers should receive only provider-neutral Codex
    /// conversation/subagent metadata here. Responses-specific beta, timing,
    /// compression, and sticky-routing headers intentionally stay on the native
    /// Responses path.
    fn build_chat_completions_headers(&self, turn_metadata_header: Option<&str>) -> ApiHeaderMap {
        let mut headers =
            build_conversation_headers(Some(self.client.state.conversation_id.to_string()));
        headers.extend(self.client.build_subagent_headers());
        if let Some(header_value) = parse_turn_metadata_header(turn_metadata_header) {
            headers.insert(X_CODEX_TURN_METADATA_HEADER, header_value);
        }
        headers
    }

    fn get_incremental_items(
        &self,
        request: &ResponsesApiRequest,
        last_response: Option<&LastResponse>,
    ) -> Option<Vec<ResponseItem>> {
        // Checks whether the current request is an incremental append to the previous request.
        // We only append when non-input request fields are unchanged and `input` is a strict
        // extension of the previous known input. Server-returned output items are treated as part
        // of the baseline so we do not resend them.
        let previous_request = self.websocket_last_request.as_ref()?;
        let mut previous_without_input = previous_request.clone();
        previous_without_input.input.clear();
        let mut request_without_input = request.clone();
        request_without_input.input.clear();
        if previous_without_input != request_without_input {
            trace!(
                "incremental request failed, properties didn't match {previous_without_input:?} != {request_without_input:?}"
            );
            return None;
        }

        let mut baseline = previous_request.input.clone();
        if let Some(last_response) = last_response {
            baseline.extend(last_response.items_added.clone());
        }

        let baseline_len = baseline.len();
        if baseline_len > 0
            && request.input.starts_with(&baseline)
            && baseline_len < request.input.len()
        {
            Some(request.input[baseline_len..].to_vec())
        } else {
            trace!("incremental request failed, items didn't match");
            None
        }
    }

    fn get_last_response(&mut self) -> Option<LastResponse> {
        self.websocket_last_response_rx
            .take()
            .and_then(|mut receiver| match receiver.try_recv() {
                Ok(last_response) => Some(last_response),
                Err(TryRecvError::Closed) | Err(TryRecvError::Empty) => None,
            })
    }

    fn prepare_websocket_request(
        &mut self,
        payload: ResponseCreateWsRequest,
        request: &ResponsesApiRequest,
    ) -> ResponsesWsRequest {
        let Some(last_response) = self.get_last_response() else {
            return ResponsesWsRequest::ResponseCreate(payload);
        };
        let responses_websockets_v2_enabled = self.client.responses_websockets_v2_enabled();
        if !responses_websockets_v2_enabled && !last_response.can_append {
            trace!("incremental request failed, can't append");
            return ResponsesWsRequest::ResponseCreate(payload);
        }
        let incremental_items = self.get_incremental_items(request, Some(&last_response));
        if let Some(append_items) = incremental_items {
            if responses_websockets_v2_enabled && !last_response.response_id.is_empty() {
                let payload = ResponseCreateWsRequest {
                    previous_response_id: Some(last_response.response_id),
                    input: append_items,
                    ..payload
                };
                return ResponsesWsRequest::ResponseCreate(payload);
            }

            if !responses_websockets_v2_enabled {
                return ResponsesWsRequest::ResponseAppend(ResponseAppendWsRequest {
                    input: append_items,
                });
            }
        }

        ResponsesWsRequest::ResponseCreate(payload)
    }

    /// Opportunistically warms a websocket for this turn-scoped client session.
    ///
    /// This performs only connection setup; it never sends prompt payloads.
    pub async fn prewarm_websocket(
        &mut self,
        otel_manager: &OtelManager,
        model_info: &ModelInfo,
    ) -> std::result::Result<(), ApiError> {
        if !self.client.responses_websocket_enabled(model_info) || self.client.websockets_disabled()
        {
            return Ok(());
        }
        if self.connection.is_some() {
            return Ok(());
        }

        let client_setup = self.client.current_client_setup().await.map_err(|err| {
            ApiError::Stream(format!(
                "failed to build websocket prewarm client setup: {err}"
            ))
        })?;

        let connection = self
            .client
            .connect_websocket(
                otel_manager,
                client_setup.api_provider,
                client_setup.api_auth,
                Some(Arc::clone(&self.turn_state)),
                None,
            )
            .await?;
        self.connection = Some(connection);
        Ok(())
    }

    /// Returns a websocket connection for this turn.
    async fn websocket_connection(
        &mut self,
        otel_manager: &OtelManager,
        api_provider: codex_api::Provider,
        api_auth: CoreAuthProvider,
        turn_metadata_header: Option<&str>,
        options: &ApiResponsesOptions,
    ) -> std::result::Result<&ApiWebSocketConnection, ApiError> {
        let needs_new = match self.connection.as_ref() {
            Some(conn) => conn.is_closed().await,
            None => true,
        };

        if needs_new {
            self.websocket_last_request = None;
            self.websocket_last_response_rx = None;
            let turn_state = options
                .turn_state
                .clone()
                .unwrap_or_else(|| Arc::clone(&self.turn_state));
            let new_conn = self
                .client
                .connect_websocket(
                    otel_manager,
                    api_provider,
                    api_auth,
                    Some(turn_state),
                    turn_metadata_header,
                )
                .await?;
            self.connection = Some(new_conn);
        }

        self.connection.as_ref().ok_or(ApiError::Stream(
            "websocket connection is unavailable".to_string(),
        ))
    }

    fn responses_request_compression(&self, auth: Option<&crate::auth::CodexAuth>) -> Compression {
        if self.client.state.enable_request_compression
            && auth.is_some_and(CodexAuth::is_chatgpt_auth)
            && self.client.state.provider.is_openai()
        {
            Compression::Zstd
        } else {
            Compression::None
        }
    }

    /// Streams a turn via the OpenAI Responses API.
    ///
    /// Handles SSE fixtures, reasoning summaries, verbosity, and the
    /// `text` controls used for output schemas.
    #[allow(clippy::too_many_arguments)]
    async fn stream_responses_api(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        otel_manager: &OtelManager,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        turn_metadata_header: Option<&str>,
    ) -> Result<ResponseStream> {
        if let Some(path) = &*CODEX_RS_SSE_FIXTURE {
            warn!(path, "Streaming from fixture");
            let stream = codex_api::stream_from_fixture(
                path,
                self.client.state.provider.stream_idle_timeout(),
            )
            .map_err(map_api_error)?;
            let (stream, _last_request_rx) =
                map_response_stream(stream, otel_manager.clone(), None);
            return Ok(stream);
        }

        let auth_manager = self.client.state.auth_manager.clone();
        let mut auth_recovery = auth_manager
            .as_ref()
            .map(super::auth::AuthManager::unauthorized_recovery);
        loop {
            let client_setup = self.client.current_client_setup().await?;
            let transport = ReqwestTransport::new(build_reqwest_client());
            let (request_telemetry, sse_telemetry) = Self::build_streaming_telemetry(otel_manager);
            let compression = self.responses_request_compression(client_setup.auth.as_ref());
            let options = self.build_responses_options(turn_metadata_header, compression);

            let request = self.build_responses_request(
                &client_setup.api_provider,
                prompt,
                model_info,
                effort,
                summary,
            )?;
            let model_io_context = self
                .client
                .state
                .model_io_recorder
                .as_ref()
                .map(|recorder| {
                    let request_id = recorder.next_request_id();
                    (recorder.clone(), request_id)
                });
            let model_io_context = if let Some((recorder, request_id)) = model_io_context {
                let request_value = serde_json::json!({
                    "model": request.model,
                    "instructions": request.instructions,
                    "tools": request.tools,
                    "tool_choice": request.tool_choice,
                    "parallel_tool_calls": request.parallel_tool_calls,
                    "reasoning": request.reasoning,
                    "store": request.store,
                    "stream": request.stream,
                    "include": request.include,
                    "prompt_cache_key": request.prompt_cache_key,
                    "text": request.text,
                    "input_len": request.input.len(),
                    "input": build_model_io_input_payload(&request),
                });
                // seq=0 is reserved for request_start.
                let mut next_seq: u64 = 1;
                recorder
                    .record_request_start(request_id, &request_value)
                    .await;

                for (idx, item) in request.input.iter().enumerate() {
                    recorder
                        .record_request_input_item(request_id, next_seq, idx, item)
                        .await;
                    next_seq += 1;
                }

                Some(ModelIoDebugContext {
                    recorder,
                    request_id,
                    next_seq,
                    request_input_items: request.input.clone(),
                    transport: "responses_http",
                    request_started_at: std::time::Instant::now(),
                })
            } else {
                None
            };
            let client = ApiResponsesClient::new(
                transport,
                client_setup.api_provider,
                client_setup.api_auth,
            )
            .with_telemetry(Some(request_telemetry), Some(sse_telemetry));
            let mut model_io_context = model_io_context;
            if let Some(ctx) = model_io_context.as_mut() {
                ctx.recorder
                    .record_transport_stage(
                        ctx.request_id,
                        ctx.next_seq,
                        ctx.transport,
                        "dispatch_start",
                        serde_json::json!({ "path": "responses_api", "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at) }),
                    )
                    .await;
                ctx.next_seq += 1;
            }
            let stream_result = client.stream_request(request, options).await;

            match stream_result {
                Ok(stream) => {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "dispatch_ready",
                                serde_json::json!({ "path": "responses_api", "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at) }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                    let (stream, _) =
                        map_response_stream(stream, otel_manager.clone(), model_io_context);
                    return Ok(stream);
                }
                Err(ApiError::Transport(
                    unauthorized_transport @ TransportError::Http { status, .. },
                )) if status == StatusCode::UNAUTHORIZED => {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "dispatch_error",
                                serde_json::json!({
                                    "path": "responses_api",
                                    "error": unauthorized_transport.to_string(),
                                    "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at),
                                }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                    handle_unauthorized(unauthorized_transport, &mut auth_recovery).await?;
                    continue;
                }
                Err(err) => {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "dispatch_error",
                                serde_json::json!({
                                    "path": "responses_api",
                                    "error": err.to_string(),
                                    "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at),
                                }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                    return Err(map_api_error(err));
                }
            }
        }
    }

    /// Streams a turn via an OpenAI-compatible Chat Completions HTTP/SSE endpoint.
    ///
    /// The upper agent loop still speaks the normalized Responses-style contract.
    /// This path converts the existing request shape into chat messages, sends it
    /// to `/chat/completions`, and normalizes streamed chat chunks back into
    /// `ResponseEvent` values before downstream tools see them.
    #[allow(clippy::too_many_arguments)]
    async fn stream_chat_completions_api(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        otel_manager: &OtelManager,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        turn_metadata_header: Option<&str>,
    ) -> Result<ResponseStream> {
        let auth_manager = self.client.state.auth_manager.clone();
        let mut auth_recovery = auth_manager
            .as_ref()
            .map(super::auth::AuthManager::unauthorized_recovery);

        loop {
            let client_setup = self.client.current_client_setup().await?;
            let transport = ReqwestTransport::new(build_reqwest_client());
            let (request_telemetry, sse_telemetry) = Self::build_streaming_telemetry(otel_manager);
            let request = self.build_responses_request(
                &client_setup.api_provider,
                prompt,
                model_info,
                effort,
                summary,
            )?;
            let chat_request =
                responses_request_to_chat_request(&request, &self.client.state.provider.profile)?;
            let body = serde_json::to_value(&chat_request)?;
            let extra_headers = self.build_chat_completions_headers(turn_metadata_header);

            let model_io_context = self
                .client
                .state
                .model_io_recorder
                .as_ref()
                .map(|recorder| {
                    let request_id = recorder.next_request_id();
                    (recorder.clone(), request_id)
                });
            let model_io_context = if let Some((recorder, request_id)) = model_io_context {
                let request_value = serde_json::json!({
                    "model": request.model,
                    "adapter": "chat_completions",
                    "chat_request": body,
                    "input_len": request.input.len(),
                    "input": build_model_io_input_payload(&request),
                });
                let mut next_seq: u64 = 1;
                recorder
                    .record_request_start(request_id, &request_value)
                    .await;

                for (idx, item) in request.input.iter().enumerate() {
                    recorder
                        .record_request_input_item(request_id, next_seq, idx, item)
                        .await;
                    next_seq += 1;
                }

                Some(ModelIoDebugContext {
                    recorder,
                    request_id,
                    next_seq,
                    request_input_items: request.input.clone(),
                    transport: "chat_completions_http",
                    request_started_at: std::time::Instant::now(),
                })
            } else {
                None
            };

            let client = ApiChatCompletionsClient::new(
                transport,
                client_setup.api_provider,
                client_setup.api_auth,
            )
            .with_telemetry(Some(request_telemetry));
            let mut model_io_context = model_io_context;
            if let Some(ctx) = model_io_context.as_mut() {
                ctx.recorder
                    .record_transport_stage(
                        ctx.request_id,
                        ctx.next_seq,
                        ctx.transport,
                        "dispatch_start",
                        serde_json::json!({ "path": "chat_completions", "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at) }),
                    )
                    .await;
                ctx.next_seq += 1;
            }

            let stream_result = client.stream(body, extra_headers).await;
            match stream_result {
                Ok(raw_stream) => {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "dispatch_ready",
                                serde_json::json!({ "path": "chat_completions", "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at) }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                    let api_stream = spawn_chat_completions_response_stream(
                        raw_stream,
                        self.client.state.provider.stream_idle_timeout(),
                        Some(sse_telemetry),
                    );
                    let (stream, _) =
                        map_response_stream(api_stream, otel_manager.clone(), model_io_context);
                    return Ok(stream);
                }
                Err(ApiError::Transport(
                    unauthorized_transport @ TransportError::Http { status, .. },
                )) if status == StatusCode::UNAUTHORIZED => {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "dispatch_error",
                                serde_json::json!({
                                    "path": "chat_completions",
                                    "error": unauthorized_transport.to_string(),
                                    "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at),
                                }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                    handle_unauthorized(unauthorized_transport, &mut auth_recovery).await?;
                    continue;
                }
                Err(err) => {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "dispatch_error",
                                serde_json::json!({
                                    "path": "chat_completions",
                                    "error": err.to_string(),
                                    "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at),
                                }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                    return Err(map_api_error(err));
                }
            }
        }
    }

    /// Streams a turn via the Responses API over WebSocket transport.
    #[allow(clippy::too_many_arguments)]
    async fn stream_responses_websocket(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        otel_manager: &OtelManager,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        turn_metadata_header: Option<&str>,
    ) -> Result<WebsocketStreamOutcome> {
        let auth_manager = self.client.state.auth_manager.clone();

        let mut auth_recovery = auth_manager
            .as_ref()
            .map(super::auth::AuthManager::unauthorized_recovery);
        loop {
            let client_setup = self.client.current_client_setup().await?;
            let compression = self.responses_request_compression(client_setup.auth.as_ref());

            let options = self.build_responses_options(turn_metadata_header, compression);
            let request = self.build_responses_request(
                &client_setup.api_provider,
                prompt,
                model_info,
                effort,
                summary,
            )?;
            let ws_payload = ResponseCreateWsRequest::from(&request);
            let model_io_context = self
                .client
                .state
                .model_io_recorder
                .as_ref()
                .map(|recorder| {
                    let request_id = recorder.next_request_id();
                    (recorder.clone(), request_id)
                });
            let mut model_io_context = if let Some((recorder, request_id)) = model_io_context {
                let request_value = serde_json::json!({
                    "model": request.model,
                    "instructions": request.instructions,
                    "tools": request.tools,
                    "tool_choice": request.tool_choice,
                    "parallel_tool_calls": request.parallel_tool_calls,
                    "reasoning": request.reasoning,
                    "store": request.store,
                    "stream": request.stream,
                    "include": request.include,
                    "prompt_cache_key": request.prompt_cache_key,
                    "text": request.text,
                    "input_len": request.input.len(),
                    "input": build_model_io_input_payload(&request),
                });
                let mut next_seq: u64 = 1;
                recorder
                    .record_request_start(request_id, &request_value)
                    .await;

                for (idx, item) in request.input.iter().enumerate() {
                    recorder
                        .record_request_input_item(request_id, next_seq, idx, item)
                        .await;
                    next_seq += 1;
                }

                Some(ModelIoDebugContext {
                    recorder,
                    request_id,
                    next_seq,
                    request_input_items: request.input.clone(),
                    transport: "responses_websocket",
                    request_started_at: std::time::Instant::now(),
                })
            } else {
                None
            };

            if let Some(ctx) = model_io_context.as_mut() {
                ctx.recorder
                    .record_transport_stage(
                        ctx.request_id,
                        ctx.next_seq,
                        ctx.transport,
                        "connection_start",
                        serde_json::json!({ "path": "responses_websocket", "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at) }),
                    )
                    .await;
                ctx.next_seq += 1;
            }

            match self
                .websocket_connection(
                    otel_manager,
                    client_setup.api_provider,
                    client_setup.api_auth,
                    turn_metadata_header,
                    &options,
                )
                .await
            {
                Ok(_) => {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "connection_ready",
                                serde_json::json!({ "path": "responses_websocket", "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at) }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                }
                Err(ApiError::Transport(TransportError::Http { status, .. }))
                    if status == StatusCode::UPGRADE_REQUIRED =>
                {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "connection_error",
                                serde_json::json!({
                                    "path": "responses_websocket",
                                    "error": "upgrade_required",
                                    "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at),
                                }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                    return Ok(WebsocketStreamOutcome::FallbackToHttp);
                }
                Err(ApiError::Transport(
                    unauthorized_transport @ TransportError::Http { status, .. },
                )) if status == StatusCode::UNAUTHORIZED => {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "connection_error",
                                serde_json::json!({
                                    "path": "responses_websocket",
                                    "error": unauthorized_transport.to_string(),
                                    "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at),
                                }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                    handle_unauthorized(unauthorized_transport, &mut auth_recovery).await?;
                    continue;
                }
                Err(err) => {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "connection_error",
                                serde_json::json!({
                                    "path": "responses_websocket",
                                    "error": err.to_string(),
                                    "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at),
                                }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                    return Err(map_api_error(err));
                }
            }

            let ws_request = self.prepare_websocket_request(ws_payload, &request);
            let mut model_io_context = model_io_context;
            if let Some(ctx) = model_io_context.as_mut() {
                ctx.recorder
                    .record_transport_stage(
                        ctx.request_id,
                        ctx.next_seq,
                        ctx.transport,
                        "dispatch_start",
                        serde_json::json!({ "path": "responses_websocket", "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at) }),
                    )
                    .await;
                ctx.next_seq += 1;
            }

            let stream_result = self
                .connection
                .as_ref()
                .ok_or_else(|| {
                    map_api_error(ApiError::Stream(
                        "websocket connection is unavailable".to_string(),
                    ))
                })?
                .stream_request(ws_request)
                .await;
            let stream_result = match stream_result {
                Ok(stream) => {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "dispatch_ready",
                                serde_json::json!({ "path": "responses_websocket", "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at) }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                    stream
                }
                Err(err) => {
                    if let Some(ctx) = model_io_context.as_mut() {
                        ctx.recorder
                            .record_transport_stage(
                                ctx.request_id,
                                ctx.next_seq,
                                ctx.transport,
                                "dispatch_error",
                                serde_json::json!({
                                    "path": "responses_websocket",
                                    "error": err.to_string(),
                                    "elapsed_ms_since_request_start": elapsed_ms_since(ctx.request_started_at),
                                }),
                            )
                            .await;
                        ctx.next_seq += 1;
                    }
                    return Err(map_api_error(err));
                }
            };
            self.websocket_last_request = Some(request);
            let (stream, last_request_rx) =
                map_response_stream(stream_result, otel_manager.clone(), model_io_context);
            self.websocket_last_response_rx = Some(last_request_rx);

            return Ok(WebsocketStreamOutcome::Stream(stream));
        }
    }

    /// Builds request and SSE telemetry for streaming API calls.
    fn build_streaming_telemetry(
        otel_manager: &OtelManager,
    ) -> (Arc<dyn RequestTelemetry>, Arc<dyn SseTelemetry>) {
        let telemetry = Arc::new(ApiTelemetry::new(otel_manager.clone()));
        let request_telemetry: Arc<dyn RequestTelemetry> = telemetry.clone();
        let sse_telemetry: Arc<dyn SseTelemetry> = telemetry;
        (request_telemetry, sse_telemetry)
    }

    /// Builds telemetry for the Responses API WebSocket transport.
    fn build_websocket_telemetry(otel_manager: &OtelManager) -> Arc<dyn WebsocketTelemetry> {
        let telemetry = Arc::new(ApiTelemetry::new(otel_manager.clone()));
        let websocket_telemetry: Arc<dyn WebsocketTelemetry> = telemetry;
        websocket_telemetry
    }

    #[allow(clippy::too_many_arguments)]
    /// Streams a single model request within the current turn.
    ///
    /// The caller is responsible for passing per-turn settings explicitly (model selection,
    /// reasoning settings, telemetry context, and turn metadata). This method will prefer the
    /// Responses WebSocket transport when enabled and healthy, and will fall back to the HTTP
    /// Responses API transport otherwise.
    pub async fn stream(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        otel_manager: &OtelManager,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        turn_metadata_header: Option<&str>,
    ) -> Result<ResponseStream> {
        let adapter = ProviderAdapterKind::for_provider(&self.client.state.provider);
        adapter.ensure_stream_supported(&self.client.state.provider)?;

        match adapter {
            ProviderAdapterKind::ResponsesNative => {
                let wire_api = self.client.state.provider.wire_api;
                match wire_api {
                    WireApi::Responses => {
                        let websocket_enabled = self.client.responses_websocket_enabled(model_info)
                            && !self.client.websockets_disabled();

                        if websocket_enabled {
                            match self
                                .stream_responses_websocket(
                                    prompt,
                                    model_info,
                                    otel_manager,
                                    effort,
                                    summary,
                                    turn_metadata_header,
                                )
                                .await?
                            {
                                WebsocketStreamOutcome::Stream(stream) => return Ok(stream),
                                WebsocketStreamOutcome::FallbackToHttp => {
                                    self.try_switch_fallback_transport(otel_manager, model_info);
                                }
                            }
                        }

                        self.stream_responses_api(
                            prompt,
                            model_info,
                            otel_manager,
                            effort,
                            summary,
                            turn_metadata_header,
                        )
                        .await
                    }
                }
            }
            ProviderAdapterKind::ChatCompletions => {
                self.stream_chat_completions_api(
                    prompt,
                    model_info,
                    otel_manager,
                    effort,
                    summary,
                    turn_metadata_header,
                )
                .await
            }
        }
    }

    /// Permanently disables WebSockets for this Codex session and resets WebSocket state.
    ///
    /// This is used after exhausting the provider retry budget, to force subsequent requests onto
    /// the HTTP transport.
    ///
    /// Returns `true` if this call activated fallback, or `false` if fallback was already active.
    pub(crate) fn try_switch_fallback_transport(
        &mut self,
        otel_manager: &OtelManager,
        model_info: &ModelInfo,
    ) -> bool {
        let websocket_enabled = self.client.responses_websocket_enabled(model_info);
        let activated = self.activate_http_fallback(websocket_enabled);
        if activated {
            warn!("falling back to HTTP");
            otel_manager.counter(
                "codex.transport.fallback_to_http",
                1,
                &[("from_wire_api", "responses_websocket")],
            );

            self.connection = None;
            self.websocket_last_request = None;
            self.websocket_last_response_rx = None;
        }
        activated
    }
}

/// Parses per-turn metadata into an HTTP header value.
///
/// Invalid values are treated as absent so callers can compare and propagate
/// metadata with the same sanitization path used when constructing headers.
fn parse_turn_metadata_header(turn_metadata_header: Option<&str>) -> Option<HeaderValue> {
    turn_metadata_header.and_then(|value| HeaderValue::from_str(value).ok())
}

/// Builds the extra headers attached to Responses API requests.
///
/// These headers implement Codex-specific conventions:
///
/// - `x-codex-beta-features`: comma-separated beta feature keys enabled for the session.
/// - `x-codex-turn-state`: sticky routing token captured earlier in the turn.
/// - `x-codex-turn-metadata`: optional per-turn metadata for observability.
fn build_responses_headers(
    beta_features_header: Option<&str>,
    turn_state: Option<&Arc<OnceLock<String>>>,
    turn_metadata_header: Option<&HeaderValue>,
) -> ApiHeaderMap {
    let mut headers = ApiHeaderMap::new();
    if let Some(value) = beta_features_header
        && !value.is_empty()
        && let Ok(header_value) = HeaderValue::from_str(value)
    {
        headers.insert("x-codex-beta-features", header_value);
    }
    if let Some(turn_state) = turn_state
        && let Some(state) = turn_state.get()
        && let Ok(header_value) = HeaderValue::from_str(state)
    {
        headers.insert(X_CODEX_TURN_STATE_HEADER, header_value);
    }
    if let Some(header_value) = turn_metadata_header {
        headers.insert(X_CODEX_TURN_METADATA_HEADER, header_value.clone());
    }
    headers
}

fn build_model_io_input_payload(request: &ResponsesApiRequest) -> Value {
    let mut system_input_items = Vec::new();
    let mut user_input_items = Vec::new();
    let mut tool_input_items = Vec::new();
    let mut other_input_items = Vec::new();

    for item in &request.input {
        match item {
            ResponseItem::Message { role, .. } if role == "user" => {
                user_input_items.push(item.clone())
            }
            ResponseItem::Message { role, .. } if role == "system" || role == "developer" => {
                system_input_items.push(item.clone());
            }
            ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::WebSearchCall { .. } => tool_input_items.push(item.clone()),
            ResponseItem::Message { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::Other => other_input_items.push(item.clone()),
        }
    }

    json!({
        "systemPrompt": &request.instructions,
        "availableTools": &request.tools,
        "toolChoice": &request.tool_choice,
        "parallelToolCalls": request.parallel_tool_calls,
        "systemInputItems": system_input_items,
        "userInputItems": user_input_items,
        "toolInputItems": tool_input_items,
        "otherInputItems": other_input_items,
    })
}

fn build_model_io_output_payload(items: &[ResponseItem]) -> Value {
    let mut assistant_output_items = Vec::new();
    let mut tool_output_items = Vec::new();
    let mut other_output_items = Vec::new();

    for item in items {
        match item {
            ResponseItem::Message { .. } | ResponseItem::Reasoning { .. } => {
                assistant_output_items.push(item.clone());
            }
            ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::WebSearchCall { .. } => tool_output_items.push(item.clone()),
            ResponseItem::GhostSnapshot { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::Other => other_output_items.push(item.clone()),
        }
    }

    json!({
        "assistantOutputItems": assistant_output_items,
        "toolOutputItems": tool_output_items,
        "otherOutputItems": other_output_items,
    })
}

fn build_model_io_response_event_payload(event: &ResponseEvent) -> Value {
    match event {
        ResponseEvent::Created => serde_json::json!({ "type": "created" }),
        ResponseEvent::OutputItemDone(item) => {
            serde_json::json!({ "type": "output_item_done", "item": item })
        }
        ResponseEvent::OutputItemAdded(item) => {
            serde_json::json!({ "type": "output_item_added", "item": item })
        }
        ResponseEvent::ServerReasoningIncluded(value) => {
            serde_json::json!({ "type": "server_reasoning_included", "value": value })
        }
        ResponseEvent::Completed {
            response_id,
            token_usage,
            can_append,
        } => serde_json::json!({
            "type": "completed",
            "response_id": response_id,
            "token_usage": token_usage,
            "can_append": can_append,
        }),
        ResponseEvent::OutputTextDelta(delta) => {
            serde_json::json!({ "type": "output_text_delta", "delta": delta })
        }
        ResponseEvent::ReasoningSummaryDelta {
            delta,
            summary_index,
        } => serde_json::json!({
            "type": "reasoning_summary_delta",
            "delta": delta,
            "summary_index": summary_index,
        }),
        ResponseEvent::ReasoningContentDelta {
            delta,
            content_index,
        } => serde_json::json!({
            "type": "reasoning_content_delta",
            "delta": delta,
            "content_index": content_index,
        }),
        ResponseEvent::ReasoningSummaryPartAdded { summary_index } => serde_json::json!({
            "type": "reasoning_summary_part_added",
            "summary_index": summary_index,
        }),
        ResponseEvent::RateLimits(snapshot) => {
            serde_json::json!({ "type": "rate_limits", "snapshot": snapshot })
        }
        ResponseEvent::ModelsEtag(etag) => {
            serde_json::json!({ "type": "models_etag", "etag": etag })
        }
    }
}

fn should_record_model_io_response_event(event: &ResponseEvent) -> bool {
    !matches!(event, ResponseEvent::ReasoningSummaryDelta { .. })
}

fn build_model_io_final_user_assistant_pair(
    request_input_items: &[ResponseItem],
    output_items: &[ResponseItem],
) -> Value {
    let user = request_input_items
        .iter()
        .rev()
        .find_map(|item| match item {
            ResponseItem::Message { role, .. } if role == "user" => Some(item.clone()),
            _ => None,
        });
    let assistant = output_items
        .iter()
        .rev()
        .find_map(|item| match item {
            ResponseItem::Message { role, .. } if role == "assistant" => Some(item.clone()),
            _ => None,
        })
        .or_else(|| {
            request_input_items
                .iter()
                .rev()
                .find_map(|item| match item {
                    ResponseItem::Message { role, .. } if role == "assistant" => Some(item.clone()),
                    _ => None,
                })
        });
    serde_json::json!({
        "user": user,
        "assistant": assistant,
    })
}

fn map_response_stream<S>(
    api_stream: S,
    otel_manager: OtelManager,
    model_io_context: Option<ModelIoDebugContext>,
) -> (ResponseStream, oneshot::Receiver<LastResponse>)
where
    S: futures::Stream<Item = std::result::Result<ResponseEvent, ApiError>>
        + Unpin
        + Send
        + 'static,
{
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);
    let (tx_last_response, rx_last_response) = oneshot::channel::<LastResponse>();

    tokio::spawn(async move {
        let mut logged_error = false;
        let mut tx_last_response = Some(tx_last_response);
        let mut items_added: Vec<ResponseItem> = Vec::new();
        let mut recorded_outcome = false;
        let mut saw_first_api_event = false;
        let mut api_stream = api_stream;
        let (recorder, request_id, mut seq, request_input_items, transport, request_started_at) =
            if let Some(ctx) = model_io_context {
                (
                    Some(ctx.recorder),
                    ctx.request_id,
                    ctx.next_seq,
                    Some(ctx.request_input_items),
                    ctx.transport,
                    Some(ctx.request_started_at),
                )
            } else {
                (None, 0, 0, None, "unknown", None)
            };
        while let Some(event) = api_stream.next().await {
            if !saw_first_api_event {
                if let Some(recorder) = recorder.as_ref() {
                    let details = match &event {
                        Ok(response_event) => serde_json::json!({
                            "event_type": build_model_io_response_event_payload(response_event)
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown"),
                            "first_event_kind": first_response_event_kind(response_event),
                            "elapsed_ms_since_request_start": request_started_at
                                .map(elapsed_ms_since)
                                .unwrap_or(0),
                        }),
                        Err(err) => serde_json::json!({
                            "error": err.to_string(),
                            "elapsed_ms_since_request_start": request_started_at
                                .map(elapsed_ms_since)
                                .unwrap_or(0),
                        }),
                    };
                    let stage = if event.is_ok() {
                        "first_event_received"
                    } else {
                        "first_event_error"
                    };
                    recorder
                        .record_transport_stage(request_id, seq, transport, stage, details)
                        .await;
                    seq += 1;
                }
                saw_first_api_event = true;
            }
            match event {
                Ok(ResponseEvent::OutputItemDone(item)) => {
                    items_added.push(item.clone());
                    if let Some(recorder) = recorder.as_ref() {
                        record_tool_call_origin(recorder, request_id, seq, &item).await;
                        recorder
                            .record_response_event(
                                request_id,
                                seq,
                                build_model_io_response_event_payload(
                                    &ResponseEvent::OutputItemDone(item.clone()),
                                ),
                            )
                            .await;
                        seq += 1;
                    }
                    if tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(item)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(ResponseEvent::OutputItemAdded(item)) => {
                    if let Some(recorder) = recorder.as_ref() {
                        record_tool_call_origin(recorder, request_id, seq, &item).await;
                        recorder
                            .record_response_event(
                                request_id,
                                seq,
                                build_model_io_response_event_payload(
                                    &ResponseEvent::OutputItemAdded(item.clone()),
                                ),
                            )
                            .await;
                        seq += 1;
                    }
                    if tx_event
                        .send(Ok(ResponseEvent::OutputItemAdded(item)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(ResponseEvent::Completed {
                    response_id,
                    token_usage,
                    can_append,
                }) => {
                    if let Some(usage) = &token_usage {
                        otel_manager.sse_event_completed(
                            usage.input_tokens,
                            usage.output_tokens,
                            Some(usage.cached_input_tokens),
                            Some(usage.reasoning_output_tokens),
                            usage.total_tokens,
                        );
                    }
                    let completed_items = std::mem::take(&mut items_added);
                    if let Some(sender) = tx_last_response.take() {
                        let _ = sender.send(LastResponse {
                            response_id: response_id.clone(),
                            items_added: completed_items.clone(),
                            can_append,
                        });
                    }
                    if let Some(recorder) = recorder.as_ref() {
                        if let Some(request_input_items) = request_input_items.as_ref() {
                            recorder
                                .record_response_event(
                                    request_id,
                                    seq,
                                    serde_json::json!({
                                        "type": "final_user_assistant_pair",
                                        "pair": build_model_io_final_user_assistant_pair(
                                            request_input_items,
                                            &completed_items,
                                        ),
                                    }),
                                )
                                .await;
                            seq += 1;
                        }
                        recorder
                            .record_response_event(
                                request_id,
                                seq,
                                serde_json::json!({
                                    "type": "completed_items",
                                    "output": build_model_io_output_payload(&completed_items),
                                }),
                            )
                            .await;
                        seq += 1;
                        recorder
                            .record_response_event(
                                request_id,
                                seq,
                                build_model_io_response_event_payload(&ResponseEvent::Completed {
                                    response_id: response_id.clone(),
                                    token_usage: token_usage.clone(),
                                    can_append,
                                }),
                            )
                            .await;
                        seq += 1;
                        recorder
                            .record_request_end(
                                request_id,
                                seq,
                                &serde_json::json!({
                                    "type": "completed",
                                    "response_id": response_id,
                                    "token_usage": token_usage,
                                    "can_append": can_append,
                                }),
                            )
                            .await;
                        recorded_outcome = true;
                        seq += 1;
                    }
                    if tx_event
                        .send(Ok(ResponseEvent::Completed {
                            response_id,
                            token_usage,
                            can_append,
                        }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(event) => {
                    if let Some(recorder) = recorder.as_ref()
                        && should_record_model_io_response_event(&event)
                    {
                        recorder
                            .record_response_event(
                                request_id,
                                seq,
                                build_model_io_response_event_payload(&event),
                            )
                            .await;
                        seq += 1;
                    }
                    if tx_event.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
                Err(err) => {
                    let mapped = map_api_error(err);
                    if !logged_error {
                        otel_manager.see_event_completed_failed(&mapped);
                        logged_error = true;
                    }
                    if let Some(recorder) = recorder.as_ref() {
                        recorder
                            .record_request_end(
                                request_id,
                                seq,
                                &serde_json::json!({
                                    "type": "error",
                                    "error": mapped.to_string(),
                                }),
                            )
                            .await;
                        recorded_outcome = true;
                        seq += 1;
                    }
                    if tx_event.send(Err(mapped)).await.is_err() {
                        return;
                    }
                }
            }
        }

        // If the upstream stream ends without a terminal `Completed` (for example cancellation),
        // try to persist whatever we saw.
        if !saw_first_api_event && let Some(recorder) = recorder.as_ref() {
            recorder
                .record_transport_stage(
                    request_id,
                    seq,
                    transport,
                    "stream_ended_before_first_event",
                    serde_json::json!({
                        "elapsed_ms_since_request_start": request_started_at
                            .map(elapsed_ms_since)
                            .unwrap_or(0),
                    }),
                )
                .await;
            seq += 1;
        }
        if !recorded_outcome && let Some(recorder) = recorder.as_ref() {
            recorder
                .record_request_end(
                    request_id,
                    seq,
                    &serde_json::json!({
                        "type": "stream_ended_without_completed",
                        "item_count": items_added.len(),
                    }),
                )
                .await;
        }
    });

    (ResponseStream { rx_event }, rx_last_response)
}

fn spawn_chat_completions_response_stream(
    raw_stream: RawStreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) -> codex_api::ResponseStream {
    let (tx_event, rx_event) = mpsc::channel::<std::result::Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(process_chat_completions_sse(
        raw_stream.bytes,
        tx_event,
        idle_timeout,
        telemetry,
    ));
    codex_api::ResponseStream { rx_event }
}

async fn process_chat_completions_sse(
    bytes: codex_client::ByteStream,
    tx_event: mpsc::Sender<std::result::Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) {
    let mut stream = bytes.eventsource();
    let mut normalizer = ChatCompletionsChunkNormalizer::default();

    loop {
        let start = std::time::Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        if let Some(telemetry) = telemetry.as_ref() {
            telemetry.on_sse_poll(&response, start.elapsed());
        }

        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(err))) => {
                let _ = tx_event.send(Err(ApiError::Stream(err.to_string()))).await;
                return;
            }
            Ok(None) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream(
                        "chat completions stream closed before completion".to_string(),
                    )))
                    .await;
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream(
                        "idle timeout waiting for chat completions SSE".to_string(),
                    )))
                    .await;
                return;
            }
        };

        trace!("Chat Completions SSE event: {}", &sse.data);
        let events = if sse.data.trim() == "[DONE]" {
            normalizer.finish_done().map_err(|err| err.to_string())
        } else {
            match serde_json::from_str::<Value>(&sse.data) {
                Ok(value) => normalizer
                    .push_chunk_value(value)
                    .map_err(|err| err.to_string()),
                Err(err) => Err(format!("failed to parse chat completions SSE event: {err}")),
            }
        };

        let events = match events {
            Ok(events) => events,
            Err(err) => {
                let _ = tx_event.send(Err(ApiError::Stream(err))).await;
                return;
            }
        };

        for event in events {
            let is_completed = matches!(event, ResponseEvent::Completed { .. });
            if tx_event.send(Ok(event)).await.is_err() {
                return;
            }
            if is_completed {
                return;
            }
        }
    }
}

async fn record_tool_call_origin(
    recorder: &ModelIoRecorder,
    request_id: u64,
    seq: u64,
    item: &ResponseItem,
) {
    match item {
        ResponseItem::FunctionCall { call_id, name, .. } => {
            recorder
                .register_tool_call_origin(call_id.clone(), request_id, seq, name.clone())
                .await;
        }
        ResponseItem::CustomToolCall { call_id, name, .. } => {
            recorder
                .register_tool_call_origin(call_id.clone(), request_id, seq, name.clone())
                .await;
        }
        ResponseItem::LocalShellCall { call_id, .. } => {
            if let Some(call_id) = call_id.as_ref() {
                recorder
                    .register_tool_call_origin(
                        call_id.clone(),
                        request_id,
                        seq,
                        "local_shell".to_string(),
                    )
                    .await;
            }
        }
        _ => {}
    }
}

/// Handles a 401 response by optionally refreshing ChatGPT tokens once.
///
/// When refresh succeeds, the caller should retry the API call; otherwise
/// the mapped `CodexErr` is returned to the caller.
async fn handle_unauthorized(
    transport: TransportError,
    auth_recovery: &mut Option<UnauthorizedRecovery>,
) -> Result<()> {
    if let Some(recovery) = auth_recovery
        && recovery.has_next()
    {
        return match recovery.next().await {
            Ok(_) => Ok(()),
            Err(RefreshTokenError::Permanent(failed)) => Err(CodexErr::RefreshTokenFailed(failed)),
            Err(RefreshTokenError::Transient(other)) => Err(CodexErr::Io(other)),
        };
    }

    Err(map_api_error(ApiError::Transport(transport)))
}

fn elapsed_ms_since(started_at: std::time::Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn first_response_event_kind(event: &ResponseEvent) -> &'static str {
    match event {
        ResponseEvent::Created => "created",
        ResponseEvent::OutputItemDone(_) => "output_item_done",
        ResponseEvent::OutputItemAdded(_) => "output_item_added",
        ResponseEvent::ServerReasoningIncluded(_) => "server_reasoning_included",
        ResponseEvent::Completed { .. } => "completed",
        ResponseEvent::OutputTextDelta(_) => "output_text_delta",
        ResponseEvent::ReasoningSummaryDelta { .. } => "reasoning_summary_delta",
        ResponseEvent::ReasoningContentDelta { .. } => "reasoning_content_delta",
        ResponseEvent::ReasoningSummaryPartAdded { .. } => "reasoning_summary_part_added",
        ResponseEvent::RateLimits(_) => "rate_limits",
        ResponseEvent::ModelsEtag(_) => "models_etag",
    }
}

struct ApiTelemetry {
    otel_manager: OtelManager,
}

impl ApiTelemetry {
    fn new(otel_manager: OtelManager) -> Self {
        Self { otel_manager }
    }
}

impl RequestTelemetry for ApiTelemetry {
    fn on_request(
        &self,
        attempt: u64,
        status: Option<HttpStatusCode>,
        error: Option<&TransportError>,
        duration: Duration,
    ) {
        let error_message = error.map(std::string::ToString::to_string);
        self.otel_manager.record_api_request(
            attempt,
            status.map(|s| s.as_u16()),
            error_message.as_deref(),
            duration,
        );
    }
}

impl SseTelemetry for ApiTelemetry {
    fn on_sse_poll(
        &self,
        result: &std::result::Result<
            Option<std::result::Result<Event, EventStreamError<TransportError>>>,
            tokio::time::error::Elapsed,
        >,
        duration: Duration,
    ) {
        self.otel_manager.log_sse_event(result, duration);
    }
}

impl WebsocketTelemetry for ApiTelemetry {
    fn on_ws_request(&self, duration: Duration, error: Option<&ApiError>) {
        let error_message = error.map(std::string::ToString::to_string);
        self.otel_manager
            .record_websocket_request(duration, error_message.as_deref());
    }

    fn on_ws_event(
        &self,
        result: &std::result::Result<Option<std::result::Result<Message, Error>>, ApiError>,
        duration: Duration,
    ) {
        self.otel_manager.record_websocket_event(result, duration);
    }
}

#[cfg(test)]
mod tests {
    use super::ModelClient;
    use super::ResponseEvent;
    use super::ResponsesApiRequest;
    use super::build_model_io_final_user_assistant_pair;
    use super::build_model_io_input_payload;
    use super::build_model_io_output_payload;
    use super::process_chat_completions_sse;
    use super::should_record_model_io_response_event;
    use crate::client_common::Prompt;
    use bytes::Bytes;
    use codex_api::error::ApiError;
    use codex_otel::OtelManager;
    use codex_protocol::ThreadId;
    use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::openai_models::ModelInfo;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn chat_sse_stream(body: impl Into<String>) -> codex_client::ByteStream {
        futures::stream::iter(vec![Ok::<Bytes, codex_client::TransportError>(
            Bytes::from(body.into()),
        )])
        .boxed()
    }

    async fn collect_chat_sse(
        body: impl Into<String>,
    ) -> Vec<std::result::Result<ResponseEvent, ApiError>> {
        let (tx, mut rx) = mpsc::channel(16);
        process_chat_completions_sse(
            chat_sse_stream(body),
            tx,
            std::time::Duration::from_secs(1),
            None,
        )
        .await;

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    }

    fn test_model_client(session_source: SessionSource) -> ModelClient {
        let provider = crate::model_provider_info::create_oss_provider_with_base_url(
            "https://example.com/v1",
            crate::model_provider_info::WireApi::Responses,
        );
        ModelClient::new(
            None,
            ThreadId::new(),
            provider,
            session_source,
            None,
            false,
            false,
            false,
            false,
            None,
            None,
        )
    }

    fn test_model_info() -> ModelInfo {
        serde_json::from_value(json!({
            "slug": "gpt-test",
            "display_name": "gpt-test",
            "description": "desc",
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [
                {"effort": "medium", "description": "medium"}
            ],
            "shell_type": "shell_command",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 1,
            "upgrade": null,
            "base_instructions": "base instructions",
            "model_messages": null,
            "supports_reasoning_summaries": false,
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": null,
            "truncation_policy": {"mode": "bytes", "limit": 10000},
            "supports_parallel_tool_calls": false,
            "context_window": 272000,
            "auto_compact_token_limit": null,
            "experimental_supported_tools": []
        }))
        .expect("deserialize test model info")
    }

    fn test_otel_manager() -> OtelManager {
        OtelManager::new(
            ThreadId::new(),
            "gpt-test",
            "gpt-test",
            None,
            None,
            None,
            "test-originator".to_string(),
            false,
            "test-terminal".to_string(),
            SessionSource::Cli,
        )
    }

    #[test]
    fn build_subagent_headers_sets_other_subagent_label() {
        let client = test_model_client(SessionSource::SubAgent(SubAgentSource::Other(
            "memory_consolidation".to_string(),
        )));
        let headers = client.build_subagent_headers();
        let value = headers
            .get("x-openai-subagent")
            .and_then(|value| value.to_str().ok());
        assert_eq!(value, Some("memory_consolidation"));
    }

    #[tokio::test]
    async fn chat_sse_done_completes_without_json_parse() {
        let events = collect_chat_sse("data: [DONE]\n\n").await;

        assert_eq!(events.len(), 1);
        match events.into_iter().next().expect("event") {
            Ok(ResponseEvent::Completed {
                response_id,
                token_usage,
                can_append,
            }) => {
                assert_eq!(response_id, "");
                assert_eq!(token_usage, None);
                assert_eq!(can_append, false);
            }
            other => panic!("expected completed event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_sse_finish_reason_completes_without_done() {
        let chunk = json!({
            "id": "chatcmpl-test",
            "choices": [{
                "delta": {"content": "hello"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 2,
                "completion_tokens": 1,
                "total_tokens": 3
            }
        });
        let events = collect_chat_sse(format!("data: {chunk}\n\n")).await;

        assert_eq!(events.len(), 3);
        match &events[0] {
            Ok(ResponseEvent::OutputTextDelta(delta)) => assert_eq!(delta, "hello"),
            other => panic!("expected text delta, got {other:?}"),
        }
        match &events[1] {
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. })) => {
                assert_eq!(
                    content,
                    &vec![ContentItem::OutputText {
                        text: "hello".to_string(),
                    }]
                );
            }
            other => panic!("expected assistant message, got {other:?}"),
        }
        match &events[2] {
            Ok(ResponseEvent::Completed {
                response_id,
                token_usage,
                can_append,
            }) => {
                assert_eq!(response_id, "chatcmpl-test");
                let usage = token_usage.as_ref().expect("usage");
                assert_eq!(usage.input_tokens, 2);
                assert_eq!(usage.output_tokens, 1);
                assert_eq!(usage.total_tokens, 3);
                assert_eq!(*can_append, false);
            }
            other => panic!("expected completed event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_sse_malformed_json_fails_closed() {
        let events = collect_chat_sse("data: {not json}\n\n").await;

        assert_eq!(events.len(), 1);
        match events.into_iter().next().expect("event") {
            Err(ApiError::Stream(message)) => {
                assert!(
                    message.contains("failed to parse chat completions SSE event"),
                    "unexpected error: {message}"
                );
            }
            other => panic!("expected stream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn summarize_memories_returns_empty_for_empty_input() {
        let client = test_model_client(SessionSource::Cli);
        let model_info = test_model_info();
        let otel_manager = test_otel_manager();

        let output = client
            .summarize_memories(Vec::new(), &model_info, None, &otel_manager)
            .await
            .expect("empty summarize request should succeed");
        assert_eq!(output.len(), 0);
    }

    #[test]
    fn build_responses_request_uses_none_when_no_tools_are_available() {
        let client = test_model_client(SessionSource::Cli);
        let session = client.new_session();
        let model_info = test_model_info();
        let prompt = Prompt {
            input: vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "hello".to_string(),
                }],
                end_turn: None,
                phase: None,
            }],
            tools: Vec::new(),
            parallel_tool_calls: false,
            base_instructions: BaseInstructions {
                text: "Judge whether the agent is actually done.".to_string(),
            },
            personality: None,
            output_schema: None,
        };

        let request = session
            .build_responses_request(
                &client
                    .state
                    .provider
                    .to_api_provider(None)
                    .expect("api provider"),
                &prompt,
                &model_info,
                Some(codex_protocol::openai_models::ReasoningEffort::Minimal),
                ReasoningSummaryConfig::None,
            )
            .expect("request should build");

        assert!(request.tools.is_empty());
        assert_eq!(request.tool_choice, "none");
    }

    #[test]
    fn build_responses_request_keeps_auto_when_tools_exist() {
        let client = test_model_client(SessionSource::Cli);
        let session = client.new_session();
        let model_info = test_model_info();
        let prompt = Prompt {
            input: vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "list files".to_string(),
                }],
                end_turn: None,
                phase: None,
            }],
            tools: vec![crate::client_common::tools::ToolSpec::LocalShell {}],
            parallel_tool_calls: false,
            base_instructions: BaseInstructions {
                text: "Use tools if needed.".to_string(),
            },
            personality: None,
            output_schema: None,
        };

        let request = session
            .build_responses_request(
                &client
                    .state
                    .provider
                    .to_api_provider(None)
                    .expect("api provider"),
                &prompt,
                &model_info,
                Some(codex_protocol::openai_models::ReasoningEffort::Minimal),
                ReasoningSummaryConfig::None,
            )
            .expect("request should build");

        assert_eq!(request.tool_choice, "auto");
        assert_eq!(request.tools.len(), 1);
    }

    #[test]
    fn model_io_input_payload_groups_system_user_and_tool_items() {
        let system_item = ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "follow policy".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let user_item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hello".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let tool_item = ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            arguments: "{\"command\":\"pwd\"}".to_string(),
            call_id: "call-1".to_string(),
        };
        let assistant_history_item = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "working...".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let request = ResponsesApiRequest {
            model: "gpt-test".to_string(),
            instructions: "system prompt text".to_string(),
            input: vec![
                system_item.clone(),
                user_item.clone(),
                tool_item.clone(),
                assistant_history_item.clone(),
            ],
            tools: Vec::new(),
            tool_choice: "auto".to_string(),
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            include: Vec::new(),
            prompt_cache_key: None,
            text: None,
        };

        let payload = build_model_io_input_payload(&request);

        assert_eq!(
            payload,
            json!({
                "systemPrompt": "system prompt text",
                "availableTools": [],
                "toolChoice": "auto",
                "parallelToolCalls": true,
                "systemInputItems": [system_item],
                "userInputItems": [user_item],
                "toolInputItems": [tool_item],
                "otherInputItems": [assistant_history_item],
            })
        );
    }

    #[test]
    fn model_io_output_payload_groups_assistant_and_tool_items() {
        let assistant_item = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "I can run tools".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let tool_item = ResponseItem::FunctionCall {
            id: None,
            name: "list_dir".to_string(),
            arguments: "{\"path\":\".\"}".to_string(),
            call_id: "call-2".to_string(),
        };

        let payload = build_model_io_output_payload(&[assistant_item.clone(), tool_item.clone()]);

        assert_eq!(
            payload,
            json!({
                "assistantOutputItems": [assistant_item],
                "toolOutputItems": [tool_item],
                "otherOutputItems": [],
            })
        );
    }

    #[test]
    fn model_io_final_pair_prefers_latest_user_and_output_assistant() {
        let user_item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "summarize status".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let assistant_output = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "status summary".to_string(),
            }],
            end_turn: None,
            phase: None,
        };

        let pair = build_model_io_final_user_assistant_pair(
            std::slice::from_ref(&user_item),
            std::slice::from_ref(&assistant_output),
        );
        assert_eq!(
            pair,
            json!({
                "user": user_item,
                "assistant": assistant_output,
            })
        );
    }

    #[test]
    fn model_io_final_pair_falls_back_to_latest_assistant_from_request_input() {
        let user_item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "show my recent context".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let assistant_history_item = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "existing assistant context".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let tool_output = ResponseItem::FunctionCallOutput {
            call_id: "call-3".to_string(),
            output: FunctionCallOutputPayload::from_text("{\"ok\":true}".to_string()),
        };

        let pair = build_model_io_final_user_assistant_pair(
            &[assistant_history_item.clone(), user_item.clone()],
            &[tool_output],
        );
        assert_eq!(
            pair,
            json!({
                "user": user_item,
                "assistant": assistant_history_item,
            })
        );
    }

    #[test]
    fn model_io_event_filter_skips_reasoning_summary_delta() {
        let event = ResponseEvent::ReasoningSummaryDelta {
            delta: "partial summary".to_string(),
            summary_index: 0,
        };
        assert_eq!(should_record_model_io_response_event(&event), false);
    }

    #[test]
    fn model_io_event_filter_keeps_reasoning_content_delta() {
        let event = ResponseEvent::ReasoningContentDelta {
            delta: "internal chain".to_string(),
            content_index: 0,
        };
        assert_eq!(should_record_model_io_response_event(&event), true);
    }
}
