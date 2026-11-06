# Tildy Zsh 适配层（协议 v1）

`tildy.zsh` 是 Tildy 的 Zsh 侧：每个提示符先立即显示基础提示符，再通过 `zle -F` 异步读取后端结果，按代次与 worker epoch 校验后，只在安全编辑状态重绘。默认后端是会话常驻的 `tildy worker`，故障时自动降级为每个提示符一次的 `tildy render`。

- 协议：[M1 计划 "Shell 协议 v1"](../../docs/protocol.md)；帧格式与转义的来历见 [ADR 0001](../../docs/decisions/0001-shell-protocol.md)。
- 安全重绘条件：[ADR 0002](../../docs/decisions/0002-safe-redraw.md)。
- v1 的 worker 状态机、self-pipe、行数沿用、transient prompt、多 RES：[ADR 0003](../../docs/decisions/0003-zsh-v1.md)。
- worker 自身的生命周期：[ADR 0004](../../docs/decisions/0004-worker-lifecycle.md)。
- instant prompt（`instant.zsh`，`tildy init zsh --instant`）：[ADR 0006](../../docs/decisions/0006-instant-prompt.md)。

## 使用

在 `.zshrc` 末尾（`compinit` 与插件之后）加入：

```zsh
eval "$(tildy init zsh)"
```

`tildy init zsh` 输出三行变量定义（`TILDY_BIN`、`TILDY_VERSION`、`TILDY_PROTOCOL_VERSION`），后面是本文件原文。脚本加载后导出 `TILDY_BIN` 与 `TILDY_VERSION`，子进程（如 `tildy configure`）据此判断 Tildy 已在当前 Shell 中启用。也可以直接 `source shell/zsh/tildy.zsh`，此时用 `PATH` 上的 `tildy`，不做版本检查。重复 `source`/`eval` 不会重复安装 hook；升级后再次 `eval "$(tildy init zsh)"` 会更新 `TILDY_VERSION`，下一个提示符发现 worker 版本不符时发 `BYE` 并换用新二进制。

不要修改自己的 `~/.zshrc` 做实验，可以用独立目录：

```sh
d=$(mktemp -d); printf 'eval "$(%s init zsh)"\n' "$PWD/target/debug/tildy" > $d/.zshrc
ZDOTDIR=$d TILDY_DEBUG_LOG=$d/log zsh -i
```

### instant prompt

初始化慢（插件管理器、`compinit`、nvm 等）时，可以在 `.zshrc` **最开头**再加一行：

```zsh
eval "$(tildy init zsh --instant)"   # 放在 .zshrc 第一行，插件管理器之前
# …其余配置…
eval "$(tildy init zsh)"             # 仍在末尾
```

它立即打印上一个会话缓存的提示符（同一目录、宽度、`TERM_PROGRAM`、版本），然后让其余初始化在关闭回显、stdin 为 `/dev/null`、stdout/stderr 写入私有临时文件的状态下运行；第一个 `precmd` 恢复这些，擦掉 instant prompt（zle 在同一位置画出真正的提示符），并把初始化期间的输出打印在提示符上方。`eval` 每次启动要运行一次 `tildy`（约几毫秒）；想完全不在热路径上启动进程，可以生成一次文件再 `source`（升级 Tildy 后重新生成）：

```zsh
tildy init zsh --instant >| ~/.cache/tildy-instant.zsh   # 一次
source ~/.cache/tildy-instant.zsh                         # .zshrc 第一行
```

`TILDY_INSTANT_PROMPT=off`（或设置 `TILDY_INSTANT_DISABLE`）关闭，`verbose` 在初始化有输出时多打一行说明。需要交互输入的初始化（密码、`[y/n]` 确认）以及 `exec tmux` 之类必须放在这一行之前。缓存由 worker 在每个 `fresh` 结果后写入 `$XDG_CACHE_HOME/tildy/instant/<key>.prompt`（默认 `~/.cache/tildy/instant/`），是纯数据，只经提示符展开打印，从不 `source` 或 `eval`。

用假后端（不需要编译）：

```sh
d=$(mktemp -d); printf 'source %s\n' "$PWD/shell/zsh/tildy.zsh" > $d/.zshrc
ZDOTDIR=$d TILDY_BACKEND=$PWD/bench/fake-backend.sh TILDY_DEBUG_LOG=$d/log TILDY_FAKE_DELAY_MS=500 zsh -i
```

## 行为概要

