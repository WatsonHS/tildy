//! `tildy agent wrap [--app NAME] [--scope …] -- CMD…`: run CMD in a pty,
//! forward its bytes both ways untouched and record its OSC 7501 program
//! status reports as agent status records. Owner: M2 task D. See
//! docs/agent-wrap.md.
//!
//! Transparency comes first, because the Zsh adapter runs `claude`, `codex`
//! and friends through this command by default:
//!
//! - When the pty cannot be set up for any reason (stdin, stdout and stderr
//!   are not all the same terminal, the store is unusable, `openpty`, fork or
//!   the command's start fails) or `TILDY_AGENT_WRAP=0`, CMD is exec'd
//!   directly with the inherited descriptors.
//! - Nothing but the child's bytes reaches the real stdout/stderr. Diagnostics
//!   go to `TILDY_DEBUG_LOG` when set, and to stderr only with `TILDY_DEBUG=1`
//!   and only before the command starts. The one exception is an exec failure
//!   (`command not found`), reported like a shell would, exit 127/126.
//! - Output is written to the terminal before it is scanned; recording runs on
//!   its own thread so a slow store never delays forwarding, and a panic in
//!   the scanner or the store only stops recording.
//! - The real terminal is in raw mode while the command runs and is restored
//!   on every way out (exit, forwarded signals, panics) and while suspended.
//!   SIGINT/SIGTERM/SIGHUP/SIGQUIT sent to wrap go to the command's process
//!   group; SIGWINCH copies the window size to the pty. The exit status is
//!   the command's (128+N when killed by signal N).
//! - Job control works as without wrap: a forked monitor process leads the
//!   pty's session and runs the command in its own foreground process group
//!   (as sudo's `use_pty` does), so Ctrl-Z stops the command, wrap then
//!   stops itself and the user's shell reports the job suspended; `fg`
//!   continues both.
//!
//! Process tree: shell → wrap (proxy, records) → monitor (pty session
//! leader) → CMD.

use clap::Args;
use tildy::agent::osc::{self, Event, Kind, Report, Scanner, State};
use tildy::agent::{self, Filter, Record, Scope, Source, Store};
use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Args)]
pub struct WrapArgs {
    /// Stable program name for the records (default: basename of CMD).
    #[arg(long)]
    pub app: Option<String>,
    /// Where the records show: session, worktree or global (default: the
    /// worktree when the current directory is in a git worktree, else global).
    #[arg(long, value_name = "session|worktree|global")]
    pub scope: Option<String>,
    /// Scope id (session: default $TILDY_SESSION_ID; worktree: default the
    /// current directory's worktree identity).
    #[arg(long, value_name = "ID")]
    pub scope_id: Option<String>,
    /// Testing aid: append every scanner event and record operation to FILE as
    /// JSON lines.
    #[arg(long, value_name = "FILE", hide = true)]
    pub debug_reports: Option<PathBuf>,
    /// Command and arguments (after `--`); any bytes, not only UTF-8.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true, value_parser = clap::value_parser!(OsString))]
    pub cmd: Vec<OsString>,
}

pub fn run(a: WrapArgs) -> i32 {
    if std::env::var_os("TILDY_AGENT_WRAP").is_some_and(|v| v == "0") {
        return exec_direct(&a.cmd);
    }
    let app = app_name(a.app.as_deref(), &a.cmd[0]);
    match Proxy::setup(&a, &app) {
        Ok(p) => p.run(),
        Err(why) => {
            diag(&format!("running {} directly: {why}", a.cmd[0].to_string_lossy()));
            exec_direct(&a.cmd)
        }
    }
}

/// Replace this process with CMD (the fallback path). Returns only when the
/// exec fails, with the shell's exit codes for that.
fn exec_direct(cmd: &[OsString]) -> i32 {
    let err = Command::new(&cmd[0]).args(&cmd[1..]).exec();
    let what = match err.kind() {
        io::ErrorKind::NotFound => "command not found".to_string(),
        io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => err.to_string(),
    };
    eprintln!("tildy agent wrap: {what}: {}", cmd[0].to_string_lossy());
    if err.kind() == io::ErrorKind::NotFound {
        127
    } else {
        126
    }
}

