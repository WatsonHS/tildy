//! On-disk store of agent status records.
//!
//! Layout: `<state>/tildy/agent/<scope kind>/<scope-hash>/<id-hash>.json`,
//! where `<state>` is `$XDG_STATE_HOME` (absolute) or `~/.local/state`, and
//! both hashes are FNV-1a 64 in hex (`config::content_revision`) of the scope
//! id and the record id. Directories are created 0700, files 0600. A record
//! is replaced whole by an atomic rename, so readers never see a partial file
//! and never lock; writers (publish, clear, drop_on_prompt, expire) serialize
//! per scope directory with `flock` on the directory.
//!
//! The directory of a scope changes its mtime whenever a record in it is
//! created, replaced or removed: that is what the worker watches.

use super::record::{id_within, Record, Reject, Scope, ScopeKind};
use crate::config::content_revision;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Records kept per scope; beyond it the least recently updated live records
/// are removed (7501 caps a terminal at 256 records).
pub const MAX_RECORDS_PER_SCOPE: usize = 256;
/// Larger record files are ignored (a valid record is well under 8 KB).
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// `expire` removes empty scope directories not modified for this long.
const EMPTY_DIR_GRACE: Duration = Duration::from_secs(3600);
/// Leftover temp files older than this are removed by `expire`.
const TMP_GRACE: Duration = Duration::from_secs(60);

/// Wall clock in unix milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A record as stored, with where it lives and when it expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stored {
    #[serde(flatten)]
    pub record: Record,
    pub path: PathBuf,
    /// `received_at_ms + ttl_s * 1000`.
    pub expires_at_ms: u64,
    /// The `app` of the nearest ancestor record in the same scope, when the
    /// record has none of its own (7501 inheritance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherited_app: Option<String>,
}

impl Stored {
    /// The record's app, or the inherited one.
    pub fn app(&self) -> Option<&str> {
        self.record.app.as_deref().or(self.inherited_app.as_deref())
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms <= now_ms
    }
}

/// Which records `list` returns and `clear` removes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Only these scopes; empty means every scope in the store.
    pub scopes: Vec<Scope>,
    /// Only this record and its descendants (`id`, `id/…`); `Some("")` is the
    /// root record and therefore everything.
    pub id: Option<String>,
    /// Only records whose app (own or inherited) is this.
    pub app: Option<String>,
    /// `Some(false)`: live records only; `Some(true)`: expired only; `None`: both.
    pub expired: Option<bool>,
    /// Time for the expiry test (default: now).
    pub now_ms: Option<u64>,
}

impl Filter {
    /// Live records of these scopes (every scope when empty).
    pub fn live(scopes: Vec<Scope>) -> Filter {
        Filter { scopes, expired: Some(false), ..Filter::default() }
    }

    fn matches(&self, s: &Stored, now: u64) -> bool {
        (self.scopes.is_empty() || self.scopes.contains(&s.record.scope))
            && self.id.as_deref().is_none_or(|id| id_within(&s.record.id, id))
            && self.app.as_deref().is_none_or(|a| s.app() == Some(a))
            && self.expired.is_none_or(|e| s.is_expired(now) == e)
    }
}

/// The agent status store. Cheap to clone; holds only its root path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    root: PathBuf,
}

struct DirLock {
    _file: fs::File,
}

fn io_reject(what: &str, path: &Path, e: io::Error) -> Reject {
    Reject::Io(format!("cannot {what} {}: {e}", path.display()))
}

