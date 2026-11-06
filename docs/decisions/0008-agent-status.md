# 0008：Agent 状态采用 OSC 7501 词汇、文件存储与 worker 推送

- 日期：2026-10-20
- 状态：提议中

## 背景

PLAN.md 把"Agent 状态显示在正确的会话或 worktree 的提示符里"列为 AI 原生卖点；M0 留下一份自造词汇的草案（`status: running|waiting_input|completed|failed`、`producer_id`、`task_id`、`summary`）。M2 开始时的事实与约束：

- Mitchell Hashimoto 于 2026-10-06 发布 Program Status Protocol（OSC 7501，规范 rev 0.4，2026-10-10，<https://www.superlogical.com/rex/docs/build/program-status>）。Claude Code 自 2.1.295 起主动发出 OSC 7501（作者当前 2.1.303），libghostty 已解析，cmux 的 Ghostty 分支已显示。7501 定义了状态（idle/working/done/blocked/error，clear 为操作）、kind、progress、app、层级 id、base64 的 title/msg、硬上限、"报告整体替换记录"、"进程退出或 OSC 133 提示符时丢弃 working/blocked"。
- OSC 序列只到终端；Shell 与 worker 看不到子进程的输出，所以需要自己的采集通道。
- 作者 2026-10-20 否决 Claude Code hooks（需要改 `~/.claude/settings.json`，与无需配置的理念冲突），改为 Zsh 适配层对已知 Agent 程序自动套用 `tildy agent wrap`（任务 D 的 pty 代理）。
- 每个 Shell 有自己的 worker（ADR 0004），worker 之间没有通信；发布者（wrap、CLI）是短命或独立的进程，可能在任意 pane、任意目录。
- `src/model.rs` 冻结：provider 一次 `fetch` 只返回一个 `ProviderValue`，key 是 `&'static str`；协议 v1 允许同一代次多帧 RES，Shell 用后到的覆盖先到的。
- 性能预算：REQ 到 partial p95 < 2 ms（`tests/backend_worker.rs`）。

## 决定

记录字段与 7501 一一对应（明文存储 title/msg），外加 `scope`（session/worktree/global）、`origin_session`、`source`、`seq`、`ttl_s`；记录存为 `$XDG_STATE_HOME/tildy/agent/<kind>/<scope-hash>/<id-hash>.json` 的独立文件，原子替换、按 scope 目录加锁。显示由 `agent_status` provider 读取请求的三个 scope；worker 每秒 `stat` 这三个目录并记住最早过期时刻，变化时重新提交当前请求并对同一代次推送一帧 RES，每个 REQ 先执行 7501 的提示符规则（`drop_on_prompt`）。

## 设计要点

### 词汇与校验

- 状态、kind、progress、app、id 语法、title ≤192 字节、msg ≤2048 字节、无控制字符（U+0000–001F、U+007F–009F）全部照搬 7501；超限整条拒绝。`kind` 只在 blocked、`progress` 只在 working/blocked 有意义，发布时在其他状态下丢弃而不是拒绝（7501 "used only with"），这样转发 7501 报告的采集器不会因为程序多写一个键而丢记录。Schema 对存储形态更严格（这两条用 `if/else` 表达）。
- `--osc` 接受原始报文体，按 7501 的宽松规则解析（跳过坏对、忽略未知键、重复键取最后、无效 kind/progress/app 视为缺省），整条拒绝的条件也照搬。任务 D 的流式解析器另行实现同一规则（`src/agent/osc.rs`）。
- app 继承：没有 `app` 的记录取同 scope 最近祖先记录的 `app`（`Stored::inherited_app`），只用于显示与按 app 过滤。

### 作用域解析