/// `--app`, else the basename of CMD, coerced into the 7501 `app` grammar
/// (`[A-Za-z0-9_.+-]{1,32}`): other characters become `_`.
pub(crate) fn app_name(explicit: Option<&str>, cmd0: &OsStr) -> String {
    let raw = match explicit.filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => Path::new(cmd0).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
    };
    let s: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "_.+-".contains(c) { c } else { '_' })
        .take(osc::MAX_APP)
        .collect();
    if s.is_empty() {
        "app".into()
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// Diagnostics: never on the real stdout/stderr once the command runs.

static CHILD_STARTED: AtomicBool = AtomicBool::new(false);

fn diag(msg: &str) {
    static LOG: OnceLock<Option<PathBuf>> = OnceLock::new();
    let log = LOG.get_or_init(|| std::env::var_os("TILDY_DEBUG_LOG").filter(|v| !v.is_empty()).map(PathBuf::from));
    if let Some(path) = log {
        // Same shape as the Zsh adapter's log: "<ms>\t<event>\t<fields>".
        let line = format!("{}\twrap\t{}\n", agent::now_ms(), msg.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n"));
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
    if !CHILD_STARTED.load(Ordering::SeqCst) && std::env::var("TILDY_DEBUG").is_ok_and(|v| v == "1") {
        eprintln!("tildy agent wrap: {msg}");
    }
}


// ---------------------------------------------------------------------------
// Record bookkeeping: 7501 terminal semantics for the wrapped pty.

/// Records a wrapped program may hold at once (7501: 256 per terminal; the
/// least recently updated one is evicted when full).
const MAX_RECORDS: usize = 256;

/// One record change for the store, with the full store id
/// (`wrap/<app>/<pid>[/<report id>]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    Publish(Pub),
    /// Remove the record `id`, and with `subtree` its descendants too.
    Remove { id: String, subtree: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pub {
    pub id: String,
    pub app: String,
    pub state: State,
    pub kind: Option<Kind>,
    pub progress: Option<u8>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

impl Op {
    #[cfg(test)]
    fn id(&self) -> &str {
        match self {
            Op::Publish(p) => &p.id,
            Op::Remove { id, .. } => id,
        }
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            Op::Publish(p) => {
                let mut v = serde_json::json!({"op": "publish", "id": p.id, "app": p.app, "state": p.state});
                let o = v.as_object_mut().expect("object");
                if let Some(k) = p.kind {
                    o.insert("kind".into(), serde_json::json!(k));
                }
                if let Some(n) = p.progress {
                    o.insert("progress".into(), n.into());
                }
                if let Some(t) = &p.title {
                    o.insert("title".into(), t.clone().into());
                }
                if let Some(m) = &p.msg {
                    o.insert("msg".into(), m.clone().into());
                }
                v
            }
            Op::Remove { id, subtree } => serde_json::json!({"op": "remove", "id": id, "subtree": subtree}),
        }
    }
}

/// How the command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    Code(i32),
    Signal(i32),
}

impl Exit {
    pub fn code(self) -> i32 {
        match self {
            Exit::Code(c) => c,
            Exit::Signal(s) => 128 + s,
        }
    }

    fn summary(self) -> (State, Option<String>) {
        match self {
            Exit::Code(0) => (State::Done, None),
            Exit::Code(c) => (State::Error, Some(format!("exit {c}"))),
            Exit::Signal(s) => (State::Error, Some(format!("signal {}", signal_name(s)))),
        }
    }
}

fn signal_name(sig: i32) -> String {
    let name = match sig {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        _ => return sig.to_string(),
    };
    name.to_string()
}

#[derive(Debug, Clone)]
struct Tracked {
    state: State,
    app: String,
    touched: u64,
    last: Pub,
}

/// The records of one wrapped program, keyed by report id ("" = root).
/// Turns scanner events into store operations with the 7501 rules a
/// terminal applies: full replacement per id, `clear` of a subtree or of
/// everything, `app` inherited from the nearest ancestor, working/blocked
/// (and idle) dropped at a shell prompt and at process exit, done/error kept.
pub(crate) struct Collector {
    prefix: String,
    default_app: String,
    records: BTreeMap<String, Tracked>,
    tick: u64,
}

impl Collector {
    pub fn new(app: &str, pid: u32) -> Collector {
        Collector { prefix: format!("wrap/{app}/{pid}"), default_app: app.to_string(), records: BTreeMap::new(), tick: 0 }
    }

    fn full_id(&self, key: &str) -> String {
        if key.is_empty() {
            self.prefix.clone()
        } else {
            format!("{}/{key}", self.prefix)
        }
    }

    fn inherited_app(&self, key: &str) -> String {
        let mut k = key;
        while !k.is_empty() {
            k = k.rfind('/').map_or("", |i| &k[..i]);
            if let Some(t) = self.records.get(k) {
                return t.app.clone();
            }
        }
        self.default_app.clone()
    }

    fn set(&mut self, key: String, p: Pub, ops: &mut Vec<Op>) {
        self.tick += 1;
        self.records.insert(key.clone(), Tracked { state: p.state, app: p.app.clone(), touched: self.tick, last: p.clone() });
        if self.records.len() > MAX_RECORDS {
            let oldest = self.records.iter().filter(|(k, _)| **k != key).min_by_key(|(_, t)| t.touched).map(|(k, _)| k.clone());
            if let Some(old) = oldest {
                self.records.remove(&old);
                ops.push(Op::Remove { id: self.full_id(&old), subtree: false });
            }
        }
        ops.push(Op::Publish(p));
    }

    /// The record shown from the start, so programs that never speak 7501
    /// still appear: root `working`, no message.
    pub fn start(&mut self) -> Vec<Op> {
        let mut ops = Vec::new();
        let p = Pub {
            id: self.prefix.clone(),
            app: self.default_app.clone(),
            state: State::Working,
            kind: None,
            progress: None,
            title: None,
            msg: None,
        };
        self.set(String::new(), p, &mut ops);
        ops
    }

    /// Apply one scanner event. Returns why a report was not recorded (its
    /// id does not fit under the wrap prefix within the 7501 id limits).
    pub fn on_event(&mut self, ev: &Event, ops: &mut Vec<Op>) -> Result<(), String> {
        match ev {
            Event::Report(r) if r.state == State::Clear => {
                match &r.id {
                    Some(id) => {
                        let sub = format!("{id}/");
                        self.records.retain(|k, _| k != id && !k.starts_with(&sub));
                        ops.push(Op::Remove { id: self.full_id(id), subtree: true });
                    }
                    None => {
                        self.records.clear();
                        ops.push(Op::Remove { id: self.prefix.clone(), subtree: true });
                    }
                }
                Ok(())
            }
            Event::Report(r) => self.report(r, ops),
            Event::Prompt => {
                self.drop_active(ops);
                Ok(())
            }
            Event::Rejected(_) => Ok(()),
        }
    }

    fn report(&mut self, r: &Report, ops: &mut Vec<Op>) -> Result<(), String> {
        let key = r.id.clone().unwrap_or_default();
        let id = self.full_id(&key);
        osc::check_id(&id).map_err(|e| format!("report id {key:?} does not fit under {}: {e}", self.prefix))?;
        let app = r.app.clone().unwrap_or_else(|| self.inherited_app(&key));
        let p = Pub {
            id,
            app,
            state: r.state,
            kind: r.kind,
            progress: r.progress,
            title: r.title.clone(),
            msg: r.msg.clone(),
        };
        self.set(key, p, ops);
        Ok(())
    }

    /// Drop idle/working/blocked records (exactly those ids; done/error
    /// descendants stay).
    fn drop_active(&mut self, ops: &mut Vec<Op>) {
        let gone: Vec<String> = self
            .records
            .iter()
            .filter(|(_, t)| matches!(t.state, State::Idle | State::Working | State::Blocked))
            .map(|(k, _)| k.clone())
            .collect();
        for k in gone {
            self.records.remove(&k);
            ops.push(Op::Remove { id: self.full_id(&k), subtree: false });
        }
    }

    /// The command was continued after a suspension, during which the
    /// user's shell prompted and so dropped its idle/working/blocked
    /// records (the 7501 prompt rule): publish them again.
    pub fn resumed(&self) -> Vec<Op> {
        self.records
            .values()
            .filter(|t| matches!(t.state, State::Idle | State::Working | State::Blocked))
            .map(|t| Op::Publish(t.last.clone()))
            .collect()
    }

    /// The child exited: unless the program's root record already says
    /// done/error, record the outcome there (done for exit 0, error with
    /// "exit N" / "signal NAME" otherwise); then drop idle/working/blocked.
    pub fn on_exit(&mut self, exit: Exit) -> Vec<Op> {
        let mut ops = Vec::new();
        let root_final = matches!(self.records.get("").map(|t| t.state), Some(State::Done | State::Error));
        if !root_final {
            let (state, msg) = exit.summary();
            let app = self.records.get("").map_or_else(|| self.default_app.clone(), |t| t.app.clone());
            let p = Pub { id: self.prefix.clone(), app, state, kind: None, progress: None, title: None, msg };
            self.set(String::new(), p, &mut ops);
        }
        self.drop_active(&mut ops);
        ops
    }
}

/// Drop publishes that a later operation in the same batch makes moot (a
/// later publish of the same id, or a removal covering it): each publish
/// replaces its record whole, so only the last one matters. Removals stay.
pub(crate) fn coalesce(ops: Vec<Op>) -> Vec<Op> {
    let covers = |r: &str, subtree: bool, id: &str| {
        id == r || (subtree && id.len() > r.len() && id.starts_with(r) && id.as_bytes()[r.len()] == b'/')
    };
    let mut keep = vec![true; ops.len()];
    {
        let mut later_pub: HashSet<&str> = HashSet::new();
        let mut later_rm: Vec<(&str, bool)> = Vec::new();
        for (i, op) in ops.iter().enumerate().rev() {
            match op {
                Op::Publish(p) => {
                    if later_pub.contains(p.id.as_str()) || later_rm.iter().any(|(r, s)| covers(r, *s, &p.id)) {
                        keep[i] = false;
                    }
                    later_pub.insert(&p.id);
                }
                Op::Remove { id, subtree } => later_rm.push((id, *subtree)),
            }
        }
    }
    ops.into_iter().zip(keep).filter_map(|(op, k)| k.then_some(op)).collect()
}

// ---------------------------------------------------------------------------
// Store glue. Everything that knows about the store lives here.

/// Destination of record operations (the store; a stand-in in tests).
pub(crate) trait Sink: Send + 'static {
    /// Slow one-time preparation, run on the store thread before the first
    /// batch is taken (so a short program's start and exit coalesce).
    fn init(&mut self) {}
    fn apply(&mut self, op: &Op);
}

/// Who publishes and where the records show.
#[derive(Debug, Clone)]
struct Origin {
    scope: Option<String>,
    scope_id: Option<String>,
    cwd: PathBuf,
    session: Option<String>,
}

/// The agent store, with the scope resolved on first use (off the main
/// thread: the default scope costs one `git rev-parse`).
struct StoreSink {
    store: Store,
    origin: Origin,
    scope: Option<Scope>,
}

impl StoreSink {
    /// Fails when the store is unusable (then wrap execs CMD directly).
    fn open(origin: Origin) -> Result<StoreSink, String> {
        use std::os::unix::fs::DirBuilderExt;
        let store = Store::open().map_err(|e| format!("agent store: {e}"))?;
        let root = store.root().to_path_buf();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .map_err(|e| format!("agent store {}: {e}", root.display()))?;
        let c = std::ffi::CString::new(root.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: access(2) on a NUL-terminated path.
        if unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
            return Err(format!("agent store {}: {}", root.display(), last_err()));
        }
        Ok(StoreSink { store, origin, scope: None })
    }

    fn remove(&self, scope: Scope, id: &str, subtree: bool) -> Result<(), String> {
        if subtree {
            let filter = Filter { scopes: vec![scope], id: Some(id.to_string()), ..Filter::default() };
            self.store.clear(&filter).map(|_| ()).map_err(|e| e.to_string())
        } else {
            self.store.remove(&scope, id).map(|_| ()).map_err(|e| e.to_string())
        }
    }
}

/// `--scope`/`--scope-id`, else the current directory's worktree, else global.
fn resolve_scope(o: &Origin) -> Scope {
    let wt = || tildy::providers::git::worktree_identity(&o.cwd);
    match o.scope.as_deref() {
        Some("session") => match o.scope_id.clone().or_else(|| o.session.clone()) {
            Some(id) => return Scope::session(id),
            None => diag("--scope session without --scope-id or TILDY_SESSION_ID; using the default scope"),
        },
        Some("worktree") => match o.scope_id.clone().or_else(wt) {
            Some(id) => return Scope::worktree(id),
            None => diag("--scope worktree outside a git worktree; using global"),
        },
        Some("global") => return Scope::global(),
        Some(other) => diag(&format!("unknown --scope {other:?}; using the default scope")),
        None => {}
    }
    wt().map(Scope::worktree).unwrap_or_else(Scope::global)
}

fn agent_state(s: State) -> agent::State {
    match s {
        State::Idle => agent::State::Idle,
        State::Working => agent::State::Working,
        State::Done => agent::State::Done,
        State::Blocked => agent::State::Blocked,
        // `clear` never reaches a publish (it is an Op::Remove).
        State::Error | State::Clear => agent::State::Error,
    }
}

fn agent_kind(k: Kind) -> agent::Kind {
    match k {
        Kind::Permission => agent::Kind::Permission,
        Kind::Question => agent::Kind::Question,
        Kind::Auth => agent::Kind::Auth,
    }
}

impl Sink for StoreSink {
    fn init(&mut self) {
        self.scope = Some(resolve_scope(&self.origin));
    }

    fn apply(&mut self, op: &Op) {
        let scope = self.scope.get_or_insert_with(|| resolve_scope(&self.origin)).clone();
        let res = match op {
            Op::Publish(p) => {
                let mut r = Record::new(agent_state(p.state), scope);
                r.id = p.id.clone();
                r.app = Some(p.app.clone());
                r.kind = p.kind.map(agent_kind);
                r.progress = p.progress;
                r.title = p.title.clone();
                r.msg = p.msg.clone();
                r.origin_session = self.origin.session.clone();
                r.source = Source::Osc;
                self.store.publish(r).map(|_| ()).map_err(|e| e.to_string())
            }
            Op::Remove { id, subtree } => self.remove(scope, id, *subtree),
        };
        if let Err(e) = res {
            diag(&format!("store: {e}"));
        }
    }
}

/// Runs a [`Sink`] on its own thread so store writes never hold up the
/// byte forwarding. Batches are coalesced; `finish` waits a bounded time.
struct Recorder {
    tx: Option<mpsc::Sender<Vec<Op>>>,
    done: Option<mpsc::Receiver<()>>,
}

impl Recorder {
    fn start<S: Sink>(mut sink: S) -> Recorder {
        let (tx, rx) = mpsc::channel::<Vec<Op>>();
        let (done_tx, done_rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("tildy-wrap-store".into()).spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sink.init();
                while let Ok(mut batch) = rx.recv() {
                    while let Ok(more) = rx.try_recv() {
                        batch.extend(more);
                    }
                    for op in coalesce(batch) {
                        sink.apply(&op);
                    }
                }
            }));
            if r.is_err() {
                diag("store thread panicked; recording stopped");
            }
            let _ = done_tx.send(());
        });
        match spawned {
            Ok(_) => Recorder { tx: Some(tx), done: Some(done_rx) },
            Err(e) => {
                diag(&format!("cannot start the store thread: {e}"));
                Recorder { tx: None, done: None }
            }
        }
    }

    fn send(&self, ops: Vec<Op>) {
        if let (false, Some(tx)) = (ops.is_empty(), &self.tx) {
            let _ = tx.send(ops);
        }
    }

    fn finish(mut self, wait: Duration) {
        drop(self.tx.take());
        if let Some(done) = self.done.take() {
            if done.recv_timeout(wait).is_err() {
                diag("store writes did not finish in time");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The pty proxy.

/// Write end of the signal self-pipe (-1 when no proxy is running).
static SIG_PIPE_W: AtomicI32 = AtomicI32::new(-1);
const SIGNALS: [libc::c_int; 7] =
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGWINCH, libc::SIGCHLD, libc::SIGCONT];
const FORWARDED: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];
/// Input bytes queued for the command before wrap stops reading stdin.
const MAX_PENDING_INPUT: usize = 1 << 16;
/// After the command exits: keep draining the pty until it is quiet this long
/// (or reports EOF), but never longer than `DRAIN_MAX`.
const DRAIN_QUIET: Duration = Duration::from_millis(100);
const DRAIN_MAX: Duration = Duration::from_secs(2);
const STORE_FINISH: Duration = Duration::from_secs(2);

#[cfg(target_vendor = "apple")]
fn errno_ptr() -> *mut libc::c_int {
    // SAFETY: returns the calling thread's errno location.
    unsafe { libc::__error() }
}
#[cfg(not(target_vendor = "apple"))]
fn errno_ptr() -> *mut libc::c_int {
    // SAFETY: returns the calling thread's errno location.
    unsafe { libc::__errno_location() }
}

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = SIG_PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: write(2) and errno access are async-signal-safe; errno is
        // preserved for the interrupted code.
        unsafe {
            let saved = *errno_ptr();
            let b = sig as u8;
            libc::write(fd, (&b as *const u8).cast(), 1);
            *errno_ptr() = saved;
        }
    }
}