impl Store {
    /// The store under `$XDG_STATE_HOME` (absolute) or `$HOME/.local/state`.
    /// Nothing is created until the first publish.
    pub fn open() -> io::Result<Store> {
        crate::session::state_dir_with(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
            .map(Store::open_with)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot resolve the state directory (set HOME or XDG_STATE_HOME)"))
    }

    /// The store under `state_dir` (an XDG state home such as
    /// `~/.local/state`): records live in `<state_dir>/tildy/agent/`.
    pub fn open_with(state_dir: impl AsRef<Path>) -> Store {
        Store { root: state_dir.as_ref().join("tildy").join("agent") }
    }

    /// `<state>/tildy/agent`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding the records of `scope` (it may not exist yet). Its
    /// mtime changes on every publish, clear or drop in that scope.
    pub fn scope_dir(&self, scope: &Scope) -> PathBuf {
        self.root.join(scope.kind.as_str()).join(content_revision(scope.id.as_bytes()))
    }

    fn record_path(&self, scope: &Scope, id: &str) -> PathBuf {
        self.scope_dir(scope).join(format!("{}.json", content_revision(id.as_bytes())))
    }

    /// Store a record, replacing the one with the same scope and id.
    ///
    /// The record is normalized (`kind` only with blocked, `progress` only
    /// with working/blocked) and validated against the 7501 limits; then, under
    /// the scope's lock: a live record with the same id must have a smaller
    /// `seq` (else [`Reject::StaleSeq`]); `seq` 0 becomes previous + 1 (1 for
    /// a new id); `ttl_s` is resolved, `received_at_ms` set to now and
    /// `prompt_seen` cleared. Expired records of the scope are removed and the
    /// scope is capped at [`MAX_RECORDS_PER_SCOPE`].
    pub fn publish(&self, mut record: Record) -> Result<Stored, Reject> {
        record.normalize();
        record.validate()?;
        let dir = self.scope_dir(&record.scope);
        let path = self.record_path(&record.scope, &record.id);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let lock = match lock_dir(&dir, true) {
                Ok(l) => l,
                Err(e) if e.kind() == io::ErrorKind::NotFound && attempt < 3 => continue,
                Err(e) => return Err(io_reject("create", &dir, e)),
            };
            let now = now_ms();
            let mut rec = record.clone();
            if let Some(prev) = read_record(&path).filter(|p| p.scope == rec.scope && p.id == rec.id) {
                let live = prev.expires_at_ms() > now;
                if rec.seq == 0 {
                    rec.seq = prev.seq.saturating_add(1);
                } else if live && rec.seq <= prev.seq {
                    return Err(Reject::StaleSeq { id: rec.id.clone(), current: prev.seq, got: rec.seq });
                }
            } else if rec.seq == 0 {
                rec.seq = 1;
            }
            rec.ttl_s = Some(rec.ttl());
            rec.received_at_ms = Some(now);
            rec.prompt_seen = false;
            match write_record(&path, &rec) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound && attempt < 3 => {
                    // The (empty) scope directory was removed by `expire`
                    // between our mkdir and the write: start over.
                    drop(lock);
                    continue;
                }
                Err(e) => return Err(io_reject("write", &path, e)),
            }
            let others = scan_dir(&dir);
            housekeep(&others, &path, now);
            let inherited = inherited_app(&rec, others.iter().map(|(_, r)| r));
            drop(lock);
            return Ok(Stored { expires_at_ms: rec.expires_at_ms(), record: rec, path, inherited_app: inherited });
        }
    }

    /// Records matching `filter`, highest display precedence first (blocked >
    /// error > done > working > idle), newest first within a state.
    pub fn list(&self, filter: &Filter) -> Vec<Stored> {
        let now = filter.now_ms.unwrap_or_else(now_ms);
        let mut out: Vec<Stored> = self.dirs_for(&filter.scopes).iter().flat_map(|d| stored_in(d)).filter(|s| filter.matches(s, now)).collect();
        sort_for_display(&mut out);
        out
    }

    /// Remove the records matching `filter` (each scope under its lock) and
    /// return them. An empty filter removes everything.
    pub fn clear(&self, filter: &Filter) -> Result<Vec<Stored>, Reject> {
        let now = filter.now_ms.unwrap_or_else(now_ms);
        let mut removed = vec![];
        for dir in self.dirs_for(&filter.scopes) {
            if !stored_in(&dir).iter().any(|s| filter.matches(s, now)) {
                continue;
            }
            let lock = match lock_dir(&dir, false) {
                Ok(l) => l,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_reject("lock", &dir, e)),
            };
            for s in stored_in(&dir).into_iter().filter(|s| filter.matches(s, now)) {
                match fs::remove_file(&s.path) {
                    Ok(()) => removed.push(s),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_reject("remove", &s.path, e)),
                }
            }
            drop(lock);
        }
        sort_for_display(&mut removed);
        Ok(removed)
    }

    /// Remove exactly one scope/id under its scope lock. Descendants survive.
    pub fn remove(&self, scope: &Scope, id: &str) -> Result<Option<Stored>, Reject> {
        super::record::check_id(id).map_err(|message| Reject::Invalid { field: "id".into(), message })?;
        let dir = self.scope_dir(scope);
        let _lock = match lock_dir(&dir, false) {
            Ok(lock) => lock,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_reject("lock", &dir, e)),
        };
        let Some(found) = stored_in(&dir).into_iter().find(|s| s.record.scope == *scope && s.record.id == id) else {
            return Ok(None);
        };
        match fs::remove_file(&found.path) {
            Ok(()) => Ok(Some(found)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_reject("remove", &found.path, e)),
        }
    }

    /// The shell session `origin_session` is showing a new prompt (7501's
    /// OSC 133 rule): its idle/working/blocked records are removed; its
    /// done/error records are marked `prompt_seen` the first time and removed
    /// the second time, so each is shown at least once. Expired records of
    /// that origin go too. Returns the number of records removed.
    pub fn drop_on_prompt(&self, origin_session: &str) -> usize {
        if origin_session.is_empty() {
            return 0;
        }
        let now = now_ms();
        let mine = |r: &Record| r.origin_session.as_deref() == Some(origin_session);
        let mut removed = 0;
        for dir in self.dirs_for(&[]) {
            if !scan_dir(&dir).iter().any(|(_, r)| mine(r)) {
                continue;
            }
            let Ok(lock) = lock_dir(&dir, false) else { continue };
            for (path, r) in scan_dir(&dir).into_iter().filter(|(_, r)| mine(r)) {
                if r.state.is_final() && !r.prompt_seen && r.expires_at_ms() > now {
                    let mut seen = r;
                    seen.prompt_seen = true;
                    let _ = write_record(&path, &seen);
                } else if fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
            drop(lock);
        }
        removed
    }

    /// Remove records whose TTL ran out by `now_ms`, unreadable record files,
    /// stale temp files and empty scope directories left for an hour. Returns
    /// the number of record files removed.
    pub fn expire(&self, now_ms: u64) -> usize {
        let mut removed = 0;
        for dir in self.dirs_for(&[]) {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            let mut doomed = vec![];
            let mut any = false;
            for e in entries.flatten() {
                any = true;
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    if name.ends_with(".tmp") && older_than(&p, TMP_GRACE) {
                        let _ = fs::remove_file(&p);
                    }
                    continue;
                }
                if !name.ends_with(".json") {
                    continue;
                }
                match read_record(&p) {
                    Some(r) if r.expires_at_ms() > now_ms => {}
                    // Unreadable files are left a moment in case a writer of a
                    // newer tildy produced them; ours are always valid.
                    None if !older_than(&p, TMP_GRACE) => {}
                    _ => doomed.push(p),
                }
            }
            if !doomed.is_empty() {
                if let Ok(lock) = lock_dir(&dir, false) {
                    for p in doomed {
                        // Re-check under the lock: it may have been replaced.
                        let still = read_record(&p).is_none_or(|r| r.expires_at_ms() <= now_ms);
                        if still && fs::remove_file(&p).is_ok() {
                            removed += 1;
                        }
                    }
                    drop(lock);
                }
            } else if !any && older_than(&dir, EMPTY_DIR_GRACE) {
                let _ = fs::remove_dir(&dir);
            }
        }
        removed
    }

    /// Earliest `expires_at_ms` over every stored record (possibly in the
    /// past for a record `expire` has not removed yet).
    pub fn next_expiry(&self) -> Option<u64> {
        self.dirs_for(&[]).iter().flat_map(|d| scan_dir(d)).map(|(_, r)| r.expires_at_ms()).min()
    }

    /// Scope directories to read: the given scopes' (deduplicated), or every
    /// existing one.
    fn dirs_for(&self, scopes: &[Scope]) -> Vec<PathBuf> {
        if !scopes.is_empty() {
            let mut v: Vec<PathBuf> = scopes.iter().map(|s| self.scope_dir(s)).collect();
            v.dedup();
            let mut seen = std::collections::HashSet::new();
            v.retain(|p| seen.insert(p.clone()));
            return v;
        }
        let mut out = vec![];
        for kind in ScopeKind::ALL {
            let Ok(rd) = fs::read_dir(self.root.join(kind.as_str())) else { continue };
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    out.push(e.path());
                }
            }
        }
        out.sort();
        out
    }
}

