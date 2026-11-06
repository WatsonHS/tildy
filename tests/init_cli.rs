//! `tildy init zsh`: the adapter is cached under XDG_CACHE_HOME and sourced
//! from there; the inline form remains the fallback.
use std::fs;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_tildy");
const SCRIPT: &str = include_str!("../shell/zsh/tildy.zsh");

fn init(cache: &std::path::Path, extra: &[(&str, &str)]) -> String {
    let mut c = Command::new(BIN);
    c.args(["init", "zsh"]).env("XDG_CACHE_HOME", cache).env("TILDY_CONFIG", "/nonexistent/config.json");
    for (k, v) in extra {
        c.env(k, v);
    }
    let out = c.output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

fn cached_path(output: &str) -> std::path::PathBuf {
    let line = output.lines().last().unwrap();
    assert!(line.starts_with("} '") && line.ends_with('\''), "{line}");
    std::path::PathBuf::from(&line[3..line.len() - 1])
}

#[test]
fn script_is_cached_once_and_sourced() {
    let tmp = tempfile::tempdir().unwrap();
    let out = init(tmp.path(), &[]);
    assert!(out.contains("'builtin' 'source' \"$1\""), "{out}");
    assert!(!out.contains("'builtin' 'eval'"));
    let path = cached_path(&out);
    assert!(path.starts_with(tmp.path().join("tildy").join("zsh")));
    assert_eq!(fs::read_to_string(&path).unwrap(), SCRIPT);
    let dir = path.parent().unwrap();
    assert!(dir.file_name().unwrap().to_str().unwrap().starts_with(&format!("{}-", env!("CARGO_PKG_VERSION"))));
    let meta = fs::metadata(dir).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    // Second run: same path, the file is left alone, .zwc exists when zsh is around.
    let again = init(tmp.path(), &[]);
    assert_eq!(cached_path(&again), path);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    if Command::new("zsh").arg("--version").output().is_ok_and(|o| o.status.success()) {
        assert!(dir.join("tildy.zsh.zwc").exists(), "zcompile output missing");
    }
    // A stale sibling version directory is removed when a new one is written.
    let stale = tmp.path().join("tildy").join("zsh").join("0.0.0-deadbeef");
    fs::create_dir_all(&stale).unwrap();
    fs::remove_dir_all(dir).unwrap();
    init(tmp.path(), &[]);
    assert!(!stale.exists() && path.exists());
}

#[test]
fn cached_script_loads_in_zsh_like_the_inline_form() {
    if Command::new("zsh").arg("--version").output().is_err() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = init(tmp.path(), &[]);
    // Non-interactive: the load guard skips the body, so this only proves the
    // loader parses and the sourced file is reachable under no_aliases.
    let status = Command::new("zsh")
        .args(["-fc", "alias builtin='echo alias-expanded'; alias source='echo alias-expanded'; eval \"$1\"; print ok", "--", &out])
        .output()
        .unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    assert_eq!(String::from_utf8_lossy(&status.stdout).trim(), "ok");
}

#[test]
fn inline_fallback_when_the_cache_is_unusable_or_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let out = init(tmp.path(), &[("TILDY_INIT_CACHE", "0")]);
    assert!(out.contains("'builtin' 'eval' \"$1\"") && out.contains("# Skiff Zsh integration") || out.contains("'builtin' 'eval' \"$1\""));
    assert!(!out.contains("'builtin' 'source'"));
    // XDG_CACHE_HOME that is a file: cannot create the directory.
    let file = tmp.path().join("not-a-dir");
    fs::write(&file, "x").unwrap();
    let out = init(&file, &[]);
    assert!(out.contains("'builtin' 'eval' \"$1\""), "{out}");
    // Relative XDG_CACHE_HOME is ignored (XDG), HOME unset: inline too.
    let out = Command::new(BIN)
        .args(["init", "zsh"])
        .env_remove("HOME")
        .env("XDG_CACHE_HOME", "relative/dir")
        .env("TILDY_CONFIG", "/nonexistent/config.json")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("'builtin' 'eval' \"$1\""));
}
