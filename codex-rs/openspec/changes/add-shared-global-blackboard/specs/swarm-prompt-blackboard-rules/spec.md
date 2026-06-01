## ADDED Requirements

### Requirement: Swarm system prompt includes blackboard governance rules
The system SHALL embed explicit shared blackboard instructions in swarm system prompt content, including allowed write method, lock usage, and required entry format.

#### Scenario: Swarm prompt is rendered for an agent turn
- **WHEN** system prompt text is generated
- **THEN** the prompt contains blackboard rules that require shell-based operations with lock protection and `[agent_name]：message_content` format

### Requirement: Prompt includes current session blackboard path
The system SHALL inject the current session's shared blackboard file path into swarm system prompt content automatically.

#### Scenario: New session starts
- **WHEN** first swarm turn is prepared for the session
- **THEN** the system prompt includes the resolved absolute or workspace-relative path of that session's blackboard file

### Requirement: Prompt exposes agent self-identity
The system SHALL inject each agent's own name into system prompt content so blackboard writes can use correct prefixes.

#### Scenario: Agent receives prompt context
- **WHEN** a named agent enters a turn
- **THEN** the prompt explicitly states that agent's name for use in blackboard entry prefixes
