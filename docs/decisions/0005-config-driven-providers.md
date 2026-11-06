# 0005：配置驱动的 provider、语言版本检测与缓存

- 日期：2026-10-19
- 状态：提议中

## 背景

M1b 要"大部分替代 Starship"（`docs/modules.md`）：提示符要显示 node、rust、go、python（含 venv/conda 名）、java、c 的版本和 docker 上下文，其余语言经 Starship 桥接。约束与事实：

- 版本命令本身很慢。本机（macOS，10 核，测量时负载 6.5–8）直接执行 15 次的中位数：`node --version` 23.1 ms，`rustc --version` 36.0 ms，`java -Xinternalversion` 30.4 ms（冷启动时 46–56 ms），`go version` 14.2 ms，`cc --version` 19.9 ms，`python3 --version` 7.1 ms。计划要求版本命令不在任何关键路径上执行，热请求 REQ 到 partial 仍需 p95 < 2 ms。
- M1 的 worker 在启动时固定使用 `providers::all()`（session 三个 + git），配置变化不影响 provider；三个 session provider 共用 key `session`，调度器缓存按 provider 序号区分（0004 中记录的变通做法）。
- `src/model.rs` 冻结：`ProviderSpec.key` 是 `&'static str`；模块数据走 `ProviderValue::Module(id, Slot<ModuleValue>)` 与 `PromptData.modules`；未检测到的模块是 `Slot { Fresh, value: None }`。
- 协议 v1 的 REQ 原为 9 个字段，`env` 是按位置拼接的值。M1b 需要后台任务数 `jobs` 和按名字取值的 env（`VIRTUAL_ENV`、`CONDA_DEFAULT_ENV`、`DOCKER_CONTEXT`、`DOCKER_HOST`）。
- worker 进程的环境变量是它启动那一刻 Shell 的环境；之后 Shell 里 `export`、`nvm use`、激活 venv 的结果 worker 看不到，只能通过 REQ 的 `env` 传入。
- 并行开发：任务 A 定义配置字段与渲染（`fields.version` 不带 `v`，由渲染器加前缀），任务 D 实现 Starship 桥接 provider。

## 决定

provider 集合由配置决定：`providers::from_config` 返回固定的 session/git provider，加上布局中每个 provider 类型模块（`language`、`command`、`python`、`docker_context`、`starship`）各一个实例，provider key 就是模块 ID；配置修订变化时 worker 重建 provider 集合，按 key 保留调度状态。语言版本由 provider 先按当前目录的直接条目检测、命中后才执行命令，命令结果进入一个进程级缓存（键为 cwd、argv、命中的检测文件 mtime 签名，默认 TTL 600 秒、超时 500 毫秒，同 cwd 同 argv 并发只执行一次），而且模块抓取总是在该请求的 partial 发出之后才启动。

## 设计要点

### 协议

- REQ 接受 9 或 10 个字段。第 10 个字段 `jobs` 是十进制数字（u32，拒绝空串、符号、溢出）；9 个字段等价于 `jobs = 0`。`protocol::encode_request` 总是写出 10 个字段（与 M1b 的 Shell 一致），`fixtures/protocol/wire-samples.txt` 里 9 字段的 REQ 仍可解析，重新编码后等于原行加 `\t0`。
- `env` 是以 U+001F 连接的 `KEY=VALUE` 对，整个字段按原规则反转义（`\\`、`\t`、`\n`）后再由 `PromptRequest::env_map()` 拆分；值里可以有 `=`（按第一个 `=` 分割）。M1 的位置式 env 仍被接受，`env_map()` 对它返回空表。整个解码后的字符串仍是 env 缓存修订。`protocol::encode_env` 是测试与工具用的编码器。

### provider 集合与 key

