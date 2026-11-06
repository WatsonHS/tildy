# Agent 状态：`tildy agent` 与 `agent_status` 模块

让 Agent（Claude Code、Codex、Gemini CLI、aider……）或任何程序把"正在做什么、是否在等你"写进一条状态记录，Tildy 把它显示在**正确的提示符**里：同一个 Shell 会话、同一个 git worktree 的所有 pane，或全局。词汇采用 OSC 7501（Program Status Protocol rev 0.4，<https://www.superlogical.com/rex/docs/build/program-status>），记录字段与 7501 报告一一对应。设计理由见 `docs/decisions/0008-agent-status.md`。

## 可选功能（默认关闭，2026-10-20 作者决定）

作者否决了默认显示："运行 Agent 的时候也看不到终端提示"。内置配置和全部预设都不放 `agent_status` 模块，`agent.wrap` 默认 false，worker 在没有放置该模块时不读状态目录。启用是两步配置：把 `{"type": "agent_status"}` 模块放进布局某一行的 `right`，并设 `"agent": {"wrap": true}` 让 Zsh 适配层为已知 Agent 程序套上 `tildy agent wrap`（任务 D 的 pty 代理，见 `docs/agent-wrap.md`）。启用后在一个 pane 里运行 `claude`，同一 worktree 的其他 pane 的提示符会显示：

```
~/code/tildy main ❯                          ⏳ claude Bash: rm -rf target +1
```

工作方式：

- `tildy init zsh` 读取生效配置，把 `agent.programs` 写进脚本（`_tildy_agent_programs`）。默认列表：`claude codex gemini aider opencode amp goose copilot cursor-agent`。
- 适配层加载时，为列表中**存在于 PATH**、且**没有被用户定义为函数、别名、内建或保留字**的程序各定义一个同名函数：
  `claude() { _tildy_agent_run claude "$@" }`。函数只在 stdin 和 stdout 都是终端、`TILDY_BIN` 可执行、`TILDY_AGENT_WRAP` 不是 `0` 时运行 `"$TILDY_BIN" agent wrap --app claude -- claude "$@"`，否则 `command claude "$@"`。退出码就是程序自己的退出码；管道、重定向（`claude -p … | jq`）不经过代理。
- wrap 启动时发布 `working`，转发程序自己发出的 OSC 7501 报告，退出时若程序没报 done/error 就按退出码合成；记录的 id 是 `wrap/<app>/<pid>`，`origin_session` 是当前 Shell。
- 被包装的程序列在 `$_tildy_agent_wrapped`，调试日志（`TILDY_DEBUG_LOG`）有一条 `agent_wrap claude,codex`。

关闭或调整（"可退出的魔法"）：

| 想要 | 做法 |
| --- | --- |
| 本次不经过代理 | `TILDY_AGENT_WRAP=0 claude` 或 `command claude` |
| 当前 Shell 起全部关闭 | 在加载 tildy 之前 `export TILDY_AGENT_WRAP=0` |
| 永久关闭 | 配置 `{"agent": {"wrap": false}}` |
| 只包装某几个 | 配置 `{"agent": {"programs": ["claude", "codex"]}}`，或环境变量 `TILDY_AGENT_PROGRAMS="claude codex"`（空格或逗号分隔，加载时读取） |
| 去掉一个已定义的包装 | `unfunction claude` |
| 自己的同名别名或函数 | 照常生效：已存在的函数、别名不会被覆盖 |

## 记录

格式：`schema/agent-state.schema.json`（v1）。实现：`src/agent/record.rs`。

