//! `tildy theme` without a terminal: the non-interactive `--preset` path
//! writes a minimal user config that `config::load` reads back through the
//! config transaction (history entry + rollback command), and the interactive
//! path refuses to start without a TTY.
use serde_json::{json, Value};
use tildy::config::{self, SymbolTier, PRESET_INFO};
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn tildy(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tildy"))
        .args(args)
        .current_dir(cwd)
        .env("TILDY_CONFIG", cwd.join("default-config.json"))
        // The transaction history must not land in the developer's real state dir.
        .env("XDG_STATE_HOME", cwd.join("state"))
        .env_remove("TILDY_SYMBOLS")
        .env("TILDY_LANG", "zh")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn theme_is_the_primary_command_and_configure_remains_compatible() {
    let dir = tempfile::tempdir().unwrap();
    let help = tildy(&["--help"], dir.path());
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.lines().any(|line| line.split_whitespace().next() == Some("theme")));
    assert!(!text.lines().any(|line| line.split_whitespace().next() == Some("configure")));
    let theme_help = tildy(&["theme", "--help"], dir.path());
    let alias_help = tildy(&["configure", "--help"], dir.path());
    assert!(theme_help.status.success() && alias_help.status.success());
    assert!(String::from_utf8_lossy(&theme_help.stdout).contains("Usage: tildy theme"));
    assert_eq!(theme_help.stdout, alias_help.stdout);

    let current = dir.path().join("theme.json");
    let legacy = dir.path().join("configure.json");
    let theme = tildy(&["theme", "--preset", "tokyo-night", "--one-line", "--output", current.to_str().unwrap()], dir.path());
    let alias = tildy(&["configure", "--preset", "tokyo-night", "--one-line", "--output", legacy.to_str().unwrap()], dir.path());
    assert!(theme.status.success() && alias.status.success());
    assert_eq!(theme.stdout, alias.stdout);
    assert_eq!(read_json(&current), read_json(&legacy));
}

#[test]
fn non_interactive_writes_minimal_config_that_loads_back() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("nested").join("config.json");
    let o = tildy(
        &["theme", "--preset", "tokyo-night", "--one-line", "--no-time", "--symbols", "unicode", "--output", out.to_str().unwrap()],
        dir.path(),
    );
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    // stdout is exactly the written JSON.
    let printed: Value = serde_json::from_slice(&o.stdout).unwrap();
    let written = read_json(&out);
    assert_eq!(printed, written);
    let keys: Vec<_> = written.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["layout", "preset", "schema_version", "symbols"]);

    let cfg = config::load(Some(&out)).unwrap();
    assert_eq!(cfg.file.preset.as_deref(), Some("tokyo-night"));
    assert_eq!(cfg.symbols, SymbolTier::Unicode);
    let rows = &cfg.file.layout.rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].left.last().map(String::as_str), Some("char"));
    assert!(!rows[0].left.iter().any(|id| cfg.file.modules[id].module_type.as_str() == "time"));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("已写入"), "{err}");
    // No previous file: the rollback removes it.
    assert!(err.contains("回滚：tildy config rollback --to builtin --expect-revision "), "{err}");
    assert!(err.contains("TILDY_CONFIG"), "custom --output differs from the default path: {err}");
}

#[test]
fn existing_file_goes_to_history_and_the_printed_rollback_restores_it() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("config.json");
    let old = "{ \"schema_version\": 1, \"preset\": \"plain\", \"modules\": { \"cwd\": { \"truncation_length\": 5 } } }\n";
    std::fs::write(&out, old).unwrap();
    let o = tildy(&["theme", "--preset", "pastel-powerline", "--output", out.to_str().unwrap()], dir.path());
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    // The key this command does not manage stays, after the managed ones.
    assert_eq!(read_json(&out), json!({"schema_version": 1, "preset": "pastel-powerline", "modules": {"cwd": {"truncation_length": 5}}}));
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.find("\"preset\"").unwrap() < text.find("\"modules\"").unwrap(), "{text}");
    assert_eq!(config::load(Some(&out)).unwrap().file.modules["cwd"].truncation_length, Some(5));
    // Only the persistent transaction lock may sit next to the file; the
    // previous content is a history entry, not a scattered `.bak` file.
    let stray: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("config.json."))
        .filter(|p| p.file_name().unwrap() != "config.json.lock")
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("原文件已存入历史"), "{err}");
    let line = err.lines().find(|l| l.starts_with("回滚：tildy config rollback ")).unwrap_or_else(|| panic!("{err}"));
    let args: Vec<&str> = line.trim_start_matches("回滚：tildy ").split(' ').collect();
    // The printed command, run against this file, restores the old content.
    let r = Command::new(env!("CARGO_BIN_EXE_tildy"))
        .args(&args)
        .arg("--json")
        .env("TILDY_CONFIG", &out)
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(r.status.code(), Some(0), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), old);
    // The second run refuses a stale revision instead of overwriting: simulate
    // by applying with the revision from before the rollback.
    let stale = args[args.iter().position(|a| *a == "--expect-revision").unwrap() + 1];
    let r = Command::new(env!("CARGO_BIN_EXE_tildy"))
        .args(["config", "apply", "--file", out.to_str().unwrap(), "--expect-revision", stale, "--json"])
        .env("TILDY_CONFIG", &out)
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(r.status.code(), Some(3), "{}", String::from_utf8_lossy(&r.stderr));
}

