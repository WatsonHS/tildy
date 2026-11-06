//! Persistent backend core (`tildy worker`), independent of the real config,
//! renderer and providers so tests can drive it with fakes over pipes.
//!
//! Threads: one stdin reader (lines -> channel), one short-lived thread per
//! provider fetch (result -> channel). All state lives on the calling thread,
//! which owns the scheduler and is the only writer of stdout, so "is this still
//! the latest request?" and "send" happen in one step without locks.
//!
//! Per REQ: reload config if its file changed (and rebuild the provider set
//! when the renderer reports a new config epoch) -> submit to the scheduler ->
//! render and send `partial` (or `fresh` if everything is already final), then
//! start the fetch threads (on a cold start the session-global and git fetches
//! start first and the worker waits up to `fast_wait` for session-global
//! providers; config module fetches, which may run version commands, always
//! start after the partial) -> later, each result that changes what the latest
//! request shows is rendered and sent, `fresh` once every slot is final. A
//! snapshot is queued after every RES and written by a background thread
//! (latest wins). Exits on stdin EOF, `BYE`, parent death, or a failed write
//! (EPIPE), removing its snapshot.
//!
//! Instant prompt: a REQ whose `env` carries `TILDY_INSTANT_KEY` (removed
//! before scheduling, see [`super::instant`]) makes every `fresh` RES also
//! write the instant-prompt cache file for that key, on a background thread.
//!
//! Agent status (docs/agent-status.md): each REQ first applies the OSC 7501
//! prompt rule to the records this session published
//! (`Store::drop_on_prompt`). With an `agent_status` provider, the worker
//! then checks the store directories of the request's scopes (session,
//! worktree, global) every `agent_poll`; when one changed or a record expired
//! and the module's data differs, the current request is scheduled again and
//! its settled result is sent as another RES for the same generation.

use super::instant::{self, InstantWriter};
use super::watch::{self, RefreshGate, WorktreeWatch, WatchInbox, WatchManager, WatchNotice};
use super::{assemble, read_line_bounded, InputLine, PromptRenderer, ProviderSlot};
use crate::agent::{Filter as AgentFilter, Store as AgentStore};
use crate::diagnostics::{
    self, duration_ms, now_ms, Counters, ProviderCycleTiming, ResultInfo, SlotInfo, Snapshot,
    TimingCycle, TimingRecorder, WatchDiagnostics, WorkerInfo,
    SNAPSHOT_KIND, SNAPSHOT_SCHEMA_VERSION, TIMING_HISTORY_LIMIT,
};
use crate::model::Dependency;
use crate::model::{
    ChangeData, Freshness, PromptData, RenderStatus, RenderedPrompt, BINARY_VERSION, PROTOCOL_VERSION};
use crate::protocol::{self, ShellFrame, MAX_REQUEST_BYTES};
use crate::providers::{slot_target, Provider, ProviderValue, SlotTarget, GIT_KEY};
use crate::scheduler::{FetchId, FetchOrder, Scheduler, SchedulerConfig, SlotView};
use std::collections::VecDeque;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Where the worker writes its session snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotTarget {
    /// `session::snapshot_path(session_id)` once the first REQ names the session.
    Auto,
    Path(PathBuf),
    Disabled,
}

#[derive(Debug, Clone)]
pub struct WorkerOptions {
    pub parent_pid: Option<u32>,
    pub parent_poll: Duration,
    pub snapshot: SnapshotTarget,
    /// Snapshots in the sessions directory older than this are removed at start
    /// (only with `SnapshotTarget::Auto`).
    pub prune_snapshots_after: Option<Duration>,
    pub home: Option<PathBuf>,
    /// Cold-start wait for session-global providers before the first partial.
    pub fast_wait: Duration,
    pub epoch: u64,
    pub pid: u32,
    pub scheduler: SchedulerConfig,
    /// Instant-prompt cache directory; `None` disables writing it.
    pub instant_dir: Option<PathBuf>,
    /// Agent status store (prompt rule on each REQ, push for agent_status
    /// modules); `None` disables both.
    pub agent_store: Option<AgentStore>,
    /// How often the agent status directories are checked.
    pub agent_poll: Duration,
    /// Disable OS watching while retaining idle fallback (useful on unsupported filesystems).
    pub watch_enabled: bool,
    pub idle_refresh: Option<Duration>,
    #[cfg(test)]
    watch_setup_delay: Duration,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        WorkerOptions {
            parent_pid: None,
            parent_poll: super::PARENT_POLL_INTERVAL,
            snapshot: SnapshotTarget::Auto,
            prune_snapshots_after: Some(Duration::from_secs(7 * 24 * 3600)),
            home: std::env::var_os("HOME").map(PathBuf::from),
            fast_wait: Duration::from_millis(5),
            epoch: super::new_worker_epoch(),
            pid: std::process::id(),
            scheduler: SchedulerConfig::default(),
            instant_dir: instant::cache_dir(),
            agent_store: AgentStore::open().ok(),
            agent_poll: Duration::from_secs(1),
            watch_enabled: std::env::var("TILDY_WATCH").ok().as_deref() != Some("0"),
            idle_refresh: watch::idle_refresh(
                std::env::var("TILDY_IDLE_REFRESH_S").ok().as_deref(),
            ),
            #[cfg(test)]
            watch_setup_delay: Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    Eof,
    Bye,
    ParentGone,
    /// Writing a frame failed (EPIPE: the shell closed its end).
    OutputClosed,
}

enum Event {
    Watch(WatchNotice),
    Line(InputLine),
    Eof,
    Fetched { id: FetchId, key: &'static str, value: Option<Box<ProviderValue>>, elapsed: Duration },
}

/// Run the worker until EOF, BYE, parent exit or a write error. `output` must
/// be the shell's pipe; every frame is flushed as soon as it is written.
///
/// `make_providers` runs after HELLO is flushed, so the handshake does not wait
/// for provider construction. The renderer may replace that set later
/// ([`PromptRenderer::providers`]). Provider keys must be unique; a provider
/// whose key repeats an earlier one is dropped with a warning on stderr.
pub fn run_worker<R, W, F>(
    input: R,
    mut output: W,
    make_providers: F,
    renderer: Box<dyn PromptRenderer>,
    opts: WorkerOptions,
) -> ExitReason
where
    R: Read + Send + 'static,
    W: Write,
    F: FnOnce() -> Vec<Arc<dyn Provider>>,
{
    let hello = protocol::encode_hello(opts.epoch, opts.pid);
    if output.write_all(hello.as_bytes()).and_then(|_| output.flush()).is_err() {
        return ExitReason::OutputClosed;
    }
    let (tx, rx) = mpsc::channel();
    spawn_reader(input, tx.clone());
    let providers = unique_providers(make_providers());
    let specs: Vec<_> = providers.iter().map(|p| p.spec().clone()).collect();
    let now = Instant::now();
    let mut w = Worker {
        out: output,
        providers,
        renderer,
        sched: Scheduler::new(specs, opts.scheduler.clone()),
        config_epoch: 0,
        tx,
        rx,
        deferred: VecDeque::new(),
        next_parent_check: opts.parent_pid.map(|_| now + opts.parent_poll),
        started_at_ms: now_ms(),
        opts,
        timings: TimingRecorder::default(),
        counters: Counters::default(),
        received_at: None,
        last_sent: None,
        last_result: None,
        session_id: None,
        snapshot_path: None,
        snapshots: None,
        instant_key: None,
        presentation_env: instant::PresentationEnv::default(),
        instant_identity: None,
        preserve_instant: false,
        instant: None,
        agent: AgentWatch::default(),
        external: ExternalWatch::default(),
        observation: ObservationState::default(),
        timing_history: VecDeque::new(),
        cycle_fetches: HashMap::new(),
        fetch_elapsed: HashMap::new(),
        cycle_serial: 0,
        cycle_ids: VecDeque::new(),
        module_reports: Vec::new(),
    };
    let reason = w.event_loop().unwrap_err();
    w.cleanup();
    reason
}

fn spawn_reader<R: Read + Send + 'static>(input: R, tx: Sender<Event>) {
    let fallback = tx.clone();
    let spawned = thread::Builder::new().name("tildy-stdin".into()).spawn(move || {
        let mut r = BufReader::with_capacity(16 * 1024, input);
        loop {
            match read_line_bounded(&mut r, MAX_REQUEST_BYTES) {
                Ok(Some(line)) => {
                    if tx.send(Event::Line(line)).is_err() {
                        return;
                    }
                }
                Ok(None) | Err(_) => {
                    let _ = tx.send(Event::Eof);
                    return;
                }
            }
        }
    });
    if spawned.is_err() {
        // Without a reader no request can arrive; behave as EOF.
        let _ = fallback.send(Event::Eof);
    }
}

/// Keep the first provider for each key (the scheduler cache is keyed by it).
fn unique_providers(providers: Vec<Arc<dyn Provider>>) -> Vec<Arc<dyn Provider>> {
    let mut seen = std::collections::HashSet::new();
    providers
        .into_iter()
        .filter(|p| {
            let key = p.spec().key;
            let first = seen.insert(key);
            if !first {
                eprintln!("tildy worker: duplicate provider key {key:?} ignored");
            }
            first
        })
        .collect()
}

/// Config module providers (keyed by module id).
fn is_module(key: &str) -> bool {
    matches!(slot_target(key), SlotTarget::Module(_))
}

/// On a cold start only the session-global providers (which the first partial
/// waits for) and git start before the partial. Everything else that observes
/// the cwd, in particular config modules that may run version commands, starts
/// after it.
fn starts_after_partial(spec: &crate::model::ProviderSpec) -> bool {
    spec.key != GIT_KEY && spec.deps.contains(&Dependency::Cwd)
}

/// Sends the fetch result, or a failure if the provider panicked (Drop runs
/// during unwinding), so the scheduler never waits for a dead thread.
struct FetchGuard {
    tx: Sender<Event>,
    id: FetchId,
    key: &'static str,
    start: Instant,
    sent: bool,
}

impl FetchGuard {
    fn finish(mut self, value: ProviderValue) {
        self.sent = true;
        let _ = self.tx.send(Event::Fetched {
            id: self.id,
            key: self.key,
            value: Some(Box::new(value)),
            elapsed: self.start.elapsed(),
        });
    }
}

impl Drop for FetchGuard {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self.tx.send(Event::Fetched { id: self.id, key: self.key, value: None, elapsed: self.start.elapsed() });
        }
    }
}

