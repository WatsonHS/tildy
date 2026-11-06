# Support and validation scope

Tildy targets Zsh on macOS and Linux. Git ≥ 2.31 is required for worktree identity. Current validation uses Zsh 5.9 and Rust 1.95.0; no lower Rust minimum is declared.

| Platform | Existing validation |
| --- | --- |
| macOS ARM64 | Native Rust, Zsh, PTY, Homebrew install/upgrade and resource checks |
| Ubuntu 24.04 ARM64 | Native container Rust, Zsh, PTY, filesystem notifications and install/upgrade checks |
| Ubuntu 24.04 x86_64 | Emulated package and Homebrew checks; not a full native test matrix |
| macOS x86_64 | Cross-compiled package; runtime/install validation remains outstanding |

The matrix summarizes prior 0.4.0 validation, not a claim that every later build repeated it. Version-specific checks are in the [release record](releases/0.4.1.md). Linux prebuilt packages require system glibc ≥ 2.39. Other distributions, older glibc, WSL and network filesystems have no blanket support claim.

GUI checks for 0.4.0 covered Ghostty 1.3.1, cmux 0.65.0, iTerm2 3.7.6 and Terminal.app 2.15. Terminal.app keeps full accepted prompts to preserve native command navigation; the other three can also compact history. These checks do not establish compatibility with all future terminal versions. PTY tests model shell behavior and cannot replace GUI navigation/font/IME checks.

Default Nerd symbols require an appropriate font. Use the plain preset or `TILDY_SYMBOLS=ascii` where needed. Explicit width calibration disables instant-cache reuse while a font revision is set; see [terminal width](terminal-width.md).
