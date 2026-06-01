## ADDED Requirements

### Requirement: Debug UI identifies agents by display name
The debug collaboration UI SHALL display agent entries by human-readable agent name rather than raw thread/agent IDs.

#### Scenario: UI renders multi-agent timeline
- **WHEN** the trace contains entries for one or more agents
- **THEN** each entry shows the agent display name and does not require raw ID display for primary identification

### Requirement: Debug UI uses lane-based collaboration layout
The debug collaboration UI SHALL render agent timelines in parallel lanes, with a deterministic placement rule that supports one, two, three, and more than three agents.

#### Scenario: Exactly three agents are present
- **WHEN** the UI renders a trace with three agents
- **THEN** the layout places agent lanes left, center, and right with stable lane assignment during the session

### Requirement: Timeline entries expose trace context for auditing
Each rendered timeline entry SHALL show timestamp, role, agent name, and message/tool content so users can inspect collaboration flow end-to-end.

#### Scenario: User inspects a specific timeline row
- **WHEN** a timeline row is opened in the debug view
- **THEN** the row exposes timestamp, role, agent name, and content fields for that event