struct Worker<W: Write> {
    out: W,
    providers: Vec<Arc<dyn Provider>>,
    renderer: Box<dyn PromptRenderer>,
    sched: Scheduler<ProviderValue>,
    /// Last `PromptRenderer::config_epoch` the provider set was built for.
    config_epoch: u64,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    deferred: VecDeque<Event>,
    opts: WorkerOptions,
    next_parent_check: Option<Instant>,
    started_at_ms: u64,
    timings: TimingRecorder,
    counters: Counters,
    received_at: Option<(Instant, u64)>,
    /// Last RES sent: (request seq, prompt incl. status), to skip duplicates.
    last_sent: Option<(u64, RenderedPrompt)>,
    last_result: Option<ResultInfo>,
    session_id: Option<String>,
    snapshot_path: Option<PathBuf>,
    snapshots: Option<diagnostics::SnapshotWriter>,
    /// Instant-prompt cache key of the current request (from its `env`).
    instant_key: Option<String>,
    presentation_env: instant::PresentationEnv,
    instant_identity: Option<instant::Identity>,
    preserve_instant: bool,
    instant: Option<InstantWriter>,
    agent: AgentWatch,
    external: ExternalWatch,
    observation: ObservationState,
    timing_history: VecDeque<TimingCycle>,
    cycle_fetches: HashMap<FetchId, u64>,
    fetch_elapsed: HashMap<FetchId, u64>,
    cycle_serial: u64,
    cycle_ids: VecDeque<u64>,
    module_reports: Vec<crate::model::ModuleReport>,
}

#[derive(Default)]
struct ExternalWatch {
    inbox: Option<WatchInbox>,
    manager: Option<WatchManager>,
    identity: Option<Vec<PathBuf>>,
    external_files: Vec<PathBuf>,
    external_parents: BTreeMap<PathBuf, PathBuf>,
    token: u64,
    gate: RefreshGate,
    paths: Vec<PathBuf>,
    rescan: bool,
    fallback: Option<Instant>,
    diagnostics: WatchDiagnostics,
    refreshing: bool,
}

#[derive(Clone, Default)]
struct Observation {
    root: Option<PathBuf>,
    branch: Option<String>,
    files: Option<u32>,
    environments: BTreeMap<String, String>,
}
impl Observation {
    fn from_data(data: &PromptData) -> Self {
        let git = data.git.value.as_ref().filter(|g| g.in_repo);
        let environments = data
            .modules
            .iter()
            .flat_map(|(id, slot)| {
                slot.value.iter().flat_map(move |v| {
                    ["env", "context"].into_iter().filter_map(move |k| {
                        v.fields.get(k).map(|v| (format!("{id}/{k}"), v.clone()))
                    })
                })
            })
            .collect();
        Self {
            root: git.and_then(|g| g.worktree_root.clone()),
            branch: git.and_then(|g| {
                if g.detached {
                    g.head_short.clone()
                } else {
                    g.branch.clone()
                }
            }),
            files: git.map(|g| g.unstaged.saturating_add(g.untracked)),
            environments,
        }
    }
    fn changes(&self, next: &Self) -> Option<ChangeData> {
        let same_repo = self.root == next.root;
        let branch_changed = same_repo
            && self.branch.is_some()
            && next.branch.is_some()
            && self.branch != next.branch;
        let files_added = if same_repo {
            self.files
                .zip(next.files)
                .map_or(0, |(a, b)| b.saturating_sub(a))
        } else {
            0
        };
        let environments: Vec<String> = self
            .environments
            .keys()
            .chain(next.environments.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .filter(|k| self.environments.get(*k) != next.environments.get(*k))
            .map(|k| {
                format!(
                    "{} → {}",
                    self.environments.get(k).map(String::as_str).unwrap_or("∅"),
                    next.environments.get(k).map(String::as_str).unwrap_or("∅")
                )
            })
            .collect();
        (branch_changed || files_added > 0 || !environments.is_empty()).then(|| ChangeData {
            branch_from: branch_changed.then(|| self.branch.clone()).flatten(),
            branch_to: branch_changed.then(|| next.branch.clone()).flatten(),
            files_added,
            environments,
        })
    }
}

#[derive(Default)]
struct ObservationState {
    next_seq: u64,
    observed: Option<Observation>,
    anchor: Option<Observation>,
    external: bool,
    sent: VecDeque<(u64, u64, Observation, bool)>,
    last_ack: u64,
    submission_id: Option<String>,
}
impl ObservationState {
    fn new_request(&mut self) {
        self.observed = None;
        self.anchor = None;
        self.external = false;
        self.sent.clear();
    }
    fn begin_external(&mut self) {
        if self.anchor.is_none() {
            self.anchor = self.observed.clone();
        }
        self.external = true;
    }
    fn acknowledge(&mut self, gen: u64, seq: u64) {
        if seq <= self.last_ack {
            return;
        }
        if let Some((_, _, observation, eligible)) = self.sent.iter().find(|(g, s, _, _)| *g == gen && *s == seq)
        {
            // Cached partials after submission may predate the command. Only
            // an accepted, settled result can establish the idle baseline.
            if *eligible { self.observed = Some(observation.clone()); }
            self.last_ack = seq;
            self.sent.retain(|(_, s, _, _)| *s > seq);
        }
    }
}

/// Agent status push state for the current request.
#[derive(Default)]
struct AgentWatch {
    /// Next check; `None` while no agent_status provider is active.
    next_poll: Option<Instant>,
    /// Watched scope directories and their mtimes (`None`: missing).
    stamp: Vec<(PathBuf, Option<SystemTime>)>,
    /// Earliest expiry (unix ms) among the live watched records.
    next_expiry_ms: Option<u64>,
    /// The current request was scheduled again for an agent change: send
    /// only its settled result, and only if it differs from the last RES.
    refreshing: bool,
    /// Last background `Store::expire` (housekeeping, at most hourly).
    last_expire: Option<Instant>,
}

/// How often a worker removes expired agent records and empty scope dirs.
const AGENT_EXPIRE_EVERY: Duration = Duration::from_secs(3600);

type Exit = Result<(), ExitReason>;

impl<W: Write> Worker<W> {
    /// Runs until an exit condition; always returns `Err(reason)`.
    fn event_loop(&mut self) -> Exit {
        loop {
            self.check_parent()?;
            let ev = match self.deferred.pop_front() {
                Some(ev) => ev,
                None => match self.wait_event() {
                    Some(ev) => ev,
                    None => {
                        self.on_deadlines()?;
                        continue;
                    }
                },
            };
            self.handle(ev)?;
            self.on_deadlines()?;
        }
    }

    /// Block until an event, the next scheduler deadline or the next parent
    /// check, whichever is first. `None` means a timer fired.
    fn wait_event(&mut self) -> Option<Event> {
        let wake = [self.sched.next_deadline(), self.next_parent_check, self.agent.next_poll,
            self.external.gate.deadline(),
            self.external.fallback,].into_iter().flatten().min();
        match wake {
            Some(t) => match self.rx.recv_timeout(t.saturating_duration_since(Instant::now())) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => Some(Event::Eof),
            },
            None => Some(self.rx.recv().unwrap_or(Event::Eof)),
        }
    }