| 字段 | 取值 | 说明 |
| --- | --- | --- |
| `protocol_version` | `1` | |
| `id` | `[A-Za-z0-9_.+-]{1,32}` 段以 `/` 连接，≤8 层、≤128 字节 | 缺省为根记录；`/` 表示父子关系（清除子树、继承 `app`） |
| `app` | `[A-Za-z0-9_.+-]{1,32}` | 稳定的程序标识；缺省继承最近祖先记录的 `app` |
| `state` | `idle` `working` `done` `blocked` `error` | `clear` 是操作，不是状态 |
| `kind` | `permission` `question` `auth` | 只用于 `blocked`，其他状态下发布时丢弃 |
| `progress` | 0–100 | 只用于 `working`/`blocked`，其他状态下丢弃 |
| `title` | ≤192 字节 UTF-8 | 明文；无控制字符（U+0000–001F、U+007F–009F，含换行、制表符） |
| `msg` | ≤2048 字节 UTF-8 | 单行，规则同上 |
| `scope` | `{kind: session\|worktree\|global, id}` | `session` 的 id 是 `TILDY_SESSION_ID`；`worktree` 的 id 等于 `GitData.worktree_id`（`<canonical common dir>@<canonical toplevel>`）；`global` 的 id 是 `*` |
| `origin_session` | 字符串 | 发布者所在 Shell 的 `TILDY_SESSION_ID` |
| `source` | `osc` `cli` | 采集通道 |
| `seq` | ≥1 | 同 scope 同 id 只接受比现存未过期记录更大的 seq；缺省为上一条 +1 |
| `ttl_s` | 1–86400 | 缺省：idle/working/blocked 7200，done/error 900 |
| `received_at_ms` | Unix 毫秒 | 存储时写入的墙钟 |
| `prompt_seen` | 布尔 | 内部：done/error 已在 origin 会话的一个提示符上显示过 |

校验按 7501 的硬上限：超限、控制字符、id 语法错误、未知字段整条拒绝，什么都不写入。每次发布**整体替换**同 scope 同 id 的记录：没写的键就没有了。

## 生命周期

- **TTL**：`received_at_ms + ttl_s` 之后不再显示（墙钟；发布者退出后照样过期）。
- **7501 的提示符规则**（对应 OSC 133 `A`）：`origin_session` 的 Shell 出现新提示符时，worker 调用 `drop_on_prompt`：
  - `idle`、`working`、`blocked`：丢弃；
  - `done`、`error`：第一次只标记 `prompt_seen`（这个提示符照常显示），第二次丢弃——"至少显示一次"。
  - 没有 `origin_session` 的记录（`--no-origin`，或在 Tildy 之外发布）只靠 TTL 或清除离开。
- **清除**：带 `id` 删除该记录及其子记录；不带 `id` 删除该 scope 下该 `app` 的全部记录（都不给则删除该 scope 全部）。

## 存储

`$XDG_STATE_HOME/tildy/agent/<kind>/<scope-hash>/<id-hash>.json`（默认 `~/.local/state/tildy/agent/…`），两个 hash 都是 FNV-1a 64 的十六进制。目录 0700、文件 0600；写入为临时文件 + rename，读者从不加锁；写者（publish、clear、drop、expire）对 scope 目录 `flock`。每个 scope 最多 256 条，满了删最久未更新的。目录的 mtime 在发布、替换、删除时变化，worker 靠它发现更新。

库接口（`tildy::agent`，供 CLI、provider、worker 与 `tildy agent wrap` 共用）：

```rust
let store = Store::open()?;                       // 或 Store::open_with(state_dir)
let stored: Stored = store.publish(record)?;      // Err(Reject::{Invalid, StaleSeq, Io})
let live: Vec<Stored> = store.list(&Filter::live(vec![Scope::global()]));
let removed = store.clear(&Filter { scopes, id: Some("wrap/claude/42".into()), ..Filter::default() })?;
store.drop_on_prompt(session_id);                 // 7501 提示符规则
store.expire(now_ms);                             // 删除过期记录与空目录
store.next_expiry();                              // 最早的过期时刻（Unix 毫秒）
store.scope_dir(&scope);                          // 该 scope 的目录（worker 监视）
let report = parse_osc_body("state=blocked:kind=permission:msg=…")?; // 7501 报文体
```

## CLI