- 启动：加载时就启动 worker（`$TILDY_BIN worker --parent-pid $$`），通过两个打开后立即删除的 FIFO 通信。worker 先发 `HELLO`，协议版本或二进制版本与 `TILDY_PROTOCOL_VERSION`/`TILDY_VERSION` 不符时发 `BYE` 并重启。HELLO 到达之前的请求（通常是第一个提示符）走一次性 `render`。
- 每个提示符：`precmd` 递增代次、放上基础提示符、发出 `REQ`，然后记 `ready`。`cd` 让代次跳 2（`chpwd` 一次、`precmd` 一次），但只发一个请求；在 widget 里 `cd`（如 fzf 的 Alt-C）时 `chpwd` 立即发请求。
- 请求（M1b）：`REQ` 共 10 个制表符分隔字段，第 9 个 `env`、第 10 个 `jobs`。`jobs` 为 `${#jobstates}`；`env` 是 `KEY=VALUE` 以 U+001F 连接，键集合为 `SSH_CONNECTION`、`SSH_TTY`、`USER`、`HOST`、`VIRTUAL_ENV`、`CONDA_DEFAULT_ENV`、`DOCKER_CONTEXT`、`DOCKER_HOST`，空值或未设置的键省略，值中的 U+001F 先去掉，再按 `\t`、`\n`、`\\` 转义。
- 结果：同一代次可以收到多个 RES（`partial`、`fresh`、迟到的第二个 `fresh`），后到的覆盖先到的；RES 的 epoch 必须等于 HELLO 的 epoch。
- 行数：基础提示符沿用上一个结果的 `L` 行数；行数不同且输入为空时，在安全编辑状态直接重绘并更新高度；输入非空时记录 `defer gen height_with_input`，清空输入后应用，或留到下一周期。补全、搜索与续行保护仍然生效。
- 降级：worker EOF、坏帧、半帧、首帧超时、写失败时改走 `render`，当前提示符的请求立即重发；下一个请求时重启 worker，60 秒内最多重启 3 次，超过后本会话一直用 `render`（`backend_degraded restart_limit`）。
- 缩放：`TRAPWINCH` 只往 self-pipe 写一个字节，`zle -F` 回调里发新请求，空闲时也会刷新。
- instant prompt：`instant.zsh` 的片段运行过时，第一个 precmd 先调用 `_tildy_instant_finish`（片段自己的 precmd hook 通常已经调用过），第一个周期的基础提示符就是片段打印的缓存内容（同一目录与宽度时），行数也取缓存的行数，所以第一个结果不会因 `lines_mismatch` 被丢弃；若结果行数仍不同（配置改了行数），空输入时直接显示新高度的主题；正在输入时暂缓。每个请求的 `env` 带上片段计算的 `TILDY_INSTANT_KEY`，worker 用它写缓存并在调度前去掉。
- transient：结果带 `T` 行时（`history.mode` 为 `compact`），回车接受命令后（`zle-line-finish`）把提示符换成 `T` 行内容；Ctrl-C 放弃一行时也折叠，之后恢复完整提示符。退出码不受影响。
- 空回车（包括只有空白字符的输入）清除下一提示符的退出码和错误颜色，保留真实 `$?` 与已有失败摘要；compact/full 模式均生效。窗口缩放保留当前显示状态，非空输入的语法错误仍显示真实退出码。

