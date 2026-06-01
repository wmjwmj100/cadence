## ADDED Requirements

### Requirement: Wait completion MUST be callback-correlated
The system MUST mark a wait target as completed only after receiving a callback reply that matches the configured correlation keys for that wait target.

#### Scenario: Target agent finishes without callback
- **WHEN** the waited-on agent reaches a terminal lifecycle state but no matching callback reply has been received
- **THEN** the wait target remains non-completed and the wait operation remains active

#### Scenario: Matching callback arrives
- **WHEN** a callback reply is received with correlation fields matching a configured wait target
- **THEN** the system marks that wait target as completed and advances overall wait completion accordingly

### Requirement: Wait UI MUST render callback payload as result content
When a wait target completes via callback correlation, the wait UI MUST display the callback payload content as the completion result instead of the waited agent's last message.

#### Scenario: Callback payload differs from agent last message
- **WHEN** the callback payload content and waited agent's last emitted message content are different
- **THEN** the wait completion panel shows the callback payload content

#### Scenario: Multiple agent messages before callback
- **WHEN** the waited agent emits additional messages before sending the matching callback reply
- **THEN** wait result content remains empty until callback arrival and then switches to callback payload content

### Requirement: Wait sender and receiver labels MUST prefer agent names
Wait UI rows for sender and receiver SHALL display `agent_name` values from wait metadata when available, and SHALL use a deterministic fallback only when `agent_name` is unavailable.

#### Scenario: Agent names provided
- **WHEN** wait metadata includes sender and receiver `agent_name` values
- **THEN** the UI renders those names and does not render opaque internal IDs as primary labels

#### Scenario: Agent name missing
- **WHEN** one side of wait metadata has no `agent_name`
- **THEN** the UI renders the defined fallback label for that side while preserving other available names

### Requirement: Wait status presentation MUST be lifecycle-consistent
Wait UI state rendering MUST map backend wait lifecycle states consistently so that running and completed are mutually exclusive for a target at any instant.

#### Scenario: Running state before callback
- **WHEN** no matching callback has been received and no timeout/failure terminal event has occurred
- **THEN** the target status is rendered as running or pending and not rendered as completed

#### Scenario: Completed after callback
- **WHEN** a matching callback has been processed for the target
- **THEN** the target status is rendered as completed and no concurrent running indicator is shown