fn last_err() -> io::Error {
    io::Error::last_os_error()
}

fn set_flags(fd: RawFd, cloexec: bool, nonblock: bool) {
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        if cloexec {
            let f = libc::fcntl(fd, libc::F_GETFD);
            libc::fcntl(fd, libc::F_SETFD, f | libc::FD_CLOEXEC);
        }
        if nonblock {
            let f = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, f | libc::O_NONBLOCK);
        }
    }
}

/// stdin, stdout and stderr are all the same terminal (the pty can only
/// stand in for one terminal; anything redirected means CMD must see the
/// real descriptors).
fn one_terminal() -> Result<(), String> {
    let mut dev = None;
    for fd in 0..3 {
        // SAFETY: isatty/fstat only inspect the descriptor.
        if unsafe { libc::isatty(fd) } != 1 {
            return Err(format!("fd {fd} is not a terminal"));
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            return Err(format!("fstat fd {fd}: {}", last_err()));
        }
        match dev {
            None => dev = Some(st.st_rdev),
            Some(d) if d != st.st_rdev => return Err("stdin, stdout and stderr are different terminals".into()),
            _ => {}
        }
    }
    Ok(())
}

fn winsize(fd: RawFd) -> Option<libc::winsize> {
    // SAFETY: TIOCGWINSZ fills the struct we own.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    (unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0).then_some(ws)
}

