//! `tildy agent publish|list|clear` against a sandboxed state directory, and
//! the `agent_status` module end to end through `tildy render`.

use serde_json::Value;
use tildy::agent::base64_encode;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_tildy");

struct Sandbox {
    _dir: tempfile::TempDir,
    state: PathBuf,
    plain: PathBuf,
    session: Option<String>,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        Sandbox { state, plain, session: Some("4242.17".into()), _dir: dir }
    }

    /// The same store and directories as another shell session (`None`: no
    /// TILDY_SESSION_ID at all).
    fn as_session(&self, session: Option<&str>) -> Sandbox {
        Sandbox { _dir: tempfile::tempdir().unwrap(), state: self.state.clone(), plain: self.plain.clone(), session: session.map(str::to_string) }
    }

    fn cmd(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.arg("agent").args(args).current_dir(cwd).env("XDG_STATE_HOME", &self.state).env("HOME", self._dir.path()).env_remove("GIT_DIR");
        match &self.session {
            Some(s) => c.env("TILDY_SESSION_ID", s),
            None => c.env_remove("TILDY_SESSION_ID"),
        };
        c
    }

    fn run_in(&self, cwd: &Path, args: &[&str]) -> Output {
        self.cmd(cwd, args).output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.plain, args)
    }

    /// `--json` envelope and exit code.
    fn json_in(&self, cwd: &Path, args: &[&str]) -> (Value, i32) {
        let mut a = args.to_vec();
        a.push("--json");
        let out = self.run_in(cwd, &a);
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{args:?}: {e}: {}", String::from_utf8_lossy(&out.stdout)));
        assert_eq!(v["api_version"], 1);
        (v, out.status.code().unwrap())
    }

    fn json(&self, args: &[&str]) -> (Value, i32) {
        self.json_in(&self.plain, args)
    }

    fn records(&self, cwd: &Path, args: &[&str]) -> Vec<Value> {
        let mut a = vec!["list"];
        a.extend_from_slice(args);
        let (v, code) = self.json_in(cwd, &a);
        assert_eq!(code, 0, "{v}");
        v["data"]["records"].as_array().unwrap().clone()
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn repo(parent: &Path, name: &str) -> PathBuf {
    let r = parent.join(name);
    std::fs::create_dir_all(r.join("sub")).unwrap();
    git(&r, &["init", "-q"]);
    r
}

fn err_of(v: &Value) -> (&str, Option<&str>) {
    (v["error"]["code"].as_str().unwrap(), v["error"]["path"].as_str())
}

#[test]
fn publish_flags_list_and_file_layout() {
    let sb = Sandbox::new();
    let (v, code) = sb.json(&[
        "publish", "--state", "blocked", "--kind", "permission", "--app", "claude", "--id", "wrap/claude/7", "--msg", "Bash: rm -rf target",
        "--title", "tildy", "--progress", "30",
    ]);
    assert_eq!(code, 0, "{v}");
    let r = &v["data"]["record"];
    assert_eq!(v["data"]["action"], "published");
    assert_eq!((r["state"].as_str(), r["kind"].as_str(), r["app"].as_str()), (Some("blocked"), Some("permission"), Some("claude")));
    assert_eq!((r["seq"].as_u64(), r["ttl_s"].as_u64(), r["progress"].as_u64()), (Some(1), Some(7200), Some(30)));
    assert_eq!(r["scope"], serde_json::json!({"kind": "global", "id": "*"}), "outside a worktree: global");
    assert_eq!((r["origin_session"].as_str(), r["source"].as_str()), (Some("4242.17"), Some("cli")));
    let path = PathBuf::from(r["path"].as_str().unwrap());
    assert!(path.starts_with(sb.state.join("tildy/agent/global")));
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(stored.get("path").is_none() && stored["received_at_ms"].is_u64());

    let recs = sb.records(&sb.plain, &[]);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["msg"], "Bash: rm -rf target");
    assert_eq!(recs[0]["expired"], false);
    // Text mode.
    let out = sb.run(&["list"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("blocked/permission"), "{text}");
    assert!(text.contains("claude wrap/claude/7 30% [tildy] Bash: rm -rf target"), "{text}");
    // kind/progress are dropped where 7501 does not use them.
    let (v, _) = sb.json(&["publish", "--state", "done", "--kind", "auth", "--progress", "5", "--id", "x", "--no-origin"]);
    let r = &v["data"]["record"];
    assert!(r.get("kind").is_none() && r.get("progress").is_none() && r.get("origin_session").is_none(), "{r}");
    assert_eq!(r["ttl_s"], 900);
}

#[test]
fn scope_resolution() {
    let sb = Sandbox::new();
    let top = sb.plain.parent().unwrap().join("repos");
    let a = repo(&top, "a");
    let b = repo(&top, "b");
    let (v, code) = sb.json_in(&a.join("sub"), &["publish", "--state", "working", "--app", "codex", "--msg", "in a"]);
    assert_eq!(code, 0, "{v}");
    let wt = v["data"]["record"]["scope"].clone();
    assert_eq!(wt["kind"], "worktree");
    let canon = std::fs::canonicalize(&a).unwrap();
    assert!(wt["id"].as_str().unwrap().ends_with(&format!("@{}", canon.display())), "{wt}");
    // Same worktree from another directory sees it; another repo does not.
    assert_eq!(sb.records(&a, &[]).len(), 1);
    assert_eq!(sb.records(&b, &[]).len(), 0);
    assert_eq!(sb.records(&sb.plain, &[]).len(), 0);
    assert_eq!(sb.records(&b, &["--all"]).len(), 1);
    assert_eq!(sb.records(&b, &["--scope", "worktree", "--cwd", a.to_str().unwrap()]).len(), 1);
    // --scope session uses TILDY_SESSION_ID; another session does not see it.
    let (v, _) = sb.json(&["publish", "--state", "idle", "--scope", "session", "--app", "amp"]);
    assert_eq!(v["data"]["record"]["scope"], serde_json::json!({"kind": "session", "id": "4242.17"}));
    assert_eq!(sb.records(&sb.plain, &[]).len(), 1);
    let other = sb.as_session(Some("999.1"));
    assert_eq!(other.records(&sb.plain, &[]).len(), 0);
    assert_eq!(other.records(&sb.plain, &["--scope", "session", "--scope-id", "4242.17"]).len(), 1);
    // Errors.
    let no_session = sb.as_session(None);
    let (v, code) = no_session.json(&["publish", "--state", "idle", "--scope", "session"]);
    assert_eq!((code, err_of(&v).0), (4, "environment_unavailable"));
    let (v, code) = sb.json(&["publish", "--state", "idle", "--scope", "worktree"]);
    assert_eq!((code, err_of(&v).0), (4, "environment_unavailable"), "plain dir is not a worktree");
    let (v, code) = sb.json(&["publish", "--state", "idle", "--scope-id", "x"]);
    assert_eq!((code, err_of(&v)), (2, ("validation_failed", Some("scope"))));
    let (v, code) = sb.json(&["publish", "--state", "idle", "--scope", "galaxy"]);
    assert_eq!((code, err_of(&v)), (2, ("validation_failed", Some("scope"))));
    let (v, code) = sb.json(&["publish", "--state", "idle", "--scope", "global", "--scope-id", "x"]);
    assert_eq!((code, err_of(&v)), (2, ("validation_failed", Some("scope_id"))));
    let (v, code) = sb.json(&["publish", "--state", "idle", "--scope", "worktree", "--scope-id", "custom-wt"]);
    assert_eq!((code, v["data"]["record"]["scope"]["id"].as_str()), (0, Some("custom-wt")));
}

#[test]
fn validation_and_seq_errors() {
    let sb = Sandbox::new();
    let cases: &[(&[&str], &str)] = &[
        (&["publish", "--state", "sleeping"], "state"),
        (&["publish", "--app", "x"], "state"),
        (&["publish", "--state", "blocked", "--kind", "bribe"], "kind"),
        (&["publish", "--state", "working", "--progress", "101"], "progress"),
        (&["publish", "--state", "idle", "--id", "a//b"], "id"),
        (&["publish", "--state", "idle", "--app", "has space"], "app"),
        (&["publish", "--state", "idle", "--msg", "two\nlines"], "msg"),
        (&["publish", "--state", "idle", "--msg", "esc \u{1b}[31m"], "msg"),
        (&["publish", "--state", "idle", "--title", &"t".repeat(193)], "title"),
        (&["publish", "--state", "idle", "--ttl", "0"], "ttl_s"),
        (&["publish", "--state", "idle", "--ttl", "86401"], "ttl_s"),
        (&["publish", "--state", "idle", "--seq", "0"], "seq"),
        (&["publish", "--state", "idle", "--origin", "a\tb"], "origin_session"),
        (&["publish", "--osc", "kind=auth"], "osc.state"),
        (&["publish", "--osc", "state=idle:msg=Z"], "osc.msg"),
        (&["clear", "--id", "a b"], "id"),
    ];
    for (args, path) in cases {
        let (v, code) = sb.json(args);
        assert_eq!((code, err_of(&v)), (2, ("validation_failed", Some(*path))), "{args:?}: {v}");
    }
    let long = "m".repeat(2048);
    let (_, code) = sb.json(&["publish", "--state", "idle", "--msg", &long, "--id", "max"]);
    assert_eq!(code, 0, "2048 bytes is the limit");
    let (v, code) = sb.json(&["publish", "--state", "idle", "--msg", &format!("{long}x")]);
    assert_eq!((code, err_of(&v).1), (2, Some("msg")));
    assert_eq!(sb.records(&sb.plain, &[]).len(), 1, "rejected records are not stored");

    // Duplicate and older seq are refused; a larger one or none is accepted.
    let p = |seq: &str| sb.json(&["publish", "--state", "working", "--id", "job", "--seq", seq]);
    assert_eq!(p("3").1, 0);
    let (v, code) = p("3");
    assert_eq!((code, err_of(&v)), (3, ("revision_conflict", Some("seq"))));
    assert!(v["error"]["hint"].as_str().unwrap().contains("--seq 4"));
    assert_eq!(p("2").1, 3);
    assert_eq!(p("4").1, 0);
    let (v, _) = sb.json(&["publish", "--state", "done", "--id", "job"]);
    assert_eq!(v["data"]["record"]["seq"], 5);
    // Text-mode errors go to stderr with the field.
    let out = sb.run(&["publish", "--state", "working", "--id", "job", "--seq", "1"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("tildy agent publish: seq: seq 1 is not newer than 5"));
}

#[test]
fn osc_bodies_and_files() {
    let sb = Sandbox::new();
    let msg = base64_encode("Bash: rm -rf target".as_bytes());
    let body = format!("state=blocked:kind=permission:msg={msg}:app=claude:progress=200:future=1");
    let (v, code) = sb.json(&["publish", "--osc", &body, "--id", "wrap/claude/1"]);
    assert_eq!(code, 0, "{v}");
    let r = &v["data"]["record"];
    assert_eq!((r["msg"].as_str(), r["source"].as_str(), r["id"].as_str()), (Some("Bash: rm -rf target"), Some("osc"), Some("wrap/claude/1")));
    assert!(r.get("progress").is_none(), "out-of-range progress is absent in 7501");
    let (v, code) = sb.json(&["publish", "--osc", "7501;state=working:id=wrap/claude/2:app=claude"]);
    assert_eq!((code, v["data"]["record"]["id"].as_str()), (0, Some("wrap/claude/2")));
    // state=clear with an id removes that record and its children.
    sb.json(&["publish", "--osc", "state=idle:id=wrap/claude/1/sub"]);
    let (v, code) = sb.json(&["publish", "--osc", "state=clear:id=wrap/claude/1"]);
    assert_eq!((code, v["data"]["action"].as_str(), v["data"]["count"].as_u64()), (0, Some("cleared"), Some(2)));
    let (v, _) = sb.json(&["publish", "--state", "clear", "--app", "claude"]);
    assert_eq!(v["data"]["count"], 1);
    assert!(sb.records(&sb.plain, &[]).is_empty());
    let out = sb.run(&["publish", "--osc", "state=idle", "--state", "idle"]);
    assert_eq!(out.status.code(), Some(2), "--osc conflicts with --state (clap usage error)");

    // --file: a schema v1 record; unknown fields and bad values are refused.
    let f = sb.plain.join("rec.json");
    std::fs::write(&f, r#"{"protocol_version":1,"id":"ci/build","app":"make","state":"error","msg":"tests failed","scope":{"kind":"global","id":"*"},"seq":7,"ttl_s":60}"#).unwrap();
    let (v, code) = sb.json(&["publish", "--file", f.to_str().unwrap()]);
    assert_eq!(code, 0, "{v}");
    assert_eq!((v["data"]["record"]["seq"].as_u64(), v["data"]["record"]["ttl_s"].as_u64()), (Some(7), Some(60)));
    std::fs::write(&f, r#"{"protocol_version":1,"state":"idle","scope":{"kind":"global","id":"*"},"colour":"red"}"#).unwrap();
    let (v, code) = sb.json(&["publish", "--file", f.to_str().unwrap()]);
    assert_eq!((code, err_of(&v).1), (2, Some("file")));
    std::fs::write(&f, r#"{"protocol_version":1,"state":"idle","scope":{"kind":"global","id":"elsewhere"}}"#).unwrap();
    let (v, code) = sb.json(&["publish", "--file", f.to_str().unwrap()]);
    assert_eq!((code, err_of(&v).1), (2, Some("scope.id")));
    // stdin.
    let mut child = sb.cmd(&sb.plain, &["publish", "--file", "-", "--json"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"protocol_version":1,"state":"done","app":"make","scope":{"kind":"global","id":"*"},"seq":1}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn clear_and_expiry() {
    let sb = Sandbox::new();
    for (id, app) in [("a", "x"), ("a/b", "x"), ("c", "y")] {
        assert_eq!(sb.json(&["publish", "--state", "working", "--id", id, "--app", app]).1, 0);
    }
    let (v, _) = sb.json(&["clear", "--id", "a"]);
    assert_eq!(v["data"]["count"], 2);
    let (v, _) = sb.json(&["clear", "--app", "nobody"]);
    assert_eq!(v["data"]["count"], 0);
    // TTL: gone from list after expiry, visible with --expired, purged by clear --expired.
    assert_eq!(sb.json(&["publish", "--state", "done", "--id", "short", "--ttl", "1"]).1, 0);
    assert_eq!(sb.records(&sb.plain, &[]).len(), 2);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let live = sb.records(&sb.plain, &[]);
    assert_eq!(live.iter().map(|r| r["id"].as_str().unwrap()).collect::<Vec<_>>(), ["c"]);
    let all = sb.records(&sb.plain, &["--expired"]);
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|r| r["id"] == "short" && r["expired"] == true));
    let (v, code) = sb.json(&["clear", "--expired"]);
    assert_eq!((code, v["data"]["action"].as_str(), v["data"]["count"].as_u64()), (0, Some("expired"), Some(1)));
    assert_eq!(sb.records(&sb.plain, &["--expired"]).len(), 1);
    let (v, _) = sb.json(&["clear"]);
    assert_eq!(v["data"]["count"], 1);
    assert!(sb.records(&sb.plain, &["--all", "--expired"]).is_empty());
}

#[test]
fn no_state_directory_is_exit_4() {
    let mut c = Command::new(BIN);
    let out = c.args(["agent", "list", "--json"]).env_remove("HOME").env_remove("XDG_STATE_HOME").output().unwrap();
    assert_eq!(out.status.code(), Some(4));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "environment_unavailable");
}

/// The builtin config shows the highlighted record on the right of the prompt.
#[test]
fn render_shows_the_summary() {
    let sb = Sandbox::new();
    // The module is opt-in: place it on the right of the built-in row.
    let cfg = sb.plain.join("agent-config.json");
    std::fs::write(
        &cfg,
        r#"{"schema_version":1,"symbols":"unicode","modules":{"agent":{"type":"agent_status"}},"layout":{"rows":[{"left":["cwd"],"right":["agent"]}]}}"#,
    )
    .unwrap();
    let render = |session: &str| -> String {
        let mut child = Command::new(BIN)
            .arg("render")
            .env("XDG_STATE_HOME", &sb.state)
            .env("TILDY_CONFIG", &cfg)
            .env("TILDY_SYMBOLS", "unicode")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let req = format!("REQ\t1\t{session}\t{}\t120\t24\t0\t0\t\t0\n", sb.plain.display());
        child.stdin.take().unwrap().write_all(req.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0));
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(!render("4242.17").contains("\nR\t"), "no records: no right prompt");
    sb.json(&["publish", "--state", "blocked", "--kind", "permission", "--app", "claude", "--msg", "Bash: rm -rf target", "--scope", "session"]);
    sb.json(&["publish", "--state", "working", "--app", "cargo", "--id", "build"]);
    let res = render("4242.17");
    let right = res.lines().find(|l| l.starts_with("R\t")).unwrap_or_else(|| panic!("{res}"));
    assert_eq!(right, "R\t%F{yellow}⏳ claude Bash: rm -rf target +1%f");
    let other = render("1.2");
    let right = other.lines().find(|l| l.starts_with("R\t")).unwrap();
    assert_eq!(right, "R\t%F{cyan}◐ cargo%f", "another session sees only the global record");
}

/// `tildy init zsh` carries the agent program list from the effective config.
#[test]
fn init_emits_agent_programs() {
    let sb = Sandbox::new();
    let init = |config: Option<&str>| -> String {
        let cfg = sb.plain.join("config.json");
        match config {
            Some(c) => std::fs::write(&cfg, c).unwrap(),
            None => {
                let _ = std::fs::remove_file(&cfg);
            }
        }
        let out = Command::new(BIN).args(["init", "zsh"]).env("TILDY_CONFIG", &cfg).output().unwrap();
        assert_eq!(out.status.code(), Some(0));
        String::from_utf8(out.stdout).unwrap()
    };
    let line = |s: &str| s.lines().find(|l| l.starts_with("_tildy_agent_programs=")).unwrap().to_string();
    // Opt-in since 2026-10-20: nothing is wrapped unless `agent.wrap` is true.
    let default = init(None);
    assert!(default.contains("'builtin' 'typeset' '-ga' _tildy_agent_programs\n"));
    assert_eq!(line(&default), "_tildy_agent_programs=()");
    assert_eq!(
        line(&init(Some(r#"{"schema_version":1,"agent":{"wrap":true}}"#))),
        "_tildy_agent_programs=('claude' 'codex' 'gemini' 'aider' 'opencode' 'amp' 'goose' 'copilot' 'cursor-agent')"
    );
    assert_eq!(line(&init(Some(r#"{"schema_version":1,"agent":{"programs":["claude","my-agent"]}}"#))), "_tildy_agent_programs=()");
    assert_eq!(line(&init(Some(r#"{"schema_version":1,"agent":{"wrap":true,"programs":["claude","my-agent"]}}"#))), "_tildy_agent_programs=('claude' 'my-agent')");
    assert_eq!(line(&init(Some(r#"{"schema_version":1,"agent":{"wrap":false}}"#))), "_tildy_agent_programs=()");
    // An invalid config falls back to the built-in list (the prompt does too).
    assert_eq!(line(&init(Some("{"))), line(&default));
    // Program names are validated by the config.
    let out = Command::new(BIN).args(["capabilities", "--json"]).env("TILDY_CONFIG", sb.plain.join("config.json")).output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["config"]["valid"], false);
    std::fs::write(sb.plain.join("config.json"), r#"{"schema_version":1,"agent":{"programs":["rm -rf"]}}"#).unwrap();
    let out = Command::new(BIN).args(["capabilities", "--json"]).env("TILDY_CONFIG", sb.plain.join("config.json")).output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["data"]["config"]["error"].as_str().unwrap().contains("agent.programs[0]"), "{v}");
    std::fs::write(sb.plain.join("config.json"), r#"{"schema_version":1,"agent":{"wrap":false}}"#).unwrap();
    let out = Command::new(BIN).args(["capabilities", "--json"]).env("TILDY_CONFIG", sb.plain.join("config.json")).output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["agent"]["wrap"], false);
    assert_eq!(v["data"]["agent"]["states"], serde_json::json!(["idle", "working", "done", "blocked", "error"]));
}
