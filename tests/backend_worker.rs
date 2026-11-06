//! Worker core driven in-process over OS pipes with fake providers and a fake
//! renderer (the real config/render/providers are not used here).
//!
//! Latency numbers: `cargo test --test backend_worker -- --nocapture latency`.

use tildy::model::{
    Dependency, Freshness, GitData, GitState, ModuleValue, PromptData, PromptRequest, ProviderSpec, RenderStatus,
    RenderedPrompt, Slot, BINARY_VERSION,
};
use tildy::protocol::encode_request;
use tildy::providers::{Provider, ProviderValue};
use tildy::scheduler::SchedulerConfig;
use tildy::session::worker::{run_worker, ExitReason, SnapshotTarget, WorkerOptions};
use tildy::session::PromptRenderer;
use std::io::{BufRead, BufReader, Write};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const GIT_DEPS: &[Dependency] = &[Dependency::Cwd];
const NO_DEPS: &[Dependency] = &[];

// ---------------------------------------------------------------- fakes

struct FakeGit {
    spec: ProviderSpec,
    delay_ms: Arc<AtomicU64>,
    calls: Arc<AtomicUsize>,
}

impl Provider for FakeGit {
    fn spec(&self) -> &ProviderSpec {
        &self.spec
    }
    fn fetch(&self, req: &PromptRequest) -> ProviderValue {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(self.delay_ms.load(Ordering::SeqCst)));
        let name = req.cwd.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        ProviderValue::Git(Slot::fresh(GitData {
            in_repo: true,
            worktree_id: Some(format!("wt-{name}")),
            branch: Some(format!("{name}#{n}")),
            state: GitState::Clean,
            ..GitData::default()
        }))
    }
}

struct FakeHost {
    spec: ProviderSpec,
    calls: Arc<AtomicUsize>,
}

impl Provider for FakeHost {
    fn spec(&self) -> &ProviderSpec {
        &self.spec
    }
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ProviderValue::Hostname(Some("box".into()))
    }
}

struct Panicky {
    spec: ProviderSpec,
}

impl Provider for Panicky {
    fn spec(&self) -> &ProviderSpec {
        &self.spec
    }
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        panic!("fake provider panic (expected in this test)");
    }
}

struct Fakes {
    git_delay: Arc<AtomicU64>,
    git_calls: Arc<AtomicUsize>,
    host_calls: Arc<AtomicUsize>,
}

impl Fakes {
    fn new(git_delay_ms: u64) -> Self {
        Fakes {
            git_delay: Arc::new(AtomicU64::new(git_delay_ms)),
            git_calls: Arc::new(AtomicUsize::new(0)),
            host_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn providers(&self, git_timeout_ms: u64) -> Vec<Arc<dyn Provider>> {
        vec![
            Arc::new(FakeGit {
                spec: ProviderSpec {
                    key: "git",
                    deps: GIT_DEPS,
                    ttl: Duration::from_secs(5),
                    timeout: Duration::from_millis(git_timeout_ms),
                },
                delay_ms: self.git_delay.clone(),
                calls: self.git_calls.clone(),
            }),
            Arc::new(FakeHost {
                spec: ProviderSpec {
                    key: "hostname",
                    deps: NO_DEPS,
                    ttl: Duration::from_secs(3600),
                    timeout: Duration::from_millis(100),
                },
                calls: self.host_calls.clone(),
            }),
        ]
    }
}

#[derive(Default)]
struct RenderCounts {
    refresh: AtomicUsize,
    render: AtomicUsize,
}

/// `<cwd-name> <git> <host>`; git: `…` pending, branch fresh, `~branch` stale,
/// `?` timeout, `!` error.
struct FakeRenderer {
    counts: Arc<RenderCounts>,
}

impl PromptRenderer for FakeRenderer {
    fn refresh(&mut self) {
        self.counts.refresh.fetch_add(1, Ordering::SeqCst);
    }
    fn render(&mut self, d: &PromptData) -> RenderedPrompt {
        self.counts.render.fetch_add(1, Ordering::SeqCst);
        let name = d.request.cwd.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let branch = d.git.value.as_ref().and_then(|g| g.branch.clone()).unwrap_or_default();
        let git = match d.git.freshness {
            Freshness::Pending => "…".to_string(),
            Freshness::Fresh => branch,
            Freshness::Stale => format!("~{branch}"),
            Freshness::Timeout => "?".into(),
            Freshness::Error => "!".into(),
        };
        RenderedPrompt {
            // The worker overrides the status; render something wrong on purpose.
            status: RenderStatus::Fresh,
            left: vec![format!("{name} {git} {}", d.hostname.as_deref().unwrap_or("-"))],
            right: Some(format!("exit={}", d.request.last_exit)),
            transient: Some("> ".into()),
            history: None,
            observation_seq: None,
        }
    }
    fn revision(&self) -> Option<String> {
        Some("fake-rev".into())
    }
}

// ---------------------------------------------------------------- harness

#[derive(Debug, Clone, PartialEq)]
enum Frame {
    Hello { protocol: String, version: String, epoch: u64, pid: u32 },
    Res { gen: u64, status: String, epoch: u64, left: Vec<String>, right: Option<String>, transient: Option<String>,
        observation_seq: Option<u64>, },
}

impl Frame {
    fn res(&self) -> (u64, &str, &str) {
        match self {
            Frame::Res { gen, status, left, .. } => (*gen, status.as_str(), left[0].as_str()),
            other => panic!("expected RES, got {other:?}"),
        }
    }
}

struct Harness {
    input: Option<std::io::PipeWriter>,
    frames: Receiver<(Instant, Frame)>,
    worker: Option<JoinHandle<ExitReason>>,
    counts: Arc<RenderCounts>,
    epoch: u64,
    watch_state: Option<tempfile::TempDir>,
}

fn opts(snapshot: SnapshotTarget) -> WorkerOptions {
    WorkerOptions {
        snapshot,
        home: Some(PathBuf::from("/home/u")),
        epoch: 4242,
        pid: 777,
        prune_snapshots_after: None,
        agent_store: None,
        watch_enabled: false,
        idle_refresh: None,
        ..WorkerOptions::default()
    }
}

impl Harness {
    fn start(providers: Vec<Arc<dyn Provider>>, opts: WorkerOptions) -> Self {
        let counts = Arc::new(RenderCounts::default());
        let renderer = Box::new(FakeRenderer { counts: counts.clone() });
        Self::start_with(providers, renderer, counts, opts)
    }

    fn start_with(
        providers: Vec<Arc<dyn Provider>>,
        renderer: Box<dyn PromptRenderer>,
        counts: Arc<RenderCounts>,
        opts: WorkerOptions,
    ) -> Self {
        let (in_r, in_w) = std::io::pipe().unwrap();
        let (out_r, out_w) = std::io::pipe().unwrap();
        let epoch = opts.epoch;
        let worker = thread::spawn(move || run_worker(in_r, out_w, move || providers, renderer, opts));
        let (tx, frames) = mpsc::channel();
        thread::spawn(move || read_frames(BufReader::new(out_r), tx));
        Harness { input: Some(in_w), frames, worker: Some(worker), counts, epoch, watch_state: None }
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        let w = self.input.as_mut().unwrap();
        w.write_all(bytes).unwrap();
        w.flush().unwrap();
    }

    fn send(&mut self, req: &PromptRequest) -> Instant {
        let mut line = encode_request(req);
        line.push(b'\n');
        let t = Instant::now();
        self.send_raw(&line);
        t
    }

    fn next(&self, timeout_ms: u64) -> Option<(Instant, Frame)> {
        self.frames.recv_timeout(Duration::from_millis(timeout_ms)).ok()
    }

    fn expect(&self, timeout_ms: u64) -> Frame {
        self.next(timeout_ms).map(|(_, f)| f).expect("frame expected")
    }

    /// Collect frames until `quiet_ms` passes with none.
    fn drain(&self, quiet_ms: u64) -> Vec<Frame> {
        let mut out = Vec::new();
        while let Some((_, f)) = self.next(quiet_ms) {
            out.push(f);
        }
        out
    }

    fn hello(&self) {
        match self.expect(1000) {
            Frame::Hello { protocol, version, epoch, pid } => {
                assert_eq!(protocol, "1");
                assert_eq!(version, BINARY_VERSION);
                assert_eq!(epoch, self.epoch);
                assert_eq!(pid, 777);
            }
            other => panic!("first frame must be HELLO, got {other:?}"),
        }
    }

    fn close_input(&mut self) {
        self.input = None;
    }

    fn join(&mut self, timeout_ms: u64) -> ExitReason {
        self.join_inner(timeout_ms).expect("worker already joined")
    }

    fn join_inner(&mut self, timeout_ms: u64) -> Option<ExitReason> {
        let handle = self.worker.take()?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while !handle.is_finished() {
            assert!(Instant::now() < deadline, "worker did not exit within {timeout_ms} ms");
            thread::sleep(Duration::from_millis(2));
        }
        Some(handle.join().unwrap())
    }
}

impl Drop for Harness {
    /// Stop the worker (EOF) before temp dirs used for snapshots are removed.
    fn drop(&mut self) {
        self.close_input();
        if !thread::panicking() {
            let _ = self.join_inner(2000);
        }
    }
}

/// Strict frame reader: HELLO lines, and RES..END blocks with L/R/T lines.
fn read_frames(mut r: BufReader<std::io::PipeReader>, tx: mpsc::Sender<(Instant, Frame)>) {
    let mut open: Option<Frame> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            assert!(open.is_none(), "half frame at EOF");
            return;
        }
        let l = line.strip_suffix('\n').expect("frames end with newline");
        let f: Vec<&str> = l.splitn(2, '\t').collect();
        let (tag, rest) = (f[0], f.get(1).copied().unwrap_or(""));
        match (tag, open.as_mut()) {
            ("HELLO", None) => {
                let p: Vec<&str> = rest.split('\t').collect();
                assert_eq!(p.len(), 4, "{l:?}");
                let hello = Frame::Hello {
                    protocol: p[0].into(),
                    version: p[1].into(),
                    epoch: p[2].parse().unwrap(),
                    pid: p[3].parse().unwrap(),
                };
                if tx.send((Instant::now(), hello)).is_err() {
                    return;
                }
            }
            ("RES", None) => {
                let p: Vec<&str> = rest.split('\t').collect();
                assert_eq!(p.len(), 3, "{l:?}");
                assert!(p[1] == "partial" || p[1] == "fresh", "{l:?}");
                open = Some(Frame::Res {
                    gen: p[0].parse().unwrap(),
                    status: p[1].into(),
                    epoch: p[2].parse().unwrap(),
                    left: vec![],
                    right: None,
                    transient: None,
                    observation_seq: None,
                });
            }
            (
                "O",
                Some(Frame::Res {
                    observation_seq, ..
                }),
            ) => *observation_seq = Some(rest.parse().unwrap()),
            ("L", Some(Frame::Res { left, .. })) => left.push(rest.into()),
            ("R", Some(Frame::Res { right, .. })) if right.is_none() => *right = Some(rest.into()),
            ("T", Some(Frame::Res { transient, .. })) if transient.is_none() => *transient = Some(rest.into()),
            ("K" | "U" | "S" | "H", Some(Frame::Res { .. })) => {},
            ("END", Some(Frame::Res { gen, left, .. })) => {
                assert_eq!(rest.parse::<u64>().unwrap(), *gen, "END gen matches RES gen");
                assert!(!left.is_empty(), "at least one L line");
                if tx.send((Instant::now(), open.take().unwrap())).is_err() {
                    return;
                }
            }
            _ => panic!("unexpected line {l:?} (open frame: {open:?})"),
        }
    }
}

