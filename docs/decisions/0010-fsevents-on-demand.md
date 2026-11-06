# ADR 0010：FSEvents 与 CoreFoundation 按需 dlopen，不再链接

- 日期：2026-10-22
- 状态：已接受（取代 M3 报告"第二轮资源优化"中"直接链接子框架、拒绝 dlopen"的结论）

## 背景

M3 的 macOS 监听后端直接链接 CoreFoundation 与 FSEvents 子框架。M3 报告里对比过 dlopen：watch 关闭时 RSS 更低，watch 开启时 RSS 更高，于是保留了直接链接。当时只看 worker 的常驻内存。

M4 启动项量到了另一面：链接这两个框架后，**每一个** tildy 进程启动都要由 dyld 加载约 700 个动态库（M2 二进制为 83 个），`tildy --version` 的进程启动中位数 22 ms，M2 为 9 ms。而一次 shell 启动要起两个 tildy 进程（`init zsh --instant` 与 `init zsh`），一次性 `render`、`configure`、`explain` 等每次调用也都在付这笔钱；真正用到 FSEvents 的只有 worker 里的监听线程。

## 决定

`src/session/watch/macos.rs` 不再 `#[link]` 任何框架。第一次启动监听时用 `dlopen`（`RTLD_LAZY | RTLD_LOCAL`，按绝对路径打开 CoreFoundation 与 FSEvents 子框架）和 `dlsym` 解析 11 个入口，缓存在 `OnceLock` 里，句柄永不关闭。任一符号缺失即视为"FSEvents 不可用"，走既有的轮询兜底。`build.rs` 删除。

RSS 不再作为指标（M3 收尾时作者已改定，见 ROADMAP 决策表）；本决定按 physical footprint 与进程启动时间判断。

## 证据（作者机器，2026-10-22）

| | 链接 | dlopen |
| --- | ---: | ---: |
| `tildy --version` 启动中位数 | 21.9 ms | 6.5 ms |
| 启动时加载的动态库 | 703 | 83 |
| `.zshrc` 里 instant 段 | 19–22 ms | 15–16 ms |
| `.zshrc` 里 init 段（含脚本缓存） | 26.5 ms | 21–22 ms |
| 单 worker watch 开启 footprint | 4,929 KiB | 5,009 KiB |
| 单 worker watch 开启 RSS | 11,408 KiB | 14,896 KiB |

footprint 差 80 KiB 在噪声内；RSS 的增加是 dlopen 映射方式不同造成的共享页计数，与 M3 报告的观察一致，不计入。

## 后果

- 不用监听的进程完全不碰框架；worker 的监听线程第一次启动多付一次 dlopen（约 10 ms，在管理线程上，不阻塞 worker）。
- 第三方不再能从 `otool -L` 看出 tildy 依赖 FSEvents；文档以本 ADR 为准。
- Linux 的 `notify` 后端不受影响。
