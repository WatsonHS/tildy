# Linux + Zsh 回归

在项目根目录构建 Ubuntu 24.04 测试镜像。包括 Rust 1.95.0、Zsh 5.9、三个 PTY 插件、真实 Starship 1.24.2、Python 测试依赖以及 ARM64/x86_64 交叉工具链。

```sh
docker build -f tests/linux/Dockerfile -t tildy-linux-builder:rust-1.95.0 tests/linux
docker run -d --name tildy-linux-check --init --cpus 4 --memory 6g --pids-limit 2048 tildy-linux-builder:rust-1.95.0
tar -cf - Cargo.toml Cargo.lock LICENSE NOTICE licenses docs src shell schema fixtures tests scripts bench |
  docker exec -i -u 0 tildy-linux-check tar -C /work -xmf -
docker exec -u 0 tildy-linux-check chown -R 1000:1000 /work
docker exec -it tildy-linux-check bash
```

将源码复制进 Linux 文件系统，避免用 macOS bind mount 的通知行为代替原生 inotify。以下命令在容器里顺序执行；不要在性能断言运行期间并行编译。

```sh
export CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=4
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --no-fail-fast
python3 fixtures/validate.py
python3 -m unittest discover -s bench -p test_resource.py
python3 -m unittest discover -s tests/linux -p test_release.py
bash scripts/package-linux.sh
binary=target/$(rustc -vV | sed -n 's/^host: //p')/release/tildy
TILDY_SELFTEST_BIN="/work/$binary" python3 shell/zsh/selftest/selftest.py
mkdir -p linux-results
python3 tests/pty/run.py --tildy-bin "$binary" --json linux-results/pty-default.json
python3 tests/pty/run.py --fake-mode persistent --tildy-bin "$binary" --json linux-results/pty-persistent.json
python3 tests/pty/run.py --real --tildy-bin "$binary" --json linux-results/pty-real.json
python3 bench/resource.py --binary "linux=/work/$binary" --out linux-results/resource-idle
python3 bench/resource.py --binary "linux=/work/$binary" --out linux-results/resource-churn --mode churn --cell 10:different --preset pastel-powerline --duration 60
```

默认与 persistent 矩阵应无依赖缺失跳过；real 矩阵只允许跳过声明为 fake-only 的场景。`cargo test` 的 Git 微基准原本是 ignored；真实 Starship 已装入镜像，不应缺失。

`package-smoke.sh` 接受一个压缩包路径，在目标架构的 Ubuntu 用户空间内验证版本、配置、预览与幂等初始化。ARM64 完整矩阵通过不能替代 x86_64 完整矩阵；模拟 x86_64 的包冒烟需单列。

`homebrew-smoke.py` 验证实际安装到 Homebrew Cellar 的包。`prepare --profile /绝对路径 --version 旧版本` 保存配置、`.zshrc` 哈希并运行两次真实 Zsh；执行 `brew update && brew upgrade tildy` 后，用同一路径运行 `verify --version 新版本`，检查配置保留、初始化幂等和新版本编译缓存。干净安装后用新路径运行 `fresh --version 新版本` 检查零配置默认值及真实 Zsh。三个模式都需 `--profile`，默认 prefix 是 `/home/linuxbrew/.linuxbrew`。容器中另执行 `brew test WatsonHS/tildy/tildy`。首次 Linux 发布没有公开旧版 Linux 包，升级基线需明确标注本地 0.3.5 验证包，不冒充公开下载。

Linux 资源数据用 `/proc/PID/stat` 与 `status`，CPU 按 `SC_CLK_TCK` 量化；`phys_footprint_bytes` 为 null。短时没有 CPU tick 增长不能证明 macOS 的 0.001% 门槛，RSS 也不能替代 macOS physical footprint。

取回证据与包后，可用 `docker rm -f tildy-linux-check` 清理本次容器。

M4 0.4.0 发布复验：同一 smoke 支持 macOS（`--prefix /opt/homebrew` 或隔离 prefix），从 PATH 查找 Zsh。升级后和全新安装时加 `--m4`，额外检查项目层来源及无效层回退、Starship 固定源导入、重复写入冲突与原文件保留。Linux 以公开 0.3.6 为升级基线。
