# ADR 0009：项目更名 skiff → tildy

日期：2026-10-22。状态：已采纳（作者决定）。

## 背景

项目自 M0 起以 `skiff` 为代号，早期名称检查当时就查明 `skiff` 在 crates.io、npm、PyPI 均被占用，GitHub 上有多个活跃同名项目（含 Notion 收购的 Skiff 邮箱），正式命名推迟到 M6。M3 收尾后准备建公开的 Homebrew tap 与二进制发布仓库，名字一旦出现在公开仓库和 formula 里就有外部引用，而它还贴在命令名、环境变量前缀、配置与状态目录、zsh 函数前缀、OSC 标记与分隔线文本上，此时改名只是一次机械替换，M4 之后再改就要带迁移逻辑。

## 决定

更名为 **tildy**（来自 `~`，提示符里最常见的符号）。2026-10-22 复查：crates.io、Homebrew formula 与 cask、PyPI、npm 均空闲，GitHub 名称搜索只有两个十几星的无关小项目。另查过 36 个常见英文单词，crates.io 上无一空闲；两词组合如 `promptwise`、`zprompt` 虽空闲，但 "prompt" 一词如今默认指 LLM 提示词，或与 zsh 社区现有项目撞名，均不取。

## 影响

- 命令名 `tildy`，Cargo 包名 `tildy`，二进制 `target/release/tildy`。
- 环境变量前缀 `TILDY_*`（原 `SKIFF_*`），zsh 函数前缀 `_tildy_*`。
- 目录：`~/.config/tildy`、`$XDG_STATE_HOME/tildy`、`$XDG_CACHE_HOME/tildy`。作者机器上的原目录已复制到新位置，旧目录保留不删。
- 协议与快照里的字面量（`tildy.session_snapshot` 等）同步更名；协议版本仍为 1，因为更名前没有外部用户。
- 历史记录中可能以 Tildy 指代更名前的 skiff。名称检查与原始验收记录已私下归档；本记录保留命名决定。
