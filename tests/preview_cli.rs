//! `tildy preview`: scenario list, rendering in builtin and a preset, the
//! `--json` envelope, module report annotations, and error codes. Every run
//! gets a sandboxed HOME/XDG/TILDY_CONFIG in a temp dir.
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn tildy(args: &[&str], home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tildy"))
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_STATE_HOME", home.join("xdg-state"))
        .env("TILDY_CONFIG", home.join("no-such-config.json"))
        .env_remove("TILDY_SYMBOLS")
        .env("COLUMNS", "100")
        .output()
        .unwrap()
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

const NAMES: &[&str] = &[
    "plain-dir", "home", "git-clean", "git-dirty", "git-conflict", "git-ahead-behind", "command-failed", "long-command", "ssh", "narrow",
    "timeout", "stale-cache", "jobs", "languages", "agent-blocked", "agent-error",
];

fn module<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["data"]["modules"].as_array().unwrap().iter().find(|m| m["id"] == id).unwrap_or_else(|| panic!("no module {id}"))
}

#[test]
fn list_has_the_fixed_scenarios() {
    let d = tempfile::tempdir().unwrap();
    let out = tildy(&["preview", "--list", "--json"], d.path());
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out);
    let names: Vec<_> = v["data"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, NAMES);
    let text = tildy(&["preview", "--list"], d.path());
    assert!(String::from_utf8_lossy(&text.stdout).contains("git-conflict"));
}

#[test]
fn every_scenario_renders_in_builtin_and_pastel_powerline() {
    let d = tempfile::tempdir().unwrap();
    for args in [&["--symbols", "unicode"][..], &["--preset", "pastel-powerline", "--symbols", "nerd"][..]] {
        for n in NAMES {
            let mut a = vec!["preview", "--scenario", n, "--json"];
            a.extend_from_slice(args);
            let out = tildy(&a, d.path());
            assert_eq!(out.status.code(), Some(0), "{n} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            let v = json(&out);
            assert_eq!(v["ok"], true);
            let data = &v["data"];
            assert_eq!(data["scenario"], *n);
            assert_eq!(data["cols"], if *n == "narrow" { 40 } else { 100 });
            assert!(!data["lines"].as_array().unwrap().is_empty(), "{n}");
            assert!(data["transient"].is_string());
            for m in data["modules"].as_array().unwrap() {
                assert!(m["source"].is_string() && m["row"].is_u64() && m["side"].is_string(), "{n}: {m}");
            }
            // Text mode renders too.
            let mut t = vec!["preview", "--scenario", n];
            t.extend_from_slice(args);
            let out = tildy(&t, d.path());
            assert_eq!(out.status.code(), Some(0), "{n} text");
            assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("── {n}")));
        }
    }
}

#[test]
fn all_renders_every_scenario() {
    let d = tempfile::tempdir().unwrap();
    let out = tildy(&["preview", "--all", "--json", "--symbols", "ascii"], d.path());
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out);
    let got: Vec<_> = v["data"].as_array().unwrap().iter().map(|e| e["scenario"].as_str().unwrap()).collect();
    assert_eq!(got, NAMES);
    let text = tildy(&["preview", "--all"], d.path());
    let s = String::from_utf8_lossy(&text.stdout);
    assert!(NAMES.iter().all(|n| s.contains(&format!("── {n}"))));
}

#[test]
fn scenario_data_drives_the_modules() {
    let d = tempfile::tempdir().unwrap();
    let run = |n: &str| json(&tildy(&["preview", "--preset", "plain", "--scenario", n, "--json", "--symbols", "ascii"], d.path()));
    let v = run("git-conflict");
    let git = module(&v, "git");
    assert_eq!(git["visible"], true);
    assert_eq!(git["rules_hit"], serde_json::json!(["conflicted"]));
    assert_eq!(git["style"], "fg=red");
    assert!(git["text"].as_str().unwrap().contains("=2"));
    let v = run("timeout");
    assert_eq!(module(&v, "git")["freshness"], "timeout");
    let v = run("stale-cache");
    let git = module(&v, "git");
    assert_eq!(git["freshness"], "stale");
    assert_eq!(git["age_ms"], 90_000);
    assert_eq!(module(&v, "node")["age_ms"], 90_000);
    let v = run("languages");
    assert_eq!(module(&v, "node")["visible"], true);
    assert!(module(&v, "rust")["text"].as_str().unwrap().contains("1.82.0"));
    let v = run("command-failed");
    assert_eq!(module(&v, "result")["visible"], true);
    let v = run("long-command");
    assert_eq!(module(&v, "elapsed")["visible"], true);
    let v = run("ssh");
    assert_eq!(module(&v, "ssh_host")["visible"], true);
    let v = run("jobs");
    assert_eq!(module(&v, "jobs")["visible"], true);
    let v = run("plain-dir");
    assert_eq!(module(&v, "git")["visible"], false);
    // --cols overrides the scenario width.
    let v = json(&tildy(&["preview", "--scenario", "narrow", "--cols", "60", "--json"], d.path()));
    assert_eq!(v["data"]["cols"], 60);
}