- 固定 provider 的 key 改为互不相同：`session/ssh`、`session/hostname`、`session/username`、`git`。调度器缓存键改为 (provider key, 作用域, env 修订)，不再用序号；0004 中"共用 key 时按序号区分、诊断名显示为 `session/ssh`"的变通做法删除，快照里的 provider 名就是 key。
- 模块 provider 的 key 是模块 ID。模块 ID 只能是 `[A-Za-z0-9_-]`（配置校验保证），所以不会与 `session/…` 冲突；只可能与 `git` 冲突，此时跳过该模块并报告问题。worker 核心收到重复 key 时保留第一个并在 stderr 警告。
- `ProviderSpec.key` 必须是 `&'static str`：模块 ID 经 `providers::intern` 驻留，每个不同的 ID 只泄漏一次，配置重载不会重复泄漏。
- `from_config` 只为 `Config::layout_modules()` 里出现的模块建 provider（没放进布局的模块不会显示，不该跑命令）；字段从合并后的原始 JSON `config.json["modules"][id]` 读取：`language`、`module`、`detect_files`、`detect_extensions`、`detect_folders`、`argv`、`regex`、`timeout_ms`、`cache_ttl_s`。未知语言、无效正则、key 冲突只让该模块没有数据，问题字符串返回给调用方，worker 写 stderr 并在快照 `degraded` 里记 `module_invalid: …`。
- 无值的 slot 按 key 映射（`providers::slot_target`）：`git` → `PromptData.git`，`session`/`session/…` → 忽略，其余 → `PromptData.modules[key]`。这样模块在第一次抓取完成前是 `pending`，外层超时时是 `timeout`，不会被读成"未检测到"。M1 的 `is_git_key`（前缀匹配 `git`）删除：名为 `github` 的模块不再被当成 Git。

### 检测规则

- 每次抓取只读取 cwd 的直接条目，最多 500 项（超出部分不看，`DirScan.truncated` 标记）。命中条件：条目名在 `files` 中；扩展名（最后一个点之后，点开头的文件名没有扩展名）在 `extensions` 中；或者是名字在 `folders` 中的目录（符号链接按目标判断）。目录不可读视为未检测到。三个列表都为空的 `command` 模块在每个目录都运行。
- 内置表（`providers::modules::LANGUAGES`）与计划一致：nodejs、rust、golang、java、c、python 的检测文件与扩展名、版本命令。语言模块上的 `detect_*` 字段按类别替换内置列表（`[]` 关闭该类检测），缺省的类别保留内置值。
- python：检测命中，或请求 env 中 `VIRTUAL_ENV` 非空（Starship 1.24.2 实测在没有 Python 文件的目录里也显示已激活的 venv），都算检测到。设置了 `VIRTUAL_ENV` 时先执行 `$VIRTUAL_ENV/bin/python --version`（worker 的 PATH 里没有 venv），找不到可执行文件时依次退回 `python3`、`python`。`fields.env` 取 `pyvenv.cfg` 的 `prompt`（去掉引号与括号），否则 `VIRTUAL_ENV` 的 basename，否则 `CONDA_DEFAULT_ENV`。版本拿不到但有环境名时仍显示环境名（slot 带 error 说明）。
- docker_context：只看检测文件（Dockerfile、docker-compose.yml/yaml、compose.yml/yaml、.dockerenv），不执行命令。来源依次为请求 env 的 `DOCKER_HOST`、`DOCKER_CONTEXT`，再是 `$DOCKER_CONFIG/config.json`（否则 `~/.docker/config.json`，worker 环境）的 `currentContext`。`default`、`desktop-linux`、`unix://` 开头的值与空值隐藏（value 为 None）。与计划的差异见"后果"。
- starship：把模块的 `detect_*` 列表交给任务 D 的 `StarshipProvider::with_detect`，未检测到时不启动 `starship`。

### 版本提取

- 带正则时取第一个参与匹配的捕获组（没有捕获组则取整个匹配），先在 stdout 找，找不到再找 stderr（Python 2 的 `--version` 写 stderr）；不带正则（`command` 未给 `regex`）时取 stdout 第一个非空行，输出为空则隐藏模块。只接受退出码 0 的结果。
- 内置正则：nodejs `v?(\d+\.\d+\.\d+)`，rust `rustc (\S+)`，golang `go(\S+)`，c 为第一行中的第一个版本号 `\A[^\n]*?(\d+\.\d+(?:\.\d+)*)`，python `Python (\S+)`，java `(?:JRE.*?\(|OpenJ9 )(\d+(?:\.\d+){0,2})`（与计划不同，见"后果"）。
- 输出：`ModuleValue.text` 与 `fields.version` 都是原始版本（如 `24.14.0`，不带 `v`）；python 另有 `fields.env`，docker 为 `text` 与 `fields.context`。
- 失败都是 slot 而不是 panic：超时 → `Timeout`（"`node --version` timed out after 500 ms"），可执行文件不存在、非零退出、输出里没有版本 → `Error`，消息写入 `slot.error`。子进程在自己的进程组里运行，超时时整组 SIGKILL（与 Git 共用 `providers::exec::run`），每个输出流最多保留 64 KiB。

