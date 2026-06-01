## Why

Current debugging does not provide a complete, unified trace of how multi-agent conversations evolve, which makes it difficult to audit decisions, diagnose failures, and explain collaboration behavior. We need a deterministic and user-visible logging model now because agent histories are updated incrementally and cross-agent coordination has become a core runtime path.

## What Changes

- Capture full conversation history for each task across all agent roles (`system`, `developer`, `user`, `assistant`, `tool`) in one continuously updated long history.
- Define history update behavior so each update rewrites prior history state with the latest canonical version, while preserving a complete final task-level timeline.
- Record precise per-message timestamps (to the second) for all participants, including the user-facing primary agent, spawned sub-agents, and the global coordination/dispatcher agent.
- Store logs under a dedicated folder per conversation/task with a stable structure suitable for inspection and replay.
- Render collaboration flow in the frontend with clear side-by-side agent lanes (for example left/center/right for three agents), showing agent names instead of opaque IDs.

## Capabilities

### New Capabilities
- `multi-agent-debug-history`: Produce canonical, full-fidelity conversation history including all message roles and tool interactions across primary, spawned, and global coordinator agents.
- `timestamped-collaboration-trace`: Attach second-level timestamps to every message/event and preserve ordering across agents for accurate timeline reconstruction.
- `conversation-log-organization`: Persist each conversation's debug artifacts in a dedicated folder with overwrite-on-update semantics for current canonical history.
- `agent-name-based-debug-visualization`: Expose a frontend collaboration view that groups logs by human-readable agent name and displays concurrent agent streams clearly.

### Modified Capabilities
None.

## Impact

- Runtime message/event pipeline and history aggregation logic across orchestrator and agent execution paths.
- Agent spawn/coordination metadata handling (name resolution, parent-child relationship, and event ordering).
- Conversation log persistence layer (folder layout, overwrite behavior, and serialization format).
- Frontend debug/observability UI for multi-agent timeline and lane-based rendering.