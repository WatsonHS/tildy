# 场景预览：`tildy preview`

用固定场景数据渲染提示符，并输出每个模块的结构化报告。不访问真实仓库、不执行命令、不读环境变量；时间固定为 14:05。Agent 改完配置后用它确认画面，再决定是否应用。

```
tildy preview --scenario NAME [--config F | --preset P] [--cols N] [--symbols nerd|unicode|ascii] [--json]
tildy preview --all  [同上]
tildy preview --list [--json]
```

- 不给 `--config`/`--preset` 时预览当前生效的配置（`TILDY_CONFIG`，否则 `~/.config/tildy/config.json`，文件不存在则为内置默认）。`--config` 和 `--preset` 互斥。
- `--cols` 缺省取场景自己的宽度（100，`narrow` 为 40）。`--symbols` 缺省取配置解析出的符号层。
- 文本模式：ANSI 画面，再每个模块一行（id、类型、可见/隐藏原因、规则命中、列数，另有 source、位置、新鲜度、年龄、最终样式）。`--json` 标准输出只含 `CliEnvelope`。
- 退出码：0 成功；2 场景名、符号层、预设或配置文件无效（`error.path` 指向出错字段，如 `scenario`、`symbols`、`layout.rows[0].left[0]`）；1 内部错误。

## 场景

名称固定，Agent 可依赖。

| 名称 | 数据 |
| --- | --- |
| `plain-dir` | `~/projects/notes`，不在仓库内 |
| `home` | `~` |
| `git-clean` | `~/code/tildy`，main，干净 |
| `git-dirty` | main，已暂存 1、未暂存 2、未跟踪 1 |
| `git-conflict` | 分支 `feature/preview`，operation `merge`，conflicted 2 |
| `git-ahead-behind` | main，领先 3、落后 1 |
| `command-failed` | 上一条命令退出码 1 |
| `long-command` | 上一条命令 12.3 s |
| `ssh` | SSH 会话，主机 `build-01` |
| `narrow` | 40 列，深层目录，分支 `feature/invoice-export`，脏；各行按 `priority` 收缩到 40 列以内，放不下的模块报 `no_room`（[收缩规则](presets.md#窄终端按优先级收缩)） |
| `timeout` | Git 槽 `timeout`，无值 |
| `stale-cache` | Git 与已检测的语言模块槽 `stale`，age 90 s |
| `jobs` | 两个后台任务 |
| `languages` | node 22.3.0、rust 1.82.0、python 3.12.1（venv `demo`） |
| `agent-blocked` | `agent_status` 槽：state `blocked`、kind `permission`、app `claude-code`、msg `Bash: rm -rf target`、more 1 |
| `agent-error` | `agent_status` 槽：state `error`、app `claude-code`、msg `Build failed: 3 errors` |

其余场景中，没有数据的模块槽是"已检测但无值"（`not_detected`）。`agent_status` 槽的字段：`state`、`kind`、`app`、`title`、`msg`、`id`、`age_s`、`count`、`blocked`、`error`、`done`、`working`、`idle`、`more`（`more` 仅在大于 0 时给出，`progress` 仅在有进度时给出）。场景数据在 `src/cli/scenarios.rs`。

## `--json` 的 `data`

单个场景为一个对象，`--all` 为按场景顺序的数组，`--list` 为 `[{name, description, cols}]`。

| 字段 | 含义 |
| --- | --- |
| `scenario` | 场景名 |
| `cols` | 使用的终端宽度 |
| `symbols` | 使用的符号层 |
| `lines` | 左侧各行，Zsh 提示符转义形式（与 worker 发给 Shell 的相同） |
| `right` | 右侧提示符（Zsh 转义形式），无则 `null` |
| `transient` | 命令执行后收起的形式，历史模式为 `full` 时 `null` |
| `modules` | `ModuleReport` 数组，按渲染顺序 |

`ModuleReport` 字段：

| 字段 | 含义 |
| --- | --- |
| `id`、`module_type` | 模块 id 与类型 |
| `visible` | 是否显示 |
| `hidden_reason` | 隐藏原因：`show_when:<field>`、`empty`、`no_data`、`not_detected`、`pending`、`timeout`、`error`、`not_ssh_or_root`、`unsupported_right_row` 等 |
| `text` | 显示的纯文本（无样式、无转义） |
| `columns` | 占用列数 |
| `rules_hit` | 命中的规则 id |
| `freshness` | 数据新鲜度：`fresh`、`stale`、`pending`、`timeout`、`error`，无数据槽的模块为 `null` |
| `source` | 定义来源：`builtin`、`preset:<name>`、`user`（用户文件里出现该模块 id 即为 `user`） |
| `age_ms` | 数据年龄（毫秒） |
| `style` | 规则叠加后的最终样式，如 `fg=yellow bold`；仅可见模块 |
| `row`、`side` | 布局行（从 0 起）与 `left`/`right` |

后五个字段只由预览填写，缺省时不出现在 JSON 里；提示符路径的输出和 `fixtures/render` 的既有期望文件不变。

## Agent 使用示例

```sh
# 1. 取当前配置与修订
tildy config get --json            # data.revision

# 2. 修改（写到临时文件）
cp ~/.config/tildy/config.json /tmp/new.json   # 然后编辑 /tmp/new.json

# 3. 校验
tildy config validate --file /tmp/new.json --json

# 4. 预览改动在各场景下的效果
tildy preview --scenario git-dirty --config /tmp/new.json --json
tildy preview --scenario narrow    --config /tmp/new.json --cols 40 --json
#    检查 modules[].visible / hidden_reason / style，lines 是否符合预期

# 5. 应用（带预期修订，失败保留原文件）
tildy config apply --file /tmp/new.json --expect-revision REV --json
```

`config` 子命令见 `docs/configuration.md`。
