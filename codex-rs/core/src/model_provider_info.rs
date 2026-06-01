//! Registry of model providers supported by Codex.
//!
//! Providers can be defined in two places:
//!   1. Built-in defaults compiled into the binary so Codex works out-of-the-box.
//!   2. User-defined entries inside `~/.codex/config.toml` under the `model_providers`
//!      key. These override or extend the defaults at runtime.

use crate::auth::AuthMode;
use crate::error::EnvVarError;
use codex_api::Provider as ApiProvider;
use codex_api::provider::RetryConfig as ApiRetryConfig;
use http::HeaderMap;
use http::header::HeaderName;
use http::header::HeaderValue;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;

const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_STREAM_MAX_RETRIES: u64 = 15;
const DEFAULT_REQUEST_MAX_RETRIES: u64 = 15;
/// Hard cap for user-configured `stream_max_retries`.
const MAX_STREAM_MAX_RETRIES: u64 = 100;
/// Hard cap for user-configured `request_max_retries`.
const MAX_REQUEST_MAX_RETRIES: u64 = 100;

const OPENAI_PROVIDER_NAME: &str = "OpenAI";
const KIMI_MOONSHOT_PROVIDER_NAME: &str = "Kimi Moonshot";
const KIMI_MOONSHOT_DEFAULT_BASE_URL: &str = "https://api.moonshot.cn/v1";
const KIMI_MOONSHOT_BASE_URL_ENV_KEY: &str = "MOONSHOT_BASE_URL";
const KIMI_MOONSHOT_API_KEY_ENV_KEY: &str = "MOONSHOT_API_KEY";
const CHAT_WIRE_API_REMOVED_ERROR: &str = "`wire_api = \"chat\"` is no longer supported.\nHow to fix: set `wire_api = \"responses\"` in your provider config.\nMore info: https://github.com/openai/codex/discussions/7782";
pub(crate) const LEGACY_OLLAMA_CHAT_PROVIDER_ID: &str = "ollama-chat";
pub(crate) const OLLAMA_CHAT_PROVIDER_REMOVED_ERROR: &str = "`ollama-chat` is no longer supported.\nHow to fix: replace `ollama-chat` with `ollama` in `model_provider`, `oss_provider`, or `--local-provider`.\nMore info: https://github.com/openai/codex/discussions/7782";

/// Wire protocol that the provider speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WireApi {
    /// The Responses API exposed by OpenAI at `/v1/responses`.
    #[default]
    Responses,
}

/// Compatibility layer used after wecode has built its internal Responses-style request.
///
/// Future model integrations should usually add a provider profile/config entry instead of
/// changing the agent loop. `ResponsesNative` keeps the existing OpenAI/Codex path; the chat
/// adapter is the seam for Kimi, GLM, Ollama, and other OpenAI-compatible chat providers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCompatibility {
    /// Provider accepts OpenAI Responses API requests directly.
    #[default]
    ResponsesNative,
    /// Provider accepts Chat Completions requests; wecode must translate to/from Responses events.
    ChatCompletionsAdapter,
}

/// Provider-specific tool-call normalization policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStyle {
    /// Standard OpenAI-compatible function/tool call messages.
    #[default]
    OpenAi,
    /// Kimi/Moonshot-compatible chat tool calls, including stricter assistant-tool metadata.
    Kimi,
    /// GLM/SiliconFlow-compatible chat tool calls.
    Glm,
    /// Ollama OpenAI-compatible chat tool calls.
    Ollama,
}

/// How to preserve conversation state for providers that do not store Responses state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoryStrategy {
    /// Use the provider's native Responses `previous_response_id` semantics.
    #[default]
    PreviousResponseId,
    /// Replay bounded prior messages when adapting to stateless chat-completions providers.
    Replay,
}

