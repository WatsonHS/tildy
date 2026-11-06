# 0012：仓库配置只覆盖显示，写事务仍只管理用户文件

- 日期：2026-10-22
- 状态：已接受（M4 D，已随 0.4.0 发布）
- 更新：PLAN 中原设想的公共配置／机器覆盖，本轮以以下明确边界取代。

## 背景

用户要求继续 M4 的 Starship 导入与配置分层。当前内置默认、预设和用户文件已有完整合并与事务，缺少项目外观覆盖和来源诊断。仓库文件可能来自第三方，不能借配置改变命令、检测路径或激活原本未使用的 provider。

## 决定

增加 `builtin → preset → user → project` 中最后一层。沿请求的实际 cwd 向上找到最近的 `.git` 目录或文件，仅读取该根目录的 `.tildy.json`；支持 linked worktree 和子模块边界，不叠加父仓库文件，不读取非 Git 目录散落的同名文件。不增加系统 `/etc/tildy/config.json` 或第二份机器配置；机器选择继续使用 `TILDY_CONFIG`。在 Shell 初始化前设置 `TILDY_PROJECT_CONFIG=0` 可禁用仓库发现；已运行的 worker 继承旧环境，需要重启 Shell。

仓库层必须为 schema v1，顶层只接受 `modules`、`layout`、`symbols`、`palette`、`palettes`、`add_newline` 和 `$comment`。模块只能覆盖已有 ID 的 `style`、`symbol`、`format`、`padding`、`icon_padding`、`priority`、`show_when`、`success_style`、`error_style`、`error_symbol`、`$comment`。对象按键合并，数组整体替换；通过替换布局移除模块。完整合并结果仍经过普通配置校验。

**布局只允许引用下层布局已经放置的 ID**，因此不能通过调整布局启用用户定义但未使用的 command/Starship provider。禁止新增模块，以及 `type`、`argv`、`module`、`starship_config`、检测列表、缓存、超时、agent、history、preset 等执行或行为字段。隐藏已有模块可以减少 provider，不增加执行权限或监听路径。显示字段本身仍使用现有模板校验与 Zsh 转义。

仓库文件上限 64 KiB，只读取普通文件；`O_NOFOLLOW` 拒绝符号链接，`O_NONBLOCK` 加描述符检查避免 FIFO 等特殊文件阻塞，读取长度也受限。任意读取或验证错误使整层失效，完整保留下层结果，报告具体路径和原因；不做部分应用。消失的文件视为正常缺省。

运行时按每次 REQ 的 cwd 发现仓库，比较路径和文件身份（mtime、长度、inode、ctime）后决定是否重载；切换目录、编辑或删除在**下次提示符请求**生效。此轮不增加空闲时配置文件监听。无变化时复用解析结果；上下文不同且配置改变时，现有 epoch 机制重建 provider 并更新 instant 缓存身份。

`config get --effective [--cwd DIR]` 返回 `project` 状态、`effective_revision` 和 `sources` 中的 `project` 来源。来源粒度与原有接口一致：顶层键和模块，表示参与覆盖的最高层，不声称模块内所有字段都来自该层。`valid/problem/fallback` 继续描述用户文件，项目错误位于 `project.valid/problem`。

`revision` 保留用户源文件修订，供 `apply/rollback/theme/import --write` 的并发比较；`effective_revision` 单独组合用户修订和有效仓库内容，不参与用户文件写入。CLI 和向导只写用户文件，不把合并结果回写。项目作者可编辑 `.tildy.json`，使用 `config validate --project --file FILE` 校验；`schema --project` 提供自包含的字段 Schema，跨层 ID 约束由加载器检查。

`preview` 默认配置及 `explain --here` 采用当前目录的仓库层；显式 `preview --config/--preset` 和 `explain --here --config` 保持候选文档的独立预览。`render/worker --config` 选择用户层，仍按 REQ cwd 解析仓库层。底层 `config::load/from_str` 保持纯用户文档语义，事务校验和预设生成不会意外依赖调用目录。

## 证据

2026-10-22：`tests/project_config.rs` 验证路径边界、符号链接 cwd、非法字段、潜伏命令、整层回退、大小限制、FIFO、显式 cwd、禁用、Schema 与写事务修订；真实 worker 在单进程内切换请求目录、编辑颜色并拒绝激活隐藏命令。`src/cli/worker.rs` 单元测试覆盖来源、编辑／删除／非法回退和无变化 epoch 复用。使用方式见 [导入与分层](../import-layering.md)；历史原始验证记录已私下归档。

## 后果

- 项目可分享外观，而命令、路径和 agent 行为继续由用户文件决定。
- 项目配置依赖用户现有模块 ID 和布局；不提供通用跨主题的完整配置包，也不自动启用隐藏模块。
- 配置错误不阻止提示符工作；诊断与用户文件冲突控制分别保留。
- 显示覆盖可能遮掉用户想看的状态，可通过禁用环境变量恢复；不将此层宣称为完整的恶意仓库沙箱。
- 系统层、额外机器层、项目写事务和空闲配置热刷新未实现，后续有实际需要时再决定。
