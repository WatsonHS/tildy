# ADR 0013：终端标记与重画职责

- 日期：2026-10-23。
- 状态：已接受；已实现并完成 M4 多终端验收，随 0.4.0 发布。

## 发现

真实 Ghostty 1.3.1 / cmux 0.65.0 中，Tildy 的 `_tildy_preexec` 每次把摘要钩子移动到 `precmd_functions` 末尾。Ghostty 也要求自己的钩子在末尾，否则只直接输出 A，并尝试下轮再移动。两者持续竞争，异步重画丢失完整标记。此前 PTY 假钩子无条件装饰 PS1，没有模拟这个约束。

iTerm2 的集成脚本可能先于 Tildy 注册。它先装饰 PS1，随后 Tildy 设置基础提示符又覆盖标记。Terminal.app 则在用户按 Return 时建立自己的原生标记；Tildy 的压缩重画擦除该行，也删除标记。

Ghostty 会自行清除、重定位已标记的提示符；Tildy 再按普通终端重排行数上移，会擦掉上一条命令输出。

## 决定

- 摘要钩子保留注册位置，不争抢最后一个 precmd 槽位。摘要只打印一次、位于新提示符 A 之前；D 的位置由终端集成决定，可能在摘要前或后。**撤销 M3 对所有集成都保证 D < 摘要 < A 的假设**。不伪造 D、不包装或修改终端的函数。
- 基础提示符和缓存首帧也保留已经存在的 OSC 133 包装；用户基础提示符的 `prompt_subst` / `prompt_bang` 语义不变。
- `TERM_PROGRAM=ghostty` 或 `TERM=xterm-ghostty`，且 `_ghostty_precmd` 已加载时，终端负责重画起点；Tildy 仍做宽度截断、请求防抖和异步更新，省去额外的向上光标补偿。其他终端沿用已有重排补偿。
- `TERM_PROGRAM=Apple_Terminal` 时保留已接受的完整提示符，跳过 Enter/Ctrl-C 的压缩重画，即使配置为 compact。配置、协议与摘要设置不变；debug log 记录 `transient_skipped ... terminal_native_marks`。这是原生跳转的兼容回退，不声称 Terminal.app 支持 OSC 133。

## 证据

四个真实 GUI 终端的版本、校准、窗口缩放、字体缩放、历史跳转、instant 和预敲输入见 [历史 GUI 验证范围](../support.md)。新 PTY 用例覆盖最后钩子竞争、提前安装的包装、Ghostty 双重位移和 Terminal 原生标记回退；PTY 只证明 Shell 行为，不能代替 GUI 跳转验收。

## 后果

终端集成可以稳定保留标记，Ghostty/cmux 缩放不再额外移动光标。代价是 Terminal.app 的历史保留完整提示符，且摘要不能跨所有集成都保证在完成 D 之后。

本轮测试使用独立配置、缓存与状态目录，仅缩放测试会话的字体大小，没有修改用户 `.zshrc` 或安装字体。字体覆盖与字符列宽是不同问题：校准不能让缺失的 Nerd 字形出现。终端或字体变更后需要重新校准和验收。
