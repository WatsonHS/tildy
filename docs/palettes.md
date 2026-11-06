# 调色板与主题

日期：2026-10-20（M2 追加任务 E2）。目标：不手调样式就能换主题。三样东西配合使用：

- **调色板**（`palette` / `palettes`，Starship 语义）：给颜色起名字，样式里写名字；
- **内置调色板**：14 个常见配色，按统一的**角色色**命名；
- **按角色着色的布局预设**：`powerline`、`lean`、`classic`、`rainbow`、`minimal`，样式里只写角色名，所以换一个调色板就整体换色。

```json
{ "schema_version": 1, "preset": "lean", "palette": "nord" }
```

```sh
tildy palette list                          # 内置调色板与色条
tildy palette show nord                     # 角色色与自带颜色的色块、示例段
tildy preset show lean --palette nord       # 用示例数据画出来
tildy preset show --all --palette dracula   # 所有按角色着色的预设都换成 dracula
tildy theme                             # 选中按角色着色的预设后 ←/→ 换调色板
```

## 调色板怎么生效

与 Starship 相同：顶层 `palette` 选中一个调色板，`palettes` 定义调色板（`{名字: {颜色名: 颜色}}`）；任何样式颜色都可以写当前调色板里的颜色名：

- 样式对象的 `foreground` / `background`（模块的 `style`、`success_style`、`error_style`、`state_style`、`rules[].style` 等所有 `*style`）；
- `format`、`repo_root_format` 以及符号类字段（`symbol`、`text`、`status_symbols.*`、行的 `separator`/`subseparator`/`start`/`end` 等）里 `[文字](样式)` 片段的颜色，例如 `[$time](bold fg:accent bg:surface)`。

规则：

1. **调色板优先于 16 色名**：调色板定义了 `red`，`"foreground": "red"` 就用调色板的 `red`。所以给固定配色的预设设 `palette` 也会替换它用到的同名颜色（例如 `plain` 的 `blue`、`green`），十六进制颜色不受影响。
2. **值必须是具体颜色**：颜色名、`bright-<名字>`、`purple`/`bright-purple`、0–255、`#rrggbb`；不能引用别的调色板颜色（与 Starship 一致）。
3. **名字**：调色板名 `[A-Za-z0-9_-]{1,64}`；颜色名以字母或 `_` 开头、由 `[A-Za-z0-9_-]` 组成、最长 64，不能是 `bold`、`dimmed`、`dim`、`italic`、`underline`、`none`。片段里的颜色名不区分大小写（先找完全相同的，再找只差大小写的）。
4. **内置调色板相当于预先写在 `palettes` 里**：`{"palette": "nord"}` 不用定义就能用；用户写 `palettes.nord` 时按键合并到内置的 nord 上（只改某个角色、或加自己的颜色）。
5. **校验带路径**：未知调色板报 `palette`；未知颜色名报所在字段，例如 `modules.cwd.style.foreground: unknown color 'bg9': not a color name and not defined in palette 'nord'`；`format` 片段里的未知颜色报 `modules.x.format`；调色板里的坏值报 `palettes.mine.accent`。符号类字段里的片段只替换认得的名字、不报错（它们本来就可能是普通文字）。
6. **解析时机**：配置加载时就把调色板颜色换成具体值。`Config::file`（渲染器、provider 看到的类型化配置）只有具体颜色，渲染器不需要知道调色板；`Config::json`（`tildy config get --effective` 一类的原始合并结果）保留原来写的名字。`palette` 未设置时什么都不替换，所以已有配置、包括 `pastel-powerline` 的输出逐字节不变。

## 角色色

每个内置调色板都定义这 14 个角色色；按角色着色的预设只用它们，所以自定义调色板只要定义齐这 14 个就能配这 5 个预设。

