//! Runtime adapter selection and pure request normalization for model providers.
//!
//! The agent loop speaks a normalized Responses-style contract. New base models
//! should be integrated by adding a provider profile and an adapter here, rather
//! than by branching the upper agent/tool loop. Chat-completions providers use
//! the pure request/event transforms in this module plus the HTTP/SSE runtime in
//! `client.rs`; WebSocket and unary chat transports remain future extensions.

use codex_api::ResponseEvent;
use codex_api::ResponsesApiRequest;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::error::CodexErr;
use crate::error::Result;
use crate::model_provider_info::ModelProviderInfo;
use crate::model_provider_info::ProviderCompatibility;
use crate::model_provider_info::ProviderProfile;

/// Runtime adapter family selected from [`ModelProviderInfo::profile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderAdapterKind {
    /// Provider accepts Responses requests directly; this is the existing path.
    ResponsesNative,
    /// Provider accepts Chat Completions requests and needs request/event translation.
    ChatCompletions,
}

impl ProviderAdapterKind {
    /// Select the runtime adapter for a provider profile.
    pub(crate) fn for_provider(provider: &ModelProviderInfo) -> Self {
        match provider.profile.compat {
            ProviderCompatibility::ResponsesNative => Self::ResponsesNative,
            ProviderCompatibility::ChatCompletionsAdapter => Self::ChatCompletions,
        }
    }

    /// Confirms that the selected adapter can stream through the current runtime.
    ///
    /// This remains as a narrow capability gate for future adapter families. Adding
    /// another model provider should normally only require a profile entry plus any
    /// provider-specific request/event normalization here.
    pub(crate) fn ensure_stream_supported(self, _provider: &ModelProviderInfo) -> Result<()> {
        match self {
            Self::ResponsesNative | Self::ChatCompletions => Ok(()),
        }
    }
}

/// Minimal OpenAI-compatible chat-completions request shape used by the HTTP/SSE runtime.
///
/// Future providers should keep their wire differences contained in this adapter:
/// build this request from a [`ResponsesApiRequest`], send it to the provider's
/// chat endpoint, then normalize returned messages/tool calls back into
/// [`ResponseEvent`] values before handing them to the agent loop.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ChatCompletionsRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tools: Vec<ChatTool>,
    pub(crate) tool_choice: String,
    pub(crate) parallel_tool_calls: bool,
    pub(crate) stream: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ChatMessage {
    #[serde(default = "default_assistant_role")]
    pub(crate) role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tool_calls: Vec<ChatToolCall>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ChatTool {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) function: ChatToolFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ChatToolFunction {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) strict: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ChatToolCall {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default, rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) function: ChatToolCallFunction,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ChatToolCallFunction {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) arguments: String,
}

/// Convert a normalized Responses request into a conservative chat-completions request.
///
/// This intentionally supports only the OpenAI-compatible subset shared by Kimi,
/// GLM, Ollama, and other chat providers: text messages, function tools,
/// assistant tool-call replay, and tool outputs. Unsupported Responses-native
/// items fail closed unless the provider profile explicitly asks to strip them.
pub(crate) fn responses_request_to_chat_request(
    request: &ResponsesApiRequest,
    profile: &ProviderProfile,
) -> Result<ChatCompletionsRequest> {
    let mut messages = Vec::new();
    if !request.instructions.trim().is_empty() {
        messages.push(ChatMessage::text(
            "system",
            truncate_chars(request.instructions.clone(), profile.max_message_chars),
        ));
    }

    for item in bounded_history_items(&request.input, profile) {
        match response_item_to_chat_message(item, profile)? {
            Some(message) => messages.push(message),
            None => {}
        }
    }
    repair_chat_tool_message_sequence(&mut messages);

    Ok(ChatCompletionsRequest {
        model: request.model.clone(),
        messages,
        tools: responses_tools_to_chat_tools(&request.tools, profile)?,
        tool_choice: normalize_tool_choice(&request.tool_choice, profile)?,
        parallel_tool_calls: request.parallel_tool_calls && !profile.disable_parallel_tool_calls,
        stream: request.stream,
    })
}

