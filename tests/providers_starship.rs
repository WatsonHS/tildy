//! Starship bridge provider: real `starship` when installed, fake binaries for failure modes.
use tildy::model::{Freshness, PromptRequest};
use tildy::providers::starship::StarshipProvider;
use tildy::providers::{Provider, ProviderValue};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

fn fixture() -> (MutexGuard<'static, ()>, tempfile::TempDir) {
    // These tests create executables while other tests fork. On Linux a child
    // can briefly inherit another thread's writable script fd before exec
    // closes it, producing ETXTBSY. Serialize fixture writes and spawns (also
    // protecting this suite's STARSHIP_CONFIG mutation), not provider behavior.
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    (guard, tempfile::tempdir().unwrap())
}

fn req(cwd: &Path) -> PromptRequest {
    PromptRequest {
        gen: 1,
        session_id: "t".into(),
        cwd: cwd.to_path_buf(),
        cols: 80,
        lines: 24,
        last_exit: 0,
        last_duration_ms: 0,
        env: String::new(),
        jobs: 0,
    }
}

fn module(v: ProviderValue) -> (String, tildy::model::Slot<tildy::model::ModuleValue>) {
    match v {
        ProviderValue::Module(id, slot) => (id, slot),
        other => panic!("unexpected {other:?}"),
    }
}

