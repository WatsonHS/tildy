# 0007：配置事务（预期修订、原子保存、历史目录）

- 日期：2026-10-20
- 状态：已接受；2026-10-22 锁策略由 [ADR 0011](0011-config-lock-identity.md) 更新。以下保留 M2 决策背景。

## 背景

PLAN.md 的"AI 原生"目标要求 Agent 能安全地读、校验、预览、应用和回滚配置，所有操作有无交互接口和结构化结果（docs/architecture.md 目标 1、4）。已有的约束与事实：

- 配置修订 `Config.revision` 是用户文件字节的 FNV-1a 64（`config::content_revision`），无文件时为 `builtin`；worker 按 mtime/size 变化重载，加载失败回退到内置配置（`src/cli/worker.rs`）。
- `tildy configure` 已有"同目录临时文件 + `rename`，旧文件复制为 `<path>.bak-<时间>`"的写法（`src/cli/configure.rs` `write_config`），但没有修订检查，备份散落在配置目录、没有上限、不可按修订查找。
- 结果外壳 `CliEnvelope` 与退出码 0/2/3/4/5/1 已冻结（`src/model.rs`、`schema/error.schema.json`），`ErrorCode` 不能新增取值；失败时 `data` 必须为 null，`message` 不超过 1000 字符。
- 多个 Agent 可能并发修改同一份配置；用户也可能同时手工编辑。
- 用户的配置文件可能是指向 dotfiles 仓库的符号链接。
- 测试必须能把配置与历史放进临时目录（`TILDY_CONFIG`、`XDG_CONFIG_HOME`、`XDG_STATE_HOME`、`HOME`）。

## 决定

`tildy config apply|rollback` 是以文件字节修订为版本号的比较并交换：调用方带上读到的修订（`--expect-revision`，无文件为 `builtin`），在 `$XDG_STATE_HOME/tildy/config-history/.lock` 的 `flock` 下比较，不符返回 `revision_conflict`；相符则先在目标目录写好并 `fsync` 临时文件，再把当前文件原样存为历史条目 `<unix_ms>-<revision>.json`（旁注 `.meta.json` 记录操作），最后 `rename` 覆盖，任何一步失败都撤销已做的步骤、保留原文件。`apply` 只接受通过加载器校验的内容，`rollback` 只恢复当前版本能加载的历史条目，历史保留最新 50 条。

## 设计要点

### 修订与比较

- 修订只取字节，不做 JSON 规范化：格式变化也算修改，避免"内容等价但文件不同"时 Agent 覆盖用户的手工排版。`apply` 写入候选文件的原始字节，所以 `config validate` 报告的修订就是 `apply` 后的修订，Agent 不需要再读一次。
- `--expect-revision` 必填（`--force` 时可省）；格式不对（不是 `builtin` 也不是 16 位十六进制）是 `validation_failed` 而不是冲突，免得拼写错误被误报成"别人改过"。
- 新内容与当前字节相同：什么也不写、不记历史，返回 `changed: false`，让重试幂等。
- `config get` 对损坏的文件也成功返回修订与原文（`valid: false` 加 `problem`），否则 Agent 无法带着正确修订写入修复。

### 锁

- 锁文件放在历史目录而不是配置目录：历史目录归 tildy 所有，不在用户的配置目录（可能是 dotfiles 仓库）里留下文件。`flock` 随进程退出自动释放，不会留下需要清理的陈旧锁。
- 锁只串行化 tildy 的事务；手工编辑器不参与，但比较发生在锁内、紧挨着写入，窗口只有一次 `rename` 之前的几个系统调用。`tildy configure` 仍用自己的 `.bak` 写法，不取锁（见后果）。

### 写入顺序

1. 校验候选内容（`config::from_str`，与 worker 相同的分层与校验）。失败：`validation_failed`，什么都不碰，也不创建历史目录。
2. 取锁、读当前文件、比较修订。
3. 在目标所在目录写临时文件、复制原文件权限、`fsync`。配置目录不可写在这一步失败，此时还没有历史条目。
4. 把当前文件写入历史（临时名 + `rename`，0600）。失败则删除第 3 步的临时文件。
5. `rename` 临时文件覆盖目标，`fsync` 目录。失败则删除临时文件和第 4 步的条目。
6. 修剪历史到 50 条（失败忽略）。

先暂存新内容再记历史，是为了让"配置目录不可写"不留下一条"被替换了但其实没替换"的历史。

### 历史目录

- 位置按 XDG 状态目录（与会话快照同一规则 `session::state_dir_with`）：历史是机器状态，不是配置，不该进 dotfiles。
- 条目内容是被替换文件的原始字节，文件名 `<unix_ms>-<revision>.json` 按计划固定；操作类型、原路径与取代它的修订放在旁注 `<id>.meta.json`，旁注缺失时条目仍可用（`op: unknown`）。时间戳在目录内严格递增（取 `max(now, 最新条目 + 1)`），顺序与修剪不受时钟回拨影响。
- 只有存在的文件才入历史；"没有文件"由 `--to builtin` 表示，不写空条目。
- 历史在所有配置路径间共享、按修订寻址：`TILDY_CONFIG` 切换到另一个文件时仍能按修订恢复内容，条目的 `path` 说明它原来在哪。

### 回滚