fn sort_for_display(v: &mut [Stored]) {
    v.sort_by(|a, b| {
        b.record
            .state
            .rank()
            .cmp(&a.record.state.rank())
            .then(b.record.received_at_ms.cmp(&a.record.received_at_ms))
            .then(a.record.id.cmp(&b.record.id))
    });
}

fn older_than(p: &Path, age: Duration) -> bool {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .is_some_and(|d| d > age)
}

/// A valid record file (size-capped, schema-valid), or None.
fn read_record(path: &Path) -> Option<Record> {
    let f = fs::File::open(path).ok()?;
    if f.metadata().ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let r: Record = serde_json::from_reader(io::BufReader::new(f)).ok()?;
    r.validate().ok()?;
    (r.received_at_ms.is_some() && r.seq > 0).then_some(r)
}

/// Every valid record file in a scope directory.
fn scan_dir(dir: &Path) -> Vec<(PathBuf, Record)> {
    let Ok(rd) = fs::read_dir(dir) else { return vec![] };
    let mut out = vec![];
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let p = e.path();
        if let Some(r) = read_record(&p) {
            // A file whose name does not match its content (hand-copied,
            // hash collision) is not ours to show.
            if p.file_name().is_some_and(|n| *n == *format!("{}.json", content_revision(r.id.as_bytes()))) {
                out.push((p, r));
            }
        }
    }
    out
}

