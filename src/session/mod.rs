//! Worker lifecycle: epoch, parent watch, snapshot path. Owner: task B.
//!
//! The backend cores live here so they can be tested without the real config,
//! renderer and providers:
//!
//! * [`worker::run_worker`]: persistent backend (HELLO, REQ loop, partial and
//!   fresh RES, scheduler, snapshot, parent watch, EOF/BYE exit).
//! * [`oneshot::run_oneshot`]: `tildy render` (one REQ, one RES).
//!
//! Both take providers as `Arc<dyn Provider>` and rendering as a
//! [`PromptRenderer`]; `src/cli/{worker,render}.rs` wire in `render::render`
//! with `config::*` and the config-derived providers
//! (`providers::from_config`, handed over through
//! [`PromptRenderer::providers`]). Nothing here calls `config::*`.

pub mod instant;
pub mod oneshot;
pub mod watch;
pub mod worker;

use crate::model::{Freshness, PromptData, PromptRequest, RenderedPrompt, Slot};
use crate::providers::{slot_target, Provider, ProviderValue, SlotTarget};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, BufRead, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How often the worker checks that the parent shell still exists.
pub const PARENT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Worker epochs are in `[1, 2^53)`: 0 is reserved for one-shot mode and values
/// stay exact for JSON readers (jq, JavaScript) and fit Zsh integers.
pub const EPOCH_LIMIT: u64 = 1 << 53;

/// A random, non-zero worker identity (from /dev/urandom, falling back to the
/// randomly seeded std hasher).
pub fn new_worker_epoch() -> u64 {
    let mut buf = [0u8; 8];
    let raw = match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => u64::from_le_bytes(buf),
        Err(_) => fallback_random(),
    };
    raw % (EPOCH_LIMIT - 1) + 1
}

fn fallback_random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(d.as_nanos());
    }
    h.finish()
}

/// True while a process with this pid exists (`kill(pid, 0)`; EPERM counts as
/// alive). Pid 0 and values outside `pid_t` are never "alive" so a bogus
/// `--parent-pid` cannot address a process group.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence/permission check.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// `$XDG_STATE_HOME` (absolute only, per the XDG spec) else `$HOME/.local/state`.
pub fn state_dir_with(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(x) = xdg_state_home.map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Some(x);
    }
    home.map(PathBuf::from).filter(|p| p.is_absolute()).map(|h| h.join(".local/state"))
}

/// Directory holding per-session snapshots.
pub fn sessions_dir_with(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    state_dir_with(xdg_state_home, home).map(|d| d.join("tildy").join("sessions"))
}