fn copy_winsize(master: RawFd) {
    if let Some(ws) = winsize(0) {
        // SAFETY: TIOCSWINSZ reads the struct; the kernel signals the pty's
        // foreground process group.
        unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &ws) };
    }
}

/// Blocking write of all bytes (EINTR retried, EAGAIN waited out).
fn write_all(fd: RawFd, mut b: &[u8]) -> io::Result<()> {
    while !b.is_empty() {
        // SAFETY: writes from a valid slice.
        let n = unsafe { libc::write(fd, b.as_ptr().cast(), b.len()) };
        if n > 0 {
            b = &b[n as usize..];
            continue;
        }
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        let e = last_err();
        match e.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::EAGAIN) => {
                let mut p = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
                // SAFETY: polls one descriptor.
                unsafe { libc::poll(&mut p, 1, 1000) };
            }
            _ => return Err(e),
        }
    }
    Ok(())
}

/// Raw mode on a terminal for as long as the guard lives; dropping it (also
/// during unwinding) restores the attributes it was given.
struct RawGuard {
    fd: RawFd,
    orig: libc::termios,
}

impl RawGuard {
    fn enter(fd: RawFd, orig: libc::termios) -> io::Result<RawGuard> {
        let mut raw = orig;
        // SAFETY: cfmakeraw edits our copy; tcsetattr applies it to `fd`.
        unsafe { libc::cfmakeraw(&mut raw) };
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(last_err());
        }
        Ok(RawGuard { fd, orig })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read from the same descriptor.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig) };
    }
}

/// Our signal handlers; dropping restores the previous dispositions.
struct Handlers {
    old: [(libc::c_int, libc::sigaction); SIGNALS.len()],
}

impl Handlers {
    fn install(pipe_w: RawFd) -> Handlers {
        SIG_PIPE_W.store(pipe_w, Ordering::SeqCst);
        // SAFETY: zeroed sigaction is a valid "default" value to overwrite.
        let mut old = [(0, unsafe { std::mem::zeroed::<libc::sigaction>() }); SIGNALS.len()];
        for (i, &sig) in SIGNALS.iter().enumerate() {
            // SAFETY: installs a handler that only calls async-signal-safe code.
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                sa.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut sa.sa_mask);
                old[i].0 = sig;
                libc::sigaction(sig, &sa, &mut old[i].1);
            }
        }
        Handlers { old }
    }
}

impl Drop for Handlers {
    fn drop(&mut self) {
        for (sig, sa) in &self.old {
            // SAFETY: restores the disposition saved by `install`.
            unsafe { libc::sigaction(*sig, sa, std::ptr::null_mut()) };
        }
        SIG_PIPE_W.store(-1, Ordering::SeqCst);
    }
}

/// Monitor -> wrap messages: a tag byte and a native-endian i32.
const MSG_PID: u8 = b'P'; // the command started; value: its pid
const MSG_FAIL: u8 = b'F'; // the command could not be started; value: errno
const MSG_STOP: u8 = b'S'; // the command stopped; value: the signal
const MSG_EXIT: u8 = b'X'; // the command ended; value: its raw wait status
/// Wrap -> monitor: continue the stopped command.
const MSG_CONT: u8 = b'C';

fn send_msg(fd: RawFd, tag: u8, v: i32) -> bool {
    let mut b = [tag, 0, 0, 0, 0];
    b[1..].copy_from_slice(&v.to_ne_bytes());
    write_all(fd, &b).is_ok()
}

fn exit_from_wait(st: libc::c_int) -> Exit {
    if libc::WIFEXITED(st) {
        Exit::Code(libc::WEXITSTATUS(st))
    } else if libc::WIFSIGNALED(st) {
        Exit::Signal(libc::WTERMSIG(st))
    } else {
        Exit::Code(1)
    }
}

fn ignore_signal(sig: libc::c_int) -> libc::sigaction {
    // SAFETY: plain sigaction calls on structs we own.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_IGN;
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(sig, &sa, &mut old);
        old
    }
}

/// The monitor: a forked copy of wrap (forked while wrap is still single
/// threaded) that becomes the session leader of the pty and runs the command
/// in its own process group in the foreground. That group is not orphaned
/// (its parent, the monitor, is in the same session), so the kernel's job
/// control works for it: Ctrl-Z stops it, and programs that suspend
/// themselves (vim, Claude Code) can. The monitor reports start, stops and
/// the exit status to wrap and continues the command on request. Never
/// returns.
fn monitor(slave: RawFd, status_w: RawFd, control_r: RawFd, saved: &[(libc::c_int, libc::sigaction)], cmd: &[OsString]) -> ! {
    let fail = |e: Option<i32>| -> ! {
        send_msg(status_w, MSG_FAIL, e.unwrap_or(libc::EINVAL));
        // SAFETY: leave the forked copy without running destructors or atexit.
        unsafe { libc::_exit(1) }
    };
    // SAFETY: syscalls in a single-threaded child of wrap.
    unsafe {
        for (sig, sa) in saved {
            libc::sigaction(*sig, sa, std::ptr::null_mut());
        }
        if libc::setsid() < 0 || libc::ioctl(slave, libc::TIOCSCTTY as _, 0) < 0 {
            fail(last_err().raw_os_error());
        }
    }
    // tcsetpgrp from a background group raises SIGTTOU unless ignored.
    let ttou = ignore_signal(libc::SIGTTOU);
    let mut ep = [-1; 2];
    // SAFETY: pipe writes two descriptors into the array.
    if unsafe { libc::pipe(ep.as_mut_ptr()) } != 0 {
        fail(last_err().raw_os_error());
    }
    set_flags(ep[0], true, false);
    set_flags(ep[1], true, false);
    // SAFETY: fork in a single-threaded process.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail(last_err().raw_os_error());
    }
    if pid == 0 {
        // The command: own process group, foreground of the pty, the pty on
        // fds 0-2, the original SIGTTOU disposition, then exec.
        // SAFETY: syscalls in the single-threaded command child.
        unsafe {
            libc::setpgid(0, 0);
            libc::tcsetpgrp(slave, libc::getpid());
            libc::sigaction(libc::SIGTTOU, &ttou, std::ptr::null_mut());
            for fd in 0..3 {
                libc::dup2(slave, fd);
            }
            if slave > 2 {
                libc::close(slave);
            }
        }
        let err = Command::new(&cmd[0]).args(&cmd[1..]).exec();
        let e = err.raw_os_error().unwrap_or(libc::ENOEXEC).to_ne_bytes();
        // SAFETY: report errno to the monitor and leave.
        unsafe {
            libc::write(ep[1], e.as_ptr().cast(), e.len());
            libc::_exit(127);
        }
    }
    // SAFETY: plain syscalls in the monitor.
    unsafe {
        libc::setpgid(pid, pid);
        libc::tcsetpgrp(slave, pid);
        libc::close(ep[1]);
    }
    let mut e = [0u8; 4];
    // SAFETY: reads at most 4 bytes; EOF means the exec succeeded (CLOEXEC).
    let n = loop {
        let n = unsafe { libc::read(ep[0], e.as_mut_ptr().cast(), e.len()) };
        if n >= 0 || last_err().raw_os_error() != Some(libc::EINTR) {
            break n;
        }
    };
    if n == 4 {
        let mut st = 0;
        // SAFETY: reap the failed command.
        unsafe { libc::waitpid(pid, &mut st, 0) };
        fail(Some(i32::from_ne_bytes(e)));
    }
    send_msg(status_w, MSG_PID, pid);
    loop {
        let mut st = 0;
        // SAFETY: waits for our own child.
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WUNTRACED) };
        if r < 0 {
            if last_err().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            // SAFETY: nothing left to watch.
            unsafe { libc::_exit(0) };
        }
        if !libc::WIFSTOPPED(st) {
            send_msg(status_w, MSG_EXIT, st);
            // SAFETY: done; the pty goes away with us.
            unsafe { libc::_exit(0) };
        }
        let wrap_alive = send_msg(status_w, MSG_STOP, libc::WSTOPSIG(st));
        // Wait for wrap's "continue" (after the user's `fg`), watching for
        // the command dying while stopped.
        loop {
            let mut p = libc::pollfd { fd: control_r, events: libc::POLLIN, revents: 0 };
            // SAFETY: polls one descriptor.
            let ready = wrap_alive && unsafe { libc::poll(&mut p, 1, 200) } > 0;
            let mut b = [0u8; 1];
            // SAFETY: reads one byte when readable.
            let got = if ready { unsafe { libc::read(control_r, b.as_mut_ptr().cast(), 1) } } else { -1 };
            if !wrap_alive || got == 0 {
                // wrap is gone: like a closed terminal.
                // SAFETY: signals the command's group.
                unsafe {
                    libc::kill(-pid, libc::SIGHUP);
                    libc::kill(-pid, libc::SIGCONT);
                }
                break;
            }
            if got == 1 && b[0] == MSG_CONT {
                // SAFETY: give the command the terminal back and continue it.
                unsafe {
                    libc::tcsetpgrp(slave, pid);
                    libc::kill(-pid, libc::SIGCONT);
                }
                break;
            }
            let mut st2 = 0;
            // SAFETY: non-blocking check for the command's end.
            if unsafe { libc::waitpid(pid, &mut st2, libc::WNOHANG) } == pid && !libc::WIFSTOPPED(st2) {
                send_msg(status_w, MSG_EXIT, st2);
                // SAFETY: done.
                unsafe { libc::_exit(0) };
            }
        }
    }
}

