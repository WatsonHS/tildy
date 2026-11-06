# 内置预设

日期：2026-10-19（2026-10-20 M2 E2 补充按角色着色的预设）。目标：不用手调样式，一行配置换外观。Tildy 内置 21 个预设：自己的 `plain`、作者日用的 `pastel-powerline`，Starship 1.24.2 自带的全部 12 个预设（`starship preset --list`，其中 `catppuccin-powerline` 另外拆出 latte、frappe、macchiato 三个配色），以及 5 个按调色板角色着色、可以用 `palette` 一键换色的布局预设（`powerline`、`lean`、`classic`、`rainbow`、`minimal`，见 [docs/palettes.md](palettes.md)）。

## 列表

| 预设 | Nerd 字体 | 符号层 | 说明 |
| --- | --- | --- | --- |
| `plain` | 可选 | auto | Tildy 默认的朴素外观：单行、无背景色，Nerd 字体层显示图标（不写 `preset` 时就是它） |
| `pastel-powerline` | 需要 | nerd | 粉彩 powerline 两行布局，含用户、目录、Git、语言、时间，第二行 ❯（作者 starship.toml 的变体，见下文差异） |
| `tokyo-night` | 需要 | nerd | Tokyo Night 深蓝渐变 powerline：░▒▓ 起头、苹果图标、圆角分隔，第二行 ❯ |
| `gruvbox-rainbow` | 需要 | nerd | Gruvbox 暖色彩虹 powerline：系统图标、用户、目录、Git、语言、conda、时间，第二行 Nerd 箭头 |
| `catppuccin-powerline` | 需要 | nerd | Catppuccin Mocha（深色）彩色 powerline 单行，含命令耗时与 ❯ |
| `catppuccin-latte` | 需要 | nerd | 同上，Latte 配色（浅色终端背景用） |
| `catppuccin-frappe` | 需要 | nerd | 同上，Frappé 配色 |
| `catppuccin-macchiato` | 需要 | nerd | 同上，Macchiato 配色 |
| `jetpack` | 不需要 | auto | 左侧只有 ◎，目录、Git、语言、时间放在右侧（RPROMPT），几何符号 |
| `pure-preset` | 不需要 | auto | 仿 Pure：目录、灰色分支与 `*` 脏标记、命令耗时，第二行紫色 ❯ |
| `bracketed-segments` | 可选 | auto | Starship 默认外观，各模块用方括号包起来紧挨排列 |
| `nerd-font-symbols` | 需要 | nerd | Starship 默认外观，语言与工具全部换成 Nerd 图标 |
| `no-empty-icons` | 可选 | auto | Starship 默认外观；工具未安装时不显示空图标 |
| `no-nerd-font` | 不需要 | unicode | Starship 默认外观，不用任何 Nerd 字形 |
| `no-runtime-versions` | 可选 | auto | Starship 默认外观，只显示语言图标、不显示版本号 |
| `plain-text-symbols` | 不需要 | ascii | Starship 默认外观，全部符号换成纯文字 |
| `powerline` | 可选 | auto | pastel-powerline 的结构（用户、目录、Git、语言、时间，第二行 ❯），颜色全部取自调色板角色；默认调色板 `catppuccin-mocha` |
| `lean` | 可选 | auto | 仿 Powerlevel10k lean：两行、无背景；目录与 Git 在左，状态、耗时、任务、虚拟环境、时间右对齐在第一行；默认 `tokyo-night` |
| `classic` | 可选 | auto | 仿 Powerlevel10k classic：两侧都是中性底色（角色 `surface`）的 powerline 段，细分隔线；默认 `one-dark` |
| `rainbow` | 可选 | auto | 仿 Powerlevel10k rainbow：每段一种底色，Git 按状态变色，失败时状态段变红；默认 `gruvbox-dark` |
| `minimal` | 可选 | auto | 单行：目录、Git 分支与状态标记、❯；默认 `nord` |

后 5 个是**按角色着色的预设**：它们的样式里只写角色色名（`bg1`…`bg6`、`fg`、`fg_dim`、`surface`、`accent`、`ok`、`warn`、`err`、`info`），由调色板给出具体颜色，所以 `{"preset": "lean", "palette": "nord"}` 一行就能换配色；前 16 个的颜色是固定的（保真于原始外观），`palette` 不会给它们换色（只会替换它们用到的同名颜色，例如调色板若定义了 `red`）。

