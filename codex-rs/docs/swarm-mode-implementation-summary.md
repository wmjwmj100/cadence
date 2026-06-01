# Swarm 模式当前实现摘要

这份说明以当前源码为准，重点描述 `spawn_agent / call / wait` 协作链路、环境感知注入、共享黑板，以及两套 debug 产物的位置。

## 1. 协作主链路

Swarm 模式的主入口来自 `core/src/models_manager/collaboration_mode_presets.rs`，会根据 session source 与当前是否为 complex 选择 `swarm_main.md` / `swarm_main_complex.md` / `swarm_sub.md` / `swarm_sub_complex.md` 作为 developer instructions。进入 Swarm 后，工具面被收敛为 `spawn_agent`、`call`、`wait`、`remind`，而 `send_input / resume_agent / close_agent` 被显式禁用。

`spawn_agent` 只负责创建子线程和配置，不直接投递任务；它会继承父 turn 的 collaboration mode、模型、权限与大部分运行配置，把调用时传入的 `system_prompt` 追加到子 agent 的 developer instructions，然后返回 `agent_name` 而不是 thread id。当前 thread-spawn 深度上限是 1，所以只有主 agent 可以继续 spawn 一层子 agent。对于 `Swarm Complex`，sub-agent 不再统一退回普通 `swarm_sub.md`，而是会解析到专门的 `swarm_sub_complex.md`，从而把 strict-artifact / validation lane 的 verifier-facing evidence 约束也带到子线程。

`call` 是真正的通信入口，语义是 dispatch-only。它会按目标 agent 当前状态决定是“启动新 turn”还是“把消息排进正在运行的 turn”，并且始终把消息写入 `collab_inbox`。消息关联依赖 `message_id / reply_to_message_id`，`need_reply=true` 时会在 `collab_inbox` 里登记 required-reply obligation，收到带 `reply_to_message_id` 的回包后再消解这条 obligation。`wait` 则不再按 target 集合聚合等待，而是只接受 `timeout_ms`，阻塞到 inbox 里出现下一条消息后按 FIFO 取出返回；也就是说，当前相关性仍然靠 `call` 的消息 id 协议维持，而不是靠 `wait.targets[]`。

另外，所有函数型工具调用都必须带 `summary`。这个 `summary` 在 `core/src/tools/registry.rs` 中会先被剥离，再写入 agent workboard；这些摘要既是“其他 agent 状态”的来源，也是 dispatcher 自动协调的输入。dispatcher 运行在 `core/src/swarm/{dispatcher,dispatcher_runtime}.rs`，维护一个最多 40 条的摘要队列，每累计 10 条非 dispatcher 摘要就触发一次激活，并按“依赖信号 / 重复失败 / 共享对象”规则自动发 `remind`。

## 2. 环境感知与共享黑板

Swarm 的环境感知不是靠额外工具，而是靠 prompt 注入完成的。在每次真正发起 sampling request 之前，`core/src/codex.rs` 会向上下文尾部追加一个合成的 `user/assistant` 对：其中 user 消息包含当前 UTC 时间、协作中的 agent 列表、每个 agent 最多 40 条摘要，以及共享黑板快照；assistant 消息是固定 ACK，用来让模型把这块内容当作“已吸收的环境状态”而不是需要显式回复的对话。这也是当前 swarm 做“其他智能体状态 + 共享黑板状态”环境感知的核心实现。

共享黑板本身由 `core/src/blackboard.rs` 和 `core/src/codex.rs` 负责初始化。文件路径是 `<workspace>/.blackboard/<owner_thread_id>.md`，锁文件是同路径再加 `.lock`。如果 session source 是 `ThreadSpawn` 子 agent，blackboard owner 会回退到父线程 id，因此整组 swarm agent 共享同一个黑板文件，而不是每个子 agent 各自一份。Swarm 额外 developer instructions 还会把 agent 自己的名字、blackboard 路径、lock 路径和标准写入格式 `[agent_name]：message_content` 一起注入，约束 agent 必须通过 shell + 文件锁来写黑板。除此之外，如果某个 agent 还有未完成的 `need_reply` 义务，运行时也会再注入一条 synthetic user reminder，提醒它尽快用 `call + reply_to_message_id` 回补通信闭环。

## 3. Debug 方式与文件位置

当前有两套通过环境变量开启的 debug 方案。

第一套是每个 agent 独立的 model I/O trace。开启方式是 `CODEX_DEBUG_MODEL_IO=1`，可选目录覆盖变量是 `CODEX_DEBUG_MODEL_IO_DIR`；未覆盖时默认写到 `<codex_home>/debug/model-io/`。产物有两份：`model-io-<thread_id>.md`，以及 `ai-flow/model-io-<thread_id>.ai.jsonl`。它适合看单个 agent 的请求、输入项、工具调用和响应时间线。

第二套是整个 swarm 会话级别的 canonical debug trace。开启方式是 `CODEX_DEBUG_TRACE=1`，可选目录覆盖变量是 `CODEX_DEBUG_TRACE_DIR`；默认目录是 `<codex_home>/debug/conversations/`。每个根会话会生成一个目录，里面至少有 `history.latest.json` 和 `metadata.json`。这套 trace 会把 primary agent、subagent 和 dispatcher 合并成统一 lane 视图，并且每次更新都重写最新快照，因此更适合看跨 agent 协作时序。

主要实现位置可以直接看：`core/src/tools/handlers/{collab,call,wait,remind}.rs`、`core/src/tools/handlers/collab_inbox.rs`、`core/src/codex.rs`、`core/src/blackboard.rs`、`core/src/swarm/{dispatcher,dispatcher_runtime}.rs`、`core/src/model_io_recorder.rs`、`core/src/debug_trace.rs`。
