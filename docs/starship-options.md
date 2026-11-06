# Starship 配置项覆盖表（M2 任务 E1）

日期：2026-10-20。依据：Starship 1.24.2 的 `docs/config/README.md`（<https://starship.rs/config/>，本机 `starship 1.24.2` 的 `print-config --default` 核对默认值）。选项名与默认值从 Starship 文档的选项表逐项提取（不漏项），状态与 E1 完成后的代码一致；改动导入器或模块时请同步本表。

## 怎么读

| 状态 | 含义 |
| --- | --- |
| 原生 | tildy 配置里有对应字段（表中给出字段名）；`tildy import starship` 也会映射它 |
| 导入映射 | tildy 没有同名字段，但导入器把它翻译成等价配置（`show_when`、规则、行结构等） |
| 桥接 | 模块整体走 `{"type":"starship","module":NAME}`：由 `starship module NAME` 渲染，读用户自己的 starship.toml，所以该模块的全部选项照常生效（需要装 Starship，每个模块一个进程，带检测列表时不命中不启动） |
| 需 Shell 字段 | 需要 Shell 在 REQ 里多发数据（当前适配层没有），未实现，导入时记为不支持 |
| 不支持 | 没有实现，原因见说明；导入时进入 `unsupported` 清单（标“静默接受”的除外） |
| 不适用 | 只对 Windows/WSL 等 tildy 不支持的环境有意义 |

每个原生模块的 `disabled`：导入时 `disabled = true`（或 Starship 默认关闭且未打开）的模块被跳过并列入 `skipped`。

## 汇总

Starship 1.24.2 文档列出 102 个配置段（含全局与 custom）共 804 个选项，另加未列在表里的全局 `continuation_prompt`，合计 805 项：

| 状态 | 选项数 |
| --- | --- |
| 原生 | 207 |
| 导入映射 | 41 |
| 桥接 | 510 |
| 需 Shell 字段 | 8 |
| 不支持 | 35 |
| 不适用 | 4 |
| 合计 | 805 |

原生加导入映射覆盖 248 项，集中在日常使用的 32 个模块（下表“原生模块”）；其余 68 个模块的 510 项走 Starship 桥接；不支持 35 项（其中 custom 命令 16 项）、需 Shell 字段 8 项、不适用 4 项。

M2 任务 E1 新增：全局 `add_newline`、`command_timeout_ms`；原生模块 `package`、`git_commit`、`git_metrics`、`shlvl`、`localip`、`memory_usage`、`sudo`（默认不在任何布局里）、`direnv`、`nix_shell`、`shell`；Starship `status` 导入为原生 `exit_status`；directory、git_branch、git_status、git_state、cmd_duration、hostname、username、time、status、jobs、docker_context 的选项（逐项见下）。

## 与 Starship 行为不同之处（原生模块）

- `add_newline`：空行是提示符的第一行；transient（compact 历史）收起后空行保留在历史里（p10k 的做法，2026-10-20 起），`history.add_newline: false` 可关掉；Starship 没有 transient。
- `hostname`：完整主机名取自 Shell 转发的 `HOST`（Zsh 的 `$HOST`），没有时退回会话 provider 已在第一个点截断的名字，此时 `trim_at = ""` 也看不到完整名字。
- `username`：Starship 在“当前用户不是登录用户”时也显示，tildy 不比较 LOGNAME（未转发）。
- `git_commit`：默认 7 位短哈希来自 git 状态 provider，不额外起进程；`commit_hash_length > 7` 或启用标签时才有模块 provider。`tag_max_candidates` 按 `git describe` 的距离判断。
- `git_metrics`：`git diff HEAD --shortstat`（暂存加未暂存，不含未跟踪文件，与 Starship 默认的 gitoxide 路径一致）；未出生的 HEAD 退回 `git diff --shortstat`。tildy 默认格式是 `$diff`（`+A -D`，按 added_style/deleted_style 着色），因为样式变量不能写进 tildy 模板的 span。
- `git_status`：`typechanged` 不支持（见表）；`up_to_date` 在有上游且不领先不落后时显示，上游已删除（gone）时 tildy 也会显示。
- `sudo`：跑 `sudo -n true`（默认 500 ms 超时）。worker 与 Shell 同会话、同控制终端，按终端区分的 sudo 票据照常生效；没有任何内置布局或预设启用它。
- `direnv`：跑 `direnv status --json`，只把请求 env 里的 DIRENV_DIR、DIRENV_FILE 传给它（worker 自己的 DIRENV_* 先清掉；需要 Shell 转发，见下）。`loadedRC` 为 null 时 tildy 显示 `not loaded`，Starship 1.24.2 在这种 JSON 上解析失败而隐藏模块。
- `memory_usage`：macOS 用 `host_statistics64` 的 free + inactive 页（与 Starship 所用 systemstat 一致），Linux 用 MemAvailable；显示格式同 Starship（`21GiB/32GiB`，小数舍去）。
- `shell`：REQ 只来自 Zsh 适配层，所以总是 `zsh` 的指示符；将来别的适配层可以转发 `TILDY_SHELL`。
- `time`：注入时间（测试、预览）在 `utc_time_offset` 下按 UTC 解释；`time_range` 按本地时间判断（与 Starship 相同）。
- `directory`：`read_only` 在渲染时用 `access(W_OK)` 判断，不存在的目录不算只读。`repo_root_style` 未设置而只写 `repo_root_format` 时仍按 M1b 行为在仓库根处套用它（Starship 只在设置 `repo_root_style` 时套用）。
- `git_state`：格式里没有 `$git_state` 时 Starship 不显示进行中状态，tildy 的 git 模块默认显示（导入不写 `show_state: false`，作者配置的导入结果因此不变）。

