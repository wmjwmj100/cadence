## Why

当前多智能体系统缺少跨智能体的全局状态观察与主动协作机制，导致依赖信息无法及时传递、重复失败难以及时打断、并行工作容易冲突。引入全局调度智能体可以在不打断现有执行流的前提下建立轻量协作闭环，提升整体吞吐与任务完成率。

## What Changes

- 新增全局 FIFO summary 队列，记录所有智能体（含调度智能体）每次工具调用后的行为摘要，并附带写入时间戳。
- 为所有智能体增加强制 summary 写入约束，并为队列增加容量上限（40）与严格 FIFO 淘汰规则。
- 新增调度智能体激活机制：每累计 10 条非调度 summary 触发一次，输入为当前队列全量内容。
- 新增调度智能体运行约束：无持久对话历史，仅通过队列中的历史 summary 形成隐式记忆。
- 新增协作判断与去重规则：覆盖任务依赖、反复失败、同一对象冲突、信息互补四类触发条件，并避免对未响应智能体重复提醒。
- 新增 `remind` 工具调用契约、参数校验、错误处理与投递语义；提醒通过 inbox 在目标智能体下一轮触发时送达。
- 要求调度智能体每次成功调用 `remind` 后，额外写入一条自身 summary 记录提醒行为。

## Capabilities

### New Capabilities

- `global-summary-fifo-queue`: 定义并维护全局 summary 队列的数据结构、写入规则、时间戳格式、容量与淘汰语义。
- `dispatcher-activation-cycle`: 定义非调度 summary 计数、10 条触发激活、激活输入与计数归零规则。
- `dispatcher-collaboration-judgment`: 定义调度提示词、协作判定标准与基于历史提醒的去重策略。
- `dispatcher-remind-delivery`: 定义 `remind` 工具 schema、参数校验、错误处理、inbox 延迟送达与提醒后 summary 记录规则。
- `dispatcher-boundary-behaviors`: 定义满队列、新摘要并发写入、无效目标智能体、低活跃智能体等边界行为。

### Modified Capabilities

- (none)

## Impact

- 运行时：需要在统一工具调用路径注入 summary 记录逻辑，并对调度智能体计数规则做差异化处理。
- 调度层：需要新增可周期触发的 dispatcher 执行回路，以及从 FIFO 全量读取构造单轮输入的能力。
- 工具与消息系统：需要新增 `remind` 工具并复用现有 inbox 投递通道。
- 数据与可观测性：需要新增或扩展 summary 存储、时间戳记录、触发计数与调度日志。
