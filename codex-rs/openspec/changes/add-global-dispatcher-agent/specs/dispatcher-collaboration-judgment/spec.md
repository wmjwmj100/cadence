## ADDED Requirements

### Requirement: Dispatcher evaluates full queue content per activation
On each activation, the dispatcher SHALL evaluate the entire FIFO queue snapshot for that activation and SHALL use queue entries as its sole memory context.

#### Scenario: Activation starts with fresh dialog state
- **WHEN** the dispatcher is activated
- **THEN** its decision input contains only the current queue snapshot and not prior persistent chat history

### Requirement: Collaboration reminders follow explicit trigger criteria
The dispatcher SHALL issue a reminder when at least one of the following is detected in the queue snapshot: task dependency, repeated failure loop, concurrent work on the same object, or complementary missing context.

#### Scenario: Dependency is detected between two agents
- **WHEN** queue entries show agent A has required output and agent B is blocked waiting for that output
- **THEN** the dispatcher issues a reminder to coordinate the handoff

### Requirement: Dispatcher deduplicates unanswered reminders
If the dispatcher has already reminded a target agent and that target has produced no newer summary entry afterward, the dispatcher SHALL NOT send another reminder to the same target in the current activation.

#### Scenario: Target has not responded since prior reminder
- **WHEN** the queue contains a previous dispatcher reminder to agent X and no later summary from agent X
- **THEN** the dispatcher skips sending a duplicate reminder to agent X

### Requirement: Dispatcher remains silent when no collaboration is needed
If no collaboration condition is met, the dispatcher SHALL end the activation without calling reminder tools and without emitting additional output.

#### Scenario: Snapshot has no actionable coordination gap
- **WHEN** queue analysis finds no dependency, loop, overlap, or complementary-context condition
- **THEN** the dispatcher performs no reminder calls and exits the activation

