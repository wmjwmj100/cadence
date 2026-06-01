## ADDED Requirements

### Requirement: Wait call SHALL not require target or message filters
The wait interface MUST allow invocation without `target_agent` and `message` filter fields, and MUST treat wait as a generic inbox-availability wait.

#### Scenario: Wait invoked without target fields
- **WHEN** an agent invokes wait without providing `target_agent` and `message`
- **THEN** the call is accepted and enters waiting state based only on inbox availability

### Requirement: Wait SHALL complete when inbox has any new message
The system MUST complete a pending wait as soon as the waiting agent inbox contains at least one newly available message.

#### Scenario: Non-matching message arrives first
- **WHEN** a new inbox message arrives that would not match the old target/message filter behavior
- **THEN** wait completes immediately and returns that message instead of continuing to block

#### Scenario: Inbox remains empty
- **WHEN** no new inbox message is available
- **THEN** wait remains pending and does not complete

### Requirement: Wait return order SHALL follow inbox consumption order
When multiple messages are queued for the waiting agent, wait MUST return messages according to the inbox's established consumption order.

#### Scenario: Multiple queued messages
- **WHEN** two or more inbox messages are already queued when wait checks availability
- **THEN** wait returns the earliest consumable message according to the inbox order policy
