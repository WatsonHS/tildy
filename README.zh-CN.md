# Tildy

Tildy 是 Rust 实现的异步 Zsh 提示符。Git、语言版本等慢查询在后台更新，日常输入保持响应；人、脚本和 Agent 共用配置与诊断接口。

[English](README.md) · [配置](docs/configuration.md) · [开发与测试](CONTRIBUTING.md)

## 安装

```sh
brew tap WatsonHS/tildy
brew trust WatsonHS/tildy
brew install tildy
tildy init
```

新开终端生效。`init` 会备份并修改 `.zshrc`，支持 `ZDOTDIR`，重复执行不会重复添加；用 `tildy init --dry-run` 预览。启用前停用其他提示符的初始化语句。

默认粉彩双行主题需要 Nerd Font。可用 `tildy theme` 交互选择，或执行 `tildy theme --preset plain`。无需创建配置文件。

支持 macOS / Linux + Zsh。Linux 预编译包需要 glibc ≥ 2.39。不同架构的验证程度见[支持矩阵](docs/support.md)。升级执行 `brew update && brew upgrade tildy`，随后新开终端或执行 `exec zsh`。

## 日常操作

```sh
tildy theme                              # 实时预览主题和布局
tildy config get --effective --json      # 当前配置及来源
tildy explain --here --json              # 解释模块显示与隐藏
tildy timings --last 5                   # 当前会话实际查询耗时
tildy inspect --list --json              # 活跃会话
tildy capabilities --json                # Agent 的能力发现入口
```

配置支持校验、场景预览、修订检查、历史与回滚。最近 Git 根的 `.tildy.json` 只能覆盖显示，不能增加命令或激活原来隐藏的模块。[配置指南](docs/configuration.md) · [导入与分层](docs/import-layering.md)

初始化前设置 `export TILDY_LANG=zh` 使用中文，`auto` 跟随系统语言。Agent 状态默认关闭，提示符渲染不依赖模型或联网。

## 开发与恢复

```sh
cargo build --locked --release
scripts/dev-shell.sh --release
```

开发 shell 使用隔离目录。测试见 [CONTRIBUTING.md](CONTRIBUTING.md)，移除集成和恢复配置见[卸载说明](docs/uninstall.md)。

原创代码采用 [MIT](LICENSE) 许可；第三方预设、调色板和依赖的署名见 [NOTICE](NOTICE)。