    fn check_parent(&mut self) -> Exit {
        let (Some(pid), Some(next)) = (self.opts.parent_pid, self.next_parent_check) else {
            return Ok(());
        };
        let now = Instant::now();
        if now < next {
            return Ok(());
        }
        if !super::pid_alive(pid) {
            return Err(ExitReason::ParentGone);
        }
        self.next_parent_check = Some(now + self.opts.parent_poll);
        Ok(())
    }

    fn on_deadlines(&mut self) -> Exit {
        self.agent_poll()?;
        self.external_poll()?;
        let now = Instant::now();
        if self.sched.next_deadline().is_none_or(|d| d > now) {
            return Ok(());
        }
        let update = self.sched.expire(now);
        for (id, key, elapsed) in &update.timed_out { self.record_fetch_duration(*id, key, *elapsed, true); }
        self.note_cycle_timeouts();
        self.spawn(update.orders);
        if update.changed_current {
            self.send_current()?;
        }
        Ok(())
    }

    fn handle(&mut self, ev: Event) -> Exit {
        match ev {
            Event::Watch(notice) => {
                self.watch_notice(notice);
                Ok(())
            }
            Event::Eof => Err(ExitReason::Eof),
            Event::Fetched { id, key, value, elapsed } => {
                if self.on_fetched(id, key, value, elapsed) {
                    self.send_current()?;
                }
                Ok(())
            }
            Event::Line(InputLine::TooLong) => {
                self.protocol_error(&protocol::ProtocolError::TooLong);
                Ok(())
            }
            Event::Line(InputLine::Line(bytes)) => match protocol::parse_line(&bytes) {
                Ok(None) => Ok(()),
                Ok(Some(ShellFrame::Bye)) => Err(ExitReason::Bye),
                Ok(Some(ShellFrame::Ack { gen, epoch, seq })) => {
                    if epoch == self.opts.epoch
                        && self.sched.current_request().is_some_and(|r| r.gen == gen)
                    {
                        self.observation.acknowledge(gen, seq);
                    }
                    Ok(())
                }
                Ok(Some(ShellFrame::Request(req))) => {
                    self.counters.requests += 1;
                    while let Ok(ev) = self.rx.try_recv() {
                        self.deferred.push_back(ev);
                    }
                    if self.newer_request_queued() {
                        self.counters.dropped_requests += 1;
                        return Ok(());
                    }
                    self.on_request(req)
                }
                Err(e) => {
                    self.protocol_error(&e);
                    Ok(())
                }
            },
        }
    }

    fn protocol_error(&mut self, e: &protocol::ProtocolError) {
        self.counters.protocol_errors += 1;
        eprintln!("tildy worker: protocol error: {e}");
    }

    /// A valid REQ is already waiting behind this one. Scans from the back: in a
    /// burst the newest event is usually a REQ, so this is O(1) per request.
    fn newer_request_queued(&self) -> bool {
        self.deferred.iter().rev().any(|ev| {
            matches!(ev, Event::Line(InputLine::Line(b))
                if b.starts_with(b"REQ\t") && matches!(protocol::parse_line(b), Ok(Some(ShellFrame::Request(_)))))
        })
    }

    fn on_request(&mut self, mut req: crate::model::PromptRequest) -> Exit {
        self.agent.refreshing = false;
        self.external.refreshing = false;
        let submission = req
            .env
            .split('\u{1f}')
            .find_map(|p| p.strip_prefix("TILDY_SUBMISSION_ID="))
            .map(str::to_string);
        let submitted = submission.is_none() || submission != self.observation.submission_id;
        req.env = req
            .env
            .split('\u{1f}')
            .filter(|p| !p.starts_with("TILDY_SUBMISSION_ID="))
            .collect::<Vec<_>>()
            .join("\u{1f}");
        if submitted {
            self.observation.new_request();
        } else {
            self.observation.sent.clear();
        }
        self.observation.submission_id = submission;
        let instant_shown = instant::take_shown(&mut req);
        self.presentation_env = instant::PresentationEnv::take(&mut req);
        let instant_key = instant::take_key(&mut req);
        self.instant_key = self.presentation_env.instant_allowed().then_some(instant_key).flatten();
        let now = Instant::now();
        self.received_at = Some((now, now_ms()));
        self.note_session(&req.session_id);
        self.renderer.refresh_for(&req.cwd);
        self.instant_identity = self.renderer.revision().map(|revision| instant::Identity::new(&req, revision, &self.presentation_env));
        self.preserve_instant = instant_shown && self.last_sent.is_none()
            && self.instant_identity.as_ref().zip(self.instant_key.as_ref()).zip(self.opts.instant_dir.as_ref())
                .is_some_and(|((identity, key), dir)| identity.matches_file(&dir.join(instant::file_name(key))));
        self.sync_providers();
        let before = self.provider_stats();
        let orders = self.sched.submit(req, now);
        self.begin_cycle(&before);
        self.sync_watch();
        if self.sched.awaiting_uncached_global() {
            // Cold start: session and git fetches first, module fetches only
            // after the partial.
            let (late, early): (Vec<_>, Vec<_>) =
                orders.into_iter().partition(|o| starts_after_partial(self.sched.spec(o.provider)));
            self.spawn(early);
            self.fast_wait(now + self.opts.fast_wait);
            let sent = self.send_current();
            self.agent_on_request();
            self.spawn(late);
            return sent;
        }
        // Warm path: the partial goes out before the fetch threads are spawned
        // (spawning costs tens of microseconds; the fetches start right after).
        let sent = self.send_current();
        self.agent_on_request();
        self.spawn(orders);
        sent
    }

    /// Adopt the renderer's provider set when its config epoch changed. Kept
    /// keys keep their scheduler state. Cache entries of module providers are
    /// dropped unless the very same provider instance is handed over again
    /// (a module id may now name a different command); they refill quickly
    /// from the process-wide version cache.
    fn sync_providers(&mut self) {
        let epoch = self.renderer.config_epoch();
        if epoch == self.config_epoch {
            return;
        }
        self.config_epoch = epoch;
        let Some(providers) = self.renderer.providers() else { return };
        let providers = unique_providers(providers);
        let same: std::collections::HashSet<&'static str> = providers
            .iter()
            .filter(|p| self.providers.iter().any(|o| std::ptr::addr_eq(Arc::as_ptr(o), Arc::as_ptr(p))))
            .map(|p| p.spec().key)
            .collect();
        let specs = providers.iter().map(|p| p.spec().clone()).collect();
        self.sched.reconfigure(specs, |key| !is_module(key) || same.contains(key));
        self.providers = providers;
    }

