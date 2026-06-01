## ADDED Requirements

### Requirement: Agent context SHALL include up to thirty historical summaries
When constructing collaboration context for any agent, the system MUST inject historical summary entries with a hard upper limit of 30 entries per agent context build.

#### Scenario: More than thirty summaries exist
- **WHEN** at least 31 historical summaries are available for injection
- **THEN** exactly 30 summaries are injected into that agent's context

#### Scenario: Fewer than thirty summaries exist
- **WHEN** fewer than 30 historical summaries are available
- **THEN** all available summaries are injected

### Requirement: Summary selection SHALL prefer most recent history
If the available summary count exceeds 30, the injected set MUST be the 30 most recent summaries.

#### Scenario: Historical window truncation
- **WHEN** a long summary history exceeds the injection cap
- **THEN** older summaries outside the latest 30 are excluded from injection

### Requirement: Injected summaries SHALL preserve chronological readability
The injected summary block MUST preserve chronological order from older to newer within the selected window.

#### Scenario: Ordered prompt construction
- **WHEN** the latest 30 summaries are selected for injection
- **THEN** they are rendered in time order so downstream agents can follow causal sequence
