# Swarm 协作自动化测试（含调度智能体观测）

本文档提供一条命令的自动化流程，用于测试多智能体协作实现，并抓取中间过程与辅助协作（dispatcher）行为。

## 目标

- 自动运行一次 `wecode.exe` 的 `--swarm` 场景
- 自动收集以下证据
  - 多智能体协作工具调用链（`spawn_agent` / `call` / `wait`）
  - 各智能体输出片段
  - dispatcher 激活输入（FIFO 快照）
  - dispatcher 提醒输出（target/message/reason）
  - dispatcher 激活指标（attempt/success/failure/dedupe）
- 自动生成可读报告（Markdown）和结构化报告（JSON）

## 新增脚本

- `scripts/swarm-auto-test.ps1`
  - 负责执行测试、落盘原始日志、触发报告生成
- `scripts/swarm-report.py`
  - 负责解析 `events.jsonl` + `stderr.log` + 可选 `model-io`，产出报告

## 快速开始

在仓库根目录执行：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\swarm-auto-test.ps1 `
  -WecodePath .\target\debug\wecode.exe
```

默认输出目录：

```text
tmp/swarm-auto/<timestamp>/
```

核心产物：

- `events.jsonl`: `wecode exec --json` 原始事件流
- `stderr.log`: 运行日志（包含 dispatcher debug 行）
- `report.md`: 人类可读报告
- `report.json`: 结构化报告（便于二次分析/CI）
- `model-io/`（默认开启）: 按线程拆分的模型 I/O 调试日志

## 常用参数

```powershell
# 使用自定义提示词文件
powershell -ExecutionPolicy Bypass -File .\scripts\swarm-auto-test.ps1 `
  -WecodePath .\target\debug\wecode.exe `
  -PromptFile .\tmp\my-prompt.txt

# 使用内联提示词
powershell -ExecutionPolicy Bypass -File .\scripts\swarm-auto-test.ps1 `
  -WecodePath .\target\debug\wecode.exe `
  -Prompt "请执行一次 3-agent 协作并输出风险分析。"

# 指定输出目录
powershell -ExecutionPolicy Bypass -File .\scripts\swarm-auto-test.ps1 `
  -WecodePath .\target\debug\wecode.exe `
  -OutDir .\tmp\swarm-ci\run-001

# 关闭 model-io 采集（仅保留 events/stderr）
powershell -ExecutionPolicy Bypass -File .\scripts\swarm-auto-test.ps1 `
  -WecodePath .\target\debug\wecode.exe `
  -DisableModelIo
```

## dispatcher 输入输出如何观测

脚本会设置：

- `RUST_LOG=codex_core::swarm::dispatcher=debug,warn`

报告通过 `stderr.log` 中的 dispatcher 结构化日志提取：

- 激活输入：
  - `dispatcher activation input: activation_id=..., queue_entries=..., queue_lines_json=...`
- 提醒发送：
  - `dispatcher reminder sent: activation_id=..., target=..., message_json=..., reason_json=...`
- 去重抑制：
  - `dispatcher reminder dedupe_suppressed: ...`
- 目标不存在：
  - `dispatcher reminder target_not_found: ...`
- 激活完成指标：
  - `dispatcher activation complete: activation_id=..., reminder_attempts=..., reminder_successes=...`

## 建议接入方式（全流程自动化）

可以把该脚本接入你自己的 agent 自测链路：

1. agent 触发 `swarm-auto-test.ps1`
2. agent 读取 `report.json`
3. agent 根据报告中的以下段落给出建议并自动迭代
   - `event_summary.collab_calls`
   - `dispatcher.activations`
   - `model_io.threads`
   - `parse_errors`
4. 若建议涉及代码改动，执行改动后再次运行同脚本做回归对比

## 当前边界

- dispatcher 目前是运行时内部调度路径，非独立可交互线程；观测依赖日志与事件汇总。
- 如果你的 `wecode.exe` 不是最新编译，可能缺少本文新增的 dispatcher 可机读日志字段。
- 若模型未按提示大量调用工具，dispatcher 激活次数可能不足；建议在 prompt 中显式要求并行 + call/wait。

