## Context

当前多智能体协作里，`wait` 的完成条件依赖 `target_agent + message` 匹配，调用方需要提前给出期望回复目标与文本。该模型在实际协作中会出现两个问题：
1) 目标智能体可能先发来其他有效 inbox 信息但 `wait` 仍不返回；
2) 匹配逻辑让 wait 语义与“收到了新协作输入就继续推进”不一致。

另一方面，全局状态注入目前每个智能体最多只带 2 条历史 summary，导致跨轮信息遗失快，调度与协作决策缺少上下文连续性。该能力需要扩展到每个智能体最多注入 30 条，并维持稳定上限。

## Goals / Non-Goals

**Goals:**
- 让 `wait` 不再要求 `target_agent`、`message` 参数。
- 将 `wait` 语义统一为“只要 inbox 有新内容即可返回”。
- 保持 inbox 消息消费行为可预测（按既有队列顺序返回）。
- 将每个智能体的历史 summary 注入上限从 2 调整为 30。
- 为 wait 与 summary 注入窗口更新对应测试，避免行为回退。

**Non-Goals:**
- 不重构 inbox 存储结构或改造消息传输协议。
- 不改变 summary 产生时机、summary 文本格式与全局 FIFO 容量规则。
- 不在本次变更中引入动态配置化的注入上限。

## Decisions

1. `wait` 接口去除目标匹配参数。
   - Decision: wait 工具输入不再包含 `target_agent` 和 `message` 必填字段，调用时仅表达“等待下一条 inbox 消息”。
   - Rationale: 简化调用契约，避免调用方构造脆弱匹配条件。
   - Alternative considered: 保留参数但改为可选并优先走“任意消息返回”。Rejection: 会保留双语义分支，增加维护和测试复杂度。

2. `wait` 完成条件改为 inbox 非空即返回。
   - Decision: 只要检测到 inbox 存在可消费消息，wait 立即完成并返回该消息。
   - Rationale: 与协作“被动接收提醒/回复后继续”的直觉一致，降低等待死锁概率。
   - Alternative considered: inbox 非空后继续做内容筛选。Rejection: 与新需求冲突，且会把筛选责任错误地放在 wait 层。

3. summary 注入窗口固定扩大到每智能体最多 30 条。
   - Decision: 构造智能体上下文时，从历史 summary 中选取最近 30 条（不足则全量）。
   - Rationale: 在可控 token 成本下显著提升上下文连续性。
   - Alternative considered: 无上限注入。Rejection: 容易引发提示词膨胀和性能抖动。

4. 注入顺序保持时间序，优先保留最近历史。
   - Decision: 选择“最近 30 条”，在注入展示时保持从旧到新的可读顺序。
   - Rationale: 既满足“最近信息优先”，又保留事件因果链。
   - Alternative considered: 倒序注入（新到旧）。Rejection: 阅读与推理负担更高，容易误判先后关系。

## Risks / Trade-offs

- [Risk] `wait` 放宽后可能更早返回，调用方若假设特定消息会产生行为变化。 -> Mitigation: 明确文档与测试，要求调用方在返回后自行判断消息内容。
- [Risk] summary 注入从 2 增到 30 会增加上下文 token。 -> Mitigation: 保持硬上限 30，并优先注入最近记录。
- [Risk] inbox 同时有多条消息时，返回哪一条若不明确会引发不稳定。 -> Mitigation: 沿用既有 inbox 读取顺序（FIFO）并写入回归测试。

## Migration Plan

1. 更新 wait 工具 schema/参数校验，移除 `target_agent`、`message` 依赖。
2. 调整 wait 执行逻辑为“inbox 非空即返回第一条可消费消息”。
3. 更新 wait 相关调用方与状态展示，删除对匹配消息判定的路径。
4. 将 summary 注入窗口常量从 2 调整为 30，并保留时间序输出。
5. 补充单测与集成测试：wait 返回条件、inbox 顺序、summary 注入条数与顺序。

Rollback:
- 回滚到上一版本 wait 匹配逻辑与 summary 注入窗口常量即可恢复旧行为。

## Open Questions

- wait 返回结构是否需要额外标注“本次为任意 inbox 命中”以便上层日志区分？
- 30 条注入窗口未来是否需要配置化（例如按模型上下文长度动态收缩）？
