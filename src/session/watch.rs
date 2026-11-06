//! Event-driven worktree watching. The callback never runs providers or scans
//! the repository: it only hands bounded event paths back to the worker.
use crate::model::GitData;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
type NativeWatch = macos::NativeWatch;
#[cfg(not(target_os = "macos"))]
type NativeWatch = notify::RecommendedWatcher;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
type Pending = Option<(Result<Vec<PathBuf>, ()>, u64)>;

pub const DEBOUNCE: Duration = Duration::from_millis(300);
pub const MIN_REFRESH: Duration = Duration::from_secs(1);

pub struct WorktreeWatch {
    // Dropping the handle stops the OS watcher and its callback.
    _handle: NativeWatch,
    pending: Arc<Mutex<Pending>>,
}

impl WorktreeWatch {
    pub fn identity(git: &GitData) -> Option<Vec<PathBuf>> {
        let root = git.worktree_root.as_ref()?;
        let dir = git.git_dir.as_ref()?;
        let common = std::fs::read_to_string(dir.join("commondir"))
            .ok()
            .map(|s| dir.join(s.trim()))
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| dir.clone());
        Some(vec![root.clone(), dir.clone(), common])
    }

    pub fn take_events(&self) -> Option<(Result<Vec<PathBuf>, ()>, u64)> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    pub fn start(
        identity: Vec<PathBuf>,
        external_files: &[PathBuf],
        mut event: impl FnMut() + Send + 'static,
    ) -> std::io::Result<Self> {
        let pending: Arc<Mutex<Pending>> = Arc::default();
        let inbox = pending.clone();
        let callback = move |paths: Result<Vec<PathBuf>, ()>| {
            let mut pending = inbox.lock().unwrap_or_else(|e| e.into_inner());
            let notify = pending.is_none();
            let (old, count) = pending.get_or_insert_with(|| (Ok(Vec::new()), 0));
            *count += 1;
            match (old.as_mut(), paths) {
                (Ok(old), Ok(paths)) if old.len() + paths.len() <= 256 => old.extend(paths),
                _ => *old = Err(()),
            }
            drop(pending);
            // At most one outstanding worker event, even during large builds.
            if notify { event(); }
        };
        // Validate/collect the entire set before native initialization, so a
        // partial target never starts a stream. No recursive directory scan.
        let paths = watch_paths(&identity, external_files)?;
        #[cfg(target_os = "macos")]
        let handle = NativeWatch::start(paths, callback)?;
        #[cfg(not(target_os = "macos"))]
        let handle = start_notify(&paths, callback)?;
        Ok(Self {
            _handle: handle,
            pending,
        })
    }
}

fn watch_paths(identity: &[PathBuf], external_files: &[PathBuf]) -> std::io::Result<Vec<(PathBuf, bool)>> {
    let root = identity.first().ok_or_else(|| std::io::Error::other("no worktree watch root"))?.canonicalize()?;
    let mut paths = vec![(root.clone(), true)];
    let mut registered = std::collections::HashSet::from([root.clone()]);
    for dir in identity.iter().skip(1) {
        if !dir.starts_with(&root) && !dir.starts_with(&identity[0]) {
            let dir = dir.canonicalize()?;
            if registered.insert(dir.clone()) {
                paths.push((dir.clone(), false));
                let refs = dir.join("refs");
                if refs.is_dir() && registered.insert(refs.clone()) { paths.push((refs, true)); }
            }
        }
    }
    for file in external_files {
        if let Some(parent) = external_parent(file).filter(|p| !p.starts_with(&root)) {
            if registered.insert(parent.clone()) { paths.push((parent, false)); }
        }
    }
    if paths.iter().any(|(p,_)| !p.is_dir()) { return Err(std::io::Error::other("watch target is not a directory")); }
    Ok(paths)
}

#[cfg(not(target_os = "macos"))]
fn start_notify(paths: &[(PathBuf, bool)], mut callback: impl FnMut(Result<Vec<PathBuf>, ()>) + Send + 'static) -> std::io::Result<NativeWatch> {
    use notify::{RecursiveMode, Watcher};
    let mut handle = notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
        Ok(e) if !matches!(e.kind, notify::EventKind::Access(_)) => callback(if e.need_rescan() { Err(()) } else { Ok(e.paths) }),
        Ok(_) => {},
        Err(_) => callback(Err(())),
    }).map_err(std::io::Error::other)?;
    let mut batch = handle.paths_mut();
    for (path, recursive) in paths { batch.add(path, if *recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive }).map_err(std::io::Error::other)?; }
    batch.commit().map_err(std::io::Error::other)?;
    Ok(handle)
}

