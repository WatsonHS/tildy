//! Real-binary checks for the backend commands on paths that do not reach the
//! config/render/providers implementations (those are covered in-process by
//! backend_worker.rs and backend_oneshot.rs with fakes).

use tildy::diagnostics::{write_snapshot, Counters, Snapshot, WorkerInfo, SNAPSHOT_KIND, SNAPSHOT_SCHEMA_VERSION};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_tildy");

fn render_with(stdin: &[u8]) -> std::process::Output {
    let mut child = Command::new(BIN)
        .arg("render")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn render_protocol_error_exits_5_with_empty_stdout() {
    for input in [&b""[..], b"garbage\n", b"REQ\t1\ts\t/\t80\t24\t0\t0\n", b"REQ\t1\ts\t/x\\q\t80\t24\t0\t0\t\n"] {
        let out = render_with(input);
        assert_eq!(out.status.code(), Some(5), "{input:?}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty(), "{input:?}");
        assert!(String::from_utf8_lossy(&out.stderr).starts_with("tildy render: protocol error"), "{input:?}");
    }
}

#[test]
fn render_bye_exits_0_silently() {
    let out = render_with(b"BYE\n");
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
}

#[test]
fn worker_sends_hello_first() {
    let state = tempfile::tempdir().unwrap();
    let mut child = Command::new(BIN)
        .args(["worker", "--parent-pid", &std::process::id().to_string()])
        .env("XDG_STATE_HOME", state.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let f: Vec<&str> = line.trim_end_matches('\n').split('\t').collect();
    assert_eq!(f[0], "HELLO");
    assert_eq!(f[1], tildy::model::PROTOCOL_VERSION.to_string());
    assert_eq!(f[2], tildy::model::BINARY_VERSION);
    let epoch: u64 = f[3].parse().unwrap();
    assert!(epoch > 0 && epoch < 1 << 53);
    assert_eq!(f[4], child.id().to_string());
    // Provider construction (task D) happens after HELLO; with the M1 stubs it
    // may panic, so only the handshake is asserted here. BYE/EOF end the worker.
    let _ = child.stdin.take().unwrap().write_all(b"BYE\n");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        assert!(std::time::Instant::now() < deadline, "worker did not exit after BYE");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn snapshot(pid: u32) -> Snapshot {
    Snapshot {
        kind: SNAPSHOT_KIND.into(),
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        written_at_ms: tildy::diagnostics::now_ms(),
        worker: WorkerInfo {
            binary_version: tildy::model::BINARY_VERSION.into(),
            protocol_version: 1,
            epoch: 99,
            pid,
            parent_pid: None,
            started_at_ms: 0,
        },
        session_id: Some("777.42".into()),
        config_revision: Some("builtin".into()),
        request: None,
        request_received_at_ms: None,
        last_result: None,
        slots: vec![],
        providers: vec![],
        counters: Counters { requests: 2, ..Counters::default() },
        degraded: vec![],
        watch: Default::default(),
        module_reports: vec![],
        timing_history: vec![],
    }
}

#[test]
fn inspect_reads_snapshot_as_json_and_text() {
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("tildy/sessions/777.42.json");
    write_snapshot(&path, &snapshot(std::process::id())).unwrap();
    let out = Command::new(BIN)
        .args(["inspect", "--session", "777.42", "--json"])
        .env("XDG_STATE_HOME", state.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["api_version"], 1);
    assert_eq!(v["data"]["worker_alive"], true);
    assert_eq!(v["data"]["snapshot"]["worker"]["epoch"], 99);
    assert_eq!(v["data"]["snapshot"]["counters"]["requests"], 2);

    let out = Command::new(BIN)
        .args(["inspect", "--session", "777.42"])
        .env("XDG_STATE_HOME", state.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("session   777.42  worker pid"), "{text}");
    assert!(text.contains("counters  2 requests"), "{text}");
}

#[test]
fn inspect_missing_session_is_backend_unavailable() {
    let state = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args(["inspect", "--session", "../../nope", "--json"])
        .env("XDG_STATE_HOME", state.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "backend_unavailable");
    assert!(v["error"]["message"].as_str().unwrap().contains("_.._.._nope.json"), "path is sanitized");
}
