## 1. Backend Wait Semantics

- [x] 1.1 Locate and remove wait-completion heuristics that infer completion from target-agent terminal state.
- [x] 1.2 Update wait-target completion logic to require callback correlation keys (target + reply linkage) before marking completed.
- [x] 1.3 Extend wait result/view-model plumbing to carry callback payload content and `agent_name` metadata with deterministic fallback data.
- [x] 1.4 Add core/runtime tests for no-callback terminal agent, matched callback completion, and unmatched callback non-completion.

## 2. TUI Wait Rendering Alignment

- [x] 2.1 Refactor wait UI state consumption to use backend wait lifecycle state as source of truth.
- [x] 2.2 Replace sender/receiver ID rendering with `agent_name`-first labels plus fallback when names are missing.
- [x] 2.3 Render callback payload as wait completion content and remove reliance on waited agent last-message output.
- [x] 2.4 Normalize wait status presentation so running/pending/completed are mutually consistent for each target.

## 3. End-to-End Behavior Verification

- [x] 3.1 Add integration coverage that wait completes only when a correlated callback reply is received.
- [x] 3.2 Add regression coverage that agent completion without callback keeps wait non-completed.
- [x] 3.3 Add regression coverage that mismatched callback replies do not complete unrelated wait targets.

## 4. UI Snapshot and Quality Checks

- [x] 4.1 Update or add `codex-tui` snapshots covering sender/receiver `agent_name` rendering.
- [x] 4.2 Update or add `codex-tui` snapshots covering callback payload output and corrected running/completed transitions.
- [x] 4.3 Execute targeted crate tests for changed modules and review failures against the new wait contract.
