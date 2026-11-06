# 显式终端列宽校准

Tildy 只在用户运行 `tildy calibrate` 时查询真实终端。启动、instant、precmd、ZLE、worker、oneshot 和 preview 不发送 CPR 查询。没有匹配的测量结果时，使用 `unicode-width` 的完整字符串宽度。

## 使用与支持边界

先给当前终端字体配置指定一个显式修订标识；字体或终端配置改变后应更新标识，再重新校准。这个标识由用户指定，Tildy 不读取或推断字体名称。

```zsh
export TILDY_FONT_REVISION=ghostty-local-font-v1
tildy calibrate --font-revision "$TILDY_FONT_REVISION" \
  --cluster '中' --cluster 'é' --cluster '👩‍💻' --cluster '☀️' --cluster '' --cluster '%'
```

校准命令需要一个属于当前前台进程组的 controlling TTY。JSON 输出可以重定向，探测在 `/dev/tty` 上进行。请在探测期间不要输入；任意普通输入会取消本次校准，取消不写 profile，也不会将未知字节重放成 shell 命令。

只对单个、精确的 Unicode 字素簇记录单元格 advance；一个 emoji 的结果不推断给其他 emoji，一个私有区字形的结果也不推断给整个私有区。CPR 测的是终端光标移动的单元格数，不是字体绘制范围或视觉溢出。单字节可打印 ASCII 必须为一列，非 ASCII 精确簇允许 0–8 列。

## Profile 与匹配

保存路径为 `$XDG_CACHE_HOME/tildy/width/profile.json`，没有绝对的 XDG cache 路径时使用 `$HOME/.cache/tildy/width/profile.json`。这是数据文件，不会被 source 或 eval。文件采用版本 1，最多 64 KiB、256 条记录；每个样本必须是一个字素簇，最多 128 UTF-8 字节，禁止控制字符和双向文本覆盖字符，禁止重复键。写入使用独占临时文件、0600 权限、同步和原子 rename；失败保留之前的文件。

匹配条件全部相等：`TERM`、`TERM_PROGRAM`、`TERM_PROGRAM_VERSION`、有效 locale（`LC_ALL`、`LC_CTYPE`、`LANG` 依次取第一个非空值）、显式 `TILDY_FONT_REVISION`。标识未设置、为空、身份不符、版本不支持、文件损坏或文件过大时，安全回退到默认度量。没有显式字体标识时，渲染不读取 profile。启用标识时，每次渲染入口最多检查一次文件身份，并复用已解析结果；模块内的度量不访问磁盘。

一轮渲染使用固定的 `WidthPolicy`。作用域退出或 panic 时恢复之前的策略，不把校准状态泄漏到下次渲染。真实 worker、oneshot、preview、模块报告、左右碰撞、分支/目录/Agent 截断和 fill 使用同一策略。截断和 fill 以字素簇为单位，不拆开 ZWJ 或组合字符。

渲染器把测量过的簇写成 `%{字簇%NG%}`；`%` 本身仍写为 `%%`。Rust 的 `visible_width` 和 Zsh 的 `_tildy_width` 读取同一列数声明，preview 去掉声明但保留字簇。原始 ANSI SGR 的零宽组继续按零宽处理。[Zsh 的 `%G` 定义](https://zsh.sourceforge.io/Doc/Release/Prompt-Expansion.html#Visual-effects)说明该声明仅改变提示符宽度记账。

协议 v1 的每条提示符行仍有 4000 UTF-8 字节上限。超长输出会在完整字素簇/提示符 escape/`%{…%G%}` 边界截断，不留下未闭合的零宽组。模块报告描述截断前的渲染；超过协议行上限的自定义内容应另行缩短，不能以报告列数断言实际传输行仍包含全部内容。

只要 `TILDY_FONT_REVISION` 非空，就禁用旧 instant 缓存的读取和写入，即使 profile 尚未匹配。这样更新校准结果也不会复用另一个度量策略生成的 instant 提示符。标识应在 `.zshrc` 顶部 instant snippet 之前设置；此实现不修改用户的启动文件。

## TTY 恢复与已知限制

探测在独立的显式前台命令中完成，保存并恢复 termios，使用备用屏幕，结束时恢复原屏幕和光标。CPR 在远离常见功能键响应的固定坐标进行，重复测量必须一致。混入输入、异常/越界响应、超时或中断取消本次写入，并进行有限清理。

[Xterm 的 CPR 文档](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)指出响应可以与修饰 F3 的序列同形。因此本实现不声称可以识别任意输入或绝对隔离无限延迟的终端响应。终端、代理层和复用器必须通过真实验收后使用；超时后的有限清理只能覆盖有界延迟。

校准影响 Tildy 生成的提示符布局；它不改变终端字体、其他程序，也不重写 ZLE 的用户输入缓冲区。用户输入内容的编辑仍由 Zsh 自身处理。

## 验证

`tests/terminal_width.rs` 覆盖精确 profile、身份不匹配回退、preview、真实布局与报告、左右碰撞、字素簇截断/fill、fallback prompt character 和 Zsh `%G` 度量。CLI 的模拟终端测试验证成功、失败、延迟、输入交织与 termios 恢复。真实终端记录通过 `scripts/replay-terminal-width.py` 在隔离 config/cache/state（XDG 路径）下生成；HOME 仍使用当前用户值；字体 revision 使用显式 label，并记录终端版本。历史 GUI 验证范围见 [支持矩阵](support.md)，不能用 PTY 模型代替。