    /// Cold start: give cheap session-global providers a few milliseconds so the
    /// first partial already has host/user/SSH. Other events are deferred.
    fn fast_wait(&mut self, deadline: Instant) {
        while self.sched.awaiting_uncached_global() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.rx.recv_timeout(left) {
                Ok(Event::Fetched { id, key, value, elapsed }) => {
                    self.on_fetched(id, key, value, elapsed);
                }
                Ok(other) => self.deferred.push_back(other),
                Err(_) => return,
            }
        }
    }

    /// Apply a fetch result; returns true if the latest request's view changed.
    fn on_fetched(&mut self, id: FetchId, key: &'static str, value: Option<Box<ProviderValue>>, elapsed: Duration) -> bool {
        let now = Instant::now();
        let timed_out = value.as_ref().is_some_and(|v| match v.as_ref() {
            ProviderValue::Git(s) => s.freshness == Freshness::Timeout,
            ProviderValue::Module(_, s) => s.freshness == Freshness::Timeout,
            _ => false,
        });
        self.record_fetch_duration(id, key, elapsed, timed_out);
        if let Some(origin) = self.cycle_fetches.get(&id).copied() {
            self.note_cycle_completed(origin, key);
        }
        self.cycle_fetches.remove(&id);
        self.fetch_elapsed.remove(&id);
        let update = match value.map(|b| *b) {
            Some(value) => {
                let ok = match &value {
                    ProviderValue::Git(s) => !matches!(s.freshness, Freshness::Error | Freshness::Timeout),
                    ProviderValue::Module(_, s) => !matches!(s.freshness, Freshness::Error | Freshness::Timeout),
                    _ => true,
                };
                self.timings.record(key, elapsed, ok);
                let worktree = match &value {
                    ProviderValue::Git(s) => s.value.as_ref().filter(|g| g.in_repo).and_then(|g| g.worktree_id.clone()),
                    _ => None,
                };
                self.sched.complete(id, value, worktree, now)
            }
            None => {
                self.timings.record(key, elapsed, false);
                self.sched.fail(id, format!("provider {key} panicked"), now)
            }
        };
        if update.changed_current { self.note_cycle_completed(self.cycle_serial, key); }
        self.spawn(update.orders);
        self.sync_watch();
        self.write_snapshot(&self.sched.metadata_views(now));
        update.changed_current
    }

    fn spawn(&mut self, orders: Vec<FetchOrder>) {
        for order in orders {
            let provider = Arc::clone(&self.providers[order.provider]);
            let tx = self.tx.clone();
            let id = order.id;
            self.cycle_fetches.insert(id, self.cycle_serial);
            if let Some(t) = self.timing_history.back_mut().and_then(|c| {
                c.providers
                    .iter_mut()
                    .find(|p| p.provider == provider.spec().key)
            }) {
                t.executed = true;
            }
            let key = self.sched.spec(order.provider).key;
            let spawned = thread::Builder::new().name(format!("tildy-fetch-{key}")).spawn(move || {
                let guard = FetchGuard { tx, id, key, start: Instant::now(), sent: false };
                let value = provider.fetch(&order.request);
                guard.finish(value);
            });
            if let Err(e) = spawned {
                let update = self.sched.fail(id, format!("cannot start fetch thread: {e}"), Instant::now());
                // A failed spawn cannot recurse forever: `fail` only restarts a
                // queued fetch, and the cap bounds how many exist.
                self.spawn(update.orders);
            }
        }
    }

    /// Render the latest request and send it unless identical to the last RES.
    fn send_current(&mut self) -> Exit {
        let (Some(req), Some(seq)) = (self.sched.current_request().cloned(), self.sched.current_seq()) else {
            return Ok(());
        };
        let now = Instant::now();
        let views = self.sched.views(now);
        let slots: Vec<ProviderSlot> = views.iter().map(provider_slot).collect();
        let mut data = assemble(&req, self.opts.home.clone(), &slots);
        self.presentation_env.restore(&mut data.request);
        let observation = Observation::from_data(&data);
        data.changes = self
            .observation
            .anchor
            .as_ref()
            .and_then(|a| a.changes(&observation));
        let mut prompt = self.renderer.render(&data);
        let reports = self.renderer.reports();
        prompt.status = if self.sched.is_settled() { RenderStatus::Fresh } else { RenderStatus::Partial };
        if self.preserve_instant && prompt.status == RenderStatus::Partial {
            // The shell still displays valid cached text. Unknown provider
            // slots are not evidence that its visible modules disappeared.
            self.write_snapshot(&views);
            return Ok(());
        }
        self.preserve_instant = false;
        if self.last_sent.as_ref().is_some_and(|(s, p)| *s == seq && same_prompt(p, & prompt)) {
            self.write_snapshot(&views);
            return Ok(());
        }
        if ( self.agent.refreshing || self.external.refreshing)
            && (prompt.status != RenderStatus::Fresh || self.last_sent.as_ref().is_some_and(|(_, p)| same_prompt(p, & prompt)))
        {
            return Ok(());
        }
        self.module_reports = reports;
        self.observation.next_seq += 1;
        let observation_seq = self.observation.next_seq;
        prompt.observation_seq = Some(observation_seq);
        let baseline_eligible = prompt.status == RenderStatus::Fresh
            && self.sched.index_of(GIT_KEY).is_none_or(|_| data.git.freshness == Freshness::Fresh);
        self.observation
            .sent
            .push_back((req.gen, observation_seq, observation, baseline_eligible));
        while self.observation.sent.len() > 64 {
            self.observation.sent.pop_front();
        }
        let frame = protocol::encode_result(req.gen, self.opts.epoch, &prompt);
        self.write_frame(&frame)?;
        let latency = self.received_at.map(|(t, _)| t.elapsed()).unwrap_or_default();
        match prompt.status {
            RenderStatus::Partial => self.counters.partial_sent += 1,
            RenderStatus::Fresh => self.counters.fresh_sent += 1,
        }
        self.last_result = Some(ResultInfo {
            gen: req.gen,
            status: prompt.status,
            sent_at_ms: now_ms(),
            latency_ms: duration_ms(latency),
            left_lines: prompt.left.len().max(1),
            bytes: frame.len(),
        });
        let fresh = prompt.status == RenderStatus::Fresh;
        self.last_sent = Some((seq, prompt));
        self.write_snapshot(&views);
        if fresh {
            self.write_instant(data);
        }
        Ok(())
    }

    /// Store this prompt as the next session's instant prompt: the same data
    /// rendered as a first prompt (exit 0, no duration, no jobs). Only when the
    /// shell sent a cache key; the write happens on a background thread.
    fn write_instant(&mut self, mut data: crate::model::PromptData) {
        let (Some(key), Some(dir)) = (self.instant_key.as_deref(), self.opts.instant_dir.as_ref()) else { return };
        let path = dir.join(instant::file_name(key));
        data.changes = None;
        data.request.last_exit = 0;
        data.request.last_duration_ms = 0;
        data.request.jobs = 0;
        let mut contents = instant::encode(&self.renderer.render(&data));
        if let Some(identity) = &self.instant_identity { identity.append_to(&mut contents); }
        let dir = dir.clone();
        self.instant.get_or_insert_with(|| InstantWriter::start(Some(dir))).submit(path, contents);
    }

    fn write_frame(&mut self, frame: &str) -> Exit {
        self.out
            .write_all(frame.as_bytes())
            .and_then(|_| self.out.flush())
            .map_err(|_| ExitReason::OutputClosed)
    }

    fn note_session(&mut self, session_id: &str) {
        if self.session_id.as_deref() == Some(session_id) {
            return;
        }
        self.session_id = Some(session_id.to_string());
        self.snapshot_path = match &self.opts.snapshot {
            SnapshotTarget::Disabled => None,
            SnapshotTarget::Path(p) => Some(p.clone()),
            SnapshotTarget::Auto => {
                let dir = super::sessions_dir();
                if let (Some(dir), Some(age)) = (dir.clone(), self.opts.prune_snapshots_after) {
                    let _ = thread::Builder::new()
                        .name("tildy-prune".into())
                        .spawn(move || diagnostics::prune_snapshots(&dir, age));
                }
                dir.map(|d| d.join(super::snapshot_file_name(session_id)))
            }
        };
    }

    /// Hand the snapshot to the background writer (never blocks on disk).
    fn write_snapshot<V>(&mut self, views: &[SlotView<V>]) {
        let Some(path) = self.snapshot_path.clone() else { return };
        let snapshot = self.snapshot(views);
        self.snapshots.get_or_insert_with(diagnostics::SnapshotWriter::start).submit(path, snapshot);
    }

    fn snapshot<V>(&self, views: &[SlotView<V>]) -> Snapshot {
        let slots = views
            .iter()
            .map(|v| SlotInfo {
                provider: v.key.to_string(),
                freshness: v.freshness,
                age_ms: v.age_ms,
                error: v.error.clone(),
                fetching: v.fetching,
                scope: v.scope.to_string(),
            })
            .collect();
        let providers = (0..self.sched.provider_count())
            .map(|p| self.timings.summary(self.sched.spec(p).key, self.sched.stats(p)))
            .collect::<Vec<_>>();
        let mut degraded = self.renderer.degraded();
        degraded.extend(
            views
                .iter()
                .filter(|v| v.freshness == Freshness::Error && v.error.as_deref().is_some_and(|e| e.contains("unresponsive")))
                .map(|v| format!("provider_unresponsive:{}", v.key)),
        );
        Snapshot {
            kind: SNAPSHOT_KIND.into(),
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            written_at_ms: now_ms(),
            worker: WorkerInfo {
                binary_version: BINARY_VERSION.into(),
                protocol_version: PROTOCOL_VERSION,
                epoch: self.opts.epoch,
                pid: self.opts.pid,
                parent_pid: self.opts.parent_pid,
                started_at_ms: self.started_at_ms,
            },
            session_id: self.session_id.clone(),
            config_revision: self.renderer.revision(),
            request: self.sched.current_request().cloned(),
            request_received_at_ms: self.received_at.map(|(_, ms)| ms),
            last_result: self.last_result.clone(),
            slots,
            providers,
            counters: self.counters.clone(),
            degraded,
            watch: self.external.diagnostics.clone(),
            module_reports: self.module_reports.clone(),
            timing_history: self.timing_history.iter().cloned().collect(),
        }
    }

    /// Stop the snapshot writer and remove the snapshot: a clean exit leaves
    /// nothing behind (a crash leaves the last snapshot for inspection).
    fn cleanup(&mut self) {
        self.external.manager = None;
        self.external.inbox = None;
        if let Some(w) = self.snapshots.take() {
            w.shutdown(true);
        }
        // The instant cache outlives the session: finish queued writes.
        if let Some(w) = self.instant.take() {
            w.finish();
        }
    }
}