所有子命令支持 `--json`（标准输出只有 `CliEnvelope`）。退出码：0 成功；2 输入无效（`error.path` 指向字段，如 `msg`、`osc.state`、`scope`）；3 `seq` 不比现存记录新（`revision_conflict`，`error.path` 为 `seq`）；4 没有状态目录、不在 worktree 里、没有 `TILDY_SESSION_ID`、存储不可写。

### `tildy agent publish`

```
tildy agent publish --state S [--id ID] [--app APP] [--kind K] [--progress N] [--title T] [--msg M]
                    [--scope session|worktree|global] [--scope-id ID] [--cwd DIR]
                    [--seq N] [--ttl S] [--origin SESSION | --no-origin] [--json]
tildy agent publish --file record.json|-  [--seq N] [--ttl S] [--origin SESSION | --no-origin]   # scope 取自文件
tildy agent publish --osc 'state=blocked:kind=permission:msg=<base64>' [--id ID] [--app APP] [scope 选项]
```

- 无 `--scope`：`--cwd`（默认当前目录）在 git worktree 内用 worktree，否则 global。`--scope session` 用 `--scope-id` 或 `$TILDY_SESSION_ID`。
- `origin_session` 取 `--origin`，否则 `$TILDY_SESSION_ID`；`--no-origin` 不写。
- `--state clear`（或报文 `state=clear`）执行清除：带 `--id` 删该记录及子记录，否则删该 scope 下 `--app` 的全部记录。
- `--osc` 按 7501 的宽松规则：去掉键值两侧空白；没有 `=`、键不是小写字母、值含 `[A-Za-z0-9_.,+/=-]` 以外字节的对跳过；未知键忽略；重复键取最后一个；`kind`、`progress`、`app` 无效视为没有；缺 `state`、未知 `state`、`id` 语法错误、`title`/`msg` 的 base64 解不开或超限、报文超过 4096 字节则整条拒绝。报文自带的 `id`、`app` 优先于 `--id`、`--app`。`source` 记为 `osc`。

### `tildy agent list`

```
tildy agent list [--scope … [--scope-id ID] [--cwd DIR] | --all] [--expired] [--json]
```

默认列出当前 Shell 的提示符会显示的记录：`$TILDY_SESSION_ID` 的 session、当前目录的 worktree、global，未过期的。`--all` 列出所有 scope；`--expired` 包含已过期未删除的。顺序与提示符相同：blocked > error > done > working > idle，同状态新的在前。`--json` 的 `data.records[]` 是记录字段加 `path`、`expires_at_ms`、`age_s`、`expired`、`inherited_app`（继承来的 app）。

### `tildy agent clear`

```
tildy agent clear [--id ID] [--app APP] [--scope … | --all] [--expired] [--json]
```

默认范围同 `list`。`--id` 删除该记录及子记录，`--app` 只删该 app 的记录，`--expired` 只删已过期的；单独 `--expired` 清理所有 scope 的过期记录、损坏文件与空目录。

## `agent_status` 模块

```json
{ "modules": { "agent": { "type": "agent_status", "max_width": 40,
    "state_symbols": { "blocked": { "nerd": "\uf059", "unicode": "⏳", "ascii": "?" } },
    "rules": [ { "id": "perm", "when": { "field": "agent.kind", "op": "eq", "value": "permission" },
                 "style": { "foreground": "magenta", "bold": true } } ] } } }
```

- provider（`src/providers/agent.rs`）：每个 `agent_status` 模块一个实例，key 为模块 id，deps `Cwd` + `Env`，ttl 1 s；读取 session（请求的 session id）、worktree（cwd 的 worktree 身份，按 cwd 缓存 10 s；cwd 的祖先目录里没有 `.git` 时不启动 git）与 global 三个 scope 的未过期记录。没有记录时隐藏（`not_detected`）。
- 字段（`format` 变量；`show_when`/`rules` 里写作 `agent.<字段>`）：`count`，各状态计数 `blocked` `error` `done` `working` `idle`，突出记录的 `state` `kind` `app` `title` `msg` `progress` `id` `age_s` `scope`，`more`（其余记录数，为 0 时 `$more` 为空）。计数、`progress`、`age_s` 在规则里是数字。
- 默认 `format`：`$symbol( $app)( $msg)( +$more)`。`$msg` 截到 `max_width` 列（默认 40，按显示宽度，超出时以 `…`/`...` 结尾）。
- `$symbol`：`state_symbols[state]`，否则模块的 `symbol`，否则内置：