- `session` 的 id 是 Zsh 适配层导出的 `TILDY_SESSION_ID`（`$$.$RANDOM$RANDOM`，与 REQ 的 session 字段相同）。
- `worktree` 的 id 与 `GitData.worktree_id` 完全相同：`providers::git::worktree_identity(cwd)` 复用 git provider 的 `rev-parse --git-common-dir --show-toplevel` 与规范化。同一仓库的不同 linked worktree 是不同的 scope；同一 worktree 的子目录是同一个 scope。不用目录前缀匹配。
- 发布默认：cwd 在 worktree 内用 worktree，否则 global——Agent 通常在某个仓库里工作，同仓库其他 pane 最需要看到它。
- provider 按 cwd 缓存 worktree 身份 10 s，cwd 的祖先没有 `.git` 时不启动 git；worker 的监视只用缓存（`peek`），从不在 worker 线程上启动进程。

### 生命周期

- TTL 用墙钟（`received_at_ms + ttl_s`）：发布者与读者是不同进程，没有共享的单调时钟；缺省 TTL 按状态（活动状态 2 h，done/error 15 min）保证发布者崩溃后记录也会消失。
- 7501 的 OSC 133 规则落在 `origin_session` 上：该会话出现新提示符（worker 收到 REQ）时删除 idle/working/blocked；done/error 第一次只标记 `prompt_seen`、第二次删除，所以发布它的 pane 至少看到一次。`drop_on_prompt` 在 partial 发出之后、provider 抓取之前同步执行，不影响 REQ→partial。
- `seq`：同 scope 同 id 只接受比现存未过期记录更大的 seq；缺省为上一条 +1。在 scope 目录锁内读—比较—写，并发发布者不会让 seq 倒退。过期记录不再约束 seq（重启的发布者可以从 1 开始）。

### 存储

- 一条记录一个文件：发布是"写临时文件 + rename"，读者（provider、CLI）不加锁也不会读到半个文件；按 id 寻址无需索引；目录 mtime 天然就是"这个 scope 变了"的信号。写者对 scope 目录 `flock`，跨进程、跨线程都成立。
- 文件名用 FNV-1a 64（与配置修订同一函数）：scope id 是任意路径、session id 来自 Shell，不能直接做文件名；读取时校验文件内容的 id 与文件名一致，冲突或手工复制的文件不显示。
- 每个 scope 最多 256 条（7501 的每终端上限），满了删最久未更新的；`expire` 删除过期记录、损坏文件、遗留临时文件和闲置一小时以上的空目录，worker 每小时在后台执行一次。

### 推送

- 监视三个目录的 mtime 而不是用 kqueue/inotify：每秒三个 `stat` 的成本可以忽略，可移植，且与 worker 已有的定时器循环（parent poll、调度器截止时间）合在一起，不需要新线程。
- 变化时先在 worker 线程上读取这三个 scope 的记录、算出模块数据，与当前显示的数据比较（忽略每秒都变的 `age_s`），相同就不做任何事；不同才把当前请求重新提交给调度器。重新提交复用调度器的全部语义（缓存、合并、超时），代价是重新验证依赖 cwd 的 provider（例如再跑一次 `git status`），这只在 Agent 状态真的变化时发生，每秒最多一次。这一轮只发送"全部槽结束"的结果，而且与上一帧相同就不发，避免 partial 闪动。
- provider 的 key 是模块 id（与其他配置模块一致），因为 `Provider::fetch` 只能返回一个值；多个 `agent_status` 模块各一个实例、读同一份数据。`Provider` trait 增加带默认实现的 `as_agent_status()`，worker 借此找到需要监视的 provider，测试替身无需改动。

### 采集通道

1. `tildy agent wrap`（任务 D）：pty 代理原样转发字节并解析 OSC 7501，`source: osc`。Zsh 适配层为 `agent.programs`（默认 claude、codex、gemini、aider、opencode、amp、goose、copilot、cursor-agent）中存在于 PATH、且未被用户定义为函数/别名/内建/保留字的程序定义同名函数，只在 stdin/stdout 是终端时经过代理；`agent.wrap: false`、`TILDY_AGENT_WRAP=0`、`TILDY_AGENT_PROGRAMS` 可以关闭或调整，`unfunction` 可以去掉单个包装。
2. `tildy agent publish`：标志、`--file`（Schema 格式）或 `--osc`（原始 7501 报文体），`source: cli`。

### 不做的事

