# Agent configuration and diagnostics

Use `tildy capabilities --json` and `tildy schema --json` for discovery. Read the current revision, generate a candidate, validate it, preview relevant scenarios and apply it with `--expect-revision`. Retain the original revision for rollback. Use `explain`, `timings` and `inspect` to diagnose actual decisions.

The executable example changes dirty Git to yellow, shows the hostname only over SSH and shows duration above three seconds. It checks previews, applies the candidate, inspects a real worker and restores the original configuration:

```sh
cargo build --locked
python3 scripts/replay-agent-flows.py --binary "$PWD/target/debug/tildy"
cargo test --locked --test agent_flows
```

The example isolates HOME, configuration and state in a temporary directory. It does not need model access or authentication. `--root DIR` retains its transcript locally; review generated files before sharing them.
