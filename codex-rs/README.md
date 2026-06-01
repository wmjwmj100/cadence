# Wecode CLI (Rust Implementation)

Gradence provides Wecode CLI as a standalone, native executable to ensure a zero-dependency install.

## Installing Wecode

Today, the easiest way to install Wecode is via `npm`:

```shell
npm i -g @gradence/wecode
wecode
```

You can also install via Homebrew (`brew install --cask wecode`) or download a platform-specific release directly from our [GitHub Releases](https://github.com/gradence/wecode/releases).

## Documentation quickstart

- First run with Wecode? Start with `docs/getting-started.md` (links to the walkthrough for prompts, keyboard shortcuts, and session management).
- Want deeper control? See `docs/config.md` and `docs/install.md`.

## What's new in the Rust CLI

The Rust implementation is now the maintained Wecode CLI and serves as the default experience. It includes a number of features that the legacy TypeScript CLI never supported.

### Config

Wecode supports a rich set of configuration options. Note that the Rust CLI uses `config.toml` instead of `config.json`. See `docs/config.md` for details.

### Model Context Protocol Support

#### MCP client

Wecode CLI functions as an MCP client that allows the Wecode CLI and IDE extension to connect to MCP servers on startup. See the [`configuration documentation`](docs/config.md#connecting-to-mcp-servers) for details.

#### MCP server (experimental)

Wecode can be launched as an MCP _server_ by running `wecode mcp-server`. This allows _other_ MCP clients to use Wecode as a tool for another agent.

Use the [`@modelcontextprotocol/inspector`](https://github.com/modelcontextprotocol/inspector) to try it out:

```shell
npx @modelcontextprotocol/inspector wecode mcp-server
```

Use `wecode mcp` to add/list/get/remove MCP server launchers defined in `config.toml`, and `wecode mcp-server` to run the MCP server directly.

### Notifications

You can enable notifications by configuring a script that is run whenever the agent finishes a turn. The [notify documentation](docs/config.md#notify) includes a detailed example that explains how to get desktop notifications via [terminal-notifier](https://github.com/julienXX/terminal-notifier) on macOS. When Wecode detects that it is running under WSL 2 inside Windows Terminal (`WT_SESSION` is set), the TUI automatically falls back to native Windows toast notifications so approval prompts and completed turns surface even though Windows Terminal does not implement OSC 9.

### `wecode exec` to run Wecode programmatically/non-interactively

To run Wecode non-interactively, run `wecode exec PROMPT` (you can also pass the prompt via `stdin`) and Wecode will work on your task until it decides that it is done and exits. Output is printed to the terminal directly. You can set the `RUST_LOG` environment variable to see more about what's going on.
Use `wecode exec --ephemeral ...` to run without persisting session rollout files to disk.

### Experimenting with the Wecode Sandbox

To test to see what happens when a command is run under the sandbox provided by Wecode, we provide the following subcommands in Wecode CLI:

```
# macOS
wecode sandbox macos [--full-auto] [--log-denials] [COMMAND]...

# Linux
wecode sandbox linux [--full-auto] [COMMAND]...

# Windows
wecode sandbox windows [--full-auto] [COMMAND]...

# Legacy aliases
wecode debug seatbelt [--full-auto] [--log-denials] [COMMAND]...
wecode debug landlock [--full-auto] [COMMAND]...
```

### Selecting a sandbox policy via `--sandbox`

The Rust CLI exposes a dedicated `--sandbox` (`-s`) flag that lets you pick the sandbox policy **without** having to reach for the generic `-c/--config` option:

```shell
# Run Wecode with the default, read-only sandbox
wecode --sandbox read-only

# Allow the agent to write within the current workspace while still blocking network access
wecode --sandbox workspace-write

# Danger! Disable sandboxing entirely (only do this if you are already running in a container or other isolated env)
wecode --sandbox danger-full-access
```

The same setting can be persisted in `~/.wecode/config.toml` via the top-level `sandbox_mode = "MODE"` key, e.g. `sandbox_mode = "workspace-write"`.

## Code Organization

This folder is the root of a Cargo workspace. It contains quite a bit of experimental code, but here are the key crates:

- [`core/`](./core) contains the business logic for Wecode. Ultimately, we hope this to be a library crate that is generally useful for building other Rust/native applications that use Wecode.
- [`exec/`](./exec) "headless" CLI for use in automation.
- [`tui/`](./tui) CLI that launches a fullscreen TUI built with [Ratatui](https://ratatui.rs/).
- [`cli/`](./cli) CLI multitool that provides the aforementioned CLIs via subcommands.

If you want to contribute or inspect behavior in detail, start by reading the module-level `README.md` files under each crate and run the project workspace from the top-level `codex-rs` directory so shared config, features, and build scripts stay aligned.
