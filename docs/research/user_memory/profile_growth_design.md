# Profile Growth Design

## Method: Evidence-Grounded Atomic Profile Growth

The proposed method has four layers:

1. **Evidence log**: append each user turn with stable turn ID, timestamp/session, text, and any deletion/no-memory flags.
2. **Candidate extraction**: extract explicit candidate facts from user text, scoped to durable categories: preferences, constraints, goals, projects, recurring context, and stable self-descriptions.
3. **Profile merge policy**: merge candidates into atomic profile facts using contradiction handling, confidence updates, provenance, and temporal validity.
4. **Retrieval view**: materialize a small task-relevant view for prompt injection, ranked by relevance, confidence, recency, and importance.

## Update policy

For each extracted candidate:

- **New explicit fact**: add active fact with moderate confidence and evidence reference.
- **Repeated compatible fact**: raise confidence and append evidence ID.
- **Specificity refinement**: update value only if the new value is more precise and compatible; keep evidence chain.
- **Contradiction/correction**: set old fact `status=inactive`, close `valid_to_turn`, create a replacement fact with correction evidence.
- **Temporary context**: keep out of durable profile unless the user states it is recurring or future-relevant.
- **Sensitive or ambiguous inference**: do not store unless explicit, necessary, and user-beneficial.
- **Delete/no-memory request**: remove matching facts from active retrieval and retain only minimal audit metadata if required by product policy.

## Retrieval policy

Prompt injection should use a bounded profile view:

1. Filter to `status=active`, non-expired, allowed-by-controls facts.
2. Score candidates by lexical/semantic relevance to the current user task, confidence, recency, and importance.
3. Deduplicate near-equivalent facts by predicate/value.
4. Inject concise bullet facts with provenance-hidden user-facing phrasing.
5. Never inject inactive contradictions unless the task asks about profile history.

## Reflection and compaction

Run reflection after a session or after notable profile churn:

- Detect clusters of repeated evidence and materialize higher-confidence stable facts.
- Detect contradictions and preference drift.
- Lower confidence for facts that are old, unused, and unsupported by recent evidence.
- Produce a short audit note that explains why profile facts changed.

## Implementation mapping from peer code review

Shared code mapping indicates the active profile owner is the Rust Office pipeline, especially `codex-rs/core/src/office/pilot.rs`, `persistence.rs`, and `web.rs`; `run/agent-brain-bridge.py` reads `office-store.json` profiles for prompt construction and does not update profiles. Therefore, the smallest implementation direction is to place extraction/merge in the Office owner-message, daily-reflection, or memory pipeline and let bridge prompt injection consume the resulting active profile view.

## Acceptance criteria

A useful profile-growth implementation should:

- Improve active-fact F1 on multi-turn profile cases over append-only and last-turn baselines.
- Preserve provenance for every active fact.
- Close stale facts after explicit corrections.
- Keep retrieval views small enough for prompt use.
- Respect inspect/delete/no-memory controls.
