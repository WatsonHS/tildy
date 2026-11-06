# Changelog

## 0.4.1 — 2026-11-07

- Blank or whitespace-only input dismisses the preceding command's error indicator and error color. The real `$?` and existing failure summary are preserved; subsequent failures and syntax errors still display their status.
- Source distribution now includes an MIT license, third-party preset/palette notices, contributor instructions and public technical guides.
- Binary archives include project and dependency license notices; release builds remap developer home and source paths in embedded diagnostics and normalize archive owner/timestamp metadata.

## 0.4.0 — 2026-10-23

- Starship import reports ignored, bridged and manual items; imported bridges retain their source TOML path.
- Display-only project configuration at the nearest Git root, with validation and source tracking.
- Ghostty/cmux/iTerm2 integration and resize fixes. Terminal.app preserves full history prompts for native command navigation.
- macOS and Linux ARM64/x86_64 packages, configuration write locks, startup caching and installation/upgrade checks.
