//! `agent_status` provider: live agent status records for the prompt's
//! session, worktree and the global scope. Owner: M2 task B.
//!
//! Each fetch reads the three scope directories of the [`Store`] (no process
//! when the worktree identity of the cwd is cached; see [`WorktreeCache`])
//! and summarizes the live records into one `ModuleValue`:
//!
//! * `count`, and per state `blocked`, `error`, `done`, `working`, `idle`;
//! * the highlighted record (blocked > error > done > working > idle, newest
//!   first): `state`, `kind`, `app`, `title`, `msg`, `progress`, `id`,
//!   `age_s`, `scope`;
//! * `more`: the number of other records.
//!
//! No live record: a `Fresh` slot without a value (the module hides as
//! `not_detected`).

use super::{intern, Provider, ProviderValue};
use crate::agent::{Filter, Scope, Store, Stored};
use crate::model::{Dependency, Freshness, ModuleValue, PromptRequest, ProviderSpec, Slot};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEPS: &[Dependency] = &[Dependency::Cwd, Dependency::Env];
pub const TTL: Duration = Duration::from_secs(1);
pub const TIMEOUT: Duration = Duration::from_millis(1500);
/// How long a cwd's worktree identity is reused before `git rev-parse` runs again.
pub const WORKTREE_CACHE_TTL: Duration = Duration::from_secs(10);
const WORKTREE_CACHE_MAX: usize = 128;

/// Per-cwd cache of `git::worktree_identity`. A cwd with no `.git` entry in
/// any ancestor directory is answered without running git.
#[derive(Default)]
pub struct WorktreeCache {
    entries: Mutex<HashMap<PathBuf, (Option<String>, Instant)>>,
}

impl WorktreeCache {
    pub fn get(&self, cwd: &Path) -> Option<String> {
        let now = Instant::now();
        if let Some((id, at)) = self.lock().get(cwd) {
            if now.duration_since(*at) < WORKTREE_CACHE_TTL {
                return id.clone();
            }
        }
        let id = if has_git_ancestor(cwd) { super::git::worktree_identity(cwd) } else { None };
        let mut m = self.lock();
        if m.len() >= WORKTREE_CACHE_MAX {
            m.retain(|_, (_, at)| now.duration_since(*at) < WORKTREE_CACHE_TTL);
            if m.len() >= WORKTREE_CACHE_MAX {
                m.clear();
            }
        }
        m.insert(cwd.to_path_buf(), (id.clone(), now));
        id
    }