/// Optional provider adapter knobs for non-native base models.
///
/// To add a new model with minimal code, create a new `[model_providers.<id>.profile]` block in
/// config and select `compat = "chat_completions_adapter"`, then tune only the capability flags
/// that differ from the defaults. The upper agent loop should continue to see normalized
/// Responses events and tool calls.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ProviderProfile {
    /// Which compatibility adapter to use for this provider.
    #[serde(default)]
    pub compat: ProviderCompatibility,
    /// How this provider expects chat-completions tool-call messages to be shaped.
    #[serde(default)]
    pub tool_call_style: ToolCallStyle,
    /// How wecode should preserve prior turn state for this provider.
    #[serde(default)]
    pub history_strategy: HistoryStrategy,
    /// Heartbeat interval for adapters that may block before producing upstream deltas.
    #[serde(default)]
    pub heartbeat_interval_ms: Option<u64>,
    /// Maximum prior chat messages to replay for stateless providers.
    #[serde(default)]
    pub max_history_messages: Option<u64>,
    /// Maximum characters of tool output to include in adapted chat messages.
    #[serde(default)]
    pub max_tool_output_chars: Option<u64>,
    /// Maximum characters of any single adapted chat message.
    #[serde(default)]
    pub max_message_chars: Option<u64>,
    /// Disable parallel tool calls when the provider's chat API cannot preserve them reliably.
    #[serde(default)]
    pub disable_parallel_tool_calls: bool,
    /// Some chat providers require assistant tool-call messages to include reasoning metadata.
    #[serde(default)]
    pub requires_reasoning_content_for_tool_calls: bool,
    /// Drop request fields unsupported by the chat adapter instead of forwarding them upstream.
    #[serde(default)]
    pub strip_unsupported_params: bool,
}

impl<'de> Deserialize<'de> for WireApi {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "responses" => Ok(Self::Responses),
            "chat" => Err(serde::de::Error::custom(CHAT_WIRE_API_REMOVED_ERROR)),
            _ => Err(serde::de::Error::unknown_variant(&value, &["responses"])),
        }
    }
}

/// Serializable representation of a provider definition.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderInfo {
    /// Friendly display name.
    pub name: String,
    /// Base URL for the provider's OpenAI-compatible API.
    pub base_url: Option<String>,
    /// Environment variable that stores the user's API key for this provider.
    pub env_key: Option<String>,

    /// Optional instructions to help the user get a valid value for the
    /// variable and set it.
    pub env_key_instructions: Option<String>,

    /// Value to use with `Authorization: Bearer <token>` header. Use of this
    /// config is discouraged in favor of `env_key` for security reasons, but
    /// this may be necessary when using this programmatically.
    pub experimental_bearer_token: Option<String>,

    /// Which wire protocol this provider expects.
    #[serde(default)]
    pub wire_api: WireApi,

    /// Optional query parameters to append to the base URL.
    pub query_params: Option<HashMap<String, String>>,

    /// Additional HTTP headers to include in requests to this provider where
    /// the (key, value) pairs are the header name and value.
    pub http_headers: Option<HashMap<String, String>>,

    /// Optional HTTP headers to include in requests to this provider where the
    /// (key, value) pairs are the header name and _environment variable_ whose
    /// value should be used. If the environment variable is not set, or the
    /// value is empty, the header will not be included in the request.
    pub env_http_headers: Option<HashMap<String, String>>,

    /// Maximum number of times to retry a failed HTTP request to this provider.
    pub request_max_retries: Option<u64>,

    /// Number of times to retry reconnecting a dropped streaming response before failing.
    pub stream_max_retries: Option<u64>,

    /// Idle timeout (in milliseconds) to wait for activity on a streaming response before treating
    /// the connection as lost.
    pub stream_idle_timeout_ms: Option<u64>,

    /// Does this provider require an OpenAI API Key or ChatGPT login token? If true,
    /// user is presented with login screen on first run, and login preference and token/key
    /// are stored in auth.json. If false (which is the default), login screen is skipped,
    /// and API key (if needed) comes from the "env_key" environment variable.
    #[serde(default)]
    pub requires_openai_auth: bool,

    /// Whether this provider supports the Responses API WebSocket transport.
    #[serde(default)]
    pub supports_websockets: bool,

    /// Optional adapter/capability profile for non-native model providers.
    #[serde(default)]
    pub profile: ProviderProfile,
}

