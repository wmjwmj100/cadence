# Codex Office 系统工作逻辑

> 最后更新：2026-05-30

---

## 1. 系统概览

Codex Office 是一个基于 Wecode CLI 的多智能体协作办公系统。它让多个人类用户各自拥有一个 AI Agent，Agent 之间可以通过 `call`/`wait` 互相协作，并通过 `exec_command` 实际执行代码更改、文件创建等任务。

```
┌─────────────────────────────────────────────────────────┐
│                    浏览器 (Web UI)                       │
│            http://192.168.40.151:8080                   │
└──────────────────────┬──────────────────────────────────┘
                       │ HTTP
┌──────────────────────▼──────────────────────────────────┐
│              codex-office-server (Rust)                  │
│  ┌─────────┐  ┌──────────┐  ┌────────────────────┐     │
│  │ Web 服务 │  │ Agent 线程│  │ Runtime Store      │     │
│  │ (axum)  │  │ Manager  │  │ (JSON 持久化)       │     │
│  └─────────┘  └──────────┘  └────────────────────┘     │
│                      │                                   │
│  ┌───────────────────▼──────────────────────────────┐   │
│  │           Swarm 多智能体运行时                     │   │
│  │  Agent A (CEO)  Agent B (算法)  Agent C (Infra)   │   │
│  │  Agent D (产品)  Agent E (工程)  Agent F (运营)   │   │
│  │                                                   │   │
│  │  每个 Agent 有独立的:                              │   │
│  │  - LLM 线程 (调用 GPT-5.5)                        │   │
│  │  - 工具集 (call / wait / exec_command / ...)      │   │
│  │  - 工作空间                                       │   │
│  └───────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
                       │
┌──────────────────────▼──────────────────────────────────┐
│           agent-brain-bridge.py (Python)                 │
│   监控收件箱 → 调用 LLM → 写入黑板 (辅助处理)            │
└─────────────────────────────────────────────────────────┘
```

---

## 2. 核心组件

### 2.1 codex-office-server

- **语言**：Rust，基于 `codex-rs` monorepo
- **Web 框架**：axum
- **端口**：8080（默认，可通过 `--port` 修改）
- **监听**：`0.0.0.0`（所有网卡，局域网内可访问）
- **沙箱模式**：`danger-full-access`（无沙箱限制）
- **审批策略**：`never`（无需人工审批）
- **运行模式**：Swarm（多智能体协作模式）

### 2.2 agent-brain-bridge.py

- **语言**：Python 3
- **模式**：守护进程（`--watch`，每 5 秒轮询）
- **功能**：监控 `office-collab-inbox.json`，调用 LLM 处理消息，写入黑板
- **可选**：如果 Agent 线程正常工作，Brain Bridge 是辅助角色

### 2.3 持久化存储

| 文件 | 用途 |
|---|---|
| `office-store.json` | 账号、用户画像、Agent 配置 |
| `office-runtime.json` | Agent 运行时状态、活动流 |
| `office-collab-inbox.json` | Agent 间协作消息收件箱 |
| `office-blackboard.json` | 共享黑板（所有 Agent 的产出） |
| `office-memory/` | 每个 Agent 的长期记忆 |

---

## 3. Agent 生命周期

### 3.1 创建时机

1. **CEO 首次发消息**：CEO Agent 线程创建，同时触发所有固定 Agent 线程创建
2. **其他用户发消息**：对应的 Agent 线程被创建（CEO 作为父线程）
3. **所有 Agent 创建后**：作为后台任务持续运行，直到服务重启

```rust
// codex-office-server.rs
let mut agent_config = ConfigBuilder::default()
    .harness_overrides(ConfigOverrides {
        approval_policy: Some(AskForApproval::Never),
        sandbox_mode: Some(SandboxMode::DangerFullAccess),
        ..Default::default()
    })
    .build().await?;
agent_config.experimental_mode = Some(ModeKind::Swarm);
```

### 3.2 Agent 状态

| 状态 | 含义 |
|---|---|
| `idle` | 空闲，等待指令 |
| `working` | 正在处理任务（LLM 调用 / 工具执行中） |
| `waiting` | 正在等待其他 Agent 或人类的回复 |

### 3.3 提示词构成

每个 Agent 的开发者指令由以下模板拼接而成：

```
基础提示词 (swarm_main.md 或 swarm_sub_worker.md)
  + 办公室协作规则 (office_collaboration_rules.md)
  + 主人绑定信息 (owner_binding.md)
  + 固定名单 (fixed_roster.md)
  + 主人画像 (从 office-store.json 动态加载)
```

