## Context

Swarm mode currently supports correlated calls through `message_id` and `reply_to_message_id`, and `wait` can observe replies from the shared collaboration inbox. However, there is no runtime enforcement that a `call` marked `need_reply: true` is actually answered before an agent turn ends. As a result, an agent can finish while still owing required replies, which leaves upstream agents blocked without an explicit recovery signal.

This change targets both the primary agent and spawned agents. The enforcement point is the same task lifecycle used by all agents, so behavior remains consistent across the swarm network.

## Goals / Non-Goals

**Goals:**
- Detect, at each agent stop boundary, whether the agent still owes replies for inbound calls that required a reply.
- Treat a reply as valid only when a later outbound `call` includes `reply_to_message_id` referencing the original inbound `message_id`.
- When unresolved obligations exist, inject a reminder `user_input` that includes actionable context (source agent, message id, original message content) so the model can reply.
- Apply the rule uniformly to main and spawned agents in Swarm mode.

**Non-Goals:**
- Changing non-Swarm collaboration behavior.
- Auto-generating reply calls on behalf of the model.
- Reworking `wait` semantics or introducing new user-facing tools.

## Decisions

1. **Track required-reply obligations separately from wait inbox draining**
   - Add a dedicated runtime tracker for required-reply obligations keyed by receiver thread id.
   - Record inbound metadata when `call` dispatches to a target with `need_reply: true`.
   - Keep this state independent from `wait` message draining so `wait` consumption cannot hide unresolved obligations.
   - Stored metadata includes at least: source agent name, source thread id, original `message_id`, original content, and first-seen sequence/time.

2. **Resolve obligations on outbound correlated replies**
   - On every outbound `call`, if `reply_to_message_id` is present, attempt to resolve a matching unresolved inbound obligation for the current sender thread.
   - Prefer strict matching on `(source_thread_id, reply_to_message_id)`; keep source agent name for human-readable reminder text.
   - Unmatched `reply_to_message_id` values remain no-op for this tracker.

3. **Stop-time enforcement hook at task-finish path**
   - Add the stop check to the shared task-finish lifecycle (`Session::on_task_finished` path) so all agent roles use the same enforcement.
   - Run enforcement only when collaboration mode is Swarm.
   - If unresolved obligations exist at stop time, synthesize a `user_input` reminder payload and queue it before final stop so the model receives one more actionable turn.

4. **Reminder payload format is high-context and deterministic**
   - Reminder text includes, per unresolved message: source agent name, original `message_id`, and original call body.
   - Reminder explicitly instructs the model to reply via `call` with a new `message_id` and `reply_to_message_id` set to the unresolved id.
   - If multiple unresolved obligations exist, include all of them in one structured reminder block to reduce turn churn.

5. **Bounded re-reminder policy to avoid runaway loops**
   - Maintain per-obligation reminder-attempt count for the current lifecycle.
   - Allow reminder reinjection at subsequent stop boundaries, but cap repeated reminders (for example, configurable low cap) and emit a warning event when cap is reached.
   - This preserves enforcement intent while preventing infinite self-triggered loops if the model continually ignores reminders.

## Risks / Trade-offs

- **[Risk] False-positive unresolved detection when agent names or routing differ** ¡ú Mitigation: match by source thread id + message id, not by display name alone.
- **[Risk] Extra turns and token cost from reminder injection** ¡ú Mitigation: aggregate unresolved obligations into a single reminder and cap repeated reminders.
- **[Risk] Tracker state growth in long sessions** ¡ú Mitigation: remove obligations immediately on resolution, and apply bounded retention/eviction for stale unresolved entries.
- **[Risk] Behavioral surprise outside Swarm** ¡ú Mitigation: hard gate enforcement by `ModeKind::Swarm`.

## Migration Plan

1. Introduce required-reply tracker types and APIs in collaboration runtime state.
2. Wire call ingestion/resolution hooks in call handling:
   - inbound `need_reply: true` => add unresolved obligation
   - outbound `reply_to_message_id` => resolve obligation
3. Add stop-time enforcement in the shared task-finish path for Swarm mode.
4. Add tests for:
   - unresolved obligation triggers reminder user_input
   - valid correlated reply clears obligation
   - multi-obligation reminder formatting
   - cap behavior preventing infinite reminder loops
   - non-Swarm mode no-op behavior
5. Roll out behind existing Swarm behavior path (no external API migration required).

## Open Questions

- Should reminder caps be hardcoded or configurable in settings for different swarm workloads?
- Should reminders also be emitted as structured events for UI observability in addition to injected `user_input`?
- For legacy/invalid messages that omit `message_id`, should we log-only or surface a soft warning to the model?
