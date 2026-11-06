# Tildy PTY 测试 harness（M0 任务 B，M1 任务 A 扩展）

针对 `docs/architecture.md` 的"Shell 协议 v0"与"PTY harness 契约"编写，M1 按 `docs/protocol.md`"PTY 回归（任务 A）"升级到协议 v1 并能驱动真实二进制。用 pexpect 启动 `zsh -i`，用 pyte 维护虚拟屏幕，断言同时依据屏幕内容与 `TILDY_DEBUG_LOG`。仅依赖标准库、pexpect、pyte。

## 运行

```sh
python3 tests/pty/run.py                              # 全部场景（含三个插件变体）
python3 tests/pty/run.py --list                       # 列出场景名
python3 tests/pty/run.py --scenario typing_during_refresh
python3 tests/pty/run.py --scenario 'plugins_*'       # 支持 glob，可重复或逗号分隔
python3 tests/pty/run.py --plugins autosuggestions,fzf   # 只跑这些插件变体；none 表示不跑插件变体
python3 tests/pty/run.py --json bench/results/pty.json
python3 tests/pty/run.py --tildy-script PATH --backend PATH
```

真实二进制：

```sh
cargo build                                           # 二进制内嵌 shell/zsh/tildy.zsh，改过脚本要重新编译
python3 tests/pty/run.py --real                       # 支持真实二进制的场景全部用 eval "$(target/debug/tildy init zsh)"；只支持假后端的场景 skip
python3 tests/pty/run.py --real --plugins fzf
python3 tests/pty/run.py --tildy-bin target/release/tildy --real
```

fzf 变体会验证 `fzf-history-widget` 已加载。新版使用 `fzf --zsh`，Ubuntu 24.04 的 fzf 0.44.1 使用 `/usr/share/doc/fzf/examples/` 中的补全与按键脚本；仅存在可执行文件不算插件就绪。Linux 最小镜像需要保留该目录，见 [Dockerfile](../linux/Dockerfile)。

不加 `--real` 时，只支持真实二进制的场景（`zero_config_real_binary`、`*real_slow_git_refresh`）也会用真实二进制跑。真实二进制不可用时这些场景标记 skip，原因写在 reason 里：二进制不存在、`tildy init zsh` 内嵌的脚本与 `shell/zsh/tildy.zsh` 不一致（需要重新 `cargo build`）、或 `tildy render` 不能回答一个 REQ。

其他选项：`--fake-mode oneshot|persistent`（没有固定后端形态的 M0 场景用哪种，默认 oneshot）、`--plugin-dir`（默认 `$TILDY_TEST_PLUGIN_DIR` 或 `~/.oh-my-zsh/custom/plugins`）、`--timeout-scale N`（慢机器放大所有超时）、`--keep`（保留临时目录）、`-v`（每个场景都打印证据）。

默认使用 `shell/zsh/tildy.zsh` 与 `bench/fake-backend.sh`；任一不存在时回退到 `tests/pty/stubs/` 下的桩，并在输出开头打印 `NOTICE: using stub ...`（此时结果只是 harness 自测，不代表真实实现）。有任何场景失败则退出码为 1，参数错误为 2。

## 隔离

