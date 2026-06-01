# Official Memory Control References

- OpenAI Memory FAQ: https://help.openai.com/en/articles/8590148-memory-faq
- Claude memory tool docs: https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool
- Graphiti / Zep docs: https://help.getzep.com/graphiti/getting-started/welcome

Engineering takeaways used in this project:
- Long-term memory must be inspectable and deletable by users.
- Durable memory should be separated from transient context and current-task state.
- Temporal/provenance metadata is required to avoid stale or contradictory profile facts.