fn stored_in(dir: &Path) -> Vec<Stored> {
    let recs = scan_dir(dir);
    let apps: BTreeMap<&str, &str> = recs.iter().filter_map(|(_, r)| r.app.as_deref().map(|a| (r.id.as_str(), a))).collect();
    recs.iter()
        .map(|(p, r)| Stored {
            record: r.clone(),
            path: p.clone(),
            expires_at_ms: r.expires_at_ms(),
            inherited_app: if r.app.is_some() { None } else { ancestor_app(&r.id, &apps) },
        })
        .collect()
}

fn ancestor_app(id: &str, apps: &BTreeMap<&str, &str>) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    let mut cur = id;
    loop {
        cur = match cur.rfind('/') {
            Some(i) => &cur[..i],
            None if !cur.is_empty() => "",
            None => return None,
        };
        if let Some(a) = apps.get(cur) {
            return Some(a.to_string());
        }
        if cur.is_empty() {
            return None;
        }
    }
}

fn inherited_app<'a>(rec: &Record, others: impl Iterator<Item = &'a Record>) -> Option<String> {
    if rec.app.is_some() {
        return None;
    }
    let apps: BTreeMap<&str, &str> = others.filter_map(|r| r.app.as_deref().map(|a| (r.id.as_str(), a))).collect();
    ancestor_app(&rec.id, &apps)
}

/// After a publish: drop expired records and cap the scope (never the record
/// just written, at `keep`).
fn housekeep(recs: &[(PathBuf, Record)], keep: &Path, now: u64) {
    let mut live: Vec<&(PathBuf, Record)> = vec![];
    for e in recs {
        if e.0 == keep {
            continue;
        }
        if e.1.expires_at_ms() <= now {
            let _ = fs::remove_file(&e.0);
        } else {
            live.push(e);
        }
    }
    if live.len() >= MAX_RECORDS_PER_SCOPE {
        live.sort_by_key(|(_, r)| r.received_at_ms);
        for (p, _) in live.iter().take(live.len() + 1 - MAX_RECORDS_PER_SCOPE) {
            let _ = fs::remove_file(p);
        }
    }
}

fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Exclusive `flock` on a scope directory (created first when `create`).
fn lock_dir(dir: &Path, create: bool) -> io::Result<DirLock> {
    use std::os::fd::AsRawFd;
    if create {
        create_private_dir_all(dir)?;
    }
    let f = fs::File::open(dir)?;
    loop {
        // SAFETY: flock on a valid open descriptor; released when `f` closes.
        let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        if r == 0 {
            return Ok(DirLock { _file: f });
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Atomically replace `path` with the record's JSON (temp file 0600 + rename).
fn write_record(path: &Path, rec: &Record) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().ok_or_else(|| io::Error::other("record path has no directory"))?;
    let name = path.file_name().ok_or_else(|| io::Error::other("record path has no file name"))?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    let json = serde_json::to_vec_pretty(rec).map_err(io::Error::other)?;
    let result = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(&json)?;
        f.write_all(b"\n")?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::record::{Kind, State};
    use std::os::unix::fs::PermissionsExt;

    fn rec(state: State, scope: Scope, id: &str) -> Record {
        let mut r = Record::new(state, scope);
        r.id = id.into();
        r
    }

    #[test]
    fn exact_remove_preserves_descendants_and_other_scopes() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open_with(td.path());
        for (scope, id) in [(Scope::global(), ""), (Scope::global(), "a"), (Scope::global(), "a/b"), (Scope::session("s"), "a")] {
            store.publish(rec(State::Done, scope, id)).unwrap();
        }
        assert_eq!(store.remove(&Scope::global(), "a").unwrap().unwrap().record.id, "a");
        assert!(store.remove(&Scope::global(), "a").unwrap().is_none());
        assert!(store.remove(&Scope::global(), "").unwrap().is_some());
        let records = store.list(&Filter::default());
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|r| r.record.id == "a/b"));
        assert!(records.iter().any(|r| r.record.scope == Scope::session("s")));
        assert!(store.remove(&Scope::session("absent"), "a").unwrap().is_none());
    }

    #[test]
    fn publish_layout_permissions_and_replace() {
        let td = tempfile::tempdir().unwrap();
        let st = Store::open_with(td.path());
        assert_eq!(st.root(), td.path().join("tildy/agent"));
        let mut r = rec(State::Blocked, Scope::worktree("/r/.git@/r"), "wrap/claude/1");
        r.app = Some("claude".into());
        r.kind = Some(Kind::Permission);
        r.progress = Some(10);
        let s = st.publish(r.clone()).unwrap();
        assert_eq!(s.record.seq, 1);
        assert_eq!(s.record.ttl_s, Some(7200));
        assert!(s.record.received_at_ms.is_some());
        let dir = st.scope_dir(&Scope::worktree("/r/.git@/r"));
        assert_eq!(s.path.parent().unwrap(), dir);
        assert_eq!(dir.parent().unwrap().file_name().unwrap(), "worktree");
        assert_eq!(fs::metadata(&s.path).unwrap().permissions().mode() & 0o777, 0o600);
        for d in [&dir, &st.root().to_path_buf(), &st.root().join("worktree")] {
            assert_eq!(fs::metadata(d).unwrap().permissions().mode() & 0o777, 0o700, "{}", d.display());
        }
        // Each publish replaces the record: keys left out are gone.
        let mut r2 = rec(State::Done, Scope::worktree("/r/.git@/r"), "wrap/claude/1");
        r2.kind = Some(Kind::Auth);
        let s2 = st.publish(r2).unwrap();
        assert_eq!(s2.path, s.path);
        assert_eq!((s2.record.seq, s2.record.app.clone(), s2.record.kind, s2.record.ttl_s), (2, None, None, Some(900)));
        let all = st.list(&Filter::default());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].record, s2.record);
        let names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "no temp files left: {names:?}");
    }

    #[test]
    fn seq_rules() {
        let td = tempfile::tempdir().unwrap();
        let st = Store::open_with(td.path());
        let g = Scope::global;
        let mut r = rec(State::Working, g(), "job");
        r.seq = 5;
        assert_eq!(st.publish(r.clone()).unwrap().record.seq, 5);
        assert_eq!(st.publish(r.clone()), Err(Reject::StaleSeq { id: "job".into(), current: 5, got: 5 }));
        r.seq = 4;
        assert_eq!(st.publish(r.clone()).unwrap_err().code(), "stale_seq");
        r.seq = 0;
        assert_eq!(st.publish(r.clone()).unwrap().record.seq, 6, "auto seq");
        r.seq = 7;
        assert_eq!(st.publish(r.clone()).unwrap().record.seq, 7);
        // Same id in another scope, or another id: independent.
        r.seq = 1;
        r.scope = Scope::session("s1");
        assert!(st.publish(r.clone()).is_ok());
        let mut other = rec(State::Idle, g(), "job/sub");
        other.seq = 1;
        assert!(st.publish(other).is_ok());
        // An expired record accepts any seq.
        let past = rec(State::Working, g(), "old");
        let s = st.publish(Record { seq: 9, ..past.clone() }).unwrap();
        let mut stale = s.record.clone();
        stale.received_at_ms = Some(1);
        write_record(&s.path, &stale).unwrap();
        assert_eq!(st.publish(Record { seq: 2, ..past }).unwrap().record.seq, 2);
        // Invalid records never touch the store.
        let mut bad = rec(State::Idle, g(), "x");
        bad.msg = Some("a\u{7}".into());
        assert_eq!(st.publish(bad).unwrap_err().code(), "invalid");
    }

    #[test]
    fn list_filters_order_and_inheritance() {
        let td = tempfile::tempdir().unwrap();
        let st = Store::open_with(td.path());
        let wt = Scope::worktree("w1");
        let mut parent = rec(State::Working, wt.clone(), "wrap/claude/7");
        parent.app = Some("claude".into());
        st.publish(parent).unwrap();
        st.publish(rec(State::Blocked, wt.clone(), "wrap/claude/7/sub")).unwrap();
        st.publish(rec(State::Done, Scope::global(), "build")).unwrap();
        st.publish(rec(State::Idle, Scope::session("s9"), "")).unwrap();
        st.publish(rec(State::Error, Scope::worktree("w2"), "other")).unwrap();
        let mine = Filter::live(vec![Scope::session("s9"), wt.clone(), Scope::global()]);
        let got: Vec<_> = st.list(&mine).into_iter().map(|s| (s.record.state, s.record.id.clone(), s.app().map(str::to_string))).collect();
        assert_eq!(
            got,
            vec![
                (State::Blocked, "wrap/claude/7/sub".into(), Some("claude".into())),
                (State::Done, "build".into(), None),
                (State::Working, "wrap/claude/7".into(), Some("claude".into())),
                (State::Idle, "".into(), None),
            ]
        );
        assert_eq!(st.list(&Filter::default()).len(), 5);
        assert_eq!(st.list(&Filter { app: Some("claude".into()), ..Filter::default() }).len(), 2);
        assert_eq!(st.list(&Filter { id: Some("wrap/claude".into()), ..Filter::default() }).len(), 2);
        assert_eq!(st.list(&Filter { id: Some("wrap/claude/7/sub".into()), ..Filter::default() }).len(), 1);
        let later = Filter { now_ms: Some(now_ms() + 901_000), ..Filter::live(vec![]) };
        assert_eq!(st.list(&later).len(), 3, "done/error (900 s) expired, idle/working/blocked (7200 s) live");
        let expired = Filter { now_ms: Some(now_ms() + 901_000), expired: Some(true), ..Filter::default() };
        assert_eq!(st.list(&expired).len(), 2);
    }

    #[test]
    fn clear_by_id_app_scope() {
        let td = tempfile::tempdir().unwrap();
        let st = Store::open_with(td.path());
        let wt = Scope::worktree("w");
        for id in ["a", "a/b", "a/b/c", "ab"] {
            st.publish(rec(State::Working, wt.clone(), id)).unwrap();
        }
        let mut x = rec(State::Idle, wt.clone(), "x");
        x.app = Some("codex".into());
        st.publish(x).unwrap();
        st.publish(rec(State::Idle, Scope::global(), "a")).unwrap();
        let gone = st.clear(&Filter { scopes: vec![wt.clone()], id: Some("a/b".into()), ..Filter::default() }).unwrap();
        let mut ids: Vec<_> = gone.iter().map(|s| s.record.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["a/b", "a/b/c"]);
        let gone = st.clear(&Filter { scopes: vec![wt.clone()], app: Some("codex".into()), ..Filter::default() }).unwrap();
        assert_eq!(gone.len(), 1);
        assert_eq!(st.list(&Filter::default()).len(), 3);
        assert_eq!(st.clear(&Filter { id: Some("a".into()), ..Filter::default() }).unwrap().len(), 2, "every scope");
        assert_eq!(st.clear(&Filter::default()).unwrap().len(), 1);
        assert!(st.list(&Filter::default()).is_empty());
        assert!(st.clear(&Filter::default()).unwrap().is_empty());
    }

    #[test]
    fn drop_on_prompt_lifecycle() {
        let td = tempfile::tempdir().unwrap();
        let st = Store::open_with(td.path());
        let wt = Scope::worktree("w");
        let with_origin = |state, id: &str, origin: Option<&str>| {
            let mut r = rec(state, wt.clone(), id);
            r.origin_session = origin.map(str::to_string);
            st.publish(r).unwrap();
        };
        with_origin(State::Working, "w", Some("A"));
        with_origin(State::Blocked, "b", Some("A"));
        with_origin(State::Idle, "i", Some("A"));
        with_origin(State::Done, "d", Some("A"));
        with_origin(State::Error, "e", Some("A"));
        with_origin(State::Working, "other", Some("B"));
        with_origin(State::Working, "none", None);
        assert_eq!(st.drop_on_prompt(""), 0);
        assert_eq!(st.drop_on_prompt("A"), 3, "idle/working/blocked go at the first prompt");
        let ids = |st: &Store| st.list(&Filter::default()).into_iter().map(|s| s.record.id).collect::<Vec<_>>();
        assert_eq!(ids(&st), ["e", "d", "none", "other"]);
        assert!(st.list(&Filter::default()).iter().filter(|s| s.record.state.is_final()).all(|s| s.record.prompt_seen));
        assert_eq!(st.drop_on_prompt("A"), 2, "done/error go at the second prompt");
        assert_eq!(ids(&st), ["none", "other"]);
        // A republish resets prompt_seen.
        with_origin(State::Done, "d", Some("A"));
        st.drop_on_prompt("A");
        with_origin(State::Done, "d", Some("A"));
        assert!(!st.list(&Filter::default()).iter().any(|s| s.record.prompt_seen));
    }

    #[test]
    fn expire_and_next_expiry() {
        let td = tempfile::tempdir().unwrap();
        let st = Store::open_with(td.path());
        assert_eq!(st.next_expiry(), None);
        assert_eq!(st.expire(now_ms()), 0);
        let mut r = rec(State::Working, Scope::global(), "short");
        r.ttl_s = Some(1);
        let s1 = st.publish(r).unwrap();
        let s2 = st.publish(rec(State::Done, Scope::session("s"), "long")).unwrap();
        assert_eq!(st.next_expiry(), Some(s1.expires_at_ms));
        assert_eq!(st.expire(s1.expires_at_ms - 1), 0);
        assert_eq!(st.expire(s1.expires_at_ms), 1);
        assert_eq!(st.next_expiry(), Some(s2.expires_at_ms));
        assert_eq!(st.list(&Filter::default()).len(), 1);
        // Garbage and foreign files.
        let dir = st.scope_dir(&Scope::session("s"));
        fs::write(dir.join("junk.json"), "{").unwrap();
        fs::write(dir.join("notes.txt"), "x").unwrap();
        assert_eq!(st.list(&Filter::default()).len(), 1, "invalid files are ignored");
        let old = SystemTime::now() - Duration::from_secs(120);
        fs::File::options().write(true).open(dir.join("junk.json")).unwrap().set_modified(old).unwrap();
        assert_eq!(st.expire(now_ms()), 1, "old unreadable record file removed");
        assert!(dir.join("notes.txt").exists());
        // A copied file whose name does not match its id is ignored.
        fs::copy(&s2.path, dir.join("0000000000000000.json")).unwrap();
        assert_eq!(st.list(&Filter::default()).len(), 1);
    }

    #[test]
    fn scope_cap() {
        let td = tempfile::tempdir().unwrap();
        let st = Store::open_with(td.path());
        for i in 0..(MAX_RECORDS_PER_SCOPE + 3) {
            st.publish(rec(State::Idle, Scope::global(), &format!("r{i}"))).unwrap();
        }
        let all = st.list(&Filter::default());
        assert_eq!(all.len(), MAX_RECORDS_PER_SCOPE);
        assert!(all.iter().any(|s| s.record.id == format!("r{}", MAX_RECORDS_PER_SCOPE + 2)), "newest kept");
    }

    #[test]
    fn concurrent_publishers_keep_seq_monotonic() {
        let td = tempfile::tempdir().unwrap();
        let st = Store::open_with(td.path());
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..10 {
                        st.publish(rec(State::Working, Scope::global(), "busy")).unwrap();
                    }
                });
            }
        });
        assert_eq!(st.list(&Filter::default())[0].record.seq, 80);
    }
}
