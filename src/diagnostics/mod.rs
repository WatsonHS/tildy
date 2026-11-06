//! Snapshot file and timing records. Owner: task B.
//!
//! The worker writes a [`Snapshot`] after every RES it sends. `tildy inspect`
//! reads it. Writes are atomic (temp file in the same directory + rename) and
//! the file is private (0600, directory 0700) because it contains paths.

use crate::model::{Freshness, PromptRequest, RenderStatus};
use crate::scheduler::ProviderStats;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
pub const SNAPSHOT_KIND: &str = "tildy.session_snapshot";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub kind: String,
    pub schema_version: u32,
    pub written_at_ms: u64,
    pub worker: WorkerInfo,
    pub session_id: Option<String>,
    pub config_revision: Option<String>,
    /// Latest request received (the only one that is served).
    pub request: Option<PromptRequest>,
    pub request_received_at_ms: Option<u64>,
    pub last_result: Option<ResultInfo>,
    pub slots: Vec<SlotInfo>,
    pub providers: Vec<ProviderTiming>,
    pub counters: Counters,
    /// Reasons the worker is running in a reduced mode (e.g. `config_invalid`).
    pub degraded: Vec<String>,
    #[serde(default)]
    pub watch: WatchDiagnostics,
    #[serde(default)]
    pub module_reports: Vec<crate::model::ModuleReport>,
    #[serde(default)]
    pub timing_history: Vec<TimingCycle>,
}

/// Watcher diagnostics; timestamps are Unix milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchDiagnostics {
    pub mode: String,
    pub last_event_ms: Option<u64>,
    pub coalesced_events: u64,
    pub fallback_count: u64,
}
impl Default for WatchDiagnostics {
    fn default() -> Self {
        Self { mode: "off".into(), last_event_ms: None, coalesced_events: 0, fallback_count: 0 }
    }
}
/// One actual render/refresh cycle, never cumulative scheduler counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingCycle {
    pub gen: u64,
    pub at_ms: u64,
    pub providers: Vec<ProviderCycleTiming>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCycleTiming {
    pub provider: String,
    pub duration_ms: u64,
    pub cache_hit: bool,
    pub executed: bool,
    /// A fetch result arrived in this cycle, including shared in-flight work.
    #[serde(default)]
    pub completed: bool,
    pub timed_out: bool,
    pub coalesced: bool,
    #[serde(default)]
    pub coalesced_count: u64,
}
pub const TIMING_HISTORY_LIMIT: usize = 32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub binary_version: String,
    pub protocol_version: u32,
    /// Random per-process identity, below 2^53 so JSON readers keep it exact.
    pub epoch: u64,
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultInfo {
    pub gen: u64,
    pub status: RenderStatus,
    pub sent_at_ms: u64,
    /// Time from receiving the REQ to flushing this RES.
    pub latency_ms: f64,
    pub left_lines: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotInfo {
    pub provider: String,
    pub freshness: Freshness,
    pub age_ms: Option<u64>,
    pub error: Option<String>,
    pub fetching: bool,
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderTiming {
    pub key: String,
    pub runs: u64,
    pub errors: u64,
    pub last_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub max_ms: Option<f64>,
    pub stats: ProviderStats,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub requests: u64,
    /// REQs skipped because a newer REQ was already queued.
    pub dropped_requests: u64,
    pub protocol_errors: u64,
    pub partial_sent: u64,
    pub fresh_sent: u64,
}

/// Milliseconds since the Unix epoch.
pub fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn now_ms() -> u64 {
    unix_ms(SystemTime::now())
}

pub fn duration_ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e6).round() / 1e3
}

/// Atomically replace `path` with the JSON encoding of `snapshot`.
pub fn write_snapshot(path: &Path, snapshot: &Snapshot) -> io::Result<()> {
    write_atomic_with(path, |file| {
        let mut buffered = BufWriter::with_capacity(8192, file);
        serde_json::to_writer_pretty(&mut buffered, snapshot).map_err(io::Error::other)?;
        buffered.flush()
    })
}

/// Write `bytes` to a temp file next to `path`, then rename over `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with(path, |file| file.write_all(bytes))
}