impl ModelProviderInfo {
    fn build_header_map(&self) -> crate::error::Result<HeaderMap> {
        let capacity = self.http_headers.as_ref().map_or(0, HashMap::len)
            + self.env_http_headers.as_ref().map_or(0, HashMap::len);
        let mut headers = HeaderMap::with_capacity(capacity);
        if let Some(extra) = &self.http_headers {
            for (k, v) in extra {
                if let (Ok(name), Ok(value)) = (HeaderName::try_from(k), HeaderValue::try_from(v)) {
                    headers.insert(name, value);
                }
            }
        }

        if let Some(env_headers) = &self.env_http_headers {
            for (header, env_var) in env_headers {
                if let Ok(val) = std::env::var(env_var)
                    && !val.trim().is_empty()
                    && let (Ok(name), Ok(value)) =
                        (HeaderName::try_from(header), HeaderValue::try_from(val))
                {
                    headers.insert(name, value);
                }
            }
        }

        Ok(headers)
    }

    pub(crate) fn to_api_provider(
        &self,
        auth_mode: Option<AuthMode>,
    ) -> crate::error::Result<ApiProvider> {
        let default_base_url = if matches!(auth_mode, Some(AuthMode::Chatgpt)) {
            "https://chatgpt.com/backend-api/codex"
        } else {
            "https://api.openai.com/v1"
        };
        let base_url = self
            .base_url
            .clone()
            .unwrap_or_else(|| default_base_url.to_string());

        let headers = self.build_header_map()?;
        let retry = ApiRetryConfig {
            max_attempts: self.request_max_retries(),
            base_delay: Duration::from_millis(200),
            retry_429: false,
            retry_5xx: true,
            retry_transport: true,
        };

        Ok(ApiProvider {
            name: self.name.clone(),
            base_url,
            query_params: self.query_params.clone(),
            headers,
            retry,
            stream_idle_timeout: self.stream_idle_timeout(),
        })
    }

