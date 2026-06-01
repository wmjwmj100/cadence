# 多智能体自驱动测试与优化工作流（文档版）

这份文档用于让智能体自己完成以下闭环：

1. 自己设定协作任务
2. 运行 Swarm 场景
3. 抓取中间过程与日志
4. 分析协作逻辑问题
5. 产出优化建议并回归验证

不依赖专用脚本，直接用命令即可执行。

## 1. 前置条件

- 已有可运行二进制：`target/debug/wecode.exe`
- 在仓库根目录执行命令：`C:\work\codex-rs`
- 允许使用 `--swarm --json`

## 2. 一轮测试的标准流程

### 步骤 A：让智能体先定义“本轮测试任务”

要求智能体先生成一条测试任务描述（不是直接写代码），覆盖至少一种协作压力场景：

- 任务依赖：A 需要 B 的输出
- 同对象并行：多个 agent 操作同一文件/模块
- 反复失败：某 agent 重试同任务
- 信息互补：A 有关键上下文，B 正在阻塞

推荐给智能体的任务生成约束：

- 至少 `spawn 3` 个子智能体
- 至少一次 `wait`
- 每次工具调用写具体 `summary`
- 最终输出“问题 + 证据 + 建议”

### 步骤 B：运行被测场景并采集原始证据

```powershell
$runId = Get-Date -Format "yyyyMMdd_HHmmss"
$out = ".\tmp\swarm-manual\$runId"
New-Item -ItemType Directory -Path $out -Force | Out-Null

$env:RUST_LOG = "codex_core::swarm::dispatcher=debug,warn"
$env:CODEX_DEBUG_MODEL_IO = "1"
$env:CODEX_DEBUG_MODEL_IO_DIR = "$out\model-io"

$prompt = @'
请执行一次高可观测性的 swarm 协作测试：
1) 并行 spawn 3 个子智能体；
2) 至少两个子智能体处理同一对象；
3) 使用 call + wait 汇总结果；
4) 每次工具调用都带具体 summary；
5) 最终输出：协作风险、证据、优化建议。
'@

$prompt | .\target\debug\wecode.exe exec --swarm --json --dangerously-bypass-approvals-and-sandbox - `
  1> "$out\events.jsonl" `
  2> "$out\stderr.log"
```

### 步骤 C：抽取中间过程关键信号

#### 1) 协作工具调用链（events）

```powershell
rg '"type":"item.started".*"type":"collab_tool_call"' "$out\events.jsonl"
rg '"tool":"spawn_agent"|"tool":"send_input"|"tool":"wait"' "$out\events.jsonl"
```

目标：

- 是否真的并行 spawn
- 是否出现 call/wait 闭环
- wait 是否完成或超时

#### 2) 调度智能体输入/输出（stderr）

```powershell
rg "dispatcher activation input|dispatcher reminder sent|dispatcher reminder dedupe_suppressed|dispatcher activation complete" "$out\stderr.log"
```

目标：

- 激活输入：队列快照 `queue_lines_json`
- 提醒输出：`target/message/reason`
- 完成指标：attempt/success/failure/dedupe

#### 3) 每个 agent 的模型 I/O（model-io）

目录：

- `"$out\model-io\ai-flow\model-io-<thread_id>.ai.jsonl"`

目标：

- 看每个线程是否按预期调用工具
- 看是否有长时间停滞、重复输出、错误恢复失败

### 步骤 D：按统一规则分析

建议智能体按以下规则给结论：

1. 协作拓扑正确性  
是否存在 `spawn -> call -> wait -> reply` 的闭环。

2. 调度触发正确性  
非调度 summary 数量上来后，dispatcher 是否激活。

3. 提醒有效性  
提醒是否落到正确目标；`reason` 是否引用了真实队列证据。

4. 去重逻辑正确性  
同一目标未响应时是否抑制重复提醒；响应后是否恢复可提醒。

5. 失败恢复能力  
wait 超时、目标不存在、消息错配时是否有可诊断证据。

### 步骤 E：产出优化建议（要求可执行）

每条建议都必须包含：

- 现象
- 证据（日志原文或行）
- 根因假设
- 改动位置（文件/函数）
- 风险
- 验证方式

建议模板：

```text
[问题]
[证据]
[根因]
[修改建议]
[验证命令]
```

## 3. 智能体输出格式建议

建议智能体每轮都输出同一结构，便于自动比对：

```markdown
## Round Summary
- Scenario:
- Result:
- Key Metrics:

## Findings
1. ...
2. ...

## Evidence
- events:
- stderr:
- model-io:

## Optimization Proposals
1. ...
2. ...

## Next Round Plan
- ...
```

## 4. 回归对比（建议至少两轮）

每轮都记录以下可比较指标：

- `collab_tool_call` 总数
- `wait` 成功数/超时数
- dispatcher 激活次数
- dispatcher `reminder_successes`
- dispatcher `dedupe_suppressions`

如果改动后：

- 激活次数上升但提醒质量下降，说明策略过于激进
- 去重抑制异常高，说明目标可能未消费提醒或状态回写有问题
- wait 超时增加，说明 call/reply 相关性约束可能被破坏

## 5. 推荐最小闭环

1. 智能体生成任务  
2. 运行一次 swarm  
3. 抽取 events/stderr/model-io  
4. 产出建议  
5. 改动后再跑一次  
6. 输出“前后指标对比 + 是否通过”

做到这 6 步，基本就能让多智能体协作逻辑进入“可持续自动化优化”状态。