struct Proxy {
    /// The monitor process (our child).
    monitor: libc::pid_t,
    /// The command (the monitor's child, leader of its process group).
    pid: libc::pid_t,
    master: OwnedFd,
    status_r: OwnedFd,
    control_w: OwnedFd,
    msg: Vec<u8>,
    sig_r: OwnedFd,
    _sig_w: OwnedFd,
    handlers: Handlers,
    orig: libc::termios,
    raw: Option<RawGuard>,
    collector: Collector,
    recorder: Recorder,
    debug: Option<File>,
}

fn pipe_pair() -> Result<(OwnedFd, OwnedFd), String> {
    let mut p = [-1; 2];
    // SAFETY: pipe writes two descriptors into the array.
    if unsafe { libc::pipe(p.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", last_err()));
    }
    set_flags(p[0], true, false);
    set_flags(p[1], true, false);
    // SAFETY: just created, owned here.
    Ok(unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) })
}

/// Blocking read of one monitor message (with a deadline).
fn read_msg(fd: RawFd, timeout: Duration) -> Option<(u8, i32)> {
    let mut b = [0u8; 5];
    let mut got = 0;
    let end = Instant::now() + timeout;
    while got < b.len() {
        let left = end.saturating_duration_since(Instant::now());
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: polls one descriptor, then reads into our buffer.
        if left.is_zero() || unsafe { libc::poll(&mut p, 1, left.as_millis().max(1) as libc::c_int) } == 0 {
            return None;
        }
        let n = unsafe { libc::read(fd, b[got..].as_mut_ptr().cast(), b.len() - got) };
        if n == 0 {
            return None;
        }
        if n > 0 {
            got += n as usize;
        } else if last_err().raw_os_error() != Some(libc::EINTR) {
            return None;
        }
    }
    Some((b[0], i32::from_ne_bytes([b[1], b[2], b[3], b[4]])))
}

