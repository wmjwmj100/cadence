## ADDED Requirements

### Requirement: Shared blackboard behavior is covered by automated tests
The system SHALL include automated tests for blackboard entry format, per-session isolation, and injection content completeness.

#### Scenario: Validation suite executes
- **WHEN** the blackboard test suite runs
- **THEN** tests assert that persisted entries follow `[agent_name]：message_content` and session A/B data remain isolated

### Requirement: Concurrency behavior is validated by lock-focused tests
The system SHALL include tests that simulate concurrent blackboard reads and writes to verify lock serialization and timeout handling.

#### Scenario: Concurrent mutation test runs
- **WHEN** two or more workers operate on the same session blackboard in parallel
- **THEN** results confirm serialized access without corruption and deterministic timeout errors where expected

### Requirement: Runtime diagnostics integrate debug artifact verification
The system SHALL support verification against runtime debug outputs under `/media/wmj/BC0739C74EA78EEA/debug` to confirm injected context and blackboard snapshots match expected behavior.

#### Scenario: Debug-assisted validation is performed
- **WHEN** a test or reproduction run emits debug files
- **THEN** diagnostics can trace blackboard content and injected user-assistant pair payloads back to expected test assertions
