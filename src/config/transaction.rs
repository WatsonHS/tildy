//! Config transactions: read, validate, apply and roll back the user config
//! with expected revisions, atomic saves and a history directory. Owner: M2
//! task A. The CLI front end is `src/cli/config.rs`; ADR 0007 has the design.
//!
//! - A revision is `content_revision` of the user file's bytes, or `builtin`
//!   when no file exists. `apply` and `rollback` take the revision the caller
//!   last read (`--expect-revision`) and refuse with `revision_conflict` when
//!   the file changed since.
//! - Before a file is replaced or removed its exact bytes are copied to the
//!   history directory (`$XDG_STATE_HOME/tildy/config-history/`, default
//!   `~/.local/state/tildy/config-history/`) as `<unix_ms>-<revision>.json`,
//!   with a `<unix_ms>-<revision>.meta.json` sidecar naming the operation. The
//!   newest `HISTORY_KEEP` entries are kept.
//! - New content goes to a temporary file in the target's directory, is
//!   synced, then renamed over the target: a failure at any step leaves the
//!   previous file in place. Transactions are serialized by an advisory lock
//!   (`flock`) on `<resolved-config>.lock` and `<history>/.lock`, with a
//!   shared two-second wait budget. Lock files persist; closing releases them.
//! - A symlinked config path is followed: the file at the end of the link is
//!   read, replaced or removed, and the link itself stays.
use super::{config_path_with, content_revision, from_str, preset_json, Config, ConfigError, ErrorKind};
use crate::model::ErrorCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Revision of the zero-config state (no user file).
pub const BUILTIN: &str = "builtin";
/// History entries kept after each transaction (newest first).
pub const HISTORY_KEEP: usize = 50;
/// Error messages are capped to the length `schema/error.schema.json` allows.
const MAX_MESSAGE: usize = 1000;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// Never unlink a lock file: waiters must keep using the same inode across
/// atomic config replacements. The PID is diagnostic only, not lock ownership.
fn acquire_lock(path: &Path, deadline: Instant) -> Result<File, TxError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{FileExt, MetadataExt};
    let unavailable = |e: io::Error| {
        TxError::environment(
            format!("cannot lock {}: {e}", path.display()),
            Some("the config was not changed; check lock file and directory permissions and retry"),
        )
    };
    let mut f = OpenOptions::new().create(true).truncate(false).read(true).write(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).map_err(unavailable)?;
    if !f.metadata().map_err(unavailable)?.is_file() {
        return Err(unavailable(io::Error::other("lock must be a regular file")));
    }
    loop {
        // SAFETY: flock uses a descriptor owned by f; LOCK_NB never waits.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            let metadata = f.metadata().map_err(unavailable)?;
            let mut buf = [0; 32];
            let n = f.read_at(&mut buf, 0).map_err(unavailable)?;
            let is_record = n == 0 || std::str::from_utf8(&buf[..n]).ok().is_some_and(|s| s.trim().parse::<u32>().is_ok());
            if metadata.nlink() != 1 || metadata.len() > 32 || !is_record {
                return Err(unavailable(io::Error::other("lock must be an unshared empty file or PID record")));
            }
            f.set_len(0).map_err(unavailable)?;
            writeln!(f, "{}", std::process::id()).map_err(unavailable)?;
            return Ok(f);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::WouldBlock && e.kind() != io::ErrorKind::Interrupted {
            return Err(unavailable(e));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let mut buf = [0; 32];
            let pid = f.read_at(&mut buf, 0).ok()
                .and_then(|n| std::str::from_utf8(&buf[..n]).ok())
                .and_then(|s| s.trim().parse::<u32>().ok()).filter(|pid| *pid != 0);
            let owner = pid.map(|pid| format!("recorded holder pid {pid}")).unwrap_or_else(|| "holder pid unavailable".into());
            return Err(TxError::environment(
                format!("config locked: two-second wait budget exhausted ({owner}); {}", path.display()),
                Some("the config was not changed; wait for the other writer and retry; do not delete the lock file"),
            ));
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

/// A transaction failure with a stable code (exit code via `ErrorCode::exit_code`)
/// and, for config field errors, the JSON path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxError {
    pub code: ErrorCode,
    pub path: Option<String>,
    pub message: String,
    pub hint: Option<String>,
}

fn cap(s: String) -> String {
    if s.len() <= MAX_MESSAGE {
        return s;
    }
    let mut end = MAX_MESSAGE - 3;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

impl TxError {
    fn new(code: ErrorCode, message: impl Into<String>, hint: Option<&str>) -> Self {
        TxError { code, path: None, message: cap(message.into()), hint: hint.map(str::to_string) }
    }
    pub fn validation(message: impl Into<String>, hint: Option<&str>) -> Self {
        Self::new(ErrorCode::ValidationFailed, message, hint)
    }
    pub fn environment(message: impl Into<String>, hint: Option<&str>) -> Self {
        Self::new(ErrorCode::EnvironmentUnavailable, message, hint)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InternalError, message, None)
    }
    fn conflict(expected: &str, current: &str) -> Self {
        Self::new(
            ErrorCode::RevisionConflict,
            format!("config revision is {current}, expected {expected}"),
            Some("the file changed since it was read; run `tildy config get --json`, redo the edit on the new content and retry with its revision"),
        )
    }
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.path {
            Some(p) => write!(f, "{p}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for TxError {}

impl From<ConfigError> for TxError {
    fn from(e: ConfigError) -> Self {
        let hint = match e.kind {
            ErrorKind::Syntax => "fix the JSON syntax at the line and column in the message",
            ErrorKind::Unsupported => "this tildy understands schema_version 1; upgrade tildy or write schema_version 1",
            ErrorKind::Invalid | ErrorKind::Io => "see `tildy schema` for the fields and their values",
        };
        let code = if e.kind == ErrorKind::Io { ErrorCode::EnvironmentUnavailable } else { ErrorCode::ValidationFailed };
        let mut t = TxError::new(code, e.message, Some(hint));
        t.path = Some(e.path).filter(|p| !p.is_empty());
        t
    }
}

/// A config problem as reported by `get` (the file is read but not in effect).
#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    /// `syntax` (not JSON / not UTF-8), `invalid` (fails validation) or
    /// `unsupported` (newer `schema_version`).
    pub kind: &'static str,
    pub path: Option<String>,
    pub message: String,
}

impl From<&ConfigError> for Problem {
    fn from(e: &ConfigError) -> Self {
        let kind = match e.kind {
            ErrorKind::Syntax | ErrorKind::Io => "syntax",
            ErrorKind::Invalid => "invalid",
            ErrorKind::Unsupported => "unsupported",
        };
        Problem { kind, path: Some(e.path.clone()).filter(|p| !p.is_empty()), message: e.message.clone() }
    }
}

/// A non-fatal finding: the config is valid but part of it will not work as
/// written (provider problems, modules that are not placed in the layout).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub path: Option<String>,
    pub message: String,
}

/// `<state>/tildy/config-history`, where `<state>` is `$XDG_STATE_HOME` (if
/// absolute) or `$HOME/.local/state` (see `session::state_dir_with`).
pub fn history_dir_with(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    crate::session::state_dir_with(xdg_state_home, home).map(|d| d.join("tildy").join("config-history"))
}

pub fn history_dir() -> Option<PathBuf> {
    history_dir_with(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

/// Check a revision argument: `builtin` or 16 hex digits (lowercased).
/// `flag` names the option in the error message.
pub fn parse_revision(s: &str, flag: &str) -> Result<String, TxError> {
    let t = s.trim();
    if t == BUILTIN || (t.len() == 16 && t.chars().all(|c| c.is_ascii_hexdigit())) {
        return Ok(t.to_ascii_lowercase());
    }
    Err(TxError::validation(
        format!("{flag}: '{s}' is not a revision (expected `builtin` or 16 hex digits)"),
        Some("revisions come from `tildy config get --json` (data.revision) or `tildy config history --json`"),
    ))
}

/// Follow a chain of symlinks (at most 16) to the file that holds the
/// config. A dangling link resolves to its (missing) target, so a write
/// creates the target and keeps the link.
pub fn resolve_target(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    for _ in 0..16 {
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => match fs::read_link(&p) {
                Ok(t) if t.is_absolute() => p = t,
                Ok(t) => p = p.parent().unwrap_or(Path::new(".")).join(t),
                Err(_) => break,
            },
            _ => break,
        }
    }
    p
}

/// The user config file as it is on disk now.
#[derive(Debug, Clone)]
pub struct Current {
    /// The configured path (`$TILDY_CONFIG`, ...).
    pub path: PathBuf,
    /// `path` with symlinks resolved.
    pub target: PathBuf,
    /// File bytes; None when the file does not exist.
    pub bytes: Option<Vec<u8>>,
    pub revision: String,
}

impl Current {
    pub fn exists(&self) -> bool {
        self.bytes.is_some()
    }
}

/// Result of validating a candidate config.
#[derive(Debug, Clone)]
pub struct Checked {
    pub config: Config,
    pub revision: String,
    pub warnings: Vec<Warning>,
}

/// Validate config bytes exactly like the loader does (`config::from_str`
/// over the built-in and preset layers), then collect warnings: provider
/// problems (`providers::from_config_checked`) and user modules that no
/// layout row places.
pub fn check_bytes(bytes: &[u8]) -> Result<Checked, TxError> {
    let text = std::str::from_utf8(bytes).map_err(|e| TxError::validation(format!("not valid UTF-8: {e}"), Some("save the file as UTF-8")))?;
    let config = from_str(text)?;
    let mut warnings: Vec<Warning> = crate::providers::from_config_checked(&config).1.into_iter().map(split_problem).collect();
    let placed: BTreeSet<&str> = config.layout_modules().into_iter().map(|(id, _)| id).collect();
    if let Some(Value::Object(m)) = config.user.as_ref().and_then(|u| u.get("modules")) {
        for id in m.keys().filter(|id| !placed.contains(id.as_str())) {
            warnings.push(Warning { path: Some(format!("modules.{id}")), message: "not placed in any layout row; it is never shown".into() });
        }
    }
    Ok(Checked { revision: content_revision(bytes), config, warnings })
}

/// `"modules.x: message"` -> path + message.
fn split_problem(p: String) -> Warning {
    match p.split_once(": ") {
        Some((path, msg)) if path.starts_with("modules.") && !path.contains(' ') => {
            Warning { path: Some(path.to_string()), message: msg.to_string() }
        }
        _ => Warning { path: None, message: p },
    }
}

/// Which layer each part of the effective config comes from: `builtin`,
/// `preset:<name>` or `user`. For a module, the highest layer that defines or
/// overrides it (a user entry without `type` is merged onto the lower layer's
/// module of the same id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sources {
    /// Top-level keys of the effective config.
    pub keys: BTreeMap<String, String>,
    /// Module ids of the effective config.
    pub modules: BTreeMap<String, String>,
}

pub fn sources(config: &Config) -> Sources {
    let preset_name = config.file.preset.clone().unwrap_or_else(|| "plain".into());
    let mut preset: Value = preset_json(&preset_name).and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
    // The preset's own top-level `$comment` is not carried over (`apply_preset`).
    if let Some(p) = preset.as_object_mut() {
        p.remove("$comment");
    }
    let user = config.user.as_ref().unwrap_or(&Value::Null);
    let project = config.project.as_ref().and_then(|p| p.applied.as_ref()).unwrap_or(&Value::Null);
    let layer = |in_project: bool, in_user: bool, in_preset: bool| -> String {
        if in_project {
            "project".into()
        } else if in_user {
            "user".into()
        } else if in_preset {
            format!("preset:{preset_name}")
        } else {
            BUILTIN.into()
        }
    };
    let module_in = |v: &Value, id: &str| v.get("modules").and_then(|m| m.get(id)).is_some();
    let keys = config.json.as_object().into_iter().flat_map(|m| m.keys());
    let modules = config.json.get("modules").and_then(Value::as_object).into_iter().flat_map(|m| m.keys());
    Sources {
        keys: keys.map(|k| (k.clone(), layer(project.get(k).is_some(), user.get(k).is_some(), preset.get(k).is_some()))).collect(),
        modules: modules.map(|id| (id.clone(), layer(module_in(project, id), module_in(user, id), module_in(&preset, id)))).collect(),
    }
}

/// Outcome of `apply`.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    pub path: PathBuf,
    pub previous_revision: String,
    pub revision: String,
    /// False when the new content equals the current file (nothing written).
    pub changed: bool,
    /// The revision check was skipped (`--force`).
    pub forced: bool,
    /// History entry holding the previous file; None when there was no file
    /// or nothing changed.
    pub history_id: Option<String>,
    pub warnings: Vec<Warning>,
}

/// Outcome of `rollback`.
#[derive(Debug, Clone, Serialize)]
pub struct RolledBack {
    pub path: PathBuf,
    pub previous_revision: String,
    pub revision: String,
    pub changed: bool,
    /// History entry holding the file that was replaced or removed.
    pub history_id: Option<String>,
    /// History entry restored; None for `--to builtin`.
    pub restored_from: Option<String>,
}

/// One saved copy of a previous config file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// `<unix_ms>-<revision>` (the file is `<id>.json`).
    pub id: String,
    pub at_ms: u64,
    pub revision: String,
    pub bytes: u64,
    /// Operation that replaced or removed this content: `apply`, `rollback`,
    /// or `unknown` when the sidecar is missing.
    pub op: String,
    /// Config path the content was saved from.
    pub path: Option<PathBuf>,
    /// Revision the operation installed in its place (`builtin` = file removed).
    pub replaced_by: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    op: String,
    path: PathBuf,
    replaced_by: String,
}