impl Proxy {
    /// Everything that can fail before the command runs; an error means
    /// "exec CMD directly instead".
    fn setup(a: &WrapArgs, app: &str) -> Result<Proxy, String> {
        one_terminal()?;
        let origin = Origin {
            scope: a.scope.clone(),
            scope_id: a.scope_id.clone(),
            cwd: std::env::current_dir().map_err(|e| format!("current directory: {e}"))?,
            session: std::env::var("TILDY_SESSION_ID").ok().filter(|s| !s.is_empty()),
        };
        let sink = StoreSink::open(origin)?;
        // SAFETY: tcgetattr fills the struct we own.
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut orig) } != 0 {
            return Err(format!("tcgetattr: {}", last_err()));
        }
        let debug = a.debug_reports.as_ref().and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .map_err(|e| diag(&format!("{}: {e}", p.display())))
                .ok()
        });

        // The pty starts with the real terminal's modes and size.
        let (mut m, mut s) = (-1, -1);
        let mut term = orig;
        let mut ws = winsize(0).unwrap_or(libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 });
        // SAFETY: openpty writes two descriptors into the ints. Its termios and
        // winsize parameters are mutable pointers on macOS, const on Linux.
        if unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::addr_of_mut!(term), std::ptr::addr_of_mut!(ws)) } != 0 {
            return Err(format!("openpty: {}", last_err()));
        }
        // SAFETY: both descriptors were just opened and are owned here.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        set_flags(master.as_raw_fd(), true, true);
        set_flags(slave.as_raw_fd(), true, false);

        let (sig_r, sig_w) = pipe_pair()?;
        set_flags(sig_r.as_raw_fd(), true, true);
        set_flags(sig_w.as_raw_fd(), true, true);
        let (status_r, status_w) = pipe_pair()?;
        let (control_r, control_w) = pipe_pair()?;
        let handlers = Handlers::install(sig_w.as_raw_fd());
        // A resize between reading the size and installing the handler.
        copy_winsize(master.as_raw_fd());

        // SAFETY: wrap is single-threaded here (the store thread starts
        // later), so the forked monitor may run ordinary code.
        let monitor_pid = unsafe { libc::fork() };
        if monitor_pid < 0 {
            return Err(format!("fork: {}", last_err()));
        }
        if monitor_pid == 0 {
            // Only the monitor's ends stay open here, so it sees EOF on the
            // control pipe if wrap dies.
            drop((master, sig_r, status_r, control_w));
            let (slave, status_w, control_r) = (slave.as_raw_fd(), status_w.as_raw_fd(), control_r.as_raw_fd());
            let saved = handlers.old;
            // A panic must never unwind back into wrap's code in this copy.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| monitor(slave, status_w, control_r, &saved, &a.cmd)));
            // SAFETY: leave the forked copy.
            unsafe { libc::_exit(1) };
        }
        // Only the monitor and the command hold the pty's slave side now:
        // the master reports EOF once both are gone.
        drop((slave, status_w, control_r));
        let pid = match read_msg(status_r.as_raw_fd(), Duration::from_secs(10)) {
            Some((MSG_PID, pid)) => pid,
            other => {
                let mut st = 0;
                // SAFETY: reaps (or ends and reaps) the monitor we forked.
                unsafe {
                    if other.is_none() {
                        libc::kill(monitor_pid, libc::SIGKILL);
                    }
                    libc::waitpid(monitor_pid, &mut st, 0);
                }
                return Err(match other {
                    Some((MSG_FAIL, e)) => format!("start: {}", io::Error::from_raw_os_error(e)),
                    _ => "the pty monitor did not start".into(),
                });
            }
        };
        CHILD_STARTED.store(true, Ordering::SeqCst);
        let collector = Collector::new(app, pid as u32);
        let recorder = Recorder::start(sink);
        Ok(Proxy {
            monitor: monitor_pid,
            pid,
            master,
            status_r,
            control_w,
            msg: Vec::new(),
            sig_r,
            _sig_w: sig_w,
            handlers,
            orig,
            raw: None,
            collector,
            recorder,
            debug,
        })
    }

    fn enter_raw(&mut self) {
        // Re-applying: the old guard must not restore after the new apply.
        if let Some(old) = self.raw.take() {
            std::mem::forget(old);
        }
        match RawGuard::enter(0, self.orig) {
            Ok(g) => self.raw = Some(g),
            Err(e) => diag(&format!("raw mode: {e}")),
        }
    }

    fn run(mut self) -> i32 {
        // While proxying, a panic message must not land on the terminal.
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|info| diag(&format!("panic: {info}"))));
        let start = self.collector.start();
        self.debug_ops(&start);
        self.recorder.send(start);

        self.enter_raw();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.pump()));
        self.raw = None; // terminal back to normal before anything else
        let exit = r.unwrap_or_else(|_| self.abandon());
        if self.monitor > 0 {
            let mut st = 0;
            // SAFETY: the monitor exits right after reporting the command's
            // exit; reap it (without hanging if it is somehow still there).
            unsafe {
                let mut tries = 0;
                while libc::waitpid(self.monitor, &mut st, libc::WNOHANG) == 0 {
                    tries += 1;
                    if tries > 100 {
                        libc::kill(self.monitor, libc::SIGKILL);
                        libc::waitpid(self.monitor, &mut st, 0);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
        let ops = self.collector.on_exit(exit);
        self.debug_ops(&ops);
        self.debug_line(serde_json::json!({"exit": exit.code()}));
        self.recorder.send(ops);
        let Proxy { recorder, handlers, .. } = self;
        recorder.finish(STORE_FINISH);
        drop(handlers);
        std::panic::set_hook(prev_hook);
        exit.code()
    }

    /// The proxy loop failed: the command cannot go on without its terminal.
    /// Hang it up like a closed terminal window would and report that.
    fn abandon(&mut self) -> Exit {
        // SAFETY: signals our own command's process group.
        unsafe {
            libc::kill(-self.pid, libc::SIGHUP);
            libc::kill(-self.pid, libc::SIGCONT);
        }
        let end = Instant::now() + Duration::from_secs(1);
        while Instant::now() < end {
            if let Some((MSG_EXIT, st)) = read_msg(self.status_r.as_raw_fd(), Duration::from_millis(100)) {
                return exit_from_wait(st);
            }
        }
        Exit::Signal(libc::SIGHUP)
    }

    fn debug_line(&mut self, v: serde_json::Value) {
        if let Some(f) = &mut self.debug {
            let _ = writeln!(f, "{v}");
        }
    }

    fn debug_ops(&mut self, ops: &[Op]) {
        for op in ops {
            self.debug_line(op.to_json());
        }
    }

    /// The command stopped (Ctrl-Z reached it through the pty, or it
    /// suspended itself): give the terminal back and stop ourselves, so the
    /// user's shell sees the job suspended exactly as without wrap; on `fg`
    /// take the terminal again and have the monitor continue the command.
    /// In an orphaned process group SIGTSTP is discarded, and the command
    /// simply continues.
    fn suspend(&mut self) {
        self.raw = None;
        // SAFETY: default-action SIGTSTP to ourselves (we never catch it).
        unsafe { libc::raise(libc::SIGTSTP) };
        self.enter_raw();
        copy_winsize(self.master.as_raw_fd());
        let _ = write_all(self.control_w.as_raw_fd(), &[MSG_CONT]);
        let ops = self.collector.resumed();
        self.debug_ops(&ops);
        self.recorder.send(ops);
    }

    /// Read monitor messages; returns the command's exit once known.
    fn monitor_messages(&mut self) -> Option<Exit> {
        let mut b = [0u8; 64];
        // SAFETY: reads into our buffer (the poll said it is readable).
        let n = unsafe { libc::read(self.status_r.as_raw_fd(), b.as_mut_ptr().cast(), b.len()) };
        if n == 0 || (n < 0 && !matches!(last_err().raw_os_error(), Some(libc::EINTR | libc::EAGAIN))) {
            // The monitor is gone without a word: use its own status.
            let mut st = 0;
            // SAFETY: reaps the monitor we forked.
            let r = unsafe { libc::waitpid(self.monitor, &mut st, 0) };
            self.monitor = -1;
            return Some(if r > 0 { exit_from_wait(st) } else { Exit::Code(1) });
        }
        self.msg.extend_from_slice(&b[..n.max(0) as usize]);
        let mut exit = None;
        while self.msg.len() >= 5 {
            let m: Vec<u8> = self.msg.drain(..5).collect();
            let v = i32::from_ne_bytes([m[1], m[2], m[3], m[4]]);
            match m[0] {
                MSG_STOP => self.suspend(),
                MSG_EXIT => exit = Some(exit_from_wait(v)),
                _ => {}
            }
        }
        exit
    }

    /// Forward bytes until the command has exited and its output is drained.
    fn pump(&mut self) -> Exit {
        let master = self.master.as_raw_fd();
        let mut scanner = Scanner::new();
        let mut scanning = true;
        let mut buf = vec![0u8; 1 << 16];
        let mut to_child: Vec<u8> = Vec::new();
        let (mut stdin_open, mut master_open, mut stdout_ok) = (true, true, true);
        let mut exit: Option<Exit> = None;
        let mut drain_until = Instant::now();
        loop {
            if let Some(e) = exit {
                if !master_open || Instant::now() >= drain_until {
                    return e;
                }
            }
            let running = exit.is_none();
            let mut fds = [
                libc::pollfd { fd: self.sig_r.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: -1, events: 0, revents: 0 },
                libc::pollfd { fd: -1, events: 0, revents: 0 },
                libc::pollfd { fd: -1, events: 0, revents: 0 },
            ];
            if master_open {
                fds[1].fd = master;
                fds[1].events = libc::POLLIN | if running && !to_child.is_empty() { libc::POLLOUT } else { 0 };
            }
            if running && stdin_open && to_child.len() < MAX_PENDING_INPUT {
                fds[2].fd = 0;
                fds[2].events = libc::POLLIN;
            }
            if running {
                fds[3].fd = self.status_r.as_raw_fd();
                fds[3].events = libc::POLLIN;
            }
            let timeout = if running { -1 } else { DRAIN_QUIET.as_millis() as libc::c_int };
            // SAFETY: polls descriptors we own (or ignore with fd -1).
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if n < 0 {
                if last_err().raw_os_error() != Some(libc::EINTR) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                continue;
            }
            if n == 0 && !running {
                master_open = false; // quiet since the command exited
                continue;
            }
            if fds[0].revents & libc::POLLIN != 0 {
                self.signals(master);
            }
            let mr = fds[1].revents;
            if mr & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                // SAFETY: reads into our buffer.
                let r = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                if r > 0 {
                    let data = &buf[..r as usize];
                    // Forward first; recording never delays the terminal.
                    if stdout_ok {
                        if let Err(e) = write_all(1, data) {
                            diag(&format!("stdout: {e}; discarding further output"));
                            stdout_ok = false;
                        }
                    }
                    if scanning {
                        scanning = self.scan(&mut scanner, data);
                    }
                } else if r == 0 || !matches!(last_err().raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) || mr & libc::POLLIN == 0 {
                    master_open = false; // EOF/EIO: no process has the pty open any more
                }
            } else if mr & libc::POLLNVAL != 0 {
                master_open = false;
            }
            if mr & libc::POLLOUT != 0 {
                feed_child(master, &mut to_child);
            }
            let ir = fds[2].revents;
            if ir & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                // SAFETY: reads into our buffer.
                let r = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
                if r > 0 {
                    to_child.extend_from_slice(&buf[..r as usize]);
                    feed_child(master, &mut to_child);
                } else if r == 0 || !matches!(last_err().raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
                    stdin_open = false; // terminal hung up
                }
            } else if ir & libc::POLLNVAL != 0 {
                stdin_open = false;
            }
            if fds[3].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                if let Some(e) = self.monitor_messages() {
                    exit = Some(e);
                    drain_until = Instant::now() + DRAIN_MAX;
                }
            }
        }
    }

    /// Scan forwarded output; false when the scanner or bookkeeping panicked
    /// (recording stops, forwarding goes on).
    fn scan(&mut self, scanner: &mut Scanner, data: &[u8]) -> bool {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut events = Vec::new();
            scanner.feed(data, |e| events.push(e));
            let mut ops = Vec::new();
            for ev in &events {
                let res = self.collector.on_event(ev, &mut ops);
                if self.debug.is_some() {
                    let v = match ev {
                        Event::Report(r) => serde_json::json!({"event": "report", "report": r}),
                        Event::Rejected(e) => serde_json::json!({"event": "rejected", "reject": e}),
                        Event::Prompt => serde_json::json!({"event": "prompt"}),
                    };
                    self.debug_line(v);
                }
                if let Err(why) = res {
                    diag(&why);
                    self.debug_line(serde_json::json!({"dropped": why}));
                }
            }
            self.debug_ops(&ops);
            self.recorder.send(ops);
        }));
        r.is_ok()
    }

    fn signals(&mut self, master: RawFd) {
        let mut sigs = [0u8; 64];
        // SAFETY: drains the non-blocking self-pipe into our buffer.
        let n = unsafe { libc::read(self.sig_r.as_raw_fd(), sigs.as_mut_ptr().cast(), sigs.len()) };
        for &s in sigs.iter().take(n.max(0) as usize) {
            let sig = s as libc::c_int;
            if sig == libc::SIGWINCH {
                copy_winsize(master);
            } else if sig == libc::SIGCONT {
                // Continued after a stop we did not ask for (kill -STOP, or
                // the shell's own job control): the shell may have reset the
                // terminal meanwhile, and the size may have changed.
                if self.raw.is_some() {
                    self.enter_raw();
                }
                copy_winsize(master);
            } else if FORWARDED.contains(&sig) {
                // SAFETY: signals the command's process group (it leads its
                // group, so the pgid is its pid), or the command alone.
                unsafe {
                    if libc::kill(-self.pid, sig) != 0 {
                        libc::kill(self.pid, sig);
                    }
                }
            }
        }
    }
}