/// Normalize a completed chat message into Responses-style events for the agent loop.
///
/// This is a pure inbound helper for future runtime work; streaming transports
/// will need to emit deltas and completion events around these terminal items.
#[cfg(test)]
pub(crate) fn chat_message_to_response_events(message: ChatMessage) -> Result<Vec<ResponseEvent>> {
    let mut events = Vec::new();
    if let Some(content) = message.content {
        if !content.is_empty() {
            events.push(ResponseEvent::OutputItemDone(ResponseItem::Message {
                id: None,
                role: message.role.clone(),
                content: vec![ContentItem::OutputText { text: content }],
                end_turn: None,
                phase: None,
            }));
        }
    }

    for tool_call in message.tool_calls {
        if tool_call.kind != "function" {
            return Err(unsupported(format!(
                "unsupported chat tool call type '{}'",
                tool_call.kind
            )));
        }
        events.push(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
            id: None,
            name: tool_call.function.name,
            arguments: tool_call.function.arguments,
            call_id: tool_call.id,
        }));
    }

    Ok(events)
}

/// Normalize a full non-stream Chat Completions response into the Responses events
/// consumed by the agent loop.
#[cfg(test)]
pub(crate) fn chat_completion_response_to_response_events(
    value: Value,
) -> Result<Vec<ResponseEvent>> {
    let response: ChatCompletionResponse = serde_json::from_value(value)?;
    let mut events = Vec::new();
    for choice in response.choices {
        events.extend(chat_message_to_response_events(choice.message)?);
    }
    events.push(ResponseEvent::Completed {
        response_id: response.id.unwrap_or_default(),
        token_usage: response.usage.map(Into::into),
        can_append: false,
    });
    Ok(events)
}

/// Stateful no-network normalizer for OpenAI-compatible Chat Completions stream chunks.
///
/// Runtime code can feed decoded chat chunks here and receive normalized Responses
/// events without exposing provider-specific streaming details to the agent loop.
#[derive(Debug, Default)]
pub(crate) struct ChatCompletionsChunkNormalizer {
    response_id: Option<String>,
    content: String,
    text_item_added: bool,
    tool_calls: Vec<ChatToolCallAccumulator>,
    usage: Option<ChatCompletionUsage>,
    completed: bool,
}

impl ChatCompletionsChunkNormalizer {
    pub(crate) fn push_chunk_value(&mut self, value: Value) -> Result<Vec<ResponseEvent>> {
        let chunk: ChatCompletionChunk = serde_json::from_value(value)?;
        self.push_chunk(chunk)
    }

    pub(crate) fn finish_done(&mut self) -> Result<Vec<ResponseEvent>> {
        self.complete_once()
    }

    fn push_chunk(&mut self, chunk: ChatCompletionChunk) -> Result<Vec<ResponseEvent>> {
        if self.response_id.is_none() {
            self.response_id = chunk.id;
        }
        if chunk.usage.is_some() {
            self.usage = chunk.usage;
        }

        let mut events = Vec::new();
        for choice in chunk.choices {
            if let Some(delta) = choice.delta.content {
                if !delta.is_empty() {
                    if !self.text_item_added {
                        self.text_item_added = true;
                        events.push(ResponseEvent::OutputItemAdded(ResponseItem::Message {
                            id: None,
                            role: "assistant".to_string(),
                            content: Vec::new(),
                            end_turn: None,
                            phase: None,
                        }));
                    }
                    self.content.push_str(&delta);
                    events.push(ResponseEvent::OutputTextDelta(delta));
                }
            }
            for tool_call in choice.delta.tool_calls {
                self.accumulate_tool_call(tool_call)?;
            }
            if choice.finish_reason.is_some() {
                events.extend(self.complete_once()?);
            }
        }
        Ok(events)
    }

    fn accumulate_tool_call(&mut self, delta: ChatToolCallDelta) -> Result<()> {
        if let Some(kind) = delta.kind.as_deref() {
            if kind != "function" {
                return Err(unsupported(format!(
                    "unsupported chat tool call type '{kind}'"
                )));
            }
        }

        let index = delta.index.unwrap_or(self.tool_calls.len() as u64) as usize;
        while self.tool_calls.len() <= index {
            self.tool_calls.push(ChatToolCallAccumulator::default());
        }
        let accumulator = &mut self.tool_calls[index];
        if let Some(id) = delta.id {
            accumulator.id = id;
        }
        if let Some(kind) = delta.kind {
            accumulator.kind = kind;
        }
        if let Some(function) = delta.function {
            if let Some(name) = function.name {
                accumulator.name.push_str(&name);
            }
            if let Some(arguments) = function.arguments {
                accumulator.arguments.push_str(&arguments);
            }
        }
        Ok(())
    }