fn req(gen: u64, cwd: &str) -> PromptRequest {
    PromptRequest {
        gen,
        session_id: "4242.1".into(),
        cwd: PathBuf::from(cwd),
        cols: 80,
        lines: 24,
        last_exit: 0,
        last_duration_ms: 3,
        env: "\x1f\x1fu\x1fh\x1f".into(),
        jobs: 0,
    }
}

/// Snapshots are written by a background thread after the frame is flushed:
/// poll until one satisfies `pred`.
fn wait_snapshot(path: &Path, pred: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(v) = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
            if pred(&v) {
                return v;
            }
        }
        assert!(Instant::now() < deadline, "no matching snapshot at {}: {}", path.display(), readiness_summary(path));
        thread::sleep(Duration::from_millis(2));
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

fn us(d: Duration) -> String {
    format!("{:.0} µs", d.as_secs_f64() * 1e6)
}

// ---------------------------------------------------------------- tests

#[test]
fn hello_first_then_partial_then_fresh() {
    let fakes = Fakes::new(30);
    let mut h = Harness::start(fakes.providers(1500), opts(SnapshotTarget::Disabled));
    h.hello();
    h.send(&req(1, "/src/proj"));
    let partial = h.expect(1000);
    match &partial {
        Frame::Res { gen, status, epoch, left, right, transient,
            .. } => {
            assert_eq!((*gen, status.as_str(), *epoch), (1, "partial", 4242));
            assert_eq!(left, &vec!["proj … box".to_string()], "git pending, host ready via cold-start wait");
            assert_eq!(right.as_deref(), Some("exit=0"));
            assert_eq!(transient.as_deref(), Some("> "));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(h.expect(1000).res(), (1, "fresh", "proj proj#0 box"));
    // Second prompt in the same repo: cached git in the partial, then revalidated.
    h.send(&req(2, "/src/proj"));
    assert_eq!(h.expect(1000).res(), (2, "partial", "proj proj#0 box"));
    assert_eq!(h.expect(1000).res(), (2, "fresh", "proj proj#1 box"));
    assert_eq!(fakes.host_calls.load(Ordering::SeqCst), 1, "pure provider served from cache");
    assert_eq!(h.drain(100), vec![]);
    h.close_input();
    assert_eq!(h.join(1000), ExitReason::Eof);
    assert_eq!(h.counts.refresh.load(Ordering::SeqCst), 2, "config checked once per request");
}

#[test]
fn pure_providers_only_send_a_single_fresh() {
    // Only the session-global host provider: the cold-start wait gets it, so
    // nothing is pending and no partial is needed; later prompts hit the cache.
    let fakes = Fakes::new(0);
    let host_only = vec![fakes.providers(1500).remove(1)];
    let mut h = Harness::start(host_only, opts(SnapshotTarget::Disabled));
    h.hello();
    for g in 1..=3u64 {
        h.send(&req(g, "/a"));
        let frames = h.drain(80);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frames[0].res(), (g, "fresh", "a … box"));
    }
    assert_eq!(fakes.host_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn result_for_an_old_generation_is_never_sent() {
    let fakes = Fakes::new(60);
    let mut h = Harness::start(fakes.providers(1500), opts(SnapshotTarget::Disabled));
    h.hello();
    h.send(&req(1, "/a"));
    assert_eq!(h.expect(1000).res(), (1, "partial", "a … box"));
    h.send(&req(2, "/b"));
    let frames = h.drain(250);
    assert!(!frames.is_empty());
    for f in &frames {
        assert_eq!(f.res().0, 2, "nothing for gen 1 after gen 2 arrived: {frames:?}");
    }
    assert_eq!(frames.last().unwrap().res(), (2, "fresh", "b b#1 box"));
    assert_eq!(fakes.git_calls.load(Ordering::SeqCst), 2, "/a fetch finished but only cached");
}

#[test]
fn burst_of_requests_serves_only_the_latest() {
    let fakes = Fakes::new(20);
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("s.json");
    let mut h = Harness::start(fakes.providers(1500), opts(SnapshotTarget::Path(snap.clone())));
    h.hello();
    let mut burst = Vec::new();
    for g in 1..=20u64 {
        burst.extend(encode_request(&req(g, &format!("/d{g}"))));
        burst.push(b'\n');
    }
    h.send_raw(&burst);
    let frames = h.drain(200);
    let last = frames.last().unwrap().res();
    assert_eq!((last.0, last.1), (20, "fresh"));
    assert!(last.2.starts_with("d20 d20#"), "{last:?}");
    let gens: Vec<u64> = frames.iter().map(|f| f.res().0).collect();
    assert!(gens.iter().all(|&g| g == 1 || g == 20), "only first and last gens are served: {gens:?}");
    assert!(fakes.git_calls.load(Ordering::SeqCst) <= 2, "bounded git work");
    let s = wait_snapshot(&snap, |s| s["last_result"]["gen"] == 20 && s["last_result"]["status"] == "fresh");
    assert_eq!(s["counters"]["requests"], 20);
    assert!(s["counters"]["dropped_requests"].as_u64().unwrap() >= 18, "{}", s["counters"]);
}

#[test]
fn same_key_requests_coalesce_into_one_rerun() {
    let fakes = Fakes::new(120);
    let mut h = Harness::start(fakes.providers(1500), opts(SnapshotTarget::Disabled));
    h.hello();
    for g in 1..=5u64 {
        h.send(&req(g, "/r"));
        assert_eq!(h.expect(1000).res().1, "partial", "every REQ gets an immediate partial");
        thread::sleep(Duration::from_millis(5));
    }
    let frames = h.drain(400);
    assert_eq!(frames.last().unwrap().res(), (5, "fresh", "r r#1 box"));
    assert!(frames.iter().all(|f| f.res().0 == 5));
    assert_eq!(fakes.git_calls.load(Ordering::SeqCst), 2, "initial fetch + one rerun for 5 requests");
}

#[test]
fn outer_deadline_marks_timeout_then_late_result_updates() {
    let fakes = Fakes::new(150);
    let mut o = opts(SnapshotTarget::Disabled);
    o.scheduler = SchedulerConfig { outer_grace: Duration::from_millis(20), ..SchedulerConfig::default() };
    let mut h = Harness::start(fakes.providers(50), o);
    h.hello();
    let t0 = h.send(&req(1, "/slow"));
    assert_eq!(h.expect(1000).res(), (1, "partial", "slow … box"));
    let (t_timeout, f) = h.next(1000).unwrap();
    assert_eq!(f.res(), (1, "fresh", "slow ? box"));
    let waited = t_timeout - t0;
    assert!(waited >= Duration::from_millis(65) && waited < Duration::from_millis(140), "{waited:?}");
    let (t_late, f) = h.next(1000).unwrap();
    assert_eq!(f.res(), (1, "fresh", "slow slow#0 box"), "late result replaces the timeout");
    assert!(t_late - t0 >= Duration::from_millis(145));
}

#[test]
fn panicking_provider_becomes_error_not_a_hang() {
    let mut providers = Fakes::new(0).providers(1500);
    providers[0] = Arc::new(Panicky {
        spec: ProviderSpec { key: "git", deps: GIT_DEPS, ttl: Duration::from_secs(5), timeout: Duration::from_secs(1) },
    });
    let mut h = Harness::start(providers, opts(SnapshotTarget::Disabled));
    h.hello();
    h.send(&req(1, "/p"));
    let frames = h.drain(200);
    assert_eq!(frames.last().unwrap().res(), (1, "fresh", "p ! box"));
    h.send(&req(2, "/p"));
    assert_eq!(h.drain(200).last().unwrap().res(), (2, "fresh", "p ! box"), "worker keeps serving");
}

#[test]
fn protocol_errors_are_counted_and_skipped() {
    let fakes = Fakes::new(1);
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("sessions/x.json");
    let mut h = Harness::start(fakes.providers(1500), opts(SnapshotTarget::Path(snap.clone())));
    h.hello();
    h.send_raw(b"garbage\nREQ\t1\ts\t/x\t80\t24\t0\t0\n\nREQ\t1\ts\t/x\\q\t80\t24\t0\t0\t\n");
    let mut long = vec![b'R'; 70 * 1024];
    long.push(b'\n');
    h.send_raw(&long);
    h.send(&req(9, "/ok"));
    let frames = h.drain(150);
    assert!(frames.iter().all(|f| f.res().0 == 9), "{frames:?}");
    assert_eq!(frames.last().unwrap().res().1, "fresh");
    let s = wait_snapshot(&snap, |s| s["last_result"]["status"] == "fresh");
    assert_eq!(s["counters"]["protocol_errors"], 4, "garbage, 8 fields, bad escape, too long");
}

#[test]
fn snapshot_written_after_each_send_and_removed_on_bye() {
    // Snapshot writes are asynchronous and coalesced. Hold the provider until
    // the partial snapshot is observed, instead of racing a 20 ms sleep.
    struct HeldGit {
        inner: Arc<dyn Provider>,
        release: Mutex<Receiver<()>>,
    }
    impl Provider for HeldGit {
        fn spec(&self) -> &ProviderSpec { self.inner.spec() }
        fn fetch(&self, req: &PromptRequest) -> ProviderValue {
            let _ = self.release.lock().unwrap().recv_timeout(Duration::from_secs(5));
            self.inner.fetch(req)
        }
    }
    let fakes = Fakes::new(20);
    let (release, receive) = mpsc::channel();
    let mut providers = fakes.providers(5000);
    providers[0] = Arc::new(HeldGit { inner: providers[0].clone(), release: Mutex::new(receive) });
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("sessions/4242.1.json");
    let mut h = Harness::start(providers, opts(SnapshotTarget::Path(snap.clone())));
    h.hello();
    assert!(!snap.exists(), "no snapshot before the first RES");
    h.send(&req(3, "/repo"));
    assert_eq!(h.expect(1000).res().1, "partial");
    let s = wait_snapshot(&snap, |_| true);
    assert_eq!(s["kind"], "tildy.session_snapshot");
    assert_eq!(s["worker"]["epoch"], 4242);
    assert_eq!(s["worker"]["pid"], 777);
    assert_eq!(s["worker"]["binary_version"], BINARY_VERSION);
    assert_eq!(s["request"]["gen"], 3);
    assert_eq!(s["last_result"]["status"], "partial");
    assert_eq!(s["slots"][0]["provider"], "git");
    assert_eq!(s["slots"][0]["freshness"], "pending");
    assert_eq!(s["slots"][0]["fetching"], true);
    assert_eq!(s["config_revision"], "fake-rev");
    release.send(()).unwrap();
    assert_eq!(h.expect(1000).res().1, "fresh");
    let s = wait_snapshot(&snap, |s| s["last_result"]["status"] == "fresh");
    assert_eq!(s["slots"][0]["freshness"], "fresh");
    assert_eq!(s["slots"][0]["scope"], "worktree:wt-repo");
    assert_eq!(s["providers"][0]["runs"], 1);
    assert!(s["providers"][0]["last_ms"].as_f64().unwrap() >= 19.0);
    assert_eq!(s["counters"]["partial_sent"], 1);
    assert_eq!(s["counters"]["fresh_sent"], 1);
    assert!(s["last_result"]["latency_ms"].as_f64().unwrap() >= 19.0);
    h.send_raw(b"BYE\n");
    assert_eq!(h.join(1000), ExitReason::Bye);
    assert!(!snap.exists(), "snapshot removed on clean exit");
}

struct Fixed(ProviderSpec, ProviderValue);

impl Provider for Fixed {
    fn spec(&self) -> &ProviderSpec {
        &self.0
    }
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        self.1.clone()
    }
}

#[test]
fn provider_keys_label_slots_and_duplicates_are_dropped() {
    let spec = |key| ProviderSpec { key, deps: NO_DEPS, ttl: Duration::from_secs(60), timeout: Duration::from_millis(50) };
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(Fixed(spec("session/ssh"), ProviderValue::Ssh(true))),
        Arc::new(Fixed(spec("session/hostname"), ProviderValue::Hostname(Some("box".into())))),
        Arc::new(Fixed(spec("session/username"), ProviderValue::Username(Some("me".into())))),
        // Same key as an earlier provider: dropped (the cache is keyed by it).
        Arc::new(Fixed(spec("session/hostname"), ProviderValue::Hostname(Some("other".into())))),
    ];
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("s.json");
    let mut h = Harness::start(providers, opts(SnapshotTarget::Path(snap.clone())));
    h.hello();
    h.send(&req(1, "/a"));
    assert_eq!(h.expect(1000).res(), (1, "fresh", "a … box"), "all values in one fresh frame");
    let s = wait_snapshot(&snap, |s| s["last_result"]["status"] == "fresh");
    let labels: Vec<&str> = s["slots"].as_array().unwrap().iter().map(|v| v["provider"].as_str().unwrap()).collect();
    assert_eq!(labels, vec!["session/ssh", "session/hostname", "session/username"]);
    let timing: Vec<&str> = s["providers"].as_array().unwrap().iter().map(|v| v["key"].as_str().unwrap()).collect();
    assert_eq!(timing, labels);
    assert!(s["providers"].as_array().unwrap().iter().all(|v| v["runs"] == 1));
}

#[test]
fn exits_when_parent_disappears() {
    let mut parent = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let mut o = opts(SnapshotTarget::Disabled);
    o.parent_pid = Some(parent.id());
    o.parent_poll = Duration::from_millis(40);
    let mut h = Harness::start(Fakes::new(0).providers(1500), o);
    h.hello();
    thread::sleep(Duration::from_millis(120));
    assert!(!h.worker.as_ref().unwrap().is_finished(), "alive while parent lives");
    parent.kill().unwrap();
    parent.wait().unwrap();
    let t = Instant::now();
    assert_eq!(h.join(1000), ExitReason::ParentGone);
    assert!(t.elapsed() < Duration::from_millis(200), "{:?}", t.elapsed());
}

#[test]
fn exits_quietly_when_output_closes() {
    let (in_r, _in_w) = std::io::pipe().unwrap();
    let (out_r, out_w) = std::io::pipe().unwrap();
    drop(out_r);
    let renderer = Box::new(FakeRenderer { counts: Arc::default() });
    let providers = Fakes::new(0).providers(1500);
    let reason = run_worker(in_r, out_w, move || providers, renderer, opts(SnapshotTarget::Disabled));
    assert_eq!(reason, ExitReason::OutputClosed);
}

#[test]
fn bye_before_any_request() {
    let mut h = Harness::start(Fakes::new(0).providers(1500), opts(SnapshotTarget::Disabled));
    h.hello();
    h.send_raw(b"BYE\n");
    assert_eq!(h.join(1000), ExitReason::Bye);
}

#[test]
fn latency_req_to_partial() {
    let fakes = Fakes::new(3);
    let mut h = Harness::start(fakes.providers(1500), opts(SnapshotTarget::Disabled));
    h.hello();
    // Cold first request (includes the cold-start wait for global providers).
    let t = h.send(&req(1, "/lat"));
    let (t_cold, f) = h.next(1000).unwrap();
    assert_eq!(f.res().1, "partial");
    let cold = t_cold - t;
    assert_eq!(h.expect(1000).res().1, "fresh");

    let n = 300u64;
    let mut partial = Vec::new();
    let mut fresh = Vec::new();
    for g in 2..2 + n {
        let t = h.send(&req(g, "/lat"));
        let (tp, f) = h.next(1000).unwrap();
        assert_eq!((f.res().0, f.res().1), (g, "partial"));
        let (tf, f) = h.next(1000).unwrap();
        assert_eq!((f.res().0, f.res().1), (g, "fresh"));
        partial.push(tp - t);
        fresh.push(tf - t);
    }
    partial.sort();
    fresh.sort();
    let (p50, p95, max) = (percentile(&partial, 0.5), percentile(&partial, 0.95), *partial.last().unwrap());
    println!(
        "worker REQ->partial (warm, n={n}): p50 {} p95 {} max {}; cold first request {}; REQ->fresh with 3 ms fake git: p50 {} p95 {}",
        us(p50),
        us(p95),
        us(max),
        us(cold),
        us(percentile(&fresh, 0.5)),
        us(percentile(&fresh, 0.95)),
    );
    // Numbers above are the measurement; the assertion only guards against a
    // regression that no load explains (the suite runs 35 tests in parallel).
    assert!(p50 < Duration::from_millis(2), "REQ->partial p50 {} exceeds 2 ms", us(p50));
    assert!(p95 < Duration::from_millis(20), "REQ->partial p95 {} exceeds 20 ms", us(p95));
}

#[test]
fn latency_with_snapshot_writes() {
    let fakes = Fakes::new(3);
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::start(fakes.providers(1500), opts(SnapshotTarget::Path(dir.path().join("s.json"))));
    h.hello();
    h.send(&req(1, "/lat"));
    h.drain(50);
    let mut partial = Vec::new();
    for g in 2..202u64 {
        let t = h.send(&req(g, "/lat"));
        let (tp, _) = h.next(1000).unwrap();
        let _ = h.next(1000).unwrap();
        partial.push(tp - t);
    }
    partial.sort();
    println!(
        "worker REQ->partial with snapshot file writes (n=200): p50 {} p95 {} max {}",
        us(percentile(&partial, 0.5)),
        us(percentile(&partial, 0.95)),
        us(*partial.last().unwrap())
    );
    assert!(percentile(&partial, 0.5) < Duration::from_millis(2));
    assert!(percentile(&partial, 0.95) < Duration::from_millis(20));
}

// ------------------------------------------------- config-driven providers

/// A config module provider: records whether its fetch started before the
/// renderer had rendered the request it serves (it must not: module fetches
/// start after the partial).
struct FakeModule {
    spec: ProviderSpec,
    text: String,
    delay_ms: u64,
    rendered_gen: Arc<AtomicU64>,
    early_starts: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}

impl Provider for FakeModule {
    fn spec(&self) -> &ProviderSpec {
        &self.spec
    }
    fn fetch(&self, req: &PromptRequest) -> ProviderValue {
        if self.rendered_gen.load(Ordering::SeqCst) < req.gen {
            self.early_starts.fetch_add(1, Ordering::SeqCst);
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(self.delay_ms));
        let id = self.spec.key.to_string();
        ProviderValue::Module(id, Slot::fresh(ModuleValue { text: self.text.clone(), ..ModuleValue::default() }))
    }
}

/// Renderer standing in for `ConfigRenderer`: `epoch` selects which provider
/// set `providers()` hands over (set N for epoch N). Renders
/// `<git> <host> [id=text|…|?]...`.
type ProviderSets = Arc<Mutex<Vec<Vec<Arc<dyn Provider>>>>>;

struct SwitchRenderer {
    epoch: Arc<AtomicU64>,
    sets: ProviderSets,
    rendered_gen: Arc<AtomicU64>,
    provider_calls: Arc<AtomicUsize>,
}

impl PromptRenderer for SwitchRenderer {
    fn config_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
    fn providers(&mut self) -> Option<Vec<Arc<dyn Provider>>> {
        self.provider_calls.fetch_add(1, Ordering::SeqCst);
        let e = self.epoch.load(Ordering::SeqCst) as usize;
        self.sets.lock().unwrap().get(e - 1).cloned()
    }
    fn render(&mut self, d: &PromptData) -> RenderedPrompt {
        let git = match d.git.freshness {
            Freshness::Pending => "…".to_string(),
            _ => d.git.value.as_ref().and_then(|g| g.branch.clone()).unwrap_or_default(),
        };
        let mods: Vec<String> = d
            .modules
            .iter()
            .map(|(id, m)| {
                let v = match (m.freshness, &m.value) {
                    (Freshness::Pending, _) => "…".to_string(),
                    (_, Some(v)) => v.text.clone(),
                    (f, None) => format!("{f:?}"),
                };
                format!("{id}={v}")
            })
            .collect();
        self.rendered_gen.store(d.request.gen, Ordering::SeqCst);
        RenderedPrompt {
            status: RenderStatus::Fresh,
            left: vec![format!("{git} {} [{}]", d.hostname.as_deref().unwrap_or("-"), mods.join(" "))],
            right: None,
            transient: None,
            history: None,
            observation_seq: None,
        }
    }
}

struct ModuleRig {
    rendered_gen: Arc<AtomicU64>,
    early_starts: Arc<AtomicUsize>,
}

impl ModuleRig {
    fn module(&self, id: &str, text: &str, delay_ms: u64, calls: &Arc<AtomicUsize>) -> Arc<dyn Provider> {
        Arc::new(FakeModule {
            spec: ProviderSpec {
                key: tildy::providers::intern(id),
                deps: GIT_DEPS,
                ttl: Duration::from_secs(600),
                timeout: Duration::from_millis(500),
            },
            text: text.into(),
            delay_ms,
            rendered_gen: self.rendered_gen.clone(),
            early_starts: self.early_starts.clone(),
            calls: calls.clone(),
        })
    }
}

#[test]
fn config_providers_fill_modules_after_the_partial_and_rebuild_on_reload() {
    let fakes = Fakes::new(5);
    let base = fakes.providers(1500);
    let rig = ModuleRig { rendered_gen: Arc::new(AtomicU64::new(0)), early_starts: Arc::new(AtomicUsize::new(0)) };
    let node_calls = Arc::new(AtomicUsize::new(0));
    let rust_calls = Arc::new(AtomicUsize::new(0));
    let mut set1 = base.clone();
    set1.push(rig.module("node", "22.3.0", 20, &node_calls));
    let mut set2 = base.clone();
    set2.push(rig.module("rust", "1.95.0", 0, &rust_calls));
    let epoch = Arc::new(AtomicU64::new(1));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let renderer = Box::new(SwitchRenderer {
        epoch: epoch.clone(),
        sets: Arc::new(Mutex::new(vec![set1, set2])),
        rendered_gen: rig.rendered_gen.clone(),
        provider_calls: provider_calls.clone(),
    });
    let mut h = Harness::start_with(base, renderer, Arc::default(), opts(SnapshotTarget::Disabled));
    h.hello();

    // Cold: the module is pending in the partial, filled in the fresh frame.
    h.send(&req(1, "/src/proj"));
    assert_eq!(h.expect(1000).res(), (1, "partial", "… box [node=…]"));
    let frames = h.drain(150);
    assert_eq!(frames.last().unwrap().res(), (1, "fresh", "proj#0 box [node=22.3.0]"));
    // Warm: cached version shown in the partial while it revalidates.
    h.send(&req(2, "/src/proj"));
    assert_eq!(h.expect(1000).res(), (2, "partial", "proj#0 box [node=22.3.0]"));
    assert_eq!(h.drain(150).last().unwrap().res(), (2, "fresh", "proj#1 box [node=22.3.0]"));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1, "providers rebuilt only when the epoch changes");

    // Config reload: node removed, rust added; git and host keep their cache.
    epoch.store(2, Ordering::SeqCst);
    h.send(&req(3, "/src/proj"));
    assert_eq!(h.expect(1000).res(), (3, "partial", "proj#1 box [rust=…]"));
    assert_eq!(h.drain(150).last().unwrap().res(), (3, "fresh", "proj#2 box [rust=1.95.0]"));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fakes.host_calls.load(Ordering::SeqCst), 1, "host cache survived the reload");
    assert_eq!((node_calls.load(Ordering::SeqCst), rust_calls.load(Ordering::SeqCst)), (2, 1));
    assert_eq!(rig.early_starts.load(Ordering::SeqCst), 0, "no module fetch started before its request was rendered");
}

