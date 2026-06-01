## ADDED Requirements

### Requirement: Track required-reply inbound calls in Swarm mode
The system SHALL record an unresolved reply obligation when an agent in Swarm mode receives a `call` that includes a non-empty `message_id` and `need_reply: true`.

#### Scenario: Required-reply call creates an unresolved obligation
- **WHEN** agent `B` receives a `call` from agent `A` with `message_id` set and `need_reply: true`
- **THEN** the runtime records an unresolved obligation for agent `B` keyed to that inbound `message_id` and source agent context

### Requirement: Correlated outbound replies resolve obligations
The system SHALL mark a required-reply obligation as resolved only when the receiving agent later sends an outbound `call` that includes `reply_to_message_id` equal to the inbound required-reply `message_id`.

#### Scenario: Matching reply_to_message_id resolves the obligation
- **WHEN** agent `B` sends an outbound `call` with `reply_to_message_id` matching a previously recorded unresolved inbound required-reply `message_id`
- **THEN** the matching unresolved obligation is marked resolved and SHALL NOT trigger stop-time reminder injection

### Requirement: Stop-time unresolved check injects reminder input
The system SHALL evaluate unresolved required-reply obligations each time an agent run stops, and SHALL inject a reminder `user_input` when unresolved obligations exist.

#### Scenario: Agent stops with unresolved required-reply obligations
- **WHEN** an agent run reaches stop and at least one required-reply obligation remains unresolved
- **THEN** the runtime injects reminder `user_input` before final stop processing for that agent

### Requirement: Reminder input includes required reply context
For each unresolved required-reply obligation, the reminder `user_input` SHALL include the original message text, source agent name, and original message id so the model can issue a correlated reply.

#### Scenario: Reminder includes original call details
- **WHEN** the runtime generates a stop-time reminder for unresolved required-reply obligations
- **THEN** the reminder content includes message source agent name, unresolved `message_id`, and the original inbound call body for each unresolved obligation

### Requirement: Enforcement applies to main and spawned agents
The stop-time unresolved-check behavior SHALL apply uniformly to the primary Swarm agent and every spawned Swarm agent.

#### Scenario: Spawned agent receives required-reply reminder
- **WHEN** a spawned agent stops with unresolved required-reply obligations
- **THEN** the runtime applies the same reminder-injection behavior used for the primary Swarm agent
