# Tildy

An asynchronous Zsh prompt with a Rust core. Keep typing while Git and language tools update in the background; configure and diagnose it through a CLI that works for people, scripts and agents.

[中文说明](README.zh-CN.md) · [Configuration](docs/configuration.md) · [Contributing](CONTRIBUTING.md) · [Changelog](CHANGELOG.md)

## Features

- Instant startup prompt, background refresh and compact command history.
- Git/worktree status, language versions, command duration, exit status and SSH context.
- Interactive themes with live previews, named palettes and narrow-window layouts.
- JSON configuration with validation, revision checks, preview, history and rollback.
- `explain`, `timings` and `inspect` for actual rendering decisions and provider timings.
- Limited Starship import with an explicit report of ignored, bridged and manual items.
- Optional agent status. Rendering is local; no model service or network connection is required.

## Install

macOS or Linux, Zsh and Homebrew:

```sh
brew tap WatsonHS/tildy
brew trust WatsonHS/tildy
brew install tildy
tildy init
```

Open a new terminal. `tildy init` backs up and updates `.zshrc`, respecting `ZDOTDIR`; repeat runs are idempotent. Preview changes with `tildy init --dry-run`. Disable the initialization of other prompts before enabling Tildy.

The default `pastel-powerline` theme uses a Nerd Font. For a simpler theme:

```sh
tildy theme --preset plain
```

Linux binary packages require glibc **2.39 or newer** (for example, Ubuntu 24.04). See the [support matrix](docs/support.md) for runtime validation boundaries. Bash, Fish, PowerShell and Windows are not supported.

Upgrade with `brew update && brew upgrade tildy`, then open a new terminal or run `exec zsh`.

## Configure

No configuration file is required. Run `tildy theme` for theme, palette, layout and blank-line controls. The CLI and theme picker use the same validation and configuration transactions.

```sh
tildy config get --effective --json
tildy preview --all
tildy explain --here --json
tildy capabilities --json
```

Configuration lookup: `$TILDY_CONFIG`, then `$XDG_CONFIG_HOME/tildy/config.json`, then `~/.config/tildy/config.json`. A `.tildy.json` at the nearest Git root can override display settings only; it cannot add commands or activate previously hidden modules. See [import and project layers](docs/import-layering.md).

Messages default to English. Set `TILDY_LANG=zh` before initialization for Chinese, or `TILDY_LANG=auto` to follow the system locale. CLI syntax and JSON keys remain English.

## Build and test

Use a current Rust toolchain (validated with Rust 1.95.0), Zsh, Git and Python 3:

```sh
cargo build --locked --release
RUST_TEST_THREADS=4 cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Use `scripts/dev-shell.sh --release` to try the build in an isolated shell. See [CONTRIBUTING.md](CONTRIBUTING.md) for PTY dependencies, plugin checks and Linux validation. The [agent example](docs/agent-flows.md) tests configuration through rollback in an isolated environment.

## Documentation

- [Themes](docs/theme.md), [presets](docs/presets.md), [palettes](docs/palettes.md), [preview](docs/preview.md)
- [Configuration transactions](docs/configuration.md), [Starship import](docs/import-layering.md)
- [Diagnostics](docs/diagnostics.md), [terminal width](docs/terminal-width.md)
- [Agent status](docs/agent-status.md), [optional PTY wrapper](docs/agent-wrap.md)
- [Architecture](docs/architecture.md), [shell protocol](docs/protocol.md), [decisions](docs/decisions/README.md)
- [Compatibility policy](docs/compatibility.md), [uninstall and recovery](docs/uninstall.md)
- [Roadmap](ROADMAP.md), [release process](docs/releasing.md)

Several technical guides are currently in Chinese. Command examples and schemas are language independent.

## License

Original code: [MIT](LICENSE). Derived presets, palettes and dependencies retain their licenses; see [NOTICE](NOTICE) and [licenses](licenses/README.md).