#[test]
fn every_preset_untouched_is_just_the_preset_name() {
    let dir = tempfile::tempdir().unwrap();
    for p in PRESET_INFO {
        let out = dir.path().join(format!("{}.json", p.name));
        let o = tildy(&["theme", "--preset", p.name, "--output", out.to_str().unwrap()], dir.path());
        assert_eq!(o.status.code(), Some(0), "{}: {}", p.name, String::from_utf8_lossy(&o.stderr));
        assert_eq!(read_json(&out), json!({"schema_version": 1, "preset": p.name}));
        assert!(config::load(Some(&out)).is_ok(), "{}", p.name);
    }
}

#[test]
fn default_output_is_the_config_path() {
    let dir = tempfile::tempdir().unwrap();
    let o = tildy(&["theme", "--preset", "jetpack", "--two-line", "--no-duration"], dir.path());
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    let cfg = config::load(Some(&dir.path().join("default-config.json"))).unwrap();
    let rows = &cfg.file.layout.rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].left, ["char"]);
    assert!(rows[0].right.is_empty() && !rows[1].right.is_empty(), "RPROMPT moves to the last row");
    assert!(!rows[0].left.iter().any(|id| id == "duration" || id == "jobs"));
    assert!(!String::from_utf8_lossy(&o.stderr).contains("TILDY_CONFIG"));
}

#[test]
fn newline_flags_override_and_persist_across_preset_changes() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("default-config.json");
    std::fs::write(&out, r#"{"schema_version":1,"preset":"plain","history":{"add_newline":false}}"#).unwrap();
    for (flag, on) in [("--add-newline", true), ("--no-add-newline", false)] {
        let r = tildy(&["theme", "--preset", "pastel-powerline", flag], dir.path());
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
        assert_eq!(read_json(&out)["add_newline"], on);
        assert_eq!(config::load(Some(&out)).unwrap().file.add_newline, Some(on));
        let r = tildy(&["theme", "--preset", "tokyo-night"], dir.path());
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
        assert_eq!(read_json(&out), json!({
            "schema_version": 1, "preset": "tokyo-night", "add_newline": on,
            "history": {"add_newline": false}
        }));
    }
    let before = std::fs::read(&out).unwrap();
    let r = tildy(&["theme", "--preset", "plain", "--add-newline", "--no-add-newline"], dir.path());
    assert_eq!(r.status.code(), Some(2));
    assert_eq!(std::fs::read(&out).unwrap(), before);
}

#[test]
fn invalid_existing_newline_is_not_silently_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("default-config.json");
    let old = r#"{"schema_version":1,"preset":"plain","add_newline":"yes"}"#;
    std::fs::write(&out, old).unwrap();
    let r = tildy(&["theme", "--preset", "pastel-powerline"], dir.path());
    assert!(!r.status.success());
    assert_eq!(std::fs::read_to_string(&out).unwrap(), old);
    let r = tildy(&["theme", "--preset", "pastel-powerline", "--add-newline"], dir.path());
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(read_json(&out)["add_newline"], true);
}

#[test]
fn errors_and_missing_tty() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("config.json");
    let o = out.to_str().unwrap();
    // Interactive mode needs a terminal: exit 4, nothing written.
    let r = tildy(&["theme", "--output", o], dir.path());
    assert_eq!(r.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&r.stderr).contains("TTY"));
    assert_eq!(tildy(&["theme", "--preset", "nope", "--output", o], dir.path()).status.code(), Some(2));
    assert_eq!(tildy(&["theme", "--preset", "plain", "--symbols", "emoji", "--output", o], dir.path()).status.code(), Some(2));
    // plain has no character module: its row count cannot change.
    assert_eq!(tildy(&["theme", "--preset", "plain", "--two-line", "--output", o], dir.path()).status.code(), Some(2));
    assert!(!out.exists());
    // Asking for "off" where the preset has nothing to remove is a note, not an error.
    let r = tildy(&["theme", "--preset", "pure-preset", "--no-time", "--no-langs", "--output", o], dir.path());
    assert_eq!(r.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&r.stderr).contains("--no-time changes nothing"));
    assert_eq!(read_json(&out), json!({"schema_version": 1, "preset": "pure-preset"}));
}
