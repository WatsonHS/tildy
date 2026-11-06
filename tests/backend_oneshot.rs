//! One-shot core (`tildy render`) with fake providers and renderer.

use tildy::model::{
    Dependency, Freshness, GitData, PromptData, PromptRequest, ProviderSpec, RenderStatus, RenderedPrompt, Slot,
};
use tildy::protocol::encode_request;
use tildy::providers::{Provider, ProviderValue};
use tildy::session::oneshot::{run_oneshot, OneshotError, OneshotOptions};
use tildy::session::PromptRenderer;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const GIT_DEPS: &[Dependency] = &[Dependency::Cwd];

struct Git {
    delay: Duration,
    spec: ProviderSpec,
}

impl Provider for Git {
    fn spec(&self) -> &ProviderSpec {
        &self.spec
    }
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        std::thread::sleep(self.delay);
        ProviderValue::Git(Slot::fresh(GitData { in_repo: true, branch: Some("main".into()), ..GitData::default() }))
    }
}

struct User {
    spec: ProviderSpec,
    panic: bool,
}

impl Provider for User {
    fn spec(&self) -> &ProviderSpec {
        &self.spec
    }
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        if self.panic {
            panic!("fake provider panic (expected in this test)");
        }
        ProviderValue::Username(Some("me".into()))
    }
}

fn providers(git_delay_ms: u64, user_panics: bool) -> Vec<Arc<dyn Provider>> {
    let spec = |key| ProviderSpec { key, deps: GIT_DEPS, ttl: Duration::from_secs(5), timeout: Duration::from_secs(1) };
    vec![
        Arc::new(Git { delay: Duration::from_millis(git_delay_ms), spec: spec("git") }),
        Arc::new(User { spec: spec("username"), panic: user_panics }),
    ]
}

#[derive(Default)]
struct Renderer {
    refreshed: AtomicUsize,
    seen: std::sync::Mutex<Option<PromptData>>,
}

impl PromptRenderer for &Renderer {
    fn refresh(&mut self) {
        self.refreshed.fetch_add(1, Ordering::SeqCst);
    }
    fn render(&mut self, d: &PromptData) -> RenderedPrompt {
        *self.seen.lock().unwrap() = Some(d.clone());
        let git = match d.git.freshness {
            Freshness::Fresh => d.git.value.as_ref().and_then(|g| g.branch.clone()).unwrap_or_default(),
            Freshness::Timeout => "?".into(),
            other => format!("{other:?}"),
        };
        RenderedPrompt {
            status: RenderStatus::Partial,
            left: vec![format!("{} {git} {}", d.request.cwd.display(), d.username.as_deref().unwrap_or("-"))],
            right: None,
            transient: Some("❯ ".into()),
            history: None,
            observation_seq: None,
        }
    }
}

fn req_line(gen: u64) -> Vec<u8> {
    let r = PromptRequest {
        gen,
        session_id: "1.2".into(),
        cwd: PathBuf::from("/tmp/odd\tdir"),
        cols: 80,
        lines: 24,
        last_exit: 130,
        last_duration_ms: 4000,
        env: String::new(),
        jobs: 0,
    };
    let mut l = encode_request(&r);
    l.push(b'\n');
    l
}

fn run(input: &[u8], provs: Vec<Arc<dyn Provider>>, timeout_ms: u64) -> (Result<Option<RenderStatus>, OneshotError>, String, Renderer) {
    let renderer = Renderer::default();
    let mut out = Vec::new();
    let opts = OneshotOptions { timeout: Duration::from_millis(timeout_ms), home: None };
    let mut r = &renderer;
    let result = run_oneshot(input, &mut out, move || provs, &mut r, &opts);
    (result, String::from_utf8(out).unwrap(), renderer)
}

#[test]
fn writes_one_fresh_frame_with_epoch_zero() {
    let (r, out, renderer) = run(&req_line(5), providers(5, false), 2000);
    assert_eq!(r.unwrap(), Some(RenderStatus::Fresh));
    assert_eq!(out, "RES\t5\tfresh\t0\nL\t/tmp/odddir main me\nT\t❯ \nEND\t5\n", "tab in cwd stripped from output");
    assert_eq!(renderer.refreshed.load(Ordering::SeqCst), 1);
    let seen = renderer.seen.lock().unwrap().clone().unwrap();
    assert_eq!(seen.request.cwd, PathBuf::from("/tmp/odd\tdir"));
    assert_eq!((seen.request.last_exit, seen.request.last_duration_ms), (130, 4000));
}

#[test]
fn deadline_gives_partial_with_timeout_slot() {
    let t = Instant::now();
    let (r, out, renderer) = run(&req_line(2), providers(800, false), 60);
    let elapsed = t.elapsed();
    assert_eq!(r.unwrap(), Some(RenderStatus::Partial));
    assert_eq!(out, "RES\t2\tpartial\t0\nL\t/tmp/odddir ? me\nT\t❯ \nEND\t2\n");
    assert!(elapsed < Duration::from_millis(300), "returns at the deadline, not when git finishes: {elapsed:?}");
    let seen = renderer.seen.lock().unwrap().clone().unwrap();
    assert_eq!(seen.git.freshness, Freshness::Timeout);
    assert_eq!(seen.git.error.as_deref(), Some("no result within 60 ms"));
}

