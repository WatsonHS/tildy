//! Contract tests for `tildy config get|validate|apply|rollback|history` and
//! `tildy schema` through the built binary (docs/configuration.md). Every test
//! runs in its own temp dir with HOME, XDG_CONFIG_HOME and XDG_STATE_HOME
//! pointing inside it, and stdin closed (no TTY).
use serde_json::{json, Value};
use tildy::config::{self, content_revision};
use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_tildy");

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let s = Sandbox { dir: tempfile::tempdir().unwrap() };
        std::fs::create_dir_all(s.home()).unwrap();
        s
    }
    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn home(&self) -> PathBuf {
        self.root().join("home")
    }
    fn config(&self) -> PathBuf {
        self.root().join("xdg-config").join("tildy").join("config.json")
    }
    fn state(&self) -> PathBuf {
        self.root().join("xdg-state")
    }
    fn history_dir(&self) -> PathBuf {
        self.state().join("tildy").join("config-history")
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .current_dir(self.root())
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.root().join("xdg-config"))
            .env("XDG_STATE_HOME", self.state())
            .env_remove("TILDY_CONFIG")
            .env_remove("TILDY_SYMBOLS")
            .stdin(Stdio::null());
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }
    /// Run with `--json`; check the envelope contract; return (exit, envelope).
    fn json(&self, args: &[&str]) -> (i32, Value) {
        let mut a = args.to_vec();
        a.push("--json");
        envelope(&self.run(&a))
    }
    fn file(&self, name: &str, content: &str) -> PathBuf {
        let p = self.root().join(name);
        std::fs::write(&p, content).unwrap();
        p
    }
    fn set_config(&self, content: &str) {
        std::fs::create_dir_all(self.config().parent().unwrap()).unwrap();
        std::fs::write(self.config(), content).unwrap();
    }
    fn config_bytes(&self) -> Option<Vec<u8>> {
        std::fs::read(self.config()).ok()
    }
    fn entries(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.history_dir())
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.retain(|n| n.ends_with(".json") && !n.ends_with(".meta.json"));
        v.sort();
        v
    }
    fn revision(&self) -> String {
        let (code, v) = self.json(&["config", "get"]);
        assert_eq!(code, 0, "{v}");
        v["data"]["revision"].as_str().unwrap().to_string()
    }
}

/// The `CliEnvelope` contract (schema/error.schema.json): stdout is exactly
/// one envelope, stderr is empty, the exit code matches `error.code`.
fn envelope(o: &Output) -> (i32, Value) {
    let code = o.status.code().expect("exited normally");
    let v: Value = serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON value ({e}): {}", String::from_utf8_lossy(&o.stdout)));
    assert!(o.stderr.is_empty(), "--json must keep stderr empty: {}", String::from_utf8_lossy(&o.stderr));
    let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, BTreeSet::from(["api_version", "ok", "data", "error"]), "{v}");
    assert_eq!(v["api_version"], 1);
    let e = &v["error"];
    let ekeys: BTreeSet<&str> = e.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(ekeys, BTreeSet::from(["code", "path", "message", "hint"]), "{v}");
    if v["ok"] == true {
        assert_eq!(code, 0, "{v}");
        assert_eq!(e["code"], "ok");
        assert!(e["path"].is_null() && e["message"].is_null() && e["hint"].is_null(), "{v}");
    } else {
        assert_eq!(v["ok"], false);
        assert!(v["data"].is_null(), "{v}");
        let msg = e["message"].as_str().expect("message");
        assert!(!msg.is_empty() && msg.chars().count() <= 1000, "{v}");
        let expected = match e["code"].as_str().unwrap() {
            "validation_failed" => 2,
            "revision_conflict" => 3,
            "environment_unavailable" => 4,
            "backend_unavailable" => 5,
            "internal_error" => 1,
            other => panic!("unexpected code {other}"),
        };
        assert_eq!(code, expected, "{v}");
    }
    (code, v)
}

fn code_of(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap()
}

fn path_of(v: &Value) -> Option<&str> {
    v["error"]["path"].as_str()
}

fn fixture(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(rel)).unwrap()
}

/// Invalid candidates: (name, content, expected error.path).
fn invalid_cases() -> Vec<(&'static str, String, Option<&'static str>)> {
    vec![
        ("broken JSON", r#"{"schema_version": 1, "modules": {"#.into(), None),
        ("not an object", "[1, 2]".into(), None),
        ("unknown version", fixture("config.v1.invalid-future-version.json"), Some("schema_version")),
        ("unknown field", fixture("config.v1.invalid-unknown-field.json"), Some("modules.git_main.colour")),
        ("dangling module reference", fixture("config/invalid-dangling-ref.json"), Some("layout.rows[0].left[1]")),
        (
            "duplicate rule id",
            r#"{"schema_version": 1, "modules": {"cwd": {"rules": [
                {"id": "a", "when": {"field": "git.state", "op": "eq", "value": "clean"}},
                {"id": "a", "when": {"field": "git.state", "op": "eq", "value": "dirty"}}]}}}"#
                .into(),
            Some("modules.cwd.rules[1].id"),
        ),
        ("unknown preset", r#"{"schema_version": 1, "preset": "neon"}"#.into(), Some("preset")),
    ]
}

const A: &str = "{\n  \"schema_version\": 1,\n  \"$comment\": \"mine\",\n  \"preset\": \"pastel-powerline\"\n}\n";
const B: &str = "{\"schema_version\": 1, \"symbols\": \"ascii\"}\n";
const C: &str = "{\"schema_version\": 1, \"history\": {\"mode\": \"full\"}}\n";

// ---- get ---------------------------------------------------------------

#[test]
fn get_without_a_file_is_builtin() {
    let s = Sandbox::new();
    let (_, v) = s.json(&["config", "get"]);
    let d = &v["data"];
    assert_eq!(d["path"], s.config().to_str().unwrap());
    assert_eq!((d["exists"].clone(), d["revision"].clone(), d["valid"].clone()), (json!(false), json!("builtin"), json!(true)));
    assert!(d["config"].is_null() && d["text"].is_null() && d["bytes"].is_null() && d["problem"].is_null());

    let (_, v) = s.json(&["config", "get", "--effective"]);
    let d = &v["data"];
    assert_eq!((d["preset"].clone(), d["fallback"].clone()), (json!("pastel-powerline"), json!(false)));
    assert_eq!(d["config"], config::builtin().json);
    for (k, src) in d["sources"]["keys"].as_object().unwrap().iter().chain(d["sources"]["modules"].as_object().unwrap()) {
        assert!(src == "builtin" || src == "preset:pastel-powerline", "{k}: {src}");
    }
    // `--json` before the subcommand works too; text mode is for humans.
    let (_, v2) = envelope(&s.run(&["config", "--json", "get"]));
    assert_eq!(v2["data"]["revision"], "builtin");
    let o = s.run(&["config", "get"]);
    assert_eq!(o.status.code(), Some(0));
    let out = String::from_utf8(o.stdout).unwrap();
    assert!(out.contains("revision builtin, no file") && serde_json::from_str::<Value>(&out).is_err(), "{out}");
}