## Shell 需要新增转发的环境变量

渲染器与 provider 已经从请求 env（`PromptRequest::env_map()`）读取下列键，但 `shell/zsh/tildy.zsh` 的 `_tildy_env_field` 与 `bench/fake-backend.sh` 的白名单还没有它们；缺失时对应模块隐藏或退回旧行为：

| 键 | 模块 | 缺失时 |
| --- | --- | --- |
| SHLVL | shlvl | 模块隐藏 |
| IN_NIX_SHELL | nix_shell | 模块隐藏 |
| name | nix_shell（`$name`） | 名字为空（小写变量名，nix-shell 设置） |
| DIRENV_FILE | direnv（`detect_env_vars` 默认值；传给 `direnv status` 作为已加载的 rc） | 只按 .envrc 检测；`$loaded` 总是 not loaded |
| DIRENV_DIR | direnv（传给 `direnv status`） | 同上。DIRENV_DIFF、DIRENV_WATCHES 体积大且不需要，不必转发 |
| SSH_CONNECTION（已转发） | localip `ssh_only`、hostname `$ssh_symbol` | — |
| HOST（已转发） | hostname `trim_at` | — |
| PATH（已转发） | nix_shell `heuristic`、sudo/direnv 的 PATH | — |
| TILDY_SHELL | shell | 按 zsh 处理（只有非 Zsh 适配层才需要） |
| 用户在 `detect_env_vars` 里写的任何变量 | hostname、username、direnv | 视为未设置 |

## 原生模块

### 全局选项 → tildy `顶层`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'$all'` | 导入映射 | `layout.rows`：模块顺序、换行与 `$line_break` 分行，`[](bg:A fg:B)` 识别为 powerline 行的分隔符，其余字面段成为 `text` 模块 |
| `right_format` | `''` | 导入映射 | 最后一行的 `right` |
| `scan_timeout` | `30` | 不支持 | tildy 每个模块最多扫描 500 个目录项，不按时间预算；导入时记为不支持 |
| `command_timeout` | `500` | 原生 | `command_timeout_ms`（顶层，provider 类模块 `timeout_ms` 的默认值，默认 500） |
| `add_newline` | `true` | 原生 | `add_newline`（顶层；tildy 默认 false，导入时除非写了 `false` 都输出 `true`）。渲染器输出一个空的第一行，transient 也带前导空行（`history.add_newline: false` 关掉） |
| `palette` | `''` | 导入映射 | 导入时按所选调色板把颜色名解析成具体颜色值；原生 `palette` 属于任务 E2 |
| `palettes` | `{}` | 导入映射 | 同上；原生 `palettes` 属于任务 E2 |
| `follow_symlinks` | `true` | 不支持 | tildy 检测目录时总是跟随符号链接；导入时 `false` 记为不支持 |
| `continuation_prompt` | `'[∙](bright-black) '` | 不支持 | tildy 不画 PS2，续行由 Zsh 自己的 PS2 显示；导入时记为不支持 |

### `username` → tildy `username`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `style_root` | `'bold red'` | 导入映射 | 导入为规则 `rules[{id: root, when: session.username eq root, style}]` |
| `style_user` | `'bold yellow'` | 原生 | `style` |
| `detect_env_vars` | `[]` | 原生 | `detect_env_vars`（读请求 env；`!NAME` 隐藏） |
| `format` | `'[$user]($style) in '` | 原生 | `format`；导入时转换模板，变量 `$user` |
| `show_always` | `false` | 原生 | `show_always` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |
| `aliases` | `{}` | 原生 | `aliases` |

### `hostname` → tildy `hostname`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `ssh_only` | `true` | 导入映射 | `show_when: {field: session.ssh, op: eq, value: true}` |
| `ssh_symbol` | `'🌐 '` | 原生 | `symbol`；`$ssh_symbol` 只在 SSH 会话（请求 env 有 SSH_CONNECTION）时有值 |
| `trim_at` | `'.'` | 原生 | `trim_at`（截取 Shell 的 `$HOST`；无 HOST 时用会话 provider 的短主机名） |
| `detect_env_vars` | `[]` | 原生 | `detect_env_vars` |
| `format` | `'[$ssh_symbol$hostname]($style) in '` | 原生 | `format`；导入时转换模板，变量 `$hostname`→`$host`、`$ssh_symbol` |
| `style` | `'bold dimmed green'` | 原生 | `style` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |
| `aliases` | `{}` | 原生 | `aliases`（在 `trim_at` 之后匹配） |

