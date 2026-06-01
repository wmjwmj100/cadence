## Context

Swarm mode already requires every function/MCP tool call to include a `summary` argument, and the runtime records that summary before tool execution (`core/src/tools/registry.rs`, `capture_work_summary_and_strip_payload`).  
Today, summaries are stored as a per-agent mini workboard (2 latest entries per thread) in `core/src/agent/guards.rs`, then injected as "Other agents status" context in `core/src/codex.rs`.

The requested change needs a different behavior layer:

- A single global FIFO queue (max 40), with strict chronological ordering across all agents.
- Dispatcher activation every 10 non-dispatcher summaries since last activation.
- Stateless dispatcher turns (no persistent conversation history), with implicit memory only via queue entries.
- A dedicated `remind` tool that writes to target inbox for next natural trigger.
- Explicit duplicate-reminder suppression using dispatcher's own prior reminders.

This is a cross-cutting change across tool dispatch, runtime state, collab inbox delivery, and Swarm prompt/tool surface.

## Goals / Non-Goals

**Goals:**

- Add a global FIFO summary queue with exact write semantics and eviction (`capacity=40`).
- Trigger dispatcher activation every 10 non-dispatcher summary writes.
- Ensure dispatcher receives the full queue snapshot as activation input.
- Add `remind` tool contract (`agent_id`, `message`, `reason`) with validation and inbox delivery.
- Keep dispatcher activations stateless while preserving implicit memory via queue entries.
- Preserve existing Swarm behavior (`call`/`wait`, current progress UI) unless explicitly changed by this feature.

**Non-Goals:**

- Replacing existing per-agent workboard UI behavior.
- Introducing immediate interrupt delivery for reminders (delivery remains next turn via inbox).
- Solving long-term persistent storage for dispatcher queue across process restarts.
- Redesigning generic collab protocol beyond what is required for dispatcher/remind.

## Decisions

### 1. Add a dedicated dispatcher runtime state instead of reusing per-agent workboard storage

Create a new in-memory runtime module (for example `core/src/swarm/dispatcher_runtime.rs`) owned by session/runtime state:

- `fifo: VecDeque<SummaryEntry>` (strict cap 40)
- `non_dispatcher_since_last_activation: u8`
- `activation_in_flight: bool`
- `dispatcher_agent_id: "dispatcher"` (reserved logical id)
- Optional dedupe metadata:
  - `last_reminder_seq_by_target: HashMap<String, u64>`

`SummaryEntry` shape (runtime):

- `seq` (monotonic)
- `agent_id`
- `summary`
- `recorded_at` (UTC epoch seconds)
- `line_cache` or computed text format `{agent_id}: {summary} [HH:MM:SS]`

Why:

- Existing workboard is per-thread, only keeps 2 entries, and is sorted by agent name, not global chronological order.
- We need independent lifecycle, trigger counters, and dedupe metadata for dispatcher logic.

Alternatives considered:

- Reuse `Guards.work_entries_by_thread_id`: rejected (wrong retention model and ordering semantics).
- Persist queue in rollout history: rejected for first iteration (higher complexity, not required by spec).

### 2. Hook queue writes at the existing summary capture point

Extend `capture_work_summary_and_strip_payload` (`core/src/tools/registry.rs`) to also append into dispatcher FIFO after summary validation and agent name resolution.

Behavior:

- Every tool call summary is appended to FIFO (including dispatcher).
- Timestamp format for dispatcher input lines is rendered as `[HH:MM:SS]`.
- Queue overflow evicts oldest entry first.
- Counter increments only when `agent_id != "dispatcher"`.

Why:

- This is the single enforced path where summaries are guaranteed and already normalized.
- Avoids duplicate capture logic in each tool handler.

Alternatives considered:

- Capture via event subscribers (`AgentWorkSummaryEvent`): rejected (more indirection, weaker guarantee ordering with tool dispatch path).

### 3. Activation scheduling uses snapshot boundaries so concurrent writes are deterministic

When `non_dispatcher_since_last_activation` reaches 10:

- Capture activation snapshot sequence boundary and queue snapshot (up to 40 current entries).
- Reset counter for next cycle.
- Mark activation in-flight and schedule async dispatcher run.

Concurrent writes during activation:

- Continue appending to FIFO.
- They do not alter the in-progress snapshot.
- They count toward the next activation cycle.

Why:

- Matches required boundary rule: writes during activation are visible in queue but belong to next count window.

Alternatives considered:

- Blocking summary writes during activation: rejected (would stall normal tool flow).
- Recompute snapshot continuously during activation: rejected (non-deterministic input).