| 角色 | 含义 | 谁在用 |
| --- | --- | --- |
| `bg1`…`bg5` | 段背景（强调色），按段的顺序 | powerline（用户/任务、目录、Git、语言、docker） |
| `bg6` | 收尾段背景，中性色（深色主题里是浅灰一类） | powerline 的耗时与时间；rainbow 的系统图标与时间 |
| `fg` | 段上的文字：要能压在 `bg1`…`bg6` 和作背景用的 `accent`/`ok`/`warn`/`err`/`info` 上 | powerline、rainbow |
| `fg_dim` | 次要文字（分隔线、时间、加载中的 Git） | lean 的时间、classic 的细分隔、minimal 的分支名 |
| `surface` | 中性段背景，比终端背景亮（或暗）一档 | classic 的所有段；rainbow 的 ✔、任务、user@host 段 |
| `accent` | 强调（目录） | lean、classic、minimal 的目录；rainbow 的目录段背景 |
| `ok` | 成功（提示符、干净的 Git、领先落后） | 所有预设 |
| `warn` | 警告（改动、耗时） | 所有预设；rainbow 的脏 Git 与耗时段背景 |
| `err` | 错误（失败、冲突、Git 进行中的操作） | 所有预设；rainbow 的失败状态段背景 |
| `info` | 信息（未跟踪文件、任务、虚拟环境、时间） | 所有预设 |

角色色都写在终端背景（或 `surface`）上时要看得清，`fg` 要和段背景有对比；内置调色板选色时按 WCAG 对比度检查过（大多数组合 ≥ 4.5，Solarized、Nord、Catppuccin Latte 这类本身低对比的配色有几处在 3 左右）。

每个内置调色板还额外定义 `background` 与 `foreground`（这个配色的终端背景与正文色，`tildy palette show` 用它们画示例），以及配色自己的颜色名。

## 内置调色板

| 调色板 | 深浅 | 来源 | 作为默认的预设 |
| --- | --- | --- | --- |
| `dracula` | 深 | <https://draculatheme.com/spec> | |
| `nord` | 深 | <https://www.nordtheme.com/docs/colors-and-palettes> | minimal |
| `solarized-dark` | 深 | <https://ethanschoonover.com/solarized> | |
| `solarized-light` | 浅 | 同上 | |
| `one-dark` | 深 | <https://github.com/joshdick/onedark.vim> | classic |
| `rose-pine` | 深 | <https://rosepinetheme.com/palette>（main） | |
| `everforest` | 深 | <https://github.com/sainnhe/everforest>（dark medium） | |
| `kanagawa` | 深 | <https://github.com/rebelot/kanagawa.nvim>（wave） | |
| `gruvbox-dark` | 深 | <https://github.com/morhetz/gruvbox> | rainbow |
| `tokyo-night` | 深 | <https://github.com/folke/tokyonight.nvim>（night） | lean |
| `catppuccin-latte` | 浅 | <https://github.com/catppuccin/palette> | |
| `catppuccin-frappe` | 深 | 同上 | |
| `catppuccin-macchiato` | 深 | 同上 | |
| `catppuccin-mocha` | 深 | 同上 | powerline |

角色色的取值：