#[test]
fn module_fetch_never_delays_the_cold_partial() {
    // A slow module (and slow git) on the very first request: the partial goes
    // out after the cold-start wait for host only.
    let fakes = Fakes::new(200);
    let base = fakes.providers(1500);
    let rig = ModuleRig { rendered_gen: Arc::new(AtomicU64::new(0)), early_starts: Arc::new(AtomicUsize::new(0)) };
    let calls = Arc::new(AtomicUsize::new(0));
    let mut set = base.clone();
    set.push(rig.module("java", "21.0.2", 300, &calls));
    let renderer = Box::new(SwitchRenderer {
        epoch: Arc::new(AtomicU64::new(1)),
        sets: Arc::new(Mutex::new(vec![set])),
        rendered_gen: rig.rendered_gen.clone(),
        provider_calls: Arc::default(),
    });
    let mut h = Harness::start_with(base, renderer, Arc::default(), opts(SnapshotTarget::Disabled));
    h.hello();
    let t = h.send(&req(1, "/x"));
    let (tp, f) = h.next(1000).unwrap();
    assert_eq!(f.res(), (1, "partial", "… box [java=…]"));
    assert!(tp - t < Duration::from_millis(50), "{:?}", tp - t);
    assert_eq!(h.drain(500).last().unwrap().res(), (1, "fresh", "x#0 box [java=21.0.2]"));
    assert_eq!(rig.early_starts.load(Ordering::SeqCst), 0);
}