## 环境变量

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `TILDY_BIN` | `tildy` | Tildy 可执行文件；`tildy init zsh` 写入绝对路径 |
| `TILDY_VERSION` | 空 | 期望的 worker 二进制版本；空则不检查 |
| `TILDY_PROTOCOL_VERSION` | 空 | 期望的协议版本；空则不检查 |
| `TILDY_BACKEND` | 空 | 代替 `TILDY_BIN` 的可执行文件（单一路径，不分词），以 `worker --parent-pid PID` 或 `render` 调用；harness 用它接假后端 |
| `TILDY_BACKEND_MODE` | `persistent` | `persistent`：worker 加一次性回退；`oneshot`：从不启动 worker |
| `TILDY_BACKEND_TIMEOUT_MS` | `5000` | 一次性结果、worker 请求的第一帧、HELLO 的超时；由 `zsh/sched` 驱动，粒度 1 秒 |
| `TILDY_PERSISTENT_MAX_RESTARTS` | `3` | 时间窗内允许的 worker 重启次数（首次启动不计） |
| `TILDY_RESTART_WINDOW_MS` | `60000` | 重启限速的时间窗 |
| `TILDY_BASE_PROMPT` | `%~ %# ` | 立即显示的基础提示符（只取最后一行） |
| `TILDY_DEBUG_LOG` | 空 | 追加写入调试事件，见下 |
| `TILDY_INSTANT_PROMPT` | `quiet` | instant prompt 片段：`quiet`、`verbose`（初始化有输出时加一行说明）、`off` |
| `TILDY_INSTANT_DISABLE` | 空 | 非空时 instant prompt 片段什么都不做 |
| `TILDY_SESSION_ID` | 脚本设置 | 加载时导出的本 Shell 会话 id（与 REQ 的 session 字段相同）；`tildy agent wrap`/`publish` 把它记为记录的 `origin_session` |
| `TILDY_AGENT_WRAP` | 空 | `0`：不为 Agent 程序定义包装函数（加载时读取），已定义的包装每次调用也检查它 |
| `TILDY_AGENT_PROGRAMS` | 空 | 替换要包装的 Agent 程序列表（空格或逗号分隔，加载时读取）；默认取 `tildy init zsh` 写入的 `_tildy_agent_programs`（配置 `agent.programs`） |

假后端的 `TILDY_FAKE_*` 变量见 `bench/fake-backend.sh` 文件头。

## 调试日志

`TILDY_DEBUG_LOG` 指向的文件每行一个事件：`<毫秒时间戳>\t<事件>\t<字段...>`。

| 事件 | 字段 | 含义 |
| --- | --- | --- |
| `hook_installed` / `hook_skipped` | 名字 [原因] | 加载时安装的 hook；已有列表式 `trap` 时跳过 `winch`/`int` |
| `backend_start` | gen pid mode | worker（`persistent`）或一次性进程（`oneshot`）启动 |
| `hello` | epoch version protocol pid | 收到 worker 的 HELLO |
| `bye` | epoch reason | 向 worker 发了 BYE |
| `send` | gen cwd worker\|oneshot | 发出请求 |
| `ready` | gen | 基础提示符就位，precmd 的同步部分结束 |
| `recv` | gen status epoch | 收到完整帧（一次性为 epoch 0） |
| `accept` / `defer` / `apply_deferred` | gen [原因] | 当场画出 / 暂缓（原因见 ADR 0002）/ 暂缓后画出 |
| `discard` | gen reason | `stale`、`half_frame`、`eof`、`timeout`、`bad_frame`、`lines_mismatch` |
| `transient` | gen [int] | 提示符折叠为 `T` 行（`int` 表示 Ctrl-C） |
| `backend_degraded` | reason | worker 不可用：`eof`、`bad_frame`、`half_frame`、`timeout`、`hello_timeout`、`write_failed`、`version_mismatch`、`protocol_mismatch`、`restart_limit`、`spawn_failed`、`no_backend`、`mode_oneshot` |
| `backend_exit` | gen pid code | 后端进程结束（-1 表示只看到 EOF） |
| `instant` | rows hit\|miss | instant 片段打印了缓存的提示符（`hit`）或基础提示符（`miss`），占 rows 行屏幕 |
| `instant_done` | bytes how rows | 恢复重定向并擦除 instant prompt；bytes 为初始化期间捕获的输出字节数，how 为 `precmd`、`tildy` 或 `exit` |
| `height_changed` | gen old_lines new_lines | 安全编辑状态且输入为空时，提示符调整高度并重绘 |
| `agent_wrap` | 程序列表 | 加载时为这些 Agent 程序定义了 `tildy agent wrap` 包装函数（逗号分隔） |

## 安装的东西

