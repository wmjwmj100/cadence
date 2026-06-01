# Configuration

Wecode reads persistent settings from `~/.wecode/config.toml`. Command-line `-c` overrides can use the same keys for one-off runs.

## Select A Model Provider

Use `model_provider` to choose a provider entry, and `model` to choose the provider-specific model name.

```toml
model_provider = "openai"
model = "gpt-5.1-codex-max"
```

Provider definitions live under `[model_providers.<id>]`. Built-in providers are available without a custom block, and user-defined blocks with the same id override the built-in definition.

## Built-In Kimi / Moonshot

Wecode includes a built-in `kimi` provider for Moonshot/Kimi's OpenAI-compatible Chat Completions API. The built-in default is the China open platform base URL `https://api.moonshot.cn/v1`, matching the [Chinese quickstart](https://platform.kimi.com/docs/api/quickstart). International accounts can set `MOONSHOT_BASE_URL=https://api.moonshot.ai/v1`, matching the [international API overview](https://platform.kimi.ai/docs/api/overview). Both platforms use `Authorization: Bearer $MOONSHOT_API_KEY` for authentication and `/v1/chat/completions` for chat requests.

Minimal setup:

```shell
export MOONSHOT_API_KEY="..."
```

```toml
model_provider = "kimi"
model = "kimi-k2.6"  # replace with any Kimi model enabled for your account
```

Optional base URL override:

```shell
export MOONSHOT_BASE_URL="https://api.moonshot.ai/v1"  # international platform override
```

The built-in `kimi` profile uses the Chat Completions adapter internally while keeping Wecode's upper agent loop on the normalized Responses-style contract. Do not add `wire_api = "chat"`; custom providers should keep `wire_api = "responses"` and select the adapter through the provider profile.

If you already have a custom `[model_providers.kimi]` block, it takes precedence over the built-in provider so existing configurations keep working.

## Custom Chat-Completions-Compatible Provider

Use this shape for a GLM, Ollama-compatible gateway, local OpenAI-compatible server, or another provider that accepts Chat Completions requests but should still integrate with Wecode's normalized agent loop:

```toml
model_provider = "my-chat-provider"
model = "provider-model-name"

[model_providers.my-chat-provider]
name = "My Chat Provider"
base_url = "https://provider.example.com/v1"
env_key = "MY_PROVIDER_API_KEY"
wire_api = "responses"
supports_websockets = false
requires_openai_auth = false

[model_providers.my-chat-provider.profile]
compat = "chat_completions_adapter"
tool_call_style = "open_ai"       # also available: "kimi", "glm", "ollama"
history_strategy = "replay"
max_history_messages = 64
max_tool_output_chars = 12000
max_message_chars = 24000
disable_parallel_tool_calls = true
strip_unsupported_params = true
```

Provider profile fields are intentionally narrow. Prefer adding or tuning a provider profile over branching task prompts, benchmark wrappers, or the upper agent loop for provider-specific behavior.

## Connecting To MCP Servers

Define MCP launchers under `[mcp_servers.<name>]` in `~/.wecode/config.toml`, or manage them with `wecode mcp`. A minimal stdio server looks like this:

```toml
[mcp_servers.docs]
command = "docs-mcp-server"
args = ["--stdio"]
```

Streamable HTTP servers use a URL instead:

```toml
[mcp_servers.docs]
url = "https://example.com/mcp"
```

## Notify

Set `notify` to a command array if you want Wecode to run a script when an agent turn completes. The command receives a JSON event payload as its final argument.

```toml
notify = ["notify-send", "Wecode"]
```

## Notes And Limits

- Kimi/Moonshot requests require `MOONSHOT_API_KEY`; Wecode will not make a successful real API call without it. Make sure the key and `MOONSHOT_BASE_URL` belong to the same platform account system.
- The Chat Completions adapter currently targets HTTP/SSE streaming. WebSocket and non-stream unary Chat Completions paths are separate follow-up work.
- Run a small provider smoke test before relying on a new model/provider for benchmark results.
