//! Execute every documented CLI step in a private HOME/config/state directory.
use serde_json::Value;
use std::process::Command;

#[test]
fn documented_configuration_and_diagnostic_flow() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/replay-agent-flows.py"))
        .args(["--binary", env!("CARGO_BIN_EXE_tildy"), "--root"])
        .arg(root.path())
        .output().unwrap();
    assert!(output.status.success(), "stdout={} stderr={}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["original_revision"], result["restored_revision"]);
    let transcript: Value = serde_json::from_slice(&std::fs::read(root.path().join("cli-transcript.json")).unwrap()).unwrap();
    let steps: Vec<_> = transcript.as_array().unwrap().iter().map(|entry| entry["args"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>()).collect();
    for required in [vec!["capabilities"], vec!["config", "get"], vec!["config", "validate"], vec!["preview"], vec!["config", "apply"], vec!["config", "rollback"], vec!["inspect", "--list"], vec!["explain", "--here"], vec!["explain", "--session"], vec!["timings", "--session"]] {
        assert!(steps.iter().any(|step| step.starts_with(&required)), "missing {required:?}");
    }
}

#[test]
fn stable_diagnostic_errors() {
    let root = tempfile::tempdir().unwrap();
    for (args, code, machine) in [
        (vec!["timings", "--session", "absent", "--last", "abc", "--json"], 2, "validation_failed"),
        (vec!["timings", "--session", "absent", "--last", "-1", "--json"], 2, "validation_failed"),
        (vec!["timings", "--session", "absent", "--last", "0", "--json"], 2, "validation_failed"),
        (vec!["timings", "--session", "absent", "--last", "33", "--json"], 2, "validation_failed"),
        (vec!["timings", "--session", "absent", "--json"], 5, "backend_unavailable"),
        (vec!["explain", "--session", "absent", "--json"], 5, "backend_unavailable"),
        (vec!["explain", "--here", "--module", "missing", "--json"], 2, "validation_failed"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_tildy")).args(args).current_dir(root.path())
            .env("HOME", root.path()).env("XDG_CONFIG_HOME", root.path().join("config")).env("XDG_STATE_HOME", root.path().join("state"))
            .env_remove("TILDY_CONFIG").env_remove("TILDY_SESSION_ID").output().unwrap();
        assert_eq!(out.status.code(), Some(code), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stderr.is_empty());
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["error"]["code"], machine);
    }
}

#[test]
fn explain_here_measures_real_providers_and_maps_custom_host_id() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("candidate.json");
    std::fs::write(&config, r#"{
      "schema_version":1,
      "modules": {
        "custom_host":{"type":"hostname"},
        "slow":{"type":"command","argv":["/bin/sh","-c","sleep 1"],"timeout_ms":30}
      },
      "layout":{"rows":[{"left":["custom_host","slow"],"right":[]}]}
    }"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tildy"))
        .args(["explain", "--here", "--config"]).arg(config).arg("--json")
        .current_dir(root.path()).env("HOME", root.path()).env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state")).env_remove("TILDY_CONFIG").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let modules = value["data"]["modules"].as_array().unwrap();
    let host = modules.iter().find(|m| m["module"]["id"] == "custom_host").unwrap();
    assert_eq!(host["provider"], "session/hostname");
    assert_eq!(host["timing"]["executed"], true);
    let slow = modules.iter().find(|m| m["module"]["id"] == "slow").unwrap();
    assert_eq!(slow["provider"], "slow");
    assert_eq!(slow["data_source"], "timeout");
    assert_eq!(slow["timing"]["timed_out"], true);
    assert!(slow["timing"]["duration_ms"].as_u64().unwrap() >= 20);
}

#[test]
fn explain_session_json_distinguishes_warm_partial_settled_and_pure_cache() {
    let root = tempfile::tempdir().unwrap();
    let mut snapshot: tildy::diagnostics::Snapshot = serde_json::from_str(include_str!("../fixtures/protocol/snapshot.valid-m3.json")).unwrap();
    snapshot.session_id = Some("source-contract".into());
    snapshot.module_reports[0].freshness = Some(tildy::model::Freshness::Fresh);
    let git_slot = snapshot.slots.iter().position(|slot| slot.provider == "git").unwrap();
    snapshot.slots[git_slot].freshness = tildy::model::Freshness::Fresh;
    snapshot.slots[git_slot].age_ms = Some(100);
    snapshot.timing_history[0].providers[0].cache_hit = true;
    for (fetching, executed, completed, expected) in [(true, true, false, "cache"), (false, true, true, "fresh"), (false, false, false, "cache"), (false, false, true, "fresh")] {
        snapshot.slots[git_slot].fetching = fetching;
        snapshot.timing_history[0].providers[0].executed = executed;
        snapshot.timing_history[0].providers[0].completed = completed;
        snapshot.timing_history[0].providers[0].coalesced = !executed && completed;
        snapshot.timing_history[0].providers[0].coalesced_count = u64::from(!executed && completed);
        tildy::diagnostics::write_snapshot(&root.path().join("state/tildy/sessions/source-contract.json"), &snapshot).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_tildy"))
            .args(["explain", "--session", "source-contract", "--module", "git", "--json"])
            .env("XDG_STATE_HOME", root.path().join("state")).output().unwrap();
        assert!(output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["data"]["modules"][0]["data_source"], expected);
        assert_eq!(value["data"]["modules"][0]["timing"]["cache_hit"], true, "historical cycle hit is retained independently");
        assert_eq!(value["data"]["modules"][0]["module"]["freshness"], "fresh");
    }
}
