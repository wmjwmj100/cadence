## ADDED Requirements

### Requirement: Session startup provisions shared blackboard storage
The system SHALL ensure a `.blackboard` directory exists under the workspace root and SHALL create a session-specific shared blackboard file when a session starts.

#### Scenario: Missing storage is auto-created
- **WHEN** a new session starts and `.blackboard` or the session file does not exist
- **THEN** the runtime creates the missing directory and file before any agent turn executes

### Requirement: Shared blackboard files are isolated per session
The system SHALL map each session to exactly one blackboard file and SHALL keep writes from one session invisible to other sessions.

#### Scenario: Two sessions write independently
- **WHEN** session A and session B append entries to their shared blackboards
- **THEN** each session reads only its own file content and no cross-session lines appear

### Requirement: Session blackboard path is retained in runtime context
The system SHALL store the resolved session blackboard file path in session runtime state for prompt rendering and injection workflows.

#### Scenario: Prompt builder requests current blackboard path
- **WHEN** a swarm turn builds system prompt content
- **THEN** the prompt builder receives the same session file path that was created at session startup