pub fn sessions_dir() -> Option<PathBuf> {
    sessions_dir_with(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

/// `<state>/tildy/sessions/<session_id>.json` from the process environment.
pub fn snapshot_path(session_id: &str) -> Option<PathBuf> {
    sessions_dir().map(|d| d.join(snapshot_file_name(session_id)))
}

/// File name for a session snapshot. The session id comes from the shell, so
/// anything outside `[A-Za-z0-9._-]` becomes `_` and a leading `.` is replaced:
/// the result can never traverse directories or be hidden.
pub fn snapshot_file_name(session_id: &str) -> String {
    let mut name: String = session_id
        .chars()
        .take(128)
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    if name.is_empty() || name.starts_with('.') {
        name.insert(0, '_');
    }
    name.push_str(".json");
    name
}

/// Rendering as seen by the backend cores. The real implementation wraps
/// `config` + `render::render` (src/cli/worker.rs); tests pass closures.
pub trait PromptRenderer: Send {
    /// Called once per request before rendering and before the request is
    /// scheduled (the worker reloads the config file here when it changed).
    fn refresh(&mut self) {}
    /// Resolve context-sensitive layers against the request directory.
    fn refresh_for(&mut self, _cwd: &std::path::Path) { self.refresh(); }
    /// Changes whenever the effective config changes in a way that may change
    /// the provider set (0: no config loaded yet). The worker calls
    /// [`providers`](Self::providers) after `refresh` when this differs from the
    /// value it last saw.
    fn config_epoch(&self) -> u64 {
        0
    }
    /// Providers for the current config, replacing the set the core was
    /// started with. `None` keeps the current set (the default: tests and
    /// callers that pass providers directly).
    fn providers(&mut self) -> Option<Vec<Arc<dyn Provider>>> {
        None
    }
    fn render(&mut self, data: &PromptData) -> RenderedPrompt;
    /// Reports from the most recent render, without repeating provider work.
    fn reports(&self) -> Vec<crate::model::ModuleReport> {
        Vec::new()
    }
    /// Config revision for diagnostics.
    fn revision(&self) -> Option<String> {
        None
    }
    /// Degraded-mode reasons for diagnostics (e.g. invalid config file).
    fn degraded(&self) -> Vec<String> {
        Vec::new()
    }
}

impl<F> PromptRenderer for F
where
    F: FnMut(&PromptData) -> RenderedPrompt + Send,
{
    fn render(&mut self, data: &PromptData) -> RenderedPrompt {
        self(data)
    }
}

/// One provider's contribution to a render, after scheduling.
#[derive(Debug, Clone)]
pub struct ProviderSlot {
    pub key: &'static str,
    pub freshness: Freshness,
    pub age_ms: Option<u64>,
    pub value: Option<ProviderValue>,
    pub error: Option<String>,
}

/// Build the renderer input from the request and provider slots. Slots with a
/// value are mapped by the value's variant; slots without one (nothing cached
/// yet, or a timeout/error with no earlier value) by the provider key
/// ([`slot_target`]): the git slot and module slots then still carry
/// `pending`/`timeout`/`error`, so an unfinished lookup never reads as "clean"
/// or "not detected".
pub fn assemble(req: &PromptRequest, home: Option<PathBuf>, slots: &[ProviderSlot]) -> PromptData {
    let mut data = PromptData {
        request: req.clone(),
        home,
        git: Slot::pending(),
        ssh: false,
        hostname: None,
        username: None,
        modules: BTreeMap::new(),
        changes: None,
    };
    for s in slots {
        match &s.value {
            Some(ProviderValue::Git(inner)) => data.git = merge_slot(inner, s),
            Some(ProviderValue::Ssh(b)) => data.ssh = *b,
            Some(ProviderValue::Hostname(h)) => data.hostname = h.clone(),
            Some(ProviderValue::Username(u)) => data.username = u.clone(),
            Some(ProviderValue::Module(id, inner)) => {
                data.modules.insert(id.clone(), merge_slot(inner, s));
            }
            None => match slot_target(s.key) {
                SlotTarget::Git => data.git = empty_slot(s),
                SlotTarget::Module(id) => {
                    data.modules.insert(id.to_string(), empty_slot(s));
                }
                SlotTarget::Session => {}
            },
        }
    }
    data
}

/// Slot for a provider with no value: `pending` until a fetch produced one.
fn empty_slot<T>(s: &ProviderSlot) -> Slot<T> {
    let freshness = match s.freshness {
        Freshness::Fresh | Freshness::Stale => Freshness::Pending,
        f => f,
    };
    Slot { freshness, age_ms: None, value: None, error: s.error.clone() }
}

/// Combine the provider's own slot (which may already say Timeout/Error, e.g.
/// git killed after its timeout, a version command that timed out) with the
/// scheduler's view of the cache entry: `stale` once older than the TTL, and
/// an outer timeout/error keeps the cached value for display.
fn merge_slot<T: Clone>(inner: &Slot<T>, s: &ProviderSlot) -> Slot<T> {
    let mut g = inner.clone();
    match s.freshness {
        Freshness::Fresh => {
            if g.freshness == Freshness::Fresh {
                g.age_ms = s.age_ms;
            }
        }
        Freshness::Stale => {
            if g.freshness == Freshness::Fresh {
                g.freshness = Freshness::Stale;
            }
            g.age_ms = s.age_ms;
        }
        Freshness::Timeout | Freshness::Error => {
            g.freshness = s.freshness;
            g.age_ms = s.age_ms;
            g.error = s.error.clone().or(g.error);
        }
        Freshness::Pending => {}
    }
    g
}

/// One line of input read with a size bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputLine {
    Line(Vec<u8>),
    /// The line exceeded the bound; it was consumed up to its newline.
    TooLong,
}

/// Read one `\n`-terminated line (the newline is stripped) holding at most
/// `max` bytes in memory. Returns `Ok(None)` at EOF with no pending data; a
/// final unterminated line is returned as a line.
pub fn read_line_bounded<R: BufRead>(r: &mut R, max: usize) -> io::Result<Option<InputLine>> {
    let mut buf = Vec::new();
    let mut too_long = false;
    let mut saw_any = false;
    loop {
        let available = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            if !saw_any {
                return Ok(None);
            }
            break;
        }
        saw_any = true;
        let newline = available.iter().position(|&b| b == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];
        if !too_long {
            if buf.len() + chunk.len() > max {
                too_long = true;
                buf = Vec::new();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        let consumed = newline.map_or(available.len(), |i| i + 1);
        r.consume(consumed);
        if newline.is_some() {
            break;
        }
    }
    Ok(Some(if too_long { InputLine::TooLong } else { InputLine::Line(buf) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GitData, GitState, ModuleValue};
    use std::io::{BufReader, Cursor};

    #[test]
    fn epochs_are_random_nonzero_and_json_safe() {
        let a = new_worker_epoch();
        let b = new_worker_epoch();
        assert_ne!(a, b);
        for e in [a, b, fallback_random() % (EPOCH_LIMIT - 1) + 1] {
            assert!((1..EPOCH_LIMIT).contains(&e));
        }
    }

    #[test]
    fn pid_alive_tracks_process_lifetime() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX));
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let pid = child.id();
        assert!(pid_alive(pid));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!pid_alive(pid));
    }

    #[test]
    fn state_dir_resolution() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(state_dir_with(os("/x/state"), os("/home/u")), Some(PathBuf::from("/x/state")));
        assert_eq!(state_dir_with(os("relative"), os("/home/u")), Some(PathBuf::from("/home/u/.local/state")));
        assert_eq!(state_dir_with(os(""), os("/home/u")), Some(PathBuf::from("/home/u/.local/state")));
        assert_eq!(state_dir_with(None, None), None);
        assert_eq!(state_dir_with(None, os("rel")), None);
        assert_eq!(
            sessions_dir_with(os("/s"), None).unwrap().join(snapshot_file_name("123.456")),
            PathBuf::from("/s/tildy/sessions/123.456.json")
        );
    }

    #[test]
    fn snapshot_file_names_are_safe() {
        assert_eq!(snapshot_file_name("4242.99"), "4242.99.json");
        assert_eq!(snapshot_file_name("../../etc/passwd"), "_.._.._etc_passwd.json");
        assert_eq!(snapshot_file_name(".."), "_...json");
        assert_eq!(snapshot_file_name(""), "_.json");
        assert_eq!(snapshot_file_name("a/b\tc é"), "a_b_c__.json");
        assert_eq!(snapshot_file_name(&"x".repeat(500)).len(), 128 + 5);
    }

    #[test]
    fn bounded_reader_splits_lines_and_survives_overlong_ones() {
        let data = format!("REQ\t1\n{}\nBYE\nlast", "y".repeat(100));
        let mut r = BufReader::with_capacity(7, Cursor::new(data.into_bytes()));
        let mut got = Vec::new();
        while let Some(l) = read_line_bounded(&mut r, 50).unwrap() {
            got.push(l);
        }
        assert_eq!(
            got,
            vec![
                InputLine::Line(b"REQ\t1".to_vec()),
                InputLine::TooLong,
                InputLine::Line(b"BYE".to_vec()),
                InputLine::Line(b"last".to_vec()),
            ]
        );
        let mut empty = Cursor::new(Vec::new());
        assert_eq!(read_line_bounded(&mut empty, 10).unwrap(), None);
        let mut blank = Cursor::new(b"\n".to_vec());
        assert_eq!(read_line_bounded(&mut blank, 10).unwrap(), Some(InputLine::Line(vec![])));
    }

    fn req() -> PromptRequest {
        PromptRequest {
            gen: 1,
            session_id: "s".into(),
            cwd: "/r".into(),
            cols: 80,
            lines: 24,
            last_exit: 0,
            last_duration_ms: 0,
            env: String::new(),
            jobs: 0,
        }
    }

    fn git(branch: &str) -> Slot<GitData> {
        Slot::fresh(GitData {
            in_repo: true,
            branch: Some(branch.into()),
            state: GitState::Clean,
            ..GitData::default()
        })
    }

    fn module(text: &str) -> Slot<ModuleValue> {
        Slot::fresh(ModuleValue { text: text.into(), ..ModuleValue::default() })
    }

    fn slot(key: &'static str, freshness: Freshness, value: Option<ProviderValue>) -> ProviderSlot {
        ProviderSlot { key, freshness, age_ms: Some(7), value, error: None }
    }

    #[test]
    fn assemble_maps_values_and_freshness() {
        let slots = vec![
            slot("git", Freshness::Stale, Some(ProviderValue::Git(git("main")))),
            slot("session/ssh", Freshness::Fresh, Some(ProviderValue::Ssh(true))),
            slot("session/hostname", Freshness::Stale, Some(ProviderValue::Hostname(Some("box".into())))),
            slot("session/username", Freshness::Pending, None),
        ];
        let d = assemble(&req(), Some("/home/u".into()), &slots);
        assert_eq!(d.git.freshness, Freshness::Stale);
        assert_eq!(d.git.age_ms, Some(7));
        assert_eq!(d.git.value.as_ref().unwrap().branch.as_deref(), Some("main"));
        assert!(d.ssh);
        assert_eq!(d.hostname.as_deref(), Some("box"));
        assert_eq!(d.username, None);
        assert_eq!(d.home, Some(PathBuf::from("/home/u")));
        assert!(d.modules.is_empty());
    }

    #[test]
    fn assemble_git_without_value_and_provider_reported_timeout() {
        let d = assemble(&req(), None, &[slot("git", Freshness::Pending, None)]);
        assert_eq!(d.git, Slot::pending());
        let mut t = slot("git", Freshness::Timeout, None);
        t.error = Some("no result within 1500 ms".into());
        let d = assemble(&req(), None, &[t]);
        assert_eq!((d.git.freshness, d.git.error.as_deref()), (Freshness::Timeout, Some("no result within 1500 ms")));
        // The provider itself reported a timeout: a cache hit keeps it.
        let inner = Slot::<GitData> {
            freshness: Freshness::Timeout,
            age_ms: Some(0),
            value: None,
            error: Some("git killed".into()),
        };
        let d = assemble(&req(), None, &[slot("git", Freshness::Fresh, Some(ProviderValue::Git(inner)))]);
        assert_eq!((d.git.freshness, d.git.error.as_deref()), (Freshness::Timeout, Some("git killed")));
        // A provider key that merely starts with "git" is a module, not git.
        let d = assemble(&req(), None, &[slot("github", Freshness::Pending, None)]);
        assert_eq!(d.git, Slot::pending());
        assert_eq!(d.modules["github"], Slot::pending());
        // Outer timeout with a stale cached value keeps the value for display.
        let mut t = slot("git", Freshness::Timeout, Some(ProviderValue::Git(git("dev"))));
        t.error = Some("late".into());
        let d = assemble(&req(), None, &[t]);
        assert_eq!(d.git.freshness, Freshness::Timeout);
        assert_eq!(d.git.value.unwrap().branch.as_deref(), Some("dev"));
    }

    #[test]
    fn assemble_module_slots_merge_like_git() {
        // No value yet: pending while fetching, then the provider's verdict.
        let d = assemble(&req(), None, &[slot("node", Freshness::Pending, None)]);
        assert_eq!(d.modules["node"], Slot::pending());
        let mut t = slot("java", Freshness::Timeout, None);
        t.error = Some("no result within 500 ms".into());
        let d = assemble(&req(), None, &[t]);
        assert_eq!(d.modules["java"].freshness, Freshness::Timeout);
        // Cached value, revalidating within TTL: fresh with the cache age.
        let d = assemble(&req(), None, &[slot("node", Freshness::Fresh, Some(ProviderValue::Module("node".into(), module("22.3.0"))))]);
        assert_eq!((d.modules["node"].freshness, d.modules["node"].age_ms), (Freshness::Fresh, Some(7)));
        // Older than the TTL: stale, value kept.
        let d = assemble(&req(), None, &[slot("node", Freshness::Stale, Some(ProviderValue::Module("node".into(), module("22.3.0"))))]);
        assert_eq!(d.modules["node"].freshness, Freshness::Stale);
        assert_eq!(d.modules["node"].value.as_ref().unwrap().text, "22.3.0");
        // Not detected stays a fresh slot without value.
        let none = Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: None, error: None };
        let d = assemble(&req(), None, &[slot("rust", Freshness::Fresh, Some(ProviderValue::Module("rust".into(), none)))]);
        assert_eq!((d.modules["rust"].freshness, d.modules["rust"].value.as_ref()), (Freshness::Fresh, None));
        // The provider's own timeout (version command killed) survives a cache hit.
        let killed = Slot::<ModuleValue> { freshness: Freshness::Timeout, age_ms: None, value: None, error: Some("killed".into()) };
        let d = assemble(&req(), None, &[slot("java", Freshness::Fresh, Some(ProviderValue::Module("java".into(), killed)))]);
        assert_eq!((d.modules["java"].freshness, d.modules["java"].error.as_deref()), (Freshness::Timeout, Some("killed")));
    }
}