/// Timer policy separated from OS events so burst/coalescing behavior has
/// deterministic tests. The first event starts a trailing-edge debounce.
#[derive(Default)]
pub struct RefreshGate {
    pending_at: Option<Instant>,
    last_refresh: Option<Instant>,
    pub coalesced: u64,
}

impl RefreshGate {
    pub fn event(&mut self, now: Instant) {
        if self.pending_at.is_some() {
            self.coalesced += 1;
        }
        self.pending_at = Some(now + DEBOUNCE);
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.pending_at.map(|t| {
            self.last_refresh
                .map_or(t, |last| t.max(last + MIN_REFRESH))
        })
    }
    pub fn due(&mut self, now: Instant) -> bool {
        if self.deadline().is_some_and(|t| t <= now) {
            self.pending_at = None;
            self.last_refresh = Some(now);
            true
        } else {
            false
        }
    }
}

/// A missing config directory must not disable the healthy worktree watch.
/// Observe its nearest existing ancestor without recursing through HOME.
/// The worker includes this parent in the identity and narrows it after creation.
pub fn external_parent(file: &Path) -> Option<PathBuf> {
    file.parent()?.ancestors().find(|p| p.is_dir()).map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
}

pub fn idle_refresh(raw: Option<&str>) -> Option<Duration> {
    match raw.and_then(|s| s.parse::<u64>().ok()).unwrap_or(10) {
        0 => None,
        n => Some(Duration::from_secs(n.min(86_400))),
    }
}