| state | nerd | unicode | ascii | 默认颜色 |
| --- | --- | --- | --- | --- |
| working | `U+F051F`（md-timer_sand） | `◐` | `~` | cyan |
| blocked | `U+F059`（fa-question_circle） | `⏳` | `?` | yellow |
| error | `U+F057`（fa-times_circle） | `✘` | `x` | red |
| done | `U+F058`（fa-check_circle） | `✔` | `ok` | green |
| idle | `U+F04B2`（md-sleep） | `·` | `-` | grey |

  默认颜色叠在模块 `style` 之下（模块显式设置的颜色优先），`rules` 再叠在最上面。Zsh 提示符不支持"暗"属性（见 `render::attrs`），idle 用灰色代替。
- 内置配置的 `agent` 模块在第一行右侧（`right: ["agent", "elapsed", "result"]`）；预设不变，需要的话在用户配置里把 `agent` 加进布局。

## worker 推送

- 每个 REQ：先发 partial，再执行 `drop_on_prompt(req.session_id)`（不论配置里有没有 `agent_status` 模块），记录监视基线，然后启动 provider 抓取。
- 有 `agent_status` provider 时，worker 每 1 s `stat` 当前请求的三个 scope 目录，并记住其中最早的过期时刻。目录 mtime 变化或有记录到期，且算出的模块数据（忽略 `age_s`）与当前显示的不同时，worker 把当前请求重新提交给调度器，等所有槽结束后对**同一代次**再发一帧 `fresh` RES（协议允许同代次多帧；与上一帧相同则不发）。实测：发布后约 0.5 s 出现、TTL 到期后约 0.5 s 消失。
- worker 每小时在后台执行一次 `expire`（删除过期记录与空目录）。

`tildy capabilities --json` 的 `data.agent` 列出词汇（`states`、`kinds`、`scopes`）与生效配置的 `wrap`、`programs`，供 Agent 发现。

## 示例

```sh
# 某个脚本在跑长任务：所有 pane（global）显示
tildy agent publish --scope global --state working --app make --msg "building release" --no-origin
tildy agent publish --scope global --state done    --app make --msg "release ready"     --no-origin

# 原样转发一条 7501 报告
tildy agent publish --osc "state=blocked:kind=question:app=deploy:msg=$(printf 'Proceed?' | base64)"

# 查看与清理
tildy agent list --all --json | jq '.data.records[] | {state, app, msg}'
tildy agent clear --app make
```

## 测试

- `cargo test agent`：记录校验与 7501 报文（`src/agent/record.rs`）、存储（`src/agent/store.rs`：布局与权限、seq、过滤与顺序、清除、提示符规则、过期、上限、并发发布）、provider、渲染（`src/render/agent.rs`）、worker 推送（`src/session/worker.rs`）。
- `tests/agent_cli.rs`：CLI 的成功与失败路径、scope 解析、`--osc`、`--file`、TTL、`tildy render` 端到端。
- `tests/pty/run.py --scenario 'agent_*'`：`agent_status_push`（发布 1.5 s 内出现、输入不受影响、TTL 后 1.5 s 内消失）、`agent_status_scopes`（另一 worktree、另一会话不显示）、`agent_status_prompt_rule`（working 下一个提示符消失，done 显示一次后消失）、`agent_wrap_known_program`（零配置包装、记录进入存储、退出码保留、用户别名优先、`TILDY_AGENT_WRAP=0`）。
- `python3 fixtures/validate.py`：`fixtures/agent-state.*.json` 对 Schema，另按字节检查 `title`/`msg` 上限。