/// Write queued input to the pty without blocking.
fn feed_child(master: RawFd, q: &mut Vec<u8>) {
    while !q.is_empty() {
        // SAFETY: writes from our queue to the non-blocking master.
        let n = unsafe { libc::write(master, q.as_ptr().cast(), q.len()) };
        if n > 0 {
            q.drain(..n as usize);
            continue;
        }
        match last_err().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return,
            _ => {
                q.clear(); // the child side is gone
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(body: &str) -> Event {
        Event::Report(osc::parse_body(body.as_bytes()).unwrap())
    }

    fn feed(c: &mut Collector, events: &[Event]) -> Vec<Op> {
        let mut ops = Vec::new();
        for e in events {
            c.on_event(e, &mut ops).unwrap();
        }
        ops
    }

    fn publish(id: &str, app: &str, state: State) -> Op {
        Op::Publish(Pub { id: id.into(), app: app.into(), state, kind: None, progress: None, title: None, msg: None })
    }

    fn rm(id: &str, subtree: bool) -> Op {
        Op::Remove { id: id.into(), subtree }
    }

    #[test]
    fn app_names() {
        assert_eq!(app_name(None, OsStr::new("/opt/homebrew/bin/claude")), "claude");
        assert_eq!(app_name(Some("codex"), OsStr::new("/x/node")), "codex");
        assert_eq!(app_name(Some(""), OsStr::new("cargo")), "cargo");
        assert_eq!(app_name(None, OsStr::new("./my tool (dev)")), "my_tool__dev_");
        assert_eq!(app_name(None, OsStr::new("工具")), "__");
        assert_eq!(app_name(None, OsStr::new("/")), "app");
        assert_eq!(app_name(None, OsStr::new(&"x".repeat(40))).len(), 32);
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(app_name(None, OsStr::from_bytes(b"/bin/t\xffool")), "t_ool");
    }

    #[test]
    fn start_report_exit_lifecycle() {
        let mut c = Collector::new("claude", 42);
        assert_eq!(c.start(), vec![publish("wrap/claude/42", "claude", State::Working)]);
        let ops = feed(&mut c, &[rep("state=blocked:kind=permission:msg=WWVzPw==")]);
        assert_eq!(
            ops,
            vec![Op::Publish(Pub {
                id: "wrap/claude/42".into(),
                app: "claude".into(),
                state: State::Blocked,
                kind: Some(Kind::Permission),
                progress: None,
                title: None,
                msg: Some("Yes?".into()),
            })]
        );
        // Root still blocked at exit 0: synthesized done.
        assert_eq!(c.on_exit(Exit::Code(0)), vec![publish("wrap/claude/42", "claude", State::Done)]);
    }

    #[test]
    fn exit_keeps_the_programs_own_done_and_drops_active_children() {
        let mut c = Collector::new("cargo", 7);
        c.start();
        feed(&mut c, &[rep("state=done:app=cargo:msg=b2s="), rep("state=working:id=a"), rep("state=error:id=b"), rep("state=idle:id=b/c")]);
        assert_eq!(c.on_exit(Exit::Code(3)), vec![rm("wrap/cargo/7/a", false), rm("wrap/cargo/7/b/c", false)]);
    }

    #[test]
    fn exit_codes_and_signals_become_error_messages() {
        for (exit, msg, code) in [
            (Exit::Code(2), "exit 2", 2),
            (Exit::Signal(libc::SIGTERM), "signal SIGTERM", 143),
            (Exit::Signal(libc::SIGKILL), "signal SIGKILL", 137),
            (Exit::Signal(99), "signal 99", 227),
        ] {
            let mut c = Collector::new("x", 1);
            c.start();
            let ops = c.on_exit(exit);
            assert_eq!(exit.code(), code);
            assert_eq!(
                ops,
                vec![Op::Publish(Pub { msg: Some(msg.into()), ..match publish("wrap/x/1", "x", State::Error) {
                    Op::Publish(p) => p,
                    _ => unreachable!(),
                } })]
            );
        }
    }

    #[test]
    fn resume_republishes_active_records_only() {
        let mut c = Collector::new("vim", 3);
        c.start();
        feed(&mut c, &[rep("state=done:id=a"), rep("state=blocked:id=b:kind=question")]);
        let ops = c.resumed();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0], publish("wrap/vim/3", "vim", State::Working));
        match &ops[1] {
            Op::Publish(p) => assert_eq!((p.id.as_str(), p.state, p.kind), ("wrap/vim/3/b", State::Blocked, Some(Kind::Question))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn app_is_inherited_from_the_nearest_ancestor() {
        let mut c = Collector::new("deploy", 9);
        let ops = feed(&mut c, &[rep("state=working:app=terraform"), rep("state=working:id=us/east"), rep("state=working:id=us:app=aws"), rep("state=done:id=us/west")]);
        assert_eq!(
            ops,
            vec![
                publish("wrap/deploy/9", "terraform", State::Working),
                publish("wrap/deploy/9/us/east", "terraform", State::Working),
                publish("wrap/deploy/9/us", "aws", State::Working),
                publish("wrap/deploy/9/us/west", "aws", State::Done),
            ]
        );
    }

    #[test]
    fn clear_with_and_without_id() {
        let mut c = Collector::new("a", 1);
        feed(&mut c, &[rep("state=working:id=x"), rep("state=working:id=x/y"), rep("state=working:id=xy")]);
        assert_eq!(feed(&mut c, &[rep("state=clear:id=x")]), vec![rm("wrap/a/1/x", true)]);
        assert_eq!(c.records.keys().collect::<Vec<_>>(), ["xy"]);
        assert_eq!(feed(&mut c, &[rep("state=clear")]), vec![rm("wrap/a/1", true)]);
        assert!(c.records.is_empty());
        // Cleared root: exit synthesizes the outcome again.
        assert_eq!(c.on_exit(Exit::Code(0)), vec![publish("wrap/a/1", "a", State::Done)]);
    }

    #[test]
    fn shell_prompt_drops_active_records() {
        let mut c = Collector::new("zsh", 5);
        c.start();
        feed(&mut c, &[rep("state=done:id=build"), rep("state=blocked:id=ask")]);
        assert_eq!(feed(&mut c, &[Event::Prompt]), vec![rm("wrap/zsh/5", false), rm("wrap/zsh/5/ask", false)]);
        assert_eq!(c.records.keys().collect::<Vec<_>>(), ["build"]);
    }

    #[test]
    fn ids_that_do_not_fit_under_the_prefix_are_dropped() {
        let mut c = Collector::new("a", 1);
        let mut ops = Vec::new();
        // wrap/a/1 + 5 levels = 8: fits; 6 levels = 9: does not.
        assert!(c.on_event(&rep("state=idle:id=1/2/3/4/5"), &mut ops).is_ok());
        assert!(c.on_event(&rep("state=idle:id=1/2/3/4/5/6"), &mut ops).is_err());
        // A 124-byte report id is valid on its own but not behind "wrap/a/1/".
        let long = format!("{}/{}/{}/{}", "a".repeat(32), "b".repeat(32), "c".repeat(32), "d".repeat(25));
        assert!(c.on_event(&rep(&format!("state=idle:id={long}")), &mut ops).is_err());
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn record_cap_evicts_the_least_recently_updated() {
        let mut c = Collector::new("a", 1);
        let events: Vec<Event> = (0..MAX_RECORDS).map(|i| rep(&format!("state=working:id=r{i}"))).collect();
        feed(&mut c, &events);
        feed(&mut c, &[rep("state=working:id=r0")]); // r0 is fresh again; r1 is now the oldest
        let ops = feed(&mut c, &[rep("state=working:id=new")]);
        assert_eq!(ops, vec![rm("wrap/a/1/r1", false), publish("wrap/a/1/new", "a", State::Working)]);
        assert_eq!(c.records.len(), MAX_RECORDS);
    }

    #[test]
    fn coalescing_keeps_only_what_matters() {
        let ops = vec![
            publish("w/a", "a", State::Working),
            publish("w/a/x", "a", State::Working),
            publish("w/a", "a", State::Blocked),
            publish("w/ab", "a", State::Working),
            rm("w/a/x", false),
            publish("w/a/y", "a", State::Working),
            rm("w/a", true),
            publish("w/a", "a", State::Done),
        ];
        assert_eq!(
            coalesce(ops),
            vec![publish("w/ab", "a", State::Working), rm("w/a/x", false), rm("w/a", true), publish("w/a", "a", State::Done)]
        );
    }

    #[test]
    fn recorder_applies_everything_in_order() {
        use std::sync::{Arc, Mutex};
        struct Mem(Arc<Mutex<Vec<Op>>>);
        impl Sink for Mem {
            fn apply(&mut self, op: &Op) {
                self.0.lock().unwrap().push(op.clone());
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let r = Recorder::start(Mem(seen.clone()));
        r.send(vec![publish("w/a", "a", State::Working)]);
        r.send(vec![]);
        r.send(vec![rm("w/b", false)]);
        r.finish(Duration::from_secs(5));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.last(), Some(&rm("w/b", false)));
        assert!(seen.iter().all(|op| op.id().starts_with("w/")));
    }

    #[test]
    fn raw_guard_restores_on_drop_and_on_unwind() {
        let (mut m, mut s) = (-1, -1);
        // SAFETY: openpty writes two descriptors into the ints.
        let r = unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(r, 0);
        // SAFETY: just opened, owned here.
        let (_m, s) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        let get = || {
            // SAFETY: tcgetattr fills the struct we own.
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(s.as_raw_fd(), &mut t) }, 0);
            // PENDIN is the kernel's own "retype pending input" flag, set
            // when canonical mode comes back; it is not a mode we restore.
            (t.c_iflag, t.c_oflag, t.c_lflag & !libc::PENDIN)
        };
        // SAFETY: as above.
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(s.as_raw_fd(), &mut orig) }, 0);
        let cooked = get();
        assert_ne!(cooked.2 & libc::ICANON, 0);
        let g = RawGuard::enter(s.as_raw_fd(), orig).unwrap();
        assert_eq!(get().2 & (libc::ICANON | libc::ECHO | libc::ISIG), 0);
        assert_eq!(get().1 & libc::OPOST, 0, "no output processing on the outer terminal");
        drop(g);
        assert_eq!(get(), cooked);
        let fd = s.as_raw_fd();
        let r = std::panic::catch_unwind(move || {
            let _g = RawGuard::enter(fd, orig).unwrap();
            panic!("boom");
        });
        assert!(r.is_err());
        assert_eq!(get(), cooked, "restored while unwinding");
    }

    #[test]
    fn op_json_shape() {
        let p = Op::Publish(Pub {
            id: "wrap/a/1".into(),
            app: "a".into(),
            state: State::Blocked,
            kind: Some(Kind::Auth),
            progress: Some(3),
            title: None,
            msg: Some("m".into()),
        });
        assert_eq!(
            p.to_json(),
            serde_json::json!({"op": "publish", "id": "wrap/a/1", "app": "a", "state": "blocked", "kind": "auth", "progress": 3, "msg": "m"})
        );
        assert_eq!(rm("x", true).to_json(), serde_json::json!({"op": "remove", "id": "x", "subtree": true}));
    }
}