“符号层”是预设固定的 `symbols`：`nerd` 表示在任何终端都按 Nerd 字体画图标；`auto` 表示按终端探测（Ghostty、WezTerm 或 `TILDY_SYMBOLS=nerd` 时用 Nerd 图标，其他 UTF-8 终端用 Unicode 替代，非 UTF-8 用纯 ASCII）。`tildy preset list`（`--json` 给出结构化结果）和 `tildy capabilities --json` 的 `presets` 字段列出同样的信息。

## 怎么挑

```sh
tildy preset show --all            # 每个预设按名字依次画出来，真实颜色，一屏翻完
tildy preset show tokyo-night      # 只看一个
tildy preset show --all --here     # 用当前目录的真实 Git 与语言数据（会执行版本命令）
tildy preset show --all --symbols ascii   # 看某个符号层下的样子（nerd、unicode、ascii）
tildy preset show lean --palette nord      # 按角色着色的预设换一个调色板看
tildy palette list                         # 14 个内置调色板（tildy palette show NAME 看色块）
```

默认示例数据：`~/code/tildy`、`main` 分支有修改和未跟踪文件、node 22.3.0、python 3.12.1（虚拟环境 `demo`）、上一条命令 3.2 秒、1 个后台任务、14:05。每个预设下面还会印出执行后收起成的那一行（transient）。右侧提示符按终端宽度右对齐画在最后一行。

预览里的斜体与暗色（dim）在真实提示符里不显示，原因见“共同差异”。

## 怎么用

在 `~/.config/tildy/config.json`（或 `$TILDY_CONFIG`）写一行：

```json
{ "schema_version": 1, "preset": "tokyo-night" }
```

保存后下一个提示符就生效。预设是完整外观：它的 `modules` 与 `layout` 整体替换内置默认，用户文件再在其上合并。改局部时写不带 `type` 的模块会合并进预设的同名模块：

```json
{
  "schema_version": 1,
  "preset": "gruvbox-rainbow",
  "modules": { "time": { "time_format": "%T" }, "cwd": { "truncation_length": 5 } }
}
```

按角色着色的预设再加一行 `"palette"` 换色，也可以只改调色板里的某个角色（用户的 `palettes.<同名>` 按键合并到内置调色板上）：

```json
{ "schema_version": 1, "preset": "classic", "palette": "nord", "palettes": { "nord": { "surface": "#434C5E" } } }
```

固定了 `nerd` 符号层的预设可以用 `"symbols": "auto"` 改回自动探测。模块 ID 可以用 `tildy preset show NAME` 配合 `src/config/presets/NAME.json` 查到。

## 字体与终端要求

- **需要 Nerd 字体**：`pastel-powerline`、`tokyo-night`、`gruvbox-rainbow`、四个 `catppuccin-*`、`nerd-font-symbols`。装任意 Nerd Font（作者用 Meslo）并在终端里选中它。这些预设固定 nerd 符号层，没装字体会看到方块。
- **可选**：`plain`、`bracketed-segments`、`no-empty-icons`、`no-runtime-versions`，以及按角色着色的 5 个（nerd 层画 powerline 箭头与图标，unicode/ascii 层的 powerline 段直接相接、不画箭头，图标换成文字或省略）。有 Nerd 字体（或 `TILDY_SYMBOLS=nerd`）时显示 Node、Git 分支等图标，否则用 Unicode 或文字替代；Starship 默认用的 emoji（🐍 🦀 🐹）在 unicode 层保留，ascii 层换成文字。
- **不需要**：`jetpack`、`pure-preset`、`no-nerd-font`（Unicode 符号）、`plain-text-symbols`（纯 ASCII，适合任何终端）。`jetpack` 的几何符号（◎ □ △ ⎪）和 `pure-preset` 的 `❯`、`≡` 需要 UTF-8 终端。

## 窄终端：按优先级收缩

（2026-10-20，作者日用中反馈窄 pane 折行，M3 的"按优先级收缩"提前实现。）渲染器把每一行收缩到终端宽度：非最后一行在左侧宽于 `cols` 时收缩；最后一行在左侧加上（若有）右侧（RPROMPT）与 Zsh 在 RPROMPT 前留的一列空隙宽于 `cols - 1`（给光标留一列）时收缩，这正是 Zsh 显示 RPROMPT 的条件（左 + 右 ≤ `cols - 2`）。收缩只隐藏模块，不做新的截断（目录照旧按自己的 `truncation_length` 截断）：按 `priority` 从低到高逐个隐藏，同优先级先隐藏最右边的（最后一行的右侧算在左侧的右边），每隐藏一个就重新拼一次行，所以 powerline 的分隔符与收尾箭头总是跟着剩下的段走，不会留下悬空的箭头。被隐藏的模块在报告里是 `hidden_reason: "no_room"`。`character` 模块和 `fill` 从不隐藏；每行至少保留优先级最高的那个模块，它自己仍然放不下时这一行照旧折行（例如 ascii 层下 40 列里的超长目录）。transient 不受影响。非最后一行的右侧仍按原规则整体显示或整体隐藏（p10k 的做法），在左侧收缩之后判断。

