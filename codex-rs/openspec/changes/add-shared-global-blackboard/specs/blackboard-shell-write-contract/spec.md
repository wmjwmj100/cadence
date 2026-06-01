## ADDED Requirements

### Requirement: Blackboard mutations are shell-mediated
The system SHALL require blackboard write and update actions to execute through agent shell commands and SHALL NOT introduce a separate blackboard-specific mutation tool.

#### Scenario: Agent updates blackboard content
- **WHEN** an agent needs to append or modify shared blackboard data
- **THEN** the agent performs the operation via shell command execution against the session blackboard file

### Requirement: Blackboard entry format is standardized
The system SHALL enforce shared blackboard entries to use the exact format `[agent_name]：message_content`.

#### Scenario: Agent appends a new blackboard entry
- **WHEN** a shell append command writes one logical entry
- **THEN** the resulting line starts with `[agent_name]：` followed by non-empty message content

### Requirement: Agent identity is available for entry prefixes
The system SHALL provide each agent's own name in runtime prompt context so the agent can produce the required blackboard prefix consistently.

#### Scenario: Agent constructs a blackboard write command
- **WHEN** the agent prepares a shell write for the shared blackboard
- **THEN** the command uses the runtime-injected agent name value in the `[agent_name]` prefix