    fn complete_once(&mut self) -> Result<Vec<ResponseEvent>> {
        if self.completed {
            return Ok(Vec::new());
        }
        self.completed = true;

        let mut events = Vec::new();
        if !self.content.is_empty() {
            events.push(ResponseEvent::OutputItemDone(ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: self.content.clone(),
                }],
                end_turn: None,
                phase: None,
            }));
        }

        for tool_call in &self.tool_calls {
            let kind = if tool_call.kind.is_empty() {
                "function"
            } else {
                tool_call.kind.as_str()
            };
            if kind != "function" {
                return Err(unsupported(format!(
                    "unsupported chat tool call type '{kind}'"
                )));
            }
            events.push(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                id: None,
                name: tool_call.name.clone(),
                arguments: tool_call.arguments.clone(),
                call_id: tool_call.id.clone(),
            }));
        }

        events.push(ResponseEvent::Completed {
            response_id: self.response_id.clone().unwrap_or_default(),
            token_usage: self.usage.clone().map(Into::into),
            can_append: false,
        });
        Ok(events)
    }
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<ChatCompletionChoice>,
    #[serde(default)]
    usage: Option<ChatCompletionUsage>,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct ChatCompletionChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<ChatCompletionChunkChoice>,
    #[serde(default)]
    usage: Option<ChatCompletionUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChunkChoice {
    #[serde(default)]
    delta: ChatCompletionDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatCompletionDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCallDelta {
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    function: Option<ChatToolCallFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCallFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default)]
struct ChatToolCallAccumulator {
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ChatCompletionUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
}

impl From<ChatCompletionUsage> for TokenUsage {
    fn from(value: ChatCompletionUsage) -> Self {
        TokenUsage {
            input_tokens: value.prompt_tokens,
            cached_input_tokens: 0,
            output_tokens: value.completion_tokens,
            reasoning_output_tokens: 0,
            total_tokens: value.total_tokens,
        }
    }
}

fn default_assistant_role() -> String {
    "assistant".to_string()
}

impl ChatMessage {
    fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: normalize_chat_message_role(role.into()),
            content: Some(content.into()),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    fn tool_output(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            reasoning_content: None,
            tool_call_id: Some(call_id.into()),
            tool_calls: Vec::new(),
        }
    }

    fn tool_call(
        call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
        profile: &ProviderProfile,
    ) -> Self {
        Self {
            role: "assistant".to_string(),
            content: None,
            reasoning_content: profile
                .requires_reasoning_content_for_tool_calls
                .then(|| "tool call".to_string()),
            tool_call_id: None,
            tool_calls: vec![ChatToolCall {
                id: call_id.into(),
                kind: "function".to_string(),
                function: ChatToolCallFunction {
                    name: name.into(),
                    arguments: arguments.into(),
                },
            }],
        }
    }
}

fn normalize_chat_message_role(role: String) -> String {
    match role.as_str() {
        "system" | "user" | "assistant" | "tool" => role,
        _ => "user".to_string(),
    }
}

fn bounded_history_items<'a>(
    items: &'a [ResponseItem],
    profile: &ProviderProfile,
) -> &'a [ResponseItem] {
    let Some(max_history_messages) = profile.max_history_messages else {
        return items;
    };
    let max_history_messages = max_history_messages as usize;
    if max_history_messages == 0 || items.len() <= max_history_messages {
        items
    } else {
        &items[items.len() - max_history_messages..]
    }
}

fn response_item_to_chat_message(
    item: &ResponseItem,
    profile: &ProviderProfile,
) -> Result<Option<ChatMessage>> {
    match item {
        ResponseItem::Message { role, content, .. } => Ok(Some(ChatMessage::text(
            role.clone(),
            truncate_chars(
                content_items_to_text(content, profile)?,
                profile.max_message_chars,
            ),
        ))),
        ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        } => Ok(Some(ChatMessage::tool_call(
            call_id.clone(),
            name.clone(),
            arguments.clone(),
            profile,
        ))),
        ResponseItem::FunctionCallOutput { call_id, output } => Ok(Some(ChatMessage::tool_output(
            call_id.clone(),
            truncate_chars(
                output.body.to_text().unwrap_or_default(),
                profile.max_tool_output_chars.or(profile.max_message_chars),
            ),
        ))),
        ResponseItem::CustomToolCallOutput { call_id, output } => {
            Ok(Some(ChatMessage::tool_output(
                call_id.clone(),
                truncate_chars(
                    output.clone(),
                    profile.max_tool_output_chars.or(profile.max_message_chars),
                ),
            )))
        }
        ResponseItem::Reasoning { .. } if profile.strip_unsupported_params => Ok(None),
        ResponseItem::Reasoning { .. } => Err(unsupported(
            "reasoning items are not supported by the chat-completions adapter",
        )),
        unsupported_item if profile.strip_unsupported_params => Ok(None),
        unsupported_item => Err(unsupported(format!(
            "response item type is not supported by the chat-completions adapter: {unsupported_item:?}"
        ))),
    }
}

