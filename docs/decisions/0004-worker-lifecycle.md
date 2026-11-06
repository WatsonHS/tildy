# 0004：Worker 生命周期、调度与快照（协议 v1 后端侧）

- 日期：2026-10-19
- 状态：提议中

## 背景

M1 的常驻 worker 每个交互 Shell 一个，经 Zsh 启动时建立的管道通信（PLAN.md "后端生命周期""上下文与乱序结果""即时显示与后台状态"，`docs/protocol.md` "Shell 协议 v1""Rust 侧接口"）。约束：

- 退出可清理，崩溃可降级；父 Shell 消失后自行退出；重试有上限，队列有界。
- 丢弃旧结果是正确性保证：只能为最新请求渲染；worker 重启后不能接受旧 worker 的回包。
- 慢模块要区分 `pending`/`fresh`/`stale`/`timeout`/`error`，未完成的查询不能显示成 Git 干净；缓存过期后必须能显示为过期。
- 每个 provider 同时最多一个同类查询；子进程超时要回收。
- 并行开发期间 config、render、providers 由其他任务实现，后端核心必须能脱离它们单独测试。
- `src/model.rs` 冻结：`ProviderSpec` 只有 `key`、`deps`（`Cwd`/`Env`/`Time`）、`ttl`、`timeout`，契约没有定义 `Time` 与 TTL 的具体语义。

## 决定

worker 核心是一个单线程事件循环（`src/session/worker.rs`），输入线程和每次 provider 抓取的线程只往通道里送事件；调度器（`src/scheduler/`）是不做 IO 的状态机，只为最新请求渲染。启动即发 HELLO，以 stdin EOF 为主要退出信号、每 2 秒 `kill(pid, 0)` 检查父进程为第二道保险，每次发送 RES 后由后台线程原子写快照，正常退出时删除快照。

## 设计要点

### 结构与可测试性

- `session::worker::run_worker(input, output, make_providers, renderer, opts)` 与 `session::oneshot::run_oneshot(...)` 只依赖 `providers::Provider` trait 和 `session::PromptRenderer` trait；`src/cli/worker.rs`、`src/cli/render.rs` 负责接入 `providers::all()`、`config::*` 和 `render::render`。核心代码不调用 `config::*`。
- 状态全部在主线程：调度器、缓存、最新请求、stdout。"是否仍是最新请求"和"发送"在同一步完成，没有锁。
- provider 线程用 drop guard 回报结果：provider panic 时 guard 在栈展开中发出失败事件，对应 slot 变为 `error`，worker 不会一直等待一个已经死掉的线程。

### 启动、epoch 与 HELLO

- `worker_epoch` 取自 `/dev/urandom`（失败时用随机种子的 std hasher），范围 `[1, 2^53)`：0 留给一次性模式；低于 2^53 让 jq、JavaScript 读快照时不丢精度，也放得进 Zsh 的有符号整数。
- 第一件事就是写出并 flush `HELLO<TAB>1<TAB>binary_version<TAB>epoch<TAB>pid`，然后才启动输入线程、构造 provider，配置在第一个请求时才加载。握手不等任何初始化。
- 每个 RES 第 4 个字段是本进程的 epoch。Shell 应当只接受与当前 HELLO epoch 相同的 RES。管道本来就随 worker 更换，epoch 是额外的一层保护。

### 退出

| 触发 | 行为 |
| --- | --- |
| stdin EOF（含读错误） | 立即退出，不等待在途抓取 |
| `BYE` 行 | 立即退出 |
| 父进程消失 | `--parent-pid` 给出时每 2 秒 `kill(pid, 0)`；返回 ESRCH 即退出（EPERM 视为存活；pid 0 或超出 `pid_t` 的值不监视，避免误指进程组） |
| 写 stdout 失败（EPIPE） | 安静退出。Rust 运行时默认忽略 SIGPIPE，写入返回错误而不是杀进程 |

以上都以退出码 0 结束，并停止快照线程、删除快照文件。崩溃时快照保留，供事后查看。父进程检查放在事件循环的超时里，不另开线程，空闲时每 2 秒唤醒一次。

### 每个 REQ 的流程

