# tildy theme

自 `0.3.2` 起主命令改为 `tildy theme`；`tildy configure` 保留为兼容别名，参数与行为相同。

日期：2026-10-19。决定见 `docs/modules.md` 修订记录：最小的交互式预设选择器，不是编辑器，不引入第二套配置语义。

## 做什么

`tildy theme` 在终端里列出全部内置预设（M2 E2 起 21 个），上下键切换时用**当前目录的真实数据**（Git、语言版本，和 `tildy preset show --here` 同一套取数）实时画出提示符，再加六个开关做最常见的调整。按 Enter 写入用户配置，内容包含 `preset`、与预设不同的字段，以及明确设置过的空行偏好。更细的修改交给编辑 JSON 或 Agent（见“再往下改”）。

```sh
tildy theme                       # 交互，写入默认配置路径
tildy theme --output ./try.json   # 写到别处（不影响正在用的配置）
```

需要标准输入和标准输出都是终端，否则立即退出（退出码 4）并提示用下面的非交互写法。

## 界面

从上到下：标题行；预览（预设在当前目录下的样子，右侧提示符右对齐，下面一行是执行后收起成的样子）；预设列表（名字、Nerd 字体需求、一句话说明，选中行反色）；开关区（宽度不足时换行）；将写入的内容摘要与路径；按键提示。

整屏使用备用屏幕，退出（包括 Ctrl-C、panic、SIGINT/SIGTERM/SIGHUP）时恢复终端：回到正常模式、显示光标、离开备用屏幕。超出宽度的预览行在右边缘截断，不折行。

预览数据：每个预设第一次选中时在后台取当前目录的数据，最多等 300 ms；没取到就先画示例数据（`~/code/tildy`、main 分支有改动、node 22.3.0 等），预览标题注明“示例数据（当前目录的数据 300 ms 内未就绪，就绪后自动刷新）”，数据到了自动重画。预览里的“上条命令 3.2 秒”和“1 个后台任务”是固定的演示值，方便看到耗时与任务模块。

## 按键

| 按键 | 作用 |
| --- | --- |
| ↑ ↓ 或 j k | 选预设（首尾循环） |
| ← → 或 h l | 焦点在列表、且选中的是按角色着色的预设（powerline、lean、classic、rainbow、minimal）时：切换调色板（14 个内置调色板循环，预览标题显示当前调色板）；其他情况：选开关（焦点移到开关行） |
| 空格 | 切换当前开关 |
| Tab | 焦点在列表和开关之间切换 |
| Enter | 写入并退出 |
| q 或 Esc | 不写入，退出（退出码 0） |
| Ctrl-C | 不写入，退出（退出码 130） |
| ? | 底部显示完整按键说明 |

## 六个开关

布局开关按所选预设自己的布局计算：按模块**类型**分类，不认模块 ID。预设里没有对应模块时显示 `n/a`，按空格只给出提示。切换预设时开关的选择保留（例如选了 ascii 图标或关掉时间，换到别的预设仍然生效）。

| 开关 | 取值 | 作用 | 何时 n/a |
| --- | --- | --- | --- |
| 图标 | nerd → unicode → ascii | 写顶层 `symbols`。初始值是预设固定的层，或预设为 `auto` 时在当前终端探测到的层；带 `*` 表示与预设不同、会写入 | 从不 |
| 行数 | 两行 / 单行 | 两行变单行：所有行的左侧模块按顺序并到第一行（保留第一行的 powerline 样式与分隔符），`character` 模块放在最后，各行的右侧并成这一行的右侧（RPROMPT，总是普通样式；最后一行的在前，例如 lean 变成 `agent status elapsed …`）。单行变两行：`character` 移到新的第二行，第一行的右侧也移过去（Zsh 只在最后一行画 RPROMPT） | 布局里没有 `character` 模块（`plain`） |
| 提示符前空行 | 开 / 关 | 写顶层 `add_newline`，在命令输出与下一个提示符之间留一空行；读取已有值、实时预览，切换预设仍保留。不会改动 `history.add_newline` | 从不 |
| 时间 | 开 / 关 | 从布局去掉所有 `time` 类型模块 | 没有 `time` 模块 |
| 语言版本 | 开 / 关 | 去掉显示版本号的语言模块：`language`、`python` 的格式含 `$version`（默认格式含）；`command` 的格式含 `$version`，或含 `$text` 且命令是 `--version` 一类；Starship 桥接的语言模块 | 没有这类模块（`pure-preset` 的 python 只显示虚拟环境，`no-runtime-versions` 只显示图标） |
| 耗时与任务 | 开 / 关 | 去掉 `duration` 和 `jobs` 类型模块 | 两者都没有 |

