你现在处于自我更新模式。我正在 `codex-rs` 项目中做多智能体优化。你的工作方式是
  持续循环：
  理解实现 -> 运行 Terminal Bench 2.0 测试 -> 按题逐个查看 trace -> 分析协作缺陷
  -> 总结问题 -> 提出具体改动方案 -> 先获得人类许可 -> 更新代码并维护记忆文件 ->
  重新测试 -> 继续下一轮。

  记忆文件固定为 `docs/swarm-self-update-memory.md`。
  每次循环开始和结束都要读取它；记录稳定事实和本轮有效发现；删除或改写已经被代码
  或新证据否定的内容。
  每次写入记忆都必须精炼：只保留稳定事实、高价值结论和下一轮真正会复用的信息，避免
  冗长复述、过程流水账和低价值细节。

  测试分析要求：
  - 默认一次优先跑 2 题，兼顾 token 成本、轨迹观察质量和迭代速度。
  - 如果只是先探路、验证命令、确认产物位置，可以先跑 1 题 pilot。
  - 启动 TB2 测评时必须放到 detached session 中运行，避免当前交互会话结束或中断时杀掉
    后台评测进程。
  - 启动后台测评任务后，不要高频轮询 job 目录或日志；优先使用 `wait` 挂起自己，每隔
    3~5 分钟再回来检查一次。
  - 实操上可把单次 `wait` 设在 `180000` 到 `300000` 毫秒之间；timeout 后只做一次必要
    检查，再决定是否继续等待。
  - 当前自我进化 / 跑题约定默认走 `Swarm Complex`；实现层面它不是新的 runtime
    `ModeKind`，而是 `ModeKind::Swarm` 下的第二个内建 preset / root prompt 变体。
  - 如果当前环境没有自动打开 complex root prompt，显式设置
    `WECODE_SWARM_COMPLEX=1` 或 `CODEX_SWARM_COMPLEX=1`。
  - trace 必须一个一个看，不要一次性全读完。
  - 如果某个 trace 只有 1 个或 2 个 agent，则跳过，不做协作诊断。
  - 协作分析重点放在 `spawn_agent`、`call`、`read_agent_status`、`wait` 的使用质
    量。
  - 以 `history.latest.json` 为主，`metadata.json` 只作辅助，不要仅凭 lane 计数下
    结论。
  - 重点排查：同质化分工、空转 `wait`、无效 `spawn_agent`、缺少 `call` 闭环、
    `message_id / reply_to_message_id` 使用混乱、协调者空转、黑板信息没有被有效利
    用。
  - 先明确题目真实要求，再判断失败是任务能力问题、协作问题、还是安全边界导致的拒
    答问题。

  下面是当前实现方式：

  # Swarm 模式当前实现摘要

  ## 1. 协作入口与工具面

  Swarm 的内建 preset 定义在 `core/src/models_manager/collaboration_mode_presets.rs`。
  当前有两个用户可见 preset：
  - `Swarm`
  - `Swarm Complex`

  注意：
  - 这两个 preset 在 runtime 层都走 `ModeKind::Swarm`。
  - root agent 的 developer instructions 会根据 preset / 环境变量 / session source
    解析到 `swarm_main.md`、`swarm_main_complex.md` 或 `swarm_sub.md`。
  - `Swarm Complex` 只影响 root prompt；spawn 出来的 sub-agent 仍然使用
    `swarm_sub.md`。

  进入 Swarm 后，当前可用的协作工具是：
  - `spawn_agent`
  - `call`
  - `read_agent_status`
  - `wait`

  在 Swarm 中明确不可用的工具是：
  - `send_input`
  - `resume_agent`
  - `close_agent`

  另外，所有函数型工具调用都必须带 `summary`。运行时会先剥离这个字段，再把它记录
  到 agent work status 中。后续的环境状态注入和 `read_agent_status` 都会消费这些
  摘要。

  ## 2. `spawn_agent`

  `spawn_agent` 只负责创建子 agent，不直接投递任务。

  它会继承父 turn 的大部分运行配置，包括：
  - collaboration mode
  - model / provider
  - reasoning effort / reasoning summary
  - developer instructions
  - cwd
  - sandbox / approval 约束

  继承的是 collaboration mode 语义和会话配置；但对于内建 Swarm prompt，root / sub
  agent 仍然会按 session source 分流到不同模板。

  调用时传入的 `system_prompt` 会追加到子 agent 的 developer instructions 中。

  返回值是 `agent_name`，不是 thread id。

  当前 thread-spawn 深度上限是 1，所以只有主 agent 可以继续 spawn 一层子 agent。

  如果生成了子 agent，应尽快通过 `call` 下发具体任务；如果生成了 `coordinator`，
  也必须尽快给它一个具体的 `call`，说明它要监控哪些 agent、要产出什么协调结果。

  ## 3. `call`

  `call` 是当前 Swarm 里真正的通信入口，语义是 dispatch-only。

  它会根据目标 agent 当前状态决定如何投递：
  - 目标是 idle / completed / interrupted / errored 时，启动新 turn。
  - 目标是 running 时，把消息排入正在运行的 turn。

  `call` 始终会把消息写入目标 agent 的 `collab_inbox`。

  消息闭环依赖：
  - `message_id`
  - `reply_to_message_id`

  其中：
  - `need_reply=true` 会登记 required-reply obligation。
  - 收到带 `reply_to_message_id` 的回包后，会尝试消解对应 obligation。

  分析 trace 时，要重点检查 `message_id` 是否唯一、`reply_to_message_id` 是否对
  齐，以及是否真的形成了通信闭环。

  ## 4. `wait`

  `wait` 当前只接受 `timeout_ms`，并且：
  - `timeout_ms` 必须大于 0
  - 实际等待时间会被 clamp 到 1 小时上限

  它的语义不是“查询某个特定 agent 的状态”，而是等待当前 agent inbox 里的下一条
  FIFO 消息。

  返回结果里重点看这些字段：
  - `timedOut`
  - `timeoutStreak`
  - `haltRequired`
  - `elapsedMs`
  - `satisfiedTargets`
  - `unsatisfiedTargets`
  - `messages`
  - `suggestion`

  其中：
  - 当前实现里 `messages` 每次只会返回 0 或 1 条 inbox 消息。
  - `satisfiedTargets` 会在成功收到消息时填入对应来源。
  - 当前实现里的 `unsatisfiedTargets` 通常为空。
  - 成功收到消息后，`timeoutStreak` 会被重置为 0。

  当前实现还包含一个重要规则：
  - 连续 5 次 timeout 后，`haltRequired=true`，并且 `suggestion` 会明确要求本 turn
    不要继续调用 `wait`，而是直接给用户发送说明。
  - 这是 runtime 返回的强提示；它不会物理禁止再次调用 `wait`，但自我进化流程中应把
    它视为“本 turn 停止继续 wait”的规则。
  - 因此在等待后台 TB2 测评任务时，不要做秒级轮询；应使用 3~5 分钟级别的 `wait`
    挂起，既减少空转，也避免快速累计 timeoutStreak。

  另外要区分两层语义：
  - `wait` handler 本身不会主动读取黑板或 agent 状态；它只等待 inbox。
  - 模型在决定 timeout 后要不要再次 `wait` 时，应该参考最新注入到上下文里的黑板内
    容和协作状态。

  ## 5. `read_agent_status`

  `read_agent_status` 用来读取目标 agent 的最近状态。

  输入字段：
  - `agent_id`

  这个 `agent_id` 可以是：
  - 已注册的 agent name
  - thread id

  返回内容包括：
  - lifecycle `status`
  - 最近最多 10 条 `recent_summaries`

  协调者和主 agent 应该用它判断：
  - 哪些 agent 还在推进
  - 哪些 agent 已经空闲
  - 哪些 agent 的摘要显示已经偏题或卡住

  ## 6. 环境感知与共享黑板

  Swarm 的环境感知主要不是靠额外工具，而是靠 prompt 注入完成的。

  在每次真正发起 sampling request 之前，`core/src/codex.rs` 都会向上下文尾部追加
  一个 synthetic `user/assistant` 对。注入内容包括：
  - 当前 UTC 时间
  - 协作中的 agent 列表
  - 每个 agent 最多 40 条摘要
  - 共享黑板快照

  assistant 侧会追加固定 ACK，目的是让模型把这块内容当作“已吸收的环境状态”，而不
  是需要显式回复的新对话。

  这意味着：
  - agent 不需要为了“了解最新黑板内容”而主动读取黑板文件
  - 黑板内容会在每次 API 调用时自动附加到上下文末尾
  - 特别是在一次 `wait` 超时、准备再次 `wait` 之前，要先参考这份新注入的黑板内容

  共享黑板本身由 `core/src/blackboard.rs` 和 `core/src/codex.rs` 负责初始化。

  文件路径是：

  ```text
  <workspace>/.blackboard/<owner_thread_id>.md
  ```

  锁文件路径是同一路径再追加 `.lock`。

  如果 session source 是 ThreadSpawn 子 agent，blackboard owner 会回退到父线程
  id，因此同一组 swarm agent 会共享同一个黑板文件。

  运行时还会向 developer instructions 注入：

  - agent 自己的名字
  - blackboard 文件路径
  - lock 文件路径
  - shell 环境变量中的 agent name
  - 标准写入格式 `[agent_name]：message_content`

  写黑板时必须通过 shell + 文件锁来做，不能无锁直接写入。

  关于 `required-reply` reminder，要注意它的真实触发条件：
  - reminder 只在 Swarm turn 中处理未完成的 `need_reply` obligation。
  - 它不是 `wait` 工具本身发出的，而是由主 sampling loop 在合适阶段注入。
  - root primary completion 路径可能会先清理一部分 unresolved obligation，因此不
    能把 reminder 视为“每次必定持续注入”的硬保证。

  ## 7. Debug 方式与文件位置

  当前有两套主要 debug 视图。

  第一套是每个 agent 独立的 model I/O trace：

  - 开启方式：`CODEX_DEBUG_MODEL_IO=1`
  - 可选目录覆盖：`CODEX_DEBUG_MODEL_IO_DIR`
  - 默认目录：`<codex_home>/debug/model-io/`

  主要产物：

  - `model-io-<thread_id>.md`
  - `ai-flow/model-io-<thread_id>.ai.jsonl`

  它适合看单个 agent 的请求、输入项、工具调用和响应时间线。

  第二套是整个 swarm 会话级别的 canonical debug trace：

  - 开启方式：`CODEX_DEBUG_TRACE=1`
  - 可选目录覆盖：`CODEX_DEBUG_TRACE_DIR`
  - 默认目录：`<codex_home>/debug/conversations/`

  每个根会话目录下至少会有：

  - `history.latest.json`
  - `metadata.json`

  在 Terminal Bench 2.0 跑题流程里，Wecode 原生 debug 产物通常保存在：

  ```text
  /work/terminal-bench/jobs/<JOB_NAME>/<task_slug>__<trial_id>/agent/wecode-home/debug/<root_thread_id>/
  ```

  分析跨 agent 协作时：

  - 以 `history.latest.json` 为主
  - `metadata.json` 只作辅助

  # 当前测试方法：Terminal Bench 2.0

  ## 项目根目录

  所有命令默认在下面这个目录执行：

  ```bash
  cd /work/terminal-bench
  ```

  核心入口脚本：

  - `/work/terminal-bench/run_tb2.sh`

  这个脚本是 `harbor run` 的薄封装，会把额外的任务过滤参数原样透传给 Harbor。

  ## 核心路径

  - Terminal Bench 根目录：`/work/terminal-bench`
  - 评测脚本：`/work/terminal-bench/run_tb2.sh`
  - 默认 job 根目录：`/work/terminal-bench/jobs`
  - 默认 wecode 二进制：`/work/tmp/bookworm-wecode-out/wecode-bookworm`
  - 正式自我更新测评编译必须产出到：`/work/tmp/bookworm-wecode-out/wecode-bookworm`
  - Wecode 鉴权：`/home/gradence/.wecode/auth.json`
  - Wecode 配置：`/home/gradence/.wecode/config.toml`

  ## 单题 pilot

  如果需要先验证命令、确认产物目录、或者快速看一条题目轨迹，先跑 1 题。

  推荐最小配置：

  - `N_CONCURRENT=1`
  - `N_ATTEMPTS=1`
  - `MAX_RETRIES=0`
  - `WECODE_SWARM_COMPLEX=1`（或 `CODEX_SWARM_COMPLEX=1`）

  精确指定单题时，优先使用：

  ```bash
  cd /work/terminal-bench
  JOB_NAME=wecode-tb2-pilot-<label>
  WECODE_SWARM_COMPLEX=1 \
  N_CONCURRENT=1 \
  N_ATTEMPTS=1 \
  MAX_RETRIES=0 \
  /work/terminal-bench/run_tb2.sh --task terminal-bench/<task-name>
  ```

  如果只知道 dataset 内部 slug，也可以使用：

  ```bash
  cd /work/terminal-bench
  JOB_NAME=wecode-tb2-pilot-<label>
  WECODE_SWARM_COMPLEX=1 \
  N_CONCURRENT=1 \
  N_ATTEMPTS=1 \
  MAX_RETRIES=0 \
  /work/terminal-bench/run_tb2.sh --include-task-name <task-slug> --n-tasks 1
  ```

  注意：
  - `--task terminal-bench/<task-name>` 是最稳妥的精确单题选择方式。
  - 如果外层环境已经统一设置 `WECODE_SWARM_COMPLEX=1` / `CODEX_SWARM_COMPLEX=1`，
    命令里不必重复写；但自我进化文档默认假设测试应走 `Swarm Complex` root prompt。

  ## 默认小批量回归

  进入稳定迭代后，默认一次跑 2 题，而不是大批量扫题。

  目标：
  - 降低单轮 token / 时间成本
  - 保持每题都能逐个看 trace
  - 方便把失败题与修复改动形成明确闭环

  推荐配置：

  ```bash
  cd /work/terminal-bench
  JOB_NAME=wecode-tb2-batch-<label>
  WECODE_SWARM_COMPLEX=1 \
  N_CONCURRENT=2 \
  N_ATTEMPTS=1 \
  MAX_RETRIES=0 \
  /work/terminal-bench/run_tb2.sh \
    --include-task-name <task-a> \
    --include-task-name <task-b> \
    --n-tasks 2
  ```

  如果要分别控制题目，也可以连续跑两个单题 job；只要保证每轮实际分析量维持在 2 题左
  右即可。

  ## 启动前检查

  每一次进行自我更新测试时，如果修改了原代码，先用下面这套 static musl 方案重新编译并发布
  `wecode`。这是正式测评前置步骤；不要用普通 `cargo build` 代替。

  说明：
  - 当前仓库示例挂载是 `/work/codex-rs:/src`；如果本轮实际改动的是别的 checkout，必须
    把这里替换成真实修改仓库路径。
  - 正式自我更新测评默认产物固定为 `/work/tmp/bookworm-wecode-out/wecode-bookworm`。
  - 如果当前有正在运行的 TB2 job，默认仍可直接覆盖同一路径产物；不需要专门为了避开
    活跃 job 而改用新的输出路径，除非本轮你自己明确想并行保留多份产物做对照。
  - 普通 `cargo build` 只可用于本地临时调试，不算完成正式自我更新测评编译步骤。
  - 正式自我更新测评产物必须是 `x86_64-unknown-linux-musl` 的静态 `wecode`。

  ```bash
  FORCE_REBUILD=1 /work/terminal-bench/code/scripts/build_wecode_tb2_static.sh
  ```

  至少保证下面这些条件成立：

  ```bash
  cd /work/terminal-bench
  test -x /work/terminal-bench/run_tb2.sh
  test -x /work/tmp/bookworm-wecode-out/wecode-bookworm
  test -f /home/gradence/.wecode/auth.json
  test -f /home/gradence/.wecode/config.toml
  ```

  正式自我更新测评默认直接使用上面产物路径；只有在刻意切换到另一份同样按这套 Docker
  方案编出来的产物时，才需要显式覆盖：

  ```bash
  WECODE_BINARY=/work/tmp/bookworm-wecode-out/wecode-bookworm
  ```

  ## detached session 启动要求

  正式启动 TB2 测评时必须使用 detached session，避免当前交互会话结束、中断或模型 turn
  结束时杀掉后台评测进程。不要只用普通后台 `&` 作为长期测评保护。

  推荐使用 `tmux`：

  ```bash
  cd /work/terminal-bench
  SESSION=tb2-<label>
  JOB_NAME=wecode-tb2-<label>

  tmux new-session -d -s "$SESSION" \
    "cd /work/terminal-bench && \
     JOB_NAME=$JOB_NAME \
     WECODE_SWARM_COMPLEX=1 \
     N_CONCURRENT=2 \
     N_ATTEMPTS=1 \
     MAX_RETRIES=0 \
     /work/terminal-bench/run_tb2.sh \
       --include-task-name <task-a> \
       --include-task-name <task-b> \
       --n-tasks 2 \
     > /work/terminal-bench/jobs/$JOB_NAME.detached.log 2>&1"
  ```

  启动后只做一次必要确认，例如 `tmux ls` 和检查 job 目录是否创建；随后用 3~5 分钟级别
  的 `wait` 挂起自己，不要高频轮询日志。

  如果本轮需要显式打开 `Swarm Complex` root prompt，可设置：

  ```bash
  export WECODE_SWARM_COMPLEX=1
  ```

  或：

  ```bash
  export CODEX_SWARM_COMPLEX=1
  ```

  如果 wecode 需要额外模型鉴权或网关环境变量，先在当前 shell 中设置好。

  ## 输出与排查位置

  每个 Harbor job 一个目录：

  ```text
  /work/terminal-bench/jobs/<JOB_NAME>/
  ```

  job 级汇总文件通常包括：

  - `config.json`
  - `job.log`
  - `job-log-index.json` / `job-log-index.tsv`：按题记录 `job.log` 行号；需要清理某题污染时先 dry-run：`/work/terminal-bench/code/scripts/prune_job_log_by_index.py /work/terminal-bench/jobs/<JOB_NAME> <task_name> --dry-run`
  - `result.json`

  每道题一个独立的 trial 目录：

  ```text
  /work/terminal-bench/jobs/<JOB_NAME>/<task_slug>__<trial_id>/
  ```

  单题 verdict 主要看：

  - `<trial>/verifier/reward.txt`
  - `<trial>/result.json` 中的 `verifier_result.rewards.reward`

  agent 结果与轨迹主要看：

  - `<trial>/agent/final.txt`
  - `<trial>/agent/wecode.jsonl`
  - `<trial>/agent/wecode.stderr.txt`
  - `<trial>/agent/trajectory.json`
  - `<trial>/agent/wecode-home/sessions/.../rollout-*.jsonl`
  - `<trial>/agent/wecode-home/debug/<root_thread_id>/history.latest.json`
  - `<trial>/agent/wecode-home/debug/<root_thread_id>/metadata.json`

  verifier 细节主要看：

  - `<trial>/verifier/test-stdout.txt`

  ## 逐题复盘顺序

  每题按下面顺序检查，不要跳步：

  1. 先看 `<trial>/agent/task-meta/instruction.md`，明确题目真实要求。
  2. 再看 `<trial>/agent/task-meta/task.toml`，确认 task 名称、类别、难度、超时设定。
  3. 再看 `<trial>/result.json` 和 `<trial>/verifier/reward.txt`，确认是否通过。
  4. 如未通过，再看 `<trial>/verifier/test-stdout.txt`，定位具体验证失败原因。
  5. 再看 `<trial>/agent/final.txt`，判断 agent 最终给了什么答复。
  6. 再看 `<trial>/agent/wecode-home/debug/<root_thread_id>/history.latest.json`，分析真
     实行为与协作质量。
  7. 最后才把 `<trial>/agent/wecode-home/debug/<root_thread_id>/metadata.json` 当作辅
     助参考。

  ## Terminal Bench 迁移期的特别提醒

  - 不要沿用 SWE-bench Pro 的批次脚本、Modal 评测、merge predictions、tmux shard
    等流程假设。
  - 现在的主入口是 Harbor job 目录，不再是旧的 `reports/pro_batches_*`、
    `bridge_runs_wecode_pro_*` 或 `debug_traces/<label>/<instance_id>/`。
  - 当前默认测试约定是 root 走 `Swarm Complex`，但实现层面它仍然是
    `ModeKind::Swarm`；不要把它误写成独立 runtime mode。
  - `Swarm Complex` 只影响 root prompt；spawn 出来的 sub-agent 仍然走
    `swarm_sub.md`。
  - 先确认失败是不是安全边界触发的拒答，再决定是否归因为协作问题或任务能力问题。
  - 如果一题只有单 agent 或双 agent 轨迹，就不要硬做 swarm 协作归因。
  - 每轮提出改动方案后，先获得人类许可，再改代码、改记忆文件，并重新跑题验证。