fn write_atomic_with(path: &Path, write: impl FnOnce(&mut fs::File) -> io::Result<()>) -> io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    create_private_dir(dir)?;
    let name = path.file_name().ok_or_else(|| io::Error::other("snapshot path has no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = dir.join(tmp_name);
    let result = (|| {
        let mut f = open_private(&tmp)?;
        write(&mut f)?;
        f.write_all(b"\n")?;
        f.flush()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn read_snapshot(path: &Path) -> io::Result<Snapshot> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Remove `*.json` snapshots in `dir` not modified for `max_age`. Returns the
/// number removed. Errors on individual files are ignored.
pub fn prune_snapshots(dir: &Path, max_age: Duration) -> usize {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_snapshot = path.extension().is_some_and(|e| e == "json" || e == "tmp");
        let old = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > max_age);
        if is_snapshot && old && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn open_private(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)
}

#[cfg(not(unix))]
fn open_private(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().write(true).create(true).truncate(true).open(path)
}

/// Writes snapshots on a background thread so disk latency (slow or network
/// home directories) never delays a frame. Only the latest submitted snapshot
/// is written; older pending ones are replaced.
pub struct SnapshotWriter {
    shared: Arc<(Mutex<WriterState>, Condvar)>,
    handle: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct WriterState {
    pending: Option<(PathBuf, Snapshot)>,
    writing: bool,
    stop: bool,
    written: Option<PathBuf>,
    error_reported: bool,
}

impl SnapshotWriter {
    pub fn start() -> Self {
        let shared = Arc::new((Mutex::new(WriterState::default()), Condvar::new()));
        let worker_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("tildy-snapshot".into())
            .spawn(move || writer_loop(&worker_shared))
            .ok();
        SnapshotWriter { shared, handle }
    }

    /// Queue `snapshot` for `path`, replacing any snapshot not yet written.
    pub fn submit(&self, path: PathBuf, snapshot: Snapshot) {
        if self.handle.is_none() {
            // No thread could be started: write inline.
            let ok = write_snapshot(&path, &snapshot).is_ok();
            let mut st = lock(&self.shared.0);
            if ok {
                st.written = Some(path);
            }
            return;
        }
        let (m, cv) = &*self.shared;
        lock(m).pending = Some((path, snapshot));
        cv.notify_all();
    }

    /// Block until every submitted snapshot has been written (tests, tools).
    pub fn flush(&self) {
        let (m, cv) = &*self.shared;
        let mut st = lock(m);
        while (st.pending.is_some() || st.writing) && self.handle.is_some() {
            st = cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Stop the thread (dropping any unwritten snapshot) and, with `remove`,
    /// delete the last file written.
    pub fn shutdown(mut self, remove: bool) {
        {
            let (m, cv) = &*self.shared;
            let mut st = lock(m);
            st.pending = None;
            st.stop = true;
            cv.notify_all();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if remove {
            if let Some(p) = lock(&self.shared.0).written.take() {
                let _ = fs::remove_file(p);
            }
        }
    }
}

fn lock(m: &Mutex<WriterState>) -> std::sync::MutexGuard<'_, WriterState> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn writer_loop(shared: &(Mutex<WriterState>, Condvar)) {
    let (m, cv) = shared;
    loop {
        let (path, snapshot) = {
            let mut st = lock(m);
            while st.pending.is_none() && !st.stop {
                st = cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            if st.stop {
                return;
            }
            st.writing = true;
            st.pending.take().expect("checked above")
        };
        let result = write_snapshot(&path, &snapshot);
        let mut st = lock(m);
        st.writing = false;
        match result {
            Ok(()) => st.written = Some(path),
            Err(e) if !st.error_reported => {
                st.error_reported = true;
                eprintln!("tildy worker: cannot write snapshot {}: {e}", path.display());
            }
            Err(_) => {}
        }
        cv.notify_all();
    }
}

/// Per-provider fetch durations (last, p50 and max over a small window).
#[derive(Debug, Default, Clone)]
pub struct TimingRecorder {
    keys: BTreeMap<String, KeyTimings>,
}

#[derive(Debug, Default, Clone)]
struct KeyTimings {
    runs: u64,
    errors: u64,
    last: Option<Duration>,
    max: Option<Duration>,
    window: VecDeque<Duration>,
}

const TIMING_WINDOW: usize = 32;

impl TimingRecorder {
    pub fn record(&mut self, key: &str, elapsed: Duration, ok: bool) {
        let t = self.keys.entry(key.to_string()).or_default();
        t.runs += 1;
        if !ok {
            t.errors += 1;
        }
        t.last = Some(elapsed);
        t.max = Some(t.max.map_or(elapsed, |m| m.max(elapsed)));
        if t.window.len() == TIMING_WINDOW {
            t.window.pop_front();
        }
        t.window.push_back(elapsed);
    }

    /// Summary for `key`; `stats` comes from the scheduler.
    pub fn summary(&self, key: &str, stats: ProviderStats) -> ProviderTiming {
        let t = self.keys.get(key);
        let p50 = t.and_then(|t| {
            let mut w: Vec<Duration> = t.window.iter().copied().collect();
            w.sort();
            w.get(w.len().saturating_sub(1) / 2).copied()
        });
        ProviderTiming {
            key: key.to_string(),
            runs: t.map_or(0, |t| t.runs),
            errors: t.map_or(0, |t| t.errors),
            last_ms: t.and_then(|t| t.last).map(duration_ms),
            p50_ms: p50.map(duration_ms),
            max_ms: t.and_then(|t| t.max).map(duration_ms),
            stats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            kind: SNAPSHOT_KIND.into(),
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            written_at_ms: 1,
            worker: WorkerInfo {
                binary_version: "0.1.0".into(),
                protocol_version: 1,
                epoch: (1 << 53) - 1,
                pid: 42,
                parent_pid: Some(41),
                started_at_ms: 0,
            },
            session_id: Some("41.7".into()),
            config_revision: Some("builtin".into()),
            request: Some(PromptRequest {
                gen: 3,
                session_id: "41.7".into(),
                cwd: PathBuf::from("/tmp/x\ty"),
                cols: 80,
                lines: 24,
                last_exit: 1,
                last_duration_ms: 5,
                env: "a\u{1f}b".into(),
                jobs: 0,
            }),
            request_received_at_ms: Some(1),
            last_result: Some(ResultInfo {
                gen: 3,
                status: RenderStatus::Partial,
                sent_at_ms: 1,
                latency_ms: 0.25,
                left_lines: 1,
                bytes: 40,
            }),
            slots: vec![SlotInfo {
                provider: "git".into(),
                freshness: Freshness::Stale,
                age_ms: Some(6000),
                error: None,
                fetching: true,
                scope: "cwd:/tmp/x".into(),
            }],
            providers: vec![],
            counters: Counters { requests: 3, ..Counters::default() },
            degraded: vec!["config_invalid".into()],
            watch: WatchDiagnostics::default(),
            module_reports: vec![],
            timing_history: vec![],
        }
    }

    #[test]
    fn snapshot_round_trips_atomically_and_privately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions/41.7.json");
        let snap = sample();
        write_snapshot(&path, &snap).unwrap();
        assert_eq!(read_snapshot(&path).unwrap(), snap);
        let mut second = snap.clone();
        second.counters.requests = 4;
        write_snapshot(&path, &second).unwrap();
        assert_eq!(read_snapshot(&path).unwrap().counters.requests, 4);
        let names: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("41.7.json")], "no temp file left behind");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["slots"][0]["freshness"], "stale");
        assert_eq!(v["last_result"]["status"], "partial");
        assert_eq!(v["worker"]["epoch"].as_u64(), Some((1 << 53) - 1));
    }

    #[test]
    fn streamed_large_snapshot_is_byte_identical_and_failure_preserves_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.json");
        let mut snapshot = sample();
        snapshot.timing_history = (0..TIMING_HISTORY_LIMIT).map(|i| TimingCycle {
            gen: i as u64, at_ms: i as u64,
            providers: (0..64).map(|n| ProviderCycleTiming {
                provider: format!("provider-{n}"), duration_ms: n,
                executed: true, completed: true, cache_hit: false,
                timed_out: false, coalesced: false, coalesced_count: 0,
            }).collect(),
        }).collect();
        let mut expected = serde_json::to_vec_pretty(&snapshot).unwrap();
        expected.push(b'\n');
        assert!(expected.len() > 500_000);
        write_snapshot(&path, &snapshot).unwrap();
        assert_eq!(fs::read(&path).unwrap(), expected);
        assert_eq!(read_snapshot(&path).unwrap(), snapshot);
        let failure = write_atomic_with(&path, |file| {
            file.write_all(b"incomplete")?;
            Err(io::Error::other("injected writer failure"))
        });
        assert!(failure.is_err());
        assert_eq!(fs::read(&path).unwrap(), expected);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn write_failure_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("is-a-dir");
        fs::create_dir_all(target.join("child")).unwrap();
        assert!(write_atomic(&target, b"{}").is_err());
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn read_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(read_snapshot(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(read_snapshot(&dir.path().join("missing.json")).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn prune_removes_only_old_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.json"), b"{}").unwrap();
        fs::write(dir.path().join("keep.txt"), b"").unwrap();
        assert_eq!(prune_snapshots(dir.path(), Duration::from_secs(3600)), 0);
        assert_eq!(prune_snapshots(dir.path(), Duration::ZERO), 1);
        assert!(dir.path().join("keep.txt").exists());
        assert_eq!(prune_snapshots(&dir.path().join("missing"), Duration::ZERO), 0);
    }

    #[test]
    fn background_writer_keeps_latest_and_removes_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions/w.json");
        let w = SnapshotWriter::start();
        for n in 0..50 {
            let mut snap = sample();
            snap.counters.requests = n;
            w.submit(path.clone(), snap);
        }
        w.flush();
        assert_eq!(read_snapshot(&path).unwrap().counters.requests, 49);
        w.shutdown(true);
        assert!(!path.exists());
        // Shutdown without remove keeps the file.
        let w = SnapshotWriter::start();
        w.submit(path.clone(), sample());
        w.flush();
        w.shutdown(false);
        assert!(path.exists());
    }

    #[test]
    fn timing_recorder_summarizes() {
        let mut t = TimingRecorder::default();
        for ms in [5u64, 1, 9, 3, 7] {
            t.record("git", Duration::from_millis(ms), ms != 9);
        }
        let s = t.summary("git", ProviderStats::default());
        assert_eq!((s.runs, s.errors), (5, 1));
        assert_eq!(s.last_ms, Some(7.0));
        assert_eq!(s.p50_ms, Some(5.0));
        assert_eq!(s.max_ms, Some(9.0));
        for _ in 0..100 {
            t.record("git", Duration::from_micros(1500), true);
        }
        let s = t.summary("git", ProviderStats::default());
        assert_eq!((s.p50_ms, s.max_ms, s.runs), (Some(1.5), Some(9.0), 105));
        let empty = t.summary("host", ProviderStats::default());
        assert_eq!((empty.runs, empty.last_ms), (0, None));
    }
}
