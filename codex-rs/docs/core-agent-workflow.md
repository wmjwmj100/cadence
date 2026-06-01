# 核心 Agent 工作流（codex-rs/core）

本文聚焦 codex-rs/core 中“单个会话/线程在一次用户回合里如何驱动模型、工具、审批与状态”的实现。每一条都附带一个具体例子，便于你在源码中对照定位。

## 1. 线程与会话创建（ThreadManager -> Codex -> Session）
实现位置：`core/src/thread_manager.rs`, `core/src/codex.rs`
说明：ThreadManager 创建或恢复线程时会构建 Session，并启动 `submission_loop`。Session 初始化完成后首先发送 `SessionConfigured` 事件，包含模型、沙箱、cwd、rollout 路径等会话元信息。
例子：
```rust
// 伪代码：CLI 创建一个新线程
let new_thread = thread_manager.start_thread(config).await?;
let thread_id = new_thread.thread_id;
// 客户端会收到一次 SessionConfigured 事件
```

## 2. 多 Agent 控制面（AgentControl）
实现位置：`core/src/agent/control.rs`, `core/src/thread_manager.rs`
说明：`AgentControl` 为“多线程/多 agent”提供控制面：创建、恢复、发送输入、查询状态。它使用守卫限制最大并发线程数，并在创建后通知外部订阅线程事件。
例子：
```rust
// 伪代码：在同一会话中派生一个子 agent
let sub_thread_id = agent_control
    .spawn_agent(config.clone(), None)
    .await?;
agent_control
    .send_input(sub_thread_id, vec![UserInput::Text { text: "spawned".into(), text_elements: vec![] }])
    .await?;
```

## 3. Op 提交与主循环（submission_loop）
实现位置：`core/src/codex.rs`
说明：外部通过 `Codex::submit` 把 `Op` 送入队列，`submission_loop` 逐个消费，并分发到具体 handler（用户输入、审批反馈、配置变更、关机等）。
例子：
```rust
// 伪代码：提交一个用户回合
codex.submit(Op::UserTurn {
    cwd: "C:/work/codex".into(),
    approval_policy: AskForApproval::OnRequest,
    sandbox_policy: SandboxPolicy::WorkspaceWrite,
    model: Some("gpt-5.1-codex".into()),
    effort: None,
    summary: None,
    final_output_json_schema: None,
    items: vec![UserInput::Text { text: "解释核心流程".into(), text_elements: vec![] }],
    collaboration_mode: None,
    personality: None,
})?;
```

## 4. 每回合 TurnContext 构建与配置合并
实现位置：`core/src/codex.rs`（`make_turn_context` / `new_turn_with_sub_id`）
说明：`TurnContext` 是“本回合”的配置快照：模型信息、审批策略、沙箱、工具配置、cwd、协作模式、用户指令等。它从 SessionConfiguration 和 per-turn config 构建。
例子：
```rust
// 伪代码：构建回合上下文（示例字段）
TurnContext {
  sub_id: "turn_123",
  cwd: "C:/work/codex",
  approval_policy: AskForApproval::OnRequest,
  sandbox_policy: SandboxPolicy::WorkspaceWrite,
  tools_config: ToolsConfig::new(...),
  model_info: ModelInfo { slug: "gpt-5.1-codex", .. },
  ..
}
```

## 5. 初始上下文种子（Developer/User/Env 指令）
实现位置：`core/src/codex.rs`（`seed_initial_context_if_needed`, `build_initial_context`）
说明：首个回合会注入“初始上下文”：基于审批/沙箱策略的开发者指令、用户指令（AGENTS.md）、协作模式指令、记忆工具提示、环境上下文等。
例子：
```text
// 伪例：初始上下文里追加的指令片段
- DeveloperInstructions: "Commands must ask for approval unless trusted"
- UserInstructions: "在本仓库下工作" (来自 AGENTS.md)
- EnvironmentContext: cwd=/Users/me/project, shell=/bin/zsh
```

## 6. 记录用户输入并触发 TurnStarted
实现位置：`core/src/codex.rs`（`record_user_prompt_and_emit_turn_item`, `run_turn`）
说明：用户输入会被转换为 `ResponseItem`/`TurnItem::UserMessage` 记录进历史，同时发出 `TurnStarted` 事件，标记本回合开始。
例子：
```text
事件序列（简化）：
1) TurnStarted(turn_id=turn_123)
2) item/started(UserMessage)
3) item/completed(UserMessage)
```