impl<W: Write> Worker<W> {
    /// After a REQ's partial and before its fetches: apply the OSC 7501 prompt
    /// rule to this session's records, then take the watch baseline, so a
    /// change from here on is pushed.
    fn agent_on_request(&mut self) {
        let Some(req) = self.sched.current_request().cloned() else { return };
        if let Some(store) = &self.opts.agent_store {
            store.drop_on_prompt(&req.session_id);
            if self.agent.last_expire.is_none_or(|t| t.elapsed() >= AGENT_EXPIRE_EVERY) {
                self.agent.last_expire = Some(Instant::now());
                let store = store.clone();
                let _ = thread::Builder::new().name("tildy-agent-expire".into()).spawn(move || store.expire(now_ms()));
            }
        }
        match self.agent_watch(&req) {
            Some((store, scopes, stamp)) => {
                self.agent.next_expiry_ms = store.list(&AgentFilter::live(scopes)).iter().map(|s| s.expires_at_ms).min();
                self.agent.stamp = stamp;
                self.agent.next_poll = Some(Instant::now() + self.opts.agent_poll);
            }
            None => self.agent.next_poll = None,
        }
    }

    /// The first agent_status provider's store, the request's scopes (no git
    /// call: cached worktree only) and their directories' mtimes.
    #[allow(clippy::type_complexity)]
    fn agent_watch(
        &self,
        req: &crate::model::PromptRequest,
    ) -> Option<(AgentStore, Vec<crate::agent::Scope>, Vec<(PathBuf, Option<SystemTime>)>)> {
        let p = self.providers.iter().find_map(|p| p.as_agent_status())?;
        let store = p.store()?.clone();
        let scopes = p.watch_scopes(req);
        let stamp = scopes
            .iter()
            .map(|s| {
                let d = store.scope_dir(s);
                let m = std::fs::metadata(&d).and_then(|m| m.modified()).ok();
                (d, m)
            })
            .collect();
        Some((store, scopes, stamp))
    }

    /// Timer: when a watched directory changed or a record expired and the
    /// agent_status data differs from what the current request shows,
    /// schedule the current request again (its settled result is pushed).
    fn agent_poll(&mut self) -> Exit {
        let now = Instant::now();
        if self.agent.next_poll.is_none_or(|t| t > now) {
            return Ok(());
        }
        let Some(req) = self.sched.current_request().cloned() else {
            self.agent.next_poll = None;
            return Ok(());
        };
        let Some((store, scopes, stamp)) = self.agent_watch(&req) else {
            self.agent.next_poll = None;
            return Ok(());
        };
        self.agent.next_poll = Some(now + self.opts.agent_poll);
        let wall = now_ms();
        if stamp == self.agent.stamp && self.agent.next_expiry_ms.is_none_or(|e| e > wall) {
            return Ok(());
        }
        let records = store.list(&AgentFilter { now_ms: Some(wall), ..AgentFilter::live(scopes) });
        self.agent.stamp = stamp;
        self.agent.next_expiry_ms = records.iter().map(|s| s.expires_at_ms).min();
        if !self.agent_differs(crate::providers::agent::summarize(&records, wall), now) {
            return Ok(());
        }
        self.agent.refreshing = true;
        let keys: Vec<_> = self
            .providers
            .iter()
            .filter(|p| p.as_agent_status().is_some())
            .map(|p| p.spec().key)
            .collect();
        let before = self.provider_stats();
        let orders = self.sched.refresh(&keys, now);
        self.begin_cycle(&before);
        self.spawn(orders);
        self.send_current()
    }

    /// `fresh` differs from an agent_status slot of the current request
    /// (ignoring `age_s`, which changes every second).
    fn agent_differs(&self, fresh: Option<crate::model::ModuleValue>, now: Instant) -> bool {
        let strip = |v: Option<&crate::model::ModuleValue>| {
            v.cloned().map(|mut v| {
                v.fields.remove("age_s");
                v
            })
        };
        let fresh = strip(fresh.as_ref());
        let agent_keys: Vec<&str> =
            self.providers.iter().filter(|p| p.as_agent_status().is_some()).map(|p| p.spec().key).collect();
        self.sched.views(now).iter().filter(|v| agent_keys.contains(&v.key)).any(|v| match &v.value {
            Some(ProviderValue::Module(_, slot)) => strip(slot.value.as_ref()) != fresh,
            _ => true,
        })
    }
}

fn same_prompt(a: &RenderedPrompt, b: &RenderedPrompt) -> bool {
    a.status == b.status
        && a.left == b.left
        && a.right == b.right
        && a.transient == b.transient
        && a.history == b.history
}

impl<W: Write> Worker<W> {
    fn provider_stats(&self) -> Vec<crate::scheduler::ProviderStats> {
        (0..self.sched.provider_count())
            .map(|p| self.sched.stats(p))
            .collect()
    }

    fn begin_cycle(&mut self, before: &[crate::scheduler::ProviderStats]) {
        self.cycle_serial += 1;
        let now = Instant::now();
        let providers = self
            .sched
            .metadata_views(now)
            .iter()
            .enumerate()
            .filter_map(|(p, v)| {
                let a = self.sched.stats(p);
                let b = before.get(p).cloned().unwrap_or_default();
                let executed = a.started > b.started;
                let coalesced = a.coalesced > b.coalesced;
                let cache_hit = a.cache_hits > b.cache_hits;
                (executed || coalesced || cache_hit).then(|| ProviderCycleTiming {
                    provider: v.key.into(),
                    duration_ms: 0,
                    cache_hit,
                    executed,
                    completed: false,
                    timed_out: false,
                    coalesced,
                    coalesced_count: a.coalesced.saturating_sub(b.coalesced),
                })
            })
            .collect();
        let gen = self.sched.current_request().map_or(0, |r| r.gen);
        self.timing_history.push_back(TimingCycle {
            gen,
            at_ms: now_ms(),
            providers,
        });
        self.cycle_ids.push_back(self.cycle_serial);
        while self.timing_history.len() > TIMING_HISTORY_LIMIT {
            self.timing_history.pop_front();
            if let Some(old) = self.cycle_ids.pop_front() {
                self.cycle_fetches.retain(|_, cycle| *cycle != old);
            }
        }
    }

    /// Completion belongs to the execution's originating cycle and to any
    /// current cycle that receives its shared result. It never implies that
    /// the current cycle started another execution.
    fn note_cycle_completed(&mut self, serial: u64, key: &str) {
        let Some(cycle) = self.cycle_ids.iter().position(|s| *s == serial).and_then(|i| self.timing_history.get_mut(i)) else { return };
        if let Some(timing) = cycle.providers.iter_mut().find(|p| p.provider == key) {
            timing.completed = true;
        } else {
            // A targeted refresh may have opened this cycle while another
            // provider's older fetch was still running.
            cycle.providers.push(ProviderCycleTiming {
                provider: key.into(), duration_ms: 0, cache_hit: false,
                executed: false, completed: true, timed_out: false,
                coalesced: false, coalesced_count: 0,
            });
        }
    }

    fn record_fetch_duration(&mut self, id: FetchId, key: &str, elapsed: Duration, timed_out: bool) {
        let Some(origin) = self.cycle_fetches.get(&id).copied() else { return };
        let elapsed = elapsed.as_millis().min(u64::MAX as u128) as u64;
        let previous = self.fetch_elapsed.insert(id, elapsed).unwrap_or(0);
        if let Some(cycle) = self.cycle_ids.iter().position(|s| *s == origin).and_then(|i| self.timing_history.get_mut(i)) {
            if let Some(timing) = cycle.providers.iter_mut().find(|p| p.provider == key) {
                timing.duration_ms = timing.duration_ms.saturating_add(elapsed.saturating_sub(previous));
                timing.timed_out |= timed_out;
            }
        }
    }

    fn note_cycle_timeouts(&mut self) {
        let timed: Vec<_> = self
            .sched
            .metadata_views(Instant::now())
            .iter()
            .filter(|v| v.freshness == Freshness::Timeout)
            .map(|v| v.key)
            .collect();
        if let Some(cycle) = self.timing_history.back_mut() {
            for p in &mut cycle.providers {
                p.timed_out |= (p.executed || p.coalesced) && timed.contains(&p.provider.as_str());
            }
        }
    }

    fn sync_watch(&mut self) {
        let views = self.sched.views(Instant::now());
        let git = views.iter().find_map(|v| match &v.value {
            Some(ProviderValue::Git(s)) => s.value.as_ref(),
            _ => None,
        });
        let identity = git.filter(|g| g.in_repo).and_then(WorktreeWatch::identity);
        let mut external_files: Vec<_> = self.sched.current_request().into_iter().flat_map(|req| self.providers.iter().flat_map(|p| p.watch_paths(req))).map(|path| normalized_watch_target(&path)).collect();
        external_files.sort(); external_files.dedup();
        let external_parents: BTreeMap<_, _> = external_files.iter().filter_map(|p| watch::external_parent(p).map(|parent| (p.clone(), parent))).collect();
        if self.external.identity == identity && self.external.external_files == external_files && self.external.external_parents == external_parents {
            return;
        }
        self.external.token += 1;
        self.external.inbox = None;
        self.external.identity = identity.clone();
        self.external.external_files = external_files.clone();
        self.external.external_parents = external_parents;
        self.external.gate = RefreshGate::default();
        self.external.paths.clear();
        self.external.rescan = false;
        self.external.fallback = identity.as_ref().and(self.opts.idle_refresh).map(|d| Instant::now() + d);
        self.external.diagnostics.mode = "off".into();
        if self.opts.watch_enabled && identity.is_some() && self.external.manager.is_none() {
            let tx = self.tx.clone();
            let notice = move |notice| { let _ = tx.send(Event::Watch(notice)); };
            #[cfg(not(test))]
            let manager = WatchManager::start(notice);
            #[cfg(test)]
            let manager = WatchManager::with_delay(notice, self.opts.watch_setup_delay);
            self.external.manager = manager.ok();
        }
        if let Some(manager) = &self.external.manager {
            let paths = identity.filter(|_| self.opts.watch_enabled).map(|identity| (identity, external_files));
            manager.set_target(self.external.token, paths);
        }
    }

