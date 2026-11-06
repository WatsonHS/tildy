# Language and environment modules

These providers query tools asynchronously after detecting a matching project.

| language | 检测文件 | 检测扩展名 | 版本命令 | 提取 |
| --- | --- | --- | --- | --- |
| nodejs | package.json、.node-version、.nvmrc | js、mjs、cjs、ts、mts、cts | `node --version` | `v?(\d+\.\d+\.\d+)` |
| rust | Cargo.toml | rs | `rustc --version` | `rustc (\S+)` |
| golang | go.mod、go.sum、go.work、glide.yaml、Gopkg.yml、.go-version | go | `go version` | `go(\S+)` |
| java | pom.xml、build.gradle、build.gradle.kts、.java-version、deps.edn、project.clj、build.boot、.sdkmanrc | java、class、jar、gradle、clj、cljc | `java -Xinternalversion` | `(\d+(\.\d+)*)`（第一个版本串） |
| c | 无 | c、h | `cc --version` | 第一行的版本号 |
| python | requirements.txt、.python-version、pyproject.toml、Pipfile、tox.ini、setup.py、__init__.py | py、ipynb | `python3 --version`（无则 `python`） | `Python (\S+)`；`env` 字段来自请求 env 的 `VIRTUAL_ENV` basename 或 `CONDA_DEFAULT_ENV` |
| docker_context | Dockerfile、docker-compose.yml、docker-compose.yaml、compose.yml、compose.yaml、.dockerenv | 无 | 不执行命令 | 来源：请求 env 的 `DOCKER_CONTEXT`、`DOCKER_HOST`，否则 `~/.docker/config.json` 的 `currentContext`；值为 `default` 或空时隐藏 |

检测：只看当前目录的直接条目（最多 500 项），`detect_folders` 可选；检测命中才执行命令。缓存键：模块 ID、cwd、检测文件的 mtime 集合、命令 argv；TTL 默认 600 秒；超时默认 500 毫秒；同一命令并发只跑一次。Java 的版本命令慢（200 到 400 毫秒），依赖缓存。

