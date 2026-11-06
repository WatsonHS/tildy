//! `tildy preset list|show`, `capabilities --json` presets, and loading every
//! preset through `config::load` from a one-line user file.
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn tildy(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tildy"))
        .args(args)
        .current_dir(cwd)
        .env("COLUMNS", "100")
        .env("TILDY_CONFIG", cwd.join("no-such-config.json"))
        .env("XDG_STATE_HOME", cwd.join("state"))
        .output()
        .unwrap()
}

fn names() -> Vec<&'static str> {
    tildy::config::PRESET_INFO.iter().map(|p| p.name).collect()
}

#[test]
fn list_json_has_every_preset_with_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let out = tildy(&["preset", "list", "--json"], dir.path());
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    let list = v["data"].as_array().unwrap();
    assert_eq!(list.iter().map(|e| e["name"].as_str().unwrap()).collect::<Vec<_>>(), names());
    for e in list {
        for k in ["description", "nerd_font", "symbols", "source"] {
            assert!(e[k].as_str().is_some_and(|s| !s.is_empty()), "{e}: {k}");
        }
    }
    let starship: Vec<_> = list.iter().filter(|e| e["source"].as_str().unwrap().starts_with("starship 1.24.2")).collect();
    assert_eq!(starship.len(), 15, "12 Starship presets + 3 extra catppuccin flavors");
    let get = |n: &str| list.iter().find(|e| e["name"] == n).unwrap().clone();
    assert_eq!(get("no-nerd-font")["symbols"], "unicode");
    assert_eq!(get("plain-text-symbols")["symbols"], "ascii");
    assert_eq!(get("tokyo-night")["nerd_font"], "required");
    let text = tildy(&["preset", "list"], dir.path());
    assert!(String::from_utf8_lossy(&text.stdout).contains("gruvbox-rainbow"));
}

#[test]
fn show_all_renders_every_preset_in_each_tier() {
    let dir = tempfile::tempdir().unwrap();
    for tier in ["nerd", "unicode", "ascii"] {
        let out = tildy(&["preset", "show", "--all", "--symbols", tier], dir.path());
        assert_eq!(out.status.code(), Some(0), "{tier}: {}", String::from_utf8_lossy(&out.stderr));
        let s = String::from_utf8(out.stdout).unwrap();
        for n in names() {
            assert!(s.contains(&format!("── {n}")), "{tier}: missing header for {n}");
        }
        // Zsh escapes are expanded to ANSI.
        assert!(!s.contains("%F{") && !s.contains("%K{") && !s.contains("%f"), "{tier}");
        assert!(s.contains("\x1b[38;2;"), "{tier}: 24-bit colors expected");
    }
    // Without --symbols every preset uses its own tier: nerd presets keep their glyphs.
    let out = tildy(&["preset", "show", "tokyo-night"], dir.path());
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains('\u{e0b4}'));
}

#[test]
fn show_here_uses_the_real_directory() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("demo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| Command::new("git").args(args).current_dir(&repo).output().unwrap();
    if !git(&["init", "-q", "-b", "trunk"]).status.success() {
        eprintln!("git unavailable; skipped");
        return;
    }
    std::fs::write(repo.join("package.json"), "{}").unwrap();
    let out = tildy(&["preset", "show", "pure-preset", "--here", "--symbols", "ascii"], &repo);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("trunk"), "{s}");
    assert!(s.contains("demo"), "{s}");
}

#[test]
fn show_rejects_bad_input() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(tildy(&["preset", "show", "neon"], dir.path()).status.code(), Some(2));
    assert_eq!(tildy(&["preset", "show"], dir.path()).status.code(), Some(2));
    assert_eq!(tildy(&["preset", "show", "plain", "--symbols", "emoji"], dir.path()).status.code(), Some(2));
}

#[test]
fn capabilities_lists_presets() {
    let dir = tempfile::tempdir().unwrap();
    let out = tildy(&["capabilities", "--json"], dir.path());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let p = v["data"]["presets"].as_array().unwrap();
    assert_eq!(p.iter().map(|e| e["name"].as_str().unwrap()).collect::<Vec<_>>(), names());
    assert!(v["data"]["module_types"].as_array().unwrap().iter().any(|t| t == "text"));
}

#[test]
fn capabilities_lists_every_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let out = tildy(&["capabilities", "--json"], dir.path());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let cmds = v["data"]["commands"].as_array().unwrap();
    let names: Vec<&str> = cmds.iter().map(|c| c["name"].as_str().unwrap()).collect();
    for n in ["init", "render", "worker", "capabilities", "inspect", "import", "preset", "palette", "theme", "config", "schema", "agent", "preview"] {
        assert!(names.contains(&n), "{n} missing from {names:?}");
    }
    assert!(!names.contains(&"help"));
    assert!(!names.contains(&"configure"), "only the canonical theme command is advertised");
    let config = cmds.iter().find(|c| c["name"] == "config").unwrap();
    assert_eq!(config["json"], true);
    assert_eq!(config["subcommands"], serde_json::json!(["get", "validate", "apply", "rollback", "history"]));
    assert!(config["about"].as_str().unwrap().contains("revision"));
    let text = tildy(&["capabilities"], dir.path());
    assert!(String::from_utf8_lossy(&text.stdout).contains("commands: init, render"));
}

#[test]
fn every_preset_loads_from_a_one_line_user_file() {
    let dir = tempfile::tempdir().unwrap();
    for n in names() {
        let f = dir.path().join(format!("{n}.json"));
        std::fs::write(&f, format!(r#"{{"schema_version":1,"preset":"{n}"}}"#)).unwrap();
        let c = tildy::config::load(Some(&f)).unwrap_or_else(|e| panic!("{n}: {e:#}"));
        assert_eq!(c.file.preset.as_deref(), Some(n));
        assert!(!c.layout_modules().is_empty(), "{n}");
        // Providers build for every placed module.
        let (_, problems) = tildy::providers::from_config_checked(&c);
        assert!(problems.is_empty(), "{n}: {problems:?}");
    }
}