### 缓存键与两层缓存

1. 调度器层（0004 的语义不变）：模块 provider 依赖 `Cwd`（python、docker_context 另依赖 `Env`），属于"每个请求都重新验证"的 provider。缓存值立即显示，年龄小于 TTL 为 `fresh`、超过为 `stale`，同时调度一次抓取。所以每个提示符都会为每个模块起一次抓取线程。
2. 命令层（`providers::modules`，进程级）：键 = (cwd, argv, 签名)。签名是"按名字命中的检测文件与目录"及其 mtime 的哈希：新建、删除或修改 `.nvmrc`、`.python-version`、`package.json` 等会换键，触发重新执行；按扩展名命中的源文件不进签名（源文件不决定工具链版本）。条目在模块 TTL（默认 600 秒，`cache_ttl_s` 可改）内有效；失败结果（超时、找不到、非零退出、无版本）最多保留 30 秒（`FAILURE_TTL`），以免一次失败遮住十分钟。最多 512 条，超出淘汰最旧的。
- 合并：同一 (cwd, argv) 正在执行时，其他抓取等待它的结果（最多等自己的超时加 100 ms），不再启动第二个进程；只有签名相同时等待者的结果才写进缓存，签名不同的等待者下次重新执行。键里不含模块 ID（与计划不同）：缓存的是原始输出，各模块各自解析，同一命令的多个模块可以共享。
- 结果：同一目录的后续提示符里，模块抓取 = 读目录 + 几次 stat + 一次哈希表查找，release 下约 0.02 ms，不起进程。

### 为什么版本永远不在关键路径上

- 热请求：worker 先渲染并写出 partial，再启动所有抓取线程（0004 已有）。版本命令只可能在 partial 之后开始。
- 冷启动：原来所有抓取先启动、再最多等 5 ms 让 session provider 返回；现在冷启动只先启动 session provider 与 git，等待、发出 partial 之后才启动其余依赖 `Cwd` 的 provider（即全部模块）。规则写在 `session::worker::starts_after_partial`。
- 版本结果到达后只引起一次重新渲染与一帧 RES（内容不变的帧不重复发送）。命令卡住时由 provider 的超时和调度器的外层期限（超时 + 250 ms）截断，不影响下一个请求的 partial。
- 测试 `module_fetch_never_delays_the_cold_partial` 与 `config_providers_fill_modules_after_the_partial_and_rebuild_on_reload` 用一个记录"抓取开始时该请求是否已经渲染过"的假模块断言这一点（违反次数为 0）。

### 配置重载

- `PromptRenderer` 新增 `config_epoch()` 与 `providers()`。`ConfigRenderer` 在生效配置的修订（用户文件内容哈希，或 `builtin`）变化时把 epoch 加一；worker 每个请求先 `refresh()`（原来在 submit 之后，现在提前到 submit 之前，代价是一次 stat），epoch 变化时取新 provider 集合并调用 `Scheduler::reconfigure`。
- `reconfigure` 按 key 匹配：保留 key 的 provider 保留在途抓取、放弃计数、worktree 映射与统计；在途抓取完成时按抓取 ID 找到它的新位置。缓存条目只在 key 保留、spec 不变时保留；模块 provider 的条目在配置变化后丢弃（同一模块 ID 可能已经换了命令），除非新集合里是同一个 provider 实例。丢弃的条目很快由命令层缓存补上，不会重新执行命令。session 与 git 的缓存跨重载保留。
- 一次性模式（`tildy render`）先在 REQ 合法后加载配置，再用配置的 provider 集合；测试与外部调用方没有提供 `providers()` 时仍使用传入的 `make_providers`。

### Git 新字段

