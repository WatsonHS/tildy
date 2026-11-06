# Roadmap

Updated 2026-11-07.

Tildy focuses on a responsive, reliable Zsh prompt and configuration that both people and agents can inspect, preview and undo.

## Delivered

M0–M4 delivered asynchronous rendering, instant and compact prompts, themes, configuration transactions, diagnostics, external Git refresh, Linux packaging, Starship import and project display layers. Version 0.4.1 closes the blank-input error-display fix and opens the source to external contributors.

The maintainer reported several weeks of positive daily use on 2026-11-07. This supports continuing the project; it does not substitute for a recorded check of every terminal, architecture or release criterion. See the [release record](docs/releases/0.4.1.md).

## Next: M6 stable release

- Keep core input, redraw, worktree identity and configuration transactions covered by regression tests.
- Gather independent installation and daily-use feedback.
- Validate supported platforms and distinguish native execution, emulation and cross-compilation.
- Review CLI/schema compatibility and recovery paths before declaring 1.0.
- Preserve reproducible measurement scripts; publish bounded summaries rather than personal machine logs.

M5, a full interactive editor, is deferred. Revisit it when the existing theme picker, CLI and agent-driven preview workflow have concrete shortcomings.

Additional shells, a shared daemon, an embedded model service and a general plugin ecosystem require demonstrated use cases. They are outside the current release scope.