### `localip` → tildy `localip`（M2 新增原生）

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `ssh_only` | `true` | 原生 | `ssh_only`（默认 true） |
| `format` | `'[$localipv4]($style) '` | 原生 | `format`；导入时转换模板，变量 `$localipv4` |
| `style` | `'bold yellow'` | 原生 | `style` |
| `disabled` | `true` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `shlvl` → tildy `shlvl`（M2 新增原生）

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `threshold` | `2` | 原生 | `threshold`（默认 2） |
| `format` | `'[$symbol$shlvl]($style) '` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$shlvl` |
| `symbol` | `'↕️  '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `repeat` | `false` | 原生 | `repeat` |
| `repeat_offset` | `0` | 原生 | `repeat_offset` |
| `style` | `'bold yellow'` | 原生 | `style` |
| `disabled` | `true` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `directory` → tildy `directory`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `truncation_length` | `3` | 原生 | `truncation_length` |
| `truncate_to_repo` | `true` | 原生 | `truncate_to_repo` |
| `format` | `'[$path]($style)[$read_only]($read_only_style) '` | 原生 | `format`；导入时转换模板，变量 `$path`、`$read_only` |
| `style` | `'bold cyan'` | 原生 | `style` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |
| `read_only` | `'🔒'` | 原生 | `read_only`（`$read_only` 在 cwd 不可写时有值；tildy 默认格式 `$path` 不含它，Starship 默认格式导入后带上） |
| `read_only_style` | `'red'` | 原生 | `read_only_style`；导入时样式留在格式的 `[$read_only](…)` 里 |
| `truncation_symbol` | `''` | 原生 | `truncation_symbol` |
| `before_repo_root_style` |  | 原生 | `before_repo_root_style` |
| `repo_root_style` |  | 原生 | `repo_root_style`（设置后在仓库根处拆分路径：有 `repo_root_format` 用它，否则把 `format` 里的 `$path` 当作 `$before_root_path$repo_root$path`） |
| `repo_root_format` | `'[$before_root_path]($before_repo_root_style)[$repo_root]($repo_root_style)[$path]($style)[$read_only]($read_only_style) '` | 原生 | `repo_root_format`（变量 `$before_root_path`、`$repo_root`、`$path`、`$read_only`） |
| `home_symbol` | `'~'` | 原生 | `home_symbol` |
| `use_os_path_sep` | `true` | 不适用 | tildy 只在 Unix 上运行，分隔符总是 `/`；导入时静默接受 |
| `substitutions` |  | 原生 | `substitutions`（含 `/` 的键按最长前缀，其余按整段） |
| `fish_style_pwd_dir_length` | `0` | 原生 | `fish_style_pwd_dir_length`（有 substitutions 时不生效，与 Starship 相同） |
| `use_logical_path` | `true` | 原生 | `use_logical_path`（false 时解析符号链接） |

### `git_branch` → tildy `git`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `always_show_remote` | `false` | 原生 | `always_show_remote` |
| `format` | `'on [$symbol$branch(:$remote_branch)]($style) '` | 原生 | `format`；导入时转换模板，变量 `$symbol`→`$branch_symbol`、`$branch`、`$remote_name`、`$remote_branch` |
| `symbol` | `' '` | 原生 | `branch_symbol` |
| `style` | `'bold purple'` | 原生 | `style` |
| `truncation_length` | `2^63 - 1` | 原生 | `truncation_length`（git 模块） |
| `truncation_symbol` | `'…'` | 原生 | `truncation_symbol`（git 模块，取首字符） |
| `only_attached` | `false` | 原生 | `only_attached` |
| `ignore_branches` | `[]` | 原生 | `ignore_branches` |
| `ignore_bare_repo` | `false` | 不支持 | tildy 的 git 数据在裸仓库里视为不在仓库，模块总是隐藏（相当于 true）；导入时记为不支持 |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `git_commit` → tildy `git_commit`（M2 新增原生）

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `commit_hash_length` | `7` | 原生 | `commit_hash_length`（≤7 用 git provider 的短哈希；更长时由模块 provider 跑 `git rev-parse HEAD`） |
| `format` | `'[\($hash$tag\)]($style) '` | 原生 | `format`；导入时转换模板，变量 `$hash`、`$tag` |
| `style` | `'bold green'` | 原生 | `style` |
| `only_detached` | `true` | 原生 | `only_detached`（默认 true） |
| `tag_disabled` | `true` | 原生 | `tag_disabled`（false 时 provider 跑 `git describe --tags --long`） |
| `tag_max_candidates` | `0` | 原生 | `tag_max_candidates`（标签最多落后 HEAD 的提交数；git describe 的距离） |
| `tag_symbol` | `' 🏷  '` | 原生 | `tag_symbol` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `git_state` → tildy `git`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `rebase` | `'REBASING'` | 原生 | `state_labels.rebase`（只取文字，标签内样式不支持） |
| `merge` | `'MERGING'` | 原生 | `state_labels.merge` |
| `revert` | `'REVERTING'` | 原生 | `state_labels.revert` |
| `cherry_pick` | `'CHERRY-PICKING'` | 原生 | `state_labels.cherry-pick` |
| `bisect` | `'BISECTING'` | 原生 | `state_labels.bisect` |
| `am` | `'AM'` | 原生 | `state_labels.am` |
| `am_or_rebase` | `'AM/REBASE'` | 不支持 | tildy 能区分 am 与 rebase，不需要这个歧义标签；导入时记为不支持 |
| `style` | `'bold yellow'` | 原生 | `state_style` |
| `format` | `'\([$state( $progress_current/$progress_total)]($style)\) '` | 不支持 | tildy 的进行中状态是固定的 `(LABEL n/m)` 尾巴；导入不转换 |
| `disabled` | `false` | 导入映射 | `disabled = true` 导入为 `show_state: false`；格式里没有 `$git_state` 时不反映（git 模块默认显示状态） |