- `ZDOTDIR=tests/pty/zdotdir`：`.zshenv` 关闭 `GLOBAL_RCS`，`.zshrc` 不加载用户的 `~/.zshrc`。`.zshrc` 设置 `PS1='%~ %# '`、`no_beep`、私有 `HISTFILE`、`compinit`（`menu select` + `menu_complete`），按 `TILDY_TEST_PLUGINS` 加载插件，最后 `source $TILDY_SCRIPT`（真实二进制模式为 `eval "$($TILDY_TEST_INIT_BIN init zsh)"`），再执行场景给的 `TILDY_TEST_POST_INIT`。instant prompt 场景设置 `TILDY_TEST_INSTANT=1`：`.zshrc` 第一行记下开始时间（`$TILDY_TEST_TMP/zshrc-start`）并 `eval "$($TILDY_TEST_INIT_BIN init zsh --instant)"`，随后执行 `TILDY_TEST_PRE_INIT`（慢初始化或插件输出）。
- 每个场景有独立的 `XDG_CONFIG_HOME`/`XDG_STATE_HOME`/`XDG_CACHE_HOME`，真实二进制读不到用户配置，快照也不写进用户目录。
- 清除可能干扰的环境变量（`ZLE_*`、`ZSH*`、`STARSHIP*`、`P9K*`、`GHOSTTY*`、`TERM_PROGRAM*`、`TMUX*`、旧的 `TILDY_*` 等）；`TERM=xterm-256color`；屏幕 100x30。
- 每个场景使用独立临时目录（`/tmp/sk-xxxx/home0`，内含 `dirA`、`dirB`）、独立调试日志、独立历史文件（预置一条 `echo seedcommand_for_suggest`，供 autosuggestions 变体使用）。
- 每个场景结束后遍历 zsh 的进程树，按进程组与 pid 强杀（包括后端与 `sleep`），再删除临时目录。

## 场景与契约对应

| 契约项 | 场景名 | 要点 |
| --- | --- | --- |
| 1 | `baseline_prompt` | 延迟 400 ms，先观察到基础提示符，再出现 `[g1 home0] %`，日志有 `send`/`accept` |
| 2 | `slow_backend_not_blocking` | 延迟 2 s，`echo hi` 须在 2 s 内输出 |
| 3 | `ab_out_of_order` | `cd dirA`（1500 ms）后立刻 `cd ../dirB`（10 ms）；A 的代次不得 accept；最终提示符是 B；等满 2.6 s 确认 A 不再出现 |
| 4 | `typing_during_refresh` | 延迟 500 ms，逐字符输入 45 字符 `echo`，要求 `recv` 落在输入窗口内；检查提示符已更新、文本完整、光标位置、左移 5 格后光标；回车后回显必须逐字相同 |
| 5 | `completion_during_refresh` | A：`ls /us`+Tab 补全为 `/usr/`，回包后行不变，回车执行；B：`ls /`+Tab 打开补全菜单，回包落在菜单内，要求日志有 `defer`，菜单仍在，退出菜单后提示符被应用，随后命令照常执行 |
| 6 | `history_search_during_refresh` | 先执行 `echo HISTMARK123`，再 Ctrl-R 输入 `HISTM`，回包后 `bck-i-search` 界面与匹配行仍在，回车重新执行（该输出共出现 2 次） |
| 7 | `vi_mode` | 先 `bindkey -v`，重做场景 4；另验证 ESC `0` 与 `A` 的光标位置 |
| 8 | `resize_during_refresh` | 延迟 1 s，回包前 `setwinsize(30,80)`；要求旧代次 `discard stale`，更新代次 accept 且屏幕显示更新代次 |
| 9 | `half_frame` / `bad_gen` / `backend_fail` | 导出对应 `TILDY_FAKE_*`；日志有 `discard`，无 `accept`，提示符保留为基础提示符；随后 `unset` 变量，下一个提示符恢复 |
| 10 | `backend_timeout` | 延迟 8 s，`TILDY_BACKEND_TIMEOUT_MS=2000`；要求 `discard timeout`，随后新提示符正常 |
| 11 | `persistent_crash` | `TILDY_BACKEND_MODE=persistent`、`TILDY_FAKE_CRASH_AFTER=2`；若日志没有 `backend_start ... persistent` 则 skip；否则要求崩溃被记录且 shell 仍可用，是否重启仅记录为备注 |
| 12 | `plugins_<插件>_{baseline_prompt,typing_during_refresh,completion_during_refresh}` | 对 autosuggestions、syntax-highlighting、fzf 各重做场景 1、4、5；插件目录/`fzf` 缺失则 skip。syntax-highlighting 额外断言命令词仍有高亮色；autosuggestions 额外断言回包后建议后缀仍显示 |
| 13 | `source_twice` | 再 `source` 两次；`hook_installed` 的 precmd/chpwd/winch 各恰好 1 条；`precmd_functions`/`chpwd_functions` 无重复项 |