| 调色板 | `bg1` | `bg2` | `bg3` | `bg4` | `bg5` | `bg6` | `fg` | `fg_dim` | `surface` | `accent` | `ok` | `warn` | `err` | `info` |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `dracula` | #bd93f9 | #ff79c6 | #ffb86c | #8be9fd | #50fa7b | #f8f8f2 | #282a36 | #6272a4 | #44475a | #bd93f9 | #50fa7b | #ffb86c | #ff5555 | #8be9fd |
| `nord` | #b48ead | #bf616a | #d08770 | #88c0d0 | #8fbcbb | #d8dee9 | #2e3440 | #616e88 | #3b4252 | #88c0d0 | #a3be8c | #ebcb8b | #bf616a | #81a1c1 |
| `solarized-dark` | #6c71c4 | #d33682 | #cb4b16 | #268bd2 | #2aa198 | #93a1a1 | #002b36 | #586e75 | #073642 | #268bd2 | #859900 | #b58900 | #dc322f | #2aa198 |
| `solarized-light` | #6c71c4 | #d33682 | #cb4b16 | #268bd2 | #2aa198 | #586e75 | #fdf6e3 | #93a1a1 | #eee8d5 | #268bd2 | #859900 | #b58900 | #dc322f | #2aa198 |
| `one-dark` | #c678dd | #e06c75 | #d19a66 | #61afef | #56b6c2 | #abb2bf | #282c34 | #5c6370 | #3e4452 | #61afef | #98c379 | #e5c07b | #e06c75 | #56b6c2 |
| `rose-pine` | #c4a7e7 | #eb6f92 | #f6c177 | #9ccfd8 | #ebbcba | #908caa | #191724 | #6e6a86 | #26233a | #c4a7e7 | #31748f | #f6c177 | #eb6f92 | #9ccfd8 |
| `everforest` | #d699b6 | #e67e80 | #e69875 | #7fbbb3 | #83c092 | #9da9a0 | #2d353b | #859289 | #3d484d | #7fbbb3 | #a7c080 | #dbbc7f | #e67e80 | #83c092 |
| `kanagawa` | #957fb8 | #e46876 | #ffa066 | #7e9cd8 | #7aa89f | #c8c093 | #1f1f28 | #727169 | #363646 | #7e9cd8 | #98bb6c | #e6c384 | #e46876 | #7fb4ca |
| `gruvbox-dark` | #d3869b | #fb4934 | #fe8019 | #83a598 | #8ec07c | #a89984 | #282828 | #928374 | #3c3836 | #83a598 | #b8bb26 | #fabd2f | #fb4934 | #8ec07c |
| `tokyo-night` | #bb9af7 | #f7768e | #ff9e64 | #7aa2f7 | #73daca | #a9b1d6 | #1a1b26 | #565f89 | #3b4261 | #7aa2f7 | #9ece6a | #e0af68 | #f7768e | #7dcfff |
| `catppuccin-latte` | #8839ef | #d20f39 | #fe640b | #1e66f5 | #179299 | #7c7f93 | #eff1f5 | #8c8fa1 | #ccd0da | #8839ef | #40a02b | #fe640b | #d20f39 | #1e66f5 |
| `catppuccin-frappe` | #ca9ee6 | #e78284 | #ef9f76 | #85c1dc | #81c8be | #a5adce | #232634 | #838ba7 | #51576d | #ca9ee6 | #a6d189 | #e5c890 | #e78284 | #85c1dc |
| `catppuccin-macchiato` | #c6a0f6 | #ed8796 | #f5a97f | #7dc4e4 | #8bd5ca | #a5adcb | #181926 | #8087a2 | #494d64 | #c6a0f6 | #a6da95 | #eed49f | #ed8796 | #7dc4e4 |
| `catppuccin-mocha` | #cba6f7 | #f38ba8 | #fab387 | #74c7ec | #94e2d5 | #a6adc8 | #11111b | #7f849c | #45475a | #cba6f7 | #a6e3a1 | #f9e2af | #f38ba8 | #74c7ec |

配色自带的颜色名（都可以直接写在样式里）：

