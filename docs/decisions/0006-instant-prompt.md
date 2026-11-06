# 0006：instant prompt（启动时先显示缓存的提示符）

- 日期：2026-10-20
- 状态：提议中

## 背景

PLAN.md 把 instant prompt 列在 M4（"为 instant prompt 编写早期加载逻辑，验证启动输出、交互式初始化、重定向与恢复；提供显式关闭与兼容降级""将首屏显示与首条命令执行分别测量，不隐藏初始化成本"），应用户要求提前实现。性能预算"首屏与首条命令"要求两者分别相对基础 Zsh 测量；风险表把"instant prompt 与初始化冲突（吞字、遮挡、死锁）"列为需要开关、能力检测与回退的风险。

约束：

- 片段必须放在 `.zshrc` 最前面，早于插件管理器。此时还不知道用户配置的路径和修订，Tildy 的主脚本也还没加载。
- 热路径上尽量不启动进程；缓存只能当数据读，不能 `source` 或 `eval`（ADR 0001、0003 的"后端文本不经过替换"同样适用）。
- 渲染只在后端做；一次性模式"只读磁盘缓存，不写入状态性数据"（PLAN.md "即时显示与后台状态"）。
- REQ 的 `env` 字段整体是 Env 依赖的缓存修订（`src/scheduler` 的 `key_for`）。会话 provider（ssh、hostname、username）和 git 都依赖 Env。

实现中确认的 Zsh 行为（zsh 5.9，macOS；复现命令见"证据"）：

1. `PROMPT_SP` 在 `preprompt()` 开头、任何 `precmd` 之前输出。光标不在第 0 列时，它把光标移到下一行。
2. `{ exec 2>&$fd } 2>/dev/null` 在组结束时恢复组自己保存的 fd 2，等于撤销了 `exec` 对 fd 2 的修改。
3. 在函数里定义的 `TRAPEXIT` 或 `trap ... EXIT` 在该函数返回时执行；函数里的 `trap - EXIT` 也删不掉全局的 EXIT trap。`zshexit_functions` 则可以在函数里追加，`.zshrc` 中途 `exit`、命令行 `exit`、Ctrl-D 时都会执行（`exec` 时不执行）。
4. `exec {var}>&1` 得到的 fd 不带 close-on-exec，会被子进程继承。
5. 第一个提示符之前挂一个 `sched +0` 事件会让 zsh 执行两次 `precmd`（多出一个提示符周期）。
6. 输入回显由终端行规程负责，与有没有进程在读无关；tty 未冻结（`ttyctl -f`）时，外部命令以状态 0 结束后 zsh 会记下当时的 tty 设置，之后 zle 以它为基准。

## 决定

`tildy init zsh --instant` 输出一个对别名安全的片段，放在 `.zshrc` 第一行：它按 `$PWD`、`$COLUMNS`、`$TERM_PROGRAM` 与 init 时嵌入的二进制版本算出缓存键，读取 worker 在之前会话写下的 `<key>.prompt`，用提示符展开打印（没有缓存时打印基础提示符），然后关闭回显、把 stdin 换成 `/dev/null`、stdout/stderr 写入一个已删除的私有临时文件，直到第一个 `precmd` 恢复一切、擦掉 instant prompt 并把初始化输出打印在提示符上方。缓存键只在片段里定义一次，主脚本每个请求以 `TILDY_INSTANT_KEY` 放进 REQ 的 `env`，worker 在调度前取出它，并在每个 `fresh` RES 之后把"新会话第一个提示符"形式的结果原子写入缓存。

## 设计要点

### 片段与加载

- `shell/zsh/instant.zsh`，由 `src/cli/init.rs` 的 `--instant` 嵌入。输出两部分：`'builtin' 'typeset' '-g' _tildy_instant_version='<版本>'`，以及与主脚本相同的 `() { 'builtin' 'emulate' '-L' 'zsh' '-o' 'no_aliases'; 'builtin' 'eval' "$1" } '<片段>'`。`--instant --raw` 输出不带包装的原文。
- 推荐写法 `eval "$(tildy init zsh --instant)"` 每次启动运行一次 `tildy`；也可以 `tildy init zsh --instant >| file` 生成一次后 `source file`，热路径上没有任何进程（升级后重新生成；版本只用于缓存键，旧文件照样工作）。
- 跳过条件（片段什么都不做，主脚本也不发缓存键）：`TILDY_INSTANT_PROMPT=off`、`TILDY_INSTANT_DISABLE` 非空、非交互、`zle` 选项关闭、fd 0/1/2 任一不是 tty、`zsh -c`（`ZSH_EXECUTION_STRING`）、`zsh script`（`ZSH_SCRIPT`）、子 Shell、`TERM=dumb`、本进程已经运行过片段或已经加载了主脚本（再次 `source ~/.zshrc`）。`SHIN_STDIN` 不能用：读 `.zshrc` 时它总是 off。