fn repair_chat_tool_message_sequence(messages: &mut Vec<ChatMessage>) {
    let mut index = 0;
    while index < messages.len() {
        if messages[index].role == "tool" {
            messages.remove(index);
            continue;
        }

        if messages[index].tool_calls.is_empty() {
            index += 1;
            continue;
        }

        let required_call_ids = messages[index]
            .tool_calls
            .iter()
            .map(|tool_call| tool_call.id.clone())
            .collect::<Vec<_>>();
        let mut insert_index = index + 1;
        let mut satisfied_call_ids = Vec::new();

        while insert_index < messages.len() && messages[insert_index].role == "tool" {
            let satisfies_current_call =
                messages[insert_index]
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|tool_call_id| {
                        required_call_ids.contains(tool_call_id)
                            && !satisfied_call_ids.contains(tool_call_id)
                    });
            if satisfies_current_call {
                if let Some(tool_call_id) = messages[insert_index].tool_call_id.clone() {
                    satisfied_call_ids.push(tool_call_id);
                }
                insert_index += 1;
            } else {
                messages.remove(insert_index);
            }
        }

        for call_id in required_call_ids {
            if satisfied_call_ids.contains(&call_id) {
                continue;
            }

            if let Some(offset) = messages[insert_index..].iter().position(|message| {
                message.role == "tool" && message.tool_call_id.as_deref() == Some(call_id.as_str())
            }) {
                let tool_output = messages.remove(insert_index + offset);
                messages.insert(insert_index, tool_output);
            } else {
                messages.insert(insert_index, ChatMessage::tool_output(call_id, "aborted"));
            }
            insert_index += 1;
        }

        index = insert_index;
    }
}

fn content_items_to_text(items: &[ContentItem], profile: &ProviderProfile) -> Result<String> {
    let mut parts = Vec::new();
    for item in items {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                parts.push(text.clone());
            }
            ContentItem::InputImage { .. } if profile.strip_unsupported_params => {}
            ContentItem::InputImage { .. } => {
                return Err(unsupported(
                    "image content is not supported by the chat-completions adapter",
                ));
            }
        }
    }
    Ok(parts.join("\n"))
}

fn responses_tools_to_chat_tools(
    tools: &[Value],
    profile: &ProviderProfile,
) -> Result<Vec<ChatTool>> {
    let mut chat_tools = Vec::new();
    for tool in tools {
        match response_tool_to_chat_tool(tool, profile)? {
            Some(chat_tool) => chat_tools.push(chat_tool),
            None => {}
        }
    }
    Ok(chat_tools)
}

fn response_tool_to_chat_tool(tool: &Value, profile: &ProviderProfile) -> Result<Option<ChatTool>> {
    let Some(kind) = tool.get("type").and_then(Value::as_str) else {
        return unsupported_or_strip("tool is missing string field 'type'", profile);
    };
    if kind != "function" {
        return unsupported_or_strip(
            format!("tool type '{kind}' is not supported by the chat-completions adapter"),
            profile,
        );
    }

    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| unsupported("function tool is missing string field 'name'"))?
        .to_string();
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let parameters = tool
        .get("parameters")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let strict = tool.get("strict").and_then(Value::as_bool);

    Ok(Some(ChatTool {
        kind: "function".to_string(),
        function: ChatToolFunction {
            name,
            description,
            parameters,
            strict,
        },
    }))
}

fn normalize_tool_choice(tool_choice: &str, profile: &ProviderProfile) -> Result<String> {
    match tool_choice {
        "auto" | "none" => Ok(tool_choice.to_string()),
        other if profile.strip_unsupported_params => Ok(other.to_string()),
        other => Err(unsupported(format!(
            "tool_choice '{other}' is not supported by the chat-completions adapter"
        ))),
    }
}

fn unsupported_or_strip<T>(
    message: impl Into<String>,
    profile: &ProviderProfile,
) -> Result<Option<T>> {
    if profile.strip_unsupported_params {
        Ok(None)
    } else {
        Err(unsupported(message))
    }
}

