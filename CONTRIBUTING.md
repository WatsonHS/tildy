# Contributing

Use Rust (validated with 1.95.0), Git ≥ 2.31, Zsh 5.9 and Python 3 on macOS or Linux. Build from Cargo.lock. Other toolchain versions are not yet a declared compatibility promise.

```sh
python3 -m venv .venv
. .venv/bin/activate
python3 -m pip install -r tests/requirements.txt
cargo build --locked
RUST_TEST_THREADS=4 cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
python3 fixtures/validate.py
python3 -m unittest discover -s bench -p 'test_*.py'
python3 -m unittest discover -s tests/linux -p 'test_*.py'
python3 shell/zsh/selftest/selftest.py
python3 tests/pty/run.py --real
```

Run the fake backends when changing shell protocol or lifecycle handling:

```sh
python3 tests/pty/run.py
python3 tests/pty/run.py --fake-mode persistent
```

PTY plugin coverage requires zsh-autosuggestions, zsh-syntax-highlighting and fzf. Set `TILDY_TEST_PLUGIN_DIR` to the directory containing the first two plugin checkouts. Missing optional integrations are reported as skipped; do not report those cases as passed. See [PTY instructions](tests/pty/README.md) and [Linux instructions](tests/linux/README.md).

The suite contains scheduling-sensitive watcher checks. Use `RUST_TEST_THREADS=4` and avoid concurrent builds/benchmarks while running them; retain failed runs when reporting results.

Keep tests isolated from real shell/configuration files. Use a focused regression for a behavioral fix, then run the relevant existing checks. Avoid formatting unrelated code in a functional patch.

Describe the observed problem, resulting behavior and validation in pull requests. Changes to public JSON fields, configuration or shell frames must document migration/compatibility behavior. Added third-party material needs an upstream source, version and license notice.

Do not commit raw terminal captures, environment dumps, personal shell files or benchmark logs. Put local output in `target/` or another ignored directory and publish a redacted summary. Check diagnostic output before attaching it to an issue: it can contain paths, hostnames and command text.
