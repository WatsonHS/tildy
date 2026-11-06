//! Config-driven module providers (M1b task B): `providers::from_config`,
//! detection, version extraction, the version-command cache and coalescing.
//! Version commands are fake shell scripts addressed by absolute path, so the
//! process PATH is never touched. Timings: `-- --nocapture measure`.

use serde_json::json;
use tildy::config::Config;
use tildy::model::{Dependency, Freshness, ModuleValue, PromptRequest, Slot};
use tildy::providers::modules::{self, CacheUse, Detect, ModuleProvider};
use tildy::providers::{from_config_checked, Provider, ProviderValue};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// A validated config from a user document (merged over builtin + preset).
fn config(json: serde_json::Value) -> Config {
    tildy::config::from_str(&json.to_string()).expect("valid config")
}

fn req(cwd: &Path, env: &[(&str, &str)]) -> PromptRequest {
    PromptRequest {
        gen: 1,
        session_id: "t".into(),
        cwd: cwd.to_path_buf(),
        cols: 80,
        lines: 24,
        last_exit: 0,
        last_duration_ms: 0,
        env: tildy::protocol::encode_env(env.iter().copied()),
        jobs: 0,
    }
}

/// `<dir>/<name>`: a script that appends a line to `<dir>/<name>.calls` and
/// then runs `body`.
fn script(dir: &Path, name: &str, body: &str) -> String {
    let p = dir.join(name);
    let log = dir.join(format!("{name}.calls"));
    std::fs::write(&p, format!("#!/bin/sh\necho x >> '{}'\n{body}\n", log.display())).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

fn calls(dir: &Path, name: &str) -> usize {
    std::fs::read_to_string(dir.join(format!("{name}.calls"))).map(|s| s.lines().count()).unwrap_or(0)
}

fn version_cmd(id: &str, argv0: &str, detect: Detect, ttl_s: u64, timeout_ms: u64) -> ModuleProvider {
    ModuleProvider::command(
        id,
        vec![argv0.to_string(), "--version".into()],
        detect,
        Some(regex::Regex::new(r"v?(\d+\.\d+\.\d+)").unwrap()),
        Duration::from_secs(ttl_s),
        Duration::from_millis(timeout_ms),
    )
}

fn node_detect() -> Detect {
    Detect::from_static(&["package.json", ".nvmrc"], &["js"], &[])
}

fn text(s: &Slot<ModuleValue>) -> Option<&str> {
    s.value.as_ref().map(|v| v.text.as_str())
}

#[test]
fn from_config_builds_one_provider_per_placed_module() {
    let mut c = config(json!({
        "schema_version": 1,
        "modules": {
            "node": {"type": "language", "language": "nodejs"},
            "rs": {"type": "language", "language": "rust", "timeout_ms": 900},
            "py": {"type": "python"},
            "pylang": {"type": "language", "language": "python"},
            "dk": {"type": "docker_context"},
            "kube": {"type": "command", "argv": ["kubectl", "config", "current-context"],
                     "detect_files": ["k8s.yaml"], "detect_extensions": [".yaml"], "detect_folders": ["charts"],
                     "regex": "(\\S+)", "timeout_ms": 300, "cache_ttl_s": 60},
            "star": {"type": "starship", "module": "elixir"},
            "unused": {"type": "language", "language": "golang"},
            "broken": {"type": "language", "language": "nodejs"},
            "badre": {"type": "command", "argv": ["x"]},
            "clock": {"type": "time"}
        },
        "layout": {"rows": [
            {"left": ["cwd", "git", "node", "rs", "py", "pylang", "dk", "kube"], "right": ["clock"]},
            {"left": ["star", "broken", "badre", "node"]}
        ]}
    }));
    // Values the validator would reject, to exercise from_config's own checks.
    c.json["modules"]["broken"]["language"] = json!("cobol");
    c.json["modules"]["badre"]["regex"] = json!("(");
    let (providers, problems) = from_config_checked(&c);
    let keys: Vec<&str> = providers.iter().map(|p| p.spec().key).collect();
    assert_eq!(keys[..4], ["session/ssh", "session/hostname", "session/username", "git"]);
    assert_eq!(keys[4..], ["node", "rs", "py", "pylang", "dk", "kube", "star"], "layout order; unused and invalid modules get none");
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems.iter().any(|p| p.starts_with("modules.broken:") && p.contains("unknown language 'cobol'")), "{problems:?}");
    assert!(problems.iter().any(|p| p.starts_with("modules.badre:") && p.contains("regex")), "{problems:?}");

    let spec = |k: &str| providers.iter().find(|p| p.spec().key == k).unwrap().spec().clone();
    assert_eq!(spec("node").deps, &[Dependency::Cwd]);
    assert_eq!((spec("node").ttl, spec("node").timeout), (Duration::from_secs(600), Duration::from_millis(500)));
    assert_eq!(spec("rs").timeout, Duration::from_millis(900));
    assert_eq!(spec("py").deps, &[Dependency::Cwd, Dependency::Env]);
    assert_eq!(spec("pylang").deps, &[Dependency::Cwd, Dependency::Env], "language python behaves like type python");
    assert_eq!(spec("dk").deps, &[Dependency::Cwd, Dependency::Env]);
    assert_eq!((spec("kube").ttl, spec("kube").timeout), (Duration::from_secs(60), Duration::from_millis(300)));
}