### 4. Dispatcher executes as stateless one-shot runs with queue-only memory

Dispatcher run model:

- Each activation is a fresh execution with:
  - fixed system prompt (the dispatcher prompt in proposal),
  - one user input payload containing FIFO lines.
- No prior dispatcher conversation items are loaded.
- Implicit memory is preserved because previous dispatcher reminders remain in FIFO lines.

Implementation approach:

- Add a dedicated dispatcher runner path (internal, one-shot) rather than a normal long-lived conversational agent thread.

Why:

- Directly satisfies "no persistent conversation history".
- Avoids coupling to spawned-agent lifetime/history and name reuse constraints.

Alternatives considered:

- Persistent `dispatcher` thread: rejected (history accumulation violates requirements).
- Spawn/close a normal sub-agent per activation with reused name: rejected (name reuse and session-history coupling).

### 5. Add a dedicated `remind` tool that reuses collab inbox delivery

Tool definition:

- Name: `remind`
- Inputs: `agent_id`, `message`, `reason` (all required, non-empty strings)
- Semantics:
  - Validate `agent_id` exists and exactly matches known agent id/name from queue domain.
  - Enqueue reminder to target inbox via `collab_inbox::append_message(...)`.
  - Do not interrupt running turns; reminder is consumed on next natural trigger.

Error handling:

- Unknown `agent_id` -> tool returns error; dispatcher continues processing.
- Empty `message` or `reason` -> reject call with validation error.

Dispatcher summary rule:

- On successful `remind`, append dispatcher summary entry into FIFO describing the reminder action.
- Dispatcher-authored summary does not increment non-dispatcher trigger counter.

Alternatives considered:

- Reuse generic `call` instead of `remind`: rejected (extra fields/semantics, weaker domain-specific validation, and less explicit intent).

### 6. Enforce duplicate-reminder suppression with prompt + server-side guard

Primary dedupe remains in dispatcher prompt logic:

- If dispatcher already reminded an agent and that agent has no new summary after reminder, do not remind again.

Add server-side defensive guard:

- Track target's latest summary seq at reminder time.
- Reject/no-op duplicate reminders for the same target if no newer target summary exists.

Why:

- Prompt-only dedupe can drift with model variance.
- Lightweight guard guarantees hard suppression policy.

Alternatives considered:

- Prompt-only dedupe: rejected as non-deterministic.
- Fully rule-based dispatcher (no model): rejected because requirement explicitly defines dispatcher prompt-driven judgment.

## Risks / Trade-offs

- [Risk] Dispatcher adds extra model turns and latency under high tool-call volume. -> Mitigation: gate behind feature flag and run async fire-and-forget; add max concurrent dispatcher activation = 1 per session.
- [Risk] Over-reminding can create inbox noise. -> Mitigation: combine prompt dedupe + server-side dedupe guard + cooldown metrics.
- [Risk] Queue-only memory may lose long-range context due to 40-entry cap. -> Mitigation: keep cap strict per spec, but include dispatcher summaries to preserve essential coordination breadcrumbs.
- [Risk] Encoding or locale mismatches can corrupt summary text. -> Mitigation: normalize summary strings to UTF-8 before queue write; keep display timestamp formatting deterministic.
- [Risk] Behavioral regressions in existing Swarm status UI. -> Mitigation: keep existing per-agent workboard unchanged for UI; dispatcher queue is additional state, not a replacement.

## Migration Plan

1. Implement dispatcher runtime queue + counter, but keep dispatcher activation disabled behind `swarm_dispatcher_enabled` feature flag.
2. Wire summary capture path to write FIFO entries and enforce 40-item eviction.
3. Add `remind` tool spec and handler; add validation + inbox injection + unit tests.
4. Implement dispatcher activation runner (stateless one-shot execution with full FIFO snapshot input).
5. Enable feature in Swarm mode for internal testing, collect metrics:
   - activations/session
   - reminders/activation
   - duplicate-reminder suppressions
   - invalid-agent remind errors
6. Gradually enable by default after stabilization.

Rollback:

- Disable `swarm_dispatcher_enabled` to stop activations and remind emissions immediately.
- Keep queue writes harmlessly enabled or disable in same flag if needed.

## Open Questions

- Should dispatcher be active only in `ModeKind::Swarm`, or in all multi-agent capable modes?
- Should the user-visible `AgentWorkSummaryEvent` schema include timestamp and dispatcher fields, or remain unchanged?
- Should `agent_id` in `remind` accept thread id aliases, or require exact agent name only (strict interpretation says exact id from list)?
- Do we need a hard per-activation cap on number of `remind` calls to prevent model runaway?