- 不执行命令：provider 只读文件；唯一的子进程是按 cwd 缓存的 `git rev-parse`（与 git provider 相同）。
- 不调度、不控制 Agent：Tildy 只显示状态，不发送输入、不批准权限、不重启程序。
- 不走网络：记录只在本机状态目录里；没有服务端、没有跨机器同步。
- 不读终端：worker 与 provider 看不到任何终端内容；`tildy agent wrap` 只在自己代理的字节流里识别 OSC 7501 序列，其余字节原样转发、不记录（`docs/agent-wrap.md`）。
- 不改用户的其他程序配置（不写 `~/.claude/settings.json` 等）。

## 证据

- 规范：OSC 7501 rev 0.4 原文（上述链接，2026-10-20 读取），本 ADR 的上限、宽松规则与生命周期均出自其中的 "Encoding and limits"、"States"、"Keys" 小节。
- 推送延迟（`python3 tests/pty/run.py --scenario 'agent_*' --plugins none`，2026-10-20，本机 macOS，真实二进制 debug 构建）：发布后 0.47 s 出现在空闲提示符右侧，TTL 3 s 的记录在发布后 3.48 s 消失；同 worktree 记录 0.76 s 出现；另一 worktree、另一会话的记录 2 s 内不出现。
- worker 内的端到端测试：`src/session/worker.rs` 的 `agent_status_push_expiry_and_prompt_rule`（轮询 50 ms：推送在同一代次、TTL 到期后消失、另一会话不触发、working 在下一个 REQ 消失、done 显示一次后消失）；`tests/backend_worker.rs` 的 REQ→partial p95 < 2 ms 测试保持通过（drop 与基线在 partial 之后执行）。
- 并发：`src/agent/store.rs` 的 `concurrent_publishers_keep_seq_monotonic`（8 线程各发布 10 次自动 seq，最终 seq 为 80）。
- 被排除的方案：
  - 单文件数据库（一个 JSON 或 SQLite）：所有发布者争同一把锁、读者要解析全部记录，且失去"目录 mtime = scope 变化"的信号；SQLite 还要新依赖。
  - inotify/kqueue：平台相关、需要新线程或新依赖，收益只是把 ≤1 s 的延迟再缩短，不值得。
  - worker 之间直接通信（socket）：需要发现与认证机制，发布者是独立进程时仍要落盘。
  - Claude Code hooks：作者否决（需要改用户的 `~/.claude/settings.json`）。

## 后果

- 正面影响：任何支持 7501 的程序零配置可见；发布者可以是任何语言写的脚本（CLI 或 `--file`）；记录格式与 7501 同构，将来终端或其他工具可以直接消费；不执行命令、不调度 Agent、不走网络、不读终端内容（wrap 只解析 7501 序列，见 `docs/agent-wrap.md`）。
- 负面影响与代价：
  - 显示延迟最多约 1 s（轮询间隔）加一次重新验证的时间。
  - 每次推送会重新验证依赖 cwd 的 provider（例如 `git status`），包括 Shell 正在前台运行命令、推送帧会被当作过期代次丢弃的时候。
  - TTL 依赖墙钟，系统时间大幅跳变会让记录提前或推迟过期。
  - 每个 REQ 在 partial 之后扫描一遍存储目录（`drop_on_prompt`），记录很多时会推迟 provider 抓取的开始。
  - 测试用的 `WorkerOptions::default()` 会打开进程环境里的存储（只读扫描，只删除 origin 为测试会话 id 的记录）。
- 需要后续处理的问题：
  - Nerd 字形：计划文档中 blocked/error/done/idle 的 Nerd 字形在写入时丢失（同 M1b 的情况），本实现选用 `U+F059`、`U+F057`、`U+F058`、`U+F04B2`，working 保留计划里的 `U+F051F`；作者可在日用中调整默认值。
  - 预设没有放 `agent_status` 模块（计划要求预设不改）；是否给 powerline 预设加一个，待作者决定。
  - Shell 前台运行命令时 worker 仍会为 Agent 变化重新验证 provider；如成本可观，可在协议里加一个"Shell 忙"的提示（M3）。
