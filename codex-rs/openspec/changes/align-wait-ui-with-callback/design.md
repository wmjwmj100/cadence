## Context

The current wait UX in the TUI diverges from backend behavior in two key places: completion is inferred from whether a target agent appears finished, and displayed output is taken from the target agent's last message. The backend wait contract is callback-driven: wait completes when a correlated reply arrives, and the reply payload is the meaningful result. This mismatch produces false "completed" states and wrong result content.

This change touches cross-cutting logic across wait event production (core/runtime) and wait presentation (tui), so a design pass is needed before implementation.

## Goals / Non-Goals

**Goals:**
- Make wait completion semantics callback-first and correlation-based end to end.
- Show human-readable `agent_name` values in wait sender/receiver UI fields.
- Render callback payload content as the wait result.
- Ensure running/completed state transitions in UI are consistent with backend wait lifecycle.
- Keep existing non-wait multi-agent flows behaviorally unchanged.

**Non-Goals:**
- Redesign the entire multi-agent timeline UI outside wait surfaces.
- Introduce a new transport or protocol for agent messaging.
- Change unrelated agent state labels outside wait-specific rendering.

## Decisions

1. Backend wait status remains the source of truth; UI stops inferring completion from target agent terminal state.
   - Decision: use explicit wait-target completion signals keyed by callback correlation (target agent + awaited message id / reply-to linkage) rather than "agent finished" heuristics.
   - Rationale: avoids false positives when target agent ends without sending a callback.
   - Alternative considered: keep heuristic completion plus callback confirmation. Rejected because dual criteria still allows drift and confusing intermediate states.

2. Wait result payload is derived from the callback message body, not the target agent's last visible message.
   - Decision: plumb callback content through the wait result model and render that content in wait completion output.
   - Rationale: callback is the contractually relevant response for wait.
   - Alternative considered: show both callback and last message. Rejected for noise and ambiguity; callback should be canonical.

3. UI display names use `agent_name` when available, with deterministic fallback only for missing names.
   - Decision: map sender/receiver labels from `agent_name` fields provided by backend metadata.
   - Rationale: users cannot reason about opaque IDs in the wait panel.
   - Alternative considered: resolve names client-side by joining against a local registry each render. Rejected due to brittle synchronization and stale name risk.

4. Wait lifecycle rendering uses explicit state mapping.
   - Decision: normalize states to pending/running/completed/failed/timeout and only show "completed" when callback correlation is satisfied.
   - Rationale: removes current running/completed overlap and inconsistent terminal rendering.
   - Alternative considered: preserve current labels and patch edge cases. Rejected because underlying semantics remain inconsistent.

## Risks / Trade-offs

- [Risk] Correlation mismatches between callback events and wait targets could strand waits in running state. -> Mitigation: add focused tests for reply-to matching and target tuple matching, plus debug logging on unmatched callbacks.
- [Risk] Older events may omit `agent_name`, causing blank labels. -> Mitigation: apply fallback label policy (name -> short id) and test both paths.
- [Risk] UI snapshot churn due to state-label and payload changes. -> Mitigation: update/add insta snapshots covering running, completed-with-callback, and callback-missing cases.
- [Risk] Tightening completion criteria may expose latent backend bugs previously masked by heuristics. -> Mitigation: add backend wait-flow tests and fail fast with clear diagnostics when callbacks are absent.

## Migration Plan

1. Add/adjust backend wait event/view-model fields to carry callback-derived completion and callback payload.
2. Update TUI wait state reducer/rendering to consume backend-computed completion and payload.
3. Switch sender/receiver display fields to `agent_name` with fallback.
4. Update wait-focused tests in core and TUI snapshot coverage.
5. Rollout with no storage migration; rollback by reverting wait state mapping and callback payload wiring.

## Open Questions

- Should callback payload rendering be truncated in summary views while preserving full text in detail views?
- For timeout/failure states, should UI include the last callback-matching attempt metadata for debugging?
- Do we need protocol-level guarantees that every wait target includes both `agent_name` and stable correlation keys?