所有内置预设都按模块类型写了 `priority`（`plain` 即内置默认 `builtin.json`，同一张表）：

| priority | 模块类型 | 说明 |
| --- | --- | --- |
| 10 | `time`、`os`、`text` | 最先隐藏：时钟、系统图标与装饰性文字 |
| 20 | `language`、`command`、`starship`、`package` | 语言与工具版本 |
| 30 | `python`、`docker_context`、`env_var`、`nix_shell`、`direnv`、`shell`、`shlvl`、`memory_usage`、`localip`、`sudo` | 环境上下文（虚拟环境、docker 上下文、conda） |
| 40 | `duration` | 上一条命令耗时 |
| 50 | `jobs` | 后台任务数 |
| 60 | `hostname` | SSH 主机名 |
| 70 | `git_commit`、`git_metrics` | Git 细节 |
| 80 | `git` | 分支与状态 |
| 90 | `username`、`agent_status` | |
| 100 | `directory`、`exit_status`、`character` | 最后隐藏（`character` 从不隐藏） |

以作者的 `pastel-powerline`（`user, host, jobs, cwd, git, commit, 语言…, py, docker, elapsed, time`；第二行 `status, char`）为例，宽度不断变窄时依次消失的是：时间 → 语言版本 → docker → python → 耗时 → 后台任务 → 主机名（只在 SSH 下出现）→ 提交哈希（只在 detached HEAD 下出现）→ Git → 用户，目录留到最后；同优先级的语言版本从右往左消失。`fixtures/render/narrow_pastel_powerline_{60,40}` 与 `narrow_lean_40` 固定了这几种结果，`tildy preview --scenario narrow --preset NAME` 能直接看到。

用户文件里没写 `priority` 的模块按 50 处理（例如 `tildy import starship` 生成的配置，Starship 没有这个概念），这时同优先级从右往左隐藏；想调整顺序就给模块写 `priority`，例如 `{"modules": {"time": {"priority": 95}}}` 让时间留到最后。

## 与 Starship 的对比

在临时 Git 仓库（`main` 分支，有 `package.json`、一处修改、一个未跟踪文件）里，用 `STARSHIP_CONFIG=<预设 TOML> starship prompt --cmd-duration 3200 --jobs 1` 和 `TILDY_CONFIG={"preset":NAME} tildy render`（同样的 REQ）逐字比较可见文本，并按字符比较前景色、背景色与粗体。另在仓库深层子目录（无 package.json）、非仓库的 Rust 项目、激活虚拟环境的 Python 项目、`~/Documents`（目录替换）和上条命令失败时重复。

| 预设 | 可见文本 | 颜色 | 差异 |
| --- | --- | --- | --- |
| tokyo-night | 一致 | 一致 | 仅 Starship 被隐藏模块留下的空分隔段（见下） |
| gruvbox-rainbow | 一致 | 一致 | 同上 |
| catppuccin-powerline 及三个配色 | 一致 | 一致 | 同上 |
| jetpack | 提示符行与右侧一致 | 一致 | 缺少第一行（`$git_metrics` 增删行数，右对齐）与右侧 `pkg ◨ 1.0.0` |
| pure-preset | 一致 | 一致 | 无 |
| bracketed-segments、nerd-font-symbols、no-empty-icons、no-runtime-versions、plain-text-symbols | 除 package 段外一致 | 一致 | 缺少 `is 📦 v1.0.0`（package 模块） |
| no-nerd-font | 除 package 段与分支符号外一致 | 一致 | 分支符号为 `⎇`（Starship 保留 Powerline 字形 U+E0A0） |

### 共同差异

