# 配置事务：`tildy config` 与 `tildy schema`

配置读写共用校验、预览、修订检查和历史记录。设计见 [ADR 0007](decisions/0007-config-transactions.md)，并发锁策略见 [ADR 0011](decisions/0011-config-lock-identity.md)。

这些命令让 Agent 与脚本不经过编辑器、不需要 TTY 就能安全地改配置：先读出内容与修订，改完校验、预览，再带着读到的修订写回；写回是原子的，被替换的文件先进历史目录，可以按修订回滚。任何失败都不改动现有文件。

## 基本概念

| 名称 | 含义 |
| --- | --- |
| 用户配置文件 | `$TILDY_CONFIG`，否则 `$XDG_CONFIG_HOME/tildy/config.json`（仅绝对路径），否则 `~/.config/tildy/config.json`。与 worker、`capabilities` 用同一规则（`config::default_config_path`）。是符号链接时读写链接指向的文件，链接本身保留。 |
| 修订（revision） | 文件字节的 FNV-1a 64 位十六进制（`config::content_revision`，16 位小写），文件不存在时为 `builtin`。只看字节：改一个空格修订就变。 |
| 有效配置 | 内置默认 → `preset` 指定的预设 → 用户文件，合并规则见 `src/config/mod.rs` 开头的说明。 |
| 历史目录 | `$XDG_STATE_HOME/tildy/config-history/`（仅绝对路径），否则 `~/.local/state/tildy/config-history/`。目录 0700，文件 0600。 |
| 历史条目 | `apply` 或 `rollback` 替换、删除用户文件之前，把文件原样存为 `<unix_ms>-<revision>.json`，旁边的 `<unix_ms>-<revision>.meta.json` 记录操作（`apply`/`rollback`）、原路径和取代它的修订。条目 id 即 `<unix_ms>-<revision>`。每次事务后只保留最新 50 条。文件不存在时没有可存的内容，不产生条目（回到这种状态用 `--to builtin`）。 |

## 命令

所有子命令都接受 `--json`（放在子命令前后均可，例如 `tildy config --json get`）。`--json` 时标准输出只有一个 envelope，标准错误为空；不加时输出给人看，错误与警告写到标准错误。没有 TTY 不影响任何命令。

### `tildy config get [--effective] [--json]`

读取用户文件，永远成功（只要路径能确定、文件可读），即使文件损坏：Agent 需要拿到损坏文件的修订才能写入修复。

`data`（无 `--effective`）：

| 字段 | 说明 |
| --- | --- |
| `path` | 用户配置文件路径 |
| `exists` | 文件是否存在 |
| `revision` | 当前修订，`apply`/`rollback` 的 `--expect-revision` 用它 |
| `bytes` | 文件字节数；无文件时为 null |
| `valid` | 文件能否加载；false 时提示符使用内置配置 |
| `problem` | `valid` 为 false 时的原因 `{kind, path, message}`，`kind` 为 `syntax`（不是 JSON 或不是 UTF-8）、`invalid`、`unsupported`（`schema_version` 过新） |
| `config` | 文件解析出的 JSON 值；无文件或不是 JSON 时为 null。对象键按字母序 |
| `text` | 文件原文（保留键序、注释键与格式）；无文件时为 null |

`data`（`--effective`）：`path`、`exists`、`revision`、`valid`、`problem` 同上，另有：

| 字段 | 说明 |
| --- | --- |
| `fallback` | 用户文件存在但无效、提示符正在使用内置配置时为 true |
| `preset` | 有效配置所用的预设名 |
| `config` | 合并后的有效配置（`Config.json`，含 `$comment`） |
| `sources.keys` | 有效配置每个顶层键的来源：`builtin`、`preset:<name>`、`user` 或 `project`（M4 D 开发版） |
| `sources.modules` | 每个模块 id 的来源。取定义或覆盖它的最高一层；用户文件里不带 `type` 的条目是合并到下层同名模块上的局部覆盖，同样记为 `user` |

返回值包含 `effective_revision` 和 `project`（路径、exists、valid、revision、problem）。`revision` 仍用于用户文件写事务，`effective_revision` 包含有效项目层。`--cwd DIR` 指定仓库上下文；`valid/problem/fallback` 仍指用户层，项目错误单列。`config validate --project --file F` 与 `schema --project` 用于显示专用项目文件，详见 [ADR 0012](decisions/0012-repository-display-layer.md)。