- `stashed`：读公共 git 目录下的 `logs/refs/stash` 并数行数，等于 `git rev-list --walk-reflogs --count refs/stash`（`git stash drop` 会重写该文件）；文件不存在为 0；只有 reftable 仓库（没有 reflog 文件）才在同一期限内执行该 `rev-list` 命令。不增加 Git 进程。
- `renamed`：porcelain v2 的 `2` 记录数（重命名与复制）。`deleted`：`1`/`2` 记录中 X 或 Y 为 `D` 的条目数（每个条目只计一次；冲突记录 `u` 不计）。
- `operation` 与 `operation_progress`：只检查该 worktree 自己的 git 目录（`--git-dir`，linked worktree 与主 worktree 不同）里的标记文件，不执行命令。`rebase-merge/` → rebase，进度取 `msgnum`/`end`；`rebase-apply/` → 有 `applying` 时为 am，否则 rebase，进度取 `next`/`last`；`MERGE_HEAD` → merge；`CHERRY_PICK_HEAD` → cherry-pick；`REVERT_HEAD` → revert；`BISECT_LOG` → bisect。同时存在时按 rebase > merge > cherry-pick > revert > bisect（Starship 的顺序）。

### 一次性模式

`tildy render` 没有跨进程缓存：配置里的模块命令在这里按各自超时与 git 并行执行，受总期限（默认 2000 ms）约束。它是 worker 不可用时的回退路径，延迟约等于最慢的版本命令。

## 证据

均在 2026-10-19、macOS（Darwin 27.0，10 核，同时有其他任务编译，负载 6.5–8）、rustc 1.95.0 上采集。

- 单元测试：`cargo test --lib`（138 项全部通过，含其他任务的模块；本任务相关的是 `protocol::`、`scheduler::`、`session::`、`providers::{exec,git,modules}::`）。覆盖 10 字段 REQ 与 `jobs` 的严格解析、env 键值对与旧格式、调度器按 key 的 `reconfigure`（在途抓取跨重载完成、spec 变化丢弃缓存）、模块 slot 的 pending/fresh/stale/timeout 合并、内置正则对真实输出（含 `OpenJDK 64-Bit Server VM (19.0.2+7-44) … JRE (19.0.2+7-44)`、Java 8、Apple clang、Ubuntu gcc）、检测规则与签名、500 项上限、docker 上下文来源与隐藏值、venv 名、Git 操作标记的优先级与进度。
- 集成测试：`cargo test --test providers_modules`（14 项：`from_config` 只为布局中的模块建 provider 且按布局顺序、问题报告、key 冲突、`detect_*` 覆盖、pastel-powerline 预设、检测前不执行命令、缓存命中、检测文件 mtime 变化重新执行、TTL 过期、4 个并发查询只起 1 个进程、超时后进程组被杀、非零退出、无版本、找不到可执行文件、venv 解释器与环境名、docker）；`--test providers_git`（5 项，17 个场景含新增的 `deleted`、`stash`、`merge_in_progress`、`rebase_in_progress`，以及 stash 计数与 `git rev-list --walk-reflogs --count refs/stash` 一致、linked worktree 的操作标记互不影响）；`--test backend_worker`（18 项）、`--test backend_oneshot`（6 项）、`--test backend_cli`（5 项）。全部通过。
- 查询代价（`cargo test --release --test providers_modules -- --nocapture measure`，含 package.json、Cargo.toml、go.mod、pom.xml、main.c、setup.py 的临时目录）：首次（执行命令）nodejs 22.4 ms、rust 43.8 ms、golang 35.1 ms、java 55.4 ms、c 33.5 ms、python 16.6 ms；之后的缓存命中 p50 0.022–0.025 ms（50 次，最大 0.23 ms）；没有匹配文件的目录 0.020–0.023 ms。
- 进程内延迟（`cargo test --release --test backend_worker -- --nocapture --test-threads=1 latency`，三轮）：真实 provider（session、git、六种语言、docker_context）在含 package.json 与 Cargo.toml 的临时 git 仓库，冷启动 partial 91–129 µs，冷启动到 fresh（含首次 `node`、`rustc`）40.9–62.8 ms；热请求 n=150，REQ 到 partial p50 54–56 µs、p95 87–102 µs、最大 118–470 µs；REQ 到 fresh p50 21.0–21.6 ms（由 git status 决定）。假 provider 的原有基准：热请求 p50 33–94 µs、p95 84–573 µs，与 0004 记录的范围一致。
- 真实二进制（debug 构建，`TILDY_CONFIG` 指向只含 node、rust 两个 `language` 模块的配置，临时 git 仓库含 package.json 与 Cargo.toml）：`printf 'REQ\t1\ts\t<dir>\t100\t30\t0\t0\tUSER=x\t0\n' | target/debug/tildy render` 输出含 `v24.14.0` 与 `v1.95.0` 的 `fresh` 帧，二次运行总耗时 0.05 s。`tildy worker` 由 Python 驱动：冷启动 partial 3.0 ms（模块为 `…`），66.7 ms 时 node 版本到达，81.4 ms 时 fresh；之后每个请求 partial 0.22 ms，且已带两个版本；快照 slots 依次为 `session/ssh`、`session/hostname`、`session/username`、`git`、`node`、`rust`，模块抓取（缓存命中）0.3–0.6 ms。
- Starship 行为对照（Starship 1.24.2，`starship module …`）：docker_context 对 `desktop-linux`、`default`、`unix://…` 输出为空，`DOCKER_HOST` 与 `DOCKER_CONTEXT` 同时设置时显示 `DOCKER_HOST`，没有检测文件时不显示；python 在只有 `VIRTUAL_ENV` 的目录显示 `v3.12.7 (myenv)`，`CONDA_DEFAULT_ENV` 不进入 python 模块；c、java、nodejs、rust、golang 分别显示 `v21.0.0`、`v19.0.2`、`v24.14.0`、`v1.95.0`、`v1.26.5`，与本实现提取的版本一致。