### 缓存键

- `_tildy_instant_key`：`v<版本>_t<TERM_PROGRAM>_<PWD 的 32 位 FNV-1a，8 位十六进制>`，再把 `[A-Za-z0-9._-]` 以外的字符换成 `_`。`TERM_PROGRAM` 决定符号层，版本变化可能改变渲染。**修订（2026-10-20）**：原键含 `c<COLUMNS>`。作者在 cmux 里每个 pane 宽度不同，每种宽度都是一份缓存、第一次都未命中，首屏先出现基础提示符；现在去掉列宽，过宽的行按量出的宽度折行计数，右侧放不下就不显示，首个基础提示符复用缓存时也不再要求列宽相同。代价是窄 pane 上首屏可能折行一次，几百毫秒后由真实结果替换。
- 只定义这一次：片段读缓存时调用它，主脚本每个请求也调用它（按 `PWD`、`COLUMNS`、`TERM_PROGRAM` 记忆，变化才重算），把结果作为 `TILDY_INSTANT_KEY=<key>` 追加到 `env`。Rust 侧不推导键，只校验字符集（不合规就不写，也不改写，保证两边文件名一致）。
- worker 与一次性 `render` 都在使用请求之前用 `session::instant::take_key` 去掉这一对。原因：`env` 是缓存修订，键随目录和宽度变化，留在里面会让每次 `cd` 和缩放都使会话 provider 与 git 的缓存失效（partial 里 Git 显示为 pending、主机名重新获取），也会出现在渲染器的 `env.*` 字段里。
- 目录：片段按 `$XDG_CACHE_HOME`（仅绝对路径）或 `~/.cache` 加 `tildy/instant` 决定，主脚本在启动 worker 时把它导出为 `TILDY_INSTANT_DIR`；worker 没有这个变量时按同样规则自己算。这样即使 `.zshrc` 在片段之后改了 `XDG_CACHE_HOME`，读写的仍是同一个目录。

### 缓存文件（worker 写）

```
tildy-instant<TAB>1<TAB><n>
L<TAB><行>      # n 行，Zsh 提示符转义，按 RES 行的规则清理（protocol::sanitize_prompt_text）
R<TAB><右侧>    # 可选
```

- 只在 worker 发出 `fresh` RES 之后写，一次性 `render` 不写。写入的不是刚发出的提示符本身，而是同一份数据以 `last_exit=0`、`last_duration_ms=0`、`jobs=0` 再渲染一次：新会话的第一个提示符不该显示上个会话最后一条命令的退出码或耗时。
- 后台线程写（`InstantWriter`），同一路径只写最新内容，与上次提交相同则跳过；临时文件加 `rename`，文件 0600、目录 0700。每个 worker 第一次写之前清理一次目录：超过 30 天的删除，再按修改时间删到 200 个以内。worker 正常退出时等待排队的写入完成。
- 文件名 `<key>.prompt`（任务说明写的是 `<key>.zsh`；它不是脚本，用 `.prompt` 免得被当成可以 `source` 的文件）。

### 显示

- 片段在 `emulate -L zsh` 下用 `${(%)line}` 展开每一行后 `print -rn` 输出，`prompt_subst`、`prompt_bang` 在这里一定是关的，`$(...)` 和反引号原样显示。读入的行先去掉控制字符，与主脚本的 `_tildy_sanitize` 相同。
- 行宽用 p10k 的方法由 zsh 自己量：`%N(l.x.y)` 二分查找，正确处理 `%{...%}` 与宽字符。每行占 `ceil(宽度/COLUMNS)` 个屏幕行，总和记在 `_tildy_instant_rows`。右侧与 zle 一样右对齐、留一列，放不下就不显示；用 CHA（`ESC[nG`）定位后再回到左侧末尾。最后输出 `ESC[0m`，免得背景色在擦除时被 `ESC[J` 填满整屏。
- 没有缓存（第一次使用、新目录）时打印 `TILDY_BASE_PROMPT` 的最后一行（默认 `%~ %# `），即主脚本接下来本来就会显示的基础提示符，所以未命中也不闪烁。任务说明举例的是 `%~ ❯ `，这里选择与主脚本一致。**修订（2026-10-20）**：未命中时还会取同版本、同 `TERM_PROGRAM` 下最新的一份缓存（别的目录）的行数作为高度提示（`_tildy_instant_hint`），基础提示符在上方补空行到这个高度，主脚本以它作为首周期的行数。原因：提示符的行数取决于配置而不是目录，而主脚本在周期内不改高度（ADR 0002/0003），没有提示时第一个结果（两三行）与一行的基础提示符不匹配，会作为 `lines_mismatch` 压到下一次回车才画出来，作者在新目录开终端时看到的就是一直停在 `~ %`。有了提示，首个结果高度相同，几十毫秒内就画出来。提示错了（极少：换了预设后第一次）仍回到原来的行为。