/// The user config path plus the history directory.
#[derive(Debug, Clone)]
pub struct Store {
    pub config_path: PathBuf,
    /// None when neither `$XDG_STATE_HOME` nor `$HOME` is usable; then only
    /// `current` and validation work.
    pub history_dir: Option<PathBuf>,
}

impl Store {
    /// From the process environment: `config::default_config_path` rules and
    /// `history_dir`.
    pub fn from_env() -> Result<Store, TxError> {
        Store::from_env_with(&|k| std::env::var_os(k))
    }

    pub fn from_env_with(env: &dyn Fn(&str) -> Option<OsString>) -> Result<Store, TxError> {
        let s = |k: &str| env(k).map(|v| v.to_string_lossy().into_owned());
        let config_path = config_path_with(&s).ok_or_else(|| {
            TxError::environment("cannot resolve the config path", Some("set TILDY_CONFIG, XDG_CONFIG_HOME or HOME"))
        })?;
        Ok(Store { config_path, history_dir: history_dir_with(env("XDG_STATE_HOME"), env("HOME")) })
    }

    pub fn current(&self) -> Result<Current, TxError> {
        self.current_at(resolve_target(&self.config_path))
    }

    fn current_at(&self, target: PathBuf) -> Result<Current, TxError> {
        match fs::read(&target) {
            Ok(b) => Ok(Current { path: self.config_path.clone(), target, revision: content_revision(&b), bytes: Some(b) }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Ok(Current { path: self.config_path.clone(), target, bytes: None, revision: BUILTIN.into() })
            }
            Err(e) => Err(TxError::environment(
                format!("cannot read {}: {e}", target.display()),
                Some("check the file's permissions, or point TILDY_CONFIG at a readable file"),
            )),
        }
    }