1. 解析失败的行计入 `protocol_errors`，写 stderr 后跳过，worker 不退出；Shell 会对该代次超时。
2. 读到 REQ 时先把通道里已到的事件全部取出；如果其后还有合法 REQ，本条直接丢弃（`dropped_requests`）。有界队列：只服务最新请求。
3. 提交调度器 → 检查配置文件 mtime 与长度（每请求最多一次，变化则重新加载，无效时退回内置配置并记 `config_invalid`）→ 渲染并发送 `partial` → 再启动抓取线程。热路径先发 partial 再 spawn，省下几十微秒的线程创建时间。
4. 冷启动例外：如果某个不依赖 `Cwd` 的 provider（主机名、用户名、SSH）还没有任何缓存，先启动抓取并最多等 `fast_wait`（5 ms），这样第一个 partial 就带上 `user@host`，不会晚一帧才跳出来。依赖 `Cwd` 的 provider（Git）从不阻塞 partial。
5. 之后每个抓取结果若改变了最新请求的显示内容，就重新渲染并发送；所有 slot 都到达终态（`fresh`/`timeout`/`error`）时状态为 `fresh`，否则为 `partial`。与上一次发送完全相同（同一请求、同一状态、同一内容）的帧不重复发送。超时后迟到的结果仍会再发一个 `fresh`（后到覆盖先到）。
6. RES 的状态由 worker 决定，覆盖渲染器返回的 `RenderedPrompt.status`。

### 调度语义（对冻结契约的解释）

- 缓存键 = (provider 序号, 作用域, env 修订)。依赖 `Cwd` 时作用域为请求 cwd；某次 Git 结果报告了 `worktree_id` 后，该 cwd 改用 `worktree:<id>` 作用域，同一 worktree 的不同子目录共享条目，linked worktree 因 id 不同而互不共享。依赖 `Env` 时 env 修订进入键。cwd 或 env 变化不会复用别处的条目。
- 依赖 `Cwd` 或 `Time` 的 provider 视为"观察可变状态"（文件、时钟）：每个新请求都重新验证。缓存值立即显示，年龄小于 TTL 时标 `fresh`，超过 TTL 标 `stale`，同时调度新抓取。其他 provider 视为键的纯函数，TTL 内直接用缓存，不再抓取。
- 合并：同一 (provider, 键) 同时最多一个抓取。纯 provider 直接加入在途抓取；需要重新验证的 provider 记下一个"重跑"，等在途抓取结束后再启动。原因是在途抓取可能早于刚执行完的命令（例如 `git add` 之前）。同一 provider 最多 2 个并发（不同键，例如快速切换目录时旧仓库仍在查询），超出时只排队最新请求的那一个，旧的排队项丢弃。
- 外层期限 = `spec.timeout`（为 0 时用 2000 ms）+ 250 ms。超过即"放弃"该抓取：不再计入并发上限，最新请求的 slot 标 `timeout`，结果迟到时仍写入缓存。同一 provider 放弃的抓取达到 4 个仍未返回时，标记为 unresponsive（slot 为 `error`，快照 `degraded` 记 `provider_unresponsive:<名字>`），直到有抓取返回。子进程的 kill 由 provider 自己按 `spec.timeout` 完成（`Provider` trait 不暴露子进程）。
- 结果只按开始时间比较新旧：较早开始的抓取不会覆盖较晚开始的缓存条目。缓存最多 256 条，超出时淘汰最旧的。
- 诊断名：多个 provider 共用一个 `spec.key` 时（M1 的 ssh、hostname、username 都叫 `session`），快照里显示为 `session/ssh` 等。缓存按序号区分，不会互相串值。

### 帧安全

- REQ 必须正好 9 个字段；`gen`/`cols`/`lines`/`last_duration_ms` 只允许十进制数字（拒绝 `+1`、空格、溢出），`last_exit` 允许一个负号；`session_id` 非空、为 UTF-8、不含控制字符；`cwd` 非空；`cwd` 与 `env` 解码后不得含 NUL。`cwd` 与 `env` 只认 `\t` `\n` `\\` 三种转义，其他反斜杠序列与末尾孤立的反斜杠都是协议错误。
- 按字节解析（`protocol::parse_line(&[u8])`）：`cwd` 在 Unix 上按原始字节还原，非 UTF-8 路径不丢失；冻结签名 `parse_request(&str)` 只是包装。`env` 若含非法 UTF-8，转成带 `\xNN` 的文本修订（同时把反斜杠写成双反斜杠），不同输入不会合并成同一修订。
- 单行请求上限 64 KiB，超长行读到换行为止后丢弃并计入协议错误，内存占用有界。
- RES 每行去掉控制字符（C0、DEL、C1，含 TAB/LF/CR），每行最多 4000 字节（在字符边界截断），行尾落单的 `%`（不完整的提示符转义）去掉，`L` 最多 12 行，因此整帧远低于 Shell 的 64 KiB 上限。每帧至少一行 `L`（为空时发 `L<TAB>`）。

