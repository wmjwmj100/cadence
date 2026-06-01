## ADDED Requirements

### Requirement: Every turn injects the latest user-assistant pair
The system SHALL append a normalized representation of the latest user-assistant pair to each model turn context.

#### Scenario: Building context for a new model turn
- **WHEN** the runtime composes input messages for a swarm agent
- **THEN** the final injected context includes one latest user-assistant pair block

### Requirement: Injected assistant block includes summaries and blackboard snapshot
The system SHALL include both prior multi-agent summary content and current shared blackboard snapshot in the injected assistant-side context block.

#### Scenario: Latest pair is synthesized for injection
- **WHEN** the runtime creates the assistant-side content for the injected pair
- **THEN** the content contains summary history and current shared blackboard text for the session

### Requirement: Blackboard snapshot used for injection is consistency-safe
The system SHALL read blackboard content for injection under the same session lock policy used by write operations.

#### Scenario: Injection read races with concurrent write
- **WHEN** one agent is writing and another turn is reading blackboard for injection
- **THEN** the injection logic waits for lock and reads a consistent post-lock file snapshot