pub fn relevant_git_path(path: &Path, identity: &[PathBuf]) -> bool {
    identity.get(1..).is_some_and(|dirs| {
        dirs.iter().any(|dir| {
            path.strip_prefix(dir).ok().is_some_and(|p| {
                matches!(
                    p.components().next().and_then(|c| c.as_os_str().to_str()),
                    Some("HEAD" | "index" | "refs" | "packed-refs" | "logs" | "commondir")
                )
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn burst_debounces_and_rate_limits() {
        let t = Instant::now();
        let mut gate = RefreshGate::default();
        for n in 0..50 {
            gate.event(t + Duration::from_millis(n));
        }
        assert!(!gate.due(t + Duration::from_millis(348)));
        assert!(gate.due(t + Duration::from_millis(349)));
        assert_eq!(gate.coalesced, 49);
        gate.event(t + Duration::from_millis(350));
        assert!(!gate.due(t + Duration::from_millis(1348)));
        assert!(gate.due(t + Duration::from_millis(1349)));
        assert!(!gate.due(t + Duration::from_secs(5)));
    }
    #[test]
    fn fallback_config() {
        assert_eq!(idle_refresh(None), Some(Duration::from_secs(10)));
        assert_eq!(idle_refresh(Some("bad")), Some(Duration::from_secs(10)));
        assert_eq!(idle_refresh(Some("0")), None);
        assert_eq!(idle_refresh(Some("2")), Some(Duration::from_secs(2)));
    }
    #[test]
    fn missing_config_directory_keeps_event_watch_and_narrows_after_creation() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let target = external.path().canonicalize().unwrap().join("new/config.json");
        let old_parent = external_parent(&target).unwrap();
        let repo = root.path().canonicalize().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = WorktreeWatch::start(vec![repo.clone(), repo.join(".git"), repo.join(".git")], std::slice::from_ref(&target), move || { let _ = tx.send(()); }).unwrap();
        std::fs::create_dir(target.parent().unwrap()).unwrap();
        rx.recv_timeout(Duration::from_millis(1500)).unwrap();
        assert!(watcher.take_events().is_some());
        assert_ne!(external_parent(&target).unwrap(), old_parent);
    }

    #[test]
    fn complete_watch_target_is_collected_once_with_deduplicated_parents() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let external = external.path().canonicalize().unwrap();
        let git_dir = external.join("worktree"); let common = external.join("common");
        std::fs::create_dir_all(common.join("refs")).unwrap(); std::fs::create_dir(&git_dir).unwrap();
        let paths = watch_paths(&[root, git_dir, common], &[external.join("config.json"), external.join("other.json")]).unwrap();
        assert_eq!(paths.len(), 5, "root, git dir, common dir, refs, one config parent");
    }
    #[test]
    fn invalid_target_fails_before_native_initialization() {
        let root = tempfile::tempdir().unwrap();
        assert!(watch_paths(&[root.path().into(), root.path().join("missing-external-dir")], &[]).is_ok(), "metadata inside the recursively watched root needs no separate registration");
        assert!(watch_paths(&[root.path().join("missing")], &[]).is_err());
        let outside = tempfile::tempdir().unwrap();
        assert!(watch_paths(&[root.path().into(), outside.path().join("missing")], &[]).is_err());
    }

    #[test]
    fn linked_worktree_watches_deep_content_and_atomic_common_ref_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo"); let linked = temp.path().join("linked");
        std::fs::create_dir(&repo).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(cwd).args(args).output().unwrap();
            assert!(out.status.success(), "git failed: {}", String::from_utf8_lossy(&out.stderr)); out.stdout
        };
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-q", "-m", "initial"]);
        git(&repo, &["worktree", "add", "-q", "-b", "feature", linked.to_str().unwrap()]);
        let git_dir = PathBuf::from(String::from_utf8(git(&linked, &["rev-parse", "--absolute-git-dir"])).unwrap().trim());
        let root = linked.canonicalize().unwrap();
        std::fs::create_dir_all(root.join("deep/nested")).unwrap();
        let file = root.join("deep/nested/existing.txt"); std::fs::write(&file, "before").unwrap();
        let identity = WorktreeWatch::identity(&GitData { worktree_root: Some(root), git_dir: Some(git_dir), ..GitData::default() }).unwrap();
        let common = identity[2].clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let watch = WorktreeWatch::start(identity, &[], move || { let _ = tx.send(()); }).unwrap();
        let contains = |target: &Path| {
            let deadline = Instant::now() + Duration::from_millis(1500);
            loop {
                rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
                if watch.take_events().is_some_and(|(result,_)| result.is_ok_and(|paths| paths.iter().any(|p| p == target))) { break; }
            }
        };
        // Content modification of an existing deeply nested file, not only a
        // directory-entry creation, must be observed by the recursive root.
        std::fs::write(&file, "after").unwrap(); contains(&file);
        let temporary = common.join("refs/heads/new-ref.tmp");
        let reference = common.join("refs/heads/new-ref");
        std::fs::write(&temporary, "replacement").unwrap();
        std::fs::rename(&temporary, &reference).unwrap(); contains(&reference);
    }

}

/// Only the inbox crosses back to the worker. OS watcher construction and
/// destruction stay on the single management thread.
#[derive(Clone)]
pub struct WatchInbox(Arc<Mutex<Pending>>);
impl WatchInbox {
    #[cfg(test)]
    pub(crate) fn empty() -> Self { Self(Arc::default()) }
    pub fn take_events(&self) -> Option<(Result<Vec<PathBuf>, ()>, u64)> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

pub enum WatchNotice {
    Ready { token: u64, inbox: WatchInbox },
    Failed { token: u64 },
    Changed { token: u64 },
}

struct Target {
    token: u64,
    paths: Option<(Vec<PathBuf>, Vec<PathBuf>)>,
}
#[derive(Default)]
struct ManagerState {
    pending: Option<Target>,
    token: u64,
    stopped: bool,
}

/// A one-slot latest-target mailbox, not a thread per request. Reconfiguration
/// supersedes queued targets while slow native setup/teardown is in progress.
pub struct WatchManager {
    state: Arc<(Mutex<ManagerState>, std::sync::Condvar)>,
    _thread: std::thread::JoinHandle<()>,
}
impl WatchManager {
    pub fn start(notice: impl Fn(WatchNotice) + Send + Sync + 'static) -> std::io::Result<Self> {
        Self::start_inner(notice, Duration::ZERO)
    }
    #[cfg(test)]
    pub(crate) fn with_delay(notice: impl Fn(WatchNotice) + Send + Sync + 'static, delay: Duration) -> std::io::Result<Self> {
        Self::start_inner(notice, delay)
    }
    fn start_inner(notice: impl Fn(WatchNotice) + Send + Sync + 'static, delay: Duration) -> std::io::Result<Self> {
        let state = Arc::new((Mutex::new(ManagerState::default()), std::sync::Condvar::new()));
        let mailbox = state.clone();
        let notice = Arc::new(notice);
        let thread = std::thread::Builder::new().name("tildy-watch-manager".into()).spawn(move || {
            let mut watcher = None;
            loop {
                let target = {
                    let (lock, wake) = &*mailbox;
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while state.pending.is_none() && !state.stopped {
                        state = wake.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    if state.stopped { break; }
                    state.pending.take().expect("pending target")
                };
                // Native teardown can block; never hold the target lock here.
                drop(watcher.take());
                let current = |token| {
                    let state = mailbox.0.lock().unwrap_or_else(|e| e.into_inner());
                    !state.stopped && state.token == token
                };
                if !current(target.token) { continue; }
                let Some((identity, external_files)) = target.paths else { continue; };
                if !delay.is_zero() { std::thread::sleep(delay); }
                if !current(target.token) { continue; }
                let token = target.token;
                let callback = notice.clone();
                match WorktreeWatch::start(identity, &external_files, move || callback(WatchNotice::Changed { token })) {
                    Ok(ready) if current(token) => {
                        let inbox = WatchInbox(ready.pending.clone());
                        watcher = Some(ready);
                        notice(WatchNotice::Ready { token, inbox });
                    }
                    Ok(stale) => drop(stale),
                    Err(_) if current(token) => notice(WatchNotice::Failed { token }),
                    Err(_) => {}
                }
            }
            // Includes stop during initialization; cleanup stays off the worker.
            drop(watcher);
        })?;
        Ok(Self { state, _thread: thread })
    }
    pub fn set_target(&self, token: u64, paths: Option<(Vec<PathBuf>, Vec<PathBuf>)>) {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.token = token;
        state.pending = Some(Target { token, paths });
        wake.notify_one();
    }
}
impl Drop for WatchManager {
    fn drop(&mut self) {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.stopped = true;
        state.pending = None;
        wake.notify_one();
        // JoinHandle detaches. Native teardown may finish later, but no native
        // handle or callback can mutate the stopped worker's state.
    }
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    #[test]
    fn slow_setup_coalesces_latest_target_and_never_publishes_stale_ready() {
        let first = tempfile::tempdir().unwrap();
        let latest = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let manager = WatchManager::with_delay(move |notice| { let _ = tx.send(notice); }, Duration::from_millis(200)).unwrap();
        let paths = |root: &Path| {
            let root = root.canonicalize().unwrap();
            (vec![root.clone(), root.join(".git"), root.join(".git")], Vec::new())
        };
        manager.set_target(1, Some(paths(first.path())));
        // Let the manager enter slow native preparation while fifty targets
        // replace the one-slot mailbox. No per-target thread is created.
        std::thread::sleep(Duration::from_millis(20));
        for token in 2..=51 { manager.set_target(token, Some(paths(latest.path()))); }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap() {
                WatchNotice::Ready { token, .. } => { assert_eq!(token, 51); break; }
                WatchNotice::Failed { token } => panic!("watch failed: {token}"),
                WatchNotice::Changed { .. } => {}
            }
        }
        drop(manager);
    }
    #[test]
    fn stop_during_slow_setup_never_reactivates_the_watch() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let manager = WatchManager::with_delay(move |notice| { let _ = tx.send(notice); }, Duration::from_millis(200)).unwrap();
        manager.set_target(1, Some((vec![root.clone(), root.join(".git"), root.join(".git")], Vec::new())));
        std::thread::sleep(Duration::from_millis(20));
        drop(manager);
        // Disconnection proves the management task exited without publishing
        // Ready, rather than merely hiding an old callback until after timeout.
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Err(std::sync::mpsc::RecvTimeoutError::Disconnected)));
    }

    #[test]
    fn real_rebuilds_release_callbacks_and_management_task_on_stop() {
        let first = tempfile::tempdir().unwrap(); let second = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let manager = WatchManager::start(move |notice| { let _ = tx.send(notice); }).unwrap();
        for token in 1..=8 {
            let root = if token % 2 == 0 { second.path() } else { first.path() }.canonicalize().unwrap();
            manager.set_target(token, Some((vec![root.clone(), root.join(".git"), root.join(".git")], Vec::new())));
            let deadline = Instant::now() + Duration::from_secs(5);
            let inbox = loop {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap() {
                    WatchNotice::Ready { token: ready, inbox } => { assert_eq!(ready, token); break inbox; },
                    WatchNotice::Failed { token } => panic!("watch start failed {token}"),
                    WatchNotice::Changed { .. } => {}
                }
            };
            let file = root.join(format!("change-{token}"));
            std::fs::write(&file, "changed").unwrap();
            let deadline = Instant::now() + Duration::from_millis(1500);
            loop {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap() {
                    WatchNotice::Changed { token: changed }
                        if changed == token && inbox.take_events().is_some_and(|(result,_)| result.is_ok_and(|paths| paths.contains(&file))) => { break; }
                    WatchNotice::Failed { token } => panic!("watch failed {token}"),
                    _ => {}
                }
            }
        }
        drop(manager);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("watch management task/callback did not stop"),
                Ok(_) => {}
            }
        }
    }

}