- `add-zsh-hook`：`precmd`、`chpwd`、`preexec`。
- `TRAPWINCH`、`TRAPINT`：已有同名函数时先复制为 `_tildy_orig_*` 再串联调用；已有列表式 `trap '...' WINCH/INT` 时不安装（定义函数会覆盖它），记 `hook_skipped`。
- `add-zle-hook-widget`：`line-pre-redraw`（延迟应用）、`isearch-update`/`isearch-exit`（增量搜索状态）、`line-finish`（transient）。
- 一个 self-pipe（FIFO，打开后立即删除）及其 `zle -F` 回调。
- 首个 precmd 时包装补全 widget（`_tildy_cw_*` → `_tildy_orig_*`），用来知道补全菜单是否打开。
- instant 片段（只在它运行时）：`precmd_functions` 最前面的 `_tildy_instant_precmd` 与 `zshexit_functions` 里的 `_tildy_instant_exit`，完成后都移除自己；`_tildy_instant_*` 函数与变量；初始化期间 fd 0/1/2 的重定向和关闭的回显。
- Agent 程序包装（`docs/agent-status.md`）：`agent.programs` 中存在于 PATH、且不是已有函数/别名/内建/保留字的程序各一个同名函数，调用 `_tildy_agent_run`（只在 stdin/stdout 是终端时经过 `tildy agent wrap`，退出码透传）；列表在 `$_tildy_agent_wrapped`，`unfunction NAME` 去掉。导出 `TILDY_SESSION_ID`。
- Tildy 接管 `PROMPT`/`RPROMPT`；不修改 `prompt_subst` 等用户选项。`prompt_subst` 打开时写 `PROMPT='${_tildy_ps_left}'`，后端文本不会经过参数展开或命令替换。

## 自测

```sh
python3 shell/zsh/selftest/selftest.py            # 任务 A 自测（pexpect + pyte，独立 ZDOTDIR），含 precmd 开销测量
python3 shell/zsh/selftest/selftest.py m_precmd_cost_worker   # 只跑一项
python3 shell/zsh/selftest/zle_probe.py           # ADR 0002 的 ZLE 行为证据
zsh -f -i -c 'source shell/zsh/selftest/precmd_bench.zsh'      # zle 之外的 precmd 微基准
python3 tests/pty/run.py                          # PTY harness（假后端 + 真实二进制场景）
python3 tests/pty/run.py --real                   # 真实二进制模式
python3 tests/pty/run.py --real --scenario 'instant_*'   # instant prompt 场景
```

## 已知限制

- 超时粒度为 1 秒（`zsh/sched`）。
- 第一个提示符通常走一次性 `render`：worker 在加载时启动，但 HELLO 一般在第一个 precmd 之后几毫秒才到。
- 必须在 `compinit` 之后加载；之后重新定义补全 widget 的插件会覆盖包装（ADR 0002）。
- 已有列表式 `trap ... INT`/`WINCH` 时，Ctrl-C 不折叠提示符、空闲缩放不刷新。
- instant prompt 的限制（初始化需要输入、`exec`、写 `/dev/tty` 的程序、短暂显示过时的 Git 状态等）见 ADR 0006。
- `prompt_subst` 的写法在写入 `PROMPT` 时按当时的选项决定；如果某个 widget 在编辑中途打开 `prompt_subst`，在下一次写入之前的重绘会对后端文本做替换（ADR 0003 "未解决问题"）。

### M3 history and observation metadata

Protocol v1 keeps the existing RES header and L/R/T/END lines. Optional lines
inside RES are `O\t<observation_seq>`, `K\t<context_key>` (opaque hex),
`H\t<contextual transient line>` (repeated for multiple lines),
`U\t<single-line context suffix>`, and `S\t<summary threshold in milliseconds>`.
Old backends can omit every new line. H must fit the original L height. K/O/U/S
occur at most once. The shell acknowledges `ACK\t<gen>\t<epoch>\t<seq>` only
after applying a complete frame with the current generation and worker epoch.
Discarded, deferred, and stale frames do not advance the
worker's observation baseline.

The forwarded environment includes the private `TILDY_SUBMISSION_ID` counter.
It changes in preexec, while resize requests keep the same counter. This lets
the worker clear idle change hints on command submission without clearing them
on resize. Frames arriving while a command runs are discarded.

Compact history compares the accepted frame's context with the previous
submitted command. It reuses available prompt rows for a separator or displays
a suffix through ZLE's POSTDISPLAY after the command text. A suffix is shortened
or omitted if the input leaves insufficient room; BUFFER is never rewritten.
Failure/duration summaries run after terminal precmd hooks and before the next
prompt's OSC 133 A marker. `history.mode=full` disables all these history effects;
`history.context_separator=false` and `history.summary=false` disable each part.

Width fixtures use unicode-width on complete strings (including ZWJ, variation
selectors, combining marks, East Asian text, and private-use glyphs). The adapter
also measures text using Zsh's actual width rules when fitting history suffixes.
Explicit foreground calibration can supply exact grapheme advances and Zsh
`%G` declarations; the adapter never probes from init/precmd/ZLE. See
[terminal-width.md](../../docs/terminal-width.md) for matching, cancellation,
recovery limits, and the instant-cache exclusion.
