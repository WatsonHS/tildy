# Architecture

Tildy has a Rust core and a Zsh adapter. Each interactive shell owns a worker through pipes. The adapter shows a prompt immediately, sends context to the worker and applies valid results when the line editor can redraw safely. A one-shot renderer provides fallback when the worker is unavailable.

The worker cancels obsolete work, coalesces provider queries, caches bounded results and watches the current worktree. Generations and worker epochs prevent results for old contexts from replacing the current prompt. Providers use timeouts; shell input does not wait for slow Git or language queries.

Configuration, the theme picker and noninteractive CLI share validation, rendering and transaction code. User configuration writes use revision checks and locks. Repository overrides affect display only. Diagnostic snapshots expose actual decisions and timings without requiring a network service.

- `src/config`: loading, validation, layers, presets and transactions
- `src/providers`, `src/scheduler`, `src/session`: data acquisition and worker lifecycle
- `src/render`, `src/width`: deterministic rendering and column accounting
- `src/cli`: human and JSON command interfaces
- `shell/zsh`: initialization, ZLE integration and safe redraw
- `schema`, `fixtures`, `tests`, `bench`: contracts and reproducible validation

See [protocol](protocol.md), [configuration](configuration.md), [diagnostics](diagnostics.md) and the [decision records](decisions/README.md). Earlier decision records retain their historical context; later superseding decisions describe the current behavior.