文本模式：首行 `# <路径> (revision <修订>, valid|INVALID: …|no file)`，随后是文件原文；`--effective` 打印有效配置和来源表。

### `tildy config validate [--file F] [--json]`

按加载器的规则（`config::from_str`，合并内置与预设层）校验 `F`；`-` 读标准输入；不给 `--file` 时校验当前用户文件（不存在则报告内置配置）。

- 成功：`data` 为 `{file, exists, revision, preset, warnings}`。`revision` 是 `F` 写入后将得到的修订。`warnings` 为 `[{path, message}]`，包括 provider 层的问题（`providers::from_config_checked`：例如模块 id 与内置 provider 键冲突、该模块不会有数据）和用户文件里定义了但没有放进任何布局行的模块（永远不显示）。警告不影响退出码。
- 失败：`validation_failed`（退出码 2），`error.path` 为出错字段的 JSON 路径，例如 `modules.git_main.colour`、`layout.rows[0].left[1]`、`modules.x.rules[1].id`、`schema_version`；语法错误（`message` 带行列）和整体问题时为 null。读不到 `F` 也是 `validation_failed`（输入参数的问题）。

### `tildy config apply --file F --expect-revision REV [--force] [--json]`

1. 校验 `F`，失败同 `validate`，不碰现有文件。
2. 先取实际配置目标旁 `<文件名>.lock` 的 `flock`，再取历史目录 `.lock`；两把锁共用 2 秒等待预算。锁内读当前文件。`REV` 不等于当前修订时返回 `revision_conflict`（退出码 3）。`REV` 为 `builtin` 表示"我读的时候没有文件"。格式不对的 `REV`（不是 `builtin` 也不是 16 位十六进制）是 `validation_failed`。
3. 新内容与当前文件字节相同则什么也不写，`changed: false`。
4. 在目标文件所在目录写临时文件并 `fsync`（保留原文件权限），把当前文件存入历史，再 `rename` 覆盖目标、`fsync` 目录，最后修剪历史到 50 条。任一步失败都删除临时文件和刚写的历史条目，原文件不变；目录不可写等返回 `environment_unavailable`（退出码 4）。
5. `--force` 跳过第 2 步的修订比较（仍然取锁、仍然记历史）。不带 `--force` 时 `--expect-revision` 必填。

写入的是 `F` 的原始字节，不重新格式化，所以 `validate` 报告的 `revision` 就是 `apply` 之后的修订。

锁等待超时使用现有 `environment_unavailable`（4），消息以 `config locked` 开头，包含锁路径和记录的持有者 PID（无法读取时说明 unavailable）；配置与历史不变。PID 只是诊断信息，重试前不要删除锁文件。锁随进程退出释放，持久文件本身不表示正在占用。文件和父目录符号链接按实际目标加锁，不同 `XDG_STATE_HOME` 的新版写入也互斥；不协调手工编辑器。`rollback`、`theme` / `configure` 共用此策略，详见 [ADR 0011](decisions/0011-config-lock-identity.md)。

`data`：`{path, previous_revision, revision, changed, forced, history_id, warnings}`。`history_id` 是保存旧文件的历史条目 id；原来没有文件或没有变化时为 null。

### `tildy config rollback --to REV --expect-revision CURRENT [--json]`

- `CURRENT` 与当前修订不符时返回 `revision_conflict`。
- `--to` 为修订时，在历史目录里找该修订最新的条目，校验其内容（内容与文件名的修订不符为 `internal_error`；当前版本加载不了时为 `validation_failed` 并拒绝恢复），把当前文件存入历史后原子写入。找不到条目返回 `validation_failed`，提示查看 `tildy config history`。
- `--to builtin`：把当前文件存入历史后删除它，回到零配置。
- 当前修订已经等于 `REV` 时什么也不做，`changed: false`。

`data`：`{path, previous_revision, revision, changed, history_id, restored_from}`。`restored_from` 是被恢复的条目 id，`--to builtin` 时为 null。

历史目录在所有配置路径间共享（条目按内容寻址，`path` 记录它原来的位置），`rollback` 按修订查找，与条目来自哪个路径无关。

### `tildy config history [--json]`

`data`：`{dir, path, current_revision, keep, entries}`，`entries` 按时间从新到旧：

