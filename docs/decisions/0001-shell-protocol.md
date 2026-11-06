# 0001：Shell 协议 v0（制表符文本帧、代次校验、两种后端形态）

- 日期：2026-10-19
- 状态：提议中

## 背景

Zsh 适配层必须在不阻塞输入的前提下向渲染后端要提示符，并且只接受与当前上下文一致的结果（PLAN.md "Shell 协议""上下文与乱序结果""后端生命周期"）。约束：

- Zsh 侧只处理不透明字符串，不解析 JSON，不 `eval` 后端数据。
- 结果通过 `zle -F` 在编辑器等待按键时异步到达，可能是半帧、乱序、过期，后端也可能崩溃或卡死。
- M0 同时验证两种后端形态：每次请求启动一个进程（oneshot）与会话常驻进程（persistent），两者必须共用同一帧格式。
- 接口契约已在 `docs/architecture.md` "Shell 协议 v0" 中给出；本 ADR 记录任务 A 原型（`shell/zsh/tildy.zsh`、`bench/fake-backend.sh`）对契约的落地方式、补充的细节和已知限制。

## 决定

采用一行请求、多行结果的制表符文本帧，结果以 `RES gen` 开头、以同代次 `END gen` 结束才算完整；Zsh 只接受 `gen == 当前代次` 的完整帧，其余情况整帧丢弃并保留当前显示。oneshot 与 persistent 两种形态共用帧格式，Zsh 侧另用一个带随机 nonce 的包装子 shell 在同一管道里报告后端 pid 与退出码。

## 帧格式

请求（Zsh → 后端，一行）：

```
REQ<TAB>gen<TAB>session_id<TAB>cwd<TAB>cols<TAB>lines<TAB>last_exit<TAB>last_duration_ms
```

结果（后端 → Zsh，多行）：

```
RES<TAB>gen<TAB>fresh|partial
L<TAB><左提示符，Zsh 提示符转义>
R<TAB><右提示符，可省略>
END<TAB>gen
```

- `session_id` 为 `$$.<随机数>`；`cols`/`lines` 取请求时的 `$COLUMNS`/`$LINES`；`last_exit` 为 precmd 第一条语句捕获的 `$?`；`last_duration_ms` 由 preexec/precmd 的 `$EPOCHREALTIME` 相减得到，空回车为 0。
- 代次：precmd、chpwd、终端缩放各递增一次。`cd` 命令会让代次跳 2（chpwd 一次、随后 precmd 一次）；首个提示符之前（例如 `.zshrc` 里 `cd`）的 chpwd 不递增，所以第一个提示符固定是 `g1`。
- 解析规则（`_tildy_on_line`）：
  - 只有在帧内（收到 `RES` 之后）才接受 `L`/`R`/`END`；`R` 最多一行。
  - `status` 只能是 `fresh` 或 `partial`；`partial` 目前与 `fresh` 同样处理，只记录在 `recv` 日志里。
  - oneshot 模式下 `RES` 的 gen 必须等于该进程收到的请求 gen，否则为 `bad_frame`。
  - 空行忽略，其他无制表符的行或未知标签都是 `bad_frame`。
  - 单帧（或缓冲中未结束的一行）超过 64 KiB 视为 `bad_frame`，oneshot 下随即杀掉该进程。
- 丢弃原因与契约枚举的对应：

| 情况 | 日志 |
| --- | --- |
| 帧完整但 gen 已不是当前代次 | `recv` 后 `discard gen stale` |
| 代次变化时 oneshot 进程仍在运行（被 kill） | `discard gen stale`，随后 `backend_exit gen pid 143` |
| 有 `RES` 无 `END` 就 EOF，或帧内又来一个 `RES` | `discard gen half_frame` |
| `END` 的 gen 与 `RES` 不一致 | `discard gen half_frame`（契约原文"gen 不一致……都算半帧"） |
| 没有任何帧就 EOF（后端失败、崩溃） | `discard gen eof` |
| 超过 `TILDY_BACKEND_TIMEOUT_MS` | `discard gen timeout`，oneshot 下 kill |
| 未知行、字段非法、行数与当前提示符不同 | `discard gen bad_frame` |

### 为什么是制表符文本而不是 JSON