    fn watch_notice(&mut self, notice: WatchNotice) {
        match notice {
            WatchNotice::Ready { token, inbox } if token == self.external.token && self.external.identity.is_some() => {
                self.external.inbox = Some(inbox);
                self.external.fallback = None;
                self.external.diagnostics.mode = if cfg!(target_os = "macos") { "fsevents" } else { "events" }.into();
                // Requery through the existing gate: changes during native
                // setup were not guaranteed to produce events.
                self.external.rescan = true;
                self.external.gate.event(Instant::now());
                if let Some((paths, count)) = self.external.inbox.as_ref().and_then(WatchInbox::take_events) {
                    self.external.gate.coalesced += count.saturating_sub(1);
                    self.watch_event(token, paths);
                }
                self.write_snapshot(&self.sched.metadata_views(Instant::now()));
            }
            WatchNotice::Failed { token } if token == self.external.token => {
                self.external.inbox = None;
                self.external.diagnostics.mode = "off".into();
                if self.external.fallback.is_none() {
                    self.external.fallback = self.opts.idle_refresh.map(|d| Instant::now() + d);
                }
                self.write_snapshot(&self.sched.metadata_views(Instant::now()));
            }
            WatchNotice::Changed { token } if token == self.external.token => {
                if let Some((paths, count)) = self.external.inbox.as_ref().and_then(WatchInbox::take_events) {
                    self.external.gate.coalesced += count.saturating_sub(1);
                    self.watch_event(token, paths);
                }
            }
            _ => {} // Late setup/event for a superseded worktree.
        }
    }

    fn watch_event(&mut self, token: u64, paths: Result<Vec<PathBuf>, ()>) {
        if token != self.external.token || self.external.identity.is_none() {
            return;
        }
        let now = Instant::now();
        match paths {
            Ok(paths) => {
                let identity = self.external.identity.as_ref().expect("watch identity");
                let relevant: Vec<_> = paths
                    .into_iter()
                    .filter(|p| {
                        repository_event_path(p, identity)
                            || self.external.external_files.iter().any(|target| self.external_target_changed(p, target))
                    })
                    .collect();
                if relevant.is_empty() {
                    return;
                }
                for path in relevant {
                    if self.external.paths.len() < 256 && !self.external.paths.contains(&path) {
                        self.external.paths.push(path);
                    }
                }
            }
            Err(()) => {
                self.external.rescan = true;
                self.external.inbox = None;
                if let Some(manager) = &self.external.manager { manager.set_target(self.external.token, None); }
                self.external.diagnostics.mode = "off".into();
                self.external.fallback = self.opts.idle_refresh.map(|d| now + d);
            }
        }
        self.external.diagnostics.last_event_ms = Some(now_ms());
        self.external.gate.event(now);
        self.external.diagnostics.coalesced_events = self.external.gate.coalesced;
    }

    /// A broad ancestor watch also sees unrelated siblings (including our
    /// own snapshot). Only the declared file, or an actual change in its
    /// nearest existing ancestor, is relevant outside the worktree.
    fn external_target_changed(&self, path: &Path, target: &Path) -> bool {
        external_path_matches_target(path, target, self.external.external_parents.get(target).map(PathBuf::as_path))
    }

    fn external_poll(&mut self) -> Exit {
        let now = Instant::now();
        let fallback = self.external.fallback.is_some_and(|t| t <= now);
        let event = self.external.gate.due(now);
        if !fallback && !event {
            return Ok(());
        }
        if fallback {
            self.external.diagnostics.fallback_count += 1;
            self.external.fallback = self.opts.idle_refresh.map(|d| now + d);
        }
        let Some(req) = self.sched.current_request().cloned() else {
            return Ok(());
        };
        let paths = std::mem::take(&mut self.external.paths);
        let rescan = std::mem::take(&mut self.external.rescan) || fallback;
        let repo_paths: Vec<_> = paths.iter().filter(|path| self.external.identity.as_ref().is_some_and(|identity| {
            repository_event_path(path, identity)
        })).cloned().collect();
        let keys: Vec<_> = self.providers.iter().filter(|provider| {
            if provider.as_agent_status().is_some() || !provider.spec().deps.contains(&Dependency::Cwd) {
                return false;
            }
            if rescan { return true; }
            // A provider's conservative default applies only to repository
            // events. External events belong solely to its declared files.
            (!repo_paths.is_empty() && provider.affected_by_paths(&req, &repo_paths))
                || provider.watch_paths(&req).iter().any(|target| {
                    let target = normalized_watch_target(target);
                    paths.iter().any(|path| self.external_target_changed(path, &target))
                })
        }).map(|provider| provider.spec().key).collect();
        if keys.is_empty() {
            return Ok(());
        }
        self.observation.begin_external();
        self.external.refreshing = true;
        let before = self.provider_stats();
        let orders = self.sched.refresh(&keys, now);
        self.begin_cycle(&before);
        self.spawn(orders);
        self.write_snapshot(&self.sched.metadata_views(now));
        Ok(())
    }
}

fn repository_event_path(path: &Path, identity: &[PathBuf]) -> bool {
    // Git-generated objects/locks do not affect status; declared external
    // config files are checked separately and may intentionally live here.
    if identity[1..].iter().any(|dir| path.starts_with(dir)) {
        watch::relevant_git_path(path, identity) && path.extension().is_none_or(|ext| ext != "lock")
    } else {
        path.starts_with(&identity[0])
    }
}

fn external_path_matches_target(path: &Path, target: &Path, previous_parent: Option<&Path>) -> bool {
    path == target || (target.starts_with(path)
        && previous_parent != watch::external_parent(target).as_deref())
}

/// Native events use canonical paths even when a missing config was named
/// through /var -> /private/var. Resolve the existing prefix, keeping the
/// missing suffix, so registration and event routing use the same identity.
fn normalized_watch_target(path: &Path) -> PathBuf {
    let absolute;
    let path = if path.is_absolute() { path } else {
        absolute = std::env::current_dir().map(|cwd| cwd.join(path)).unwrap_or_else(|_| path.to_path_buf());
        absolute.as_path()
    };
    if let Ok(canonical) = path.canonicalize() { return canonical; }
    for parent in path.ancestors().skip(1).filter(|p| p.is_dir()) {
        if let (Ok(canonical), Ok(suffix)) = (parent.canonicalize(), path.strip_prefix(parent)) {
            return canonical.join(suffix);
        }
    }
    path.to_path_buf()
}

