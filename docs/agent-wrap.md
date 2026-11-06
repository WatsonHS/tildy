# tildy agent wrap：从终端输出采集 OSC 7501

日期：2026-10-20。任务 D（`docs/architecture.md`）。实现：`src/agent/osc.rs`（解析器）、`src/cli/wrap.rs`（pty 代理）；测试：`tests/agent_wrap.rs` 与两个文件内的单元测试。记录格式与存储见 `docs/agent-status.md`。

## 做什么

```sh
tildy agent wrap -- claude              # 配置 agent.wrap=true 后 Zsh 适配层对已知 Agent 程序就这样运行（默认关闭）
tildy agent wrap -- codex
tildy agent wrap --app build -- cargo build --release
tildy agent wrap --scope global -- brew upgrade
```

`wrap` 在一个新的 pty 里运行命令，把它的输出**逐字节原样**写到真实终端，把键盘输入原样转给它；同时扫描输出流里的 OSC 7501（Program Status Protocol rev 0.4）报告，写成 agent 状态记录，让**其他 pane、其他 worktree 的提示符**也能看到"claude 在等你批准"。终端自己对 7501 的显示（Ghostty/cmux 的角标）不受影响，因为序列照样到达终端。

| 选项 | 说明 |
| --- | --- |
| `--app NAME` | 记录的 `app` 与 id 前缀；缺省取命令的 basename。不合 `[A-Za-z0-9_.+-]{1,32}` 的字符替换为 `_`，截到 32 字节 |
| `--scope session\|worktree\|global` | 显示范围；缺省：当前目录在 git worktree 里用 worktree，否则 global |
| `--scope-id ID` | 范围 id；`session` 缺省取 `TILDY_SESSION_ID`，`worktree` 缺省取当前目录的 worktree 身份 |

退出码就是命令自己的退出码；被信号 N 杀死时为 128+N。

## 记录

| 时机 | 写入 |
| --- | --- |
| 命令启动 | 根记录 `wrap/<app>/<pid>`：`working`，无 msg（不发 7501 的程序也能显示） |
| 每条有效报告 | id 为 `wrap/<app>/<pid>` 加报告自带的 `id`（`/` 连接）；整体替换同 id 记录；`app` 取报告的，缺省继承最近祖先记录的，再缺省用 `--app` |
| `state=clear` | 带 `id`：删除该记录及其子记录；不带：删除 `wrap/<app>/<pid>` 下全部记录 |
| 输出中出现 `OSC 133 ; A`（被包装的是 Shell 时的新提示符） | 删除 idle/working/blocked，保留 done/error（7501 的提示符规则） |
| 从挂起恢复（`fg`） | 挂起期间 Shell 出了提示符，worker 已按提示符规则删掉 idle/working/blocked；把这些记录按最后的内容重新发布 |
| 命令退出 | 根记录若不是程序自己报的 done/error：退出码 0 合成 `done`，否则 `error`，msg 为 `exit N` 或 `signal SIGTERM` 这类；然后删除 idle/working/blocked，保留 done/error |

- `<pid>` 是命令本身的进程号。`origin_session` 取 `TILDY_SESSION_ID`，`source` 为 `osc`，`seq` 交给存储递增，TTL 用缺省值（idle/working/blocked 7200 s，done/error 900 s）。
- 一个被包装的程序最多同时持有 256 条记录（7501 的每终端上限），超出时淘汰最久未更新的一条。
- 记录 id 受 7501 的 8 层、128 字节限制，前缀已占 3 层，所以报告自带的 id 最多 5 层，且加上前缀后不超过 128 字节；超出的报告不记录（写调试日志）。
- 存储写入在单独的线程上进行，同一批里被后续操作覆盖的发布会合并掉；退出时最多等 2 s 把最后的记录写完。

## OSC 7501 解析（rev 0.4）

