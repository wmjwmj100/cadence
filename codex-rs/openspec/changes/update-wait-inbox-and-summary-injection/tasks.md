## 1. Wait Interface Contract Update

- [x] 1.1 Locate wait tool/input schema and remove required `target_agent` / `message` fields.
- [x] 1.2 Update wait argument parsing and validation paths to accept parameterless wait calls.
- [x] 1.3 Adjust any user-facing wait help text or tool metadata to describe the new inbox-driven semantics.

## 2. Wait Runtime Behavior Migration

- [x] 2.1 Refactor wait completion logic to return once inbox has any consumable message.
- [x] 2.2 Remove or bypass legacy target/message matching branches in wait state transitions.
- [x] 2.3 Preserve established inbox consumption order when multiple messages are queued.
- [x] 2.4 Update wait result payload mapping so returned content comes from the consumed inbox message.

## 3. Global Summary Injection Window Expansion

- [x] 3.1 Find the per-agent summary injection cap and change it from 2 to 30.
- [x] 3.2 Ensure context builder selects the most recent 30 summaries when history exceeds the cap.
- [x] 3.3 Keep injected summary rendering in chronological order (old -> new) within the selected window.

## 4. Regression Tests and Verification

- [x] 4.1 Add/adjust tests that wait without `target_agent` / `message` is accepted and remains pending only when inbox is empty.
- [x] 4.2 Add/adjust tests that any new inbox message completes wait immediately, including messages that would fail old matching rules.
- [x] 4.3 Add/adjust tests for multi-message inbox ordering to confirm deterministic first-return behavior.
- [x] 4.4 Add/adjust tests that each agent injects up to 30 summaries, keeps most recent entries, and preserves chronological output order.
- [x] 4.5 Run targeted crate tests for changed wait/collab modules and summarize any expected snapshot/test updates.