- `dracula`：`current_line`、`selection`、`comment`、`cyan`、`green`、`orange`、`pink`、`purple`、`red`、`yellow`
- `nord`：`nord0`…`nord15`，另有 `nord3_bright`（#616E88，Nord 官方编辑器配色里的注释色，用作 `fg_dim`；`nord3` 在 nord0 上几乎看不见）
- `solarized-dark`、`solarized-light`：`base03`、`base02`、`base01`、`base00`、`base0`、`base1`、`base2`、`base3`、`yellow`、`orange`、`red`、`magenta`、`violet`、`blue`、`cyan`、`green`
- `one-dark`：`black`、`white`、`red`、`dark_red`、`green`、`yellow`、`dark_yellow`、`blue`、`purple`、`cyan`、`comment_grey`、`gutter_fg_grey`、`cursor_grey`、`visual_grey`、`menu_grey`、`special_grey`、`vertsplit`
- `rose-pine`：`base`、`surface_rp`、`overlay`、`muted`、`subtle`、`text`、`love`、`gold`、`rose`、`pine`、`foam`、`iris`、`highlight_low`、`highlight_med`、`highlight_high`
- `everforest`：`bg_dim`、`bg0`、`bg_visual`、`bg_red`、`bg_yellow`、`bg_green`、`bg_blue`、`bg_purple`、`red`、`orange`、`yellow`、`green`、`aqua`、`blue`、`purple`、`grey0`、`grey1`、`grey2`、`statusline1`…`statusline3`
- `kanagawa`：wave 主题的全部颜色，`sumiInk0`…`sumiInk6`、`waveBlue1`/`2`、`winterGreen`/`Yellow`/`Red`/`Blue`、`autumnGreen`/`Red`/`Yellow`、`samuraiRed`、`roninYellow`、`waveAqua1`/`2`、`dragonBlue`、`oldWhite`、`fujiWhite`、`fujiGray`、`oniViolet`/`2`、`crystalBlue`、`springViolet1`/`2`、`springBlue`、`lightBlue`、`springGreen`、`boatYellow1`/`2`、`carpYellow`、`sakuraPink`、`waveRed`、`peachRed`、`surimiOrange`、`katanaGray`
- `gruvbox-dark`：gruvbox.vim 的命名 `dark0_hard`…`dark4`、`gray`、`light0_hard`…`light4`、`bright_*`、`neutral_*`、`faded_*`（red、green、yellow、blue、purple、aqua、orange），另有简写 `red`…`orange`（= 深色模式用的 bright 版）
- `tokyo-night`：`bg`、`bg_dark`、`bg_dark1`、`bg_highlight`、`terminal_black`、`fg_dark`、`fg_gutter`、`dark3`、`comment`、`dark5`、`blue0`…`blue7`、`cyan`、`magenta`、`magenta2`、`purple`、`orange`、`yellow`、`green`、`green1`、`green2`、`teal`、`red`、`red1`
- `catppuccin-*`：`rosewater`、`flamingo`、`pink`、`mauve`、`red`、`maroon`、`peach`、`yellow`、`green`、`teal`、`sky`、`sapphire`、`blue`、`lavender`、`text`、`subtext1`、`subtext0`、`overlay2`、`overlay1`、`overlay0`、`surface2`、`surface1`、`surface0`、`base`、`mantle`、`crust`

名字冲突的处理：角色名优先。Tokyo Night 与 Everforest 自己的 `fg` 改叫 `foreground`；Everforest 自己的 `bg1`…`bg5` 不再单独提供（`surface` 就是它的 bg2）；Rosé Pine 自己的 `surface` 改叫 `surface_rp`。

## 按角色着色的预设

五个预设都是“Nerd 字体可选”（符号层 auto）：nerd 层画 powerline 箭头与图标，unicode 与 ascii 层的 powerline 段直接相接（不画箭头）、图标换成文字或省略。最后一行的右侧（RPROMPT）都以内置的 `agent`（Agent 状态）模块开头。

### `powerline`（默认 `catppuccin-mocha`）

pastel-powerline 的结构：第一行 powerline 段 `用户 ✦任务 │ 目录 │ Git │ c go java node python rust │ docker │ 耗时 时间`，第二行 `❯`。段背景依次是 `bg1`（用户、任务）、`bg2`（目录）、`bg3`（Git）、`bg4`（语言）、`bg5`（docker）、`bg6`（耗时、时间），段上文字 `fg`，提示符成功 `ok`、失败 `err`。

### `lean`（默认 `tokyo-night`）

仿 Powerlevel10k 的 lean 风格（`config/p10k-lean.zsh`，两行）：

```
~/code/tildy main ⇡2 !2 ?1                                              ✘ 1 3s ✦ demo 14:05:09
❯
```