### 重定向与恢复

- 打印之前先 `sysopen -rw -o creat,excl,cloexec -m 600` 在 `$TMPDIR` 创建捕获文件并立即删除；创建失败就整段放弃（不打印、不重定向，但保留键函数，worker 照样更新缓存）。文件已删除，Shell 在初始化中途被杀也不会留下任何东西；内容在恢复时通过保留的 fd `sysseek` 到开头再 `sysread` 读回。
- 打印后执行 `exec {fd0}<&0 {fd1}>&1 {fd2}>&2 0</dev/null 1>&$ofd 2>&$ofd`。stdin 换成 `/dev/null` 与 p10k 相同：初始化里读 stdin 的命令立刻得到 EOF，而不是吃掉用户提前输入的字符，也不会对着看不见的问题一直等待。
- `_tildy_instant_finish`（幂等，保持 `$?`）：恢复 fd 0/1/2 并关闭保存的副本（不能包在 `{ } 2>/dev/null` 里，第 2 条）、`stty echo`、读回捕获内容、擦除 instant prompt、打印捕获内容（不以换行结尾时补一个换行，因为本提示符的 `PROMPT_SP` 已经做过了）；`verbose` 模式再加一行黄色说明。
- 调用者：片段插在 `precmd_functions` 最前面的 hook（随后把自己移除）；主脚本 `_tildy_precmd` 开头（以防 `.zshrc` 重置了 `precmd_functions`）；`zshexit_functions` 里的 hook（初始化中 `exit`，完成后把自己移除）。不用 EXIT trap（第 3 条：在包装函数里设置会在函数返回时触发，放到顶层又会与用户的 trap 冲突），也不用 `sched +0` 兜底（第 5 条）。
- 主脚本启动的 worker 与一次性子进程会关闭保存的 tty 副本（第 4 条），并从环境拿到 `TILDY_INSTANT_DIR`。

### 回显

- 打印 instant prompt 之后立即 `command stty -echo`（打印在前，首屏不等这次 fork）。用户在初始化期间输入的字符留在 tty 队列里、不回显，zle 启动后读到它们；规范模式下已经键入的回车会让该行在 zle 启动时直接执行，与没有 instant prompt 时的预输入行为一致。
- 由于第 6 条，`stty -echo` 之后 zsh 记下的 tty 状态就是"无回显"，所以第一个 `precmd` 必须 `stty echo`，否则之后的命令都在无回显下运行。两次 `stty` 各是一次 fork，计入首条命令的成本（见"证据"）。

### 擦除与第一个提示符

- 擦除：回到 instant prompt 第一行的第 0 列，`ESC[J` 清到屏幕末尾，zle 随后在同一位置画真正的提示符。上移的行数是 `rows - 1`，在 `precmd` 中且用户开着 `PROMPT_SP`、`PROMPT_CR`、最后一行宽度大于 0 时再加 1（第 1 条：此时 `PROMPT_SP` 已经把光标挪到下一行）。选项在 `emulate -L zsh` 之前采样。
- 主脚本：缓存命中时，`_tildy_next_lines` 取缓存的行数，第一个周期的基础提示符就是缓存内容（同一目录、同一宽度时），经 `_tildy_set_prompt` 写入（按 `_tildy_precmd` 开头采样的 `prompt_subst`/`prompt_bang`，后端文本不经过替换）。因此正常情况下 instant prompt 被擦掉后 zle 画出完全相同的文字，直到 partial/fresh 结果替换它，屏幕上没有闪烁，第一个结果也不会因 `lines_mismatch` 被丢弃。
- 第一个结果的行数与缓存不同（配置改了行数）时，按 ADR 0003 本周期不画它，但不能让过时内容停留一整个周期：换成同高度的普通基础提示符，记 `instant_replaced gen`，走与结果相同的安全重绘检查。
- transient prompt 照常工作：第一个周期的 `T` 行来自第一个结果。