#[test]
fn get_returns_value_text_and_revision() {
    let s = Sandbox::new();
    s.set_config(A);
    let (_, v) = s.json(&["config", "get"]);
    let d = &v["data"];
    assert_eq!(d["revision"], content_revision(A.as_bytes()));
    assert_eq!(d["exists"], true);
    assert_eq!(d["valid"], true);
    assert_eq!(d["bytes"], A.len());
    assert_eq!(d["text"], A);
    assert_eq!(d["config"], serde_json::from_str::<Value>(A).unwrap());
    let o = s.run(&["config", "get"]);
    let out = String::from_utf8(o.stdout).unwrap();
    assert!(out.ends_with(A) && out.contains(&content_revision(A.as_bytes())), "{out}");
}

#[test]
fn get_effective_names_the_source_of_each_key_and_module() {
    let s = Sandbox::new();
    s.set_config(
        r##"{"schema_version": 1, "preset": "pastel-powerline",
            "modules": {"time": {"style": {"foreground": "#FFFFFF"}}, "hello": {"type": "text", "text": "hi"}}}"##,
    );
    let (_, v) = s.json(&["config", "get", "--effective"]);
    let d = &v["data"];
    assert_eq!(d["preset"], "pastel-powerline");
    assert_eq!(d["config"]["modules"]["time"]["style"]["background"], "#33658A", "partial override merged onto the preset module");
    let keys = &d["sources"]["keys"];
    assert_eq!(keys["schema_version"], "user");
    assert_eq!(keys["preset"], "user");
    assert_eq!(keys["modules"], "user");
    assert_eq!(keys["layout"], "preset:pastel-powerline");
    assert_eq!(keys["symbols"], "preset:pastel-powerline");
    assert_eq!(keys["history"], "builtin");
    let mods = &d["sources"]["modules"];
    assert_eq!(mods["time"], "user");
    assert_eq!(mods["hello"], "user");
    assert_eq!(mods["cwd"], "preset:pastel-powerline");
    assert!(mods.get("ssh_host").is_none(), "the preset replaces the built-in modules");
    let o = s.run(&["config", "get", "--effective"]);
    let out = String::from_utf8(o.stdout).unwrap();
    assert!(out.contains("modules.cwd") && out.contains("preset:pastel-powerline"), "{out}");
}

#[test]
fn get_reports_a_broken_file_with_its_revision() {
    let s = Sandbox::new();
    let broken = "{\"schema_version\": 1,,}";
    s.set_config(broken);
    let (code, v) = s.json(&["config", "get"]);
    assert_eq!(code, 0);
    let d = &v["data"];
    assert_eq!(d["revision"], content_revision(broken.as_bytes()));
    assert_eq!((d["valid"].clone(), d["problem"]["kind"].clone()), (json!(false), json!("syntax")));
    assert!(d["config"].is_null());
    assert_eq!(d["text"], broken);
    let (_, v) = s.json(&["config", "get", "--effective"]);
    assert_eq!((v["data"]["fallback"].clone(), v["data"]["valid"].clone()), (json!(true), json!(false)));
    assert_eq!(v["data"]["config"], config::builtin().json);
    // A file that parses but does not validate: invalid, with the field path.
    s.set_config(&fixture("config.v1.invalid-unknown-field.json"));
    let (_, v) = s.json(&["config", "get"]);
    assert_eq!(v["data"]["problem"]["kind"], "invalid");
    assert_eq!(v["data"]["problem"]["path"], "modules.git_main.colour");
    assert!(v["data"]["config"].is_object());
}

// ---- validate ----------------------------------------------------------

#[test]
fn validate_reports_revision_and_warnings() {
    let s = Sandbox::new();
    let f = s.file("ok.json", A);
    let (_, v) = s.json(&["config", "validate", "--file", f.to_str().unwrap()]);
    assert_eq!(v["data"]["revision"], content_revision(A.as_bytes()));
    assert_eq!(v["data"]["preset"], "pastel-powerline");
    assert_eq!(v["data"]["warnings"], json!([]));
    // Provider-layer problems and unplaced modules are warnings, not errors.
    let w = s.file(
        "warn.json",
        r#"{"schema_version": 1, "modules": {"git": {"type": "command", "argv": ["true"]}, "lonely": {"type": "text", "text": "x"}}}"#,
    );
    let (code, v) = s.json(&["config", "validate", "--file", w.to_str().unwrap()]);
    assert_eq!(code, 0);
    let warnings = v["data"]["warnings"].as_array().unwrap();
    let paths: Vec<_> = warnings.iter().map(|w| w["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["modules.git", "modules.lonely"], "{v}");
    assert!(warnings[0]["message"].as_str().unwrap().contains("already in use"));
    let o = s.run(&["config", "validate", "--file", w.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stderr).contains("warning: modules.lonely"));
    // stdin
    let mut c = s.cmd(&["config", "validate", "--file", "-", "--json"]);
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), B.as_bytes()).unwrap();
    let (_, v) = envelope(&child.wait_with_output().unwrap());
    assert_eq!(v["data"]["revision"], content_revision(B.as_bytes()));
}

#[test]
fn validate_failures_have_code_path_and_exit_2() {
    let s = Sandbox::new();
    for (name, content, path) in invalid_cases() {
        let f = s.file("candidate.json", &content);
        let (code, v) = s.json(&["config", "validate", "--file", f.to_str().unwrap()]);
        assert_eq!((code, code_of(&v)), (2, "validation_failed"), "{name}: {v}");
        assert_eq!(path_of(&v), path, "{name}: {v}");
        // Text mode: same exit code, the path on stderr, nothing on stdout.
        let o = s.run(&["config", "validate", "--file", f.to_str().unwrap()]);
        assert_eq!(o.status.code(), Some(2), "{name}");
        assert!(o.stdout.is_empty());
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.starts_with("tildy config: ") && err.contains("hint: "), "{name}: {err}");
        if let Some(p) = path {
            assert!(err.contains(p), "{name}: {err}");
        }
    }
    let f = s.root().join("latin1.json");
    std::fs::write(&f, b"{\"schema_version\": 1, \"$comment\": \"caf\xe9\"}").unwrap();
    let (code, v) = s.json(&["config", "validate", "--file", f.to_str().unwrap()]);
    assert_eq!(code, 2, "{v}");
    let (code, v) = s.json(&["config", "validate", "--file", s.root().join("missing.json").to_str().unwrap()]);
    assert_eq!((code, code_of(&v)), (2, "validation_failed"), "{v}");
}