### `git_metrics` → tildy `git_metrics`（M2 新增原生）

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `added_style` | `'bold green'` | 原生 | `added_style`（tildy 默认格式 `$diff` 用；导入时样式留在格式的 span 里） |
| `deleted_style` | `'bold red'` | 原生 | `deleted_style` |
| `only_nonzero_diffs` | `true` | 原生 | `only_nonzero_diffs` |
| `format` | `'([+$added]($added_style) )([-$deleted]($deleted_style) )'` | 原生 | `format`；导入时转换模板，变量 `$added`、`$deleted`（tildy 另有 `$diff`） |
| `disabled` | `true` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |
| `ignore_submodules` | `false` | 原生 | `ignore_submodules` |

### `git_status` → tildy `git`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'([\[$all_status$ahead_behind\]]($style) )'` | 原生 | `format`；导入时转换模板，变量 `$all_status`、`$ahead_behind` 与各计数变量（`$typechanged` 丢弃） |
| `conflicted` | `'='` | 原生 | `status_symbols.conflicted`（可含 `$count`、样式 span） |
| `ahead` | `'⇡'` | 原生 | `status_symbols.ahead` |
| `behind` | `'⇣'` | 原生 | `status_symbols.behind` |
| `diverged` | `'⇕'` | 原生 | `status_symbols.diverged`（`$ahead_count`、`$behind_count`） |
| `up_to_date` | `''` | 原生 | `status_symbols.up_to_date`（有上游且不领先不落后时） |
| `untracked` | `'?'` | 原生 | `status_symbols.untracked` |
| `stashed` | `'\$'` | 原生 | `status_symbols.stashed` |
| `modified` | `'!'` | 原生 | `status_symbols.modified` |
| `staged` | `'+'` | 原生 | `status_symbols.staged` |
| `renamed` | `'»'` | 原生 | `status_symbols.renamed` |
| `deleted` | `'✘'` | 原生 | `status_symbols.deleted` |
| `typechanged` | `""` | 不支持 | `GitData` 没有类型变更计数（模型冻结，且 `src/cli/preview.rs` 逐字段构造 GitData）；类型变更计入 modified/staged。导入时非空值记为不支持 |
| `style` | `'bold red'` | 原生 | `style` |
| `ignore_submodules` | `false` | 原生 | `ignore_submodules`（git 模块；`git status --ignore-submodules=dirty`） |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |
| `windows_starship` |  | 不适用 | 只用于 WSL 下的 Windows 路径；导入时记为不支持 |
| `use_git_executable` | `false` | 不适用 | tildy 总是调用 git 可执行文件；导入时静默接受 |

### `docker_context` → tildy `docker_context`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'via [$symbol$context]($style) '` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$context` |
| `symbol` | `'🐳 '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `only_with_files` | `true` | 导入映射 | false 导入为三个空检测列表（tildy 的空规则总是命中） |
| `detect_extensions` | `[]` | 原生 | `detect_extensions` |
| `detect_files` | `['compose.yml', 'compose.yaml', 'docker-compose.yml', 'docker-compose.yaml', 'Dockerfile']` | 原生 | `detect_files` |
| `detect_folders` | `[]` | 原生 | `detect_folders` |
| `style` | `'blue bold'` | 原生 | `style` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `package` → tildy `package`（M2 新增原生）

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'is [$symbol$version]($style) '` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$version` |
| `symbol` | `'📦 '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `version_format` | `'v${raw}'` | 原生 | `version_format` |
| `style` | `'bold 208'` | 原生 | `style` |
| `display_private` | `false` | 原生 | `display_private` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `c` → tildy `language (c)`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'via [$symbol($version(-$name) )]($style)'` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$version`（编译器名 `$name` 丢弃） |
| `version_format` | `'v${raw}'` | 原生 | `version_format` |
| `symbol` | `'C '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `detect_extensions` | `['c', 'h']` | 原生 | `detect_extensions` |
| `detect_files` | `[]` | 原生 | `detect_files` |
| `detect_folders` | `[]` | 原生 | `detect_folders` |
| `commands` | `[ [ 'cc', '--version' ], [ 'gcc', '--version' ], [ 'clang', '--version' ] ]` | 不支持 | tildy 只跑 `cc --version`；其他编译器请用 `command` 模块 |
| `style` | `'bold 149'` | 原生 | `style` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `golang` → tildy `language (golang)`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'via [$symbol($version )]($style)'` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$version` |
| `version_format` | `'v${raw}'` | 原生 | `version_format` |
| `symbol` | `'🐹 '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `detect_extensions` | `['go']` | 原生 | `detect_extensions` |
| `detect_files` | `['go.mod', 'go.sum', 'go.work', 'glide.yaml', 'Gopkg.yml', 'Gopkg.lock', '.go-version']` | 原生 | `detect_files` |
| `detect_folders` | `['Godeps']` | 原生 | `detect_folders` |
| `style` | `'bold cyan'` | 原生 | `style` |
| `not_capable_style` | `'bold red'` | 不支持 | tildy 不比较工具链与 go.mod 的 go 指令 |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `java` → tildy `language (java)`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'via [${symbol}(${version} )]($style)'` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$version` |
| `version_format` | `'v${raw}'` | 原生 | `version_format` |
| `detect_extensions` | `['java', 'class', 'gradle', 'jar', 'cljs', 'cljc']` | 原生 | `detect_extensions` |
| `detect_files` | `['pom.xml', 'build.gradle.kts', 'build.sbt', '.java-version', 'deps.edn', 'project.clj', 'build.boot', '.sdkmanrc']` | 原生 | `detect_files` |
| `detect_folders` | `[]` | 原生 | `detect_folders` |
| `symbol` | `'☕ '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `style` | `'red dimmed'` | 原生 | `style` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `nodejs` → tildy `language (nodejs)`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'via [$symbol($version )]($style)'` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$version` |
| `version_format` | `'v${raw}'` | 原生 | `version_format` |
| `symbol` | `' '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `detect_extensions` | `['js', 'mjs', 'cjs', 'ts', 'mts', 'cts']` | 原生 | `detect_extensions` |
| `detect_files` | `['package.json', '.node-version', '.nvmrc']` | 原生 | `detect_files` |
| `detect_folders` | `['node_modules']` | 原生 | `detect_folders` |
| `style` | `'bold green'` | 原生 | `style` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |
| `not_capable_style` | `'bold red'` | 不支持 | tildy 不检查 package.json 的 engines.node |