## 7. 技能与应用（Skill/App）解析与注入
实现位置：`core/src/codex.rs`, `core/src/skills/*`, `core/src/mentions.rs`
说明：系统会解析显式技能/应用提及（如 `$skill-name` / `$app-name`），构建技能注入消息，并依据 MCP 工具列表决定可用的 app 工具。
例子：
```text
用户输入: "$skill-creator 生成一个新技能" 
=> 解析出 skill-creator
=> 构建 skill 注入消息，插入到对话历史中
```

## 8. 工具路由与 Prompt 组装
实现位置：`core/src/codex.rs`（`built_tools` / `run_sampling_request`）
说明：根据模型能力与特性开关构建 `ToolRouter`，再过滤得到本回合可用 `ToolSpec`；随后组装 `Prompt`（输入 + 工具 + 指令）。
例子：
```rust
let prompt = Prompt {
  input: history_items,
  tools: vec!["shell", "apply_patch", "read_file", ...],
  parallel_tool_calls: model_info.supports_parallel_tool_calls,
  base_instructions: "...",
  output_schema: None,
};
```

## 9. 采样请求与流式事件处理
实现位置：`core/src/codex.rs`（`run_sampling_request`, `try_run_sampling_request`）
说明：通过 `ModelClientSession::stream` 进行采样，逐个处理 `ResponseEvent`（OutputItemAdded、OutputTextDelta、OutputItemDone、Completed 等），并转为 UI 事件。
例子：
```text
流式事件（简化）：
- OutputItemAdded(assistant message)
- OutputTextDelta("第一段")
- OutputTextDelta("第二段")
- OutputItemDone(assistant message)
- Completed(token_usage=...)
```

## 10. 工具调用生命周期（ToolRouter -> Handler）
实现位置：`core/src/tools/router.rs`, `core/src/tools/parallel.rs`, `core/src/tools/handlers/*`
说明：当模型输出 FunctionCall/CustomToolCall 时，`ToolRouter::build_tool_call` 解析为 `ToolCall`；`ToolCallRuntime` 负责并行/串行执行，并分发到具体 handler（shell/apply_patch/read_file 等）。
例子：
```json
// 伪例：模型输出 shell 工具调用
{"type":"function_call","name":"shell","arguments":"{\"command\":[\"rg\",\"-n\",\"AgentControl\"],\"workdir\":\".\"}"}
```

## 11. 审批与沙箱编排（ToolOrchestrator）
实现位置：`core/src/tools/orchestrator.rs`, `core/src/tools/sandboxing.rs`
说明：工具调用会经过审批策略和沙箱选择。若需要审批，系统发出请求并等待用户决策；沙箱失败后可按策略升级重试（例如请求无沙箱执行）。
例子：
```text
- 申请执行 shell 命令 -> 触发 ExecApproval 请求
- 用户选择 "ApprovedForSession" -> 后续相同命令前缀可免审批
```

## 12. 工具输出回灌与继续采样
实现位置：`core/src/stream_events_utils.rs`, `core/src/tools/parallel.rs`
说明：工具执行完成后，输出会转换为 `ResponseInputItem::FunctionCallOutput` 并写回历史，`needs_follow_up` 置为 true 以继续下一轮采样。
例子：
```text
shell 输出: "matches: 12"
=> 作为 FunctionCallOutput 插入历史
=> 模型继续生成后续解释
```

## 13. Token 统计与自动压缩
实现位置：`core/src/codex.rs`（`run_pre_sampling_compact`, `run_auto_compact`）
说明：每次采样会更新 token 使用量。若超过自动压缩阈值，系统会启动 compact 任务（本地或远程）以缩短上下文。
例子：
```text
total_usage_tokens >= auto_compact_limit
=> 触发 compact
=> 压缩后的历史再进入下一轮采样
```

## 14. 回合结束、差异与中断
实现位置：`core/src/codex.rs`, `core/src/tools/events.rs`, `core/src/tasks/mod.rs`
说明：当响应 Completed，系统更新 token 信息并尝试生成 `TurnDiff`（由 `TurnDiffTracker` 汇总 patch/exec 变化），然后发出 TurnComplete。中断会取消当前任务并向工具返回 "aborted by user"。
例子：
```text
- Completed(token_usage=...)
- TurnDiff(unified_diff="diff --git ...")
- TurnComplete(last_agent_message="done")
```

---

如果你希望继续深挖某个环节（例如“审批缓存的 key 生成规则”“apply_patch 解析与安全校验流程”“MCP 工具调用与回包格式”），告诉我具体方向，我可以在此文档上追加细化章节。
