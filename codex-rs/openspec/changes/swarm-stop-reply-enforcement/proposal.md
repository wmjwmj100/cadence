## Why

In Swarm mode, an agent can receive a call that explicitly requires a reply, then stop without replying. This silently drops coordination dependencies and can leave other agents blocked with no clear recovery signal.

## What Changes

- Add required-reply tracking for every agent turn (main agent and spawned agents), including incoming calls that carry `message_id` and require-reply intent.
- Add stop-time validation that checks whether each required-reply inbound call has a matching outbound reply referencing the original message id.
- When unresolved required-reply calls exist at stop time, inject a reminder `user_input` entry to the stopping agent before final termination.
- Include high-context reminder details: original call content, source agent name, original message id, and reply expectation.
- Ensure reminders are generated for each unresolved required-reply call so agents can close pending obligations explicitly.

## Capabilities

### New Capabilities
- `swarm-required-reply-enforcement`: Enforce and surface unresolved required-reply call obligations when an agent stops in Swarm mode.

### Modified Capabilities
- None.

## Impact

- Affected systems: Swarm runtime turn lifecycle, call/message tracking, and stop/finalization flow.
- Affected behavior: Agent shutdown now performs required-reply completeness checks and may emit reminder `user_input` entries.
- Observability: Reminder payloads provide actionable context (source agent, message id, original message text) to reduce coordination deadlocks.
- APIs/dependencies: No external API or dependency change expected; this is runtime behavior within existing Swarm orchestration.
