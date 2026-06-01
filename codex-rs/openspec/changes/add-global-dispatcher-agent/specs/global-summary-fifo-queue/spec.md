## ADDED Requirements

### Requirement: Tool calls append global summary entries
The system SHALL append exactly one entry to a global summary FIFO queue whenever any agent invokes a tool, including the dispatcher agent.

#### Scenario: Any agent tool call records one entry
- **WHEN** an agent invokes a tool and provides a valid summary
- **THEN** the runtime appends one queue entry containing that agent's identifier and summary content

### Requirement: Summary entries carry dispatcher-readable timestamps
The system SHALL include a write-time timestamp for each summary entry and SHALL render entries in dispatcher input as `{agent_id}: {summary_content} [HH:MM:SS]`.

#### Scenario: Queue is serialized for dispatcher input
- **WHEN** the dispatcher is activated and receives queue content
- **THEN** every entry includes a timestamp suffix formatted as `[HH:MM:SS]`

### Requirement: Global queue uses strict bounded FIFO eviction
The global summary queue SHALL keep at most 40 entries and SHALL evict the oldest entry before appending a new one when the queue is full.

#### Scenario: Queue overflows at capacity
- **WHEN** the queue already contains 40 entries and a new summary is appended
- **THEN** the oldest existing entry is removed and the new entry is appended at the tail