### 快照

- 路径：`$XDG_STATE_HOME/tildy/sessions/<id>.json`；`XDG_STATE_HOME` 未设置或不是绝对路径时用 `$HOME/.local/state/tildy/sessions/`。`--snapshot` 可覆盖。`<id>` 来自 Shell，`[A-Za-z0-9._-]` 以外的字符都换成 `_`，开头的 `.` 前面补 `_`，长度上限 128，不会穿越目录。
- 内容（`diagnostics::Snapshot`，`kind: "tildy.session_snapshot"`，`schema_version: 1`）：worker 版本、协议版本、epoch、pid、父 pid、启动时间；session_id、配置修订；最新请求及收到时间；最近一次 RES（gen、状态、发送时间、从收到请求到 flush 的耗时、行数、字节数）；每个 slot 的 freshness、age、错误、是否在抓取、作用域；每个 provider 的次数、错误数、最近一次/p50/最大耗时，以及调度统计（started、cache_hits、coalesced、timeouts、late 等）；计数器；降级原因。
- 写入：每次发送 RES 后，把快照交给后台线程，只保留最新一份。写临时文件 `.<name>.<pid>.tmp` 后 `rename`，文件 0600、目录 0700，内容含路径所以不公开。主循环不碰磁盘：慢盘或网络 home 目录不会拖慢提示符。
- 生命周期：正常退出时删除；首次确定快照目录时，后台删除超过 7 天未修改的 `*.json`/`*.tmp`，清理崩溃遗留的文件。`tildy inspect --session ID [--json]` 读取快照，并附带 `worker_alive`（`kill(pid, 0)`）；找不到快照时报 `backend_unavailable`（退出码 5）。

### 一次性模式（`tildy render`）

读一行 REQ（BYE 则静默退出 0）→ 请求合法后才构造 provider 并加载配置 → 所有 provider 并行，总期限 `TILDY_RENDER_TIMEOUT_MS`（默认 2000，非正整数时用默认值）→ 全部按时返回发 `fresh`，否则发 `partial` 并把缺失的 slot 标 `timeout` → epoch 填 0。协议错误：stderr 一行说明，stdout 为空，退出码 5。

### 对 Shell 的重启预期

- HELLO 的协议版本或二进制版本与 `TILDY_PROTOCOL_VERSION`/`TILDY_VERSION` 不符时，Shell 发 `BYE` 并重启；worker 收到 BYE 立即退出。
- worker 死亡（EOF）、超时或发出坏帧时，Shell 改走 `tildy render`；同一会话 60 秒内最多重启 3 次（`docs/protocol.md`）。worker 本身不重启自己，也不保留跨进程状态，新 worker 从空缓存开始，epoch 不同。
- 同一 gen 可能收到多个 RES：`partial`、可能更新的 `partial`、`fresh`，以及超时之后迟到结果带来的第二个 `fresh`。Shell 按"后到覆盖先到"处理即可。

## 证据

均在 2026-10-19、macOS（Darwin 27.0，10 核，同时有其他任务在编译）、rustc 1.95.0 上采集。

