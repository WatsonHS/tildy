//! Importer tests: golden file for the author's Pastel Powerline config plus edge cases.
use serde_json::{json, Value};
use tildy::import::import_starship;
use std::path::PathBuf;
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/import").join(name)
}

fn author() -> tildy::import::ImportResult {
    let src = std::fs::read_to_string(fixture("author-pastel-powerline.toml")).unwrap();
    import_starship(&src).unwrap()
}

#[test]
fn author_config_matches_golden() {
    let got = author().candidate;
    let want: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture("author-pastel-powerline.expected.json")).unwrap()).unwrap();
    assert_eq!(got, want, "regenerate with: tildy import starship --file fixtures/import/author-pastel-powerline.toml");
}

#[test]
fn author_config_shape() {
    let c = author().candidate;
    assert_eq!(c["schema_version"], 1);
    assert!(c.get("preset").is_none());
    let rows = c["layout"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["style"], "powerline");
    assert_eq!(rows[0]["separator"]["nerd"], "\u{e0b0}");
    assert_eq!(
        rows[0]["left"],
        json!(["user", "cwd", "git", "c", "elixir", "elm", "go", "haskell", "java", "julia", "node", "nim", "rust", "docker", "time"])
    );
    assert_eq!(rows[1], json!({ "left": ["char"] }));
    // Every id in the layout has a module, and every module has a type.
    for r in rows {
        for id in r["left"].as_array().unwrap() {
            assert!(c["modules"].get(id.as_str().unwrap()).is_some(), "missing module {id}");
        }
    }
    let m = &c["modules"];
    assert_eq!(m["user"]["style"]["background"], "#9A348E");
    let glyph = m["user"]["symbol"]["nerd"].as_str().unwrap();
    assert_eq!(glyph.chars().count(), 1);
    assert!(('\u{e000}'..='\u{f8ff}').contains(&glyph.chars().next().unwrap()) || glyph.chars().next().unwrap() > '\u{f0000}');
    assert_eq!(m["user"]["symbol"]["ascii"], "u");
    assert_eq!(m["cwd"]["style"]["background"], "#DA627D");
    assert_eq!(m["cwd"]["truncation_symbol"], "…/");
    assert_eq!(m["git"]["style"]["background"], "#FCA17D");
    assert_eq!(m["git"]["status_symbols"]["ahead"], "⇡");
    assert_eq!(m["node"]["language"], "nodejs");
    assert_eq!(m["node"]["style"]["background"], "#86BBD8");
    assert_eq!(m["docker"]["style"]["background"], "#06969A");
    assert_eq!(m["time"]["format"], "♥ $time");
    assert_eq!(m["time"]["time_format"], "%R");
    assert_eq!(m["time"]["style"]["background"], "#33658A");
    assert_eq!(m["char"]["success_style"]["foreground"], "green");
    assert_eq!(m["char"]["error_style"]["foreground"], "red");
    assert_eq!(m["elixir"], json!({ "type": "starship", "module": "elixir", "detect_files": ["mix.exs"], "style": {"background":"#86BBD8"} }));
    // No format override survives where the tildy default is equivalent.
    for id in ["c", "go", "node", "rust", "java", "docker", "cwd", "git"] {
        assert!(m[id].get("format").is_none(), "{id} should use the default format");
    }
}

#[test]
fn author_config_candidate_loading_is_reported() {
    tildy::config::from_str(&author().candidate.to_string()).unwrap();
}

#[test]
fn empty_format() {
    let r = import_starship("format = \"\"\n").unwrap();
    assert_eq!(r.candidate["layout"]["rows"], json!([{ "left": [] }]));
    assert!(r.candidate["modules"].as_object().unwrap().is_empty());
}

#[test]
fn disabled_modules_skipped() {
    let r = import_starship("format = \"$directory$nodejs\"\n[nodejs]\ndisabled = true\n").unwrap();
    assert_eq!(r.candidate["layout"]["rows"], json!([{ "left": ["cwd"] }]));
    assert_eq!(r.skipped, vec!["nodejs"]);
}

#[test]
fn unknown_module_is_bridged() {
    let r = import_starship("format = \"$directory$kubernetes\"\n").unwrap();
    assert_eq!(r.candidate["modules"]["kubernetes"], json!({ "type": "starship", "module": "kubernetes" }));
}

#[test]
fn no_powerline_keeps_plain_rows() {
    let r = import_starship("format = \"$username$directory\\n$character\"\n[username]\nshow_always = true\n").unwrap();
    let rows = &r.candidate["layout"]["rows"];
    assert_eq!(rows, &json!([{ "left": ["user", "cwd"] }, { "left": ["char"] }]));
    assert!(rows[0].get("style").is_none());
}

#[test]
fn unsupported_options_are_listed() {
    // read_only and fish_style_pwd_dir_length are mapped since M2 (task E1).
    let r = import_starship("format = \"$directory$git_branch\"\n[directory]\nread_only = \"x\"\nfish_style_pwd_dir_length = 2\n[git_branch]\nignore_bare_repo = true\n[git_status]\nwindows_starship = \"/x\"\n").unwrap();
    let items: Vec<_> = r.unsupported.iter().map(|u| u.item.as_str()).collect();
    assert!(items.contains(&"git_branch.ignore_bare_repo"));
    assert!(items.contains(&"git_status.windows_starship"));
    assert!(!items.contains(&"directory.read_only") && !items.contains(&"directory.fish_style_pwd_dir_length"));
}

