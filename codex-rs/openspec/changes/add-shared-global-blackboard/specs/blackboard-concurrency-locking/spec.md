## ADDED Requirements

### Requirement: Blackboard read/write operations acquire session lock
The system SHALL acquire a session-scoped lock before reading or writing the shared blackboard file to prevent concurrent corruption.

#### Scenario: Concurrent writes from multiple agents
- **WHEN** two agents attempt to write the same session blackboard at the same time
- **THEN** only one write enters the critical section at a time and both writes complete without file corruption

### Requirement: Lock contention failures are explicit
The system SHALL return a deterministic error when lock acquisition exceeds timeout and SHALL preserve existing blackboard content on failure.

#### Scenario: Lock cannot be acquired in time
- **WHEN** an agent command cannot obtain the blackboard lock before configured timeout
- **THEN** the write is aborted with a lock-timeout error and no partial mutation is persisted

### Requirement: Read-modify-write sequences are atomic under lock
The system SHALL execute read-modify-write blackboard updates within a single lock scope so intermediate states are not externally visible.

#### Scenario: Agent edits an existing entry
- **WHEN** a command reads the file, updates targeted content, and writes back
- **THEN** other concurrent readers never observe a partially updated file state
