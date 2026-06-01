# Profile Growth Offline Evaluation

## Goal

Measure whether a profile-growth policy creates the right durable user facts across dialogue turns while avoiding stale, contradicted, or irrelevant facts.

## Harness

`eval_profile_growth.py` embeds small synthetic dialogue cases inspired by LaMP, LoCoMo, LongMemEval, PersonaLens, Mem0, and Graphiti-style temporal memory. It does not call network services or LLM APIs.

Each case contains:

- ordered user turns,
- expected active facts after all turns,
- expected inactive facts after corrections,
- query terms for task-relevant retrieval,
- notes about the memory behavior being tested.

## Policies compared

- **append_only**: stores every extracted candidate as active; useful as a naive memory baseline.
- **last_turn_only**: stores only facts from the final user turn; useful as a short-context baseline.
- **evidence_atomic**: proposed deterministic approximation of the target method with contradiction handling and provenance.

## Metrics

| Metric | Meaning |
| --- | --- |
| `active_precision` | Fraction of active predicted facts that are correct. Penalizes stale or irrelevant memories. |
| `active_recall` | Fraction of expected active facts recovered. Penalizes missing profile growth. |
| `active_f1` | Balanced active profile quality. |
| `inactive_recall` | Fraction of expected stale facts correctly closed. |
| `provenance_coverage` | Fraction of active facts with at least one evidence turn ID. |
| `retrieval_precision` | Fraction of retrieved facts relevant to the query. |
| `retrieval_recall` | Fraction of expected query-relevant facts retrieved. |
| `avg_active_facts` | Mean active profile size per case. Controls bloat. |

## Acceptance thresholds

The harness fails if `evidence_atomic` does not satisfy all thresholds:

- `active_f1 >= 0.80`
- `active_precision >= 0.80`
- `active_recall >= 0.80`
- `inactive_recall >= 0.80`
- `provenance_coverage == 1.00`
- `retrieval_precision >= 0.80`
- `retrieval_recall >= 0.80`

These thresholds are intentionally high for the synthetic cases because they are deterministic and explicit. A later LLM-backed evaluator should use larger cases and tolerate extraction ambiguity.

## Iteration loop

1. Add a failing dialogue case for a profile-growth behavior.
2. Run `python3 docs/research/user_memory/eval_profile_growth.py`.
3. Improve extraction/merge/retrieval policy until the proposed policy clears thresholds and beats baselines.
4. Port the policy into the actual Office profile pipeline only after the offline harness captures the expected behavior.