`osc::Scanner` 增量扫描任意切分的输出块，只读不改。序列 `ESC ] 7501 ; body ST`，ST 为 `ESC \` 或 BEL；body 是以 `:` 分隔的 `key=value`。

- key 为 `[a-z]+`，value 为 `[A-Za-z0-9_.,+/=-]*`，两侧空白去掉；不合语法的对跳过、其余照常处理；未知 key 忽略；重复 key 取最后一个。
- `state` 必须有且为 `idle working done blocked error clear` 之一，否则整条忽略。`kind`（permission/question/auth）只随 `blocked`，`progress`（0–100 的整数）只随 `working`/`blocked`；无效时视为没有。`app` 不合语法视为没有。
- `id` 为 1–8 段 `[A-Za-z0-9_.+-]{1,32}`、总长 ≤128 字节，不合法则整条忽略（不退回根记录）。
- `title`/`msg` 为标准 base64（可省略填充，但填充要么完整要么没有，末尾多余位必须为 0）的 UTF-8；先查编码长度（256/2732），再解码，解码后 ≤192/2048 字节；非法 base64、非法 UTF-8、含任何控制字符（U+0000–001F、U+007F、U+0080–009F，含换行与制表符）都使整条丢弃。
- 规范限制表里的任一项（整条序列 4096 字节、key 16 字节、`app` 32 字节、id、title、msg）超限都丢弃整条；每一对都先检查，再决定是否采用。
- 字节处理同常见 VT 解析器：OSC 内的 C0 控制符（BEL、ESC、CAN、SUB 除外）忽略；CAN/SUB 中止序列；ESC 后跟的不是 `\` 时序列作废，这个 ESC 作为下一个转义序列的开头。只认 7 位的 `ESC ]`。
- 缓冲上限 4096 字节；更长的 7501 序列报一次 `too_long` 并跳过到它的终止符，其他 OSC（比如很大的 OSC 52）根本不缓冲。
- `OSC 7501 ; ?`（功能查询）和以 `?` 开头的回复不是报告，原样通过，由真实终端回答。若程序先收到 `CSI c` 的回复，说明是终端不支持 7501，与 wrap 无关。

`osc::parse_body` 单独解析报文体，可供 `tildy agent publish --osc` 复用。

## 透明性

`wrap` 是 `claude` 等程序的默认运行路径，因此：

- **建不起 pty 就直接 exec 原命令**：标准输入、输出、错误不全是终端或不是同一个终端（管道、重定向：`claude -p … | jq`）、存储不可用（`XDG_STATE_HOME` 不可写等）、`openpty` 或 fork 失败、命令起不来、或 `TILDY_AGENT_WRAP=0`。此时命令就是 wrap 这个进程本身，描述符原样继承。
- **除命令的字节外不向真实 stdout/stderr 写任何东西**。诊断写到 `TILDY_DEBUG_LOG`（与 Zsh 适配层同一个日志，事件名 `wrap`）；`TILDY_DEBUG=1` 时命令启动前的诊断也写到 stderr。唯一例外是命令不存在或不可执行：像 Shell 一样打印 `tildy agent wrap: command not found: X`，退出码 127（不可执行为 126）。
- **先写终端再解析**：每块输出先完整写到终端，再交给解析器；记录写入在另一个线程。解析或存储出错（包括 panic）只停止记录，转发照常。
- **raw mode**：运行期间真实终端处于 raw 模式（无输入处理、无输出处理，pty 内部的行规程负责这些）；命令退出、收到转发的信号、panic、被挂起时都恢复原来的属性。
- **窗口尺寸**：pty 创建时即为真实终端的尺寸，SIGWINCH 时复制过去（内核随即给命令发 SIGWINCH）；从挂起恢复时也重新复制。
- **信号**：发给 wrap 的 SIGINT、SIGTERM、SIGHUP、SIGQUIT 转发给命令的进程组。键盘上的 Ctrl-C、Ctrl-\ 是字节，经 pty 的行规程送到命令，和不用 wrap 时一样。
- **作业控制**：wrap fork 一个监视进程做 pty 的会话首进程，命令在它下面自成前台进程组（与 sudo 的 `use_pty` 相同）。这样命令的进程组不是孤儿进程组，Ctrl-Z 能让它停下，自己挂起的程序（vim、Claude Code）也能挂起；wrap 随即恢复终端并挂起自己，Shell 显示 `suspended`，`fg` 后 wrap 重新进入 raw 模式、复制尺寸并让命令继续。
- **终端查询与回复**：DA1/DA2、kitty 键盘协议查询（`CSI ? u`）、OSC 52、OSC 7501 查询、同步输出（`CSI ? 2026 $ p`）、鼠标、括号粘贴、备用屏都只是字节，双向原样通过；测试逐字节核对。
- 进程关系：Shell → wrap（转发、记录）→ 监视进程（pty 会话首进程）→ 命令。

## 限制

- 命令运行在另一个 pty 里：`tty` 显示的设备名不同；终端的鼠标模式、备用屏、括号粘贴等由命令发出的序列开关，wrap 不跟踪也不在退出时替命令复位（与直接运行时一致：命令崩溃留下的模式同样留在终端里）。
- 子进程的 SIGPIPE 为默认处理（与 Shell 启动程序时相同）；wrap 本身继承到的 SIGPIPE 忽略状态不会传下去。
- 命令退出后最多再转发 2 s 的剩余输出（通常立刻读到 pty 的结束）。
- 开销（release，本机）：到命令第一个输出字节多约 6 ms。缺省范围要跑一次 `git rev-parse`（macOS 的 `/usr/bin/git` 约 17 ms），在记录线程上进行，不推迟命令启动；但运行时间比它还短的命令（如 `claude --version`）会在退出时多等约 20 ms，等最后一条记录写完。
- 每个被包装的程序最多 256 条记录；报告自带的 id 最多 5 层（见上）。
- `--debug-reports FILE`（隐藏选项，测试用）把扫描到的事件和记录操作逐行写成 JSON。

## 安全

- 解析器拒绝 `title`/`msg` 含控制字符的报告，所以记录里不会有 ESC、换行这类能改变提示符或终端状态的字符；长度、id 语法同样按 7501 硬上限拒绝。
- 记录只是数据：wrap 不执行、不解释报告里的任何内容，不回写终端（7501 规定终端唯一的回写是功能查询的回复，那由真实终端负责）。
- 记录文件在 `$XDG_STATE_HOME/tildy/agent/` 下，目录 0700、文件 0600（存储负责）。
- 子进程继承 wrap 的环境（包括 `TILDY_SESSION_ID`）与工作目录，不额外注入变量。

## 测试

- `src/agent/osc.rs`：29 个单元测试——两种终止符、任意切分（逐字节、每个切点）、查询与回复、与 CSI/OSC 8/OSC 52/DCS/OSC 133 交错、作废与中止、C0 忽略、空白、非法对、重复 key、state/kind/progress/app/id 规则、base64（含不完整填充、非零尾位、往返）、控制字符、各项长度上限与整条 4096 字节边界、溢出后重新同步与缓冲上限。
- `src/cli/wrap.rs`：14 个单元测试——app 名（含非 UTF-8 命令名）、启动/报告/退出生命周期、恢复后重新发布、退出码与信号的合成消息、app 继承、clear、提示符规则、id 超出前缀限制、256 条淘汰、批量合并、记录线程、raw mode 在 drop 与 unwind 时恢复。
- `tests/agent_wrap.rs`：17 个集成测试，经真实二进制、真实 pty（wrap 以测试 pty 为控制终端）、独立的 `XDG_STATE_HOME`：输出与直接运行逐字节相同且记录正确、启动记录与退出合成、退出码与信号退出码、INT/TERM/HUP 转发、非终端时直接 exec、存储不可用与 `TILDY_AGENT_WRAP=0` 时直接 exec、命令不存在、非 UTF-8 参数原样传递、`cat` 交互、键入的 Ctrl-C 与 Ctrl-Z、在交互 zsh 里 Ctrl-Z 挂起并 `fg` 恢复、`zsh -f` 交互与退出码、启动时与 SIGWINCH 时的窗口尺寸、终端查询与回复双向逐字节通过、OSC 133 提示符规则、worktree/session/global 范围、`less` 与 `vim` 不卡死；每次退出都核对终端属性已恢复。
