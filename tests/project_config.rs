//! Repository overrides: trust boundary, discovery, fallback and CLI revisions.
use serde_json::{json, Value};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use tildy::config::{self, project};

fn base() -> config::Config {
    config::from_str(r#"{"schema_version":1,"preset":"plain","modules":{"char":{"type":"character"},"dormant":{"type":"command","argv":["false"]}},"layout":{"rows":[{"left":["cwd","char"]}]}}"#).unwrap()
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    dir
}

#[test]
fn display_overrides_preserve_user_data_and_provider_definitions() {
    let base = base();
    let config = project::parse(&base, r#"{"schema_version":1,"modules":{"cwd":{"style":{"foreground":"red"},"priority":1}},"layout":{"rows":[{"left":["char","cwd"]}]},"symbols":"ascii"}"#).unwrap();
    assert_eq!(config.user, base.user);
    assert_eq!(config.file.modules["cwd"].style.foreground.as_deref(), Some("red"));
    assert_eq!(config.json["modules"]["dormant"], base.json["modules"]["dormant"]);
    assert_ne!(config.revision, base.revision);
    assert_eq!(config.file.layout.rows[0].left, ["char", "cwd"]);
}

#[test]
fn unsafe_fields_and_dormant_commands_are_rejected_before_merge() {
    let base = base();
    for (patch, path) in [
        (json!({"preset":"lean"}), "preset"),
        (json!({"agent":{"programs":["evil"]}}), "agent"),
        (json!({"command_timeout_ms":1}), "command_timeout_ms"),
        (json!({"modules":{"new":{"style":{}}}}), "modules.new"),
        (json!({"modules":{"cwd":{"type":"command","argv":["evil"]}}}), "modules.cwd.argv"),
        (json!({"modules":{"dormant":{"argv":["evil"]}}}), "modules.dormant.argv"),
        (json!({"modules":{"dormant":{"detect_files":["x"]}}}), "modules.dormant.detect_files"),
        (json!({"modules":{"cwd":{"starship_config":"/tmp/evil"}}}), "modules.cwd.starship_config"),
        (json!({"layout":{"rows":[{"left":["dormant"]}]}}), "layout.rows"),
    ] {
        let mut layer = patch;
        layer["schema_version"] = json!(1);
        let error = project::parse(&base, &layer.to_string()).unwrap_err();
        assert_eq!(error.path, path, "{layer}: {error}");
    }
    assert!(project::parse(&base, r#"{"schema_version":1,"modules":{"cwd":{"style":{"foreground":"bad color"}}}}"#).is_err());
}

#[test]
fn nearest_worktree_boundary_and_symlinked_cwd() {
    let dir = repo();
    let nested = dir.path().join("nested");
    std::fs::create_dir_all(nested.join("src")).unwrap();
    let expected = dir.path().canonicalize().unwrap().join(project::FILE_NAME);
    assert_eq!(project::discover(&nested), Some(expected));
    std::fs::write(nested.join(".git"), "gitdir: ../elsewhere").unwrap();
    let expected = nested.canonicalize().unwrap().join(project::FILE_NAME);
    assert_eq!(project::discover(&nested.join("src")), Some(expected.clone()));
    let alias = dir.path().join("alias");
    symlink(nested.join("src"), &alias).unwrap();
    assert_eq!(project::discover(&alias), Some(expected));
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join(project::FILE_NAME), "{}").unwrap();
    assert!(project::discover(outside.path()).is_none());
}

#[test]
fn invalid_oversized_special_or_removed_file_keeps_lower_layer() {
    let dir = repo();
    let path = dir.path().join(project::FILE_NAME);
    let original = base();
    for contents in ["bad JSON".into(), " ".repeat(project::MAX_BYTES + 1), "{\"schema_version\":1,\"modules\":{\"cwd\":{\"style\":{\"foreground\":\"red\"}}},\"agent\":{}}".into()] {
        std::fs::write(&path, contents).unwrap();
        let got = project::apply(original.clone(), Some(&path));
        assert_eq!(got.json, original.json);
        assert_eq!(got.revision, original.revision);
        assert!(!got.project.unwrap().valid);
    }
    std::fs::remove_file(&path).unwrap();
    symlink(dir.path().join("absent"), &path).unwrap();
    assert!(!project::apply(original.clone(), Some(&path)).project.unwrap().valid);
    std::fs::remove_file(&path).unwrap();
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let start = std::time::Instant::now();
    assert!(!project::apply(original.clone(), Some(&path)).project.unwrap().valid);
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
    std::fs::remove_file(&path).unwrap();
    let missing = project::apply(original, Some(&path)).project.unwrap();
    assert!(!missing.exists && missing.valid && missing.applied.is_none());
}

fn cli(dir: &Path, config: &Path, args: &[&str]) -> (i32, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_tildy")).args(args).arg("--json")
        .current_dir(dir).env("HOME", dir).env("XDG_STATE_HOME", dir.join("state"))
        .env("TILDY_CONFIG", config).env_remove("TILDY_PROJECT_CONFIG").output().unwrap();
    let value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
    (out.status.code().unwrap(), value)
}

#[test]
fn effective_get_reports_layer_and_keeps_user_revision_for_writes() {
    let dir = repo();
    let user = dir.path().join("user.json");
    let text = base().user.unwrap().to_string();
    std::fs::write(&user, &text).unwrap();
    let path = dir.path().join(project::FILE_NAME);
    std::fs::write(&path, r#"{"schema_version":1,"modules":{"cwd":{"style":{"foreground":"red"}}}}"#).unwrap();
    let (code, get) = cli(dir.path(), &user, &["config", "get", "--effective"]);
    assert_eq!(code, 0);
    let data = &get["data"];
    assert_eq!(data["revision"], config::content_revision(text.as_bytes()));
    assert_ne!(data["effective_revision"], data["revision"]);
    assert_eq!(data["sources"]["modules"]["cwd"], "project");
    assert_eq!(data["project"]["valid"], true);
    assert_eq!(cli(dir.path(), &user, &["config", "validate", "--project", "--file", path.to_str().unwrap()]).0, 0);
    assert_eq!(cli(dir.path(), &user, &["config", "apply", "--file", user.to_str().unwrap(), "--expect-revision", data["revision"].as_str().unwrap()]).0, 0);
    std::fs::write(&path, "{}").unwrap();
    let (_, bad) = cli(dir.path(), &user, &["config", "get", "--effective"]);
    assert_eq!(bad["data"]["valid"], true, "user validity is separate");
    assert_eq!(bad["data"]["project"]["valid"], false);
    assert_eq!(bad["data"]["project"]["problem"]["path"], "schema_version");
    assert_ne!(bad["data"]["sources"]["modules"]["cwd"], "project");
    assert_eq!(cli(dir.path(), &user, &["config", "validate", "--project", "--file", path.to_str().unwrap()]).0, 2);
}

#[test]
fn explicit_context_and_opt_out() {
    let dir = repo();
    let outside = tempfile::tempdir().unwrap();
    let user = outside.path().join("missing.json");
    std::fs::write(dir.path().join(project::FILE_NAME), r#"{"schema_version":1,"symbols":"ascii"}"#).unwrap();
    let (_, v) = cli(outside.path(), &user, &["config", "get", "--effective", "--cwd", dir.path().to_str().unwrap()]);
    assert_eq!(v["data"]["config"]["symbols"], "ascii");
    let out = Command::new(env!("CARGO_BIN_EXE_tildy")).args(["config", "get", "--effective", "--json"])
        .current_dir(dir.path()).env("TILDY_CONFIG", user).env("TILDY_PROJECT_CONFIG", "0").output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["data"]["project"].is_null());
    let (_, schema) = cli(dir.path(), &PathBuf::from("missing.json"), &["schema", "--project"]);
    assert!(schema["data"]["schema"]["properties"].get("agent").is_none());
    assert!(schema["data"]["schema"]["$defs"]["module"]["properties"].get("argv").is_none());
}

#[test]
fn persistent_worker_uses_request_cwd_and_reloads_project_without_restart() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let dir = repo();
    let outside = tempfile::tempdir().unwrap();
    let user = outside.path().join("user.json");
    std::fs::write(&user, r#"{"schema_version":1,"preset":"plain","modules":{"label":{"type":"text","text":"PROJECT","style":{"foreground":"green"}},"hidden":{"type":"command","argv":["touch","SHOULD_NOT_RUN"]}},"layout":{"rows":[{"left":["label"]}]}}"#).unwrap();
    let path = dir.path().join(project::FILE_NAME);
    struct Child(std::process::Child);
    impl Drop for Child { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let mut child = Child(Command::new(env!("CARGO_BIN_EXE_tildy")).arg("worker")
        .current_dir(outside.path()).env("TILDY_CONFIG", &user).env("XDG_STATE_HOME", outside.path().join("state"))
        .env_remove("TILDY_PROJECT_CONFIG").env("TILDY_IDLE_REFRESH_MS", "0")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap());
    let mut stdin = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) { if sender.send(line).is_err() { break; } }
    });
    assert!(receiver.recv_timeout(Duration::from_secs(5)).unwrap().starts_with("HELLO\t"));
    for (gen, cwd, patch, color) in [
        (1, dir.path(), Some(r#"{"schema_version":1,"modules":{"label":{"style":{"foreground":"red"}}}}"#), "red"),
        (2, dir.path(), Some(r#"{"schema_version":1,"modules":{"label":{"style":{"foreground":"blue"}}}}"#), "blue"),
        (3, outside.path(), None, "green"),
        (4, dir.path(), Some(r#"{"schema_version":1,"layout":{"rows":[{"left":["hidden"]}]}}"#), "green"),
    ] {
        if let Some(patch) = patch { std::fs::write(&path, patch).unwrap(); }
        writeln!(stdin, "REQ\t{gen}\tproject-test\t{}\t120\t24\t0\t0\t", cwd.display()).unwrap();
        stdin.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut frame = String::new();
        loop {
            let line = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap_or_else(|e| panic!("request {gen}: {e}; last frame: {frame}"));
            if line.starts_with("RES\t") { frame.clear(); }
            frame.push_str(&line);
            frame.push('\n');
            if line == format!("END\t{gen}") && frame.contains(&format!("%F{{{color}}}")) && frame.contains("PROJECT") { break; }
        }
    }
    assert!(!dir.path().join("SHOULD_NOT_RUN").exists());
    assert!(!outside.path().join("SHOULD_NOT_RUN").exists());
    writeln!(stdin, "BYE").unwrap();
    drop(stdin);
    assert!(child.0.wait().unwrap().success());
    reader.join().unwrap();
}