| 字段 | 说明 |
| --- | --- |
| `id` | `<unix_ms>-<revision>`，文件为 `<id>.json` |
| `at_ms` | 存入时间（Unix 毫秒）。同一目录内严格递增 |
| `revision` | 条目内容的修订 |
| `bytes` | 字节数 |
| `op` | 替换或删除它的操作：`apply`、`rollback`；旁注文件缺失时为 `unknown` |
| `path` | 原用户配置路径（旁注缺失时为 null） |
| `replaced_by` | 该操作装上的修订（`builtin` 表示文件被删除） |

历史目录不存在时返回空列表。

### `tildy schema [--json]`

命令发现：`tildy capabilities --json` 的 `data.commands` 列出每个子命令的名称、一句话说明、嵌套子命令与是否支持 `--json`（直接从 clap 定义生成，不会与 `--help` 脱节）。`tildy configure` 的写入也走这里的事务（2026-10-20 起）：以启动时读到的修订作为 `expect`，文件期间被改过则 `revision_conflict`；原文件进同一份历史，打印的回滚命令就是 `config rollback`。

输出编译进二进制的 `schema/config.v1.schema.json`（`include_str!`）。文本模式是 Schema 原文；`--json` 时 `data` 为 `{schema_version: 1, schema: {…}}`。这是 Agent 发现字段及其说明的入口；版本、模块类型、预设列表等能力仍由 `tildy capabilities --json` 提供。

## Envelope 与退出码

所有 `--json` 输出都是 `CliEnvelope`（`schema/error.schema.json`）：

```json
{ "api_version": 1, "ok": false, "data": null,
  "error": { "code": "revision_conflict", "path": null,
             "message": "config revision is 26418910d44d1e9c, expected b4a872ab24e0deb4",
             "hint": "the file changed since it was read; run `tildy config get --json`, ..." } }
```

成功时 `ok: true`、`error.code` 为 `ok`、其余错误字段为 null；失败时 `data` 为 null、`message` 非空（不超过 1000 字符）。

| `error.code` | 退出码 | 何时出现 |
| --- | --- | --- |
| `ok` | 0 | 成功（包括 `changed: false` 与带警告的校验） |
| `validation_failed` | 2 | JSON 损坏、不是 UTF-8、未知字段、未知 `schema_version`、悬空模块引用、重复规则 id 等（带 `path`）；读不到 `--file`；缺少必填参数；修订参数格式不对；`rollback` 找不到历史条目或条目无法加载 |
| `revision_conflict` | 3 | `--expect-revision` 与当前文件的修订不符 |
| `environment_unavailable` | 4 | 无法确定配置路径或历史目录（没有 `HOME`/`XDG_*`/`TILDY_CONFIG`）；配置文件不可读；配置目录或历史目录不可写；锁文件不可用或 2 秒锁等待预算耗尽 |
| `internal_error` | 1 | 历史条目内容与其修订不符等不应发生的情况 |

`backend_unavailable`（5）不会由这些命令返回。clap 自身的参数解析错误（未知选项）仍以退出码 2 和用法说明结束，不输出 envelope。

## Schema v1 冻结与兼容策略

自 M2 起 `schema/config.v1.schema.json` 冻结为配置格式 v1（`schema_version: 1`）的公开契约：

- 它与加载器一致：模块类型枚举等于 `config::MODULE_TYPES`，预设、符号层、历史模式、行样式、规则算子、语言的枚举与 `src/config` 中的常量一致；根、模块、样式、符号、条件、规则、行、布局、历史、`agent`、`status_symbols`、`state_labels`、`state_symbols` 各对象的字段集合与加载器接受的完全相同，类型专属字段适用的模块类型、各类型的必填字段也相同。`tests/config_cli.rs` 的 `schema_matches_the_loader` 在每次 `cargo test` 中检查这些，`fixtures/validate.py` 用它校验 fixtures、内置配置和全部预设。
- M2 冻结时纳入 v1 的新增字段（均为可选，不写时行为不变）：顶层 `agent`（`wrap`、`programs`，任务 B）与 `agent_status` 模块的 `state_symbols`、`max_width`。
- Schema 表达不了的检查（合并后的跨引用、规则 id 唯一、模板语法、正则可编译、`schema_version` 过新）由加载器负责并带 JSON 路径报告，见 Schema 顶层 `description`。

v1 之后的兼容策略：