/// Real providers in-process: session, git and every builtin language module
/// plus docker_context, in a temp git repo with package.json and Cargo.toml.
/// The partial must stay fast although module fetches (version commands on
/// the first request, cache hits afterwards) follow every request.
#[test]
fn latency_with_real_module_providers() {
    use tildy::providers::modules::{self, ModuleProvider};
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().canonicalize().unwrap();
    let ok = Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap().success();
    assert!(ok);
    for f in ["package.json", "Cargo.toml"] {
        std::fs::write(repo.join(f), "{}").unwrap();
    }
    let mut providers: Vec<Arc<dyn Provider>> = tildy::providers::all().into_iter().map(Arc::from).collect();
    let (ttl, timeout) = (Duration::from_secs(600), Duration::from_millis(2000));
    for lang in ["nodejs", "rust", "golang", "java", "c", "python"] {
        providers.push(Arc::new(ModuleProvider::language(lang, modules::language(lang).unwrap(), ttl, timeout)));
    }
    providers.push(Arc::new(ModuleProvider::docker_context("docker", ttl, timeout)));
    let counts = Arc::new(RenderCounts::default());
    let renderer = Box::new(ModulesShown { counts: counts.clone() });
    let mut h = Harness::start_with(providers, renderer, counts, opts(SnapshotTarget::Disabled));
    h.hello();
    let cwd = repo.to_string_lossy().into_owned();
    let t = h.send(&req(1, &cwd));
    let (tp, f) = h.next(2000).unwrap();
    assert_eq!(f.res().1, "partial");
    let cold_partial = tp - t;
    let (cold_fresh, cold_text) = loop {
        let (tf, f) = h.next(3000).expect("fresh frame");
        if f.res().1 == "fresh" {
            break (tf - t, f.res().2.to_string());
        }
    };
    let n = 150u64;
    let mut partial = Vec::new();
    let mut fresh = Vec::new();
    for g in 2..2 + n {
        let t = h.send(&req(g, &cwd));
        let (tp, f) = h.next(2000).unwrap();
        assert_eq!((f.res().0, f.res().1), (g, "partial"));
        partial.push(tp - t);
        loop {
            let (tf, f) = h.next(2000).unwrap();
            if f.res().1 == "fresh" {
                fresh.push(tf - t);
                break;
            }
        }
    }
    partial.sort();
    fresh.sort();
    println!(
        "real providers (git + 6 languages + docker): cold partial {}, cold settle {:.1} ms ({}); warm REQ->partial n={n} p50 {} p95 {} max {}; REQ->fresh p50 {} p95 {}",
        us(cold_partial),
        cold_fresh.as_secs_f64() * 1e3,
        cold_text,
        us(percentile(&partial, 0.5)),
        us(percentile(&partial, 0.95)),
        us(*partial.last().unwrap()),
        us(percentile(&fresh, 0.5)),
        us(percentile(&fresh, 0.95)),
    );
    assert!(percentile(&partial, 0.95) < Duration::from_millis(2), "REQ->partial p95 {}", us(percentile(&partial, 0.95)));
}