fn script(dir: &Path, name: &str, body: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

// Functional assertions should not depend on a 500 ms process-start budget
// while the test runner is busy; the timeout test overrides this explicitly.
fn provider(id: String, module: String, bin: String) -> StarshipProvider {
    StarshipProvider::with_binary(id, module, bin).with_timeout(Duration::from_secs(2))
}

fn have_starship() -> bool {
    std::process::Command::new("starship").arg("--version").output().is_ok()
}

#[test]
fn pinned_source_overrides_environment_and_missing_source_is_an_error() {
    let (_fixture, dir) = fixture();
    let source = dir.path().join("source.toml");
    std::fs::write(&source, "").unwrap();
    let bin = script(dir.path(), "show-config", "printf '%s' \"$STARSHIP_CONFIG\"");
    let p = provider("bridge".into(), "elixir".into(), bin)
        .with_config(Some(source.clone())).with_timeout(Duration::from_secs(2));
    assert_eq!(p.watch_paths(&req(dir.path())), vec![source.clone()]);
    let (_, slot) = module(p.fetch(&req(dir.path())));
    assert_eq!(slot.freshness, Freshness::Fresh);
    assert_eq!(slot.value.unwrap().text, source.to_str().unwrap());
    std::fs::remove_file(source).unwrap();
    let (_, slot) = module(p.fetch(&req(dir.path())));
    assert_eq!(slot.freshness, Freshness::Error);
    assert!(slot.error.unwrap().contains("missing"));
}

#[test]
fn real_starship_renders_directory_and_time() {
    let (_fixture, dir) = fixture();
    if !have_starship() {
        eprintln!("skipped: starship is not installed");
        return;
    }
    let work = dir.path().join("proj");
    std::fs::create_dir(&work).unwrap();
    let cfg = dir.path().join("starship.toml");
    std::fs::write(
        &cfg,
        "format = \"$directory$time\"\n[directory]\nstyle = \"bold cyan\"\ntruncation_length = 1\n\
         [time]\ndisabled = false\ntime_format = \"%R\"\nformat = \"[ 100% $time ](bg:#33658A fg:#ffffff)\"\n",
    )
    .unwrap();
    // One test mutates the process environment; no other test here reads STARSHIP_CONFIG.
    std::env::set_var("STARSHIP_CONFIG", &cfg);

    let p = StarshipProvider::new("cwd_bridge".into(), "directory".into());
    assert_eq!(p.spec().key, "cwd_bridge");
    let (id, slot) = module(p.fetch(&req(&work)));
    assert_eq!(id, "cwd_bridge");
    assert_eq!(slot.freshness, Freshness::Fresh);
    let v = slot.value.expect("directory renders");
    let (zsh, cols) = v.prerendered.expect("prerendered");
    assert!(v.text.contains("proj"), "text: {:?}", v.text);
    assert!(zsh.starts_with("%B%F{6}"), "zsh: {zsh:?}");
    assert!(zsh.contains("proj"), "zsh: {zsh:?}");
    assert!(zsh.bytes().all(|b| b >= 0x20), "control byte in {zsh:?}");
    assert!(!v.text.contains('\x1b'));
    assert_eq!(cols, unicode_width_of(&v.text));
    assert!((4..40).contains(&cols), "cols {cols}");

    let p = StarshipProvider::new("clock".into(), "time".into());
    let (_, slot) = module(p.fetch(&req(&work)));
    let v = slot.value.expect("time renders");
    let (zsh, cols) = v.prerendered.unwrap();
    assert!(zsh.contains("100%%"), "percent must be doubled: {zsh:?}");
    assert!(zsh.contains("%K{#33658a}"), "24-bit bg kept: {zsh:?}");
    assert!(zsh.contains("%F{#ffffff}"), "24-bit fg kept: {zsh:?}");
    assert!(zsh.bytes().all(|b| b >= 0x20));
    // " 100% HH:MM " is 12 columns plus nothing else visible.
    assert_eq!(cols, 12, "text {:?}", v.text);
}

fn unicode_width_of(s: &str) -> usize {
    // Test-local width: inputs are ASCII here.
    assert!(s.is_ascii());
    s.len()
}

#[test]
fn missing_binary_is_an_error_slot() {
    let (_fixture, dir) = fixture();
    let p = provider("x".into(), "elixir".into(), "/nonexistent/starship-bin".into());
    let (_, slot) = module(p.fetch(&req(dir.path())));
    assert_eq!(slot.freshness, Freshness::Error);
    assert!(slot.value.is_none());
    assert!(slot.error.unwrap().contains("not found"));
}

#[test]
fn empty_output_is_fresh_none() {
    let (_fixture, dir) = fixture();
    let bin = script(dir.path(), "fake", "printf ''");
    let p = provider("x".into(), "elixir".into(), bin);
    let (_, slot) = module(p.fetch(&req(dir.path())));
    assert_eq!(slot.freshness, Freshness::Fresh);
    assert!(slot.value.is_none());
    assert!(slot.error.is_none());
}

#[test]
fn whitespace_only_output_is_hidden() {
    let (_fixture, dir) = fixture();
    let bin = script(dir.path(), "fake", "printf '\\033[1m  \\033[0m\\n'");
    let p = provider("x".into(), "elixir".into(), bin);
    let (_, slot) = module(p.fetch(&req(dir.path())));
    assert!(slot.value.is_none());
}

#[test]
fn timeout_kills_the_process_group() {
    let (_fixture, dir) = fixture();
    let marker = dir.path().join("survived");
    let bin = script(dir.path(), "fake", &format!("(sleep 2; touch {}) &\nsleep 5", marker.display()));
    let p = provider("x".into(), "elixir".into(), bin).with_timeout(Duration::from_millis(100));
    let t = Instant::now();
    let (_, slot) = module(p.fetch(&req(dir.path())));
    assert!(t.elapsed() < Duration::from_millis(1500), "took {:?}", t.elapsed());
    assert_eq!(slot.freshness, Freshness::Timeout);
    assert!(slot.value.is_none());
    std::thread::sleep(Duration::from_millis(2300));
    assert!(!marker.exists(), "background child outlived the timeout");
}

#[test]
fn nonzero_exit_is_error() {
    let (_fixture, dir) = fixture();
    let bin = script(dir.path(), "fake", "echo boom >&2; exit 3");
    let p = provider("x".into(), "elixir".into(), bin);
    let (_, slot) = module(p.fetch(&req(dir.path())));
    assert_eq!(slot.freshness, Freshness::Error);
    let error = slot.error.unwrap();
    assert!(error.contains("boom"), "unexpected error: {error}");
}

#[test]
fn passes_module_name_and_shell_env_and_cwd() {
    let (_fixture, dir) = fixture();
    let bin = script(dir.path(), "fake", "printf '%s|%s|%s|%s' \"$1\" \"$2\" \"$STARSHIP_SHELL\" \"$(pwd -P)\"");
    let p = provider("x".into(), "elixir".into(), bin);
    let (_, slot) = module(p.fetch(&req(dir.path())));
    assert!(slot.value.is_some(), "module output missing: {slot:?}");
    let text = slot.value.unwrap().text;
    let want_cwd = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(text, format!("module|elixir|zsh|{}", want_cwd.display()));
}

#[test]
fn bad_cwd_is_error() {
    let p = provider("x".into(), "elixir".into(), "true".into());
    let (_, slot) = module(p.fetch(&req(Path::new("/nonexistent/dir"))));
    assert_eq!(slot.freshness, Freshness::Error);
}

#[test]
fn detect_lists_gate_the_spawn() {
    let (_fixture, dir) = fixture();
    let ran = dir.path().join("ran");
    let bin = script(dir.path(), "fake", &format!("touch {}; printf hi", ran.display()));
    let mk = || {
        provider("x".into(), "elixir".into(), bin.clone())
            .with_detect(vec!["mix.exs".into()], vec!["ex".into()], vec!["deps".into()])
    };
    let work = dir.path().join("w");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("README"), "").unwrap();
    let (_, slot) = module(mk().fetch(&req(&work)));
    assert_eq!(slot.freshness, Freshness::Fresh);
    assert!(slot.value.is_none());
    assert!(!ran.exists(), "starship must not be spawned without a match");

    for (name, dirp) in [("mix.exs", false), ("a.ex", false), ("deps", true)] {
        let w = dir.path().join(format!("m-{name}"));
        std::fs::create_dir(&w).unwrap();
        if dirp { std::fs::create_dir(w.join(name)).unwrap() } else { std::fs::write(w.join(name), "").unwrap() }
        let (_, slot) = module(mk().fetch(&req(&w)));
        assert_eq!(slot.value.expect(name).text, "hi");
    }
    assert!(ran.exists());
}
