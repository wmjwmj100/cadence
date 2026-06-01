## ADDED Requirements

### Requirement: Canonical debug history includes all conversation roles
The system SHALL maintain a canonical debug history per conversation that records every relevant interaction with explicit role values: `system`, `developer`, `user`, `assistant`, and `tool`.

#### Scenario: Role-complete turn is recorded
- **WHEN** a turn includes system/developer instructions, user input, assistant output, and tool invocation/output
- **THEN** the canonical debug history contains entries for each interaction with the corresponding role value

### Requirement: Canonical debug history includes all agent participants
The system SHALL include entries from the primary user-facing assistant agent, spawned sub-agents, and the global coordinator/dispatcher agent in the same canonical conversation history.

#### Scenario: Multi-agent collaboration occurs
- **WHEN** a task includes spawned sub-agent work and coordinator/dispatcher actions
- **THEN** the canonical history includes all related entries in one merged conversation timeline

### Requirement: Tool interactions are preserved in full debug history
The system SHALL capture tool invocation context and tool result content in the canonical debug history so that tool-driven execution can be audited end-to-end.

#### Scenario: Tool call executes successfully
- **WHEN** an agent invokes a tool and receives tool output
- **THEN** the canonical history contains entries representing both the tool call and the returned output
