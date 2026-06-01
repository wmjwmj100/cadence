## ADDED Requirements

### Requirement: Remind tool validates required parameters
The `remind` tool SHALL require non-empty `agent_id`, `message`, and `reason` parameters for each call.

#### Scenario: Required parameter is missing or empty
- **WHEN** the dispatcher calls `remind` without a valid non-empty `message` or `reason`
- **THEN** the tool call is rejected with a validation error

### Requirement: Remind tool rejects unknown targets
The `remind` tool SHALL return an error when `agent_id` does not match a known target agent identifier in the current system.

#### Scenario: Unknown target identifier is submitted
- **WHEN** `remind` is called with an `agent_id` that does not exist
- **THEN** the tool returns an error and no inbox message is created

### Requirement: Successful reminders are delivered via inbox for next trigger
On successful `remind`, the system SHALL enqueue reminder content into the target agent inbox and SHALL deliver it when that target agent is naturally triggered next, without interrupting any currently running turn.

#### Scenario: Target is currently running when reminder is sent
- **WHEN** the dispatcher successfully calls `remind` for a running target agent
- **THEN** the reminder is queued in inbox and consumed on the target's next natural turn trigger

### Requirement: Successful remind calls emit dispatcher summary entries
After each successful `remind` call, the dispatcher SHALL append one summary entry to the global FIFO queue describing the reminder action.

#### Scenario: Dispatcher sends one successful reminder
- **WHEN** a `remind` call completes successfully
- **THEN** one dispatcher-authored summary entry is appended to the FIFO queue

