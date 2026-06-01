## ADDED Requirements

### Requirement: Activation counter excludes dispatcher summaries
The runtime SHALL maintain a non-dispatcher summary counter since the last dispatcher activation, and dispatcher-authored summary entries SHALL NOT increment this counter.

#### Scenario: Dispatcher writes a summary after remind
- **WHEN** the dispatcher appends its own summary entry to the global queue
- **THEN** the non-dispatcher activation counter value remains unchanged

### Requirement: Dispatcher activates every 10 non-dispatcher summaries
The runtime SHALL trigger one dispatcher activation each time the non-dispatcher summary counter reaches exactly 10.

#### Scenario: First activation after startup
- **WHEN** the system has accepted 10 non-dispatcher summary entries since startup
- **THEN** the dispatcher is activated once

### Requirement: Activation consumes full queue snapshot and resets count
At trigger time, the dispatcher activation input SHALL be a snapshot of all current FIFO entries (up to 40), and the non-dispatcher counter SHALL reset to 0 for the next cycle.

#### Scenario: Trigger boundary creates next counting window
- **WHEN** an activation is triggered at count 10
- **THEN** the dispatcher receives the full queue snapshot and subsequent non-dispatcher summaries are counted in a new cycle starting from 0