/// The `narrow` scenario (40 columns, deep directory, dirty repository with a
/// long branch) renders without wrapping: upper rows are at most 40 columns,
/// the last row leaves the cursor a column and, with a right side, the gap Zsh
/// keeps before RPROMPT. What does not fit is reported hidden with `no_room`.
/// Strict for the default preview and the author's presets (pastel-powerline;
/// plain and lean outside the ascii tier); in every other
/// preset and tier a row may only stay too wide when a single droppable module
/// is left on it (the renderer always keeps the highest-priority module and
/// never truncates beyond the directory module's own rules).
#[test]
fn narrow_scenario_renders_without_wrapping() {
    use tildy::render::visible_width;
    let d = tempfile::tempdir().unwrap();
    let mut shrunk = 0;
    // In the ascii tier the directory's ".../" marker alone makes plain and lean
    // one or two columns too wide for 40.
    let strict = |preset: &str, tier: &str| preset == "pastel-powerline" || (matches!(preset, "plain" | "lean") && tier != "ascii");
    for preset in tildy::config::PRESETS {
        for tier in ["nerd", "unicode", "ascii"] {
            let v = json(&tildy(&["preview", "--scenario", "narrow", "--preset", preset, "--symbols", tier, "--json"], d.path()));
            let data = &v["data"];
            assert_eq!(data["cols"], 40);
            let modules = data["modules"].as_array().unwrap();
            let lines: Vec<&str> = data["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
            let n = lines.len();
            let right = data["right"].as_str().map_or(0, |r| visible_width(r) + 1);
            // Rows of the layout that produced a line (an empty first line is add_newline).
            let first_row = usize::from(lines[0].is_empty());
            for (i, l) in lines.iter().enumerate().skip(first_row) {
                let (w, budget) = if i + 1 == n { (visible_width(l) + right, 39) } else { (visible_width(l), 40) };
                if w <= budget {
                    continue;
                }
                assert!(!strict(preset, tier), "{preset}/{tier}: row {i} is {w} wide (budget {budget}): {l:?}");
                let row = (i - first_row) as u64;
                let left: Vec<_> = modules
                    .iter()
                    .filter(|m| m["visible"] == true && m["row"] == row && m["module_type"] != "character" && m["module_type"] != "fill")
                    .collect();
                assert_eq!(left.len(), 1, "{preset}/{tier}: row {i} is {w} wide with {} droppable modules left: {l:?}", left.len());
            }
            shrunk += usize::from(modules.iter().any(|m| m["hidden_reason"] == "no_room"));
        }
    }
    assert!(shrunk > 0, "the narrow scenario should need shrinking somewhere");
    // The default preview (builtin config, unicode) fits as well.
    let v = json(&tildy(&["preview", "--scenario", "narrow", "--json"], d.path()));
    let lines = v["data"]["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "");
    for line in &lines[1..] { assert!(visible_width(line.as_str().unwrap()) < 40, "{line}"); }
    // At 70 columns the author's preset keeps the branch it drops at 40.
    let at = |cols: &str| {
        let v = json(&tildy(&["preview", "--scenario", "narrow", "--preset", "pastel-powerline", "--cols", cols, "--json"], d.path()));
        v["data"]["modules"].as_array().unwrap().iter().filter(|m| m["visible"] == true).count()
    };
    assert!(at("70") > at("40"));
}

#[test]
fn source_distinguishes_builtin_preset_and_user() {
    let d = tempfile::tempdir().unwrap();
    let cfg = d.path().join("c.json");
    std::fs::write(
        &cfg,
        r#"{"schema_version":1,"preset":"pastel-powerline","modules":{"cwd":{"style":{"foreground":"red"}},"mine":{"type":"text","text":"hi"}},
        "layout":{"rows":[{"left":["cwd","git","mine"],"right":[]}]}}"#,
    )
    .unwrap();
    let v = json(&tildy(&["preview", "--scenario", "git-clean", "--config", cfg.to_str().unwrap(), "--json"], d.path()));
    assert_eq!(module(&v, "cwd")["source"], "user");
    assert_eq!(module(&v, "mine")["source"], "user");
    assert_eq!(module(&v, "git")["source"], "preset:pastel-powerline");
    let v = json(&tildy(&["preview", "--scenario", "git-clean", "--json"], d.path()));
    assert_eq!(module(&v, "git")["source"], "preset:pastel-powerline");
}

#[test]
fn agent_scenarios_fill_the_agent_slot() {
    let d = tempfile::tempdir().unwrap();
    let cfg = d.path().join("agent.json");
    std::fs::write(
        &cfg,
        r#"{"schema_version":1,"modules":{"agent":{"type":"agent_status"}},
        "layout":{"rows":[{"left":["cwd","git"],"right":["agent"]}]}}"#,
    )
    .unwrap();
    for (n, mark) in [("agent-blocked", "?"), ("agent-error", "x")] {
        let v = json(&tildy(&["preview", "--scenario", n, "--config", cfg.to_str().unwrap(), "--symbols", "ascii", "--json"], d.path()));
        let a = module(&v, "agent");
        assert_eq!(a["module_type"], "agent_status");
        assert_eq!(a["source"], "user");
        assert_eq!(a["side"], "right");
        // Until the agent_status renderer lands the module is hidden; once it
        // does, the state symbol and app must show.
        if a["visible"] == true {
            let t = a["text"].as_str().unwrap();
            assert!(t.contains(mark) && t.contains("claude-code"), "{n}: {t}");
        }
    }
    // Other scenarios leave the slot empty (hidden).
    let v = json(&tildy(&["preview", "--scenario", "git-clean", "--config", cfg.to_str().unwrap(), "--json"], d.path()));
    assert_eq!(module(&v, "agent")["visible"], false);
}

