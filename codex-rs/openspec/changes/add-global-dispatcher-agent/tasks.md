## 1. Dispatcher Runtime Foundation

- [x] 1.1 Add a dedicated dispatcher runtime state module for global FIFO summaries, activation counters, and in-flight activation flags.
- [x] 1.2 Define summary entry data model with agent identifier, summary text, sequence, and timestamp.
- [x] 1.3 Add formatter logic to render dispatcher input lines as `{agent_id}: {summary_content} [HH:MM:SS]`.

## 2. Global Summary Queue Integration

- [x] 2.1 Extend the shared tool summary capture path to append every tool-call summary into the dispatcher FIFO queue.
- [x] 2.2 Enforce strict FIFO capacity of 40 entries with oldest-entry eviction on overflow.
- [x] 2.3 Count only non-dispatcher summary writes toward activation triggers.

## 3. Activation Cycle Orchestration

- [x] 3.1 Trigger dispatcher activation when non-dispatcher summary count reaches 10 and reset the counter for the next cycle.
- [x] 3.2 Capture a stable queue snapshot for each activation and ensure writes during activation are deferred to the next cycle.
- [x] 3.3 Implement single-activation-in-flight coordination to avoid overlapping dispatcher runs.

## 4. Stateless Dispatcher Execution

- [x] 4.1 Implement a one-shot dispatcher execution path that receives only the queue snapshot as activation input.
- [x] 4.2 Ensure dispatcher runs without persistent conversation history between activations.
- [x] 4.3 Inject the dispatcher system prompt and collaboration criteria into the one-shot execution path.

## 5. Remind Tool and Inbox Delivery

- [x] 5.1 Add `remind` tool schema to the tool surface with required `agent_id`, `message`, and `reason` parameters.
- [x] 5.2 Implement `remind` handler validation for non-empty required fields and known target agent checks.
- [x] 5.3 Reuse inbox delivery to enqueue successful reminders for target agents' next natural trigger without interrupting current runs.
- [x] 5.4 Append one dispatcher-authored summary entry after each successful `remind` call and keep it excluded from activation counting.

## 6. Dedupe Rules and Edge-Case Handling

- [x] 6.1 Implement reminder dedupe guard to suppress repeated reminders when the target agent has no newer summary since the previous reminder.
- [x] 6.2 Keep processing remaining reminder candidates when one `remind` call fails (for example unknown target agent).
- [x] 6.3 Preserve compatibility with existing per-agent workboard/status UI while adding dispatcher queue behavior.

## 7. Verification and Observability

- [x] 7.1 Add unit tests for FIFO write rules, timestamp formatting, and 40-entry eviction.
- [x] 7.2 Add unit tests for activation counting, dispatcher-excluded summaries, and trigger-at-10 behavior.
- [x] 7.3 Add tests for stable activation snapshots when new summaries arrive during dispatcher execution.
- [x] 7.4 Add tests for `remind` validation failures, unknown target handling, inbox delivery semantics, and dispatcher summary emission.
- [x] 7.5 Add metrics/logging assertions for activation count, reminder attempts, reminder failures, and dedupe suppressions.