## M1 场景（协议 v1）

后端一栏：`fake` 只用假后端，`real` 只用真实二进制，`both` 默认用假后端、`--real` 时用真实二进制。

| 场景名 | 后端 | 要点 |
| --- | --- | --- |
| `zero_config_real_binary` | real | 无配置文件，`eval "$(tildy init zsh)"`；出现含目录的默认提示符，HELLO 被接受，之后的请求走 worker，没有 `backend_degraded` |
| `accepted_frame_is_visible` | both | 在 `accept` 后用 FIFO 暂停回调，检查终端已经显示完整新提示符；真实模式同时检查右侧退出状态。验证先绘制、再确认，不依赖后续清理或按键触发刷新。 |
| `worker_degrade_to_oneshot` | both | kill 空闲 worker：`backend_exit`、`backend_degraded eof`，下一个提示符由 `render` 画出（epoch 0），随后 worker 重启并服务请求；假后端另测 kill 在途请求：`discard eof`，同代次改走 oneshot 重发并画出 |
| `version_mismatch_restart` | both | 第一个 worker 报错误版本：BYE（trace 中可见）、重启、新 epoch 服务后续请求；每个 worker 都不符时 1 + 3 次启动后 `restart_limit`，之后只走 oneshot 且没有残留 worker |
| `instant_prompt_miss_uses_sibling_height` | real | 新目录未命中：基础提示符按同配置其它目录缓存的行数补高，两行结果在首周期即画出，无 `lines_mismatch` |
| `transient_keeps_blank_line` | real | `add_newline` 配置下回车折叠后，`❯ echo …` 上方仍是空行（T 行多行），输出紧跟其下 |
| `transient_prompt` | both | 回车、Ctrl-C、isearch 接受都折叠为 `❯ `；`false` 之后 `echo rc=$?` 输出 `rc=1`，REQ 的 last_exit 为 1；两行提示符折叠为一行 |
| `blank_line_clears_exit_status`（含三个插件变体） | real | compact/full 下空回车及纯空白输入清除退出码和错误颜色，真实 `$?` 与已有摘要保留；缩放保持显示状态，后续失败和语法错误仍显示退出码 |
| `multi_line_result` | fake | 首个空输入周期即显示两行，后续输入与光标正确 |
| `height_change_with_input` | fake | 输入非空时高度变化暂缓；Ctrl-U 清空后自动应用，不需要回车 |
| `reload_newline_height` | real | 首次三行提示符和 3→2→3→2 重载立即显示，保留命令输出与光标 |
| `resize_idle` / `resize_idle_worker` | fake / both | 没有在途请求时缩放，不按键也会发新代次并画出 |
| `nested_shell` | both | zsh 里再开 zsh：内层有自己的 worker；内层 `exit` 后其 worker 2 秒内退出，外层 worker 仍在 |
| `fast_cd_burst` | both | 一次输入 10 条 `cd`：10 个提示符、至少 10 个请求、代次 +20（每条 `cd` 的 chpwd 与 precmd 各递增一次，只有 precmd 发请求）；最后画出的是 dirB，画出的代次单调递增，zsh 下只剩 worker 一个子进程 |
| `terminal_kill_no_orphan` | both | 对 zsh 发 SIGKILL、SIGHUP，worker 2 秒内退出 |
| `partial_then_fresh` | fake | 同一代次 `partial`、`fresh`、迟到的第二个 `fresh` 依次画出，后到的覆盖先到的，输入与光标不受影响 |
| `worker_epoch_mismatch` | fake | RES 的 epoch 与 HELLO 不同：`discard bad_frame`、`backend_degraded bad_frame`，worker 被 kill，同代次改走 oneshot 并画出 |
| `prompt_subst_injection` | both | `setopt prompt_subst prompt_bang` 下进入名为 `x$(touch PWNED1)`、`` y`touch PWNED2` ``、`x$(touch /tmp/tildy-pwned-<pid>)`（嵌套路径）的目录：提示符原样显示名字，没有文件被创建 |
| `real_slow_git_refresh`（含三个插件变体） | real | `PATH` 前置一个先 sleep 0.4 s 的 `git`：partial 立即画出，fresh 在逐字输入中途到达，行内容与光标正确（syntax-highlighting 保持高亮，autosuggestions 保持建议）；补全菜单打开时 fresh 到达记 `defer`，离开菜单后 `apply_deferred` |
| `source_twice` | both | 同 M0；真实二进制下重复 `eval "$(tildy init zsh)"` |