1. **只做加法**：v1 内只能新增**可选**字段、新增模块类型或枚举值，且缺省行为必须等于不写该字段时的旧行为。旧配置在新版本中加载结果不变。
2. **删除或改义需升版本**：删除字段、改变字段含义或缺省值、把可选改为必填、收紧取值范围，都必须把 `schema_version` 升到 2，新增 `schema/config.v2.schema.json`，并提供从 v1 的迁移（加载器继续接受 v1）。
3. **新版本配置在旧二进制上明确失败**：`schema_version` 大于二进制支持的版本时，加载器返回 `unsupported`（`config validate`/`apply` 报 `validation_failed`，`path` 为 `schema_version`），worker 回退到内置配置，不会按旧语义误读。
4. 新增可选字段的版本里，旧二进制会把它当作未知字段拒绝（v1 的对象都是 `additionalProperties: false`）。所以用到新字段的配置应在 `tildy capabilities --json` 的 `binary_version` 满足要求后再写；Agent 先用 `config validate` 确认。
5. 改动 Schema 时同步修改 `src/config/validate.rs` 与 fixtures，`cargo test` 和 `python3 fixtures/validate.py` 必须同时通过。

## 无交互端到端脚本

下面的脚本完成"读 → 改 → 校验 → 预览 → 应用 → 回滚"，每一步都用 `--json` 并检查 `ok`；结束时配置回到开始时的状态（开始时没有文件则文件被删除）。需要 `tildy` 在 `PATH` 中与 `python3`。`tests/config_cli.rs` 的 `end_to_end_script_from_docs` 从本文件提取这段脚本，在沙箱 `HOME` 下分别以"无用户文件"和"已有用户文件"运行（测试中 `preview` 一步由替身代替，`tildy preview` 由任务 C 交付）。

<!-- BEGIN e2e-script -->
```sh
#!/bin/sh
# get -> edit -> validate -> preview -> apply -> rollback, non-interactive.
set -eu
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Print data[KEY] of an envelope file; fail (exit 1) unless ok is true.
field() {
  python3 -c 'import json, sys
d = json.load(open(sys.argv[1]))
if not d["ok"]:
    sys.exit("tildy: %s (%s)" % (d["error"]["message"], d["error"]["code"]))
print(d["data"][sys.argv[2]] if len(sys.argv) > 2 else "ok")' "$@"
}

# 1. Read the user config and the revision the edit is based on.
tildy config get --json > "$work/get.json"
base=$(field "$work/get.json" revision)

# 2. Edit a copy: start from the user file (a fresh document when there is
#    none or it is broken) and change one key.
python3 - "$work/get.json" "$work/candidate.json" <<'EOF'
import json, sys
data = json.load(open(sys.argv[1]))["data"]
cfg = data["config"] if data["exists"] and data["valid"] else {"schema_version": 1}
cfg["symbols"] = "ascii"
with open(sys.argv[2], "w") as f:
    json.dump(cfg, f, indent=2, ensure_ascii=False)
    f.write("\n")
EOF

# 3. Validate: exit 2 with error.path on failure; warnings in data.warnings.
tildy config validate --file "$work/candidate.json" --json > "$work/validate.json"
field "$work/validate.json" warnings

# 4. Preview the candidate in a fixed scenario (no real repository, no commands).
tildy preview --scenario git-dirty --config "$work/candidate.json" --json > "$work/preview.json"
field "$work/preview.json"

# 5. Apply, guarded by the revision from step 1 (exit 3 if the file changed meanwhile).
tildy config apply --file "$work/candidate.json" --expect-revision "$base" --json > "$work/apply.json"
new=$(field "$work/apply.json" revision)
echo "applied $new (was $base)"

# 6. Roll back to where we started (`builtin` removes the file again).
tildy config rollback --to "$base" --expect-revision "$new" --json > "$work/rollback.json"
echo "rolled back to $(field "$work/rollback.json" revision)"
```
<!-- END e2e-script -->

要点：

- 第 2 步改的是副本；不要原地编辑用户文件再 `apply`，那样当前修订已经变了，会得到 `revision_conflict`。
- 第 5 步遇到退出码 3 时，重新执行第 1 步，在新内容上重做修改。
- 想撤销任意一次写入：`tildy config history --json` 找到修订，`tildy config rollback --to <修订> --expect-revision <当前修订>`。