    /// If `env_key` is Some, returns the API key for this provider if present
    /// (and non-empty) in the environment. If `env_key` is required but
    /// cannot be found, returns an error.
    pub fn api_key(&self) -> crate::error::Result<Option<String>> {
        match &self.env_key {
            Some(env_key) => {
                let api_key = std::env::var(env_key)
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| {
                        crate::error::CodexErr::EnvVar(EnvVarError {
                            var: env_key.clone(),
                            instructions: self.env_key_instructions.clone(),
                        })
                    })?;
                Ok(Some(api_key))
            }
            None => Ok(None),
        }
    }

    /// Effective maximum number of request retries for this provider.
    pub fn request_max_retries(&self) -> u64 {
        self.request_max_retries
            .unwrap_or(DEFAULT_REQUEST_MAX_RETRIES)
            .min(MAX_REQUEST_MAX_RETRIES)
    }

    /// Effective maximum number of stream reconnection attempts for this provider.
    pub fn stream_max_retries(&self) -> u64 {
        self.stream_max_retries
            .unwrap_or(DEFAULT_STREAM_MAX_RETRIES)
            .min(MAX_STREAM_MAX_RETRIES)
    }

    /// Effective idle timeout for streaming responses.
    pub fn stream_idle_timeout(&self) -> Duration {
        self.stream_idle_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_millis(DEFAULT_STREAM_IDLE_TIMEOUT_MS))
    }
    pub fn create_openai_provider() -> ModelProviderInfo {
        ModelProviderInfo {
            name: OPENAI_PROVIDER_NAME.into(),
            // Allow users to override the default OpenAI endpoint by
            // exporting `OPENAI_BASE_URL`. This is useful when pointing
            // Codex at a proxy, mock server, or Azure-style deployment
            // without requiring a full TOML override for the built-in
            // OpenAI provider.
            base_url: std::env::var("OPENAI_BASE_URL")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: Some(
                [("version".to_string(), env!("CARGO_PKG_VERSION").to_string())]
                    .into_iter()
                    .collect(),
            ),
            env_http_headers: Some(
                [
                    (
                        "OpenAI-Organization".to_string(),
                        "OPENAI_ORGANIZATION".to_string(),
                    ),
                    ("OpenAI-Project".to_string(), "OPENAI_PROJECT".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            // Use global defaults for retry/timeout unless overridden in config.toml.
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: true,
            supports_websockets: true,
            profile: ProviderProfile::default(),
        }
    }

    /// Built-in Kimi/Moonshot provider profile.
    ///
    /// Adding another OpenAI-compatible chat provider should follow this shape:
    /// choose a stable provider id, set the provider base URL/API-key env var,
    /// and keep all wire differences inside [`ProviderProfile`] plus the adapter
    /// in `provider_adapter.rs` instead of branching the upper agent loop.
    pub fn create_kimi_moonshot_provider() -> ModelProviderInfo {
        let base_url = std::env::var(KIMI_MOONSHOT_BASE_URL_ENV_KEY)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| KIMI_MOONSHOT_DEFAULT_BASE_URL.to_string());
        Self::create_kimi_moonshot_provider_with_base_url(&base_url)
    }

    pub fn create_kimi_moonshot_provider_with_base_url(base_url: &str) -> ModelProviderInfo {
        ModelProviderInfo {
            name: KIMI_MOONSHOT_PROVIDER_NAME.into(),
            base_url: Some(base_url.into()),
            env_key: Some(KIMI_MOONSHOT_API_KEY_ENV_KEY.into()),
            env_key_instructions: Some(
                "Create a Moonshot/Kimi API key, then export MOONSHOT_API_KEY=<your key>."
                    .to_string(),
            ),
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            profile: ProviderProfile {
                compat: ProviderCompatibility::ChatCompletionsAdapter,
                tool_call_style: ToolCallStyle::Kimi,
                history_strategy: HistoryStrategy::Replay,
                max_history_messages: Some(64),
                max_tool_output_chars: Some(12_000),
                max_message_chars: Some(24_000),
                disable_parallel_tool_calls: true,
                requires_reasoning_content_for_tool_calls: true,
                strip_unsupported_params: true,
                ..ProviderProfile::default()
            },
        }
    }

    pub fn is_openai(&self) -> bool {
        self.name == OPENAI_PROVIDER_NAME
    }
}

pub const DEFAULT_LMSTUDIO_PORT: u16 = 1234;
pub const DEFAULT_OLLAMA_PORT: u16 = 11434;

pub const LMSTUDIO_OSS_PROVIDER_ID: &str = "lmstudio";
pub const OLLAMA_OSS_PROVIDER_ID: &str = "ollama";
pub const KIMI_MOONSHOT_PROVIDER_ID: &str = "kimi";

/// Built-in default provider list.
pub fn built_in_model_providers() -> HashMap<String, ModelProviderInfo> {
    use ModelProviderInfo as P;

    // Keep built-ins focused on the native provider, the first bundled
    // chat-completions adapter target, and local OSS endpoints. Users can
    // still add or override providers in config.toml.
    [
        ("openai", P::create_openai_provider()),
        (
            KIMI_MOONSHOT_PROVIDER_ID,
            P::create_kimi_moonshot_provider(),
        ),
        (
            OLLAMA_OSS_PROVIDER_ID,
            create_oss_provider(DEFAULT_OLLAMA_PORT, WireApi::Responses),
        ),
        (
            LMSTUDIO_OSS_PROVIDER_ID,
            create_oss_provider(DEFAULT_LMSTUDIO_PORT, WireApi::Responses),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

pub fn create_oss_provider(default_provider_port: u16, wire_api: WireApi) -> ModelProviderInfo {
    // These CODEX_OSS_ environment variables are experimental: we may
    // switch to reading values from config.toml instead.
    let default_codex_oss_base_url = format!(
        "http://localhost:{codex_oss_port}/v1",
        codex_oss_port = std::env::var("CODEX_OSS_PORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(default_provider_port)
    );

    let codex_oss_base_url = std::env::var("CODEX_OSS_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(default_codex_oss_base_url);
    create_oss_provider_with_base_url(&codex_oss_base_url, wire_api)
}

pub fn create_oss_provider_with_base_url(base_url: &str, wire_api: WireApi) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "gpt-oss".into(),
        base_url: Some(base_url.into()),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        profile: ProviderProfile::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_deserialize_ollama_model_provider_toml() {
        let azure_provider_toml = r#"
name = "Ollama"
base_url = "http://localhost:11434/v1"
        "#;
        let expected_provider = ModelProviderInfo {
            name: "Ollama".into(),
            base_url: Some("http://localhost:11434/v1".into()),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            profile: ProviderProfile::default(),
        };

        let provider: ModelProviderInfo = toml::from_str(azure_provider_toml).unwrap();
        assert_eq!(expected_provider, provider);
    }

    #[test]
    fn test_deserialize_azure_model_provider_toml() {
        let azure_provider_toml = r#"
name = "Azure"
base_url = "https://xxxxx.openai.azure.com/openai"
env_key = "AZURE_OPENAI_API_KEY"
query_params = { api-version = "2025-04-01-preview" }
        "#;
        let expected_provider = ModelProviderInfo {
            name: "Azure".into(),
            base_url: Some("https://xxxxx.openai.azure.com/openai".into()),
            env_key: Some("AZURE_OPENAI_API_KEY".into()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: Some(maplit::hashmap! {
                "api-version".to_string() => "2025-04-01-preview".to_string(),
            }),
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            profile: ProviderProfile::default(),
        };

        let provider: ModelProviderInfo = toml::from_str(azure_provider_toml).unwrap();
        assert_eq!(expected_provider, provider);
    }

    #[test]
    fn test_deserialize_example_model_provider_toml() {
        let azure_provider_toml = r#"
name = "Example"
base_url = "https://example.com"
env_key = "API_KEY"
http_headers = { "X-Example-Header" = "example-value" }
env_http_headers = { "X-Example-Env-Header" = "EXAMPLE_ENV_VAR" }
        "#;
        let expected_provider = ModelProviderInfo {
            name: "Example".into(),
            base_url: Some("https://example.com".into()),
            env_key: Some("API_KEY".into()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: Some(maplit::hashmap! {
                "X-Example-Header".to_string() => "example-value".to_string(),
            }),
            env_http_headers: Some(maplit::hashmap! {
                "X-Example-Env-Header".to_string() => "EXAMPLE_ENV_VAR".to_string(),
            }),
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            profile: ProviderProfile::default(),
        };

        let provider: ModelProviderInfo = toml::from_str(azure_provider_toml).unwrap();
        assert_eq!(expected_provider, provider);
    }

    #[test]
    fn test_create_kimi_moonshot_provider_with_chat_profile() {
        let provider = ModelProviderInfo::create_kimi_moonshot_provider_with_base_url(
            "https://api.moonshot.ai/v1",
        );

        assert_eq!(KIMI_MOONSHOT_PROVIDER_NAME, provider.name);
        assert_eq!(
            Some("https://api.moonshot.ai/v1".to_string()),
            provider.base_url
        );
        assert_eq!(
            Some(KIMI_MOONSHOT_API_KEY_ENV_KEY.to_string()),
            provider.env_key
        );
        assert!(
            provider
                .env_key_instructions
                .as_deref()
                .is_some_and(|instructions| instructions.contains(KIMI_MOONSHOT_API_KEY_ENV_KEY))
        );
        assert_eq!(WireApi::Responses, provider.wire_api);
        assert!(!provider.requires_openai_auth);
        assert!(!provider.supports_websockets);
        assert_eq!(
            ProviderCompatibility::ChatCompletionsAdapter,
            provider.profile.compat
        );
        assert_eq!(ToolCallStyle::Kimi, provider.profile.tool_call_style);
        assert_eq!(HistoryStrategy::Replay, provider.profile.history_strategy);
        assert_eq!(Some(64), provider.profile.max_history_messages);
        assert_eq!(Some(12_000), provider.profile.max_tool_output_chars);
        assert_eq!(Some(24_000), provider.profile.max_message_chars);
        assert!(provider.profile.disable_parallel_tool_calls);
        assert!(provider.profile.strip_unsupported_params);
    }

    #[test]
    fn test_builtin_model_providers_include_kimi_moonshot() {
        let providers = built_in_model_providers();
        let provider = providers
            .get(KIMI_MOONSHOT_PROVIDER_ID)
            .expect("built-in kimi provider");

        assert_eq!(KIMI_MOONSHOT_PROVIDER_NAME, provider.name);
        assert_eq!(
            Some(KIMI_MOONSHOT_DEFAULT_BASE_URL.to_string()),
            provider.base_url
        );
        assert_eq!(
            Some(KIMI_MOONSHOT_API_KEY_ENV_KEY.to_string()),
            provider.env_key
        );
        assert_eq!(
            ProviderCompatibility::ChatCompletionsAdapter,
            provider.profile.compat
        );
        assert_eq!(ToolCallStyle::Kimi, provider.profile.tool_call_style);
        assert_eq!(HistoryStrategy::Replay, provider.profile.history_strategy);
    }

    #[test]
    fn test_deserialize_provider_profile_fields_from_toml() {
        let provider_toml = r#"
name = "Kimi Moonshot"
base_url = "https://api.moonshot.cn/v1"
env_key = "MOONSHOT_API_KEY"

[profile]
compat = "chat_completions_adapter"
tool_call_style = "kimi"
history_strategy = "replay"
heartbeat_interval_ms = 15000
max_history_messages = 48
max_tool_output_chars = 12000
max_message_chars = 24000
disable_parallel_tool_calls = true
requires_reasoning_content_for_tool_calls = true
strip_unsupported_params = true
        "#;

        let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
        assert_eq!(
            ProviderCompatibility::ChatCompletionsAdapter,
            provider.profile.compat
        );
        assert_eq!(ToolCallStyle::Kimi, provider.profile.tool_call_style);
        assert_eq!(HistoryStrategy::Replay, provider.profile.history_strategy);
        assert_eq!(Some(15_000), provider.profile.heartbeat_interval_ms);
        assert_eq!(Some(48), provider.profile.max_history_messages);
        assert_eq!(Some(12_000), provider.profile.max_tool_output_chars);
        assert_eq!(Some(24_000), provider.profile.max_message_chars);
        assert!(provider.profile.disable_parallel_tool_calls);
        assert!(provider.profile.requires_reasoning_content_for_tool_calls);
        assert!(provider.profile.strip_unsupported_params);
    }

    #[test]
    fn test_provider_profile_defaults_keep_existing_configs_compatible() {
        let provider_toml = r#"
name = "GLM SiliconFlow"
base_url = "https://api.siliconflow.cn/v1"
env_key = "SILICONFLOW_API_KEY"
        "#;

        let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
        assert_eq!(
            ProviderCompatibility::ResponsesNative,
            provider.profile.compat
        );
        assert_eq!(ToolCallStyle::OpenAi, provider.profile.tool_call_style);
        assert_eq!(
            HistoryStrategy::PreviousResponseId,
            provider.profile.history_strategy
        );
        assert_eq!(None, provider.profile.heartbeat_interval_ms);
        assert_eq!(None, provider.profile.max_history_messages);
        assert_eq!(None, provider.profile.max_tool_output_chars);
        assert_eq!(None, provider.profile.max_message_chars);
        assert!(!provider.profile.disable_parallel_tool_calls);
        assert!(!provider.profile.requires_reasoning_content_for_tool_calls);
        assert!(!provider.profile.strip_unsupported_params);
    }

    #[test]
    fn test_deserialize_chat_wire_api_shows_helpful_error() {
        let provider_toml = r#"
name = "Gradence using Chat Completions"
base_url = "https://api.openai.com/v1"
env_key = "OPENAI_API_KEY"
wire_api = "chat"
        "#;

        let err = toml::from_str::<ModelProviderInfo>(provider_toml).unwrap_err();
        assert!(err.to_string().contains(CHAT_WIRE_API_REMOVED_ERROR));
    }
}
