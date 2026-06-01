## 1. Session Blackboard Provisioning

- [x] 1.1 Add session-start initialization to create `<workspace>/.blackboard/` when missing.
- [x] 1.2 Add session file mapping and auto-create `<workspace>/.blackboard/<session_id>.md` at session bootstrap.
- [x] 1.3 Persist the resolved session blackboard path in runtime session state for downstream prompt/injection usage.

## 2. Swarm Prompt Blackboard Rules

- [x] 2.1 Extend swarm system prompt builder to inject current agent name and current session blackboard path automatically.
- [x] 2.2 Add explicit prompt rules that require shell-based blackboard operations with lock protection.
- [x] 2.3 Add explicit prompt rule enforcing blackboard entry format `[agent_name]：message_content`.

## 3. Shell Write Contract and Locking

- [x] 3.1 Implement a reusable session-scoped lock helper for blackboard reads/writes (for example lock-file + timeout policy).
- [x] 3.2 Ensure blackboard read-modify-write operations execute within one lock critical section.
- [x] 3.3 Return deterministic lock-timeout errors without partial file mutations.

## 4. Injection Pipeline Updates

- [x] 4.1 Extend turn-context assembly to always append a normalized latest user-assistant pair block.
- [x] 4.2 Build injected assistant-side block from both multi-agent summary history and current blackboard snapshot.
- [x] 4.3 Read blackboard snapshot under session lock and apply length control to keep prompt size bounded.

## 5. Verification and Debug Diagnostics

- [x] 5.1 Add tests for blackboard storage provisioning and per-session file isolation.
- [x] 5.2 Add tests for entry format compliance and prompt injection of agent name/path/rules.
- [x] 5.3 Add concurrent read/write tests verifying lock serialization and timeout behavior.
- [x] 5.4 Add integration tests validating latest user-assistant pair injection includes summaries and blackboard content.
- [x] 5.5 Add debug-driven checks using `/media/wmj/BC0739C74EA78EEA/debug` artifacts to trace blackboard and injection outputs.