#[test]
fn validate_without_file_checks_the_current_config() {
    let s = Sandbox::new();
    let (_, v) = s.json(&["config", "validate"]);
    assert_eq!((v["data"]["exists"].clone(), v["data"]["revision"].clone()), (json!(false), json!("builtin")));
    s.set_config(B);
    let (_, v) = s.json(&["config", "validate"]);
    assert_eq!(v["data"]["revision"], content_revision(B.as_bytes()));
    s.set_config(&fixture("config/invalid-dangling-ref.json"));
    let (code, v) = s.json(&["config", "validate"]);
    assert_eq!((code, path_of(&v)), (2, Some("layout.rows[0].left[1]")));
}

// ---- apply -------------------------------------------------------------

#[test]
fn apply_writes_exact_bytes_and_saves_the_previous_file() {
    let s = Sandbox::new();
    let a = s.file("a.json", A);
    let (_, v) = s.json(&["config", "apply", "--file", a.to_str().unwrap(), "--expect-revision", "builtin"]);
    let ra = content_revision(A.as_bytes());
    assert_eq!(v["data"]["previous_revision"], "builtin");
    assert_eq!(v["data"]["revision"], ra);
    assert_eq!((v["data"]["changed"].clone(), v["data"]["forced"].clone()), (json!(true), json!(false)));
    assert!(v["data"]["history_id"].is_null(), "no previous file, nothing to save");
    assert_eq!(s.config_bytes().unwrap(), A.as_bytes(), "the candidate's bytes, not a re-encoding");
    assert!(s.entries().is_empty());

    let b = s.file("b.json", B);
    let (_, v) = s.json(&["config", "apply", "--file", b.to_str().unwrap(), "--expect-revision", &ra]);
    let rb = content_revision(B.as_bytes());
    assert_eq!((v["data"]["previous_revision"].clone(), v["data"]["revision"].clone()), (json!(ra), json!(rb)));
    let id = v["data"]["history_id"].as_str().unwrap().to_string();
    assert!(id.ends_with(&format!("-{ra}")), "{id}");
    assert_eq!(s.entries(), [format!("{id}.json")]);
    assert_eq!(std::fs::read(s.history_dir().join(format!("{id}.json"))).unwrap(), A.as_bytes());
    assert_eq!(s.config_bytes().unwrap(), B.as_bytes());
    assert_eq!(s.revision(), rb);

    let (_, h) = s.json(&["config", "history"]);
    let d = &h["data"];
    assert_eq!(d["dir"], s.history_dir().to_str().unwrap());
    assert_eq!((d["current_revision"].clone(), d["keep"].clone()), (json!(rb), json!(50)));
    let e = &d["entries"][0];
    assert_eq!(e["id"], id.as_str());
    assert_eq!(e["revision"], ra);
    assert_eq!(e["bytes"], A.len());
    assert_eq!(e["op"], "apply");
    assert_eq!(e["path"], s.config().to_str().unwrap());
    assert_eq!(e["replaced_by"], rb);
    assert!(e["at_ms"].as_u64().unwrap() > 1_700_000_000_000);
    let o = s.run(&["config", "history"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains(&ra));

    // Same content again: nothing written, no history entry.
    let (_, v) = s.json(&["config", "apply", "--file", b.to_str().unwrap(), "--expect-revision", &rb]);
    assert_eq!((v["data"]["changed"].clone(), v["data"]["history_id"].clone()), (json!(false), Value::Null));
    assert_eq!(s.entries().len(), 1);
}

#[test]
fn apply_keeps_the_file_when_the_candidate_is_invalid() {
    let s = Sandbox::new();
    s.set_config(A);
    let ra = content_revision(A.as_bytes());
    for (name, content, path) in invalid_cases() {
        let f = s.file("candidate.json", &content);
        let (code, v) = s.json(&["config", "apply", "--file", f.to_str().unwrap(), "--expect-revision", &ra]);
        assert_eq!((code, code_of(&v), path_of(&v)), (2, "validation_failed", path), "{name}: {v}");
        assert_eq!(s.config_bytes().unwrap(), A.as_bytes(), "{name}");
    }
    assert!(s.entries().is_empty());
    assert!(!s.history_dir().exists() || s.entries().is_empty());
}

#[test]
fn apply_checks_the_expected_revision() {
    let s = Sandbox::new();
    s.set_config(A);
    let ra = content_revision(A.as_bytes());
    let b = s.file("b.json", B);
    let bp = b.to_str().unwrap();
    // Stale revision: conflict, exit 3, nothing changes.
    let stale = content_revision(b"something else");
    let (code, v) = s.json(&["config", "apply", "--file", bp, "--expect-revision", &stale]);
    assert_eq!((code, code_of(&v)), (3, "revision_conflict"), "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains(&ra) && msg.contains(&stale), "{msg}");
    assert!(v["error"]["hint"].as_str().unwrap().contains("config get"));
    // `builtin` while a file exists is a conflict too.
    let (code, _) = s.json(&["config", "apply", "--file", bp, "--expect-revision", "builtin"]);
    assert_eq!(code, 3);
    assert_eq!(s.config_bytes().unwrap(), A.as_bytes());
    assert!(s.entries().is_empty());
    // Malformed or missing revision: validation_failed.
    let (code, v) = s.json(&["config", "apply", "--file", bp, "--expect-revision", "latest"]);
    assert_eq!((code, code_of(&v)), (2, "validation_failed"));
    assert!(v["error"]["message"].as_str().unwrap().contains("--expect-revision"));
    let (code, v) = s.json(&["config", "apply", "--file", bp]);
    assert_eq!(code, 2);
    assert!(v["error"]["message"].as_str().unwrap().contains("--expect-revision is required"));
    let (code, v) = s.json(&["config", "apply", "--expect-revision", &ra]);
    assert_eq!(code, 2);
    assert!(v["error"]["message"].as_str().unwrap().contains("--file is required"));
    // Uppercase hex is the same revision.
    let (code, _) = s.json(&["config", "apply", "--file", bp, "--expect-revision", &ra.to_uppercase()]);
    assert_eq!(code, 0);
    // --force skips the check and still records history.
    let c = s.file("c.json", C);
    let (_, v) = s.json(&["config", "apply", "--file", c.to_str().unwrap(), "--force"]);
    assert_eq!(v["data"]["forced"], true);
    assert_eq!(v["data"]["previous_revision"], content_revision(B.as_bytes()));
    assert!(v["data"]["history_id"].is_string());
    let (_, v) = s.json(&["config", "apply", "--file", bp, "--expect-revision", &stale, "--force"]);
    assert_eq!(v["data"]["changed"], true);
    assert_eq!(s.entries().len(), 3);
}

#[test]
fn apply_save_failures_keep_the_file() {
    let s = Sandbox::new();
    s.set_config(A);
    let ra = content_revision(A.as_bytes());
    let b = s.file("b.json", B);
    let dir = s.config().parent().unwrap().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let probe = std::fs::write(dir.join("probe"), "x").is_ok();
    if !probe {
        let (code, v) = s.json(&["config", "apply", "--file", b.to_str().unwrap(), "--expect-revision", &ra]);
        assert_eq!((code, code_of(&v)), (4, "environment_unavailable"), "{v}");
        assert!(v["error"]["hint"].as_str().unwrap().contains("not changed"));
        assert_eq!(s.config_bytes().unwrap(), A.as_bytes());
        assert!(s.entries().is_empty(), "no history entry for a failed apply");
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    if probe {
        eprintln!("skipped: running with privileges that ignore directory permissions");
        return;
    }
    let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(leftovers, ["config.json"], "no temporary files left");

    // An unwritable state directory (no history directory yet) also fails
    // before the config changes.
    std::fs::remove_dir_all(s.state()).unwrap();
    std::fs::create_dir_all(s.state()).unwrap();
    std::fs::set_permissions(s.state(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let (code, v) = s.json(&["config", "apply", "--file", b.to_str().unwrap(), "--expect-revision", &ra]);
    std::fs::set_permissions(s.state(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!((code, code_of(&v)), (4, "environment_unavailable"), "{v}");
    assert_eq!(s.config_bytes().unwrap(), A.as_bytes());
    let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(leftovers, ["config.json"]);

    // History directory writable, but the entry cannot be written (the
    // directory exists and is read-only): the staged file is removed.
    std::fs::create_dir_all(s.history_dir()).unwrap();
    std::fs::write(s.history_dir().join(".lock"), "").unwrap();
    std::fs::set_permissions(s.history_dir(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let (code, v) = s.json(&["config", "apply", "--file", b.to_str().unwrap(), "--expect-revision", &ra]);
    std::fs::set_permissions(s.history_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!((code, code_of(&v)), (4, "environment_unavailable"), "{v}");
    assert_eq!(s.config_bytes().unwrap(), A.as_bytes());
    let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(leftovers.into_iter().filter(|n| n != "config.json.lock").collect::<Vec<_>>(), ["config.json"]);
    assert!(s.entries().is_empty());
}

#[test]
fn apply_keeps_permissions_and_follows_symlinks() {
    let s = Sandbox::new();
    let real_dir = s.root().join("dotfiles");
    std::fs::create_dir_all(&real_dir).unwrap();
    let real = real_dir.join("tildy.json");
    std::fs::write(&real, A).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::create_dir_all(s.config().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, s.config()).unwrap();
    let b = s.file("b.json", B);
    let (_, v) = s.json(&["config", "apply", "--file", b.to_str().unwrap(), "--expect-revision", &content_revision(A.as_bytes())]);
    assert_eq!(v["data"]["path"], s.config().to_str().unwrap());
    assert!(std::fs::symlink_metadata(s.config()).unwrap().file_type().is_symlink(), "the link stays a link");
    assert_eq!(std::fs::read_to_string(&real).unwrap(), B);
    assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o600);
    // Rolling back to builtin removes the file behind the link; apply recreates it.
    let rb = content_revision(B.as_bytes());
    let (_, v) = s.json(&["config", "rollback", "--to", "builtin", "--expect-revision", &rb]);
    assert_eq!(v["data"]["revision"], "builtin");
    assert!(!real.exists() && std::fs::symlink_metadata(s.config()).is_ok());
    assert_eq!(s.revision(), "builtin");
    let (code, _) = s.json(&["config", "apply", "--file", b.to_str().unwrap(), "--expect-revision", "builtin"]);
    assert_eq!(code, 0);
    assert_eq!(std::fs::read_to_string(&real).unwrap(), B);
}

#[test]
fn concurrent_applies_with_one_revision_have_one_winner() {
    let s = Sandbox::new();
    s.set_config(A);
    let ra = content_revision(A.as_bytes());
    let files: Vec<PathBuf> = (0..8).map(|i| s.file(&format!("c{i}.json"), &format!("{{\"schema_version\": 1, \"$comment\": \"writer {i}\"}}"))).collect();
    let children: Vec<_> = files
        .iter()
        .map(|f| {
            s.cmd(&["config", "apply", "--file", f.to_str().unwrap(), "--expect-revision", &ra, "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let codes: Vec<i32> = children.into_iter().map(|c| envelope(&c.wait_with_output().unwrap()).0).collect();
    assert_eq!(codes.iter().filter(|c| **c == 0).count(), 1, "{codes:?}");
    assert_eq!(codes.iter().filter(|c| **c == 3).count(), 7, "{codes:?}");
    assert_eq!(s.entries().len(), 1);
    let now = String::from_utf8(s.config_bytes().unwrap()).unwrap();
    assert!(now.contains("writer"), "{now}");
}

fn hold_lock(path: &Path) -> std::fs::File {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).unwrap();
    // SAFETY: the test owns the descriptor and keeps it open until released.
    assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    f.set_len(0).unwrap();
    writeln!(f, "{}", std::process::id()).unwrap();
    f
}

fn bounded_output(mut child: std::process::Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(6);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("config writer exceeded the lock wait deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn all_config_writers_timeout_without_changing_config_or_history() {
    let s = Sandbox::new();
    s.set_config(A);
    let revision = s.revision();
    let candidate = s.file("candidate.json", B);
    let lock_path = s.config().with_extension("json.lock");
    let _held = hold_lock(&lock_path);
    let args = [
        vec!["config", "apply", "--file", candidate.to_str().unwrap(), "--expect-revision", &revision, "--json"],
        vec!["config", "apply", "--file", candidate.to_str().unwrap(), "--force", "--json"],
        vec!["config", "rollback", "--to", "builtin", "--expect-revision", &revision, "--json"],
        vec!["theme", "--preset", "plain"],
        vec!["configure", "--preset", "plain"],
    ];
    let started = Instant::now();
    let children: Vec<_> = args.iter().map(|args| s.cmd(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()).collect();
    for (args, child) in args.iter().zip(children) {
        let output = bounded_output(child);
        assert_eq!(output.status.code(), Some(4), "{args:?}: {output:?}");
        let message = if args[0] == "config" {
            let (_, v) = envelope(&output);
            v["error"]["message"].as_str().unwrap().to_string()
        } else {
            assert!(output.stdout.is_empty());
            String::from_utf8(output.stderr).unwrap()
        };
        assert!(message.contains("config locked") && message.contains(&format!("pid {}", std::process::id())), "{message}");
        assert!(message.contains("config.json.lock"), "{message}");
    }
    assert!(started.elapsed() >= Duration::from_millis(1800));
    assert!(started.elapsed() < Duration::from_secs(6));
    assert_eq!(s.config_bytes().unwrap(), A.as_bytes());
    assert!(s.entries().is_empty());
}

#[test]
fn different_state_directories_and_symlink_aliases_share_the_config_lock() {
    let s = Sandbox::new();
    s.set_config(A);
    let revision = s.revision();
    let alias = s.root().join("alias.json");
    std::os::unix::fs::symlink(s.config(), &alias).unwrap();
    let parent_alias = s.root().join("parent-alias");
    std::os::unix::fs::symlink(s.config().parent().unwrap(), &parent_alias).unwrap();
    let held = hold_lock(&s.config().with_extension("json.lock"));
    let states: Vec<_> = (0..8).map(|i| s.root().join(format!("state-{i}"))).collect();
    let mut children: Vec<_> = states.iter().enumerate().map(|(i, state)| {
        let candidate = s.file(&format!("c{i}.json"), &format!("{{\"schema_version\":1,\"$comment\":\"writer {i}\"}}"));
        let config = match i % 3 {
            0 => s.config(),
            1 => alias.clone(),
            _ => parent_alias.join("config.json"),
        };
        s.cmd(&["config", "apply", "--file", candidate.to_str().unwrap(), "--expect-revision", &revision, "--json"])
            .env("TILDY_CONFIG", config).env("XDG_STATE_HOME", state)
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    }).collect();
    std::thread::sleep(Duration::from_millis(300));
    assert!(children.iter_mut().all(|c| c.try_wait().unwrap().is_none()), "every alias must wait on the held config lock");
    assert_eq!(s.config_bytes().unwrap(), A.as_bytes());
    drop(held);
    let outcomes: Vec<_> = children.into_iter().map(|c| envelope(&bounded_output(c))).collect();
    assert_eq!(outcomes.iter().filter(|(code, _)| *code == 0).count(), 1, "{outcomes:?}");
    assert_eq!(outcomes.iter().filter(|(code, _)| *code == 3).count(), 7, "{outcomes:?}");
    let entries: usize = states.iter().map(|state| {
        std::fs::read_dir(state.join("tildy/config-history")).unwrap().flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json") && !e.file_name().to_string_lossy().ends_with(".meta.json")).count()
    }).sum();
    assert_eq!(entries, 1);
    assert!(alias.is_symlink());
}

#[test]
fn config_and_history_locks_share_one_timeout_budget() {
    let s = Sandbox::new();
    s.set_config(A);
    let candidate = s.file("candidate.json", B);
    let config_lock = hold_lock(&s.config().with_extension("json.lock"));
    let _history_lock = hold_lock(&s.history_dir().join(".lock"));
    let started = Instant::now();
    let child = s.cmd(&["config", "apply", "--file", candidate.to_str().unwrap(), "--force", "--json"])
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    drop(config_lock);
    let (code, v) = envelope(&bounded_output(child));
    assert_eq!(code, 4, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("config-history/.lock"), "{v}");
    assert!(started.elapsed() >= Duration::from_millis(1800));
    assert!(started.elapsed() < Duration::from_millis(2800), "must not restart the two-second budget for history");
    assert_eq!(s.config_bytes().unwrap(), A.as_bytes());
    assert!(s.entries().is_empty());
}

#[test]
fn killed_lock_holder_leaves_a_reusable_lock_file() {
    let s = Sandbox::new();
    s.set_config(A);
    let lock = s.config().with_extension("json.lock");
    let script = "import fcntl,os,sys,time\nf=open(sys.argv[1],'w')\nfcntl.flock(f,fcntl.LOCK_EX)\nf.write(str(os.getpid()))\nf.flush()\nprint('ready',flush=True)\ntime.sleep(10)";
    let mut holder = Command::new("python3").args(["-c", script]).arg(&lock).stdout(Stdio::piped()).spawn().unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(holder.stdout.take().unwrap()).read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), "ready");
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(lock.exists());
    let candidate = s.file("candidate.json", B);
    let (code, v) = s.json(&["config", "apply", "--file", candidate.to_str().unwrap(), "--expect-revision", &s.revision()]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.config_bytes().unwrap(), B.as_bytes());
    assert!(lock.exists());
}

#[test]
fn lock_symlink_is_rejected_without_touching_its_target() {
    let s = Sandbox::new();
    s.set_config(A);
    let other = s.file("unrelated", "keep me");
    std::os::unix::fs::symlink(&other, s.config().with_extension("json.lock")).unwrap();
    let candidate = s.file("candidate.json", B);
    let (code, v) = s.json(&["config", "apply", "--file", candidate.to_str().unwrap(), "--force"]);
    assert_eq!(code, 4, "{v}");
    assert_eq!(std::fs::read_to_string(other).unwrap(), "keep me");
    assert_eq!(s.config_bytes().unwrap(), A.as_bytes());
    assert!(s.entries().is_empty());
}

#[test]
fn unrelated_regular_file_and_hard_link_at_lock_path_are_not_overwritten() {
    for hard_link in [false, true] {
        let s = Sandbox::new();
        s.set_config(A);
        let lock = s.config().with_extension("json.lock");
        let other = s.file("unrelated", "12345\n");
        if hard_link {
            std::fs::hard_link(&other, &lock).unwrap();
        } else {
            std::fs::write(&lock, "keep me").unwrap();
        }
        let before = std::fs::read(&lock).unwrap();
        let candidate = s.file("candidate.json", B);
        let (code, v) = s.json(&["config", "apply", "--file", candidate.to_str().unwrap(), "--force"]);
        assert_eq!(code, 4, "{v}");
        assert_eq!(std::fs::read(lock).unwrap(), before);
        assert_eq!(std::fs::read_to_string(other).unwrap(), "12345\n");
        assert_eq!(s.config_bytes().unwrap(), A.as_bytes());
        assert!(s.entries().is_empty());
    }
}

// ---- rollback ----------------------------------------------------------

#[test]
fn rollback_restores_revisions_and_builtin() {
    let s = Sandbox::new();
    let (ra, rb) = (content_revision(A.as_bytes()), content_revision(B.as_bytes()));
    let a = s.file("a.json", A);
    let b = s.file("b.json", B);
    s.json(&["config", "apply", "--file", a.to_str().unwrap(), "--expect-revision", "builtin"]);
    s.json(&["config", "apply", "--file", b.to_str().unwrap(), "--expect-revision", &ra]);

    let (_, v) = s.json(&["config", "rollback", "--to", &ra, "--expect-revision", &rb]);
    let d = &v["data"];
    assert_eq!((d["previous_revision"].clone(), d["revision"].clone(), d["changed"].clone()), (json!(rb), json!(ra), json!(true)));
    assert!(d["restored_from"].as_str().unwrap().ends_with(&ra));
    assert!(d["history_id"].as_str().unwrap().ends_with(&rb));
    assert_eq!(s.config_bytes().unwrap(), A.as_bytes());

    let (_, v) = s.json(&["config", "rollback", "--to", "builtin", "--expect-revision", &ra]);
    assert_eq!(v["data"]["revision"], "builtin");
    assert!(v["data"]["restored_from"].is_null());
    assert!(v["data"]["history_id"].as_str().unwrap().ends_with(&ra));
    assert!(s.config_bytes().is_none(), "--to builtin removes the user file");
    assert_eq!(s.revision(), "builtin");

    // Already there: no-op.
    let (_, v) = s.json(&["config", "rollback", "--to", "builtin", "--expect-revision", "builtin"]);
    assert_eq!(v["data"]["changed"], false);
    // From builtin back to a saved revision.
    let (_, v) = s.json(&["config", "rollback", "--to", &rb, "--expect-revision", "builtin"]);
    assert!(v["data"]["history_id"].is_null(), "no file to save");
    assert_eq!(s.config_bytes().unwrap(), B.as_bytes());

    let (_, h) = s.json(&["config", "history"]);
    let ops: Vec<(&str, &str, &str)> = h["data"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["op"].as_str().unwrap(), e["revision"].as_str().unwrap(), e["replaced_by"].as_str().unwrap()))
        .collect();
    assert_eq!(ops, [("rollback", ra.as_str(), "builtin"), ("rollback", rb.as_str(), ra.as_str()), ("apply", ra.as_str(), rb.as_str())]);
    let o = s.run(&["config", "rollback", "--to", &ra, "--expect-revision", &rb]);
    assert_eq!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stdout).contains("restored revision"));
}

#[test]
fn rollback_failures() {
    let s = Sandbox::new();
    s.set_config(A);
    let ra = content_revision(A.as_bytes());
    // History entry missing.
    let unknown = content_revision(b"never saved");
    let (code, v) = s.json(&["config", "rollback", "--to", &unknown, "--expect-revision", &ra]);
    assert_eq!((code, code_of(&v)), (2, "validation_failed"), "{v}");
    assert!(v["error"]["hint"].as_str().unwrap().contains("config history"));
    // Conflict.
    let (code, v) = s.json(&["config", "rollback", "--to", "builtin", "--expect-revision", &unknown]);
    assert_eq!((code, code_of(&v)), (3, "revision_conflict"));
    // Malformed and missing arguments.
    let (code, _) = s.json(&["config", "rollback", "--to", "yesterday", "--expect-revision", &ra]);
    assert_eq!(code, 2);
    let (code, v) = s.json(&["config", "rollback", "--to", "builtin"]);
    assert_eq!(code, 2);
    assert!(v["error"]["message"].as_str().unwrap().contains("--expect-revision is required"));
    let (code, v) = s.json(&["config", "rollback", "--expect-revision", &ra]);
    assert_eq!(code, 2);
    assert!(v["error"]["message"].as_str().unwrap().contains("--to is required"));
    assert_eq!(s.config_bytes().unwrap(), A.as_bytes());
    assert!(s.entries().is_empty());

    // A saved entry that this tildy cannot load is not restored.
    let bad = fixture("config.v1.invalid-unknown-field.json");
    let rbad = content_revision(bad.as_bytes());
    std::fs::create_dir_all(s.history_dir()).unwrap();
    std::fs::write(s.history_dir().join(format!("1000-{rbad}.json")), &bad).unwrap();
    let (code, v) = s.json(&["config", "rollback", "--to", &rbad, "--expect-revision", &ra]);
    assert_eq!((code, code_of(&v), path_of(&v)), (2, "validation_failed", Some("modules.git_main.colour")), "{v}");
    assert!(v["error"]["hint"].as_str().unwrap().contains(&format!("1000-{rbad}.json")), "the hint names the saved file: {v}");
    // An entry whose content does not match its name is corrupt.
    let fake = content_revision(b"x");
    std::fs::write(s.history_dir().join(format!("1001-{fake}.json")), B).unwrap();
    let (code, v) = s.json(&["config", "rollback", "--to", &fake, "--expect-revision", &ra]);
    assert_eq!((code, code_of(&v)), (1, "internal_error"), "{v}");
    assert_eq!(s.config_bytes().unwrap(), A.as_bytes());
    // Entries without a sidecar are listed with op "unknown".
    let (_, h) = s.json(&["config", "history"]);
    assert_eq!(h["data"]["entries"][0]["op"], "unknown");
    assert!(h["data"]["entries"][0]["path"].is_null());
}

// ---- history -----------------------------------------------------------

#[test]
fn history_keeps_the_newest_50() {
    let s = Sandbox::new();
    std::fs::create_dir_all(s.history_dir()).unwrap();
    for i in 0..55u64 {
        let body = format!("{{\"schema_version\": 1, \"$comment\": \"old {i}\"}}");
        std::fs::write(s.history_dir().join(format!("{}-{}.json", 1_000_000 + i, content_revision(body.as_bytes()))), body).unwrap();
    }
    std::fs::write(s.history_dir().join("README.txt"), "not an entry").unwrap();
    let (_, h) = s.json(&["config", "history"]);
    assert_eq!(h["data"]["entries"].as_array().unwrap().len(), 55, "pruning happens on writes only");
    s.set_config(A);
    let b = s.file("b.json", B);
    let (_, v) = s.json(&["config", "apply", "--file", b.to_str().unwrap(), "--expect-revision", &content_revision(A.as_bytes())]);
    let id = v["data"]["history_id"].as_str().unwrap();
    let (_, h) = s.json(&["config", "history"]);
    let entries = h["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 50);
    assert_eq!(entries[0]["id"], id, "newest first");
    let oldest = entries.last().unwrap()["id"].as_str().unwrap();
    assert!(oldest.starts_with("1000006-"), "the six oldest were pruned: {oldest}");
    assert!(s.history_dir().join("README.txt").exists(), "foreign files are left alone");
    assert_eq!(s.entries().len(), 50);
}

#[test]
fn history_of_a_fresh_install_is_empty() {
    let s = Sandbox::new();
    let (_, h) = s.json(&["config", "history"]);
    assert_eq!(h["data"]["entries"], json!([]));
    assert_eq!(h["data"]["current_revision"], "builtin");
    assert!(!s.history_dir().exists(), "reading the history creates nothing");
    let o = s.run(&["config", "history"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("no history yet"));
}

// ---- environment -------------------------------------------------------

#[test]
fn missing_environment_is_environment_unavailable() {
    let s = Sandbox::new();
    let bare = |args: &[&str]| {
        let mut c = Command::new(BIN);
        c.args(args).arg("--json").current_dir(s.root()).env_clear().env("PATH", "/usr/bin:/bin").stdin(Stdio::null());
        c
    };
    for args in [&["config", "get"][..], &["config", "history"], &["config", "validate"]] {
        let (code, v) = envelope(&bare(args).output().unwrap());
        assert_eq!((code, code_of(&v)), (4, "environment_unavailable"), "{args:?}: {v}");
        assert!(v["error"]["hint"].as_str().unwrap().contains("HOME"));
    }
    // TILDY_CONFIG alone is enough to read; the history needs a state dir.
    let cfg = s.file("explicit.json", B);
    let (code, v) = envelope(&bare(&["config", "get"]).env("TILDY_CONFIG", &cfg).output().unwrap());
    assert_eq!(code, 0);
    assert_eq!(v["data"]["revision"], content_revision(B.as_bytes()));
    let c = s.file("c.json", C);
    let (code, v) = envelope(
        &bare(&["config", "apply", "--file", c.to_str().unwrap(), "--expect-revision", &content_revision(B.as_bytes())])
            .env("TILDY_CONFIG", &cfg)
            .output()
            .unwrap(),
    );
    assert_eq!((code, code_of(&v)), (4, "environment_unavailable"), "{v}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), B);
    // An unreadable config file.
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&cfg).is_err() {
        let (code, v) = envelope(&bare(&["config", "get"]).env("TILDY_CONFIG", &cfg).output().unwrap());
        assert_eq!((code, code_of(&v)), (4, "environment_unavailable"), "{v}");
    }
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn tildy_config_env_var_selects_the_file() {
    let s = Sandbox::new();
    let alt = s.root().join("alt").join("c.json");
    let b = s.file("b.json", B);
    let (code, v) = envelope(
        &s.cmd(&["config", "apply", "--file", b.to_str().unwrap(), "--expect-revision", "builtin", "--json"]).env("TILDY_CONFIG", &alt).output().unwrap(),
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["path"], alt.to_str().unwrap());
    assert_eq!(std::fs::read_to_string(&alt).unwrap(), B);
    assert!(s.config_bytes().is_none());
}

// ---- schema ------------------------------------------------------------

fn schema_file() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/config.v1.schema.json")).unwrap()
}

#[test]
fn schema_prints_the_embedded_schema() {
    let s = Sandbox::new();
    let o = s.run(&["schema"]);
    assert_eq!(o.status.code(), Some(0));
    let text = String::from_utf8(o.stdout).unwrap();
    assert_eq!(text.trim_end(), schema_file().trim_end());
    let (_, v) = envelope(&s.run(&["schema", "--json"]));
    assert_eq!(v["data"]["schema_version"], 1);
    assert_eq!(v["data"]["schema"], serde_json::from_str::<Value>(&schema_file()).unwrap());
    assert_eq!(v["data"]["schema"]["$id"], "tildy://schema/config/v1");
}

fn names(v: &Value) -> BTreeSet<String> {
    v.as_array().unwrap_or_else(|| panic!("not an array: {v}")).iter().map(|s| s.as_str().unwrap().to_string()).collect()
}

fn props(v: &Value) -> BTreeSet<String> {
    v["properties"].as_object().unwrap_or_else(|| panic!("no properties: {v}")).keys().filter(|k| *k != "$comment").cloned().collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Fields the loader accepts in an object, read from its unknown-field error
/// for the probe key `zzz` at `path`.
fn loader_fields(doc: &str, path: &str) -> BTreeSet<String> {
    let e = config::from_str(doc).expect_err(doc);
    assert_eq!(e.path, path, "{doc}: {e}");
    let list = e.message.split_once("allowed: ").unwrap_or_else(|| panic!("{e}")).1.trim_end_matches(')');
    list.split(", ").map(str::to_string).collect()
}

/// Schema v1 freeze: the published schema and the loader agree on enums,
/// on the fields of every object and on which module types take which
/// type-specific field.
#[test]
fn schema_matches_the_loader() {
    let schema: Value = serde_json::from_str(config::SCHEMA_V1_JSON).unwrap();
    let defs = &schema["$defs"];
    let module = &defs["module"];
    let mp = &module["properties"];
    // Enums.
    assert_eq!(names(&mp["type"]["enum"]), set(config::MODULE_TYPES));
    assert_eq!(names(&schema["properties"]["preset"]["enum"]), set(config::PRESETS));
    assert_eq!(names(&schema["properties"]["symbols"]["enum"]), set(config::SYMBOL_SETTINGS));
    assert_eq!(names(&schema["properties"]["history"]["properties"]["mode"]["enum"]), set(config::HISTORY_MODES));
    assert_eq!(names(&defs["row"]["properties"]["style"]["enum"]), set(config::ROW_STYLES));
    assert_eq!(names(&defs["condition"]["properties"]["op"]["enum"]), set(config::OPERATORS));
    assert_eq!(names(&mp["language"]["enum"]), set(config::LANGUAGES));
    assert_eq!(props(&mp["status_symbols"]), set(config::STATUS_SYMBOL_KEYS));
    let ops: Vec<&str> = config::GIT_OPERATIONS.iter().map(|(k, _)| *k).collect();
    assert_eq!(props(&mp["state_labels"]), set(&ops));
    assert_eq!(props(&mp["state_symbols"]), set(config::AGENT_STATES));
    assert_eq!(schema["properties"]["schema_version"]["const"], tildy::model::SCHEMA_VERSION);

    // Fields of every object kind.
    let m = |body: &str| format!(r#"{{"schema_version":1,"modules":{{"x":{body}}},"layout":{{"rows":[{{"left":["x"]}}]}}}}"#);
    let cond = r#"{"field":"a","op":"eq","value":1}"#;
    let cases: Vec<(String, &str, &Value)> = vec![
        (r#"{"schema_version":1,"zzz":1}"#.into(), "zzz", &schema),
        (m(r#"{"type":"text","text":"a","zzz":1}"#), "modules.x.zzz", module),
        (m(r#"{"type":"text","text":"a","style":{"zzz":1}}"#), "modules.x.style.zzz", &defs["style"]),
        (m(r#"{"type":"text","text":"a","symbol":{"zzz":""}}"#), "modules.x.symbol.zzz", &defs["sym"]["oneOf"][1]),
        (m(r#"{"type":"text","text":"a","show_when":{"field":"a","op":"eq","value":1,"zzz":1}}"#), "modules.x.show_when.zzz", &defs["condition"]),
        (m(&format!(r#"{{"type":"text","text":"a","rules":[{{"id":"r","when":{cond},"zzz":1}}]}}"#)), "modules.x.rules[0].zzz", &defs["rule"]),
        (m(r#"{"type":"git","status_symbols":{"zzz":"x"}}"#), "modules.x.status_symbols.zzz", &mp["status_symbols"]),
        (m(r#"{"type":"git","state_labels":{"zzz":"x"}}"#), "modules.x.state_labels.zzz", &mp["state_labels"]),
        (r#"{"schema_version":1,"layout":{"rows":[{"left":["cwd"]}],"zzz":1}}"#.into(), "layout.zzz", &schema["properties"]["layout"]),
        (r#"{"schema_version":1,"layout":{"rows":[{"left":["cwd"],"zzz":1}]}}"#.into(), "layout.rows[0].zzz", &defs["row"]),
        (r#"{"schema_version":1,"history":{"zzz":1}}"#.into(), "history.zzz", &schema["properties"]["history"]),
        (r#"{"schema_version":1,"agent":{"zzz":1}}"#.into(), "agent.zzz", &schema["properties"]["agent"]),
        (m(r#"{"type":"agent_status","state_symbols":{"zzz":"x"}}"#), "modules.x.state_symbols.zzz", &mp["state_symbols"]),
    ];
    for (doc, path, def) in &cases {
        assert_eq!(loader_fields(doc, path), props(def), "fields at {path}");
    }

    // Type-specific module fields: the schema's `if required [type, f] then
    // type in E` rules against the loader's "only valid for" check.
    // Rules of the form `if type == T then required [f]` are the per-type
    // required fields, checked against bare modules below.
    let mut restricted: std::collections::BTreeMap<String, BTreeSet<String>> = Default::default();
    let mut required: std::collections::BTreeMap<String, String> = Default::default();
    for rule in module["allOf"].as_array().unwrap() {
        if let Some(t) = rule["if"]["properties"]["type"]["const"].as_str() {
            let fields = names(&rule["then"]["required"]);
            assert_eq!(fields.len(), 1, "{rule}");
            required.insert(t.to_string(), fields.into_iter().next().unwrap());
            continue;
        }
        let req = names(&rule["if"]["required"]);
        let field = req.iter().find(|f| *f != "type").unwrap_or_else(|| panic!("unexpected rule {rule}")).clone();
        restricted.insert(field, names(&rule["then"]["properties"]["type"]["enum"]));
    }
    for ty in config::MODULE_TYPES {
        let loader = match config::from_str(&m(&format!(r#"{{"type":"{ty}"}}"#))) {
            Ok(_) => None,
            Err(e) => {
                assert!(e.message.contains("required"), "bare {ty}: {e}");
                Some(e.path.strip_prefix("modules.x.").unwrap_or_else(|| panic!("bare {ty}: {e}")).to_string())
            }
        };
        assert_eq!(loader, required.get(*ty).cloned(), "required field of {ty} modules");
    }
    for field in props(module) {
        if field == "type" {
            continue;
        }
        for ty in config::MODULE_TYPES {
            let doc = format!(r#"{{"schema_version":1,"modules":{{"x":{{"type":"{ty}","{field}":null}}}}}}"#);
            let loader_rejects = config::from_str(&doc)
                .err()
                .is_some_and(|e| e.path == format!("modules.x.{field}") && e.message.starts_with("only valid for"));
            let schema_rejects = restricted.get(&field).is_some_and(|types| !types.contains(*ty));
            assert_eq!(loader_rejects, schema_rejects, "field {field} on type {ty}");
        }
    }
}

// ---- end to end --------------------------------------------------------

/// The script in docs/configuration.md between the e2e markers.
fn doc_script() -> String {
    let doc = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/configuration.md")).unwrap();
    let start = doc.find("<!-- BEGIN e2e-script -->").expect("BEGIN marker");
    let end = doc.find("<!-- END e2e-script -->").expect("END marker");
    let block = &doc[start..end];
    let body = block.split_once("```sh\n").expect("sh fence").1;
    body.rsplit_once("```").expect("closing fence").0.to_string()
}

#[test]
fn end_to_end_script_from_docs() {
    if Command::new("python3").arg("--version").output().map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("skipped: python3 is not available");
        return;
    }
    let script = doc_script();
    for start in [None, Some(A)] {
        let s = Sandbox::new();
        if let Some(c) = start {
            s.set_config(c);
        }
        // `tildy` on PATH is the built binary; `preview` (task C) is stubbed.
        let bin = s.root().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let shim = bin.join("tildy");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\nif [ \"$1\" = preview ]; then\n  printf '%s\\n' '{{\"api_version\":1,\"ok\":true,\"data\":{{\"stub\":true}},\"error\":{{\"code\":\"ok\",\"path\":null,\"message\":null,\"hint\":null}}}}'\n  exit 0\nfi\nexec '{BIN}' \"$@\"\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script_path = s.file("e2e.sh", &script);
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let out = Command::new("sh")
            .arg(&script_path)
            .current_dir(s.root())
            .env("PATH", path)
            .env("HOME", s.home())
            .env("XDG_CONFIG_HOME", s.root().join("xdg-config"))
            .env("XDG_STATE_HOME", s.state())
            .env("TMPDIR", s.root())
            .env_remove("TILDY_CONFIG")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(0), "start {start:?}\nstdout: {stdout}\nstderr: {}", String::from_utf8_lossy(&out.stderr));
        let base = start.map(|c| content_revision(c.as_bytes())).unwrap_or_else(|| "builtin".into());
        assert!(stdout.contains(&format!("rolled back to {base}")), "{stdout}");
        // Back where we started, and the applied candidate is in the history.
        assert_eq!(s.config_bytes().as_deref(), start.map(str::as_bytes), "start {start:?}");
        let (_, h) = s.json(&["config", "history"]);
        let entries = h["data"]["entries"].as_array().unwrap();
        assert_eq!(entries[0]["op"], "rollback");
        let saved = std::fs::read_to_string(s.history_dir().join(format!("{}.json", entries[0]["id"].as_str().unwrap()))).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&saved).unwrap()["symbols"], "ascii");
    }
}