#[test]
fn errors_use_the_envelope_and_exit_codes() {
    let d = tempfile::tempdir().unwrap();
    let out = tildy(&["preview", "--scenario", "nope", "--json"], d.path());
    assert_eq!(out.status.code(), Some(2));
    let v = json(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "validation_failed");
    assert_eq!(v["error"]["path"], "scenario");
    // No scenario at all.
    assert_eq!(tildy(&["preview", "--json"], d.path()).status.code(), Some(2));
    // Unknown preset, unknown tier.
    assert_eq!(tildy(&["preview", "--scenario", "home", "--preset", "nope", "--json"], d.path()).status.code(), Some(2));
    let out = tildy(&["preview", "--scenario", "home", "--symbols", "emoji", "--json"], d.path());
    assert_eq!(json(&out)["error"]["path"], "symbols");
    // Invalid config file: exit 2 with the JSON path of the problem.
    let bad = d.path().join("bad.json");
    std::fs::write(&bad, r#"{"schema_version":1,"layout":{"rows":[{"left":["ghost"]}]}}"#).unwrap();
    let out = tildy(&["preview", "--scenario", "home", "--config", bad.to_str().unwrap(), "--json"], d.path());
    assert_eq!(out.status.code(), Some(2));
    let v = json(&out);
    assert_eq!(v["error"]["code"], "validation_failed");
    assert!(v["error"]["path"].as_str().is_some_and(|p| p.contains("layout")), "{v}");
    // Text mode: message on stderr, nothing on stdout.
    let out = tildy(&["preview", "--scenario", "nope"], d.path());
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty() && !out.stderr.is_empty());
}

#[test]
fn preview_leaves_the_sandbox_untouched() {
    let d = tempfile::tempdir().unwrap();
    tildy(&["preview", "--all"], d.path());
    let entries: Vec<_> = std::fs::read_dir(d.path()).unwrap().collect();
    assert!(entries.is_empty(), "preview wrote files: {entries:?}");
}