fn provider_slot(v: &SlotView<ProviderValue>) -> ProviderSlot {
    ProviderSlot { key: v.key, freshness: v.freshness, age_ms: v.age_ms, value: v.value.clone(), error: v.error.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PromptData;

    fn run(input: &str, dir: &std::path::Path) -> String {
        let mut out = Vec::new();
        let renderer = Box::new(|d: &PromptData| RenderedPrompt {
            status: RenderStatus::Fresh,
            left: vec![format!("{} rc={} jobs={}", d.request.cwd.display(), d.request.last_exit, d.request.jobs)],
            right: Some(format!("env=[{}]", d.request.env)),
            transient: None,
            history: None,
            observation_seq: None,
        });
        let opts = WorkerOptions {
            snapshot: SnapshotTarget::Disabled,
            agent_store: None,
            prune_snapshots_after: None,
            instant_dir: Some(dir.to_path_buf()),
            epoch: 9,
            ..WorkerOptions::default()
        };
        let reason = run_worker(std::io::Cursor::new(input.as_bytes().to_vec()), &mut out, Vec::new, renderer, opts);
        assert_eq!(reason, ExitReason::Eof);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn fresh_result_writes_instant_cache_without_key_in_env() {
        let dir = tempfile::tempdir().unwrap();
        let out = run("REQ\t1\ts\t/r\t80\t24\t1\t5000\tUSER=u\x1fTILDY_INSTANT_KEY=v1_c80_tnone_00c0ffee\t2\n", dir.path());
        // The live RES keeps exit/jobs and never sees the key.
        assert!(out.contains("L\t/r rc=1 jobs=2\nR\tenv=[USER=u]\n"), "{out}");
        let file = std::fs::read_to_string(dir.path().join("v1_c80_tnone_00c0ffee.prompt")).unwrap();
        // The cache is the first-prompt form: exit 0, no jobs.
        assert_eq!(file, "tildy-instant\t1\t1\nL\t/r rc=0 jobs=0\nR\tenv=[USER=u]\n");
    }

    /// Frames written by the worker, line by line, over a channel.
    struct ChanWriter(mpsc::Sender<String>, String);

    impl Write for ChanWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.1.push_str(&String::from_utf8_lossy(buf));
            while let Some(i) = self.1.find('\n') {
                let line: String = self.1.drain(..=i).collect();
                let _ = self.0.send(line.trim_end_matches('\n').to_string());
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The next RES as (gen, status, left line), within `ms`.
    fn next_res(rx: &mpsc::Receiver<String>, ms: u64) -> Option<(u64, String, String)> {
        let end = Instant::now() + Duration::from_millis(ms);
        let mut head: Option<(u64, String)> = None;
        loop {
            let line = rx.recv_timeout(end.saturating_duration_since(Instant::now())).ok()?;
            let f: Vec<&str> = line.split('\t').collect();
            match f[0] {
                "RES" => head = Some((f[1].parse().unwrap(), f[2].to_string())),
                "L" => {
                    if let Some((g, st)) = head.take() {
                        return Some((g, st, f[1].to_string()));
                    }
                }
                _ => {}
            }
        }
    }

    /// Wait for a fresh RES whose left line is `want` (other frames may come first).
    fn wait_left(rx: &mpsc::Receiver<String>, want: &str, ms: u64) -> (u64, String) {
        let end = Instant::now() + Duration::from_millis(ms);
        let mut seen = vec![];
        while let Some((g, st, left)) = next_res(rx, end.saturating_duration_since(Instant::now()).as_millis() as u64) {
            if left == want && st == "fresh" {
                return (g, st);
            }
            seen.push(format!("{g} {st} {left}"));
        }
        panic!("no RES with {want:?} within {ms} ms; saw {seen:?}");
    }

    #[test]
    fn agent_status_push_expiry_and_prompt_rule() {
        use crate::agent::{Record, Scope, State, Store};
        use crate::providers::agent::AgentStatusProvider;
        let td = tempfile::tempdir().unwrap();
        let store = Store::open_with(td.path().join("state"));
        let cwd = td.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        let (reader, mut writer) = std::io::pipe().unwrap();
        let (tx, rx) = mpsc::channel();
        let renderer = Box::new(|d: &PromptData| {
            let v = d.modules.get("agent").and_then(|s| s.value.as_ref());
            let left = v.map_or("none".to_string(), |v| format!("{} {}", v.fields["state"], v.text));
            RenderedPrompt { status: RenderStatus::Fresh, left: vec![left], right: None, transient: None,
                history: None,
                observation_seq: None, }
        });
        let opts = WorkerOptions {
            snapshot: SnapshotTarget::Disabled,
            prune_snapshots_after: None,
            instant_dir: None,
            agent_store: Some(store.clone()),
            agent_poll: Duration::from_millis(50),
            ..WorkerOptions::default()
        };
        let st = store.clone();
        let worker = thread::spawn(move || {
            let p: Arc<dyn Provider> = Arc::new(AgentStatusProvider::with_store("agent", Some(st)));
            run_worker(reader, ChanWriter(tx, String::new()), move || vec![p], renderer, opts)
        });
        let req = |gen: u64| format!("REQ\t{gen}\tS\t{}\t80\t24\t0\t0\t\t0\n", cwd.display());
        let publish = |state: State, msg: &str, origin: Option<&str>, ttl: Option<u32>| {
            let mut r = Record::new(state, Scope::session("S"));
            r.id = msg.into();
            r.msg = Some(msg.into());
            r.origin_session = origin.map(str::to_string);
            r.ttl_s = ttl;
            store.publish(r).unwrap();
        };
        writer.write_all(req(1).as_bytes()).unwrap();
        assert_eq!(wait_left(&rx, "none", 2000), (1, "fresh".into()));

        // A publish shows up on the current generation without a new REQ.
        publish(State::Blocked, "hello", None, Some(1));
        let t = Instant::now();
        assert_eq!(wait_left(&rx, "blocked hello", 1500), (1, "fresh".into()));
        assert!(t.elapsed() < Duration::from_millis(1000), "pushed after {:?}", t.elapsed());
        // ...and leaves when its TTL runs out.
        assert_eq!(wait_left(&rx, "none", 2500).0, 1);
        // A record in another session is not watched.
        let mut other = Record::new(State::Error, Scope::session("OTHER"));
        other.msg = Some("elsewhere".into());
        store.publish(other).unwrap();
        assert!(next_res(&rx, 300).is_none(), "nothing to push for another session");

        // The prompt rule: this session's working record goes at its next prompt,
        // a done record shows at one prompt and goes at the following one.
        publish(State::Working, "busy", Some("S"), None);
        wait_left(&rx, "working busy", 1500);
        writer.write_all(req(2).as_bytes()).unwrap();
        assert_eq!(wait_left(&rx, "none", 2000).0, 2);
        publish(State::Done, "built", Some("S"), None);
        wait_left(&rx, "done built", 1500);
        writer.write_all(req(3).as_bytes()).unwrap();
        let (g, st) = wait_left(&rx, "done built", 2000);
        assert_eq!((g, st.as_str()), (3, "fresh"), "shown at the first prompt");
        writer.write_all(req(4).as_bytes()).unwrap();
        loop {
            let (g, st, left) = next_res(&rx, 2000).expect("RES for gen 4");
            if g == 4 && st == "fresh" {
                assert_eq!(left, "none", "dropped at the second prompt");
                break;
            }
        }
        drop(writer);
        assert_eq!(worker.join().unwrap(), ExitReason::Eof);
    }

    #[test]
    fn no_key_no_cache() {
        let dir = tempfile::tempdir().unwrap();
        run("REQ\t1\ts\t/r\t80\t24\t0\t0\tUSER=u\t0\n", dir.path());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    #[test]
    fn observation_only_accepts_sent_current_generation_and_increasing_sequences() {
        let observed = |branch: &str| Observation {
            branch: Some(branch.into()),
            ..Observation::default()
};
        let mut state = ObservationState::default();
        state.sent.push_back((7, 10, observed("main"), true));
        state.sent.push_back((7, 11, observed("next"), true));
        state.acknowledge(6, 10);
        state.acknowledge(7, 99);
        assert!(state.observed.is_none());
        state.acknowledge(7, 11);
        assert_eq!(
            state.observed.as_ref().unwrap().branch.as_deref(),
            Some("next")
        );
        state.acknowledge(7, 10);
        assert_eq!(
            state.observed.as_ref().unwrap().branch.as_deref(),
            Some("next")
        );
        state.new_request();
        state.acknowledge(7, 11);
        assert!(state.observed.is_none());
    }

    #[test]
    fn external_changes_accumulate_from_original_observed_baseline() {
        let observation = |branch: &str, files| Observation {
            root: Some("/repo".into()),
            branch: Some(branch.into()),
            files: Some(files),
            ..Observation::default()
        };
        let mut state = ObservationState {
            observed: Some(observation("A", 0)),
            ..ObservationState::default()
        };
        state.begin_external();
        let first = state
            .anchor
            .as_ref()
            .unwrap()
            .changes(&observation("B", 1))
            .unwrap();
        assert_eq!(first.branch_from.as_deref(), Some("A"));
        state.observed = Some(observation("B", 1));
        state.begin_external();
        let next = state
            .anchor
            .as_ref()
            .unwrap()
            .changes(&observation("C", 3))
            .unwrap();
        assert_eq!(next.branch_from.as_deref(), Some("A"));
        assert_eq!(next.branch_to.as_deref(), Some("C"));
        assert_eq!(next.files_added, 3);
        state.new_request();
        assert!(state.anchor.is_none());
    }
    #[test]
    fn partial_ack_cannot_establish_or_regress_idle_observation_baseline() {
        let mut state = ObservationState::default();
        let observed = |branch: &str| Observation { branch: Some(branch.into()), ..Observation::default() };
        state.sent.push_back((1, 1, observed("cached-before-command"), false));
        state.acknowledge(1, 1);
        assert!(state.observed.is_none());
        state.begin_external();
        assert!(state.anchor.is_none());
        state.sent.push_back((1, 2, observed("after-command"), true));
        state.acknowledge(1, 2);
        state.begin_external();
        assert_eq!(state.anchor.as_ref().unwrap().branch.as_deref(), Some("after-command"));
        state.sent.push_back((2, 3, observed("stale-resize-cache"), false));
        state.acknowledge(2, 3);
        assert_eq!(state.observed.as_ref().unwrap().branch.as_deref(), Some("after-command"));
    }

    struct WatchableGit {
        spec: crate::model::ProviderSpec,
        root: PathBuf,
        version: Arc<std::sync::atomic::AtomicU64>,
    }
    impl Provider for WatchableGit {
        fn spec(&self) -> &crate::model::ProviderSpec { &self.spec }
        fn fetch(&self, _req: &crate::model::PromptRequest) -> ProviderValue {
            let branch = if self.version.load(std::sync::atomic::Ordering::SeqCst) == 0 { "main" } else { "feature" };
            ProviderValue::Git(crate::model::Slot::fresh(crate::model::GitData {
                in_repo: true, branch: Some(branch.into()), git_dir: Some(self.root.join(".git")),
                worktree_root: Some(self.root.clone()), worktree_id: Some(self.root.display().to_string()),
                ..crate::model::GitData::default()
            }))
        }
    }
    fn complete_res(rx: &mpsc::Receiver<String>, timeout: Duration) -> (String, String, u64) {
        let deadline = Instant::now() + timeout;
        let mut status = String::new(); let mut left = String::new(); let mut seq = 0;
        loop {
            let line = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("complete RES expected");
            let (tag, rest) = line.split_once('\t').unwrap_or((&line, ""));
            match tag {
                "RES" => status = rest.split('\t').nth(1).unwrap().into(),
                "L" => left = rest.into(),
                "O" => seq = rest.parse().unwrap(),
                "END" => return (status, left, seq),
                _ => {}
            }
        }
    }
    #[test]
    fn slow_watch_setup_never_blocks_fresh_frame_and_covers_pre_ready_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        let version = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let provider = Arc::new(WatchableGit {
            spec: crate::model::ProviderSpec { key: "git", deps: &[Dependency::Cwd], ttl: Duration::from_secs(5), timeout: Duration::from_secs(1) },
            root: root.clone(), version: version.clone(),
        });
        let renderer = Box::new(|data: &PromptData| {
            let branch = data.git.value.as_ref().and_then(|g| g.branch.as_deref()).unwrap_or("pending");
            let changes = data.changes.as_ref().map(|c| format!("{}→{}", c.branch_from.as_deref().unwrap_or(""), c.branch_to.as_deref().unwrap_or(""))).unwrap_or_default();
            RenderedPrompt { status: RenderStatus::Fresh, left: vec![format!("{branch} {changes}")], right: None, transient: None, history: None, observation_seq: None }
        });
        let (input, mut shell) = std::io::pipe().unwrap();
        let (tx, rx) = mpsc::channel();
        let opts = WorkerOptions {
            epoch: 9, snapshot: SnapshotTarget::Disabled, instant_dir: None, agent_store: None,
            watch_enabled: true, watch_setup_delay: Duration::from_secs(2),
            ..WorkerOptions::default()
        };
        let worker = std::thread::spawn(move || run_worker(input, ChanWriter(tx, String::new()), move || vec![provider as Arc<dyn Provider>], renderer, opts));
        writeln!(shell, "REQ\t1\ts\t{}\t120\t40\t0\t0\tTILDY_SUBMISSION_ID=0\t0", root.display()).unwrap();
        shell.flush().unwrap();
        // Native setup is deliberately delayed 2s. A settled result must still
        // arrive within 1s; increasing a startup timeout cannot satisfy this.
        let deadline = Instant::now() + Duration::from_secs(1);
        let seq = loop {
            let (status, left, seq) = complete_res(&rx, deadline.saturating_duration_since(Instant::now()));
            if status == "fresh" { assert_eq!(left, "main "); break seq; }
        };
        writeln!(shell, "ACK\t1\t9\t{seq}").unwrap(); shell.flush().unwrap();
        version.store(1, std::sync::atomic::Ordering::SeqCst);
        // No filesystem event is emitted: the ready rescan must cover changes
        // that occurred before OS monitoring was available.
        let (status, left, _) = complete_res(&rx, Duration::from_secs(6));
        assert_eq!((status.as_str(), left.as_str()), ("fresh", "feature main→feature"));
        writeln!(shell, "BYE").unwrap(); shell.flush().unwrap();
        assert_eq!(worker.join().unwrap(), ExitReason::Bye);
    }

    #[test]
    fn external_paths_keep_missing_suffix_and_only_accept_needed_ancestor_changes() {
        let home = tempfile::tempdir().unwrap();
        let canonical_home = home.path().canonicalize().unwrap();
        let lexical = home.path().join(".docker/config.json");
        let absent = format!("tildy-absent-watch-{}", std::process::id());
        let relative = Path::new(&absent).join("config.json");
        assert_eq!(normalized_watch_target(&relative), std::env::current_dir().unwrap().canonicalize().unwrap().join(relative));
        let target = normalized_watch_target(&lexical);
        assert_eq!(target, canonical_home.join(".docker/config.json"));
        assert!(!external_path_matches_target(&canonical_home, &target, Some(&canonical_home)), "ordinary HOME metadata is unrelated");
        assert!(!external_path_matches_target(&canonical_home.join("snapshot.json"), &target, Some(&canonical_home)));
        assert!(!external_path_matches_target(&canonical_home.join(".docker-other"), &target, Some(&canonical_home)));
        std::fs::create_dir(target.parent().unwrap()).unwrap();
        assert!(external_path_matches_target(target.parent().unwrap(), &target, Some(&canonical_home)));
        assert!(external_path_matches_target(&canonical_home, &target, Some(&canonical_home)), "coarse ancestor event must cover newly created directory");
        assert!(external_path_matches_target(&target, &target, Some(target.parent().unwrap())));
        assert!(!external_path_matches_target(&canonical_home, &target, Some(target.parent().unwrap())), "old ancestors without parent movement are noise");
        std::fs::remove_dir(target.parent().unwrap()).unwrap();
        assert!(external_path_matches_target(target.parent().unwrap(), &target, Some(target.parent().unwrap())), "removed closest ancestor must broaden the watch");
        #[cfg(unix)] {
            let alias = home.path().join("alias");
            std::os::unix::fs::symlink(&canonical_home, &alias).unwrap();
            assert_eq!(normalized_watch_target(&alias.join("new/config.json")), canonical_home.join("new/config.json"));
        }
    }

    #[test]
    fn early_changed_and_stale_ready_cannot_skip_current_ready_rescan() {
        let (tx, rx) = mpsc::channel();
        let opts = WorkerOptions { snapshot: SnapshotTarget::Disabled, agent_store: None, instant_dir: None, ..WorkerOptions::default() };
        let renderer = Box::new(|_: &PromptData| RenderedPrompt { status: RenderStatus::Fresh, left: vec!["prompt".into()], right: None, transient: None, history: None, observation_seq: None });
        let mut worker = Worker {
            out: Vec::<u8>::new(), providers: Vec::new(), renderer,
            sched: Scheduler::new(Vec::new(), opts.scheduler.clone()), config_epoch: 0,
            tx, rx, deferred: VecDeque::new(), opts, next_parent_check: None,
            started_at_ms: 0, timings: TimingRecorder::default(), counters: Counters::default(),
            received_at: None, last_sent: None, last_result: None, session_id: None,
            snapshot_path: None, snapshots: None, instant_key: None, instant: None,
            presentation_env: instant::PresentationEnv::default(), instant_identity: None, preserve_instant: false,
            agent: AgentWatch::default(), external: ExternalWatch::default(),
            observation: ObservationState::default(), timing_history: VecDeque::new(),
            cycle_fetches: HashMap::new(), fetch_elapsed: HashMap::new(), cycle_serial: 0,
            cycle_ids: VecDeque::new(), module_reports: Vec::new(),
        };
        worker.external.token = 7;
        worker.external.identity = Some(vec!["/new".into()]);
        worker.external.fallback = Some(Instant::now() + Duration::from_secs(10));
        worker.watch_notice(WatchNotice::Changed { token: 7 });
        assert!(worker.external.gate.deadline().is_none());
        worker.watch_notice(WatchNotice::Ready { token: 6, inbox: WatchInbox::empty() });
        assert!(worker.external.inbox.is_none());
        assert!(worker.external.fallback.is_some());
        worker.watch_notice(WatchNotice::Ready { token: 7, inbox: WatchInbox::empty() });
        assert!(worker.external.inbox.is_some());
        assert!(worker.external.fallback.is_none());
        assert!(worker.external.rescan, "early ignored events must be covered");
        assert!(worker.external.gate.deadline().is_some());
        // A later directory change must reject both old event and ready notices.
        worker.external.token = 8;
        worker.external.inbox = None;
        worker.external.rescan = false;
        worker.external.gate = RefreshGate::default();
        worker.watch_notice(WatchNotice::Changed { token: 7 });
        worker.watch_notice(WatchNotice::Ready { token: 7, inbox: WatchInbox::empty() });
        assert!(worker.external.inbox.is_none());
        assert!(!worker.external.rescan);
        assert!(worker.external.gate.deadline().is_none());
    }

}