    fn history(&self) -> Result<&Path, TxError> {
        self.history_dir.as_deref().ok_or_else(|| {
            TxError::environment("cannot resolve the history directory", Some("set XDG_STATE_HOME or HOME to an absolute path"))
        })
    }

    /// Lock the actual target, then history (also shared with older binaries).
    /// Pin the resolved target for the entire transaction, including rereading.
    fn lock(&self) -> Result<(PathBuf, PathBuf, [File; 2]), TxError> {
        let deadline = Instant::now() + LOCK_TIMEOUT;
        let dir = self.history()?.to_path_buf();
        let unavailable = |what: &str, e: io::Error| {
            TxError::environment(format!("{what} {}: {e}", dir.display()), Some("make the history directory writable (XDG_STATE_HOME or ~/.local/state)"))
        };
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| unavailable("cannot create", e))?;
        let target = resolve_target(&self.config_path);
        let target_error = |e| TxError::environment(
            format!("cannot resolve config target {}: {e}", target.display()),
            Some("the config was not changed; check the config path and directory permissions"),
        );
        if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(target_error(io::Error::other("symlink chain is cyclic or longer than 16 links")));
        }
        let parent = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(target_error)?;
        let name = target.file_name().ok_or_else(|| target_error(io::Error::other("expected a config file path")))?;
        let target = fs::canonicalize(parent).map_err(target_error)?.join(name);
        let mut lock_name = name.to_os_string();
        lock_name.push(".lock");
        let config_lock = acquire_lock(&target.with_file_name(lock_name), deadline)?;
        let history_lock = acquire_lock(&dir.join(".lock"), deadline)?;
        Ok((dir, target, [config_lock, history_lock]))
    }

    /// Validate `bytes` and install them as the user config. `expect` is the
    /// revision the caller read (`builtin` when there was no file); `force`
    /// skips that check. The previous file goes to the history first.
    pub fn apply(&self, bytes: &[u8], expect: Option<&str>, force: bool) -> Result<Applied, TxError> {
        let expect = match (expect, force) {
            (Some(e), _) => Some(parse_revision(e, "--expect-revision")?),
            (None, true) => None,
            (None, false) => {
                return Err(TxError::validation(
                    "--expect-revision is required (or --force)",
                    Some("pass data.revision from `tildy config get --json`; `builtin` when there is no file"),
                ))
            }
        };
        let checked = check_bytes(bytes)?;
        let (dir, target, _locks) = self.lock()?;
        let cur = self.current_at(target)?;
        if let (Some(e), false) = (&expect, force) {
            if *e != cur.revision {
                return Err(TxError::conflict(e, &cur.revision));
            }
        }
        let mut out = Applied {
            path: cur.path.clone(),
            previous_revision: cur.revision.clone(),
            revision: checked.revision.clone(),
            changed: cur.bytes.as_deref() != Some(bytes),
            forced: force,
            history_id: None,
            warnings: checked.warnings,
        };
        if !out.changed {
            return Ok(out);
        }
        out.history_id = replace(&dir, &cur, Some(bytes), &checked.revision, "apply")?;
        Ok(out)
    }

    /// Restore the newest history entry with revision `to` (or remove the
    /// file for `builtin`). `expect` is the current revision as the caller
    /// read it. The file being replaced goes to the history first.
    pub fn rollback(&self, to: &str, expect: &str) -> Result<RolledBack, TxError> {
        let to = parse_revision(to, "--to")?;
        let expect = parse_revision(expect, "--expect-revision")?;
        let (dir, target, _locks) = self.lock()?;
        let cur = self.current_at(target)?;
        if expect != cur.revision {
            return Err(TxError::conflict(&expect, &cur.revision));
        }
        let mut out = RolledBack {
            path: cur.path.clone(),
            previous_revision: cur.revision.clone(),
            revision: to.clone(),
            changed: cur.revision != to,
            history_id: None,
            restored_from: None,
        };
        if !out.changed {
            return Ok(out);
        }
        let (bytes, restored_from) = if to == BUILTIN {
            (None, None)
        } else {
            let entry = list_entries(&dir)?.into_iter().find(|e| e.revision == to).ok_or_else(|| {
                TxError::validation(
                    format!("no history entry with revision {to} in {}", dir.display()),
                    Some("list the saved revisions with `tildy config history --json`; `--to builtin` removes the user file"),
                )
            })?;
            let file = dir.join(format!("{}.json", entry.id));
            let b = fs::read(&file).map_err(|e| TxError::environment(format!("cannot read {}: {e}", file.display()), None))?;
            if content_revision(&b) != to {
                return Err(TxError::internal(format!("history entry {} is corrupt (content revision {})", entry.id, content_revision(&b))));
            }
            if let Err(e) = check_bytes(&b) {
                return Err(TxError {
                    message: cap(format!("history entry {} does not validate: {}", entry.id, e.message)),
                    hint: Some(cap(format!(
                        "rollback only installs configs this tildy can load; pick another revision from `tildy config history`, or copy {} by hand",
                        file.display()
                    ))),
                    ..e
                });
            }
            (Some(b), Some(entry.id))
        };
        out.restored_from = restored_from;
        out.history_id = replace(&dir, &cur, bytes.as_deref(), &to, "rollback")?;
        Ok(out)
    }

    /// History entries, newest first. A missing directory is an empty history.
    pub fn history_entries(&self) -> Result<Vec<Entry>, TxError> {
        list_entries(self.history()?)
    }
}

