# 契约 fixture 说明

本目录是 `schema/` 下各 Schema 的样本，由 `python3 fixtures/validate.py` 校验。

## 命名约定

- `<schema>.valid[-后缀].json`：必须通过对应 Schema。
- `<schema>.invalid[-后缀].json`：必须失败，失败原因见下表。
- `<schema>` 与 `schema/<schema>.schema.json` 一一对应；`-后缀` 用于同一 Schema 的多个样本（例如 shell-frame 的请求帧与结果帧）。

## 样本与失败原因

| 样本 | 期望 | 说明 |
| --- | --- | --- |
| `config.v0.valid.json` | 通过 | 逐字取自 `PLAN.md`「标准 JSON」示例（首个 JSON 代码块）。 |
| `config.v0.invalid.json` | 失败 | 模块 `cwd` 中的字段 `prority` 是 `priority` 的拼写错误，违反 `additionalProperties: false`。 |
| `config.v0.invalid-ref.json` | 失败 | Schema 本身有效，但 `layout` 引用了未定义的模块 `node_version`。这是跨引用检查（`validate.py` 的 `extra_checks`）负责的错误。 |
| `config.v1.*.json` | 见文件名 | `valid-user` 为覆盖文件，其 layout 引用内置模块，跨引用检查同时查看内置配置。 |
| `config/invalid-dangling-ref.json` | 失败 | 子目录 `config/` 下的样本映射为 config.v1；`layout` 引用了未定义的模块 `node_version`（跨引用检查）。 |
| `protocol/` | 见子目录 README | 协议 v1 帧与快照样本，线路样本 `wire-samples.txt`。 |
| `shell-frame.valid.json` | 通过 | 请求帧：字段 gen、session_id、cwd、cols、lines、last_exit、last_duration_ms 完整。 |
| `shell-frame.valid-result.json` | 通过 | 结果帧：status 为 partial，left 为两行，right 为可选单行。 |
| `shell-frame.invalid.json` | 失败 | 请求帧缺少必需字段 `last_duration_ms`，两种帧都不匹配。 |
| `shell-frame.invalid-result.json` | 失败 | 结果帧的 status 为 `done`，不在枚举 `fresh`、`partial` 中。 |
| `context.valid.json` | 通过 | 包含 worker_epoch、generation、cwd、git_dir、worktree_id（可选）、修订与终端尺寸。 |
| `context.invalid.json` | 失败 | 缺少必需字段 `worker_epoch`（旧 worker 回包的纪元校验依赖它）。 |
| `capabilities.valid.json` | 通过 | `tildy capabilities --json` 的真实输出（CliEnvelope：api_version、ok、data、error），含 19 个模块类型与 16 个内置预设（`presets`），config 为内置配置。 |
| `capabilities.invalid.json` | 失败 | 真实输出的 module_types 中多出未知类型 `not_a_module_type`，不在枚举中。 |
| `error.valid.json` | 通过 | 失败结果：revision_conflict，data 为 null，error 含 path、message、hint。 |
| `error.valid-ok.json` | 通过 | 成功结果：ok 为 true，error.code 为 `ok`，其余错误字段为 null。 |
| `error.invalid.json` | 失败 | `ok` 为 false 但 `error.code` 为 `ok`，且 `data` 非 null；违反「失败时 code 不得为 ok、data 为 null」。 |
| `agent-state.valid.json` | 通过 | worktree 作用域、status 为 waiting_input、seq 为 3、ttl_s 为 600。 |
| `agent-state.invalid.json` | 失败 | status 为 `done`，不在枚举 `running`、`waiting_input`、`completed`、`failed` 中。 |

## 发现规则

- 顶层与子目录中的 `<schema>.valid[-后缀].json` / `<schema>.invalid[-后缀].json` 均被校验（`protocol/` 等子目录也是）。
- 子目录中不符合命名的 `.json`（如 `git/`、`render/` 的数据）被忽略；顶层不符合命名的报错。
- `config/` 目录映射为 config.v1，按文件名前缀 valid 或 invalid 决定期望结果。
- 每个 schema 至少一个有效与一个无效样本；shell-frame.v1 的四种帧各至少一个有效与一个无效 JSON 样本，且线路样本覆盖四种帧。

## 校验依赖

`validate.py` 使用已安装的 `jsonschema`（本机 4.23.0，Draft 2020-12）。脚本不自动安装依赖。

## M1b 新增

- `fixtures/render/`：预设 `pastel-powerline` 与 `plain` 的 golden 用例（powerline、Nerd 字形、format 模板、`not_detected`）。
- `fixtures/import/`：作者 Starship 配置的导入 golden。
- `fixtures/config.v1.valid-pastel-user.json`、`config.v1.invalid-preset.json`。
- `src/config/builtin.json` 与 `src/config/presets/*.json` 不在本目录，但由 `validate.py` 按 config.v1 校验（预设补上 `schema_version: 1` 后校验；`plain.json` 只含 `$comment`，是有意的空预设）。