去掉模块后变空的行整行删除。各预设的结果（摘录）：

- `pastel-powerline`：六个开关都可用；语言版本包括原生的 c/go/java/node/py/rust 和桥接的 elixir/elm/haskell/julia/nim，docker 不算。
- `tokyo-night`：耗时与任务 n/a；php（命令模块，格式含 `$version`）算语言版本。
- `pure-preset`：时间、语言版本 n/a；单行为 `user host cwd git duration py char`。
- `plain`：行数、时间 n/a；耗时与任务去掉左侧的 jobs 和右侧的 elapsed。
- 单行预设（catppuccin 四个、jetpack）可以切成两行。

## 写入什么

```json
{
  "schema_version": 1,
  "preset": "tokyo-night",
  "symbols": "unicode",
  "layout": {
    "rows": [
      {
        "left": ["text1", "cwd", "git", "node", "rust", "go", "php", "char"],
        "separator": {
          "ascii": ">",
          "nerd": "…"
        },
        "start": {
          "ascii": "",
          "nerd": "░▒▓",
          "unicode": "░▒▓"
        },
        "style": "powerline"
      }
    ]
  }
}
```

（`tildy theme --preset tokyo-night --one-line --no-time --symbols unicode` 的输出；`"nerd": "…"` 处实际是 Nerd 私有区字形，这里写不出来。）

- 始终有 `schema_version` 与 `preset`；`palette` 只在按角色着色的预设选了与其默认不同的调色板时写（切换预设时调色板选择保留，对固定配色的预设不生效）；`symbols` 只在图标层与预设不同时写；`layout` 只在任一布局开关改变了行时写，且是完整的 `rows`（合并规则里数组整体替换，见 `docs/presets.md`）。`add_newline` 在已有明确设置或操作空行开关后写入，true/false 都保留；全新配置什么都不改时文件就是 `{"schema_version": 1, "preset": "NAME"}`。
- 键顺序固定（schema_version、preset、palette、symbols、add_newline、layout），模块 ID 数组写在一行，方便之后手改。
- 写入流程（2026-10-20 改）：走 `tildy config apply` 同一条配置事务（`config::transaction::Store::apply`）：校验 → 历史目录加锁 → 比对修订（命令启动时读到的修订；期间文件被 Agent 或另一个 shell 改过则拒绝写入，退出码 3，提示重跑）→ 原文件存入历史 → 临时文件 + 原子 rename。不再在配置旁边留 `.bak`。校验失败或任何一步出错都不会留下半份配置或临时文件。目标是符号链接（dotfiles 仓库）时写到链接指向的文件，链接本身保留。
- 已有文件会被替换（历史里有原文），但本命令不管理的顶层字段（`modules`、`palettes`、`history` 等）原样保留，写在它自己的字段之后，并与生成部分一起校验；界面底部会列出保留的字段。（2026-10-20 改：之前整体替换，作者用 `tildy config apply` 加的 `add_newline` 被一次 `tildy theme` 丢掉了。）唯一不保留的是手改过、本命令无法复现的 `layout`，底部会提示。再次运行时从已有文件起步：选中它的预设、沿用它的 `palette`（内置调色板）、`symbols` 与 `add_newline`；如果它的 `layout` 正是本命令对该预设能生成的某种组合，开关恢复成那种组合，否则提示 `layout` 不保留。
- 输出：标准输出是写入的 JSON；标准错误是写到哪里（含新修订）、历史条目、回滚命令、怎么生效。`--output` 与默认路径不同时提示用 `TILDY_CONFIG` 指向它；没检测到已启用 tildy（环境里没有 `TILDY_BIN`，且 `$ZDOTDIR/.zshrc` 或 `~/.zshrc` 里没有 `tildy init zsh`）时给出要加到 `.zshrc` 的一行。

生效：在已加载 tildy 的 Shell 里按一次 Enter，下一个提示符读取新配置；或重启 Shell。

## 非交互

脚本与测试用同一套逻辑，不需要终端：