### `rust` → tildy `language (rust)`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'via [$symbol($version )]($style)'` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$version` |
| `version_format` | `'v${raw}'` | 原生 | `version_format` |
| `symbol` | `'🦀 '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `detect_extensions` | `['rs']` | 原生 | `detect_extensions` |
| `detect_files` | `['Cargo.toml']` | 原生 | `detect_files` |
| `detect_folders` | `[]` | 原生 | `detect_folders` |
| `style` | `'bold red'` | 原生 | `style` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `python` → tildy `python`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'via [${symbol}${pyenv_prefix}(${version} )(\($virtualenv\) )]($style)'` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$version`、`$virtualenv`→`$env`（`$pyenv_prefix` 丢弃） |
| `version_format` | `'v${raw}'` | 原生 | `version_format` |
| `symbol` | `'🐍 '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `style` | `'yellow bold'` | 原生 | `style` |
| `pyenv_version_name` | `false` | 不支持 | tildy 不查询 pyenv |
| `pyenv_prefix` | `'pyenv'` | 不支持 | 同上 |
| `python_binary` | `['python', 'python3', 'python2']` | 不支持 | tildy 先试 `$VIRTUAL_ENV/bin/python`，再 python3、python；不可配置 |
| `detect_extensions` | `['py', 'ipynb']` | 原生 | `detect_extensions` |
| `detect_files` | `['.python-version', 'Pipfile', '__init__.py', 'pyproject.toml', 'requirements.txt', 'setup.py', 'tox.ini']` | 原生 | `detect_files` |
| `detect_folders` | `[]` | 原生 | `detect_folders` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `conda` → tildy `env_var`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `truncation_length` | `1` | 不支持 | 环境名总是完整显示 |
| `symbol` | `'🅒 '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `style` | `'bold green'` | 原生 | `style` |
| `format` | `'via [$symbol$environment]($style) '` | 原生 | `format`；导入时转换模板，变量 `$environment`→`$env_value` |
| `ignore_base` | `true` | 导入映射 | `show_when: env.CONDA_DEFAULT_ENV ne base`（导入为 `env_var` 模块） |
| `detect_env_vars` | `["!PIXI_ENVIRONMENT_NAME"]` | 不支持 | conda 模块只读 CONDA_DEFAULT_ENV |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `nix_shell` → tildy `nix_shell`（M2 新增原生）

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'via [$symbol$state( \($name\))]($style) '` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$state`、`$name` |
| `symbol` | `'❄️ '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `style` | `'bold blue'` | 原生 | `style` |
| `impure_msg` | `'impure'` | 原生 | `impure_msg` |
| `pure_msg` | `'pure'` | 原生 | `pure_msg` |
| `unknown_msg` | `''` | 原生 | `unknown_msg` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |
| `heuristic` | `false` | 原生 | `heuristic`（PATH 含 /nix/store） |

### `memory_usage` → tildy `memory_usage`（M2 新增原生）

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `threshold` | `75` | 原生 | `threshold`（默认 75；-1 总是显示） |
| `format` | `'via $symbol [${ram}( \| ${swap})]($style) '` | 原生 | `format`；导入时转换模板，变量 `$ram`、`$ram_pct`、`$swap`、`$swap_pct` |
| `symbol` | `'🐏'` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `style` | `'bold dimmed white'` | 原生 | `style` |
| `disabled` | `true` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `direnv` → tildy `direnv`（M2 新增原生）

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'[$symbol$loaded/$allowed]($style) '` | 原生 | `format`；导入时转换模板，变量 `$loaded`、`$allowed`、`$rc_path` |
| `symbol` | `'direnv '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `style` | `'bold orange'` | 原生 | `style` |
| `disabled` | `true` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |
| `detect_extensions` | `[]` | 原生 | `detect_extensions` |
| `detect_files` | `['.envrc']` | 原生 | `detect_files` |
| `detect_folders` | `[]` | 原生 | `detect_folders` |
| `detect_env_vars` | `['DIRENV_FILE']` | 原生 | `detect_env_vars`（默认 DIRENV_FILE） |
| `allowed_msg` | `'allowed'` | 原生 | `allowed_msg` |
| `not_allowed_msg` | `'not allowed'` | 原生 | `not_allowed_msg` |
| `denied_msg` | `'denied'` | 原生 | `denied_msg` |
| `loaded_msg` | `'loaded'` | 原生 | `loaded_msg` |
| `unloaded_msg` | `'not loaded'` | 原生 | `unloaded_msg` |