/// `<module>=<text|freshness>` for every module slot.
struct ModulesShown {
    counts: Arc<RenderCounts>,
}

impl PromptRenderer for ModulesShown {
    fn render(&mut self, d: &PromptData) -> RenderedPrompt {
        self.counts.render.fetch_add(1, Ordering::SeqCst);
        let mods: Vec<String> = d
            .modules
            .iter()
            .map(|(id, m)| match &m.value {
                Some(v) => format!("{id}={}", v.text),
                None => format!("{id}:{:?}", m.freshness),
            })
            .collect();
        RenderedPrompt { status: RenderStatus::Fresh, left: vec![mods.join(" ")], right: None, transient: None,
            history: None,
            observation_seq: None, }
    }
}

struct CountRealGit(ProviderSpec, Arc<AtomicUsize>);
impl Provider for CountRealGit {
    fn spec(&self) -> &ProviderSpec {
        &self.0
    }
    fn fetch(&self, req: &PromptRequest) -> ProviderValue {
        self.1.fetch_add(1, Ordering::SeqCst);
        ProviderValue::Git(tildy::providers::git::git_status(
            &req.cwd,
            Duration::from_secs(1),
        ))
    }
}
struct ChangesShown;
impl PromptRenderer for ChangesShown {
    fn render(&mut self, d: &PromptData) -> RenderedPrompt {
        let branch = d
            .git
            .value
            .as_ref()
            .and_then(|g| g.branch.as_deref())
            .unwrap_or("pending");
        let changes = d
            .changes
            .as_ref()
            .map(|c| {
                format!(
                    "{}→{} files={}",
                    c.branch_from.as_deref().unwrap_or(""),
                    c.branch_to.as_deref().unwrap_or(""),
                    c.files_added
                )
            })
            .unwrap_or_else(|| "none".into());
        RenderedPrompt {
            status: RenderStatus::Fresh,
            left: vec![format!("{branch} changes={changes}")],
            right: None,
            transient: None,
            history: None,
            observation_seq: None,
        }
    }
}
fn real_git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn real_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    real_git(dir.path(), &["init", "-b", "main"]);
    real_git(
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    dir
}
fn expect_fresh(h: &Harness, gen: u64, contains: &str, timeout_ms: u64) -> Frame {
    let end = Instant::now() + Duration::from_millis(timeout_ms);
    let mut frames = Vec::new();
    while let Some((_, frame)) =
        h.next(end.saturating_duration_since(Instant::now()).as_millis() as u64)
    {
        if matches!(&frame, Frame::Res { gen: g, status, left, .. } if *g == gen && status == "fresh" && left[0].contains(contains))
        {
            return frame;
        }
        frames.push(frame);
    }
    panic!("expected {contains:?}, got {frames:?}")
}
fn ack(h: &mut Harness, frame: &Frame, epoch: u64) {
    if let Frame::Res {
        gen,
        observation_seq: Some(seq),
        ..
    } = frame
    {
        h.send_raw(format!("ACK\t{gen}\t{epoch}\t{seq}\n").as_bytes());
    } else {
        panic!("missing observation sequence: {frame:?}");
    }
}
// FSEvents registration/teardown is shared through the host daemon. Keep
// unrelated native fixture lifetimes separate; the dedicated concurrency test
// takes this lock once and still starts all four native streams in parallel.
static NATIVE_FIXTURES: Mutex<()> = Mutex::new(());
fn native_fixture() -> std::sync::MutexGuard<'static, ()> {
    NATIVE_FIXTURES.lock().unwrap_or_else(|error| error.into_inner())
}

static READINESS_WAITERS: AtomicUsize = AtomicUsize::new(0);
struct ReadinessWait;
impl Drop for ReadinessWait {
    fn drop(&mut self) { READINESS_WAITERS.fetch_sub(1, Ordering::SeqCst); }
}
fn readiness_summary(path: &Path) -> serde_json::Value {
    let value = std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok()).unwrap_or_default();
    serde_json::json!({
        "watch": value["watch"], "cycles": value["timing_history"].as_array().map(Vec::len),
        "slots": value["slots"], "last_result": value["last_result"],
        "parallel_readiness_waiters": READINESS_WAITERS.load(Ordering::SeqCst),
    })
}
fn wait_watch_ready(path: &Path) {
    wait_watch_ready_for_cycles(path, 2);
}
fn wait_watch_ready_for_cycles(path: &Path, min_cycles: usize) {
    let concurrent = READINESS_WAITERS.fetch_add(1, Ordering::SeqCst) + 1;
    let _waiter = ReadinessWait;
    let started = Instant::now();
    // Native stream initialization has its own readiness barrier. The worker
    // must already have sent its fresh frame; the 1.5s external-update contract
    // begins only after initialization and its gap-covering requery finish.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(snapshot) = std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
            if snapshot["watch"]["mode"] != "off"
                && snapshot["timing_history"].as_array().is_some_and(|h| h.len() >= min_cycles)
                && snapshot["slots"].as_array().is_some_and(|slots| slots.iter().all(|s| s["fetching"] == false)) {
                eprintln!("native ready+rescan after {:?}; concurrent waiters at entry={concurrent}", started.elapsed());
                return;
            }
        }
        assert!(Instant::now() < deadline, "watch did not become ready and settle: {} after {:?}: {}", path.display(), started.elapsed(), readiness_summary(path));
        thread::sleep(Duration::from_millis(2));
    }
}

fn external_harness(dir: &Path, watch: bool, snap: SnapshotTarget) -> (Harness, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountRealGit(
        ProviderSpec {
            key: "git",
            deps: GIT_DEPS,
            ttl: Duration::from_secs(5),
            timeout: Duration::from_secs(1),
        },
        calls.clone(),
    ));
    let watch_state = watch.then(|| tempfile::tempdir().unwrap());
    let watch_path = watch_state.as_ref().map(|d| d.path().join("watch.json"));
    let snapshot = if matches!(snap, SnapshotTarget::Disabled) { watch_path.as_ref().map(|p| SnapshotTarget::Path(p.clone())).unwrap_or(snap) } else { snap };
    let readiness_path = match &snapshot { SnapshotTarget::Path(p) => Some(p.clone()), _ => None };
    let mut options = opts(snapshot);
    options.watch_enabled = watch;
    options.idle_refresh = if watch {
        None
    } else {
        Some(Duration::from_millis(400))
    };
    let mut h = Harness::start_with(
        vec![provider],
        Box::new(ChangesShown),
        Arc::default(),
        options,
    );
    h.hello();
    let mut request = req(1, dir.to_str().unwrap());
    request.env = "TILDY_SUBMISSION_ID=0".into();
    h.send(&request);
    if watch { wait_watch_ready(readiness_path.as_ref().unwrap()); }
    h.watch_state = watch_state;
    (h, calls)
}

#[test]
fn external_watch_pushes_branch_with_ack_and_preserves_changes_on_resize() {
    let _native = native_fixture();
    let dir = real_repo();
    let (mut h, _) = external_harness(dir.path(), true, SnapshotTarget::Disabled);
    let frame = expect_fresh(&h, 1, "main changes=none", 3000);
    ack(&mut h, &frame, 4242);
    let t = Instant::now();
    real_git(dir.path(), &["checkout", "-b", "feature"]);
    let frame = expect_fresh(&h, 1, "main→feature", 1500);
    assert!(t.elapsed() < Duration::from_millis(1500));
    ack(&mut h, &frame, 4242);
    let mut resize = req(2, dir.path().to_str().unwrap());
    resize.cols = 100;
    resize.env = "TILDY_SUBMISSION_ID=0".into();
    h.send(&resize);
    expect_fresh(&h, 2, "main→feature", 1000);
    resize.gen = 3;
    resize.env = "TILDY_SUBMISSION_ID=1".into();
    h.send(&resize);
    expect_fresh(&h, 3, "feature changes=none", 1000);
}

#[test]
fn disabled_watch_falls_back_and_rejects_wrong_epoch_ack() {
    let dir = real_repo();
    let state = tempfile::tempdir().unwrap();
    let snap = state.path().join("snapshot.json");
    let (mut h, _) = external_harness(dir.path(), false, SnapshotTarget::Path(snap.clone()));
    let frame = expect_fresh(&h, 1, "main changes=none", 3000);
    ack(&mut h, &frame, 4243);
    real_git(dir.path(), &["checkout", "-b", "feature"]);
    expect_fresh(&h, 1, "feature changes=none", 1200);
    let snapshot = wait_snapshot(&snap, |v| {
        v["watch"]["fallback_count"].as_u64().is_some_and(|n| n > 0)
    });
    assert_eq!(snapshot["watch"]["mode"], "off");
}

