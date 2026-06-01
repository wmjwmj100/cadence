## Why

当前 `wait` 语义依赖 `target_agent + message` 精确匹配后才返回，导致等待条件过窄，与“只要有新 inbox 消息即可继续”的协作诉求不一致。同时，全局状态注入每个智能体仅保留 2 条历史 summary，跨轮上下文丢失严重，影响协作判断质量。

## What Changes

- 调整 `wait` 逻辑与接口：不再要求传入 `target_agent` 和 `message` 匹配条件。
- 将 `wait` 完成条件改为：目标智能体 inbox 只要出现任意新内容即可返回。
- 更新 wait 相关调用链与状态处理，移除对“匹配消息内容”的强依赖。
- 调整全局状态注入策略：每个智能体从历史 summary 中最多注入 30 条（由当前 2 条提升）。
- 明确 summary 注入的顺序与上限行为，避免无限增长并保持最近上下文优先。
- 补充/更新相关测试，覆盖 wait 新语义与 summary 注入窗口变化。

## Capabilities

### New Capabilities
- `wait-inbox-ready-return`: 定义 wait 在 inbox 出现新消息时立即返回的行为，以及不再要求 target/message 参数的接口契约。
- `agent-summary-history-window`: 定义每个智能体注入历史 summary 的上限为 30 条及其选择/排序规则。

### Modified Capabilities
- None (no baseline capabilities currently exist under `openspec/specs/`).

## Impact

- 影响模块：多智能体协作中的 wait 调用与 inbox 消费流程；全局 summary 到智能体上下文的注入构建流程。
- 行为影响：wait 从“匹配特定消息”切换为“任意 inbox 新消息触发返回”；上下文注入从 2 条扩大到最多 30 条。
- 验证影响：需更新 wait 行为测试、参数校验测试、summary 注入数量与顺序测试。
