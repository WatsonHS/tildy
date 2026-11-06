# 0002：编辑中安全重绘的判定与延迟应用

- 日期：2026-10-19
- 状态：提议中
- 修订：2026-10-20，缩放与终端重排（文末）；其中终端重定位职责已由 [ADR 0013](0013-terminal-prompt-ownership.md) 修订（2026-10-23）。

2026-10-22 修订：提示符行数不变的约束仅适用于有输入的编辑状态；输入为空且通过安全状态检查时允许重绘新高度，见 [ADR 0003 行数沿用](0003-zsh-v1.md#行数沿用)。

## 背景

后端结果通过 `zle -F` 在 zle 等待按键时到达，此时用户可能正在输入、补全菜单中、增量搜索中、续行（PS2）中或 `vared` 中。契约（`docs/architecture.md` "Zsh 侧行为" 第 3、4 条）要求：只在安全编辑状态调用 `zle reset-prompt`；不安全时暂存结果，到下一个安全时机再应用，代次已变则丢弃；重绘不得改变 `BUFFER`、`CURSOR`、`region_highlight` 和提示符行数。契约给出的候选判断是 `$CONTEXT == start`，具体条件由任务 A 验证。

任务 B（PTY harness）独立报告：在 `zle -F` 回调里 `$CONTEXT`、`$KEYMAP`、`$WIDGET`、`$ZLE_STATE` 全为空。本 ADR 的实验确认了这一点，并找到了能读到这些参数的调用方式。

## 决定

fd 回调保持为普通函数（`zle -F fd _tildy_on_fd`，不加 `-w`），只负责读帧和校验。当前代次的完整帧通过 `zle _tildy-apply -f nolast` 交给一个 widget 判定，这样 zle 参数可见，且 `LASTWIDGET` 不被改写。

满足以下全部条件时视为安全，此时更新 `PROMPT`/`RPROMPT` 并调用 `zle .reset-prompt`：

1. `zle` 处于活动状态；
2. `$CONTEXT == start`；
3. 没有补全 widget 正在执行（`_tildy_busy == 0`）；
4. 不在增量搜索中（`_tildy_isearch == 0` 且 `$ISEARCHMATCH_ACTIVE != 1`）。

不满足时只保留最新一个结果，在 `zle-line-pre-redraw` hook 中重试；代次变化时以 `discard stale` 丢弃。

`_tildy_busy` 由补全 widget 的薄包装维护：首个 precmd 时包装所有 `completion:*` 类型的 widget，以及 `expand-or-complete`、`complete-word`、`menu-select` 等内建补全名。包装函数进入时加一，退出时在 `always` 块里减一。`_tildy_isearch` 由 `add-zle-hook-widget` 注册的 `isearch-update`/`isearch-exit` hook 维护。

## 具体机制

- 判定函数：`_tildy_unsafe`（`shell/zsh/tildy.zsh`）。`defer` 日志的原因字段为 `context=<CONTEXT>`、`widget=<WIDGET>`（补全中，记录正在运行的顶层 widget，如 `expand-or-complete`、`fzf-completion`）、`state=isearch` 或 `zle=inactive`。
- 延迟应用：`_tildy-pre-redraw` 通过 `add-zle-hook-widget line-pre-redraw` 注册。它在每个 widget 执行后、重绘前运行，包括补全 widget 返回之后（菜单用回车结束时，zle 不再执行其他 widget，但这个 hook 仍会运行）和 isearch 退出之后。
- 日志语义（应 harness 要求）：
  - 当场画出：`recv` → `accept`；
  - 延迟后画出：`recv` → `defer` → `apply_deferred`；
  - 始终没画：`recv` → `defer` → `discard stale`。
  - 每个结果都先有 `recv`。
- 不变量：
  - 只改 `PROMPT`/`RPROMPT`，不碰 `BUFFER`、`CURSOR`、`region_highlight`。
  - 调用 `.reset-prompt`（点号内建名）以绕过其他插件的包装。
  - 应用前检查结果的 `L` 行数等于当前提示符行数，否则 `bad_frame`（见 ADR 0001）。

## 证据

复现命令：`python3 shell/zsh/selftest/zle_probe.py`。它独立于 tildy.zsh，用计时 fd 触发回调，并用 pyte 渲染屏幕。以下为 2026-10-19 在 zsh 5.9（arm64-apple-darwin）上的输出摘要。

### 1. 回调里能看到什么（probe 1、4）

| 调用方式 | 看到的参数 | 之后下一个 widget 看到的 `LASTWIDGET` |
| --- | --- | --- |
| 普通回调 `zle -F fd fn` | `CONTEXT`、`WIDGET`、`LASTWIDGET`、`KEYMAP`、`ZLE_STATE` 全部未设置；`zle` 返回 0（可调用 widget） | `self-insert`（未变） |
| widget 回调 `zle -F -w fd w` | `CONTEXT=start WIDGET=_h_widget LASTWIDGET=self-insert KEYMAP=main` | `_h_widget`（被改写） |
| 普通回调内 `zle w -f nolast` | `CONTEXT=start WIDGET=self-insert LASTWIDGET=self-insert KEYMAP=main` | `self-insert`（未变） |

结论：
- 契约里的 `$CONTEXT == start` 不能在普通回调里直接判断。
- `-w` 回调能看到参数，但每次回调都改写 `LASTWIDGET`，会破坏依赖它的 widget（如 `up-line-or-beginning-search` 判断是否连续按键）。
- 普通回调里用 `zle widget -f nolast` 再调一次，两个问题都避免了。
- 集成验证：在 tildy.zsh 下输入 `a`，等结果在下一次按键前画出，随后自定义 widget 读到 `LASTWIDGET=self-insert`。

### 2. 哪些状态下 `reset-prompt` 真的有害（probe 3）

| 状态 | `reset-prompt` 后的屏幕 | 判定 |
| --- | --- | --- |
| 普通输入 | `NEW> echo hello`，文本与光标正确 | 安全 |
| 菜单选择（menu select） | 前：`ls /Applications/` 下有 3 行 16 项菜单；后：菜单塌成只剩 `Applications/` 一项 | **破坏** |
| 普通补全列表（无菜单） | 列表完整保留 | 实测无害，但仍延迟（见下） |
| 增量搜索 | `NEW> echo foo1` / `bck-i-search: fo_` 保持，继续搜索正常 | 实测无害，保守起见延迟 |
| 续行 PS2（`CONTEXT=cont`） | `for>` 变成 `>`（另一次实验变成 `for then>`） | **破坏** |
| vi 命令模式 | 正常 | 安全 |
| `execute-named-cmd` 小缓冲区（M-x） | `execute: _` 保持，回车后命令照常执行 | 实测无害，未拦截 |
| `quoted-insert` 等待按键（^V） | 正常，后续插入的字符正确 | 实测无害，未拦截 |

`vared`（`CONTEXT=vared`）与 `select`（`CONTEXT=select`）显示的不是 `PROMPT`，统一按 context 规则延迟。

### 3. 为什么用包装计数而不是 `LASTWIDGET`/`KEYMAP`（probe 2）

| 场景 | `WIDGET` | `LASTWIDGET` | `KEYMAP` |
| --- | --- | --- | --- |
| 2a 菜单选择中（`menu_complete`，第一次 Tab 即进入菜单） | `expand-or-complete` | `self-insert`（上一个已完成的 widget） | `main` |
| 2b 唯一补全已完成 | `expand-or-complete` | `expand-or-complete` | `main` |
| 2c 第二次 Tab 才进入菜单 | `expand-or-complete` | `expand-or-complete` | `main` |

- `LASTWIDGET` 只在 widget 结束时更新。"`LASTWIDGET` 是补全 widget 就不安全"这条规则因此两头出错。2a 的菜单判断不出来。2b 的无害情况会被延迟到下一次按键，harness 的 `completion_during_refresh` 第一阶段要求不按键就应用。菜单用回车结束时也不会再有新 widget 执行，结果同样卡住。
- `WIDGET != LASTWIDGET` 能识别"嵌套在一个仍在执行的 widget 里"（2a、3h 的 `WIDGET=quoted-insert LASTWIDGET=self-insert`），但同名 widget 连续执行时会撞车（2c，常见的 Tab Tab 进菜单）。
- 菜单选择使用局部 keymap，`$KEYMAP` 始终是 `main`。`$ZLE_STATE` 只反映插入/覆盖模式和历史模式。
- `zle-line-pre-redraw` 在菜单选择内部也会触发（每次在菜单里按键都触发，`LASTWIDGET` 为上一个 widget），不能单独作为"补全结束"的信号。
- `compprefuncs`/`comppostfuncs` 每次补全后会被清空，只适用于 compsys，而且菜单选择发生在 `_main_complete` 返回之后，没有对应的结束钩子。
- 菜单选择和"do you wish to see all N possibilities?"询问都在补全 widget 内部读键。所以"补全 widget 是否仍在执行"正是需要的条件，包装计数可以精确给出这个状态。插桩实验显示，菜单中按回车后先出现包装函数的 exit，紧接着是 pre-redraw，延迟结果就在这一刻画出。

### 4. 缩放（probe 5 与源码）

- 空闲时缩放：`TRAPWINCH` 触发（`zle` 返回 0），`zle-line-pre-redraw` 不触发。所以缩放的重新请求只能在 trap 里发出。
- zsh 5.9 `Src/Zle/zle_main.c` 的 `raw_getbyte()` 只在 `poll()` 前后 `winch_unblock()`/`winch_block()`，trap 在 poll 被打断时执行。
  - 分发回调时遍历的是 `watch_fds` 的拷贝（`lwatch_fds`），所以在 trap 里新增 fd 不会破坏内存。
  - 但 poll 的 `fds` 数组只在一次回调分发之后才重建，EINTR 之后沿用旧数组。trap 里新注册的 fd 要等下一次回调或下一次按键才被 poll。
  - 有在途请求时，被 kill 的旧进程很快送来 `EXIT`，触发一次回调分发，问题不出现（harness `resize_during_refresh` 通过）。空闲且无在途请求时，新结果要到下一次按键才显示。

### 5. 集成结果

- `python3 shell/zsh/selftest/selftest.py`：16/16 通过。
  - `x_completion_menu_defer`：`defer widget=expand-or-complete`，菜单完整，回车离开菜单后约 0.4 s 记录 `apply_deferred`，提示符变为 `[g2 home] % ls /Applications/`。
  - `x_isearch_defer`：`defer state=isearch`，离开搜索时应用。
  - `c_typing_during_refresh`：结果在 4.8 s 的逐字输入窗口内第 494 ms 到达，行内容、光标位置、`echo` 回显全部正确。
- `python3 tests/pty/run.py --json bench/results/pty-real.json`：23/23 通过。包括补全、历史搜索、vi 模式、缩放，以及 autosuggestions、syntax-highlighting、fzf 三个插件下的基线、输入和补全场景。fzf 下 `defer` 原因是 `widget=fzf-completion`：fzf 的 Tab widget 通过名字调用了被包装的 `expand-or-complete`。
- 菜单选择中按 Ctrl-C：`_tildy_busy` 回到 0（`always` 块生效），延迟结果随即画出。
- `auto_menu` 下"第一次 Tab 列表、第二次 Tab 进菜单"：回包落在菜单中，记录 `defer widget=expand-or-complete`，菜单完整。
- 只开 `menu_complete`、不配 `menu select`，反复按 Tab 循环候选：两次 Tab 之间画出结果后，第二次 Tab 仍从 `/Applications/` 前进到 `/bin/`，与不加载 Tildy 的 zsh 行为一致。

### 6. 给 harness 作者的观察

zsh 在进入 zle 时用 `tcsetattr(TCSADRAIN)`。PTY 主端不持续读取时，zsh 会卡住、不再画提示符，回调也不触发。测试中所有等待都必须边读边等，不能直接 `sleep`。

## 后果

- 正面影响：
  - 菜单选择和续行这两种实测会被破坏的状态都被拦截。
  - 输入、vi 模式、唯一补全、普通列表下结果立即显示。
  - 不改写 `LASTWIDGET`，不需要 `-w` 回调。
  - 与 autosuggestions（建议后缀在刷新后仍在）、syntax-highlighting（命令词仍高亮）、fzf 共存，经 harness 验证。
- 负面影响与代价：
  - 需要包装补全 widget。包装发生在首个 precmd，晚于它重新定义补全 widget 的插件或 `compinit` 会把包装覆盖掉，此后菜单失去保护。
  - 第三方 widget 用点号名（如 `zle .expand-or-complete`）直接调用内建补全时，同样绕过包装。
  - 增量搜索和普通补全列表实测无害但仍延迟，偏保守，结果最多晚到离开该状态时才显示。
  - 菜单中的结果要等菜单结束才显示。
  - 续行（PS2）和 `vared` 期间的结果实际上总会被下一个 precmd 作废。
- 留给 PTY harness 继续探测的问题：
  - "do you wish to see all N possibilities?"询问期间回包，应记录 `defer widget=...`。
  - 在补全菜单中缩放终端。
  - `bindkey -v` 下的 `/` 搜索（`vi-history-search-backward` 使用小缓冲区，未拦截），以及会自己调用 `reset-prompt` 的 `zle-keymap-select` 光标形状插件。
  - 大段粘贴（bracketed paste）过程中回包。
  - 多行 `BUFFER`（例如 `push-line-or-edit` 或插入换行后仍处于 `CONTEXT=start`）时回包。
  - 提示符宽度变化导致折行（很长的目录名）时，视觉行数会变化，当前只按 `L` 行数检查。
  - 延迟加载 `compinit` 或 zsh-autocomplete 这类重绑补全 widget 的插件。
  - fzf 的 Alt-C（在 widget 里 `cd`）走 chpwd 的 zle 分支，应立即请求并显示新目录。
  - 空闲缩放后到下一次按键才更新的问题，M1 可以用自管道唤醒 zle 解决：启动时注册一个 fd，trap 里写一个字节，在回调里完成请求。
  - 增量搜索是否可以放开（实测无害）。

## 修订：缩放与终端重排（2026-10-20）

### 背景

作者在 cmux（基于 Ghostty，缩放时重排文本）里用两行 `pastel-powerline` 日用。快速缩窄 pane 后，比新宽度宽的第一行先折成 3 行，重绘之后旧提示符的碎片（`…/a`、`…/ai-n`）留在新提示符上方。两个原因：渲染器从不把行收缩到终端宽度（另行修复，见 [presets.md 的"窄终端：按优先级收缩"](../presets.md#窄终端按优先级收缩)）；适配层对 SIGWINCH 只是重新请求，靠 zsh 自己的重绘，而 zsh 不知道终端已经把旧行重排过。

### zsh 怎么重绘（zsh 5.9 实测）

把 PTY 输出原样抓下来（提示符 `A×60\n> `，缓冲区 `echo hi`）：

| 操作 | zsh 写出的字节 |
| --- | --- |
| 100 → 40 列 | `\r\r\e[A\e[J` + 提示符 + 缓冲区 |
| 50 → 100 列（60 列的行在 50 列时折成 2 行） | `\r\r\e[A\e[A\e[J` + …… |
| `reset-prompt`，3 行提示符换成 4 行 | `\r\r\e[A\e[A\e[J` + …… |
| `reset-prompt`，4 行（含 1 行折行）换成 2 行 | `\r\r\e[3A\e[J` + …… |

结论：SIGWINCH 时 zsh 先重画（`adjustwinsize`），再运行 `TRAPWINCH`；重画和 `reset-prompt` 都是"回到编辑区第 0 行（`\r`，缓冲区折行时先上移），再上移 *它上次画出的提示符* 的高度减一，然后 `\e[J` 清到屏尾、从那里重画"。这个高度是 zsh 在上次画的那个宽度下算出来的（超过宽度的行按折行计数）。终端如果在 SIGWINCH 之前已经把旧行按新宽度重排，旧提示符实际占的行数变多，zsh 上移得不够，差的那几行就是碎片。

### 光标算术

设提示符各行（按 zle 的算法量出的显示宽度）为 `w_1 … w_n`，`d_i` 是 zle 在宽度 `Xd` 下画出来的宽度（完整提示符为 `w_i`，截断形式为 `min(w_i, Xd - 1)`）。把这样画出的提示符和光标前的缓冲区（`LBUFFER`）放到宽度 `X` 下排，光标所在行比提示符第一行低

```
P(X, Xd) = Σ_{i<n} max(1, ⌈d_i / X⌉) + ⌊(d_n + 宽(LBUFFER)) / X⌋
```

行（缓冲区有换行时，每一行缓冲区按同样的规则各占自己的行数）。于是：

- 缩放前 zle 画在 `Xd = Cd` 宽，SIGWINCH 时它上移的正是 `P(Cd, Cd)`（它自己的模型）；
- 重排之后，旧提示符的第一行在光标上方 `P(C, Cd)` 行；
- 差值 `G = P(C, Cd) − P(Cd, Cd)` 就是 zsh 重画起点上方残留的旧行数。

例：第一行 90 列、第二行 `❯ `，100 → 40 列：`P(100,100) = 1`，`P(40,100) = ⌈90/40⌉ + 0 = 3`，`G = 2`，正是截图里折成 3 行、留下 2 行碎片的情形。

### 处理顺序

全部在 `TRAPWINCH` 里（`_tildy_winch`）。zle 只在等待输入的 `poll` 前后放开 SIGWINCH（第 4 节），所以这个 trap 不会打断任何 widget 或 `zle -F` 回调，且紧跟在 zsh 自己的重画之后：

1. 尺寸与上一个请求不同、且本轮连续缩放还没有递增过代次时，`_tildy_bump`：在途的旧宽度结果从此作废（`discard stale`），一轮连续缩放只递增一次。
2. `zle _tildy-resize -f nolast` 调用 widget（这样 `CONTEXT` 等可见，`LASTWIDGET` 不变）。处于不安全状态（补全菜单、isearch、`vared`、PS2，即 `_tildy_unsafe`）时什么也不做，记 `resize_skip`。
3. 终端会重排（见下）且 `G > 0` 时输出 `\e[{G}A`：光标上移 G 行，zle 不知道这次移动。
4. 完整提示符里有行在新宽度下放不下（上面几行宽于 `C`，最后一行宽于 `C - 1`）时，`PROMPT` 换成截断形式：上面几行写成 `%-1>>行%>>`（从左保留 `COLUMNS - 1` 列），最后一行写成 `%-1<<行%<<`（从右保留，提示符字符与光标留在原处）。这是 zsh 自己的截断，每次展开都按当时的 `COLUMNS` 算，而 SIGWINCH 时 zsh 会重新展开 `PROMPT`，所以之后连续缩放的每一次 zsh 自己的重画都已经不超宽；截掉部分里的转义序列会保留，颜色不乱。
5. `zle .reset-prompt` 加 `zle -R`：zsh 输出 `\r`、上移 `P(C, C)`（它对刚在 C 宽画出的东西的模型），而光标此时比它以为的高 G 行，于是正好落在旧提示符的第一行，`\e[J` 把碎片和它自己刚画的那份一起清掉，在那里画出截断形式。记 `resize_fit <代次> <旧列数> <新列数> <G> <是否截断>`。
6. 写一个字节到 self-pipe。回调（`_tildy_on_wake`）不立即请求，而是启动防抖定时器：一个短命子进程 `zselect -t` 睡到"最后一次 SIGWINCH 之后 80 ms"（`TILDY_RESIZE_DEBOUNCE_MS`）就退出，管道 EOF 经 `zle -F` 唤醒 zle；期间又来了 SIGWINCH 就再睡剩下的时间，否则为第 1 步递增出的代次发一个请求。precmd、chpwd 自己会发请求，并清掉这笔"欠着的请求"。
7. 新宽度的结果按原来的安全重绘规则替换截断形式（zsh 的上移这时是准确的：截断形式是它自己在 C 宽画的）。

为什么不是"自己上移到旧第一行再 `\e[J`"：`reset-prompt` 总是从它以为的光标位置再上移它以为的提示符高度。光标若已在旧第一行，zsh 会再往上移 `P(C, C)` 行，把上面的命令输出擦掉。只把光标挪高 G 行，让 zsh 自己的相对移动和它自己的 `\e[J` 完成清理，zsh 的屏幕模型也始终和屏幕一致。

终端是否重排：`TILDY_REFLOW=1`/`0` 指定；未设置时按 `TERM_PROGRAM`（ghostty、iTerm.app、Apple_Terminal、WezTerm、vscode、tmux、WarpTerminal、rio、Tabby、Hyper）、`TERM`（xterm-ghostty、xterm-kitty、alacritty、wezterm）以及 `KITTY_WINDOW_ID`、`ALACRITTY_WINDOW_ID`、`KONSOLE_VERSION`、`VTE_VERSION`、`WT_SESSION` 判断，其余按不重排处理（xterm、urxvt、Linux 控制台、pyte）。猜错的代价不对称：把不重排的终端当成重排，缩窄到某行宽度以下时会多上移 G 行、擦掉提示符上方的 G 行输出；反过来只是留下碎片（修订前的行为）。截断形式与防抖不依赖这个判断。

### OSC 133 语义提示符标记

cmux 加载 Ghostty 的 zsh 集成（`CMUX_LOAD_GHOSTTY_ZSH_INTEGRATION=1`）。它在 precmd 里（排在最后时）给 PS1 包上 `%{ESC]133;A;cl=line BEL%}`、每个换行后插 `%{ESC]133;P;k=s BEL%}`、末尾 `%{ESC]133;B BEL%}`，注释写明"PS2 mark is needed when clearing the prompt on resize"。Ghostty 的 `Screen.resize`（`clearPromptForRedraw`）在重排之后、光标不在命令输出里时，从标记的提示符起点清到屏尾（只清单元格、不删行、不动光标），`shell_redraws_prompt` 默认开启（kitty 扩展 `redraw=0` 可关）。tildy 的结果从 `zle -F` 回调写入 `PROMPT`，把这些标记丢了；集成脚本自己也写着"Themes that update prompt asynchronously from a `zle -F` handler might still remove our marks"。现在 `_tildy_set_prompt … keep`（`_tildy_marks`）在异步结果、截断形式和 transient 写入时沿用当前 `PROMPT` 里的起点、续行、终点标记（任何 `%{ESC]133;…%}` 组，Ghostty、kitty、WezTerm 的写法都匹配）；precmd 里的基础提示符不加，由集成自己的 precmd 加。有了标记，Ghostty 会先把重排后的旧行清空，上面的 G 行变成空行而不是碎片；因为行没有被删，G 的算术不变，第 3 至 5 步照样把这几行收回来。

### 证据

- `python3 tests/pty/run.py`：62/0（新增 `resize_narrow_fits`、`resize_burst_debounced`、`resize_reflow_no_fragments`、`resize_reflow_without_fixup_leaves_fragments`、`semantic_marks_kept`、`resize_narrow_real`，见 `tests/pty/README.md`）；`--real`：27/0。重排用 harness 里的 `ReflowMixin` 模拟（pyte 本身缩窄时只截断）。对照场景证明同一个重排屏幕在 `TILDY_REFLOW=0` 时出现碎片。
- 同一模型下逐步拖动（96 → 36 列，每步 6 列、每步之间让 zsh 的输出先到）：修正后无碎片；不修正时提示符上方堆了 11 行碎片，与截图一致。
- `resize_idle`/`resize_idle_worker` 现在在缩放后 90–110 ms 画出新结果（防抖 80 ms 加后端），修订前是 4–25 ms。

### 仍然受终端限制

- **竞争**：Shell 与终端是异步的。zsh 读到尺寸、算好重画之后，终端如果在处理这些字节之前又缩放了一次（拖得很快），这份重画就会按另一个宽度解释，zsh 和 tildy 的相对算术都无从得知。同一模型里让 11 次缩放全部先于任何输出生效，最后仍残留 4 行碎片。Ghostty 的清除发生在终端内部、与重排原子地进行，带着 OSC 133 标记时能清掉标记范围内的残留，但清不到比最新起点标记更高的行。
- **Shell 已经不再掌握的行**：更早的提示符、transient 行、命令输出都会被终端重排，这是正常的；某次竞争留下的碎片、滚出屏幕的行（`\e[A` 到顶就停）无法再修。
- **变宽**：终端把折行合并回去后，zsh 的上移会多走、擦掉上面的行。tildy 的行在请求宽度内不折行，截断形式也不折行，所以只剩很长的、本身已折行的输入缓冲区会触发（zsh 的行为）。
- **不安全状态**：补全菜单、isearch、`vared`、PS2 中缩放只靠 zsh 自己的重画（安全重绘规则优先）。
- **猜错重排**：见上。
- 只在 zsh 5.9 上实测；zsh 5.8 的 `reset-prompt` 是否同样按"上次画出的高度"上移未验证。

### 与 Starship、Powerlevel10k 的比较

- **Powerlevel10k** 没有 Shell 侧的修正。README 的"Horrific mess when resizing terminal window"把原因归于终端重排（"It isn't specific to Powerlevel10k"），给出的办法是：kitty ≥ 0.24 加 `POWERLEVEL9K_TERM_SHELL_INTEGRATION=true`（由终端清除提示符）、作者自己的 zsh 补丁（用 `sc`/`rc` 保存恢复光标，只在终端连同保存的光标一起重排时有效，未进上游）、关掉终端的重排，或者避免长行（gap 字符改成空格、不要 ruler、去掉右侧边框与右提示符）。
- **Starship** 也没有：它的 `PROMPT='$(starship prompt …)'` 在 SIGWINCH 时由 zsh 重新执行，宽度相关的部分（fill）按新宽度算，但行不会收缩，zsh 的上移也一样，所以重排终端里同样留碎片。不过它的 PS1 是固定字符串，集成脚本加的 OSC 133 标记一直在，Ghostty 能自己清掉旧行。作者在 cmux 里用 Starship 时没遇到这个问题，很可能就是这个原因（推断，未在 cmux 中验证）。
- **tildy（本修订）**：渲染器保证每行不超过请求宽度；缩窄时 Shell 立刻换成按新宽度截断的形式，并按重排后的行数把 zsh 的重画起点挪回旧提示符第一行；保留终端的 OSC 133 标记，让 Ghostty 的清除也生效；请求防抖，一轮缩放一个代次。

### M4 多终端修订（2026-10-23）

真实验收确认：Ghostty/cmux 的集成已负责提示符重定位时，不能再执行上面的 Shell 光标上移补偿，否则会擦掉命令输出。保留宽度拟合与异步刷新。基础提示符也需保留早于 Tildy 安装的语义包装；摘要钩子不能争抢最后 precmd 槽位。Terminal.app 原生跳转要求保留已接受的完整提示符。当前规则见 [ADR 0013](0013-terminal-prompt-ownership.md) 与 [支持范围](../support.md)。