#[test]
fn from_config_rejects_keys_that_collide_with_static_providers() {
    let mut c = config(json!({
        "schema_version": 1,
        "modules": {"go": {"type": "language", "language": "golang"}},
        "layout": {"rows": [{"left": ["git", "go"]}]}
    }));
    c.json["modules"]["git"] = json!({"type": "language", "language": "nodejs"});
    let (providers, problems) = from_config_checked(&c);
    let keys: Vec<&str> = providers.iter().map(|p| p.spec().key).collect();
    assert_eq!(keys, ["session/ssh", "session/hostname", "session/username", "git", "go"]);
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("already in use"), "{problems:?}");
    // No provider modules placed: just the static providers.
    let c = config(json!({"schema_version": 1, "layout": {"rows": [{"left": ["cwd"]}]}}));
    let (providers, problems) = from_config_checked(&c);
    assert_eq!((providers.len(), problems.len()), (4, 0));
}

#[test]
fn pastel_powerline_preset_gets_its_language_providers() {
    let (providers, problems) = from_config_checked(&config(json!({"schema_version": 1, "preset": "pastel-powerline"})));
    assert!(problems.is_empty(), "{problems:?}");
    let keys: Vec<&str> = providers.iter().map(|p| p.spec().key).collect();
    for k in ["c", "go", "java", "node", "py", "rust", "docker", "elixir"] {
        assert!(keys.contains(&k), "{k} missing from {keys:?}");
    }
    assert!(!keys.contains(&"time") && !keys.contains(&"cwd"), "non-provider modules get none: {keys:?}");
}