#[test]
fn panicking_provider_is_an_error_slot() {
    let (r, out, renderer) = run(&req_line(3), providers(0, true), 2000);
    assert_eq!(r.unwrap(), Some(RenderStatus::Fresh));
    assert!(out.contains("L\t/tmp/odddir main -\n"), "{out}");
    assert!(renderer.seen.lock().unwrap().as_ref().unwrap().username.is_none());
}

#[test]
fn protocol_errors_write_nothing_and_skip_providers() {
    for input in [
        &b""[..],
        b"\n",
        b"garbage\n",
        b"REQ\t1\ts\t/\t80\t24\t0\t0\n",
        b"REQ\tx\ts\t/\t80\t24\t0\t0\t\n",
        b"HELLO\t1\t0.1\t1\t1\n",
    ] {
        let called = Arc::new(AtomicUsize::new(0));
        let c = called.clone();
        let renderer = Renderer::default();
        let mut r = &renderer;
        let mut out = Vec::new();
        let result = run_oneshot(input, &mut out, move || {
            c.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        }, &mut r, &OneshotOptions::default());
        assert!(matches!(result, Err(OneshotError::Protocol(_))), "{input:?}: {result:?}");
        assert!(out.is_empty());
        assert_eq!(called.load(Ordering::SeqCst), 0, "providers are not built for a bad request");
        assert_eq!(renderer.refreshed.load(Ordering::SeqCst), 0, "config is not loaded for a bad request");
    }
    let mut long = vec![b'x'; 70 * 1024];
    long.push(b'\n');
    let (r, out, _) = run(&long, providers(0, false), 100);
    assert!(matches!(r, Err(OneshotError::Protocol(m)) if m.contains("exceeds")));
    assert!(out.is_empty());
}

#[test]
fn bye_and_trailing_input() {
    let (r, out, _) = run(b"BYE\n", providers(0, false), 100);
    assert_eq!(r.unwrap(), None);
    assert!(out.is_empty());
    // Only the first line is read; a request without trailing newline works.
    let mut input = req_line(8);
    input.pop();
    let (r, out, _) = run(&input, providers(0, false), 2000);
    assert_eq!(r.unwrap(), Some(RenderStatus::Fresh));
    assert!(out.starts_with("RES\t8\tfresh\t0\n"));
    let mut two = req_line(1);
    two.extend(req_line(2));
    let (_, out, _) = run(&two, providers(0, false), 2000);
    assert_eq!(out.matches("RES\t").count(), 1);
    assert!(out.starts_with("RES\t1\t"));
}

#[test]
fn closed_output_is_reported_quietly() {
    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let renderer = Renderer::default();
    let mut r = &renderer;
    let line = req_line(1);
    let provs = providers(0, false);
    let result = run_oneshot(&line[..], Closed, move || provs, &mut r, &OneshotOptions::default());
    assert!(matches!(result, Err(OneshotError::OutputClosed)));
}

#[test]
fn presentation_metadata_is_hidden_from_providers_and_restored_only_for_rendering() {
    struct InspectEnv { spec: ProviderSpec, seen: Arc<std::sync::Mutex<String>> }
    impl Provider for InspectEnv {
        fn spec(&self) -> &ProviderSpec { &self.spec }
        fn fetch(&self, req: &PromptRequest) -> ProviderValue {
            *self.seen.lock().unwrap() = req.env.clone();
            ProviderValue::Username(Some("me".into()))
        }
    }
    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    let request = PromptRequest { gen:1,session_id:"meta".into(),cwd:"/tmp".into(),cols:80,lines:24,last_exit:0,last_duration_ms:0,jobs:0,
        env:"USER=u\x1fPATH=/bin\x1fTERM=xterm\x1fTERM_PROGRAM=Ghostty\x1fTERM_PROGRAM_VERSION=1\x1fLANG=en_US.UTF-8\x1fTILDY_FONT_REVISION=test-font\x1fTILDY_INSTANT_KEY=unsafe\x1fTILDY_INSTANT_SHOWN=1".into() };
    let mut line = encode_request(&request); line.push(b'\n');
    let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(InspectEnv {
        spec:ProviderSpec { key:"username",deps:&[Dependency::Env],ttl:Duration::from_secs(1),timeout:Duration::from_secs(1) },seen:captured.clone()
    })];
    let (result,_,renderer) = run(&line,providers,1000);
    assert_eq!(result.unwrap(),Some(RenderStatus::Fresh));
    assert_eq!(*captured.lock().unwrap(),"USER=u\x1fPATH=/bin");
    let rendered=renderer.seen.lock().unwrap().clone().unwrap();
    for pair in ["TERM=xterm","TERM_PROGRAM=Ghostty","TERM_PROGRAM_VERSION=1","LANG=en_US.UTF-8","TILDY_FONT_REVISION=test-font"] {
        assert!(rendered.request.env.split('\x1f').any(|v|v==pair),"{pair}");
    }
    assert!(!rendered.request.env.contains("TILDY_INSTANT_"));
}