1. **powerline 分隔符随隐藏模块调整**：Starship 的分隔符是写死在 `format` 里的字面量，某组模块全部不显示时会留下一段只有颜色的空隙（例如 gruvbox 在没有 docker/conda 时出现两个连续箭头）。Tildy 只在相邻的可见段之间画分隔符，所以没有空隙，箭头颜色取实际相邻的两段。这是有意的差异。
2. **提示符前的空行**（Starship 的 `add_newline`）不复现。
3. **斜体与暗色**：Zsh 的提示符转义只有粗体、下划线、反色和颜色。斜体、暗色只能用原始 SGR 序列，而协议层和 Zsh 适配层出于安全会删掉提示符里的控制字符（否则会显示成 `[3m` 字样）。因此真实提示符里不画斜体与暗色，`tildy preset show` 的预览里画。受影响：jetpack（大量斜体）、Starship 默认系列里的 Java（`red dimmed`）与主机名（`dimmed`）。需要协议允许 `%{…%}` 内的 SGR 才能补上。
4. **package 模块**（📦 包版本）没有原生实现，内置预设也不走 Starship 桥接，所以不显示。
5. **只在 Starship 里存在的模块**省略：内置预设不使用 Starship 桥接（桥接模块的外观由用户自己的 starship.toml 决定，而不是预设）。没有原生类型的语言（php、ruby、kotlin、haskell、lua、zig、swift、deno、bun、elixir、scala、dart、cpp）用 `command` 模块实现：检测规则与 Starship 相同，版本命令只在检测命中时执行并缓存。
6. **os 段**只区分 macOS、Linux、Windows 等操作系统，不识别 Linux 发行版（Ubuntu、Arch 等的专用图标不用）。
7. **conda** 读 Shell 转发的 `CONDA_DEFAULT_ENV`；`ignore_base`（默认隐藏 base）照搬。

### 各预设的其他差异

- `pastel-powerline`：保留作者的变体（两行、第二行 ❯、用户段是图标、额外的 jobs/命令耗时/python/Git 进行中状态，见 `docs/modules.md` 修订记录）。Starship 1.24.2 自带的同名预设是单行、没有 ❯、用户段显示用户名、以圆角端帽（U+E0B6）起头、多出 gradle 与 scala 段。需要官方原样时可以 `tildy import starship --file fixtures/import/presets/pastel-powerline.toml` 生成（实测与 Starship 一致，只是 tildy 会在没有 character 模块的最后一行末尾补一个 ❯，且不留空分隔段）。
- `tokyo-night`：PHP 用 `php -nr …` 命令模块。
- `gruvbox-rainbow`：pixi 省略；cpp 与 Starship 默认一样不开。
- `catppuccin-*`：命令耗时的桌面通知不支持；耗时显示为 Starship 的 `3s200ms` 格式。
- `jetpack`：条件第一行（`$nix_shell$container$fill$git_metrics`）整行省略，这几个模块都没有对应实现，保留也永远不会出现；左侧省略 localip、shlvl、shell、env_var、sudo；右侧省略 package、kubernetes、云服务、电池等；Git 分支名截断（11 字符）与只读标记不支持。`ignore_branches`（main、master 不显示分支名）与仓库根目录格式（`□ tildy △`）已支持。
- `pure-preset`：Git 进行中状态（`REBASING 1/2`）画在状态之后而不是分支与状态之间；vi 模式符号不支持。
- Starship 默认外观系列（bracketed-segments、nerd-font-symbols、no-empty-icons、no-nerd-font、no-runtime-versions、plain-text-symbols）：Starship 的 `$all` 由 tildy 支持的模块按同样顺序近似；`no-empty-icons` 与默认外观看起来一样，因为 tildy 本来就不显示没有版本号的语言段。

## 为移植新增的能力

渲染与配置（原有的 render golden 全部不变）：