所有场景结束后还检查调试日志里的 `discard` 原因只出现契约枚举（`stale`、`half_frame`、`eof`、`timeout`、`bad_frame`、`lines_mismatch`）。

## instant prompt 场景（ADR 0006）

均为 real。每个场景先用一个带片段的会话执行一条命令，让 worker 写下 `home0`、100 列的缓存（`populate_instant`）。

| 场景 | 判定 |
| --- | --- |
| `instant_prompt_shows_before_init` | 500 ms 初始化期间屏幕上已是缓存提示符（`instant … hit`，距 `.zshrc` 开始 < 100 ms，早于 `instant_done`）；之后只剩真实提示符、行数与缓存一致、无残留行；第一个提示符的 transient 正常 |
| `instant_prompt_keeps_typed_input` | 初始化期间键入 `echo marker` 不回显；之后在行编辑器中，回车执行 |
| `instant_prompt_shows_init_output` | 初始化的 stdout/stderr 出现在提示符上方；之后 stdout、stderr 正常；没有残留捕获文件 |
| `instant_prompt_exit_during_init` | `.zshrc` 中途 `exit`：zshexit hook 擦除 instant prompt 并打印捕获的输出 |
| `instant_prompt_off` | `TILDY_INSTANT_PROMPT=off`：初始化期间不输出任何东西 |
| `instant_prompt_two_lines` | 两行 preset：过时的一行缓存在首个空输入周期换成两行主题；两行缓存占两行并原位替换，首个结果不被 `lines_mismatch` 丢弃（preset 不是两行时 skip） |
| `instant_prompt_subst_injection` | `prompt_subst` 下目录名含 `$(...)` 与反引号：缓存提示符与首个基础提示符原样显示，不执行 |

## M2 场景（Agent 状态）

都只用真实二进制（`docs/agent-status.md`）。harness 用 `tildy agent … --json` 在场景自己的 `XDG_STATE_HOME` 下发布与查询记录。

| 场景名 | 要点 |
| --- | --- |
| `agent_status_push` | 空闲提示符下（输入行已有文字）从外部发布 global 的 blocked 记录：1.5 s 内出现在右侧，输入不变；TTL 3 s 到期后 1.5 s 内消失，且不早于 2.5 s |
| `agent_status_scopes` | Shell 在 repoA：repoB 的 worktree 记录与另一会话的记录 2 s 内都不出现；repoA 的记录 1.5 s 内出现；`cd ../repoB` 后只显示 repoB 的记录 |
| `agent_status_prompt_rule` | 以本 Shell 的 `TILDY_SESSION_ID` 为 origin：working 记录推送出现、下一个提示符消失；done 记录推送出现、下一个提示符仍显示、再下一个消失；存储清空 |
| `agent_wrap_known_program` | PATH 上的假 `claude`（发一条 OSC 7501 blocked 报告后 sleep，退出码 3）被零配置包装：`whence -w` 为 function，运行中存储里有 `wrap/claude/<pid>` 的 blocked 记录（`source: osc`），`rc=3`；`.zshrc` 里的用户别名 `codex` 不被覆盖；`TILDY_AGENT_WRAP=0 claude` 不经过代理、不产生记录 |

