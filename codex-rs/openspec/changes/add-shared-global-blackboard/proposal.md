## Why

当前 swarm 协作缺少跨会话、跨智能体共享的关键事实平面，导致上下文易丢失、协作状态难同步、并发场景下信息覆盖风险高。需要新增全局共享黑板并纳入系统提示词与会话启动流程，以保证关键信息可持续、可注入、可审计。

## What Changes

- 新增工作目录 `.blackboard/` 目录管理能力：不存在时自动创建，并为每个会话自动创建独立共享黑板文件。
- 新增共享黑板读写规范：所有写入/修改均通过智能体使用 shell 工具执行，写入格式统一为 `[智能体名字]：消息内容`。
- 新增并发控制机制：共享黑板的读取与写入需遵循文件锁策略，避免多智能体同时修改导致冲突、丢写或脏读。
- 新增会话注入规则：每轮会话为模型注入最近一组 `user-assistant` 对话内容，注入内容包含历史 summary 与共享黑板内容。
- 调整 swarm 系统提示词：增加共享黑板行为规则，并将当前会话黑板文件路径自动注入系统提示词。
- 新增测试与调试验证：覆盖格式、会话隔离、并发锁、注入完整性等场景，并使用 `/media/wmj/BC0739C74EA78EEA/debug` 下运行产物进行排错验证。

## Capabilities

### New Capabilities

- `session-shared-blackboard-storage`: 定义 `.blackboard` 目录生命周期、按会话创建黑板文件与路径命名规则。
- `blackboard-shell-write-contract`: 定义黑板只能经 shell 工具读写、写入格式 `[智能体名字]：消息内容` 与追加/修改契约。
- `blackboard-concurrency-locking`: 定义多智能体并发读写的锁获取、超时/失败处理与一致性要求。
- `blackboard-and-history-injection`: 定义每轮注入最近 `user-assistant` 对以及其内含 summary 和黑板内容的策略。
- `swarm-prompt-blackboard-rules`: 定义 swarm 系统提示词中的黑板规则与会话黑板路径自动填充要求。
- `blackboard-integration-validation`: 定义共享黑板端到端测试范围与基于 debug 目录的诊断验证要求。

### Modified Capabilities

- (none)

## Impact

- 运行时会话层：需在会话启动阶段创建 `.blackboard` 与会话文件，并维护路径映射。
- 提示词构建层：需扩展 swarm system prompt 生成逻辑，注入黑板规则与动态路径。
- 执行与工具层：需在黑板读写路径统一使用 shell 执行与锁控制。
- 测试体系：需新增并发与注入一致性测试，并接入 debug 日志核验流程。