#[test]
fn fifty_external_writes_are_coalesced_and_count_three_files() {
    let _native = native_fixture();
    let dir = real_repo();
    let (mut h, calls) = external_harness(dir.path(), true, SnapshotTarget::Disabled);
    let frame = expect_fresh(&h, 1, "main changes=none", 3000);
    ack(&mut h, &frame, 4242);
    let before = calls.load(Ordering::SeqCst);
    for n in 0..50 {
        std::fs::write(dir.path().join(format!("new{}", n % 3)), format!("{n}")).unwrap();
    }
    expect_fresh(&h, 1, "files=3", 1500);
    thread::sleep(Duration::from_millis(1100));
    assert!(calls.load(Ordering::SeqCst) - before <= 2);
}

#[test]
fn overlapping_request_timings_keep_origin_cycle_elapsed() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("snapshot.json");
    let fakes = Fakes::new(80);
    let mut h = Harness::start(
        fakes.providers(1500),
        opts(SnapshotTarget::Path(snap.clone())),
    );
    h.hello();
    h.send(&req(1, "/src/proj"));
    h.expect(1000);
    h.send(&req(2, "/src/proj"));
    expect_fresh(&h, 2, "proj#", 1500);
    let snapshot = wait_snapshot(&snap, |v| {
        v["timing_history"].as_array().is_some_and(|h| h.len() >= 2)
    });
    let git = snapshot["timing_history"][0]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "git")
        .unwrap();
    assert!(git["duration_ms"].as_u64().unwrap() >= 70, "{snapshot}");
}

#[test]
fn cached_language_stays_visible_during_warm_request() {
    let rig = ModuleRig {
        rendered_gen: Arc::new(AtomicU64::new(0)),
        early_starts: Arc::new(AtomicUsize::new(0)),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let module = rig.module("node", "22.3.0", 35, &calls);
    let mut h = Harness::start_with(
        vec![module],
        Box::new(ModulesShown {
            counts: Arc::default(),
        }),
        Arc::default(),
        opts(SnapshotTarget::Disabled),
    );
    h.hello();
    h.send(&req(1, "/project"));
    assert!(h.expect(1000).res().2.contains("node:Pending"));
    expect_fresh(&h, 1, "node=22.3.0", 1000);
    h.send(&req(2, "/project"));
    let partial = h.expect(1000);
    assert_eq!(partial.res().1, "partial");
    assert!(partial.res().2.contains("node=22.3.0"), "{partial:?}");
    expect_fresh(&h, 2, "node=22.3.0", 1000);
}

#[test]
fn unchanged_external_query_updates_timings_without_sending_duplicate_frame() {
    let dir = real_repo();
    let state = tempfile::tempdir().unwrap();
    let snap = state.path().join("s.json");
    let (h, calls) = external_harness(dir.path(), false, SnapshotTarget::Path(snap.clone()));
    expect_fresh(&h, 1, "main changes=none", 3000);
    let snapshot = wait_snapshot(&snap, |v| {
        v["watch"]["fallback_count"]
            .as_u64()
            .is_some_and(|n| n >= 1)
            && v["timing_history"].as_array().is_some_and(|h| {
                h.len() >= 2
                    && h[1]["providers"][0]["duration_ms"]
                        .as_u64()
                        .is_some_and(|ms| ms > 0)
            })
    });
    assert!(calls.load(Ordering::SeqCst) >= 2);
    assert_eq!(snapshot["counters"]["fresh_sent"], 1);
    assert!(h.next(50).is_none());
}

#[test]
fn worktree_lock_file_changes_are_not_mistaken_for_git_temporary_locks() {
    let _native = native_fixture();
    let dir = real_repo();
    let (mut h, _) = external_harness(dir.path(), true, SnapshotTarget::Disabled);
    let frame = expect_fresh(&h, 1, "main changes=none", 3000);
    ack(&mut h, &frame, 4242);
    std::fs::write(dir.path().join("Cargo.lock"), "new lockfile").unwrap();
    expect_fresh(&h, 1, "files=1", 1500);
}

struct ConfigContext { spec: ProviderSpec, path: PathBuf, calls: Arc<AtomicUsize> }
impl Provider for ConfigContext {
    fn spec(&self) -> &ProviderSpec { &self.spec }
    fn watch_paths(&self, _req: &PromptRequest) -> Vec<PathBuf> { vec![self.path.clone()] }
    fn affected_by_paths(&self, _req: &PromptRequest, paths: &[PathBuf]) -> bool { paths.contains(&self.path) }
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = std::fs::read_to_string(&self.path).unwrap();
        ProviderValue::Module("docker".into(), Slot::fresh(tildy::model::ModuleValue { text: text.clone(), fields: std::collections::BTreeMap::from([("context".into(), text)]), prerendered: None }))
    }
}
#[test]
fn external_config_replacement_only_refreshes_its_own_provider() {
    let _native = native_fixture();
    external_config_replacement_scenario();
}
fn external_config_replacement_scenario() {
    let dir = real_repo();
    let config = tempfile::tempdir().unwrap();
    let path = config.path().canonicalize().unwrap().join("config.json");
    std::fs::write(&path, "before").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let git_calls = Arc::new(AtomicUsize::new(0));
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(CountRealGit(ProviderSpec { key: "git", deps: GIT_DEPS, ttl: Duration::from_secs(5), timeout: Duration::from_secs(1) }, git_calls.clone())),
        Arc::new(ConfigContext { spec: ProviderSpec { key: "docker", deps: GIT_DEPS, ttl: Duration::from_secs(5), timeout: Duration::from_secs(1) }, path: path.clone(), calls: calls.clone() }),
    ];
    let renderer = Box::new(|d: &PromptData| RenderedPrompt {
        status: RenderStatus::Fresh,
        left: vec![format!("{} changes={}", d.modules.get("docker").and_then(|s| s.value.as_ref()).map(|v| v.text.as_str()).unwrap_or("pending"), d.changes.as_ref().map(|c| c.environments.join(",")).unwrap_or_default())],
        right: None, transient: None, history: None, observation_seq: None,
    });
    let state = tempfile::tempdir().unwrap();
    let snapshot = state.path().join("watch.json");
    let mut options = opts(SnapshotTarget::Path(snapshot.clone())); options.watch_enabled = true;
    let mut h = Harness::start_with(providers, renderer, Arc::default(), options);
    h.hello(); h.send(&req(1, dir.path().to_str().unwrap()));
    let frame = expect_fresh(&h, 1, "before changes=", 3000); ack(&mut h, &frame, 4242);
    wait_watch_ready(&snapshot);
    let before = git_calls.load(Ordering::SeqCst);
    std::fs::write(config.path().join("new.json"), "after").unwrap();
    std::fs::rename(config.path().join("new.json"), &path).unwrap();
    expect_fresh(&h, 1, "before → after", 1500);
    assert_eq!(git_calls.load(Ordering::SeqCst), before);
    assert!(calls.load(Ordering::SeqCst) >= 2);
}

#[test]
fn overlapping_cycle_timeout_marks_the_execution_origin() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("s.json");
    let fakes = Fakes::new(1000);
    let mut options = opts(SnapshotTarget::Path(snap.clone()));
    options.scheduler.outer_grace = Duration::ZERO;
    let mut h = Harness::start(fakes.providers(60), options);
    h.hello(); h.send(&req(1, "/src/proj")); h.expect(1000);
    h.send(&req(2, "/src/proj"));
    let snapshot = wait_snapshot(&snap, |v| v["timing_history"][0]["providers"].as_array().is_some_and(|p| p.iter().any(|p| p["provider"] == "git" && p["timed_out"] == true)));
    let git = snapshot["timing_history"][0]["providers"].as_array().unwrap().iter().find(|p| p["provider"] == "git").unwrap();
    assert!(git["duration_ms"].as_u64().unwrap() >= 60);
}

struct SlowRealGit { spec: ProviderSpec, delay_ms: Arc<AtomicU64> }
impl Provider for SlowRealGit {
    fn spec(&self) -> &ProviderSpec { &self.spec }
    fn fetch(&self, req: &PromptRequest) -> ProviderValue {
        thread::sleep(Duration::from_millis(self.delay_ms.load(Ordering::SeqCst)));
        ProviderValue::Git(tildy::providers::git::git_status(&req.cwd, Duration::from_secs(1)))
    }
}