### 调试日志

`instant rows hit|miss`（打印之后）、`instant_done bytes how rows`（`how` 为 `precmd`、`tildy`、`exit`）、`instant_replaced gen`。基线用 `instant` 与 `ready` 分别度量"首屏"与"可输入"。

## 证据

均在 2026-10-20、macOS（Darwin 27.0，arm64）、zsh 5.9 上采集。

### Zsh 行为（第 1–6 条）

- 第 1 条：`python3` 用 pexpect 读原始字节，缓存命中时序列为 instant prompt、`ESC[0m`、`PROMPT_SP` 的反显 `%` 加 99 个空格与 `\r \r`、之后才是 `precmd` 里的 `\r ESC[J`。修正之前，`%` 与旧提示符留在第一行，真正的提示符画在第二行。
- 第 2 条：`zsh -fc 'exec {a}>&1 {b}>&2 1>cap.txt 2>&1; f() { { exec 1>&$a 2>&$b } 2>/dev/null; print -r -- to-stdout; print -ru2 -- to-stderr; }; f'` 之后 `cap.txt` 里是 `to-stderr`：stdout 恢复了，stderr 没有。
- 第 3 条：`.zshrc` 中 `eval "'builtin' 'trap' 'print -r -- EXITTRAP-ran' 'EXIT'"`，再在函数里 `trap - EXIT`，`exit` 时仍输出 `EXITTRAP-ran`；`zsh -i -c` 时同样执行。在匿名函数里 `zshexit_functions+=(f)` 后，`.zshrc` 里 `exit 3`、命令行 `exit`、Ctrl-D 都执行了 `f`，`exec /bin/echo` 没有。
- 第 4 条：`.zshrc` 中 `exec {myfd}>&1; /bin/ls /dev/fd` 列出 12（`myfd`）。
- 第 5 条：片段带 `sched +0 _tildy_instant_finish` 时调试日志在第一个提示符出现 `send 1`/`ready 1` 后紧接 `send 2`/`ready 2`，`recv 1` 被 `discard 1 stale`，屏幕上多一个提示符；去掉后每个周期一个 `precmd`。
- 读 `.zshrc` 时 `$options[shinstdin]` 为 `off`（`precmd` 里为 `on`）；`zsh -i -c` 时 `ZSH_EXECUTION_STRING` 非空。

### PTY harness

新增场景（`python3 tests/pty/run.py --real --scenario 'instant_*'`，真实二进制，harness 的 `.zshrc` 在 `TILDY_TEST_INSTANT=1` 时把片段放在第一行，`TILDY_TEST_PRE_INIT` 模拟慢初始化或插件输出）：

| 场景 | 观测 |
| --- | --- |
| `instant_prompt_shows_before_init` | 先由一个会话执行一条命令写入缓存；新会话在 500 ms 初始化期间屏幕上已是缓存提示符，日志 `instant 1 hit` 在 `.zshrc` 开始后 5–6 ms；`instant_done` 在其后约 580–600 ms；初始化后屏幕上只有一行提示符、在第 0 行，光标在该行；第一个命令回车后 `transient 1`，提示符折叠为 `❯ echo first_cmd_ok` |
| `instant_prompt_keeps_typed_input` | instant prompt 出现后键入 `echo marker`：初始化期间屏幕上没有回显；初始化后缓冲区为 `echo marker`、光标在其后；回车输出 `marker`，命令只出现一次 |
| `instant_prompt_shows_init_output` | 初始化中 `echo warning-from-plugin; print -u2 error-from-plugin`：期间不上屏；之后两行出现在提示符上方，`instant_done` 报告捕获字节数；之后 stdout、stderr 正常，`$TMPDIR` 下没有 `tildy-instant.*` |
| `instant_prompt_exit_during_init` | 初始化中 `echo output-before-exit; exit 3`：`instant_done … exit`，Shell 退出后屏幕上只剩这一行输出，instant prompt 已擦除 |
| `instant_prompt_off` | `TILDY_INSTANT_PROMPT=off`：500 ms 初始化期间屏幕为空，没有 `instant` 事件，之后提示符正常 |
| `instant_prompt_two_lines` | 默认配置写下一行缓存后改用两行的 `pastel-powerline`：首个结果 `instant_replaced 1`，屏幕换成一行基础提示符；两行缓存写入后新会话的 instant prompt 占第 0–1 行，初始化后原位替换为两行真实提示符，第一个结果未被 `lines_mismatch` 丢弃 |
| `instant_prompt_subst_injection` | `prompt_subst prompt_bang` 打开、目录名 ``x$(touch PWNED1)y`touch PWNED2` ``：缓存行原样带着 `$(...)`，instant prompt 与第一个周期的基础提示符都原样显示，没有生成 `PWNED*` |

