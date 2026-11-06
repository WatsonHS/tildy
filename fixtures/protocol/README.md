# 协议 v1 与快照样本

本目录是 `schema/shell-frame.v1.schema.json` 与 `schema/snapshot.schema.json` 的样本。统一由 `fixtures/validate.py` 校验（已合并原 `fixtures/protocol/validate.py`，后者已删除）：

```sh
python3 fixtures/validate.py
```

## 文件

| 文件 | 内容 |
| --- | --- |
| `shell-frame.v1.valid-*.json` | 逻辑帧（解码后的 JSON 对象）的有效样本：HELLO、REQ（含转义用的 cwd 与 env）、RES partial、RES fresh（含 R、T）、BYE。 |
| `shell-frame.v1.invalid-*.json` | 无效样本，每个都应当被 Schema 拒绝，原因见下表。 |
| `snapshot.valid.json` | 真实快照：用 `tildy worker --snapshot <path>` 启动、发送一个 REQ，在 worker 存活时读取该文件（worker 正常退出时快照会被删除）。 |
| `snapshot.invalid-*.json` | 由 `snapshot.valid.json` 派生的无效样本，原因见下表。 |
| `wire-samples.txt` | 真实的制表符线路样本（HELLO、REQ、同一 gen 的两个 RES、BYE）。`fixtures/validate.py` 按空行分帧、反转义后按 Schema 校验。 |

## 限制与 Rust 代码的对应

| 项 | 限制 | 代码位置 |
| --- | --- | --- |
| REQ 行 | 最长 64 KiB（65536 字节） | `MAX_REQUEST_BYTES` |
| RES 的 L 行 | 最多 12 行，至少 1 行（无内容时发一个空 `L`） | `MAX_LEFT_LINES` |
| 提示符行（L/R/T） | 最长 4000 UTF-8 字节，超出截断；去除控制字符；结尾孤立的 `%` 被丢弃 | `MAX_LINE_BYTES` |
| cols、lines | 十进制数字，u16 范围 0 至 65535 | `parse_unsigned::<u16>` |
| last_exit | 可带前导 `-`，i32 范围 | `parse_exit` |
| cwd、env、session_id | cwd 非空不含 NUL；env 不含 NUL，无长度上限；session_id 非空不含控制字符，无长度上限 | `parse_request_bytes` |

JSON Schema 的 `maxLength` 按字符计数，不能精确表达字节上限；`fixtures/validate.py` 对 RES 的每个提示符行做 UTF-8 字节数检查。

## 无效样本说明

| 文件 | 违反的规则 | 为什么应当失败 |
| --- | --- | --- |
| `shell-frame.v1.invalid-hello-missing-pid.json` | HELLO 必填字段 | 缺少 `pid`。线路上 HELLO 只有 5 个字段，少一个即为坏帧。 |
| `shell-frame.v1.invalid-hello-protocol-zero.json` | `protocol_version` 最小值 1 | 版本 0 不是 v1 的合法值；Shell 据此判定版本不匹配。 |
| `shell-frame.v1.invalid-req-missing-env.json` | REQ 必填字段 | 缺少 `env`。v1 的 REQ 有 9 个字段，`env` 是新增字段，旧格式请求不得被当作 v1。 |
| `shell-frame.v1.invalid-req-cwd-nul.json` | cwd 不允许 NUL | 路径中的 `\u0000` 无法用 `\t`、`\n`、`\\` 的转义规则表示，线路上无法安全传递。 |
| `shell-frame.v1.invalid-res-status-done.json` | `status` 枚举 | v1 只有 `fresh` 与 `partial`；`done` 不是协议值，Shell 无法据此判断结果是否完整。 |
| `shell-frame.v1.invalid-res-empty-left.json` | `left` 至少一行 | 没有 `L` 行的 RES 没有可显示的提示符，也无法确定行数。 |
| `shell-frame.v1.invalid-res-tab-in-left.json` | 提示符行不含控制字符 | 行内制表符会被线路解析成额外字段，破坏 `L` 行结构。 |
| `shell-frame.v1.invalid-res-trailing-percent.json` | 提示符中的 `%` 必须写成 `%%` | 结尾孤立的 `%` 在 Zsh 中是未完成的转义。注意：Schema 只能检查结尾的孤立 `%`，`%b` 之类的未知转义无法判断。 |
| `shell-frame.v1.invalid-bye-extra-field.json` | BYE 不允许额外字段 | 线路上 BYE 单独一行；携带原因字段说明生成方不是 v1 实现。 |
| `shell-frame.v1.invalid-unknown-frame.json` | `frame` 只能是 HELLO、REQ、RES、BYE | 未知标签（此处为 `PING`）不属于 v1。 |
| `snapshot.invalid-freshness.json` | Freshness 枚举 | 只允许 `pending`、`fresh`、`stale`、`timeout`、`error`；`ok` 不是合法取值。 |
| `snapshot.invalid-extra-field.json` | 快照顶层不允许额外字段 | `agent_state` 不在 v1 快照中（Agent 状态属于 M2）。 |

## 线路样本

`wire-samples.txt` 中的分隔符是真实的 TAB 字节，`env` 中的分隔符是真实的 U+001F 字节。若编辑器把 TAB 替换为空格，校验会因字段数不符报解码错误；U+001F 被替换不会被检出（`env` 的内容不受 Schema 约束），编辑时需保留原字节。

- REQ 的 `cwd` 为 `/Users/example/tmp\tdir\\x`：线路上的 `\t` 与 `\\` 是两个字符的转义，解码后分别为制表符与反斜杠。
- 第 3 与第 4 帧是同一 `gen`（7）的两个 RES：先 `partial`（Git 为 `…` 占位），后 `fresh`；每个 RES 各以 `END` 结束。