- `rollback --to REV` 取该修订最新的条目，先核对内容修订与文件名一致（不一致为 `internal_error`），再用当前版本校验；加载不了的条目拒绝恢复（`validation_failed`，提示里给出条目文件路径，用户可以手工复制）。这样事务命令装上的永远是能加载的配置，与"任何失败保留有效配置"一致。
- `--to builtin` 删除用户文件（先存历史），回到零配置。

### 符号链接

- 配置路径是符号链接时，沿链接（最多 16 层）找到真正的文件，在它的目录里暂存并 `rename`，链接本身不变；`--to builtin` 删除链接指向的文件，留下悬空链接，之后的 `apply` 经链接重建文件。与 `tildy configure` 的"链接保持为链接"一致。

### 错误码

| 情况 | 码 |
| --- | --- |
| 候选内容无效（带 `path`）、读不到 `--file`、缺少必填参数、修订格式不对、历史条目缺失或无法加载 | `validation_failed`（2） |
| 修订不符 | `revision_conflict`（3） |
| 配置路径或历史目录无法确定、配置文件不可读、配置目录或历史目录不可写 | `environment_unavailable`（4） |
| 历史条目内容与其修订不符 | `internal_error`（1） |

"历史条目缺失"归为 `validation_failed`：它是调用方给的参数指向不存在的东西，重新查询 `config history` 后改参数即可，与 `revision_conflict`（需要重新读文件）和环境问题（需要修目录权限）的处理方式不同。

## 证据

- 契约测试 `tests/config_cli.rs`（22 个，`cargo test --test config_cli`，2026-10-20 本机通过）：每个命令的成功路径；损坏 JSON、非对象、非 UTF-8、未知版本、未知字段、悬空模块引用、重复规则 id、未知预设在 `validate` 与 `apply` 下的 `code`/`path`/退出码；修订冲突；配置目录、状态目录、历史目录不可写时退出码 4 且原文件字节不变、无临时文件残留、无历史条目；历史条目缺失、内容损坏、无法加载；保留 50 条；符号链接与文件权限；无 `HOME` 时的环境错误。每个 `--json` 调用都断言标准输出恰好是一个 envelope、标准错误为空、退出码与 `error.code` 一致。
- 并发：`concurrent_applies_with_one_revision_have_one_winner` 同时启动 8 个 `apply`（同一 `--expect-revision`、不同内容），恰好 1 个成功、7 个返回 `revision_conflict`，历史只多 1 条。对照实验（2026-10-20）：临时让 `Store::lock` 跳过 `flock`，连续运行该测试 5 次，5 次都是 8 个 `apply` 全部成功（每个都基于同一修订覆盖了前一个），说明比较必须在锁内进行。
- 端到端：`end_to_end_script_from_docs` 从 docs/configuration.md 提取脚本，在"无文件""已有文件"两种起点下运行 get → 改 → validate → preview（替身）→ apply → rollback，结束时文件字节与起点相同。
- Schema 冻结：`schema_matches_the_loader` 用加载器自己的报错（未知字段列表、"only valid for"、"required"）逐一比对 Schema 中各对象的字段集合、类型专属字段、按类型必填字段与各枚举；`python3 fixtures/validate.py` 通过（65 个样本）。
- 被排除的方案：
  - 用 JSON 规范化后的哈希作修订：与 worker 已有的字节修订不一致，且会把格式改动当成"无变化"。
  - 历史放在配置目录（`config.json.bak-*`，`configure` 的做法）：无上限、混进 dotfiles、不能按修订查找。
  - 单一追加日志 `history.jsonl` 记录操作：修剪需要改写日志，与条目文件的删除不能原子地一起完成；旁注文件随条目一起删除即可。
  - 锁文件放在配置目录或用 `O_EXCL` 锁文件：前者污染用户目录，后者进程崩溃后留下陈旧锁。
  - `rollback` 原样恢复无法加载的条目：会让事务命令装上 worker 必然回退的配置；保留手工复制的出路即可。

## 后果

- 正面影响：Agent 只需 `get`（拿修订）→ `validate` → `apply --expect-revision` 三步就能安全写配置，并发写入不会互相覆盖；任何失败都不改动原文件；每次写入都可按修订撤销；`tildy schema` 给出字段说明，Schema 与加载器的一致性由测试守护。
- 负面影响与代价：历史目录最多 50 份配置副本加旁注（通常每份几百字节）；每次事务多一次 `fsync` 与几次 `rename`；修订只看字节，纯格式改动也会让 Agent 的修订过期而需要重读。
- 需要后续处理的问题：
  - ~~`tildy configure` 的写入仍走 `.bak` 备份~~ 2026-10-20 已改：`configure` 调用 `Store::apply(.., expect = 启动时读到的修订, force = false)`，与 `config apply` 共用历史、锁与冲突检测；期间被改过的文件以 `revision_conflict` 拒绝。
  - ~~`capabilities --json` 可以列出 `config` 子命令~~ 2026-10-20 已加 `commands`（从 clap 定义生成：名称、说明、子命令、是否支持 `--json`）。
  - 任务 B 新增的 `agent` 顶层段与 `agent_status` 的 `state_symbols`、`max_width` 已按"只做加法"写入 Schema v1；之后对这些字段的任何改动都要同步 Schema，否则 `schema_matches_the_loader` 失败。