    /// The cached identity of `cwd` whatever its age, without running git;
    /// `None` when `cwd` was never looked up (or is not in a worktree).
    pub fn peek(&self, cwd: &Path) -> Option<String> {
        self.lock().get(cwd).and_then(|(id, _)| id.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, (Option<String>, Instant)>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Some ancestor of `cwd` (itself included) has a `.git` entry, or `GIT_DIR`
/// is set: only then can `cwd` be inside a worktree.
fn has_git_ancestor(cwd: &Path) -> bool {
    std::env::var_os("GIT_DIR").is_some_and(|v| !v.is_empty()) || cwd.ancestors().any(|d| d.join(".git").symlink_metadata().is_ok())
}

/// The scopes a prompt shows: its session, its worktree (if any), global.
pub fn prompt_scopes(session_id: &str, worktree: Option<String>) -> Vec<Scope> {
    let mut v = Vec::with_capacity(3);
    if !session_id.is_empty() {
        v.push(Scope::session(session_id));
    }
    if let Some(w) = worktree {
        v.push(Scope::worktree(w));
    }
    v.push(Scope::global());
    v
}

/// One `agent_status` module instance; its provider key is the module id
/// (like every config module), and every instance reads the same records.
pub struct AgentStatusProvider {
    id: String,
    spec: ProviderSpec,
    store: Option<Store>,
    worktrees: WorktreeCache,
}

impl AgentStatusProvider {
    /// Reads the store of the process environment (`Store::open`).
    pub fn new(id: &str) -> Self {
        Self::with_store(id, Store::open().ok())
    }

    pub fn with_store(id: &str, store: Option<Store>) -> Self {
        AgentStatusProvider {
            id: id.to_string(),
            spec: ProviderSpec { key: intern(id), deps: DEPS, ttl: TTL, timeout: TIMEOUT },
            store,
            worktrees: WorktreeCache::default(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    /// The scopes shown for `req` (session, worktree of the cwd, global).
    pub fn scopes(&self, req: &PromptRequest) -> Vec<Scope> {
        prompt_scopes(&req.session_id, self.worktrees.get(&req.cwd))
    }

    /// Like [`scopes`](Self::scopes) but never runs git: the worktree comes
    /// from the cache filled by earlier fetches (the worker's watch list).
    pub fn watch_scopes(&self, req: &PromptRequest) -> Vec<Scope> {
        prompt_scopes(&req.session_id, self.worktrees.peek(&req.cwd))
    }

    pub fn fetch_slot(&self, req: &PromptRequest) -> Slot<ModuleValue> {
        let Some(store) = &self.store else {
            return Slot { freshness: Freshness::Error, age_ms: None, value: None, error: Some("no state directory (HOME/XDG_STATE_HOME unset)".into()) };
        };
        let now = crate::agent::now_ms();
        let records = store.list(&Filter { now_ms: Some(now), ..Filter::live(self.scopes(req)) });
        Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: summarize(&records, now), error: None }
    }
}

impl Provider for AgentStatusProvider {
    fn spec(&self) -> &ProviderSpec {
        &self.spec
    }
    fn fetch(&self, req: &PromptRequest) -> ProviderValue {
        ProviderValue::Module(self.id.clone(), self.fetch_slot(req))
    }
    fn as_agent_status(&self) -> Option<&AgentStatusProvider> {
        Some(self)
    }
}

/// The module value for records in display order (`Store::list` order:
/// the first one is highlighted). `None` when there are none.
pub fn summarize(records: &[Stored], now_ms: u64) -> Option<ModuleValue> {
    let top = records.first()?;
    let r = &top.record;
    let mut f = BTreeMap::new();
    f.insert("count".to_string(), records.len().to_string());
    for st in crate::agent::State::ALL {
        let n = records.iter().filter(|s| s.record.state == st).count();
        f.insert(st.as_str().to_string(), n.to_string());
    }
    f.insert("more".into(), (records.len() - 1).to_string());
    f.insert("state".into(), r.state.as_str().into());
    f.insert("id".into(), r.id.clone());
    f.insert("scope".into(), r.scope.kind.as_str().into());
    let age = now_ms.saturating_sub(r.received_at_ms.unwrap_or(now_ms)) / 1000;
    f.insert("age_s".into(), age.to_string());
    if let Some(k) = r.kind {
        f.insert("kind".into(), k.as_str().into());
    }
    if let Some(a) = top.app() {
        f.insert("app".into(), a.into());
    }
    if let Some(t) = &r.title {
        f.insert("title".into(), t.clone());
    }
    if let Some(m) = &r.msg {
        f.insert("msg".into(), m.clone());
    }
    if let Some(p) = r.progress {
        f.insert("progress".into(), p.to_string());
    }
    let text = r.msg.clone().or_else(|| r.title.clone()).unwrap_or_else(|| r.state.as_str().to_string());
    Some(ModuleValue { text, fields: f, prerendered: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Kind, Record, State};

    fn req(session: &str, cwd: &Path) -> PromptRequest {
        PromptRequest {
            gen: 1,
            session_id: session.into(),
            cwd: cwd.to_path_buf(),
            cols: 80,
            lines: 24,
            last_exit: 0,
            last_duration_ms: 0,
            env: String::new(),
            jobs: 0,
        }
    }

    #[test]
    fn spec_and_scopes() {
        let td = tempfile::tempdir().unwrap();
        let p = AgentStatusProvider::with_store("agent", Some(Store::open_with(td.path())));
        assert_eq!((p.spec().key, p.spec().ttl), ("agent", Duration::from_secs(1)));
        assert_eq!(p.spec().deps, &[Dependency::Cwd, Dependency::Env]);
        let plain = td.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(p.scopes(&req("S", &plain)), vec![Scope::session("S"), Scope::global()]);
        assert_eq!(p.scopes(&req("", &plain)), vec![Scope::global()]);
        std::fs::create_dir(plain.join(".git")).unwrap();
        assert!(has_git_ancestor(&plain.join("x/y")));
    }

    #[test]
    fn no_records_is_not_detected_and_records_summarize() {
        let td = tempfile::tempdir().unwrap();
        let st = Store::open_with(td.path());
        let p = AgentStatusProvider::with_store("agent", Some(st.clone()));
        let cwd = td.path().to_path_buf();
        let slot = p.fetch_slot(&req("S", &cwd));
        assert_eq!((slot.freshness, slot.value), (Freshness::Fresh, None));

        let mut w = Record::new(State::Working, Scope::global());
        w.id = "build".into();
        w.app = Some("cargo".into());
        w.progress = Some(40);
        st.publish(w).unwrap();
        let mut b = Record::new(State::Blocked, Scope::session("S"));
        b.id = "wrap/claude/9".into();
        b.app = Some("claude".into());
        b.kind = Some(Kind::Permission);
        b.msg = Some("Bash: rm -rf target".into());
        b.title = Some("tildy".into());
        st.publish(b).unwrap();
        // Other sessions' and expired records do not count.
        st.publish(Record::new(State::Error, Scope::session("OTHER"))).unwrap();
        let v = p.fetch_slot(&req("S", &cwd)).value.unwrap();
        let get = |k: &str| v.fields.get(k).map(String::as_str);
        assert_eq!(get("state"), Some("blocked"));
        assert_eq!((get("kind"), get("app"), get("msg"), get("title")), (Some("permission"), Some("claude"), Some("Bash: rm -rf target"), Some("tildy")));
        assert_eq!((get("count"), get("more"), get("blocked"), get("working"), get("error")), (Some("2"), Some("1"), Some("1"), Some("1"), Some("0")));
        assert_eq!((get("id"), get("scope"), get("age_s"), get("progress")), (Some("wrap/claude/9"), Some("session"), Some("0"), None));
        assert_eq!(v.text, "Bash: rm -rf target");
        // Without the session, only the global working record shows.
        let v = p.fetch_slot(&req("T", &cwd)).value.unwrap();
        assert_eq!((v.fields["state"].as_str(), v.fields["progress"].as_str(), v.fields["more"].as_str()), ("working", "40", "0"));
        assert_eq!(v.text, "working");
    }

    #[test]
    fn missing_store_is_an_error_slot() {
        let p = AgentStatusProvider::with_store("a", None);
        let s = p.fetch_slot(&req("S", Path::new("/")));
        assert_eq!(s.freshness, Freshness::Error);
        assert!(s.value.is_none());
    }
}
