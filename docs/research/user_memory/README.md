# User Memory Research and Evaluation

This folder contains local literature and implementation-facing artifacts for improving profile growth across dialogue turns. The working direction is to treat long-term user memory as an inspectable, provenance-rich store of small profile facts derived from an immutable evidence log, not as an opaque conversation summary.

## Local literature

| File | Use in this project |
| --- | --- |
| `2305.10250_memorybank.pdf` | MemoryBank-style long-term memory, reflection, and forgetting signals. |
| `2304.03442_generative_agents.pdf` | Reflection/retrieval over episodic observations and higher-level memories. |
| `2310.08560_memgpt.pdf` | Tiered memory management and explicit movement between context and long-term storage. |
| `2303.11366_reflexion.pdf` | Verbal reflection after failures to improve future behavior. |
| `2024_acl_lamp.pdf` | Personalized evaluation tasks and profile-item retrieval for personalization. |
| `2024_acl_locomo.pdf` | Long-context multi-session dialogue evaluation patterns. |
| `2410.10813_longmemeval.pdf` | Long-memory QA categories including temporal and multi-hop recall. |
| `2025_acl_personalens.pdf` | User-profile evaluation framing and profile lensing. |
| `2504.19413_mem0.pdf` | Production memory pipeline: extraction, update, retrieval, latency/token tradeoffs. |
| `2501.13956_zep_graphiti.pdf` | Temporal knowledge-graph memory with facts, entities, and episodes. |
| `2502.12110_a_mem.pdf` | Agentic memory ideas for selecting, updating, and using memories. |
| `2411.00027_llm_personalization_survey.pdf` | Personalization taxonomy, risks, and evaluation concerns. |
| `2504.07070_preference_alignment_survey.pdf` | Preference alignment risks and preference drift. |
| `2406.17803_user_profile_role.pdf` | Role of user profiles in LLM personalization. |
| `official_memory_controls.md` | Product safety requirements: inspectability, deletion, durable/transient separation, provenance, and temporal metadata. |

## Derived artifacts

- `literature_synthesis.md` summarizes the borrowed ideas and how they map to a profile-growth method.
- `profile_growth_design.md` specifies a compact method: evidence log, atomic facts, update policy, retrieval views, and user controls.
- `profile_growth_eval.md` defines the offline evaluation protocol and metrics.
- `eval_profile_growth.py` is a deterministic, cheap harness with embedded dialogue cases and expected profile facts.

## Quick evaluation

Run from the repository root:

```bash
python3 docs/research/user_memory/eval_profile_growth.py
```

The script prints JSON metrics and exits non-zero if the proposed policy regresses below its built-in acceptance thresholds.