#[test]
fn own_checkout_with_slow_revalidation_never_uses_acked_partial_as_idle_baseline() {
    let _native = native_fixture();
    let dir = real_repo();
    let delay_ms = Arc::new(AtomicU64::new(0));
    let provider = Arc::new(SlowRealGit { spec: ProviderSpec { key: "git", deps: GIT_DEPS, ttl: Duration::from_secs(5), timeout: Duration::from_secs(2) }, delay_ms: delay_ms.clone() });
    let state = tempfile::tempdir().unwrap();
    let snapshot = state.path().join("watch.json");
    let mut options = opts(SnapshotTarget::Path(snapshot.clone())); options.watch_enabled = true;
    let mut h = Harness::start_with(vec![provider], Box::new(ChangesShown), Arc::default(), options);
    h.hello();
    let mut request = req(1, dir.path().to_str().unwrap()); request.env = "TILDY_SUBMISSION_ID=0".into();
    h.send(&request);
    let fresh = expect_fresh(&h, 1, "main changes=none", 3000); ack(&mut h, &fresh, 4242);
    wait_watch_ready(&snapshot);
    delay_ms.store(400, Ordering::SeqCst);
    real_git(dir.path(), &["checkout", "-b", "own-checkout"]);
    // Let the own-command filesystem event enter the previous request's
    // debounce gate, then issue precmd's request before the 300ms timer fires.
    thread::sleep(Duration::from_millis(50));
    request.gen = 2; request.env = "TILDY_SUBMISSION_ID=1".into(); h.send(&request);
    let partial = h.expect(1000);
    assert_eq!(partial.res(), (2, "partial", "main changes=none"));
    ack(&mut h, &partial, 4242);
    let end = Instant::now() + Duration::from_secs(3);
    let mut settled = false;
    while let Some((_, frame)) = h.next(end.saturating_duration_since(Instant::now()).as_millis() as u64) {
        let (gen, status, text) = frame.res();
        assert!(!text.contains("main→own-checkout"), "own command was misclassified as idle: {frame:?}");
        if gen == 2 && status == "fresh" && text == "own-checkout changes=none" {
            ack(&mut h, &frame, 4242); settled = true; break;
        }
    }
    assert!(settled, "own command never settled without a change hint");
    for frame in h.drain(500) { assert!(!frame.res().2.contains("main→own-checkout"), "{frame:?}"); }
    // Establishing no baseline during the own command must not permanently
    // disable later idle changes in the same edit cycle.
    real_git(dir.path(), &["checkout", "-b", "external-after-own"]);
    let external = expect_fresh(&h, 2, "own-checkout→external-after-own", 1500);
    ack(&mut h, &external, 4242);
    request.gen = 3; request.cols = 100; h.send(&request);
    expect_fresh(&h, 3, "own-checkout→external-after-own", 1500);

}

struct SlowPureHost { spec: ProviderSpec, calls: Arc<AtomicUsize> }
impl Provider for SlowPureHost {
    fn spec(&self) -> &ProviderSpec { &self.spec }
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        self.calls.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(80));
        ProviderValue::Hostname(Some("shared-host".into()))
    }
}

#[test]
fn coalesced_previous_cycle_fetch_marks_received_completion_without_execution() {
    let state = tempfile::tempdir().unwrap();
    let snap = state.path().join("s.json");
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(SlowPureHost {
        spec: ProviderSpec { key: "session/hostname", deps: NO_DEPS, ttl: Duration::from_secs(60), timeout: Duration::from_secs(1) },
        calls: calls.clone(),
    });
    let mut h = Harness::start(vec![provider], opts(SnapshotTarget::Path(snap.clone())));
    h.hello();
    h.send(&req(1, "/project"));
    assert_eq!(h.expect(1000).res().1, "partial");
    h.send(&req(2, "/project"));
    assert_eq!(h.expect(1000).res().1, "partial");
    expect_fresh(&h, 2, "shared-host", 1000);
    let snapshot = wait_snapshot(&snap, |v| {
        v["timing_history"].as_array().is_some_and(|cycles| cycles.len() == 2
            && cycles[0]["providers"][0]["completed"] == true
            && cycles[1]["providers"][0]["completed"] == true)
    });
    let origin = &snapshot["timing_history"][0]["providers"][0];
    let current = &snapshot["timing_history"][1]["providers"][0];
    assert_eq!(origin["executed"], true);
    assert!(origin["duration_ms"].as_u64().unwrap() >= 70);
    assert_eq!(current["executed"], false);
    assert_eq!(current["coalesced"], true);
    assert_eq!(current["coalesced_count"], 1);
    assert_eq!(current["cache_hit"], false);
    assert_eq!(current["duration_ms"], 0, "shared execution duration belongs to origin");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The next pure cache-hit cycle has no new completion event.
    h.send(&req(3, "/project"));
    expect_fresh(&h, 3, "shared-host", 1000);
    let snapshot = wait_snapshot(&snap, |v| v["timing_history"].as_array().is_some_and(|c| c.len() == 3));
    let cache = &snapshot["timing_history"][2]["providers"][0];
    assert_eq!(cache["cache_hit"], true);
    assert_eq!(cache["executed"], false);
    assert_eq!(cache["completed"], false);
}

#[test]
fn concurrent_multi_path_watch_startup_keeps_settled_frames_and_targeted_refresh() {
    let _native = native_fixture();
    // Each instance registers a worktree and a distinct external config
    // parent. Keep the original 3s startup and 1.5s change contracts while
    // several FSEvents streams are created concurrently.
    let workers: Vec<_> = (0..4).map(|_| thread::spawn(external_config_replacement_scenario)).collect();
    for worker in workers { worker.join().unwrap(); }
}

struct LanguageGate {
    provider: tildy::providers::modules::ModuleProvider,
    started: mpsc::Sender<()>,
    released: Arc<(Mutex<bool>, std::sync::Condvar)>,
}
impl Provider for LanguageGate {
    fn spec(&self) -> &ProviderSpec { self.provider.spec() }
    fn fetch(&self, req: &PromptRequest) -> ProviderValue {
        self.started.send(()).unwrap();
        let (lock, cv) = &*self.released;
        let mut released = lock.lock().unwrap();
        while !*released { released = cv.wait(released).unwrap(); }
        self.provider.fetch(req)
    }
}
struct ActualLanguageRenderer {
    cfg: tildy::config::Config,
    reports: Vec<tildy::model::ModuleReport>,
    trace: Arc<Mutex<Vec<Vec<tildy::model::ModuleReport>>>>,
}
impl PromptRenderer for ActualLanguageRenderer {
    fn render(&mut self, data: &PromptData) -> RenderedPrompt {
        let out = tildy::render::render(&self.cfg, data);
        self.reports = out.reports;
        self.trace.lock().unwrap().push(self.reports.clone());
        out.prompt
    }
    fn reports(&self) -> Vec<tildy::model::ModuleReport> { self.reports.clone() }
    fn revision(&self) -> Option<String> { Some(self.cfg.revision.clone()) }
}

#[test]
fn instant_language_handoff_keeps_cached_version_until_detection_settles() {
    use tildy::providers::modules::{Detect, ModuleProvider};
    use tildy::session::instant::{self, Identity, PresentationEnv};
    use tildy::session::{assemble, ProviderSlot};
    for outcome in ["fresh", "not_detected", "error", "timeout", "config_changed", "cwd_changed", "cols_changed", "env_changed", "legacy", "miss", "font"] {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("package.json"), "{}").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cfg = tildy::config::from_str(r#"{"schema_version":1,"add_newline":false,"symbols":"ascii","modules":{"node":{"type":"language","language":"nodejs","format":"$symbol $version","version_format":"${raw}","symbol":{"ascii":"Node"}}},"layout":{"rows":[{"left":["node"]}]}}"#).unwrap();
    let mut request = req(1, dir.path().to_str().unwrap());
    let cached_data = assemble(&request, None, &[ProviderSlot {
        key: "node", freshness: Freshness::Fresh, age_ms: Some(0), error: None,
        value: Some(ProviderValue::Module("node".into(), Slot::fresh(ModuleValue {
            text: "22.3.0".into(), fields: BTreeMap::from([("version".into(), "22.3.0".into())]), prerendered: None,
        }))),
    }]);
    let cached = tildy::render::render(&cfg, &cached_data);
    assert!(cached.reports[0].visible && cached.prompt.left[0].contains("22.3.0"));
    let mut text = instant::encode(&cached.prompt);
    let mut previous = request.clone();
    if outcome == "cwd_changed" { previous.cwd = "/other".into(); }
    if outcome == "cols_changed" { previous.cols += 1; }
    if outcome == "env_changed" { previous.env.push_str("changed"); }
    let revision = if outcome == "config_changed" { "old-revision".into() } else { cfg.revision.clone() };
    if outcome != "legacy" { Identity::new(&previous, revision, &PresentationEnv::default()).append_to(&mut text); }
    std::fs::write(cache.path().join("handoff.prompt"), text).unwrap();
    let (started, ready) = mpsc::channel();
    let released = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let command = if outcome == "error" { "exit 7" } else { "printf '22.3.0\\n'" };
    let module = ModuleProvider::command("node", vec!["/bin/sh".into(), "-c".into(), command.into()],
        Detect::from_static(&["package.json"], &[], &[]), None, Duration::from_secs(600), if outcome == "timeout" { Duration::from_millis(80) } else { Duration::from_secs(2) });
    let trace = Arc::new(Mutex::new(vec![]));
    let renderer = ActualLanguageRenderer { cfg, reports: vec![], trace: trace.clone() };
    let mut options = opts(SnapshotTarget::Disabled); options.instant_dir = Some(cache.path().into());
    let mut h = Harness::start_with(vec![Arc::new(LanguageGate { provider: module, started, released: released.clone() })],
        Box::new(renderer), Arc::default(), options);
    h.hello();
    request.env.push_str("\x1fTILDY_INSTANT_KEY=handoff");
    if outcome != "miss" { request.env.push_str("\x1fTILDY_INSTANT_SHOWN=1"); }
    if outcome == "font" { request.env.push_str("\x1fTILDY_FONT_REVISION=calibrated"); }
    h.send(&request);
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    let premature = h.next(if outcome == "timeout" { 10 } else { 80 });
    if outcome == "not_detected" { std::fs::remove_file(dir.path().join("package.json")).unwrap(); }
    let timeout_frame = (outcome == "timeout").then(|| h.expect(1500));
    { let (lock, cv) = &*released; *lock.lock().unwrap() = true; cv.notify_all(); }
    let protected = matches!(outcome, "fresh" | "not_detected" | "error" | "timeout");
    if protected { assert!(premature.is_none(), "valid instant hit must not be replaced by an unknown-language partial: {premature:?}; reports={:?}", trace.lock().unwrap()); }
    else { assert_eq!(premature.unwrap().1.res().1, "partial", "{outcome}: invalid or absent instant hit must not delay the cold partial"); }
    let fresh = timeout_frame.unwrap_or_else(|| h.expect(1500));
    assert_eq!(fresh.res().1, "fresh");
    if !matches!(outcome, "not_detected" | "error" | "timeout") { assert!(fresh.res().2.contains("22.3.0"), "{outcome}: {fresh:?}"); }
    else {
        let trace = trace.lock().unwrap(); let report = &trace.last().unwrap()[0];
        assert!(!report.visible, "{outcome}: {report:?}");
        assert_eq!(report.hidden_reason.as_deref(), Some(outcome), "{report:?}");
    }
    }
}