### `sudo` → tildy `sudo`（M2 新增原生）

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'[as $symbol]($style)'` | 原生 | `format`；导入时转换模板，变量 `$symbol` |
| `symbol` | `'🧙 '` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `style` | `'bold blue'` | 原生 | `style` |
| `allow_windows` | `false` | 不适用 | tildy 不支持 Windows；导入时静默接受 |
| `disabled` | `true` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `cmd_duration` → tildy `duration`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `min_time` | `2_000` | 原生 | `min_time`；导入保持 `show_when: command.duration_ms ge N`（与 M1b 金样一致） |
| `show_milliseconds` | `false` | 原生 | `show_milliseconds`（`$human` 带毫秒）；导入改用 `$human_ms` |
| `format` | `'took [$duration]($style) '` | 原生 | `format`；导入时转换模板，变量 `$duration`→`$human`/`$human_ms` |
| `style` | `'bold yellow'` | 原生 | `style` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |
| `show_notifications` | `false` | 不支持 | 不发桌面通知；导入时 true 记为不支持 |
| `min_time_to_notify` | `45_000` | 不支持 | 同上（静默接受） |
| `notification_timeout` |  | 不支持 | 同上（静默接受） |

### `line_break` → tildy `行`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `disabled` | `false` | 导入映射 | 未禁用时 `$line_break` 把格式分成两行 |

### `jobs` → tildy `jobs`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `threshold` | `1` | 不支持 | Starship 已弃用；导入时记为不支持 |
| `symbol_threshold` | `1` | 原生 | `symbol_threshold` |
| `number_threshold` | `2` | 原生 | `number_threshold`（tildy 默认 1，Starship 2；导入写出 2） |
| `format` | `'[$symbol$number]($style) '` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$number` |
| `symbol` | `'✦'` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `style` | `'bold blue'` | 原生 | `style` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `time` → tildy `time`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'at [$time]($style) '` | 原生 | `format`；导入时转换模板，变量 `$time` |
| `use_12hr` | `false` | 原生 | `use_12hr`（默认格式 `%r`）；导入写成等价的 `time_format` |
| `time_format` | see below | 原生 | `time_format`（strftime 子集 `%R %H %M %S %I %p %Y %m %d %F %T %r %%`；其他说明符导入时改为 `%T` 并记录） |
| `style` | `'bold yellow'` | 原生 | `style` |
| `utc_time_offset` | `'local'` | 原生 | `utc_time_offset` |
| `disabled` | `true` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |
| `time_range` | `'-'` | 原生 | `time_range` |

### `status` → tildy `exit_status`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'[$symbol$status]($style) '` | 原生 | `format`；导入时转换模板，变量 `$status`/`$int`、`$hex_status`、`$common_meaning`、`$signal_number`、`$signal_name`、`$maybe_int`（导入为 `exit_status`） |
| `symbol` | `'❌'` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `success_symbol` | `''` | 原生 | `success_symbol`（非空时 0 也显示） |
| `not_executable_symbol` | `'🚫'` | 原生 | `not_executable_symbol` |
| `not_found_symbol` | `'🔍'` | 原生 | `not_found_symbol` |
| `sigint_symbol` | `'🧱'` | 原生 | `sigint_symbol` |
| `signal_symbol` | `'⚡'` | 原生 | `signal_symbol` |
| `style` | `'bold red'` | 原生 | `style` |
| `success_style` |  | 原生 | `success_style` |
| `failure_style` |  | 原生 | `error_style` |
| `recognize_signal_code` | `true` | 原生 | `recognize_signal_code` |
| `map_symbol` | `false` | 原生 | `map_symbol` |
| `pipestatus` | `false` | 需 Shell 字段 | 需要 Shell 在 REQ 里发 `$pipestatus`；导入时记为不支持 |
| `pipestatus_separator` | <code>&vert;</code> | 需 Shell 字段 | 同上 |
| `pipestatus_format` | `'\[$pipestatus\] => [$symbol$common_meaning$signal_name$maybe_int]($style) '` | 需 Shell 字段 | 同上 |
| `pipestatus_segment_format` |  | 需 Shell 字段 | 同上 |
| `disabled` | `true` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `os` → tildy `os`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'[$symbol]($style)'` | 原生 | `format`；导入时转换模板，变量 `$symbol`、`$name`/`$type` |
| `style` | `'bold white'` | 原生 | `style` |
| `disabled` | `true` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |
| `symbols` |  | 原生 | `os_symbols`（只有操作系统名；不识别 Linux 发行版） |

### `shell` → tildy `shell`（M2 新增原生）

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `bash_indicator` | `'bsh'` | 原生 | `indicators.bash` |
| `fish_indicator` | `'fsh'` | 原生 | `indicators.fish` |
| `zsh_indicator` | `'zsh'` | 原生 | `indicators.zsh` |
| `powershell_indicator` | `'psh'` | 原生 | `indicators.powershell` |
| `pwsh_indicator` |  | 原生 | `indicators.pwsh` |
| `ion_indicator` | `'ion'` | 原生 | `indicators.ion` |
| `elvish_indicator` | `'esh'` | 原生 | `indicators.elvish` |
| `tcsh_indicator` | `'tsh'` | 原生 | `indicators.tcsh` |
| `xonsh_indicator` | `'xsh'` | 原生 | `indicators.xonsh` |
| `cmd_indicator` | `'cmd'` | 原生 | `indicators.cmd` |
| `nu_indicator` | `'nu'` | 原生 | `indicators.nu` |
| `unknown_indicator` | `''` | 原生 | `unknown_indicator` |
| `format` | `'[$indicator]($style) '` | 原生 | `format`；导入时转换模板，变量 `$indicator` |
| `style` | `'white bold'` | 原生 | `style` |
| `disabled` | `true` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `character` → tildy `character`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `format` | `'$symbol '` | 导入映射 | 只显示符号；其余文字导入时记录 |
| `success_symbol` | `'[❯](bold green)'` | 原生 | `symbol` + `success_style` |
| `error_symbol` | `'[❯](bold red)'` | 原生 | `error_symbol` + `error_style` |
| `vimcmd_symbol` | `'[❮](bold green)'` | 需 Shell 字段 | 需要 Shell 发送当前 keymap（vicmd）；导入时记为不支持 |
| `vimcmd_replace_one_symbol` | `'[❮](bold purple)'` | 需 Shell 字段 | 同上 |
| `vimcmd_replace_symbol` | `'[❮](bold purple)'` | 需 Shell 字段 | 同上 |
| `vimcmd_visual_symbol` | `'[❮](bold yellow)'` | 需 Shell 字段 | 同上 |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

### `fill` → tildy `fill`

| 选项 | Starship 默认 | 状态 | tildy / 说明 |
| --- | --- | --- | --- |
| `symbol` | `'.'` | 原生 | `symbol`（导入时为 Nerd 字形补 unicode/ascii 回退） |
| `style` | `'bold black'` | 原生 | `style` |
| `disabled` | `false` | 导入映射 | `disabled = true` 的模块导入时跳过（列入 `skipped`） |

## 桥接模块

以下模块没有原生实现，导入时生成 `{"type":"starship","module":NAME}`（部分带 Starship 的检测列表），它们的选项全部由 Starship 自己读取。选项名逐个列出以便核对。

| 模块 | 选项数 | 选项 | 说明 |
| --- | --- | --- | --- |
| `aws` | 8 | `format`, `symbol`, `region_aliases`, `profile_aliases`, `style`, `expiration_symbol`, `disabled`, `force_display` | 桥接 |
| `azure` | 5 | `format`, `symbol`, `style`, `disabled`, `subscription_aliases` | 桥接 |
| `battery` | 8 | `full_symbol`, `charging_symbol`, `discharging_symbol`, `unknown_symbol`, `empty_symbol`, `format`, `display`, `disabled` | 桥接 |
| `buf` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `bun` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `cmake` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `cobol` | 8 | `symbol`, `format`, `version_format`, `style`, `detect_extensions`, `detect_files`, `detect_folders`, `disabled` | 桥接 |
| `container` | 4 | `symbol`, `style`, `format`, `disabled` | 桥接 |
| `cpp` | 9 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `commands`, `style`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `crystal` | 8 | `symbol`, `format`, `version_format`, `style`, `detect_extensions`, `detect_files`, `detect_folders`, `disabled` | 桥接 |
| `daml` | 8 | `format`, `version_format`, `symbol`, `style`, `detect_extensions`, `detect_files`, `detect_folders`, `disabled` | 桥接 |
| `dart` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `deno` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `dotnet` | 9 | `format`, `version_format`, `symbol`, `heuristic`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `elixir` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `elm` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `env_var` | 6 | `symbol`, `variable`, `default`, `format`, `description`, `disabled` | 桥接（`[env_var.NAME]` 表不导入；tildy 原生 `env_var` 只能读 Shell 转发的变量） |
| `erlang` | 8 | `format`, `version_format`, `symbol`, `style`, `detect_extensions`, `detect_files`, `detect_folders`, `disabled` | 桥接 |
| `fennel` | 8 | `format`, `version_format`, `symbol`, `style`, `detect_extensions`, `detect_files`, `detect_folders`, `disabled` | 桥接 |
| `fortran` | 9 | `symbol`, `format`, `version_format`, `style`, `detect_extensions`, `detect_files`, `detect_folders`, `commands`, `disabled` | 桥接 |
| `fossil_branch` | 6 | `format`, `symbol`, `style`, `truncation_length`, `truncation_symbol`, `disabled` | 桥接 |
| `fossil_metrics` | 5 | `format`, `added_style`, `deleted_style`, `only_nonzero_diffs`, `disabled` | 桥接 |
| `gcloud` | 7 | `format`, `symbol`, `region_aliases`, `project_aliases`, `detect_env_vars`, `style`, `disabled` | 桥接 |
| `gleam` | 7 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `style`, `disabled` | 桥接 |
| `gradle` | 9 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled`, `recursive` | 桥接 |
| `guix_shell` | 4 | `format`, `symbol`, `style`, `disabled` | 桥接 |
| `haskell` | 7 | `format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `haxe` | 8 | `format`, `version_format`, `detect_extensions`, `detect_files`, `detect_folders`, `symbol`, `style`, `disabled` | 桥接 |
| `helm` | 8 | `format`, `version_format`, `detect_extensions`, `detect_files`, `detect_folders`, `symbol`, `style`, `disabled` | 桥接 |
| `hg_branch` | 6 | `symbol`, `style`, `format`, `truncation_length`, `truncation_symbol`, `disabled` | 桥接 |
| `hg_state` | 11 | `merge`, `rebase`, `update`, `bisect`, `shelve`, `graft`, `transplant`, `histedit`, `style`, `format`, `disabled` | 桥接 |
| `julia` | 8 | `format`, `version_format`, `detect_extensions`, `detect_files`, `detect_folders`, `symbol`, `style`, `disabled` | 桥接 |
| `kotlin` | 9 | `format`, `version_format`, `detect_extensions`, `detect_files`, `detect_folders`, `symbol`, `style`, `kotlin_binary`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `kubernetes` | 11 | `symbol`, `format`, `style`, `context_aliases`, `user_aliases`, `detect_extensions`, `detect_files`, `detect_folders`, `detect_env_vars`, `contexts`, `disabled` | 桥接 |
| `lua` | 9 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `lua_binary`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `meson` | 6 | `truncation_length`, `truncation_symbol`, `format`, `symbol`, `style`, `disabled` | 桥接 |
| `mise` | 9 | `symbol`, `style`, `format`, `detect_extensions`, `detect_files`, `detect_folders`, `healthy_symbol`, `unhealthy_symbol`, `disabled` | 桥接 |
| `mojo` | 7 | `format`, `symbol`, `style`, `disabled`, `detect_extensions`, `detect_files`, `detect_folders` | 桥接 |
| `nats` | 4 | `symbol`, `style`, `format`, `disabled` | 桥接 |
| `netns` | 4 | `format`, `symbol`, `style`, `disabled` | 桥接 |
| `nim` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `ocaml` | 10 | `format`, `version_format`, `symbol`, `global_switch_indicator`, `local_switch_indicator`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `odin` | 8 | `format`, `show_commit`, `symbol`, `style`, `disabled`, `detect_extensions`, `detect_files`, `detect_folders` | 桥接 |
| `opa` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `openstack` | 4 | `format`, `symbol`, `style`, `disabled` | 桥接 |
| `perl` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `php` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `pijul_channel` | 6 | `symbol`, `style`, `format`, `truncation_length`, `truncation_symbol`, `disabled` | 桥接 |
| `pixi` | 10 | `format`, `version_format`, `symbol`, `style`, `show_default_environment`, `pixi_binary`, `detect_extensions`, `detect_files`, `detect_folders`, `disabled` | 桥接 |
| `pulumi` | 6 | `format`, `version_format`, `symbol`, `style`, `search_upwards`, `disabled` | 桥接 |
| `purescript` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `quarto` | 8 | `format`, `version_format`, `symbol`, `style`, `detect_extensions`, `detect_files`, `detect_folders`, `disabled` | 桥接 |
| `raku` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `red` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `rlang` | 8 | `format`, `version_format`, `symbol`, `style`, `detect_extensions`, `detect_files`, `detect_folders`, `disabled` | 桥接 |
| `ruby` | 9 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `detect_variables`, `style`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `scala` | 8 | `format`, `version_format`, `detect_extensions`, `detect_files`, `detect_folders`, `symbol`, `style`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `singularity` | 4 | `format`, `symbol`, `style`, `disabled` | 桥接 |
| `solidity` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `spack` | 5 | `truncation_length`, `symbol`, `style`, `format`, `disabled` | 桥接 |
| `swift` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |
| `terraform` | 9 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled`, `commands` | 桥接 |
| `typst` | 8 | `format`, `version_format`, `symbol`, `style`, `detect_extensions`, `detect_files`, `detect_folders`, `disabled` | 桥接 |
| `vagrant` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `vcsh` | 4 | `symbol`, `style`, `format`, `disabled` | 桥接 |
| `vlang` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `xmake` | 8 | `format`, `version_format`, `symbol`, `detect_extensions`, `detect_files`, `detect_folders`, `style`, `disabled` | 桥接 |
| `zig` | 8 | `format`, `version_format`, `symbol`, `style`, `disabled`, `detect_extensions`, `detect_files`, `detect_folders` | 桥接；内置预设（commands 模式）改为 `command` 模块：format、symbol、style、version_format、detect_* 映射 |

## custom 命令

`[custom.NAME]` 的 16 个选项（`command`、`when`、`require_repo`、`shell`、`description`、`unsafe_no_escape`、`detect_files`、`detect_folders`、`detect_extensions`、`symbol`、`style`、`format`、`disabled`、`os`、`use_stdin`、`ignore_timeout`）全部记为不支持：导入器不转换 custom 命令（格式里的 `$custom` 记入 `unsupported`）。同类需求可以手写 `type: command` 模块：`argv`（直接执行，不经 Shell，对应 `command`/`shell`）、`detect_files`/`detect_extensions`/`detect_folders`、`regex`、`timeout_ms`、`cache_ttl_s`、`symbol`、`style`、`format`；`when`、`require_repo`（可用 `show_when: git.in_repo`）、`os`、`use_stdin`、`unsafe_no_escape`（输出总是转义）、`ignore_timeout` 没有对应项。

