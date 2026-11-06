# Diagnostics contract

`tildy inspect --session ID [--json]` reads the last worker snapshot. `tildy inspect --list [--json]` lists readable snapshots whose worker PID still exists; it sorts by session ID. A worker removes its automatic snapshot on orderly exit. PID existence is a liveness hint and does not establish process identity after PID reuse. The existing inspect JSON envelope and `{path, worker_alive, snapshot}` data shape remain stable; list data is an array of that same object.

`tildy explain --session ID [--module ID] [--json]` explains the last rendered reports without executing providers. `tildy explain --here [--config FILE] [--cols N]` fetches real provider data for the current directory once and measures each fetch, then renders once. Without `--session` or `--here`, explain uses exported `TILDY_SESSION_ID` when available, otherwise the current directory. Explicit `--here` ignores the shell session. Unknown module IDs return `validation_failed` (exit 2); a missing snapshot or a pre-M3 snapshot without module reports returns `backend_unavailable` (exit 5), with a hint in the message to render or use `--here`.

Explain JSON data is `{session_id, config_revision, modules}`. Each item contains the original `module` report, a nonempty human `reason`, `provider` key or null, `data_source`, and this cycle's `timing` or null. `module.freshness` remains independent of `timing.cache_hit`: cached stale data is reported as `stale`, and its cache flag remains visible. A warm partial reports `cache` while revalidation is fetching; after a completed fetch supplies the settled fresh slot it reports `fresh`, even when the same cycle previously hit cache. `timing.completed` also records completion of shared work started in an earlier cycle, so a coalesced result is correctly classified. Pure cache hits remain `cache`. Historical timeout flags do not override currently fresh data after a late result. Context-only modules have no provider timing. Provider-backed module IDs map to their own fetch; render-only Git modules share `git`, and host/user use `session/hostname` and `session/username`. Reasons include unmet show conditions, no detected data, empty content and priority contraction. For `no_room`, `module.additional_columns` states the width increase needed to restore the module under the current layout and priorities.

`tildy timings [--session ID] [--last N] [--json]` selects the newest N recorded cycles, ordered oldest first. Session defaults to `TILDY_SESSION_ID`; outside a Tildy shell it is required. N defaults to 10 and must be 1–32. Invalid integers, negative values and out-of-range values return the stable JSON error envelope with `validation_failed`, exit 2. JSON data is an array of cycles. No historical cycles yields `[]` (old snapshots remain readable).

## Snapshot v1 additive fields

Authoritative schema: [snapshot.schema.json](../schema/snapshot.schema.json). All previous v1 fields retain their names and meaning. New fields deserialize with defaults when reading old snapshots:

| Field | Stable meaning |
| --- | --- |
| `watch.mode` | Watch backend name, e.g. `fsevents`, `poll`, or `off` |
| `watch.last_event_ms` | Last accepted watch event Unix milliseconds, null before any event |
| `watch.coalesced_events` | Watch events folded into a pending refresh |
| `watch.fallback_count` | Idle fallback refresh trigger count |
| `module_reports` | Reports from the last actual render, including hidden modules, rules, sources, age and layout placement |
| `timing_history` | At most 32 actual request/refresh cycles, oldest first |
| `timing_history[].gen` | Request generation; multiple idle refresh cycles can share a generation |
| `timing_history[].at_ms` | Cycle start Unix milliseconds |
| `providers[].duration_ms` | Actual execution duration rounded down to whole milliseconds; zero for a sub-millisecond execution or no execution |
| `providers[].cache_hit` | This cycle reused a cached result |
| `providers[].executed` | This cycle actually started a fetch |
| `providers[].completed` | A fetch result arrived for this cycle, including shared work started in an earlier cycle |
| `providers[].timed_out` | This cycle observed a fetch timeout |
| `providers[].coalesced` | This cycle joined an already pending fetch |
| `providers[].coalesced_count` | Actual scheduler coalesced counter delta within the cycle |

`Snapshot.providers` remains the cumulative summary (`runs`, `last_ms`, `p50_ms`, `max_ms`, scheduler stats). It must not be interpreted as per-cycle timings. The bounded history records scheduler deltas and fetch completions, including refreshes that do not change the rendered text. An in-flight cycle can be updated as results arrive.

All machine-readable commands use the existing `CliEnvelope`: `{api_version:1, ok, data, error:{code,path,message,hint}}`. `capabilities --json` discovers `explain`, `timings`, `inspect` and publishes `diagnostics.snapshot_schema_version=1`, `timing_history_limit=32`, the snapshot schema path, and `active_session_env=TILDY_SESSION_ID`. Shell protocol/config/API versions are independent of the application version.

## M2 debt resolved here

`Store::remove(&scope, id)` acquires the same per-scope directory lock as publish/clear and removes exactly the addressed record; children and other scopes survive. Wrap calls it for exact removals instead of deleting a listed file outside the lock. Subtree removal still uses `clear`.

`record::parse_osc_body` delegates report-body grammar, limits, base64 and normalization to `osc::parse_body`. Scanner and CLI now agree on unused state fields and malformed reports; e.g. `progress` on `done` is absent. This intentionally removes the former parser divergence. The scanner remains responsible for whole-stream framing and exact BEL/ST sequence byte limits.
