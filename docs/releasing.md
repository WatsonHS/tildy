# Release process

Build, test and package locally. The repository has no hosted CI; GitHub Actions is disabled.

1. Update Cargo.toml, Cargo.lock, CHANGELOG.md and the version's release record.
2. Run Rust/Clippy, schema, shell and relevant PTY regression checks. Record native vs emulated validation and skips explicitly.
3. Run `scripts/release.sh --build-only` on macOS with both Rust targets and Docker available. It stages four archives and a Homebrew formula without publishing.
4. Inspect archive contents, license notices and checksums. Exercise install/upgrade and configuration preservation against staged packages.
5. Commit the reviewed source. `scripts/release.sh` requires clean source and tap checkouts and rejects an existing release version. It uploads all assets to a draft, publishes it and updates the tap formula.
6. Verify the public assets and the Homebrew path, then update the release record with actual results.

Source releases use [WatsonHS/tildy](https://github.com/WatsonHS/tildy). Binary distribution uses [WatsonHS/homebrew-tildy](https://github.com/WatsonHS/homebrew-tildy).

## First public source snapshot

The public repository starts with a reviewed 0.4.1 snapshot and fresh history. Historical development commits contain personal machine evidence and remain in a separate private repository. Do not merge or push that historical repository into this one.

`scripts/export-source.py OUTPUT` exports the current source file set without `.git`, build outputs or local observations, checks public-source hygiene and writes a SHA-256 manifest. It does not publish, change remotes or rewrite history. `SOURCE-MANIFEST.json` records the files in the initial public snapshot; generate a fresh manifest when exporting later versions.