---

## 4. 消息流转

### 4.1 人类发送任务

```
浏览器表单 POST /api/inbox
  → owner_message 处理器
    → ① 存入 collab_inbox（共享收件箱）
    → ② 投递到 Agent 线程 (send_input)
    → ③ Agent 线程唤醒，开始新一轮 turn
    → ④ 事件流推送到 runtime store
    → ⑤ Web 页面轮询 (/api/me) 获取最新活动
```

### 4.2 Agent 间通信

```
Agent A → call({target_agent_name: "agent_b", need_reply: true, content: "..."})
  → 消息写入 collab_inbox（agent_b 的收件箱）
  → Agent B 线程收到通知
  → Agent B 处理 → 回复

Agent A → wait({timeout_ms: 3600000})
  → 阻塞等待 Agent B 的回复或超时
  → 收到回复 → 继续执行
  → 超时 → 重试（最多 5 次）→ 强制停止
```

### 4.3 Agent 呼叫人类

```
Agent B → call({target_id: "user_b", need_reply: true, content: "请确认..."})
  → 消息写入 human_inbox
  → Agent B → wait({timeout_ms: 120000})  ← 只等 120 秒
  → 人类回复（通过 Web UI）→ Agent B 收到回复
  → 人类不回复 → 120 秒超时 → 按保守默认方案继续
```

---

## 5. 任务执行

### 5.1 两种执行路径

| 任务类型 | 执行方式 |
|---|---|
| 简单任务（单文件写入、快速查询） | `exec_command` 直接执行 Shell |
| 复杂任务（多文件改动、重构、项目搭建） | `exec_command` 调用 `wecode exec` |

### 5.2 wecode exec 委派

```bash
wecode exec \
  -c approval_policy=never \
  -c sandbox_mode=danger-full-access \
  '详细的任务描述'
```

`wecode exec` 是一个完整的 Wecode Agent 实例，拥有 100+ 工具：
- Shell 命令执行
- 文件读写
- 代码编辑 (apply_patch)
- Git 操作
- 代码搜索 (grep_files)
- Web 搜索
- 测试运行

### 5.3 Agent 可用的工具

| 工具 | 功能 |
|---|---|
| `call` | 呼叫其他 Agent 或人类 |
| `wait` | 等待回复（最大 1 小时，5 次连续超时后强制停止） |
| `read_agent_status` | 查看其他 Agent 状态 |
| `spawn_agent` | 启动子 Agent |
| `exec_command` / `shell` | 执行 Shell 命令 |
| `apply_patch` | 应用代码改动 |
| `update_plan` | 更新工作计划 |
| `view_image` | 查看图片 |

---

## 6. 协作规则

### 6.1 路由规则

| 目标 | 使用 |
|---|---|
| 固定名单 Agent | `call({target_agent_name: "agent_b", ...})` |
| 人类主人 | `call({target_id: "user_b", ...})` |
| 不需要回复 | `need_reply: false` |
| 需要对方输出才能继续 | `need_reply: true` |

### 6.2 等待规则

| 场景 | 超时 |
|---|---|
| 等待 Agent 回复 | 默认（约 1 小时），建议 5 分钟 |
| 等待人类回复 | **强制 120 秒（2 分钟）** |
| 人类不回复 | **不等第二次，立即执行兜底方案** |
| 连续超时 5 次 | **系统强制停止等待，必须回复** |

### 6.3 反阻塞规则

```
关键：当另一个 Agent 在等你的回复时，
你绝不因等待人类而让整条链卡住。

做法：
1. call 人类 → wait(120s)
2. 120s 后无人回复 → 立即用保守默认值继续
3. 执行工作 → 回复上游 Agent
4. 在回复中注明 "主人未回复，已按保守方案执行"
```

---

## 7. 账号体系


### 7.2 登录流程

```
浏览器 → POST /login (username + password)
  → PersistentPilotDirectory::login()
  → 验证 accounts 表中的用户名和密码
  → 创建 Session (UUID token + HttpOnly Cookie)
  → 303 重定向到 /me (Agent 仪表盘)
```

---

## 8. Web 界面

### 8.1 仪表盘 (`/me`)

- Agent 状态指示器（idle / working / waiting）
- Agent Inbox：给 Agent 发任务
- Human Inbox：Agent 发给人类的待回复消息
- 活动流：Agent 的实时工作日志
- 主人画像：偏好和能力标签
- 每日复盘：记录当日总结
- 共享黑板入口