- 左：目录（`accent` 粗体，不按仓库截断，超过 5 级才截）、Git（分支 `ok`；`⇣⇡` 与 `*` 储藏 `ok`，`~` 冲突与进行中的操作 `err`，`+` 暂存与 `!` 修改 `warn`，`?` 未跟踪 `info`，顺序与 p10k 的 `my_git_formatter` 相同；nerd 层分支前有 Git 图标；数据未就绪时整段 `fg_dim`）。
- 第一行右侧（右对齐）：失败状态 `✘ 码`（`err`）、耗时（3 秒以上，`3s` 格式，`warn`）、后台任务图标（`ok`）、Python 虚拟环境名（`info`）、SSH 时的 `user@host`（`warn`）、时间 `%T`（`fg_dim`）。
- 第二行：`❯`（成功 `ok`、失败 `err`）。
- 与 p10k 的差异：p10k lean 默认只在信号与管道失败时显示状态（普通失败靠提示符变红），这里任何失败都显示 `✘ 码`；p10k 在提示符前加空行（`add_newline` 由任务 E1 实现，这里不设）；目录不做 p10k 的“唯一前缀”缩写；语言版本、kubectl、云账号等 p10k 右侧的大量段省略。

### `classic`（默认 `one-dark`）

仿 Powerlevel10k classic（`config/p10k-classic.zsh`，两行，无边框）：两侧的段都在同一个中性底色 `surface` 上，段之间画 `fg_dim` 的细分隔 ``（`subseparator`），左侧末尾一个 `` 收尾、右侧开头一个 `` 起头。

- 左：系统图标（nerd 层，`info`）、目录（目录图标 + 路径，`accent` 粗体）、Git（与 lean 同色）。
- 第一行右侧：成功 `✔`（`ok`）或失败 `✘ 码`（`err`）、耗时（`warn`，nerd 层后跟沙漏图标）、任务（`info`）、虚拟环境（`info`）、SSH 时的 `user@host`（`warn`）、时间（`info`，nerd 层后跟时钟图标）。p10k 右侧图标在内容之后，这里照搬。
- 第二行 `❯`。
- 与 p10k 的差异：不画 `╭─`/`╰─`/`─╮`/`─╯` 边框（p10k 向导里的 “Prompt Frame: No frame” 选项；边框会让 `tildy theme` 的单行开关产生奇怪的结果）；第二行保留 `❯`（p10k classic 默认去掉 prompt_char，靠右侧状态段）；时间用 `info` 而不是 p10k 的灰绿色（`fg_dim` 在 `surface` 上对比太低）。

### `rainbow`（默认 `gruvbox-dark`）

仿 Powerlevel10k rainbow（`config/p10k-rainbow.zsh`，两行，无边框）：每段一种底色，文字用 `fg`。

- 左：系统图标（`bg6`，对应 p10k 的白底）、目录（`accent` 底，粗体）、Git（`ok` 底；有改动或冲突时 `warn` 底，冲突计数 `err`；数据未就绪时 `fg_dim` 底）。
- 第一行右侧：成功 `✔`（`surface` 底、`ok` 字）或失败 `✘ 码`（`err` 底）、耗时（`warn` 底）、任务（`surface` 底、`info` 字）、虚拟环境（`info` 底）、SSH 时的 `user@host`（`surface` 底、`warn` 字）、时间（`bg6` 底）。
- 第二行 `❯`。差异同 classic（无边框、保留 `❯`）。

### `minimal`（默认 `nord`）

单行 `~/…/tildy  main !? ❯`：目录（`accent` 粗体，按仓库截断到 3 级）、Git 分支（`fg_dim`，nerd 层前有 ``）与 Starship 风格的状态标记（`!` 修改 `warn`、`+` 暂存 `ok`、`?` 未跟踪与 `⇡⇣⇕` `info`、`=` 冲突 `err`，不带计数）、`❯`。

## `pastel-powerline` 和 `powerline` 的区别