fn truncate_chars(value: String, max_chars: Option<u64>) -> String {
    let Some(max_chars) = max_chars else {
        return value;
    };
    if value.chars().count() <= max_chars as usize {
        return value;
    }
    value.chars().take(max_chars as usize).collect()
}

fn unsupported(message: impl Into<String>) -> CodexErr {
    CodexErr::UnsupportedOperation(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_provider_info::ProviderProfile;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;

    fn test_request() -> ResponsesApiRequest {
        ResponsesApiRequest {
            model: "kimi-k2.6".to_string(),
            instructions: "follow instructions".to_string(),
            input: Vec::new(),
            tools: Vec::new(),
            tool_choice: "none".to_string(),
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            include: Vec::new(),
            prompt_cache_key: None,
            text: None,
        }
    }

    #[test]
    fn default_provider_uses_native_responses_adapter() {
        let provider = ModelProviderInfo::create_openai_provider();

        assert_eq!(
            ProviderAdapterKind::ResponsesNative,
            ProviderAdapterKind::for_provider(&provider)
        );
        assert!(
            ProviderAdapterKind::for_provider(&provider)
                .ensure_stream_supported(&provider)
                .is_ok()
        );
    }

    #[test]
    fn chat_completions_adapter_is_supported_by_http_sse_runtime() {
        let mut provider = ModelProviderInfo::create_openai_provider();
        provider.name = "Kimi Moonshot".to_string();
        provider.profile = ProviderProfile {
            compat: ProviderCompatibility::ChatCompletionsAdapter,
            ..ProviderProfile::default()
        };

        assert_eq!(
            ProviderAdapterKind::ChatCompletions,
            ProviderAdapterKind::for_provider(&provider)
        );
        assert!(
            ProviderAdapterKind::for_provider(&provider)
                .ensure_stream_supported(&provider)
                .is_ok()
        );
    }

    #[test]
    fn converts_instructions_and_text_messages_to_chat_request() {
        let mut request = test_request();
        request.input.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hello".to_string(),
            }],
            end_turn: None,
            phase: None,
        });

        let chat = responses_request_to_chat_request(&request, &ProviderProfile::default())
            .expect("chat request");

        assert_eq!(chat.model, "kimi-k2.6");
        assert_eq!(chat.tool_choice, "none");
        assert_eq!(chat.parallel_tool_calls, true);
        assert_eq!(chat.stream, true);
        assert_eq!(chat.tools, Vec::new());
        assert_eq!(
            chat.messages,
            vec![
                ChatMessage::text("system", "follow instructions"),
                ChatMessage::text("user", "hello"),
            ]
        );
    }

    #[test]
    fn normalizes_non_standard_text_message_roles_for_chat_providers() {
        let mut request = test_request();
        request.input.push(ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "follow these constraints".to_string(),
            }],
            end_turn: None,
            phase: None,
        });

        let chat = responses_request_to_chat_request(&request, &ProviderProfile::default())
            .expect("chat request");

        assert_eq!(chat.messages[1].role, "user");
        assert_eq!(
            chat.messages[1].content.as_deref(),
            Some("follow these constraints")
        );
    }

    #[test]
    fn converts_function_tool_and_disables_parallel_calls_from_profile() {
        let mut request = test_request();
        request.tool_choice = "auto".to_string();
        request.parallel_tool_calls = true;
        request.tools.push(serde_json::json!({
            "type": "function",
            "name": "shell",
            "description": "run a command",
            "strict": false,
            "parameters": {"type": "object", "properties": {}}
        }));
        let profile = ProviderProfile {
            disable_parallel_tool_calls: true,
            ..ProviderProfile::default()
        };

        let chat = responses_request_to_chat_request(&request, &profile).expect("chat request");

        assert_eq!(chat.parallel_tool_calls, false);
        assert_eq!(chat.tools.len(), 1);
        assert_eq!(chat.tools[0].kind, "function");
        assert_eq!(chat.tools[0].function.name, "shell");
        assert_eq!(chat.tools[0].function.strict, Some(false));
    }

    #[test]
    fn fails_closed_for_unsupported_tool_unless_profile_strips_it() {
        let mut request = test_request();
        request
            .tools
            .push(serde_json::json!({"type": "local_shell"}));

        let err = responses_request_to_chat_request(&request, &ProviderProfile::default())
            .expect_err("unsupported tool should fail closed");
        assert!(err.to_string().contains("local_shell"));

        let profile = ProviderProfile {
            strip_unsupported_params: true,
            ..ProviderProfile::default()
        };
        let chat = responses_request_to_chat_request(&request, &profile).expect("chat request");
        assert!(chat.tools.is_empty());
    }

    #[test]
    fn replays_tool_calls_and_tool_outputs_with_bounds() {
        let mut request = test_request();
        request.input.push(ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            arguments: "{\"cmd\":\"pwd\"}".to_string(),
            call_id: "call-1".to_string(),
        });
        request.input.push(ResponseItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                "abcdef".to_string(),
            ),
        });
        let profile = ProviderProfile {
            max_tool_output_chars: Some(3),
            ..ProviderProfile::default()
        };

        let chat = responses_request_to_chat_request(&request, &profile).expect("chat request");

        assert_eq!(
            chat.messages[1],
            ChatMessage::tool_call(
                "call-1",
                "shell",
                "{\"cmd\":\"pwd\"}",
                &ProviderProfile::default()
            )
        );
        assert_eq!(chat.messages[2], ChatMessage::tool_output("call-1", "abc"));
    }

    #[test]
    fn inserts_missing_tool_output_after_replayed_tool_call() {
        let mut request = test_request();
        request.input.push(ResponseItem::FunctionCall {
            id: None,
            name: "exec_command".to_string(),
            arguments: "{\"cmd\":\"pwd\"}".to_string(),
            call_id: "exec_command:0".to_string(),
        });

        let chat = responses_request_to_chat_request(&request, &ProviderProfile::default())
            .expect("chat request");

        assert_eq!(chat.messages.len(), 3);
        assert_eq!(chat.messages[1].role, "assistant");
        assert_eq!(chat.messages[1].tool_calls[0].id, "exec_command:0");
        assert_eq!(
            chat.messages[2],
            ChatMessage::tool_output("exec_command:0", "aborted")
        );
    }

    #[test]
    fn moves_delayed_tool_output_next_to_matching_tool_call() {
        let mut request = test_request();
        request.input.push(ResponseItem::FunctionCall {
            id: None,
            name: "exec_command".to_string(),
            arguments: "{\"cmd\":\"pwd\"}".to_string(),
            call_id: "exec_command:0".to_string(),
        });
        request.input.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "continue".to_string(),
            }],
            end_turn: None,
            phase: None,
        });
        request.input.push(ResponseItem::FunctionCallOutput {
            call_id: "exec_command:0".to_string(),
            output: FunctionCallOutputPayload::from_text("done".to_string()),
        });

        let chat = responses_request_to_chat_request(&request, &ProviderProfile::default())
            .expect("chat request");

        assert_eq!(chat.messages.len(), 4);
        assert_eq!(chat.messages[1].role, "assistant");
        assert_eq!(
            chat.messages[2],
            ChatMessage::tool_output("exec_command:0", "done")
        );
        assert_eq!(chat.messages[3], ChatMessage::text("user", "continue"));
    }

    #[test]
    fn removes_orphan_tool_outputs_from_chat_history() {
        let mut request = test_request();
        request.input.push(ResponseItem::FunctionCallOutput {
            call_id: "orphan-call".to_string(),
            output: FunctionCallOutputPayload::from_text("orphan".to_string()),
        });
        request.input.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hello".to_string(),
            }],
            end_turn: None,
            phase: None,
        });

        let chat = responses_request_to_chat_request(&request, &ProviderProfile::default())
            .expect("chat request");

        assert_eq!(
            chat.messages,
            vec![
                ChatMessage::text("system", "follow instructions"),
                ChatMessage::text("user", "hello"),
            ]
        );
    }

    #[test]
    fn interleaves_multiple_tool_calls_with_matching_outputs() {
        let mut request = test_request();
        request.input.push(ResponseItem::FunctionCall {
            id: None,
            name: "exec_command".to_string(),
            arguments: "{\"cmd\":\"pwd\"}".to_string(),
            call_id: "exec_command:0".to_string(),
        });
        request.input.push(ResponseItem::FunctionCall {
            id: None,
            name: "read_file".to_string(),
            arguments: "{\"path\":\"README.md\"}".to_string(),
            call_id: "read_file:0".to_string(),
        });
        request.input.push(ResponseItem::FunctionCallOutput {
            call_id: "exec_command:0".to_string(),
            output: FunctionCallOutputPayload::from_text("pwd-output".to_string()),
        });
        request.input.push(ResponseItem::FunctionCallOutput {
            call_id: "read_file:0".to_string(),
            output: FunctionCallOutputPayload::from_text("read-output".to_string()),
        });

        let chat = responses_request_to_chat_request(&request, &ProviderProfile::default())
            .expect("chat request");

        assert_eq!(chat.messages.len(), 5);
        assert_eq!(chat.messages[1].tool_calls[0].id, "exec_command:0");
        assert_eq!(
            chat.messages[2],
            ChatMessage::tool_output("exec_command:0", "pwd-output")
        );
        assert_eq!(chat.messages[3].tool_calls[0].id, "read_file:0");
        assert_eq!(
            chat.messages[4],
            ChatMessage::tool_output("read_file:0", "read-output")
        );
    }

    #[test]
    fn adds_reasoning_content_to_replayed_tool_calls_when_profile_requires_it() {
        let mut request = test_request();
        request.input.push(ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            arguments: "{\"cmd\":\"pwd\"}".to_string(),
            call_id: "call-1".to_string(),
        });
        let profile = ProviderProfile {
            requires_reasoning_content_for_tool_calls: true,
            ..ProviderProfile::default()
        };

        let chat = responses_request_to_chat_request(&request, &profile).expect("chat request");

        assert_eq!(chat.messages[1].role, "assistant");
        assert_eq!(
            chat.messages[1].reasoning_content.as_deref(),
            Some("tool call")
        );
    }

    #[test]
    fn normalizes_non_stream_chat_response_to_response_events() {
        let response = serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "done"
                }
            }],
            "usage": {
                "prompt_tokens": 7,
                "completion_tokens": 3,
                "total_tokens": 10
            }
        });

        let events = chat_completion_response_to_response_events(response).expect("events");

        assert_eq!(events.len(), 2);
        match &events[0] {
            ResponseEvent::OutputItemDone(ResponseItem::Message { role, content, .. }) => {
                assert_eq!(role, "assistant");
                assert_eq!(
                    content,
                    &vec![ContentItem::OutputText {
                        text: "done".to_string()
                    }]
                );
            }
            other => panic!("unexpected first event: {other:?}"),
        }
        match &events[1] {
            ResponseEvent::Completed {
                response_id,
                token_usage,
                can_append,
            } => {
                assert_eq!(response_id, "chatcmpl-1");
                assert_eq!(
                    token_usage.as_ref().map(|usage| usage.total_tokens),
                    Some(10)
                );
                assert!(!can_append);
            }
            other => panic!("unexpected second event: {other:?}"),
        }
    }

    #[test]
    fn normalizes_non_stream_chat_tool_call_response() {
        let response = serde_json::json!({
            "id": "chatcmpl-tools",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "shell",
                            "arguments": "{\"cmd\":\"pwd\"}"
                        }
                    }]
                }
            }]
        });

        let events = chat_completion_response_to_response_events(response).expect("events");

        assert_eq!(events.len(), 2);
        match &events[0] {
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) => {
                assert_eq!(name, "shell");
                assert_eq!(arguments, "{\"cmd\":\"pwd\"}");
                assert_eq!(call_id, "call-1");
            }
            other => panic!("unexpected first event: {other:?}"),
        }
        assert!(matches!(events[1], ResponseEvent::Completed { .. }));
    }

    #[test]
    fn stream_chunk_normalizer_accumulates_text_and_completes_once() {
        let mut normalizer = ChatCompletionsChunkNormalizer::default();

        let first = normalizer
            .push_chunk_value(serde_json::json!({
                "id": "chatcmpl-stream",
                "choices": [{"delta": {"content": "hel"}}]
            }))
            .expect("first chunk");
        let second = normalizer
            .push_chunk_value(serde_json::json!({
                "id": "chatcmpl-stream",
                "choices": [{
                    "delta": {"content": "lo"},
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 4,
                    "completion_tokens": 2,
                    "total_tokens": 6
                }
            }))
            .expect("second chunk");
        let done = normalizer.finish_done().expect("done marker");

        assert_eq!(first.len(), 2);
        match &first[0] {
            ResponseEvent::OutputItemAdded(ResponseItem::Message { role, content, .. }) => {
                assert_eq!(role, "assistant");
                assert!(content.is_empty());
            }
            other => panic!("unexpected first chunk item: {other:?}"),
        }
        match &first[1] {
            ResponseEvent::OutputTextDelta(delta) => assert_eq!(delta, "hel"),
            other => panic!("unexpected first chunk delta: {other:?}"),
        }
        assert_eq!(second.len(), 3);
        match &second[0] {
            ResponseEvent::OutputTextDelta(delta) => assert_eq!(delta, "lo"),
            other => panic!("unexpected second chunk delta: {other:?}"),
        }
        match &second[1] {
            ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. }) => {
                assert_eq!(
                    content,
                    &vec![ContentItem::OutputText {
                        text: "hello".to_string()
                    }]
                );
            }
            other => panic!("unexpected completed item: {other:?}"),
        }
        match &second[2] {
            ResponseEvent::Completed {
                response_id,
                token_usage,
                can_append,
            } => {
                assert_eq!(response_id, "chatcmpl-stream");
                assert_eq!(
                    token_usage.as_ref().map(|usage| usage.total_tokens),
                    Some(6)
                );
                assert!(!can_append);
            }
            other => panic!("unexpected completed event: {other:?}"),
        }
        assert!(done.is_empty());
    }

    #[test]
    fn stream_chunk_normalizer_emits_single_text_item_added_before_deltas() {
        let mut normalizer = ChatCompletionsChunkNormalizer::default();

        let first = normalizer
            .push_chunk_value(serde_json::json!({
                "id": "chatcmpl-stream",
                "choices": [{"delta": {"content": "hel"}}]
            }))
            .expect("first chunk");
        let second = normalizer
            .push_chunk_value(serde_json::json!({
                "id": "chatcmpl-stream",
                "choices": [{"delta": {"content": "lo"}}]
            }))
            .expect("second chunk");

        assert!(matches!(first[0], ResponseEvent::OutputItemAdded(_)));
        assert!(matches!(first[1], ResponseEvent::OutputTextDelta(_)));
        assert!(matches!(second[0], ResponseEvent::OutputTextDelta(_)));
        assert!(
            !second
                .iter()
                .any(|event| matches!(event, ResponseEvent::OutputItemAdded(_)))
        );
    }

    #[test]
    fn stream_chunk_normalizer_accumulates_split_tool_calls() {
        let mut normalizer = ChatCompletionsChunkNormalizer::default();

        let first = normalizer
            .push_chunk_value(serde_json::json!({
                "id": "chatcmpl-tool-stream",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": "{\"cmd\":"
                            }
                        }]
                    }
                }]
            }))
            .expect("first chunk");
        let second = normalizer
            .push_chunk_value(serde_json::json!({
                "id": "chatcmpl-tool-stream",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {"arguments": "\"pwd\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }))
            .expect("second chunk");

        assert!(first.is_empty());
        assert_eq!(second.len(), 2);
        match &second[0] {
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) => {
                assert_eq!(name, "shell");
                assert_eq!(arguments, "{\"cmd\":\"pwd\"}");
                assert_eq!(call_id, "call-1");
            }
            other => panic!("unexpected tool-call event: {other:?}"),
        }
        assert!(matches!(second[1], ResponseEvent::Completed { .. }));
    }

    #[test]
    fn stream_chunk_normalizer_fails_closed_for_non_function_tool_calls() {
        let mut normalizer = ChatCompletionsChunkNormalizer::default();

        let err = normalizer
            .push_chunk_value(serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "type": "custom"
                        }]
                    }
                }]
            }))
            .expect_err("unsupported tool call should fail");

        assert!(err.to_string().contains("custom"));
    }
    #[test]
    fn normalizes_chat_message_tool_calls_to_response_events() {
        let message = ChatMessage {
            role: "assistant".to_string(),
            content: Some("working".to_string()),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: vec![ChatToolCall {
                id: "call-1".to_string(),
                kind: "function".to_string(),
                function: ChatToolCallFunction {
                    name: "shell".to_string(),
                    arguments: "{\"cmd\":\"pwd\"}".to_string(),
                },
            }],
        };

        let events = chat_message_to_response_events(message).expect("events");

        assert_eq!(events.len(), 2);
        match &events[0] {
            ResponseEvent::OutputItemDone(ResponseItem::Message { role, content, .. }) => {
                assert_eq!(role, "assistant");
                assert_eq!(
                    content,
                    &vec![ContentItem::OutputText {
                        text: "working".to_string()
                    }]
                );
            }
            other => panic!("unexpected first event: {other:?}"),
        }
        match &events[1] {
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) => {
                assert_eq!(name, "shell");
                assert_eq!(arguments, "{\"cmd\":\"pwd\"}");
                assert_eq!(call_id, "call-1");
            }
            other => panic!("unexpected second event: {other:?}"),
        }
    }
}