- Zsh 没有可靠且快速的 JSON 解析，按行和 `${var%%$'\t'*}` 切分只用参数展开，不 fork、不 `eval`。
- 提示符内容本身就是单行 Zsh 提示符转义串，后端已经完成编码；Zsh 侧只需要原样放进 `PROMPT`。
- 帧的完整性用 `END gen` 闭合，比 JSON 的括号配对更容易在流里检测半帧。
- 结构化数据只出现在 CLI 的 `--json` 输出里（PLAN.md），`schema/shell-frame.schema.json` 仅作文档用途。

### 转义

- 请求中的 `cwd` 由 Zsh 侧把 `\` 转为 `\\`、制表符转为 `\t`、换行转为 `\n`（`_tildy_escape`，先处理反斜杠）。调试日志的 `send` 行使用同一转义形式。
- 结果中的内容由后端负责编码：用户数据里的 `%` 写成 `%%`，控制字符不得原样出现。假后端的编码器（`encode()`）先还原 `\t` `\n` `\\`，再把 `%` 写成 `%%`、把控制字符写成可见的 `^X`/`?`。
- Zsh 侧做纵深防御（`_tildy_sanitize`）：逐行删除 `[[:cntrl:]]`（C0、DEL，UTF-8 locale 下也包括 C1），因此即使后端违约发出 ESC/BEL，也不会到达终端。实测 `TILDY_FAKE_RAW_CTRL=1` 时右提示符显示 `pct=100% bel=^G[31mraw`，ESC 与 BEL 已被删除。
- `prompt_subst`：不修改用户的全局选项，而是在写入前按当时的选项状态选择写法。`prompt_subst` 关闭时内容直接放入 `PROMPT`（只展开 `%` 转义）。`prompt_subst` 打开时写 `PROMPT='${_tildy_ps_left}'`：参数值不会再被扫描 `$(...)`、反引号或 `${...}`，只会展开 `%` 转义。`prompt_bang` 打开时把 `!` 写成 `!!`。选项状态在 `emulate -L zsh` 之前采样，因为 `emulate` 会遮蔽用户选项。

## 两种后端形态

| | oneshot（默认） | persistent |
| --- | --- | --- |
| 启动 | 每个请求 `exec {fd}< <(包装子 shell)`，子 shell 内 `print -r -- $req \| $backend &` | 首个请求时创建两个 FIFO（`mkfifo` 后立刻 `unlink`），`sysopen -o cloexec` 打开，后台启动 `包装子 shell <&req >&resp &!` |
| pid/退出码 | 包装子 shell 输出 `PID<TAB>nonce<TAB>pid` 与 `EXIT<TAB>nonce<TAB>code` | 同左 |
| 代次变化 | kill 仍在运行的旧进程（`discard stale`），通道继续读到 `EXIT` 后关闭 | 不 kill，旧结果到达时按 gen 丢弃 |
| 超时 | `discard timeout` 并 kill | `discard timeout`，不 kill（后端是共享的），迟到的帧仍记录 `recv` 后 `discard timeout` |
| 后端死亡 | 等同 EOF | `EXIT`/EOF 视为死亡，记录 `backend_exit`；若当前代次的请求因此丢失则立即重发：重启后端，累计死亡超过 `TILDY_PERSISTENT_MAX_RESTARTS`（默认 3）次后，本会话改用 oneshot |
| 写请求 | 写入管道 | 非阻塞写（`sysopen -o nonblock` + `syswrite`），局部忽略 `SIGPIPE`；写失败或短写即判定后端卡死或已死，杀掉，并让本次请求改走 oneshot |
| Shell 退出 | 进行中的进程写入已关闭的管道时收到 SIGPIPE | 写端随 Shell 关闭，后端读到 EOF 后退出；实测退出后无残留后端进程 |

两个实现细节有实测依据：

- zsh 5.9 中进程替换不设置 `$!`（实测为 `0`，若直接 `kill $!` 等于 `kill 0`，会杀掉整个进程组），`wait` 也不认这个子进程（`pid ... is not a child of this shell`）。因此 pid 与退出码改由包装子 shell 在同一管道里报告。nonce 由 Zsh 每次生成，不导出给后端，后端无法伪造这两行。
- 交互式 zsh 中进程替换子进程有独立进程组（实测 `pgid != tpgid`），终端的 SIGINT/SIGWINCH 不会打到后端。

`backend_start gen pid mode` 在读到 `PID` 行时记录，因此 oneshot 下它出现在同一代次的 `send` 之后；persistent 下只在（重新）启动时记录。

## 证据

- 实现：`shell/zsh/tildy.zsh`（`_tildy_request`、`_tildy_os_spawn`、`_tildy_p_start`、`_tildy_on_line`、`_tildy_frame_done`）；假后端 `bench/fake-backend.sh`。
- 自测：`python3 shell/zsh/selftest/selftest.py`，2026-10-19 结果为 16/16 通过，覆盖 A/B 乱序（oneshot 与 persistent）、半帧、坏 gen、失败、超时、同一提示周期内的失败保留上一个结果（`flaky-backend.sh` + 缩放）、`prompt_subst`/`prompt_bang` 注入、`%`/控制字符、persistent 崩溃重启、重复 `source`。
- PTY harness（任务 B）：`python3 tests/pty/run.py --json bench/results/pty-real.json`，2026-10-19 结果为 23/23 通过（含三个插件变体）。
- 注入实测：目录名 ``x$(touch PWNED)`touch PWNED2`${HOME}!y``，在 `setopt prompt_subst prompt_bang` 下提示符原样显示该名字，且没有生成文件（自测 `x_prompt_subst_injection`）。
- 进程替换的 `$!`、`wait` 行为：在 `zsh -f` 中执行 `exec {fd}< <(...)` 后 `print $!` 输出 `0`，`wait <pid>` 报 `not a child of this shell`（2026-10-19，zsh 5.9 arm64-apple-darwin）。
- 同步开销：在本机交互式 shell 内连续调用 200 次 `_tildy_precmd`（包含 fork 进程替换），中位数 1.07 ms，p95 2.03 ms，最大 2.47 ms；`source tildy.zsh` 约 5.8 ms（单次测量，含模块加载）。

## 后果

- 正面影响：
  - Zsh 侧没有 JSON、没有 `eval`，只做参数展开。
  - 任何残缺、过期或伪造的结果都不会画到屏幕上。
  - 两种后端形态可以无缝切换，persistent 失败时自动回退到 oneshot。
- 负面影响与代价：
  - oneshot 每个提示符多 3 次 fork（进程替换子 shell、`print`、后端）和 1 次 exec。主 shell 同步承担的只是第一次 fork（约 1 ms）。
  - 包装子 shell 在后端运行期间多驻留一个进程。
  - 被 kill 的假后端若正处在 `sleep` 中，孤儿 `sleep` 会继续跑到原定时间。它持有管道写端，但不再影响 Zsh：通道在读到 `EXIT` 行时就关闭。
- 已知限制（需要后续处理）：
  - 超时依赖 `zsh/sched`，粒度为 1 秒，实际在 `[T, T+1s)` 内触发。harness 设定 2000 ms，实测 2.0 s 记录 timeout。M1 由 worker 自己计时。
  - `cwd` 只转义 `\t` `\n` `\\`。其他控制字符（如 `\r`、ESC）和非 UTF-8 字节原样传给后端，帧大小上限和编码约定需要在 M1 写进协议版本。
  - 契约允许多行 `L`，但 M0 基础提示符固定一行，且编辑期间不得改变行数。所以行数与当前提示符不同的帧一律 `bad_frame`，多行提示符在 M0 无法显示。M1 需要"基础提示符沿用上一周期行数"的机制。
  - 在 `TRAPWINCH` 里新注册的 fd，zle 要等下一次 fd 回调或下一次按键后才开始 poll（见 ADR 0002）。所以空闲时缩放、且没有在途请求的情况下，新结果要到下一次按键才显示。
  - persistent 的写端设置了 cloexec，但如果用户此时 fork 了一个不 exec 的 zsh 子 shell（例如 `{ sleep 100 } &`），它会继承写端，后端要等它退出才看到 EOF。PLAN.md 的父进程检查留给 M1 worker。
  - 没有协议版本握手和 worker epoch。PLAN.md 要求的"重启后不接受旧 worker 回包"在 M0 只靠每个通道独立的 fd 和 nonce 隐式满足。