全量结果（最终代码，2026-10-20）：

| 运行 | 通过 | 失败 | 跳过 |
| --- | --- | --- | --- |
| `python3 tests/pty/run.py`（含 7 个 instant 场景，用真实二进制） | 52 | 0 | 0 |
| `--real` | 22 | 0 | 30 |
| `--real --plugins fzf` | 20 | 0 | 24 |

全部运行结束后 `pgrep -fl "tildy worker|fake-backend"` 无输出。`python3 shell/zsh/selftest/selftest.py`：20/20。`cargo test`：全部通过（`session::instant` 5 项、`session::worker` 2 项为新增）。期间 `worker_degrade_to_oneshot`（`--real`）失败过一次：kill 发生在 partial 之后、fresh 之前，在途请求被重发并立即重启 worker，下一周期于是走了 worker。这是场景本身的竞态（与 instant 无关，该场景不发缓存键），已改为先等 fresh 再 kill。

反证：去掉 `_tildy_precmd` 开头的 `_tildy_sample_opts` 后，`instant_prompt_subst_injection` 失败（`command substitution ran: ['PWNED2', 'PWNED1']`）：第一个周期把缓存内容写进 `PROMPT` 时还没有采样到用户打开的 `prompt_subst`。

手工验证（`zsh -i` 加独立 `ZDOTDIR`）：`TILDY_INSTANT_PROMPT=verbose` 在捕获输出之后多一行说明；`zsh -i -c 'echo hi'` 不触发片段；`.zshrc` 在片段之后 `precmd_functions=()` 时由主脚本的 `_tildy_precmd` 恢复（`instant_done … tildy`），初始化输出照常出现；`.zshenv` 中定义 `alias stty=… exec=… print=… read=… zmodload=…` 不影响片段；Shell 在初始化中被杀后 `$TMPDIR` 没有残留文件。

### 基线

`python3 bench/run.py --group instant_prompt --samples 50 --out <dir>`（target/release/tildy，Apple M1 Pro，zsh 5.9，p10k git 5c7ad75，PTY 100×30，`TERM=xterm-256color`）。每个配置在初始化里都有同样的 `sleep 0.15`，cwd 为空的临时目录；每个样本是一次冷启动的 `zsh -i`，从 `pty.fork` 之前计时；先做一次不计时的启动来填缓存（Tildy 执行一条命令让 worker 写缓存，p10k 等待它自己的 dump）。`visible` 为 PTY 上第一次出现提示符，`ready` 为可输入（Tildy：调试日志 `ready`；clean：标记出现；p10k：第二个 `SKRDY`，即 zle 画出的真实提示符），`fresh` 为第一个 fresh 结果画出。配置见 `bench/zdotdir-{clean_sleep,tildy_sleep,tildy_instant,tildy_instant_static,p10k_instant}`；`tildy_instant` 用 `eval "$(tildy init zsh --instant)"`，`tildy_instant_static` 用预先生成的文件。

| 配置 | visible 中位 / p95 | ready 中位 / p95 | fresh 中位 / p95 |
| --- | --- | --- | --- |
| `clean_sleep`（基础 Zsh） | 192.1 / 204.3 | 192.1 / 204.3 | — |
| `tildy_sleep`（无 instant） | 206.6 / 218.7 | 205.7 / 217.9 | 229.6 / 241.6 |
| `tildy_instant`（eval） | 25.4 / 42.8 | 219.4 / 240.0 | 236.4 / 256.3 |
| `tildy_instant_static`（source 文件） | 20.7 / 29.6 | 214.3 / 224.8 | 232.5 / 240.8 |
| `p10k_instant` | 20.9 / 27.7 | 234.0 / 242.9 | — |