/// Save the current file to the history, then install `new` (None = remove
/// the file). Returns the history entry id (None when there was no file).
/// On failure the current file is untouched and the new entry is removed.
fn replace(dir: &Path, cur: &Current, new: Option<&[u8]>, new_revision: &str, op: &str) -> Result<Option<String>, TxError> {
    let target = &cur.target;
    let unwritable = |what: String, e: io::Error| {
        TxError::environment(format!("{what}: {e}"), Some("the config was not changed; make its directory writable and retry"))
    };
    // 1. Stage the new content next to the target (nothing changes yet).
    let staged = match new {
        Some(b) => {
            let perms = fs::metadata(target).ok().map(|m| m.permissions());
            Some(stage(target, b, perms).map_err(|e| unwritable(format!("cannot write a temporary file next to {}", target.display()), e))?)
        }
        None => None,
    };
    let discard = |p: &Option<PathBuf>| {
        if let Some(p) = p {
            let _ = fs::remove_file(p);
        }
    };
    // 2. Keep the old content.
    let saved = match &cur.bytes {
        Some(old) => {
            let meta = Meta { op: op.into(), path: cur.path.clone(), replaced_by: new_revision.into() };
            match save_entry(dir, old, &cur.revision, &meta) {
                Ok(id) => Some(id),
                Err(e) => {
                    discard(&staged);
                    return Err(TxError::environment(
                        format!("cannot save the current config to {}: {e}", dir.display()),
                        Some("the config was not changed; make the history directory writable and retry"),
                    ));
                }
            }
        }
        None => None,
    };
    // 3. Switch.
    let result = match &staged {
        Some(tmp) => fs::rename(tmp, target).map_err(|e| unwritable(format!("cannot replace {}", target.display()), e)),
        None => match fs::remove_file(target) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(unwritable(format!("cannot remove {}", target.display()), e)),
            _ => Ok(()),
        },
    };
    if let Err(e) = result {
        discard(&staged);
        if let Some(id) = &saved {
            remove_entry(dir, id);
        }
        return Err(e);
    }
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(d) = File::open(parent) {
            let _ = d.sync_all();
        }
    }
    prune(dir, HISTORY_KEEP);
    Ok(saved)
}