/// `detect_*` keys replace the matching builtin list of a language module.
#[test]
fn detect_overrides_replace_builtin_lists() {
    let td = tempfile::tempdir().unwrap();
    let c = config(json!({
        "schema_version": 1,
        "modules": {"node": {"type": "language", "language": "nodejs", "detect_extensions": [], "detect_files": ["deno.json"]}},
        "layout": {"rows": [{"left": ["node"]}]}
    }));
    let (providers, problems) = from_config_checked(&c);
    assert!(problems.is_empty(), "{problems:?}");
    let node = providers.iter().find(|p| p.spec().key == "node").unwrap();
    std::fs::write(td.path().join("package.json"), "{}").unwrap();
    std::fs::write(td.path().join("app.js"), "").unwrap();
    match node.fetch(&req(td.path(), &[])) {
        ProviderValue::Module(_, s) => assert!(s.value.is_none() && s.error.is_none(), "package.json and .js no longer detect: {s:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn builtin_config_still_yields_the_static_providers() {
    let (providers, problems) = from_config_checked(&tildy::config::builtin());
    assert!(problems.is_empty(), "{problems:?}");
    let keys: Vec<&str> = providers.iter().map(|p| p.spec().key).collect();
    assert_eq!(keys[..4], ["session/ssh", "session/hostname", "session/username", "git"]);
}

#[test]
fn detection_gates_the_command_and_the_cache_tracks_detect_files() {
    let td = tempfile::tempdir().unwrap();
    let bin = td.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let node = script(&bin, "fakenode", "echo v99.1.2");
    let proj = td.path().join("proj");
    let other = td.path().join("other");
    std::fs::create_dir(&proj).unwrap();
    std::fs::create_dir(&other).unwrap();
    let p = version_cmd("node-a", &node, node_detect(), 600, 2000);

    // Not detected: fresh, no value, nothing runs.
    let (slot, used) = p.fetch_slot(&req(&proj, &[]));
    assert_eq!((slot.freshness, slot.value.is_none(), used), (Freshness::Fresh, true, None));
    assert_eq!(calls(&bin, "fakenode"), 0);

    std::fs::write(proj.join("package.json"), "{}").unwrap();
    let (slot, used) = p.fetch_slot(&req(&proj, &[]));
    assert_eq!((text(&slot), used), (Some("99.1.2"), Some(CacheUse::Ran)));
    assert_eq!(slot.value.as_ref().unwrap().fields["version"], "99.1.2");
    assert_eq!(calls(&bin, "fakenode"), 1);

    // Same cwd, same detect files: served from the cache.
    let (slot, used) = p.fetch_slot(&req(&proj, &[]));
    assert_eq!((text(&slot), used), (Some("99.1.2"), Some(CacheUse::Hit)));
    assert_eq!(calls(&bin, "fakenode"), 1);
    // Another module with the same argv and detect rule shares the entry.
    let twin = version_cmd("node-b", &node, node_detect(), 600, 2000);
    assert_eq!(twin.fetch_slot(&req(&proj, &[])).1, Some(CacheUse::Hit));

    // A new or modified detect file changes the key; source files do not.
    std::fs::write(proj.join("app.js"), "").unwrap();
    assert_eq!(p.fetch_slot(&req(&proj, &[])).1, Some(CacheUse::Hit));
    std::fs::write(proj.join(".nvmrc"), "20").unwrap();
    assert_eq!(p.fetch_slot(&req(&proj, &[])).1, Some(CacheUse::Ran));
    let later = SystemTime::now() + Duration::from_secs(3);
    std::fs::File::options().write(true).open(proj.join(".nvmrc")).unwrap().set_modified(later).unwrap();
    assert_eq!(p.fetch_slot(&req(&proj, &[])).1, Some(CacheUse::Ran));
    assert_eq!(calls(&bin, "fakenode"), 3);

    // Another cwd is another key; detection by extension alone.
    std::fs::write(other.join("x.js"), "").unwrap();
    assert_eq!(p.fetch_slot(&req(&other, &[])).1, Some(CacheUse::Ran));
    assert_eq!(calls(&bin, "fakenode"), 4);
}

#[test]
fn ttl_expiry_reruns() {
    let td = tempfile::tempdir().unwrap();
    let node = script(td.path(), "fakenode", "echo v1.2.3");
    std::fs::write(td.path().join("package.json"), "{}").unwrap();
    let p = version_cmd("node-ttl", &node, node_detect(), 1, 2000);
    assert_eq!(p.fetch_slot(&req(td.path(), &[])).1, Some(CacheUse::Ran));
    assert_eq!(p.fetch_slot(&req(td.path(), &[])).1, Some(CacheUse::Hit));
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(p.fetch_slot(&req(td.path(), &[])).1, Some(CacheUse::Ran));
}

#[test]
fn identical_commands_run_once_when_concurrent() {
    let td = tempfile::tempdir().unwrap();
    let slow = script(td.path(), "slownode", "sleep 0.3; echo v5.0.0");
    std::fs::write(td.path().join("package.json"), "{}").unwrap();
    let providers: Vec<Arc<ModuleProvider>> =
        (0..4).map(|i| Arc::new(version_cmd(&format!("dup{i}"), &slow, node_detect(), 600, 3000))).collect();
    let handles: Vec<_> = providers
        .iter()
        .map(|p| {
            let p = Arc::clone(p);
            let r = req(td.path(), &[]);
            std::thread::spawn(move || p.fetch_slot(&r))
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(calls(td.path(), "slownode"), 1, "one run for four concurrent lookups");
    assert!(results.iter().all(|(s, _)| text(s) == Some("5.0.0")));
    assert_eq!(results.iter().filter(|(_, u)| *u == Some(CacheUse::Ran)).count(), 1);
    assert_eq!(results.iter().filter(|(_, u)| *u == Some(CacheUse::Joined)).count(), 3);
}

#[test]
fn failures_are_slots_not_panics() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("package.json"), "{}").unwrap();
    let r = req(td.path(), &[]);

    // Timeout: the whole process group is killed.
    let hang = script(td.path(), "hangnode", "sleep 7.317; echo v1.0.0");
    let t = Instant::now();
    let (slot, _) = version_cmd("hang", &hang, node_detect(), 600, 150).fetch_slot(&r);
    assert_eq!(slot.freshness, Freshness::Timeout, "{slot:?}");
    assert!(slot.error.as_deref().unwrap().contains("timed out after 150 ms"));
    assert!(t.elapsed() < Duration::from_millis(1000), "{:?}", t.elapsed());
    std::thread::sleep(Duration::from_millis(100));
    let left = Command::new("pgrep").args(["-f", "sleep 7.317"]).output().unwrap();
    assert!(left.stdout.is_empty(), "leftover: {}", String::from_utf8_lossy(&left.stdout));

    let bad = script(td.path(), "badnode", "echo boom >&2; exit 3");
    let (slot, _) = version_cmd("bad", &bad, node_detect(), 600, 2000).fetch_slot(&r);
    assert_eq!(slot.freshness, Freshness::Error);
    assert!(slot.error.as_deref().unwrap().contains("exited with code 3: boom"), "{slot:?}");

    let garbage = script(td.path(), "garbagenode", "echo no version here");
    let (slot, _) = version_cmd("garbage", &garbage, node_detect(), 600, 2000).fetch_slot(&r);
    assert_eq!(slot.freshness, Freshness::Error);
    assert!(slot.error.as_deref().unwrap().contains("no version"), "{slot:?}");

    let missing = td.path().join("no-such-binary").to_string_lossy().into_owned();
    let (slot, _) = version_cmd("missing", &missing, node_detect(), 600, 2000).fetch_slot(&r);
    assert_eq!(slot.freshness, Freshness::Error);
    assert!(slot.error.as_deref().unwrap().contains("not found"), "{slot:?}");

    // Provider::fetch wraps the slot with the module id.
    match version_cmd("wrapped", &missing, node_detect(), 600, 2000).fetch(&r) {
        ProviderValue::Module(id, s) => assert_eq!((id.as_str(), s.freshness), ("wrapped", Freshness::Error)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn command_without_detect_or_regex_uses_first_line() {
    let td = tempfile::tempdir().unwrap();
    let ctx = script(td.path(), "ctx", "printf '\\n  prod-cluster  \\nmore\\n'");
    let empty = script(td.path(), "empty", "true");
    let dir = td.path().join("anywhere");
    std::fs::create_dir(&dir).unwrap();
    let t = Duration::from_secs(2);
    let p = ModuleProvider::command("kctx", vec![ctx], Detect::default(), None, Duration::from_secs(60), t);
    assert_eq!(text(&p.fetch_slot(&req(&dir, &[])).0), Some("prod-cluster"));
    let p = ModuleProvider::command("blank", vec![empty], Detect::default(), None, Duration::from_secs(60), t);
    let (slot, _) = p.fetch_slot(&req(&dir, &[]));
    assert_eq!((slot.freshness, slot.value.is_none()), (Freshness::Fresh, true), "empty output hides the module");
    // Unreadable cwd: never runs.
    let (slot, used) = p.fetch_slot(&req(&td.path().join("missing"), &[]));
    assert_eq!((slot.value.is_none(), used), (true, None));
}

#[test]
fn python_uses_the_virtualenv_and_names_it() {
    let td = tempfile::tempdir().unwrap();
    let venv = td.path().join("venvs/myenv");
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    script(&venv.join("bin"), "python", "echo 'Python 3.11.4'");
    let plain = td.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let p = ModuleProvider::python("py", Duration::from_secs(600), Duration::from_secs(2));
    let venv_s = venv.to_string_lossy().into_owned();

    // No python files and no virtualenv: not detected.
    let (slot, used) = p.fetch_slot(&req(&plain, &[("CONDA_DEFAULT_ENV", "base")]));
    assert_eq!((slot.value.is_none(), used), (true, None));
    // An active virtualenv shows even without python files (Starship does too),
    // and its own interpreter is asked.
    let (slot, _) = p.fetch_slot(&req(&plain, &[("VIRTUAL_ENV", &venv_s), ("USER", "me")]));
    let v = slot.value.expect("value");
    assert_eq!(v.text, "3.11.4");
    assert_eq!(v.fields["version"], "3.11.4");
    assert_eq!(v.fields["env"], "myenv");
    assert_eq!(calls(&venv.join("bin"), "python"), 1);
    // pyvenv.cfg prompt wins over the directory name.
    std::fs::write(venv.join("pyvenv.cfg"), "prompt = proj\n").unwrap();
    let (slot, _) = p.fetch_slot(&req(&plain, &[("VIRTUAL_ENV", &venv_s)]));
    assert_eq!(slot.value.unwrap().fields["env"], "proj");
    assert_eq!(calls(&venv.join("bin"), "python"), 1, "cached");
}

#[test]
fn python_conda_env_name_and_missing_interpreter() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("requirements.txt"), "").unwrap();
    let broken_venv = td.path().join("gone");
    std::fs::create_dir(&broken_venv).unwrap();
    let p = ModuleProvider::python("py2", Duration::from_secs(600), Duration::from_secs(2));
    // The venv has no interpreter: python3/python from PATH are tried next.
    // Whatever they report, the env name is kept.
    let (slot, _) = p.fetch_slot(&req(td.path(), &[("VIRTUAL_ENV", &broken_venv.to_string_lossy())]));
    assert_eq!(slot.value.expect("env shown").fields["env"], "gone");
    let (slot, _) = p.fetch_slot(&req(td.path(), &[("CONDA_DEFAULT_ENV", "ml")]));
    assert_eq!(slot.value.expect("env shown").fields["env"], "ml");
}

#[test]
fn docker_context_needs_files_and_a_visible_context() {
    let td = tempfile::tempdir().unwrap();
    let p = ModuleProvider::docker_context("dk", Duration::from_secs(600), Duration::from_millis(500));
    let r = |env: &[(&str, &str)]| p.fetch_slot(&req(td.path(), env)).0;
    assert!(r(&[("DOCKER_CONTEXT", "colima")]).value.is_none(), "no compose/Dockerfile: hidden");
    std::fs::write(td.path().join("compose.yaml"), "").unwrap();
    let s = r(&[("DOCKER_CONTEXT", "colima")]);
    assert_eq!(text(&s), Some("colima"));
    assert_eq!(s.value.unwrap().fields["context"], "colima");
    let s = r(&[("DOCKER_HOST", "unix:///var/run/docker.sock")]);
    assert_eq!((s.freshness, s.value.is_none()), (Freshness::Fresh, true));
    assert!(r(&[("DOCKER_CONTEXT", "default")]).value.is_none());
}

/// Real toolchains when installed: cold `node --version` cost, cache-hit cost,
/// and detection cost in a directory without matches.
#[test]
fn measure_real_language_lookups() {
    let td = tempfile::tempdir().unwrap();
    for f in ["package.json", "Cargo.toml", "go.mod", "pom.xml", "main.c", "setup.py"] {
        std::fs::write(td.path().join(f), "").unwrap();
    }
    let r = req(td.path(), &[]);
    let empty = tempfile::tempdir().unwrap();
    for lang in ["nodejs", "rust", "golang", "java", "c", "python"] {
        let def = modules::language(lang).unwrap();
        let p = ModuleProvider::language(lang, def, Duration::from_secs(600), Duration::from_secs(5));
        let t = Instant::now();
        let (cold, used) = p.fetch_slot(&r);
        let cold_t = t.elapsed();
        let mut hits = Vec::new();
        for _ in 0..50 {
            let t = Instant::now();
            let _ = p.fetch_slot(&r);
            hits.push(t.elapsed());
        }
        hits.sort();
        let t = Instant::now();
        let (none, _) = p.fetch_slot(&req(empty.path(), &[]));
        let miss_t = t.elapsed();
        assert!(none.value.is_none());
        println!(
            "{lang:7} first {:?} {:>8.2} ms -> {:?}; cache hit p50 {:.3} ms max {:.3} ms; not-detected {:.3} ms",
            used,
            cold_t.as_secs_f64() * 1e3,
            cold.value.as_ref().map(|v| v.text.as_str()).or(cold.error.as_deref()),
            hits[25].as_secs_f64() * 1e3,
            hits[49].as_secs_f64() * 1e3,
            miss_t.as_secs_f64() * 1e3,
        );
    }
}