// ---- CLI

fn tildy(args: &[&str], cfg: &std::path::Path, home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tildy"))
        .args(args)
        .env("TILDY_CONFIG", cfg)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("STARSHIP_CONFIG")
        .output()
        .unwrap()
}

#[test]
fn cli_prints_valid_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let out = tildy(
        &["import", "starship", "--file", fixture("author-pastel-powerline.toml").to_str().unwrap()],
        &dir.path().join("config.json"),
        dir.path(),
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("stdout is the candidate JSON");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn cli_write_and_conflict() {
    // A hostname-only config validates under both the old and the extended format.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("starship.toml");
    std::fs::write(&src, "format = \"$hostname\"\n[hostname]\nssh_only = true\nstyle = \"bold red\"\n").unwrap();
    let cfg = dir.path().join("sub/config.json");
    let args = ["import", "starship", "--file", src.to_str().unwrap(), "--write"];
    let out = tildy(&args, &cfg, dir.path());
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(written["modules"]["host"]["type"], "hostname");
    assert!(tildy::config::load(Some(&cfg)).is_ok());

    let out = tildy(&args, &cfg, dir.path());
    assert_eq!(out.status.code(), Some(3));
    // The existing file is untouched.
    assert_eq!(serde_json::from_str::<Value>(&std::fs::read_to_string(&cfg).unwrap()).unwrap(), written);
}

#[test]
fn cli_json_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("starship.toml");
    std::fs::write(&src, "format = \"$hostname\"\n").unwrap();
    let out = tildy(&["import", "starship", "--file", src.to_str().unwrap(), "--json"], &dir.path().join("c.json"), dir.path());
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert!(v["data"]["candidate"].is_object());
    assert!(v["data"]["unsupported"].is_array());
    assert_eq!(v["data"]["source"], src.to_str().unwrap());
}

#[test]
fn cli_missing_source_file() {
    let dir = tempfile::tempdir().unwrap();
    let out = tildy(&["import", "starship", "--file", "/nonexistent/starship.toml"], &dir.path().join("c.json"), dir.path());
    assert_eq!(out.status.code(), Some(4));
}

#[test]
fn report_classifies_loss_and_bridges_pin_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("chosen.toml");
    let text = "format = '$elixir$docker_context$custom.foo'\nright_format = '${custom.bar}'\nscan_timeout = 50\n[elixir]\ndetect_files = ['special.ex']\n[docker_context]\nformat = '$context $path'\n";
    std::fs::write(&src, text).unwrap();
    let out = tildy(&["import", "starship", "--file", src.to_str().unwrap(), "--json"], &dir.path().join("config.json"), dir.path());
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let report = &v["data"]["report"];
    assert_eq!(report["bridged"][0]["module"], "elixir");
    assert_eq!(report["bridged"][0]["config"], src.canonicalize().unwrap().to_str().unwrap());
    assert_eq!(report["ignored"][0]["item"], "scan_timeout");
    let manual: Vec<_> = report["manual"].as_array().unwrap().iter().map(|v| v["item"].as_str().unwrap()).collect();
    for item in ["docker_context.format", "format: $custom.foo", "format: $custom.bar"] { assert!(manual.contains(&item), "{manual:?}"); }
    let candidate = &v["data"]["candidate"];
    assert_eq!(candidate["modules"]["elixir"]["starship_config"], report["bridged"][0]["config"]);
    assert!(candidate["modules"]["elixir"].get("detect_files").is_none(), "custom detection is delegated to Starship");
    assert!(candidate["modules"].as_object().unwrap().values().all(|m| m["type"] != "command" && m["module"].as_str().is_none_or(|s| !s.starts_with("custom"))));
    assert_eq!(std::fs::read_to_string(src).unwrap(), text);
}

#[test]
fn import_obeys_shared_lock_and_concurrent_writes_have_one_winner() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.toml");
    let cfg = dir.path().join("config.json");
    std::fs::write(&src, "format = '$directory'").unwrap();
    let lock = std::fs::File::create(dir.path().join("config.json.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let args = ["import", "starship", "--write", "--json", "--file", src.to_str().unwrap()];
    let start = std::time::Instant::now();
    let out = tildy(&args, &cfg, dir.path());
    assert_eq!(out.status.code(), Some(4));
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["error"]["message"].as_str().unwrap().starts_with("config locked"));
    assert!(!cfg.exists());
    drop(lock);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..6).map(|_| scope.spawn(|| tildy(&args, &cfg, dir.path()))).collect();
        handles.into_iter().map(|h| h.join().unwrap().status.code().unwrap()).collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|&&code| code == 0).count(), 1, "{results:?}");
    assert!(results.iter().all(|code| matches!(code, 0 | 3)));
    assert_eq!(std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o111, 0);
    tildy::config::load(Some(&cfg)).unwrap();
}

#[test]
fn braced_modules_and_punctuation_keep_their_meaning() {
    let result = import_starship("format = '${directory}.$character'\n").unwrap();
    assert_eq!(result.candidate["modules"]["cwd"]["type"], "directory");
    assert_eq!(result.candidate["modules"]["text1"]["text"], ".");
    let result = import_starship("format = '$directory.'\n").unwrap();
    assert_eq!(result.candidate["modules"]["cwd"]["type"], "directory");
    assert_eq!(result.candidate["modules"]["text1"]["text"], ".");
}
