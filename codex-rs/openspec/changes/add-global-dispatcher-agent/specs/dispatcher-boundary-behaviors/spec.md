## ADDED Requirements

### Requirement: Activation uses a stable queue snapshot boundary
When dispatcher activation begins, the runtime SHALL analyze a stable snapshot of queue entries captured at trigger time, and entries written after activation start SHALL NOT be included in that in-flight activation input.

#### Scenario: New summaries arrive during activation
- **WHEN** additional summary entries are appended while dispatcher activation is already running
- **THEN** those new entries are excluded from the current activation snapshot

### Requirement: During-activation writes remain persisted for future analysis
Summary entries written during dispatcher activation SHALL still be appended to the global FIFO queue and SHALL be available to subsequent activation cycles.

#### Scenario: Queue receives writes while dispatcher is active
- **WHEN** a non-dispatcher agent writes summaries during dispatcher execution
- **THEN** those summaries remain in FIFO order and participate in later dispatcher activations

### Requirement: Remind failures for invalid targets are non-fatal
If a reminder attempt fails because a target agent identifier is invalid, dispatcher processing SHALL continue and remaining reminder candidates SHALL still be evaluated.

#### Scenario: One reminder call fails among multiple candidates
- **WHEN** dispatcher reminder attempt for target A fails with unknown `agent_id`
- **THEN** dispatcher continues processing other reminder targets in the same activation

### Requirement: Inactive agents remain out of dispatcher-visible state
Agents that do not invoke tools for an extended period SHALL produce no new summary entries and SHALL therefore remain invisible to dispatcher state analysis until new summaries appear.

#### Scenario: Agent remains idle
- **WHEN** an agent performs no tool calls across multiple activation cycles
- **THEN** dispatcher input contains no new state updates for that agent