/// Write `bytes` to a new temporary file in `target`'s directory (created if
/// missing), copying `perms` from the file it will replace.
fn stage(target: &Path, bytes: &[u8], perms: Option<fs::Permissions>) -> io::Result<PathBuf> {
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&dir)?;
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "config.json".into());
    let tmp = dir.join(format!(".{name}.tildy-tx-{}-{}", std::process::id(), crate::diagnostics::now_ms()));
    let r = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        if let Some(p) = perms {
            f.set_permissions(p)?;
        }
        f.sync_all()
    })();
    match r {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Write a file in `dir` via a temporary name and a rename (0600).
fn write_private(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let r = (|| {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, dir.join(name))
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// Store `bytes` as a new history entry; returns its id. The timestamp is
/// kept strictly above every existing entry's so the order is total.
fn save_entry(dir: &Path, bytes: &[u8], revision: &str, meta: &Meta) -> io::Result<String> {
    let newest = list_entries(dir).ok().and_then(|v| v.first().map(|e| e.at_ms)).unwrap_or(0);
    let ms = crate::diagnostics::now_ms().max(newest + 1);
    let id = format!("{ms}-{revision}");
    write_private(dir, &format!("{id}.json"), bytes)?;
    let meta = serde_json::to_vec_pretty(meta).map_err(io::Error::other)?;
    if let Err(e) = write_private(dir, &format!("{id}.meta.json"), &meta) {
        remove_entry(dir, &id);
        return Err(e);
    }
    Ok(id)
}

fn remove_entry(dir: &Path, id: &str) {
    let _ = fs::remove_file(dir.join(format!("{id}.json")));
    let _ = fs::remove_file(dir.join(format!("{id}.meta.json")));
}

/// Keep the newest `keep` entries.
fn prune(dir: &Path, keep: usize) {
    if let Ok(entries) = list_entries(dir) {
        for e in entries.iter().skip(keep) {
            remove_entry(dir, &e.id);
        }
    }
}

/// `<unix_ms>-<16 hex>.json` -> (ms, revision).
fn parse_entry_name(name: &str) -> Option<(u64, String)> {
    let stem = name.strip_suffix(".json")?;
    let (ms, rev) = stem.split_once('-')?;
    if ms.is_empty() || !ms.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if rev.len() != 16 || !rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    Some((ms.parse().ok()?, rev.to_string()))
}

/// Entries in `dir`, newest first; a missing directory is empty.
fn list_entries(dir: &Path) -> Result<Vec<Entry>, TxError> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(TxError::environment(format!("cannot read {}: {e}", dir.display()), None)),
    };
    let mut out = vec![];
    for de in rd.flatten() {
        let name = de.file_name().to_string_lossy().into_owned();
        let Some((at_ms, revision)) = parse_entry_name(&name) else { continue };
        let Ok(md) = de.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        let id = format!("{at_ms}-{revision}");
        let meta: Option<Meta> = fs::read(dir.join(format!("{id}.meta.json"))).ok().and_then(|b| serde_json::from_slice(&b).ok());
        out.push(Entry {
            id,
            at_ms,
            revision,
            bytes: md.len(),
            op: meta.as_ref().map(|m| m.op.clone()).unwrap_or_else(|| "unknown".into()),
            path: meta.as_ref().map(|m| m.path.clone()),
            replaced_by: meta.map(|m| m.replaced_by),
        });
    }
    out.sort_by(|a, b| b.at_ms.cmp(&a.at_ms).then_with(|| b.id.cmp(&a.id)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn history_dir_resolution() {
        assert_eq!(history_dir_with(os("/x/state"), os("/h")), Some(PathBuf::from("/x/state/tildy/config-history")));
        assert_eq!(history_dir_with(os("rel"), os("/h")), Some(PathBuf::from("/h/.local/state/tildy/config-history")));
        assert_eq!(history_dir_with(None, os("/h")), Some(PathBuf::from("/h/.local/state/tildy/config-history")));
        assert_eq!(history_dir_with(None, None), None);
    }

    #[test]
    fn store_from_env_follows_config_path_rules() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v));
        let s = Store::from_env_with(&env(&[("HOME", "/h")])).unwrap();
        assert_eq!(s.config_path, PathBuf::from("/h/.config/tildy/config.json"));
        assert_eq!(s.history_dir, Some(PathBuf::from("/h/.local/state/tildy/config-history")));
        let s = Store::from_env_with(&env(&[("TILDY_CONFIG", "/c.json"), ("XDG_STATE_HOME", "/s")])).unwrap();
        assert_eq!((s.config_path, s.history_dir), (PathBuf::from("/c.json"), Some(PathBuf::from("/s/tildy/config-history"))));
        let e = Store::from_env_with(&env(&[])).unwrap_err();
        assert_eq!(e.code, ErrorCode::EnvironmentUnavailable);
    }

    #[test]
    fn revisions_are_checked() {
        assert_eq!(parse_revision("builtin", "--to").unwrap(), "builtin");
        assert_eq!(parse_revision(" 0123456789ABCDEF ", "--to").unwrap(), "0123456789abcdef");
        for bad in ["", "built-in", "0123", "0123456789abcdeg", "0123456789abcdef0"] {
            let e = parse_revision(bad, "--to").unwrap_err();
            assert_eq!(e.code, ErrorCode::ValidationFailed, "{bad}");
            assert!(e.message.starts_with("--to:"), "{}", e.message);
        }
    }

    #[test]
    fn entry_names() {
        assert_eq!(parse_entry_name("1760940300123-0123456789abcdef.json"), Some((1760940300123, "0123456789abcdef".into())));
        for bad in ["1-0123456789abcdef.meta.json", "x-0123456789abcdef.json", "-0123456789abcdef.json", "1-builtin.json", "1-0123456789ABCDEF.json", ".lock"] {
            assert_eq!(parse_entry_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn error_messages_are_capped() {
        let e = TxError::validation("é".repeat(800), None);
        assert!(e.message.len() <= MAX_MESSAGE && e.message.ends_with("..."));
    }

    #[test]
    fn config_errors_map_to_codes_and_paths() {
        let e: TxError = from_str("{").unwrap_err().into();
        assert_eq!((e.code, e.path.as_deref()), (ErrorCode::ValidationFailed, None));
        let e: TxError = from_str(r#"{"schema_version":2}"#).unwrap_err().into();
        assert_eq!((e.code, e.path.as_deref()), (ErrorCode::ValidationFailed, Some("schema_version")));
    }

    #[test]
    fn sources_name_the_layer() {
        let c = from_str(r##"{"schema_version":1,"preset":"pastel-powerline","modules":{"time":{"style":{"foreground":"#FFFFFF"}},"extra":{"type":"text","text":"x"}}}"##).unwrap();
        let s = sources(&c);
        assert_eq!(s.keys["schema_version"], "user");
        assert_eq!(s.keys["preset"], "user");
        assert_eq!(s.keys["layout"], "preset:pastel-powerline");
        assert_eq!(s.keys["modules"], "user");
        assert_eq!(s.keys["history"], "builtin");
        assert_eq!(s.keys["symbols"], "preset:pastel-powerline");
        assert_eq!(s.modules["time"], "user");
        assert_eq!(s.modules["extra"], "user");
        assert_eq!(s.modules["cwd"], "preset:pastel-powerline");
        let b = sources(&super::super::builtin());
        assert_eq!(b.keys["layout"], "preset:pastel-powerline");
        assert_eq!(b.modules["cwd"], "preset:pastel-powerline");
        assert_eq!(b.keys["$comment"], "builtin");
    }

    #[test]
    fn check_bytes_warns_about_unplaced_modules_and_provider_problems() {
        let c = check_bytes(br#"{"schema_version":1,"modules":{"lonely":{"type":"text","text":"x"}}}"#).unwrap();
        assert_eq!(c.warnings, vec![Warning { path: Some("modules.lonely".into()), message: "not placed in any layout row; it is never shown".into() }]);
        let c = check_bytes(br#"{"schema_version":1,"modules":{"git":{"type":"command","argv":["true"]}}}"#).unwrap();
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        assert_eq!(c.warnings[0].path.as_deref(), Some("modules.git"));
        assert!(c.warnings[0].message.contains("already in use"));
        let e = check_bytes(b"{\"schema_version\":1,\xff}").unwrap_err();
        assert_eq!(e.code, ErrorCode::ValidationFailed);
    }

    #[test]
    fn resolve_target_follows_links() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real.json");
        let link = d.path().join("link.json");
        std::os::unix::fs::symlink("real.json", &link).unwrap();
        assert_eq!(resolve_target(&link), real);
        assert_eq!(resolve_target(&real), real);
        let lp = d.path().join("loop");
        std::os::unix::fs::symlink("loop", &lp).unwrap();
        assert_eq!(resolve_target(&lp), lp);
    }
}