| | `pastel-powerline` | `powerline` |
| --- | --- | --- |
| 用途 | 作者日用外观，复刻作者的 starship.toml（保真） | 同样结构的主题模板，换调色板用 |
| 颜色 | 固定的十六进制（#9A348E、#DA627D、#FCA17D、#86BBD8、#06969A、#33658A） | 角色色 `bg1`…`bg6`，默认 `catppuccin-mocha`；`palette` 一行换色 |
| 段上文字 | 终端默认前景色（不写 `foreground`） | `fg`（深色主题里是最深的底色，压在亮色段上） |
| 时间段底色 | 深蓝 #33658A | `bg6`，中性浅色 |
| 语言模块 | 原生的 c/go/java/node/python/rust，加 elixir、elm、haskell、julia、nim 的 Starship 桥接 | 只有原生模块（不依赖 Starship） |
| 符号层 | 固定 nerd（需要 Nerd 字体） | auto（Nerd 字体可选） |
| 提示符颜色 | `green` / `red` | `ok` / `err` |
| Git 进行中操作 | 默认黄色粗体 | `fg` 粗体 |
| `palette` | 不设；设了也只换它用到的 `green`/`red` 这类颜色名 | 默认 `catppuccin-mocha` |

`pastel-powerline` 的渲染结果在本任务前后逐字节相同（`fixtures/render/pastel_*` 未变）。

## 自定义调色板

给按角色着色的预设用，定义齐 14 个角色色即可（`tildy palette show 名字` 会标出缺哪个角色）：

```json
{
  "schema_version": 1,
  "preset": "powerline",
  "palette": "mine",
  "palettes": {
    "mine": {
      "bg1": "#9A348E", "bg2": "#DA627D", "bg3": "#FCA17D", "bg4": "#86BBD8", "bg5": "#06969A", "bg6": "#33658A",
      "fg": "#FFFFFF", "fg_dim": "244", "surface": "236",
      "accent": "blue", "ok": "green", "warn": "yellow", "err": "red", "info": "cyan",
      "brand": "#FF8800"
    }
  },
  "modules": { "time": { "style": { "background": "brand" } } }
}
```

只想改内置调色板的一两处：

```json
{ "schema_version": 1, "preset": "classic", "palette": "nord", "palettes": { "nord": { "surface": "#434C5E", "accent": "#8FBCBB" } } }
```

（值不能引用别的调色板颜色：写成 `"accent": "nord7"` 会被拒绝，报 `palettes.nord.accent`。）

不用预设、在自己的配置里用调色板也一样：`{"palette": "dracula", "modules": {"cwd": {"style": {"foreground": "purple"}}}}`。

从 Starship 迁移：`tildy import starship` 把 starship.toml 的 `[palettes]` 与 `palette` 原样保留为 tildy 的 `palettes`/`palette`，样式里引用调色板的颜色保持名字（tildy 不接受的名字，例如以数字开头的，在引用处换成十六进制并列在 `unsupported` 里）。

## 渲染上的新能力（为 p10k 布局）

Powerlevel10k 的两行布局把右侧段放在第一行，Zsh 的 RPROMPT 只能画在最后一行，所以渲染器补了两项（不影响已有预设，渲染 golden 不变）：

1. **非最后一行的 `right`**：以前报 `unsupported_right_row` 并隐藏，现在右对齐画在该行上，结束在离右边缘一列处（与 Zsh 默认 `ZLE_RPROMPT_INDENT=1` 的 RPROMPT 对齐）；该行的 `fill` 模块会填满左右之间的空隙。左右之间放不下一列空隙时右侧整体不画，模块报告 `hidden_reason: "no_room"`（p10k 的做法）。powerline 行的右侧是镜像的：段从右往左连，起头用镜像后的 `end`（默认镜像的分隔符，`` → ``），段之间用镜像的 `separator`，同底色之间用镜像的 `subseparator`（`` → ``），`start` 的镜像作为右侧收尾（例如 `░▒▓` → `▓▒░`）。最后一行的 `right`（RPROMPT）仍然总是普通样式。
2. **行的 `subseparator`**：powerline 行里相邻两段底色相同时画的细分隔（默认不画，与以前一样直接相接）；默认用下一段的前景色，可以写片段指定颜色，如 `"[](fg:fg_dim)"`。

