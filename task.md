# CodeX 2S Agent-Centric Production Task List

## Context Contract

The implementation must preserve three distinct context channels:

1. **Manual permanent system prompt**: human-authored, per-agent, durable, and never automatically rewritten by the system.
2. **Automatically updated prompt**: per-agent, durable, system-maintained, updated at most once per day from stable memory signals such as owner capability boundaries, preferences, long-term collaboration judgments, and agent capability boundaries.
3. **Runtime tail injection**: non-durable live context injected through recent user/assistant/runtime items, including other-agent snapshots, shared blackboard state, inbox/waiting state, and current task context. This is not treated as long-term prompt material.

Additional alignment decisions:

- Program state is exactly `idle / working / waiting`; no `blocked` state.
- Agent memory lives under `codex_home`, not inside a mutable workspace.
- Docker runtime is one persistent container per company; projects are sub-workspaces inside that company container.
- Production-grade means durable storage, clear migration/compatibility behavior, deterministic rendering, validation coverage, and observability/debuggability.

## Tasks and Acceptance Criteria

| # | Task | What To Build | Acceptance Result |
|---|---|---|---|
| 1 | Formalize the three-channel context contract | Define a runtime/domain contract separating manual permanent system prompt, automatically updated prompt, and runtime tail injection. Make precedence and ownership explicit. | A developer can inspect one generated model input and see the three channels separated; runtime snapshots and blackboard content never appear in durable manual or automatic prompt files. |
| 2 | Create agent-level durable storage under codex_home | Add per-agent directory layout under `codex_home/agents/<agent_id>/` with identity, manual prompt, automatic prompt, memory, daily reflections, task journal, relationship memory, and metadata. | Switching workspaces preserves the same agent memory; deleting a workspace does not delete agent durable memory; missing directories are created safely and idempotently. |
| 3 | Support human-editable permanent system prompts | Load a per-agent manual permanent system prompt from durable storage and inject it as the human-owned stable prompt channel. Never auto-edit it. | Manual file edits are reflected in the next turn; daily memory update jobs do not modify the manual prompt file; tests prove auto-update code cannot write to it. |
| 4 | Design automatic prompt sections | Define structured sections for automatic prompt material: self capability boundary, owner capability boundary, owner preferences, relationship memory, stable collaboration rules, and confidence/provenance metadata. | Automatic prompt output is deterministic, bounded in size, and contains only stable reusable facts; temporary items like “currently waiting” or “today’s task” are rejected or routed elsewhere. |
| 5 | Implement daily automatic memory update | Add an at-most-once-per-day per-agent update pipeline that reads journals/rollout summaries/collaboration outcomes and refreshes automatic prompt material. | Running the updater twice on the same day is idempotent; running it on a later day creates a new dated reflection and updates the latest automatic prompt. |
| 6 | Add memory admission and demotion rules | Classify candidate facts into long-term prompt, relationship memory, task journal, or reject. Include stability, reuse value, owner relevance, and confidence checks. | Test fixtures show short-lived task state stays out of automatic prompt, while stable facts like owner capability boundaries can enter with provenance. |
| 7 | Preserve runtime tail injection as non-durable context | Keep other-agent snapshots, shared blackboard, waiting state, and current workspace state in runtime injection only; avoid rewriting this path into durable prompt files. | Blackboard changes affect the current/future model input but do not write to manual prompt, automatic prompt, or long-term memory unless explicitly summarized by the memory pipeline. |
| 8 | Introduce minimal agent external state | Add an agent-facing state layer with exactly `idle`, `working`, and `waiting`, mapped from current runtime signals without adding `blocked`. | Every live agent exposes one of the three states; blocked-like situations are represented as `waiting` plus natural-language reason, never as a fourth enum variant. |
| 9 | Bind owner and human inbox to agent records | Make login/session APIs expose owner identity, bound agent, agent state, pending owner replies, and agent-specific inbox view. Route owner replies to that agent by default. | User A sees only user A’s bound agent state/inbox; replies from User A cannot be delivered to User B’s agent without explicit targeting. |
| 10 | Add per-agent task journals | Persist task-level journals as memory update inputs, distinct from prompt injection. Include provenance for why a stable memory was created. | A long-term memory entry can be traced back to journal/reflection evidence; deleting an unsupported memory does not cause immediate ungrounded restoration. |
| 11 | Separate private memory from shared state | Keep per-agent memory private by default; only explicit summaries or artifacts enter shared blackboard/public workspace. | Agent A’s private relationship memory is not visible to Agent B by default; shared blackboard contains only explicitly published content. |
| 12 | Implement company-level persistent Docker containers | Replace per-call Docker containers with a company-scoped persistent container backend using `docker exec`, with project subdirectories and agent private/public workspace partitions. | Multiple tool calls for the same company reuse one container; switching projects changes workdir without rebuilding container; public and agent-private paths exist in the container. |
| 13 | Add context observability/debug output | Provide a debug view that shows exactly which manual prompt, automatic prompt, and runtime tail data were injected into a turn and where each came from. | Debug output can prove there is no cross-contamination between the three channels and can identify the source file/event for each injected section. |
| 14 | Add end-to-end production acceptance scenario | Cover owner login, agent-bound inbox, three-state status, daily memory update, runtime blackboard injection, and company Docker reuse in one integrated scenario. | The scenario proves durable agent memory under `codex_home`, no blackboard pollution of long-term prompts, correct owner routing, and company-level Docker reuse. |

## Implementation Notes

- Implement incrementally, but each task should land with production-grade storage semantics, compatibility behavior, tests, and observability hooks where relevant.
- Do not use `blocked` as a program state.
- Do not store durable agent memory in workspace paths.
- Do not auto-edit human-owned manual permanent prompt files.
- Treat existing runtime tail injection as mostly valid; only adjust it where necessary to maintain separation from durable prompts.
