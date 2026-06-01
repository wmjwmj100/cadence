## 1. Required-reply tracker foundation

- [x] 1.1 Add a Swarm required-reply obligation tracker data model keyed by receiver thread and inbound `message_id`.
- [x] 1.2 Add tracker APIs to register inbound required-reply calls with source agent/thread metadata and original content.
- [x] 1.3 Add tracker APIs to resolve obligations from outbound `reply_to_message_id` references.

## 2. Call-path integration

- [x] 2.1 Update call handling to record an unresolved obligation when dispatching a `need_reply: true` call in Swarm mode.
- [x] 2.2 Update call handling to resolve matching obligations when a call includes `reply_to_message_id`.
- [x] 2.3 Ensure non-Swarm flows remain unchanged and unmatched replies are treated as no-op for enforcement state.

## 3. Stop-time enforcement and reminder injection

- [x] 3.1 Add a stop-time check in the shared task-finish lifecycle to detect unresolved required-reply obligations.
- [x] 3.2 Inject reminder `user_input` before final stop when unresolved obligations exist, including source agent name, original `message_id`, and original message text.
- [x] 3.3 Apply enforcement uniformly to primary and spawned agents, and add bounded re-reminder behavior to avoid infinite loops.

## 4. Verification and regressions

- [x] 4.1 Add unit/integration tests that verify required-reply obligations are created and cleared by correlated replies.
- [x] 4.2 Add tests that verify stop-time reminder injection for unresolved obligations (single and multiple messages).
- [x] 4.3 Add tests that verify non-Swarm mode does not enforce reminder injection and reminder-cap behavior is bounded.
