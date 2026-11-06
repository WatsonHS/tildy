//! `tildy inspect --session ID --json`: read the worker snapshot. Owner: task B.
use clap::Args;
use serde::Serialize;
use tildy::diagnostics::{self, Snapshot};
use tildy::model::{CliEnvelope, ErrorCode};
use tildy::session;
use std::fmt::Write as _;
use std::path::PathBuf;

#[derive(Args)]
pub struct InspectArgs {
    #[arg(long)]
    #[arg(required_unless_present = "list", conflicts_with = "list")]
    pub session: Option<String>,
    /// List live workers with readable session snapshots.
    #[arg(long)]
    pub list: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct InspectData {
    path: PathBuf,
    /// The worker pid from the snapshot still exists (`kill(pid, 0)`).
    worker_alive: bool,
    snapshot: Snapshot,
}

pub fn run(a: InspectArgs) -> i32 {
    if a.list {
        let mut sessions = vec![];
        let Some(dir) = session::sessions_dir() else {
            return fail(
                a.json,
                ErrorCode::EnvironmentUnavailable,
                "cannot resolve the state directory".into(),
                None,
            );
        };
        {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_none_or(|e| e != "json") {
                        continue;
                    }
                    if let Ok(snapshot) = diagnostics::read_snapshot(&path) {
                        if session::pid_alive(snapshot.worker.pid) {
                            sessions.push(InspectData {
                                path,
                                worker_alive: true,
                                snapshot,
                            });
                        }
                    }
                }
            }
        }
        sessions.sort_by(|a, b| a.snapshot.session_id.cmp(&b.snapshot.session_id));
        if a.json {
            emit(&sessions);
        } else {
            for d in sessions {
                println!(
                    "{}  pid={}  cwd={}",
                    d.snapshot.session_id.as_deref().unwrap_or("?"),
                    d.snapshot.worker.pid,
                    d.snapshot
                        .request
                        .map(|r| r.cwd.display().to_string())
                        .unwrap_or_default()
                );
            }
        }
        return 0;
    }
    let (path, snapshot) = match load_session(a.session.as_deref().unwrap_or_default()) {
        Ok(v) => v,
        Err((code, message)) => return fail(a.json, code, message, None),
    };
    let data = InspectData {
        worker_alive: session::pid_alive(snapshot.worker.pid),
        path,
        snapshot,
    };
    if a.json {
        print_json(&CliEnvelope::ok(data));
    } else {
        print!("{}", summary(&data));
    }
    0
}

pub(crate) fn load_session(id: &str) -> Result<(PathBuf, Snapshot), (ErrorCode, String)> {
    let path = session::snapshot_path(id).ok_or((
        ErrorCode::EnvironmentUnavailable,
        "cannot resolve the state directory".into(),
    ))?;
    let snapshot = diagnostics::read_snapshot(&path).map_err(|e| {
        (
            if e.kind() == std::io::ErrorKind::NotFound {
                ErrorCode::BackendUnavailable
            } else {
                ErrorCode::InternalError
            },
            format!("cannot read session {id} at {}: {e}", path.display()),
        )
    })?;
    Ok((path, snapshot))
}
pub(crate) fn emit<T: Serialize>(data: &T) {
    print_json(&CliEnvelope::ok(data));
}

pub(crate) fn fail(json: bool, code: ErrorCode, message: String, hint: Option<String>) -> i32 {
    if json {
        print_json(&CliEnvelope::<()>::err(code, message, hint));
    } else {
        eprintln!("tildy inspect: {message}");
        if let Some(h) = hint {
            eprintln!("hint: {h}");
        }
    }
    code.exit_code()
}

fn print_json<T: Serialize>(v: &T) {
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("tildy inspect: cannot encode JSON: {e}"),
    }
}

fn ago(now_ms: u64, at_ms: u64) -> String {
    let d = now_ms.saturating_sub(at_ms);
    if d < 1000 {
        format!("{d} ms ago")
    } else {
        format!("{:.1} s ago", d as f64 / 1000.0)
    }
}

fn summary(d: &InspectData) -> String {
    let s = &d.snapshot;
    let now = diagnostics::now_ms();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "session   {}  worker pid {} ({}), epoch {}, v{} protocol {}",
        s.session_id.as_deref().unwrap_or("?"),
        s.worker.pid,
        if d.worker_alive { "alive" } else { "gone" },
        s.worker.epoch,
        s.worker.binary_version,
        s.worker.protocol_version
    );
    if let Some(r) = &s.request {
        let _ = writeln!(
            out,
            "request   gen {}  cwd {}  {}x{}  exit {}  took {} ms",
            r.gen,
            r.cwd.display(),
            r.cols,
            r.lines,
            r.last_exit,
            r.last_duration_ms
        );
    }
    if let Some(r) = &s.last_result {
        let status = match r.status {
            tildy::model::RenderStatus::Fresh => "fresh",
            tildy::model::RenderStatus::Partial => "partial",
        };
        let _ = writeln!(
            out,
            "result    gen {} {status}  {} after request, sent {}",
            r.gen,
            fmt_ms(r.latency_ms),
            ago(now, r.sent_at_ms)
        );
    }
    for slot in &s.slots {
        let freshness = serde_json::to_value(slot.freshness)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let age = slot
            .age_ms
            .map(|a| format!(" age {a} ms"))
            .unwrap_or_default();
        let fetching = if slot.fetching { " (fetching)" } else { "" };
        let err = slot
            .error
            .as_deref()
            .map(|e| format!("  {e}"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "slot      {} {freshness}{age}{fetching}{err}",
            slot.provider
        );
    }
    for p in &s.providers {
        let _ = writeln!(
            out,
            "provider  {}: {} runs, last {}, p50 {}, max {}, {} timeouts, {} coalesced, {} cache hits",
            p.key,
            p.runs,
            p.last_ms.map(fmt_ms).unwrap_or_else(|| "-".into()),
            p.p50_ms.map(fmt_ms).unwrap_or_else(|| "-".into()),
            p.max_ms.map(fmt_ms).unwrap_or_else(|| "-".into()),
            p.stats.timeouts,
            p.stats.coalesced,
            p.stats.cache_hits
        );
    }
    let c = &s.counters;
    let _ = writeln!(
        out,
        "counters  {} requests ({} dropped), {} partial, {} fresh, {} protocol errors",
        c.requests, c.dropped_requests, c.partial_sent, c.fresh_sent, c.protocol_errors
    );
    let _ = writeln!(
        out,
        "degraded  {}",
        if s.degraded.is_empty() {
            "none".into()
        } else {
            s.degraded.join(", ")
        }
    );
    let _ = writeln!(
        out,
        "snapshot  {} (written {})",
        d.path.display(),
        ago(now, s.written_at_ms)
    );
    out
}

fn fmt_ms(ms: f64) -> String {
    format!("{ms:.2} ms")
}
