## Why

The wait UI currently uses target-agent completion and the target agent's last message as its completion criteria, which diverges from backend wait semantics. This mismatch causes incorrect status transitions and misleading output, so users cannot trust wait results.

## What Changes

- Align wait UI completion logic with backend callback semantics: a wait target completes only when a matching callback reply is received.
- Replace opaque sender/receiver agent IDs in wait UI with readable `agent_name` values.
- Render the callback payload as the wait result content instead of the waited agent's last message.
- Normalize terminal wait-state rendering so running and completed states are mutually consistent with backend events.

## Capabilities

### New Capabilities
- `wait-callback-visibility`: Define wait UI behavior around callback-based completion, callback content rendering, and agent-name presentation.

### Modified Capabilities
- None (no baseline capabilities currently exist under `openspec/specs/`).

## Impact

- Affected systems: wait flow in `codex-rs/tui` and wait result/event plumbing in `codex-rs/core` (or adjacent shared runtime modules).
- User-visible impact: wait rows and completion output now reflect callback receipt and callback content, reducing false completion and incorrect message display.
- Validation impact: update/add wait-focused TUI snapshots and backend wait-flow tests for callback matching, completion status, and rendered result payload.
