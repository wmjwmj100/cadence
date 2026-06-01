## ADDED Requirements

### Requirement: Each conversation stores logs in a dedicated folder
The system SHALL persist debug trace artifacts in a dedicated filesystem folder per conversation/task.

#### Scenario: New conversation begins with debug logging enabled
- **WHEN** the first debug entry for a conversation is recorded
- **THEN** the runtime creates or reuses a conversation-specific log folder for that conversation only

### Requirement: Canonical history snapshot is overwritten on each update
The system SHALL rewrite the canonical history snapshot file on every update so the latest file content fully replaces the previous snapshot.

#### Scenario: Conversation receives a new debug entry
- **WHEN** a new entry is added to canonical history
- **THEN** the canonical snapshot file is replaced with a complete latest-history snapshot that includes all entries to date

### Requirement: Snapshot replacement is atomic
The system MUST perform snapshot replacement using an atomic file replace strategy to prevent partial-history files.

#### Scenario: Process interruption occurs during write
- **WHEN** the runtime is interrupted while updating canonical history
- **THEN** readers observe either the previous complete snapshot or the new complete snapshot, but never a partially written file