- 单元测试：`cargo test --lib protocol::`（13 项，含任务 E 的 `fixtures/protocol/wire-samples.txt`：REQ/BYE 可解析，RES 由各部分重新编码后逐字节一致）、`scheduler::`（17 项）、`session::`（8 项）、`diagnostics::`（6 项），全部通过。覆盖转义与非 UTF-8 往返、严格字段校验、帧大小上限、TTL 过期标 stale、cwd/env 变化不复用、worktree 共享、重跑合并、目录突发的有界调度、外层超时与迟到结果、放弃上限、乱序结果不回退、缓存上限、原子写与权限、写失败清理、后台写入只保留最新、快照路径净化。
- 集成测试（假 provider、假渲染器，进程内通过 OS 管道驱动 worker 核心）：`cargo test --test backend_worker`（15 项）、`--test backend_oneshot`（6 项）、`--test backend_cli`（5 项，真实二进制，只走不经过其他模块的路径：render 协议错误退出 5、BYE、worker HELLO、inspect JSON/文本、缺失会话）。连续 8 轮并行运行全部通过。
- 场景覆盖：HELLO 先于一切 → partial（Git `…`、主机名已就位）→ fresh；同目录第二个提示符 partial 用缓存、fresh 用重新验证的结果；旧代次结果在新 REQ 到达后从不发送；一次写入 20 个 REQ 只服务首尾两个，Git 至多 2 次；同键 5 个请求只抓取 2 次；外层期限约 70 ms 时发 `?`，迟到结果再发一个 fresh；provider panic 变为 `!` 且 worker 继续服务；坏行、8 字段、坏转义、70 KiB 超长行共 4 个协议错误被计数并跳过；快照内容逐字段核对，BYE 后被删除；父进程被杀后约 40 ms 轮询间隔内退出；输出端关闭时返回 `OutputClosed`。
- 延迟（假渲染器，进程内，`cargo test --release --test backend_worker -- --nocapture --test-threads=1 latency`，测量从写入 REQ 到读完 partial 的 `END` 行，三轮）：热请求 n=300，p50 63–69 µs，p95 205–288 µs，最大 1.8–2.1 ms；冷启动首个请求 75–104 µs；带快照写入时 p50 32–89 µs，p95 358–806 µs。同一台机器上不含任何逻辑的同构管道往返（管道 → 读线程 → 通道 → 主线程 → 管道）：p50 10–27 µs，p95 31–127 µs，最大 3.6–4.8 ms，说明毫秒级的最大值来自系统调度。
- 真实二进制（各任务模块均已落地后，外部 Python 驱动，本仓库目录，n=200，release）：spawn 到 HELLO 约 4 ms；冷启动 REQ 到 partial 0.26–0.35 ms；热请求 REQ 到 partial p50 0.085 ms，p95 0.125 ms，最大 0.2 ms；REQ 到含真实 Git 的 fresh p50 21.6 ms。debug 构建热请求 p50 0.145 ms。新编译的二进制首次执行时 spawn 到 HELLO 为 454 ms（macOS 首次执行检查），之后各次约 4 ms。
- 曾测得的问题与修正：快照最初在主循环里同步写，下一个请求要等上一次 `rename`，热请求 p95 从约 0.27 ms 升到约 2 ms；改为后台线程写之后恢复。

## 后果

- 正面影响：
  - 后端核心可以用假 provider 和假渲染器完整测试；真实模块落地后不用改动核心即可端到端运行。
  - 旧代次结果在结构上无法发送；突发请求、卡住的 provider、慢磁盘都只影响诊断，不影响 partial 的延迟。
  - 一次性模式与 worker 共用解析、组装与编码代码，输出格式一致。
- 负面影响与代价：
  - 每次抓取创建一个线程（几十微秒）；放弃的抓取线程会一直运行到 provider 返回，每个 provider 最多容忍 4 个。
  - worker 因 EOF 退出时，在途 provider 的子进程（如 `git status`）不会被 kill，只能自己跑完。`Provider` trait 不暴露子进程句柄。
  - `kill(pid, 0)` 对僵尸进程和 pid 复用都会返回存活。父进程检查只是第二道保险，EOF 仍是主信号。
  - provider 读取的是 worker 进程自己的环境变量。请求中的 `env` 目前只作为缓存修订，worker 启动后 Shell 里再 `export` 的值 provider 看不到。
- 需要后续处理的问题：
  - 快照格式需与任务 E 的 `schema/snapshot.schema.json` 对齐（M1 集成）。
  - `Dependency::Time` 与 TTL 的语义应写进契约，或在 `model.rs` 中用文档注释固定（M1 集成）。
  - `ProviderSpec.key` 应当唯一，或在契约中写明"相同 key 等于共享数据"；当前 M1 的 provider 有三个共用 `session`（任务 D）。
  - 若需要在 worker 退出时回收子进程，需要扩展 `Provider` trait（取消句柄或进程组），M2 评估。
