## 1. Trace Schema and Contracts

- [x] 1.1 Define canonical debug trace entry structs (conversation id, entry sequence, timestamp to second, role, agent thread id/name, agent kind, content, source event) in core runtime.
- [x] 1.2 Add protocol/app-server payload types for serving canonical debug trace snapshots to clients.
- [x] 1.3 Add schema versioning fields and backward-compatible serialization rules for trace payloads.

## 2. Conversation-Scoped Aggregator

- [x] 2.1 Implement a conversation-scoped debug trace aggregator that accepts events from primary agent, sub-agents, and coordinator/dispatcher paths.
- [x] 2.2 Add monotonic per-conversation sequence assignment and deterministic ordering tie-break for same-second events.
- [x] 2.3 Integrate agent-name resolution (registered names first, fallback to reserved names, fallback to thread id) into aggregator normalization.

## 3. Role-Complete Capture Integration

- [x] 3.1 Instrument turn input assembly to capture `system`, `developer`, and `user` entries into canonical debug trace.
- [x] 3.2 Instrument assistant output handling to capture `assistant` entries (streaming/complete paths) into canonical debug trace.
- [x] 3.3 Instrument tool invocation/result boundaries and collaboration lifecycle events (`spawn`, `interaction`, `wait`, `remind`) to capture `tool` and collaboration entries.

## 4. Snapshot Persistence and Folder Layout

- [x] 4.1 Create per-conversation debug folder layout under codex debug home and initialize per-conversation metadata files.
- [x] 4.2 Implement canonical snapshot writer that rewrites the full latest history on each update.
- [x] 4.3 Implement atomic temp-file replace strategy so readers never observe partial snapshot files.

## 5. Coordinator and Multi-Agent Coverage

- [x] 5.1 Ensure dispatcher/coordinator trace entries are captured and merged into the same canonical conversation history.
- [x] 5.2 Ensure spawned sub-agent entries are associated with the root conversation timeline while preserving source thread identity.
- [x] 5.3 Add guardrails so missing agent-name registration never drops entries (fallback identity must still render).

## 6. Frontend Debug Visualization

- [x] 6.1 Add client-side data mapping for canonical trace snapshots with role badges, timestamp formatting, and agent name display.
- [x] 6.2 Implement deterministic lane layout rules (1=center, 2=left/right, 3=left/center/right with primary centered, 4+=stable order with horizontal scroll).
- [x] 6.3 Add timeline row rendering that exposes timestamp, role, agent name, and content for each entry.

## 7. Verification and Rollout Readiness

- [x] 7.1 Add unit tests for role-complete capture coverage and actor coverage (primary, sub-agent, coordinator).
- [x] 7.2 Add tests for deterministic ordering (same-second tie-break), overwrite-on-update semantics, and atomic replace behavior.
- [x] 7.3 Add UI tests for name-based lane rendering and three-agent left/center/right layout stability.
- [x] 7.4 Add integration tests validating per-conversation folder creation and end-to-end trace availability from runtime to UI surface.
- [x] 7.5 Document feature flag/config controls, retention behavior, and operational debugging guidance for the new trace pipeline.
