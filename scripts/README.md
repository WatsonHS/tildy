# scripts

`scripts/dev-shell.sh` 启动一个隔离的交互式 zsh，用于试用开发构建的 tildy。它在 `$TMPDIR` 下创建临时 `ZDOTDIR`，其中的 `.zshrc` 只设置普通 `PS1`、调试日志路径（`TILDY_DEBUG_LOG`），然后用 `target/debug/tildy`（加 `--release` 则用 release 版本）执行 `tildy init zsh`，不读取也不修改 `~/.zshrc` 或 `$HOME` 下的任何文件。`--plugins` 加载 oh-my-zsh 自定义目录里的 autosuggestions、syntax-highlighting 与 fzf 键绑定；可用 `TILDY_TEST_PLUGIN_DIR` 指定插件目录。fzf 集成与 PTY 测试共用 `scripts/fzf.zsh`，兼容新版 `fzf --zsh` 和 Ubuntu 24.04 的发行版脚本。`--config PATH` 设置 `TILDY_CONFIG`；`--fake` 不使用二进制，改为设置 `TILDY_BACKEND` 指向 `bench/fake-backend.sh` 并直接 source `shell/zsh/tildy.zsh`；`--print-zshrc` 只把生成的 `.zshrc` 打印出来后退出，便于检查。临时目录在退出后保留，调试日志与历史都在其中。

`scripts/package-linux.sh [输出目录]` 只构建 GNU/Linux ARM64 与 x86_64 两个压缩包。在 macOS 上使用 [Linux Dockerfile](../tests/linux/Dockerfile) 和独立的 Linux 文件系统；Linux 上需预装两套 Rust target 与 GCC 交叉链接器。可用 `TILDY_LINUX_BUILDER_IMAGE` 指定已构建的镜像。容器默认 Rust 1.95.0 / Ubuntu 24.04，旧版发行版兼容性需要另测。

`scripts/release.sh --build-only` 在 macOS 上生成四个平台的包（含 LICENSE、NOTICE、上游和依赖许可文本）及本地 `tildy.rb`，不访问 GitHub、不修改 tap。省略该参数才执行发布：要求源仓库与 tap 均干净，版本尚未发布；先将四个资产上传到草稿 Release，全部成功后公开，再复制含 `on_macos` / `on_linux` 的 formula 并推送。已有 Release 不覆盖，发布修订必须递增版本。formula 在 Linux 检查系统 glibc ≥ 2.39。

`scripts/collect-licenses.py OUTPUT` 收集四个发布目标的生产依赖许可，输出包名、版本、声明许可和原始许可文本，不写本机 registry 路径。

`scripts/export-source.py --check` 检查当前源文件集；`scripts/export-source.py OUTPUT` 向仓库外的新目录导出无 Git 历史的源码与 SHA-256 清单。检查是启发式规则，不代表完整安全审计。

`scripts/package-archive.py OUTPUT BINARY NOTICES` 统一创建发布归档：保留二进制和许可文本，归零所有者与时间戳，不携带本机用户名或文件扩展属性；相同输入产生相同压缩包。
