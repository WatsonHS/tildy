//! `tildy palette list|show`, `tildy preset show --palette`, `tildy theme
//! --palette` and the palettes in `capabilities --json` (M2 task E2).
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

fn tildy(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tildy"))
        .args(args)
        .current_dir(cwd)
        .env("COLUMNS", "100")
        .env("TILDY_LANG", "en")
        .env("TILDY_CONFIG", cwd.join("no-such-config.json"))
        .env("XDG_STATE_HOME", cwd.join("state"))
        .output()
        .unwrap()
}

fn json_of(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&o.stdout)))
}

fn names() -> Vec<&'static str> {
    tildy::config::PALETTE_INFO.iter().map(|p| p.name).collect()
}

#[test]
fn list_names_every_builtin_palette() {
    let dir = tempfile::tempdir().unwrap();
    let o = tildy(&["palette", "list", "--json"], dir.path());
    assert_eq!(o.status.code(), Some(0));
    let v = json_of(&o);
    let list = v["data"].as_array().unwrap();
    assert_eq!(list.iter().map(|e| e["name"].as_str().unwrap()).collect::<Vec<_>>(), names());
    for want in ["dracula", "nord", "solarized-dark", "solarized-light", "one-dark", "rose-pine", "everforest", "kanagawa", "gruvbox-dark",
        "tokyo-night", "catppuccin-latte", "catppuccin-frappe", "catppuccin-macchiato", "catppuccin-mocha"]
    {
        assert!(names().contains(&want), "{want}");
    }
    for e in list {
        assert!(["dark", "light"].contains(&e["variant"].as_str().unwrap()), "{e}");
        assert!(e["source"].as_str().unwrap().starts_with("https://"), "{e}");
    }
    let text = String::from_utf8(tildy(&["palette", "list"], dir.path()).stdout).unwrap();
    assert!(text.contains("kanagawa") && text.contains("\x1b[48;2;"), "swatches in 24-bit color");
}

#[test]
fn show_prints_swatches_and_json() {
    let dir = tempfile::tempdir().unwrap();
    let o = tildy(&["palette", "show", "nord"], dir.path());
    assert_eq!(o.status.code(), Some(0));
    let s = String::from_utf8(o.stdout).unwrap();
    assert!(s.contains("nord8") && s.contains("#88C0D0") && s.contains("\x1b[48;2;136;192;208m"), "{s}");
    let v = json_of(&tildy(&["palette", "show", "catppuccin-mocha", "--json"], dir.path()));
    let colors = v["data"]["colors"].as_array().unwrap();
    assert_eq!(colors[0], json!({"name": "bg1", "value": "#cba6f7", "role": true}));
    assert!(colors.iter().any(|c| c["name"] == "rosewater" && c["role"] == false));
    assert_eq!(v["data"]["missing_roles"], json!([]));
    assert_eq!(tildy(&["palette", "show", "neon"], dir.path()).status.code(), Some(2));
}

#[test]
fn show_finds_user_palettes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("no-such-config.json"), r##"{"schema_version":1,"palettes":{"mine":{"accent":"#123456"},"nord":{"bg1":"#000001"}}}"##)
        .unwrap();
    let v = json_of(&tildy(&["palette", "show", "mine", "--json"], dir.path()));
    assert_eq!(v["data"]["builtin"], false);
    assert_eq!(v["data"]["colors"], json!([{"name": "accent", "value": "#123456", "role": true}]));
    assert_eq!(v["data"]["missing_roles"].as_array().unwrap().len(), 13);
    // A user palette named like a built-in one merges over it.
    let v = json_of(&tildy(&["palette", "show", "nord", "--json"], dir.path()));
    assert_eq!(v["data"]["colors"][0], json!({"name": "bg1", "value": "#000001", "role": true}));
}

#[test]
fn preset_show_recolors_role_presets() {
    let dir = tempfile::tempdir().unwrap();
    let o = tildy(&["preset", "show", "powerline", "--palette", "nord", "--symbols", "nerd"], dir.path());
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    let s = String::from_utf8(o.stdout).unwrap();
    assert!(s.contains("palette nord"), "{s}");
    assert!(s.contains("\x1b[48;2;191;97;106m"), "the directory segment is nord11 (bg2): {s}");
    let default = String::from_utf8(tildy(&["preset", "show", "powerline", "--symbols", "nerd"], dir.path()).stdout).unwrap();
    assert!(default.contains("palette catppuccin-mocha"));
    // --all recolors the role-based presets and leaves the others alone.
    let all = String::from_utf8(tildy(&["preset", "show", "--all", "--palette", "dracula"], dir.path()).stdout).unwrap();
    assert_eq!(all.matches("palette dracula").count(), tildy::config::ROLE_PRESETS.len(), "{all}");
    assert_eq!(tildy(&["preset", "show", "lean", "--palette", "neon"], dir.path()).status.code(), Some(2));
    assert_eq!(tildy(&["preset", "show", "pastel-powerline", "--palette", "nord"], dir.path()).status.code(), Some(2));
    let list = json_of(&tildy(&["preset", "list", "--json"], dir.path()));
    let get = |n: &str| list["data"].as_array().unwrap().iter().find(|e| e["name"] == n).unwrap().clone();
    assert_eq!(get("classic")["palette"], "one-dark");
    assert_eq!(get("pastel-powerline")["palette"], Value::Null);
}

#[test]
fn configure_writes_the_palette_only_when_it_differs() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("c.json");
    let read = || serde_json::from_str::<Value>(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let o = tildy(&["theme", "--preset", "rainbow", "--palette", "nord", "--output", out.to_str().unwrap()], dir.path());
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(read(), json!({"schema_version": 1, "preset": "rainbow", "palette": "nord"}));
    let o = tildy(&["theme", "--preset", "rainbow", "--palette", "gruvbox-dark", "--output", out.to_str().unwrap()], dir.path());
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(read(), json!({"schema_version": 1, "preset": "rainbow"}), "the preset's default palette is not written");
    let c = tildy::config::load(Some(&out)).unwrap();
    assert_eq!(c.file.palette.as_deref(), Some("gruvbox-dark"));
    for bad in [["--preset", "rainbow", "--palette", "neon"], ["--preset", "jetpack", "--palette", "nord"]] {
        let mut args = vec!["theme"];
        args.extend(bad);
        args.extend(["--output", out.to_str().unwrap()]);
        assert_eq!(tildy(&args, dir.path()).status.code(), Some(2), "{bad:?}");
    }
}

#[test]
fn capabilities_lists_palettes_and_roles() {
    let dir = tempfile::tempdir().unwrap();
    let v = json_of(&tildy(&["capabilities", "--json"], dir.path()));
    let p = v["data"]["palettes"].as_array().unwrap();
    assert_eq!(p.iter().map(|e| e["name"].as_str().unwrap()).collect::<Vec<_>>(), names());
    assert_eq!(v["data"]["palette_roles"].as_array().unwrap().len(), tildy::config::ROLES.len());
    let presets = v["data"]["presets"].as_array().unwrap();
    assert_eq!(presets.iter().find(|e| e["name"] == "lean").unwrap()["palette"], "tokyo-night");
}