struct MissingExternalConfig { path: PathBuf, calls: Arc<AtomicUsize>, spec: ProviderSpec }
impl Provider for MissingExternalConfig {
    fn spec(&self) -> &ProviderSpec { &self.spec }
    fn watch_paths(&self, _req: &PromptRequest) -> Vec<PathBuf> { vec![self.path.clone()] }
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = std::fs::read_to_string(&self.path).unwrap_or_else(|_| "missing".into());
        ProviderValue::Module("docker".into(), Slot::fresh(tildy::model::ModuleValue {
            text, fields: std::collections::BTreeMap::new(), prerendered: None,
        }))
    }
}
struct ConservativeBridge { calls: Arc<AtomicUsize>, spec: ProviderSpec }
impl Provider for ConservativeBridge {
    fn spec(&self) -> &ProviderSpec { &self.spec }
    // Intentionally keep the conservative default affected_by_paths: this
    // models Starship and any other provider without external watch targets.
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ProviderValue::Module("bridge".into(), Slot::fresh(tildy::model::ModuleValue {
            text: String::new(), fields: std::collections::BTreeMap::new(), prerendered: None,
        }))
    }
}

#[test]
fn snapshot_beside_missing_external_config_does_not_refresh_unrelated_providers() {
    let _native = native_fixture();
    external_config_feedback_scenario(0);
}
#[test]
fn explicitly_watched_config_in_git_metadata_bypasses_only_its_noise_filter() {
    let _native = native_fixture();
    external_config_feedback_scenario(1);
}
#[test]
fn linked_worktree_common_git_config_events_only_reach_the_declared_provider() {
    let _native = native_fixture();
    external_config_feedback_scenario(2);
}
fn external_config_feedback_scenario(location: u8) {
    let repo = real_repo();
    let home = tempfile::tempdir().unwrap();
    let linked = tempfile::tempdir().unwrap();
    let linked_path = linked.path().join("linked");
    if location == 2 { real_git(repo.path(), &["worktree", "add", "-b", "linked", linked_path.to_str().unwrap()]); }
    let cwd = if location == 2 { linked_path.as_path() } else { repo.path() };
    // Preserve the lexical /var alias on macOS: native events use /private/var.
    let config = if location == 0 { home.path().join(".docker/config.json") } else { repo.path().join(".git/custom/config.json") };
    let snapshot = home.path().join("snapshot.json");
    let docker_calls = Arc::new(AtomicUsize::new(0));
    let bridge_calls = Arc::new(AtomicUsize::new(0));
    let git_calls = Arc::new(AtomicUsize::new(0));
    let spec = |key| ProviderSpec { key, deps: GIT_DEPS, ttl: Duration::from_secs(5), timeout: Duration::from_secs(1) };
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(CountRealGit(spec("git"), git_calls.clone())),
        Arc::new(MissingExternalConfig { path: config.clone(), calls: docker_calls.clone(), spec: spec("docker") }),
        Arc::new(ConservativeBridge { calls: bridge_calls.clone(), spec: spec("bridge") }),
    ];
    let renderer = Box::new(|d: &PromptData| RenderedPrompt {
        status: RenderStatus::Fresh,
        left: vec![d.modules.get("docker").and_then(|s| s.value.as_ref()).map(|v| v.text.clone()).unwrap_or("pending".into())],
        right: None, transient: None, history: None, observation_seq: None,
    });
    let mut options = opts(SnapshotTarget::Path(snapshot.clone())); options.watch_enabled = true;
    let mut h = Harness::start_with(providers, renderer, Arc::default(), options);
    h.hello(); h.send(&req(1, cwd.to_str().unwrap()));
    expect_fresh(&h, 1, "missing", 3000);
    wait_watch_ready(&snapshot);
    let stable = (bridge_calls.load(Ordering::SeqCst), docker_calls.load(Ordering::SeqCst), git_calls.load(Ordering::SeqCst));
    let before = wait_snapshot(&snapshot, |_| true);
    // More than two native debounce/rate-limit windows. Snapshot writes here
    // are real atomic writes inside the missing config's watched HOME ancestor.
    for n in 0..5 {
        std::fs::write(home.path().join("unrelated.txt"), format!("noise {n}")).unwrap();
        std::fs::create_dir(home.path().join("unrelated-directory")).unwrap();
        std::fs::remove_dir(home.path().join("unrelated-directory")).unwrap();
        thread::sleep(Duration::from_millis(250));
    }
    h.drain(1000);
    assert_eq!((bridge_calls.load(Ordering::SeqCst), docker_calls.load(Ordering::SeqCst), git_calls.load(Ordering::SeqCst)), stable,
        "snapshot/unrelated HOME events caused provider refresh; bridge={} docker={} git={}", bridge_calls.load(Ordering::SeqCst), docker_calls.load(Ordering::SeqCst), git_calls.load(Ordering::SeqCst));
    let after = wait_snapshot(&snapshot, |_| true);
    assert_eq!(after["timing_history"], before["timing_history"], "idle snapshot writes opened new cycles");
    assert_eq!(after["watch"]["coalesced_events"], before["watch"]["coalesced_events"]);

    // Creating a previously missing ancestor must narrow the watch, then an
    // atomic file replacement must still refresh only the declared provider.
    std::fs::create_dir(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "created").unwrap();
    expect_fresh(&h, 1, "created", 1500);
    wait_watch_ready_for_cycles(&snapshot, 4);
    // Narrowing HOME -> .docker replaces the native stream. Its one existing
    // ready-gap rescan legitimately revalidates all cwd providers.
    let narrowed = (bridge_calls.load(Ordering::SeqCst), docker_calls.load(Ordering::SeqCst), git_calls.load(Ordering::SeqCst));
    assert_eq!(narrowed.0, stable.0 + 1);
    assert_eq!(narrowed.2, stable.2 + 1);
    h.drain(1200);
    assert_eq!((bridge_calls.load(Ordering::SeqCst), docker_calls.load(Ordering::SeqCst), git_calls.load(Ordering::SeqCst)), narrowed);
    let temporary = config.with_file_name("replacement.json");
    std::fs::write(&temporary, "replaced").unwrap();
    std::fs::rename(&temporary, &config).unwrap();
    expect_fresh(&h, 1, "replaced", 1500);
    assert_eq!(bridge_calls.load(Ordering::SeqCst), narrowed.0);
    assert_eq!(git_calls.load(Ordering::SeqCst), narrowed.2);
}

#[test]
fn removed_external_parent_recovers_via_configured_fallback_and_recreated_watch() {
    let _native = native_fixture();
    let repo = real_repo();
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".docker/config.json");
    std::fs::create_dir(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "before").unwrap();
    let snapshot = home.path().join("snapshot.json");
    let calls = Arc::new(AtomicUsize::new(0));
    let git_calls = Arc::new(AtomicUsize::new(0));
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(CountRealGit(ProviderSpec { key: "git", deps: GIT_DEPS, ttl: Duration::from_secs(5), timeout: Duration::from_secs(1) }, git_calls)),
        Arc::new(MissingExternalConfig { path: config.clone(), calls: calls.clone(), spec: ProviderSpec { key: "docker", deps: GIT_DEPS, ttl: Duration::from_secs(5), timeout: Duration::from_secs(1) } }),
    ];
    let renderer = Box::new(|d: &PromptData| RenderedPrompt {
        status: RenderStatus::Fresh,
        left: vec![d.modules.get("docker").and_then(|s| s.value.as_ref()).map(|v| v.text.clone()).unwrap_or("pending".into())],
        right: None, transient: None, history: None, observation_seq: None,
    });
    let mut options = opts(SnapshotTarget::Path(snapshot.clone()));
    options.watch_enabled = true;
    // RootChanged can fall back. Exercise the existing configurable interval;
    // the production default remains ten seconds.
    options.idle_refresh = Some(Duration::from_millis(100));
    let mut h = Harness::start_with(providers, renderer, Arc::default(), options);
    h.hello(); h.send(&req(1, repo.path().to_str().unwrap()));
    expect_fresh(&h, 1, "before", 3000);
    wait_watch_ready(&snapshot);
    std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    expect_fresh(&h, 1, "missing", 3000);
    std::fs::create_dir(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "restored").unwrap();
    expect_fresh(&h, 1, "restored", 3000);
    wait_watch_ready(&snapshot);
    // Allow the bounded ready-gap rescan after rebuilding the target, then
    // require that fallback and self-written snapshots do not keep it running.
    h.drain(1600);
    let baseline = calls.load(Ordering::SeqCst);
    h.drain(1200);
    assert_eq!(calls.load(Ordering::SeqCst), baseline);
    let mode = if cfg!(target_os = "macos") { "fsevents" } else { "events" };
    let value = wait_snapshot(&snapshot, |v| v["watch"]["mode"] == mode);
    assert!(value["slots"].as_array().unwrap().iter().all(|s| s["fetching"] == false));
}