`tildy theme` 的“行数”开关把各行右侧合并到单行的 RPROMPT（最后一行的在前），所以 classic/rainbow 切成单行后右侧段是普通样式、没有 powerline 箭头。

## 命令与接口

- `tildy palette list [--json]`：名字、深浅、色条（`bg1`…`bg6`）、说明；`--json` 给 `name`、`description`、`variant`、`source`。
- `tildy palette show NAME [--json]`：角色色与配色自带颜色的色块和值、示例段（`fg` 压在 `bg1`…`bg6` 上）、终端背景上的文字色、`surface` 上的文字色。名字不是内置的就到用户配置的 `palettes` 里找；内置名字会先合并用户的同名 `palettes`。`--json` 给 `colors[{name, value, role}]` 与 `missing_roles`。未知名字退出码 2。
- `tildy preset show NAME --palette P`（也可与 `--all`、`--here`、`--symbols` 组合）：按角色着色的预设换调色板预览；对固定配色的预设指定 `--palette` 退出码 2，`--all` 时只给按角色着色的预设换色。`tildy preset list --json` 每项多一个 `palette`（默认调色板，固定配色为 null）。
- `tildy theme`：选中按角色着色的预设且焦点在列表时，←/→（h/l）在 14 个内置调色板间循环，预览标题显示 `调色板 NAME`；写入时只有与预设默认不同才写 `palette`；切换预设时选择保留；再次运行时从已有文件的 `palette` 起步。非交互：`tildy theme --preset NAME --palette P`。
- `tildy capabilities --json`：`palettes`（同 `palette list --json`）、`palette_roles`（14 个角色）、`presets[].palette`。
- Schema（`schema/config.v1.schema.json`）：顶层 `palette`、`palettes`，`$defs/color` 接受调色板颜色名，`$defs/palette`，行的 `subseparator`。

## 实现与测试

- 代码：`src/config/palette.rs`（内置调色板注册表、`effective` 合并、`resolve` 在 `config::build` 里替换颜色并报带路径的错误、单元测试）、`src/config/palettes/*.json`（14 个，`include_str!`）、`src/config/presets/{powerline,lean,classic,rainbow,minimal}.json`、`src/config/template.rs` 的 `span_spec_ranges`（定位片段样式，供改写）、`src/render/mod.rs` 的 `compose_upper_right` / `compose_powerline_right` / `mirror` / `push_subseparator`、`src/cli/palette.rs`、`src/cli/preset.rs`（`--palette`）、`src/cli/configure.rs`（调色板循环）、`src/cli/capabilities.rs`、`src/import/mod.rs` 与 `format.rs` 的调色板保留。
- 预设层：预设布局里引用、但预设自己没定义的模块（如内置的 `agent`）从内置层继承（`config::apply_preset`）。
- 测试：`config::palette::tests`（每个内置调色板定义全部角色且是 #rrggbb、解析与合并、错误路径、每个按角色着色的预设只用角色色且与 14 个调色板都能加载、fixtures 加载）；`render::tests::golden` 新增 10 个 `preset_<预设>_<调色板>` golden（每个预设两个调色板，覆盖 nerd/unicode/ascii、失败状态、SSH、窄终端放不下右侧）；`render::tests::upper_right_powerline_is_mirrored` 与 `multi_row_and_full_history`（右对齐与 `no_room`）；`cli::configure` 的调色板循环与按键；`tests/palette_cli.rs`；`tests/preset_import.rs` 的 catppuccin 用例改为检查保留的调色板；`fixtures/validate.py` 校验内置调色板文件与新增的 `config.v1.{valid,invalid}-palette*` 样本。
