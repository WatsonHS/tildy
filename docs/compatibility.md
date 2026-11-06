# Compatibility policy

Tildy is currently pre-1.0. Patch releases preserve configuration semantics and existing CLI/JSON fields unless a documented correctness or security fix requires a change. Breaking changes require explicit release notes and migration instructions; 1.0 has not been declared.

`api_version`, configuration `schema_version`, shell protocol version and the application version are independent. Readers should tolerate additive JSON fields. Scripts should use `--json`, stable error codes and exit codes, not localized messages or visual prompt text. Discover available commands with `capabilities --json`.

Configuration writes compare the expected user-file revision, validate and atomically replace the file with history retained. Incompatible future schemas fail validation; runtime failures preserve a usable fallback prompt. A repository display layer has its own effective revision and never changes the revision used for user-file transactions.

Shell and worker versions are checked during the handshake. Open a new shell or run `exec zsh` after upgrading. The adapter can fall back to one-shot rendering if the worker fails; fallback is not evidence that every old/new version pairing is supported.

Stable 1.0 support will require a published platform matrix and recorded migration/recovery checks. Current scope is in [support.md](support.md).