- `format` 模板支持 `[文字](样式)` 片段，样式用 Starship 写法（`bold fg:#769ff0 bg:#394260`、`bright-black`、`purple`、`218`），可嵌套，内层替换外层，未写的属性取模块自身的 `style`；括号组 `(...)` 可以嵌套。片段外的文字用模块样式。符号（`symbol`、Git 状态符号）也可以写片段，例如 `"[⬢](bold green) "`。
- 行为变化：嵌套括号组 `(a(b))` 以前是语法错误，现在合法；真实提示符不再输出斜体与暗色的原始 SGR（见“共同差异”第 3 条），`render_preview` 仍然输出。
- 新模块类型：`text`（字面量，`text` 字段，可分层）、`os`（按操作系统画固定符号，`os_symbols` 按 `macos`/`linux`/… 覆盖）、`fill`（用 `symbol` 把所在行补满到终端宽度，简化实现：只在左侧有效，右侧不展开）、`env_var`（显示 Shell 转发的环境变量，`variable` 字段）。
- 通用字段 `padding`：powerline 段两侧是否补空格（`text` 默认不补）。
- powerline 行的 `start`/`end` 端帽：在第一段之前、最后一段之后按相邻段背景色画（例如圆角 U+E0B6、`░▒▓`）；没有背景色的段画在端帽之外，像普通文字。
- 普通行的 `separator`：模块之间的连接文字（默认一个空格）；模块自己以空格结尾时不再额外加空格，所以 Starship 风格（每个模块自带尾随空格）的格式不会出现双空格。
- Git：状态符号里的 `$count`（`"⇡${count}"`）、各状态单独的变量（`$conflicted`、`$modified`、…、`$all_status`、`$ahead_behind`）、`ignore_branches`、`only_attached`；目录：`repo_root_format`（变量 `$before_root_path`、`$repo_root`、`$path`）；耗时：`$human`（`3s`）与 `$human_ms`（`3s200ms`）；后台任务：`number_threshold`；提示符：`error_symbol`；条件里可用 `env.<变量名>` 字段。

导入器（`tildy import starship` 同样受益）：

- 每个模块按 Starship 自身默认值补全后再转换，所以没写某个模块时得到的是 Starship 默认外观（`on`/`via`/`took` 字样、颜色、符号）。
- `palette`/`palettes` 原样保留为 tildy 的 `palette`/`palettes`，引用调色板的颜色保持名字（M2 E2 起；之前是换成十六进制）；库接口 `ImportOptions::palette` 指定候选的 `palette`（catppuccin 三个配色由此生成）。tildy 不接受的颜色名（如以数字开头）在引用处换成十六进制并在 `unsupported` 里说明。
- 识别 powerline 端帽、字面量段（变成 `text` 模块）、`$os`、`$fill`、`$conda`、`right_format`（最后一行的右侧）、`[line_break] disabled`、错误时不同的提示符号、Starship 的嵌套样式规则（最内层整段包住的样式生效）、只写在格式里的 Nerd 图标（移到可分层的 `symbol`）。
- `ImportOptions::commands`：没有原生类型的语言改为 `command` 模块，不再依赖 Starship 桥接。

## 维护：从 Starship 重新生成

1. `starship preset NAME > fixtures/import/presets/NAME.toml`（Starship 升级后全部重导）。
2. `TILDY_BLESS=1 cargo test --test preset_import` 重写导入器的候选配置 `fixtures/import/presets/NAME.expected.json`（commands 模式，catppuccin 三个配色换 palette），检查 diff。
3. 由候选生成 `src/config/presets/NAME.json`：去掉 `history`，加 `$comment`（来源与差异）与固定的 `symbols`；固定配色的预设不能带 `palette`（带 `palette` 的预设会被当作按角色着色），所以候选里按名字引用的调色板颜色要换成十六进制（`config::from_str(候选)` 之后的 `Config::file` 里就是解析后的值），并删掉 `palette`/`palettes`；把 `…/`、`⇡⇣⇕»✘≡`、`⌂` 这类纯字符串改成带 ascii 替代的分层写法；jetpack 删掉只剩 `fill` 的第一行。
4. `TILDY_BLESS=1 cargo test --lib render::tests::golden` 重写 `fixtures/render/preset_*.expected.json`，检查 diff。
5. 新预设在 `src/config/mod.rs` 的 `PRESET_INFO` 与 `PRESETS` 登记（中文一句话描述、Nerd 字体需求、来源）。`cargo test` 会检查每个预设可加载、`nerd_font = required` 与固定 nerd 层一致、所有档位渲染不出错。按角色着色的预设还要登记到 `ROLE_PRESETS`，测试会检查它只用角色色名、和每个内置调色板组合都能加载。

按角色着色的 5 个预设由脚本生成后手工核对，改动直接编辑 JSON 即可；改完 `TILDY_BLESS=1 cargo test --lib render::tests::golden` 重写 `fixtures/render/preset_{powerline,lean,classic,rainbow,minimal}_*.expected.json`（每个预设两个调色板）。

## 交互式选择

`tildy theme` 提供预设列表实时预览与六个开关（包括提示符前空行），回车写入配置并备份旧文件；选中按角色着色的预设时，焦点在列表上按 ←/→ 切换调色板（与预设默认不同时才写 `palette`）。说明见 [configure.md](theme.md)。