```sh
tildy theme --preset NAME [--palette P] [--symbols nerd|unicode|ascii] [--one-line|--two-line] \
                [--add-newline|--no-add-newline] [--no-time] [--no-langs] [--no-duration] [--output PATH]
```

- `--add-newline` 开启提示符前空行，`--no-add-newline` 关闭，两者互斥；未指定时保留已有选择，新配置沿用预设默认值。只指定空行参数进入交互时，仍会恢复已有布局开关。
- 未知预设、未知图标层、未知调色板，或给固定配色的预设指定 `--palette`：退出码 2。
- 对没有 `character` 模块的预设要求改行数（如 `--preset plain --two-line`）：退出码 2。
- 要求关掉预设本来就没有的模块（如 `--preset pure-preset --no-time`）：标准错误给一条说明，照常写入，退出码 0。
- 交互模式下也可以带这些开关参数，作为开关的初始值（不带 `--preset` 即进入交互）。

## 回滚

写入后会打印：

```
已写入 /Users/you/.config/tildy/config.json（修订 3fa2de494cdb266c）
原文件已存入历史（1792470000000-d81cdec02ae9a8d6，`tildy config history` 可查）
回滚：tildy config rollback --to d81cdec02ae9a8d6 --expect-revision 3fa2de494cdb266c
```

执行这条 `rollback` 即恢复（它本身也进历史，可以再撤销）；历史保留 50 条，与 `tildy config apply` 共用。原来没有文件时回滚命令是 `--to builtin`，即删掉新文件回到内置默认外观。

## 再往下改

- **手改 JSON**：在写出的文件里加不带 `type` 的模块即可局部覆盖预设的同名模块，例如 `"modules": {"time": {"time_format": "%T"}}`。模块 ID 用 `tildy preset show NAME` 配合 `src/config/presets/NAME.json` 查。合并规则见 `docs/presets.md`“怎么用”。注意之后再运行 `tildy theme` 会替换整个文件（有备份）。
- **交给 Agent**：`tildy capabilities --json` 列出模块与预设，`schema/config.v1.schema.json` 是配置 Schema；Agent 直接生成完整候选 JSON，校验后写入（M2 的 `config validate/apply` 落地后走带修订号的流程）。
- **只想看不想写**：`tildy preset show --all [--here] [--symbols T]`。

## 实现与测试

- 代码：`src/cli/configure.rs`（开关计算、写入、界面与按键循环）、`src/cli/tui.rs`（libc termios 原始模式守卫、按键解码、终端尺寸，不依赖 TUI 库）、`src/cli/preview.rs`（与 `tildy preset show` 共用的示例数据、`--here` 取数、Zsh 转义转 ANSI）。
- 交互循环 `run_loop` 的按键来自任意 `Read`、画面写到任意 `Write`；真实终端用带超时的 poll 读取，超时作为 tick，用于终端尺寸变化与延迟到达的预览数据重画。
- 单元测试（`cargo test --bin tildy configure`）：pastel-powerline、tokyo-night、pure-preset、plain 的开关计算与 n/a 判定，单行预设拆两行，覆盖项最小化，所有预设 × 所有开关组合都能通过校验并渲染，从已有布局恢复开关，300 ms 上限后退回示例数据，注入按键流（Down、空格、Enter）驱动整个循环，写入的备份/原子替换/符号链接。
- 集成测试：`tests/configure_cli.rs`（非交互写入后用 `config::load` 读回、备份与回滚提示、无终端退出码 4、错误退出码）；`tests/configure_pty.rs`（用 libc `openpty` 在伪终端里运行二进制，发送 Down、空格、Enter，检查写出的文件与终端恢复序列；q 与 Ctrl-C 不写文件）。

## 已知限制

- 是否已启用 tildy 的判断是启发式的：Zsh 脚本用 `typeset -g` 设置 `TILDY_BIN`，没有导出到环境，所以主要靠检查 `.zshrc` 里有没有 `tildy init zsh`；从别的文件 source 的配置会被误判为未启用（只是多打印一行提示）。
- `plain` 没有 `character` 模块，行数开关为 n/a。
- 写入的 `layout` 是完整的行列表，之后预设本身更新布局时，这份配置不会跟着变；重新运行 `tildy theme` 即可。
- 预览的 `auto` 图标层按运行 `tildy theme` 的终端探测；在别的终端（例如 SSH）里同一份配置可能显示另一层。