## 后果

- 正面影响：
  - 配置决定 provider：预设或用户配置里放进布局的语言模块即有数据，改配置后下一个提示符生效，session 与 git 的缓存不受影响。
  - 版本命令每个 (cwd, 工具链文件状态) 最多每 10 分钟执行一次，热路径不起进程；partial 延迟与 M1 相同。
  - provider key 唯一后，缓存、快照、诊断名都按 key，配置重载可以按 key 迁移状态。
- 负面影响与代价：
  - 每个提示符为每个模块起一个抓取线程（读目录 + stat，约 0.02 ms 加几十微秒的线程创建）。pastel-powerline 预设有 12 个模块 provider（5 个 `language`、python、docker_context 与 5 个 Starship 桥接）。
  - worker 用自己的 PATH：Shell 里 `nvm use`、`fnm`、修改 PATH 的版本切换不会反映到版本上，直到 worker 重启；按 cwd 解析的 shim（rustup 的 `rust-toolchain.toml`、asdf/mise）因为命令在 cwd 执行而有效。python 通过 `VIRTUAL_ENV` 绕过；conda 只有环境名，版本来自 worker PATH 里的 python。
  - 未在检测列表里的工具链文件（如 `rust-toolchain.toml`）变化后，版本最多滞后一个 TTL。
  - 一次性模式没有缓存，延迟约等于最慢的版本命令。
  - 与计划的差异：Java 正则改用 JRE/OpenJ9 锚点（计划的"第一个版本串"在真实输出里会取到 `64-Bit` 的 `64`）；docker 来源顺序按 Starship 实测为 `DOCKER_HOST` 优先，并额外隐藏 `desktop-linux` 与 `unix://`（作者本机 `currentContext` 是 `desktop-linux`，Starship 不显示）；python 在只有 venv 时也显示；命令缓存键不含模块 ID。
- 需要后续处理的问题：
  - `schema/snapshot.schema.json` 的 `request` 与 `schema/shell-frame.v1.schema.json` 的 `req` 都是 `additionalProperties: false` 且没有 `jobs`；快照现在会写出 `request.jobs`（`PromptRequest` 序列化），帧 Schema 也缺 `jobs` 与 env 键值对的描述（M1b 集成）。
  - 任务 D 的 `StarshipProvider::with_binary` 每个实例 `Box::leak` 一次 key，配置每重载一次泄漏一次；可改用 `providers::intern`（任务 D）。
  - 任务 A 的校验只允许 `command`/`language`/`python`/`docker_context` 带 `detect_*`，`starship` 模块的检测列表因此到不了 provider，预设里的 elixir、elm、haskell、julia、nim 桥接在每个新目录各起一个 `starship` 进程（计划修订记录中"集成阶段补"）。
  - 0004 中"REQ 必须正好 9 个字段""缓存键含 provider 序号""共用 key 的诊断名"三处已被本记录取代。
