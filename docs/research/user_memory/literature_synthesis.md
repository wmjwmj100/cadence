# Literature Synthesis for Profile Growth

## Core objective

Improve a user profile over dialogue turns without turning the profile into a brittle summary. The target system should accumulate durable, inspectable user facts, revise stale facts when the user changes their mind, preserve provenance, and retrieve only the profile facts relevant to the current turn.

## Borrowed ideas

| Source | Useful idea | Project adaptation |
| --- | --- | --- |
| MemoryBank | Long-term memory should update over time and use reflection/forgetting instead of retaining everything equally. | Add decay/staleness metadata and periodic compaction from evidence into profile facts. |
| Generative Agents | Store observations, retrieve by relevance/recency/importance, and synthesize reflections into higher-level memories. | Keep dialogue turns as evidence, then materialize atomic profile facts plus optional reflection notes. |
| MemGPT | Separate active context from archival memory and manage memory movement explicitly. | Treat profile facts as durable memory; inject only selected facts into prompts. |
| Reflexion | Use self-reflection after mistakes to improve future decisions. | Record correction evidence and lower confidence in contradicted facts instead of deleting history silently. |
| LaMP | Personalization should be evaluated with user-specific histories and retrieval from user profiles. | Score profile growth by downstream answer correctness and relevance of retrieved profile facts. |
| LoCoMo / LongMemEval | Multi-session and temporal questions expose memory failures missed by short-context tests. | Include multi-turn cases with delayed recall, preference changes, and temporal validity. |
| PersonaLens | A profile is useful only if it captures attributes that matter to personalized responses. | Evaluate exact profile facts and answer personalization, not just storage volume. |
| Mem0 | Production systems need extraction, update, retrieval, and cost/latency discipline. | Keep the harness deterministic and cheap; measure profile size and retrieval selectivity. |
| Zep / Graphiti | Episodic data and temporal graph facts help resolve entity relationships and stale facts. | Represent facts with `subject`, `predicate`, `value`, `valid_from`, `valid_to`, and evidence IDs. |
| Personalization and preference surveys | Preference drift, privacy, and over-personalization are central risks. | Require inspect/delete controls and avoid inferring sensitive attributes unless explicit and useful. |
| Official memory controls | Durable memory must be inspectable, deletable, and separated from transient context. | Design includes export/delete paths and evidence-based provenance for every fact. |

## Design principles

1. **Evidence first**: append raw user-message evidence before creating or updating profile facts.
2. **Atomic facts**: store one preference, constraint, identity claim, project, or relationship per fact.
3. **Provenance required**: every profile fact points to message evidence and update reason.
4. **Temporal validity**: contradictions close old facts with `valid_to`; they do not overwrite history without trace.
5. **Confidence is earned**: repeated consistent evidence raises confidence; corrections lower or close older facts.
6. **Retrieval is selective**: prompt injection uses task-relevant facts, not the entire profile.
7. **User control is part of quality**: export/delete and no-memory behavior are required capabilities, not optional UX polish.

## Recommended profile fact shape

```json
{
  "id": "fact-pref-meal-vegetarian",
  "subject": "user",
  "predicate": "prefers",
  "value": "vegetarian meals",
  "category": "preference",
  "confidence": 0.85,
  "valid_from_turn": 1,
  "valid_to_turn": null,
  "evidence_turn_ids": ["case1_t1", "case1_t4"],
  "status": "active",
  "sensitivity": "normal",
  "last_updated_turn": 4
}
```

## What not to borrow blindly

- Do not keep all facts active forever; stale preferences degrade personalization.
- Do not summarize away provenance; users and developers need to inspect why a fact exists.
- Do not inject the full memory store into every prompt; it increases cost and causes irrelevant personalization.
- Do not infer sensitive traits from weak evidence; durable memory should favor explicit, user-beneficial facts.