## 缩放场景（ADR 0002 修订"缩放与终端重排"）

假后端的 `TILDY_FAKE_POWERLINE=1` 加 `TILDY_FAKE_WIDE=90`：第一行是 `[g<代次> <目录>]----WIDEEND`，宽 `min(90, 请求列数)`，像按宽度收缩的渲染器；场景先执行 `echo ABOVE_1`，检查提示符正上方仍是这行输出、屏幕上没有提示符碎片、输入行与光标不变（`prompt_block`）。pyte 缩窄时只截断不重排，所以重排相关的场景用 harness 里的 `ReflowMixin`：记住软折行，宽度变化时把折行拼回逻辑行、按新宽度重新折行、光标留在逻辑行里的同一格（Ghostty/iTerm2 的行为模型，不是任何终端的复制品）。

| 场景名 | 后端 | 要点 |
| --- | --- | --- |
| `resize_narrow_fits` | fake | 100 → 50 列：立刻一次 `resize_fit 100 50 0 1`，提示符行被截到 49 列（旧代次的行），输入保留；之后只有一个请求，距缩放 ≥ 50 ms（防抖）；新代次的 50 列结果占一行显示；回车执行原输入 |
| `resize_burst_debounced` | fake | 一次缩放让请求在途（后端 500 ms），随后 15 ms 间隔连续缩放 10 次到 60 列：在途代次 `discard stale` 且从未画出；整个连拍只递增一次代次、只发一个请求，且在最后一次缩放之后；最终 60 列的行完整 |
| `resize_reflow_no_fragments` | fake | 重排屏幕、`TILDY_REFLOW=1`：100 → 40（`g=2`）→ 70 → 55（`g=1`）→ 100，每一步临时提示符与新结果都无碎片，提示符上方的输出与更早的折叠提示符都没被擦掉 |
| `resize_reflow_without_fixup_leaves_fragments` | fake | 对照：同样的重排屏幕、`TILDY_REFLOW=0`，100 → 40 后必须看到碎片（证明上一个场景测到了修正） |
| `semantic_marks_kept` | fake | `TILDY_TEST_POST_INIT` 装一个仿 Ghostty 集成的 precmd 钩子（在 tildy 之后给 PS1 包上 OSC 133 的 A、P;k=s、B）：异步结果的重绘、缩窄后的截断重绘、transient 重绘的原始输出里都还有这些标记 |
| `resize_narrow_real` | real | 真实二进制、pastel-powerline、深目录与长分支的仓库、重排屏幕：120 → 50 → 36 列，每一步临时提示符与新结果的 powerline 行都不超过列数、只有一行、上方仍是 `ABOVE_1` |

## 输出

标准输出为逐场景结果加汇总表；失败场景会打印最后屏幕快照与调试日志尾部。`--json` 文件结构：`{tildy_script, backend, using_stub, summary, results:[{name, status, duration_s, reason, notes, evidence:{screen, cursor, log}}]}`。

## 桩

`stubs/tildy.zsh` 与 `stubs/backend.sh` 只用于自测 harness：前者是最小的异步 oneshot 实现，后者支持 `TILDY_FAKE_DELAY_MS/DELAY_RULES/FAIL/HALF_FRAME/BAD_GEN/PERCENT`，不支持 persistent。`stubs/tildy-bin` 模拟真实二进制的 `init zsh`/`worker`/`render`（内部用假后端，提示符字符为 `❯` 并带 `T` 行），用 `--real --tildy-bin tests/pty/stubs/tildy-bin` 在没有编译产物时自测真实二进制路径。

## 实现该契约时发现的问题

见任务 B 的交付报告（`$CONTEXT`/`$KEYMAP`/`$WIDGET` 在 `zle -F` 回调内为空、kill 旧后端导致 A 的 stale 日志不可观察等）。