### 8.2 实时更新

- JavaScript 每 **1.5 秒** 轮询 `/api/me` 获取最新状态
- 数据无变化时跳过 DOM 重绘（避免闪烁）
- Agent 工作状态显示 `⚙️ Agent 工作中`
- 状态指示灯带脉冲动画

### 8.3 关键 API

| 端点 | 方法 | 功能 |
|---|---|---|
| `/login` | POST | 登录 |
| `/me` | GET | Agent 仪表盘 |
| `/api/me` | GET | Agent 状态 JSON |
| `/api/inbox` | POST | 发送消息给 Agent |
| `/api/blackboard` | GET/POST | 共享黑板 |
| `/interview` | GET/POST | 主人画像访谈 |
| `/api/reflection` | POST | 每日复盘 |
| `/logout` | POST | 退出 |

---

## 9. 完整工作流示例

### 场景：CEO 要求重构代码仓库

```
1. CEO (userA) 在 Web UI 输入：
   "请重构 auth 模块，把 JWT 认证换成 session 认证"

2. CEO Agent (agent_ceo) 收到消息：
   → LLM 分析需求
   → 判断需要咨询 Infra 和工程 Agent
   → call(agent_c, need_reply=true): "请评估 session 方案的部署影响"
   → call(agent_d, need_reply=true): "请评估 auth 模块的代码改动范围"
   → wait(agent_c) + wait(agent_d)

3. Agent C (Infra) 处理：
   → LLM 分析
   → 需要确认 Redis 选型 → call(owner: userC, timeout=120s)
   → 主人 userC 回复: "用 Redis Cluster"
   → exec_command: "grep -r redis docker-compose*"
   → 回复: "需要加 Redis Cluster, 部署脚本 3 处改动"

4. Agent D (工程) 处理：
   → LLM 分析 → 不需要咨询主人
   → exec_command: "grep -r JWT src/auth/"
   → 回复: "涉及 5 个文件, 约 200 行代码"

5. CEO Agent 汇总：
   → 形成完整任务描述
   → exec_command: wecode exec "重构 auth 模块..."
   → wecode exec 完整 Agent 执行:
      ├ 读取所有相关文件
      ├ 修改代码 (JWT → session)
      ├ 更新测试
      ├ 运行测试验证
      └ 返回: "修改了 8 个文件, 测试全部通过"

6. CEO Agent → call(owner: userA): "重构完成..."
```

---

## 10. 部署与运维

### 10.1 启动

```bash
# 1. 启动 Office Server
/work/Cadence/codex-rs/target/debug/codex-office-server \
  --db /work/Cadence/run/office/office-store.json \
  --host 0.0.0.0 \
  --port 8080 &

# 2. 启动 Brain Bridge
python3 /work/Cadence/run/agent-brain-bridge.py --watch &
```

### 10.2 访问

```
http://192.168.40.151:8080
```

同局域网内任何设备均可访问。

### 10.3 依赖

- Rust 编译的 `codex-office-server` 二进制
- `~/.wecode/config.toml`（LLM API 配置）
- Python 3（Brain Bridge）
- 局域网连通（同一网段）

### 10.4 关键配置

```toml
# ~/.wecode/config.toml
model_provider = "codex"
model = "gpt-5.5"
model_reasoning_effort = "xhigh"

[model_providers.codex]
base_url = "https://pixboostai.com:8443/gateway/v1"
wire_api = "responses"
requires_openai_auth = true
```

---

## 11. 关键文件索引

| 文件 | 说明 |
|---|---|
| `core/src/bin/codex-office-server.rs` | 服务器入口 |
| `core/src/office/web.rs` | Web 路由、Agent 线程管理、事件记录 |
| `core/src/office/pilot.rs` | 账号、画像、登录逻辑 |
| `core/src/office/persistence.rs` | 持久化读写 |
| `core/src/office/prompts.rs` | 提示词渲染 |
| `core/src/office/tools.rs` | 工具能力策略 |
| `core/src/office/workspace.rs` | 工作空间定义 |
| `core/templates/office/office_collaboration_rules.md` | 协作规则模板 |
| `core/templates/office/owner_binding.md` | 主人绑定模板 |
| `core/templates/office/fixed_roster.md` | 固定名单模板 |
| `core/src/tools/handlers/call.rs` | call 工具实现 |
| `core/src/tools/handlers/wait.rs` | wait 工具实现 |
| `core/src/tools/spec.rs` | 工具集配置 |