单位 ms，n=50，缓存命中 50/50。采集时机器负载 5.1–9.3（另一任务在编译）；更早一轮负载 3.5–4.8 的结果几乎相同（clean 188.6/202.7；`tildy_instant` visible 24.5/36.8、ready 218.6/240.2；`tildy_instant_static` 21.0/29.4、215.4/238.1；`p10k_instant` 20.5/32.6、234.5/252.4）。

- 首屏：instant prompt 在启动后约 21–25 ms 出现，与初始化长度无关，比基础 Zsh 早约 170 ms；文件形式与 p10k 持平，`eval` 形式多一次 `tildy` 启动（中位 +4.7 ms，p95 +13 ms；在 Python 里单独计时 `tildy init zsh --instant` p50 3.8 ms，`/usr/bin/true` 2.6 ms）。
- 可输入：相对不带 instant 的 Tildy，文件形式中位 +8.6 ms、p95 +6.8 ms，`eval` 形式中位 +13.7 ms、p95 +22.1 ms（两次 `stty`、片段解析、`eval` 的进程）。相对基础 Zsh，文件形式中位 +22.2、p95 +20.4 ms，`eval` 形式中位 +27.3、p95 +35.7 ms，在"首屏与首条命令各自额外 p95 ≤ 50 ms"之内，但这是负载下的数据，需要在空闲机器上复测才能作为发布证据。Tildy 的 `ready` 记在 zle 画提示符之前，clean 与 p10k 是提示符字节到达 PTY 的时刻，口径上对 Tildy 略有利（通常不到 1 ms）。
- 第一个 fresh 结果在 `ready` 后 17–24 ms 画出（第一个提示符走一次性 `render`），缓存命中时这段时间屏幕上是缓存内容而不是基础提示符。

## 后果

- 正面影响：
  - 慢初始化期间立即看到（通常完全正确的）提示符，且不吞字、不遮挡、初始化输出不丢；第一个提示符没有"基础提示符 → 结果"的闪烁。
  - 首屏与可输入分别有日志事件和基线组，初始化成本没有被隐藏。
  - 键只在 Shell 定义；worker 只做校验和原子写入；一次性模式不写状态，`env` 修订不受影响。
- 负面影响与代价：
  - 缓存内容可能过时：上个会话在同一目录最后一次 `fresh` 时的 Git 分支、脏状态、语言版本会短暂显示，直到第一个 partial（通常在 `ready` 之后几到几十毫秒）替换它。与 p10k 相同，接受。配置不在键里：改了配置的第一次启动短暂显示旧样式；行数变了时由 `instant_replaced` 处理。
  - 两次 `stty` fork 计入首条命令；`eval` 形式在首屏之前多一次 `tildy` 进程启动（文件形式没有）。
  - 第一个提示符通常由 `render` 画出（ADR 0003），缓存由 worker 从第二个提示符起写入；从未执行过命令的会话不会刷新缓存。
- 已知限制（文档化，不处理）：
  - 需要交互输入的初始化：stdin 是 `/dev/null`；`read -q`/`read -k` 直接读终端，问题文字却进了捕获文件，看起来像卡住，直到按键（按键会被它吃掉）。这类代码以及 `exec tmux`、`exec zsh` 之类必须放在片段之前（`exec` 出去的程序继承被重定向的 fd，zshexit hook 也不会执行）。
  - 直接写 `/dev/tty` 的程序（pinentry-curses、部分 `ssh-add` 提示）会移动光标，擦除位置随之错位。
  - `.zshrc` 在片段之后 `cd`：instant prompt 显示启动目录；第一个周期改用普通基础提示符。启动后立刻改变窗口大小或 `COLUMNS`：键不同，未命中。
  - 片段运行时光标不在第 0 列（`.zshenv`/`.zprofile` 输出了不带换行的内容）：行数计算偏差。
  - 初始化期间启动的第三方后台进程继承保存的 tty 副本（fd ≥ 10）和捕获文件作为 stdout/stderr（之后的输出丢失）。
  - 使用 ECMA-48 控制序列（CHA、CUU、ED、SGR 0），`TERM=dumb` 时跳过；对 `{` 定义全局别名会破坏加载器（与主脚本相同）。
  - 只在 macOS 验证；Linux 验证仍在 M4。
- 需要后续处理的问题：
  - HELLO 之前的请求排队给 worker（ADR 0003 后续项）后，第一个提示符也能刷新缓存。
  - 是否把配置修订放进缓存文件，命中时与当前配置不符就不用（需要在片段里读配置文件的 mtime，`zsh/stat`）。
  - Linux 与更多终端（tmux、Ghostty、iTerm2）的实测。
