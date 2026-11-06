//! Instant prompt cache (docs/decisions/0006-instant-prompt.md).
//!
//! The snippet from `tildy init zsh --instant` runs at the top of `.zshrc` and
//! prints the prompt cached by the previous session before the rest of the
//! startup files run. The worker writes that cache: after every `fresh` RES it
//! renders the same data once more as a new session's first prompt would look
//! (exit status 0, no duration, no jobs) and stores the `L`/`R` lines.
//!
//! The cache key is computed by the shell only (one place: `_tildy_instant_key`
//! in `shell/zsh/instant.zsh`) and arrives as `TILDY_INSTANT_KEY=<key>` inside
//! the REQ `env` field. [`take_key`] removes that pair before the request is
//! scheduled, so the key never becomes part of the env cache revision or the
//! renderer's `env.*` fields. The directory comes from `TILDY_INSTANT_DIR`
//! (exported to the worker by the shell, the snippet's choice), else
//! `$XDG_CACHE_HOME/tildy/instant`, else `~/.cache/tildy/instant`.
//!
//! File format (`<dir>/<key>.prompt`, data only, never sourced or eval'd):
//!
//! ```text
//! tildy-instant<TAB>1<TAB><n>
//! L<TAB><line>      (n times, Zsh prompt escapes, sanitized like a RES line)
//! R<TAB><right>     (optional)
//! ```
//!
//! Writes are atomic (temp file + rename, 0600 in a 0700 directory) and happen
//! on a background thread, latest content per path wins. The directory is
//! pruned once per worker: files older than [`MAX_AGE`] go, then the oldest
//! beyond [`MAX_ENTRIES`].

use crate::diagnostics::write_atomic;
use crate::model::{PromptRequest, RenderedPrompt};
use serde::{Deserialize, Serialize};
use crate::protocol::{sanitize_prompt_text, MAX_LEFT_LINES};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime};

/// `env` pair carrying the cache key (removed by [`take_key`]).
pub const KEY_ENV: &str = "TILDY_INSTANT_KEY";
/// Worker environment variable naming the cache directory.
pub const DIR_ENV: &str = "TILDY_INSTANT_DIR";
pub const FORMAT_HEADER: &str = "tildy-instant";
pub const FORMAT_VERSION: u32 = 1;
pub const FILE_SUFFIX: &str = ".prompt";
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
pub const MAX_ENTRIES: usize = 200;
const MAX_KEY_LEN: usize = 160;

/// Remove the `TILDY_INSTANT_KEY` pair from `req.env` and return the key if it
/// is a safe file name stem (`[A-Za-z0-9._-]`, not starting with `.`, at most
/// 160 bytes). An unsafe key is removed as well and yields `None`: the shell
/// and the worker must agree on the file name, so nothing is rewritten here.
pub fn take_key(req: &mut PromptRequest) -> Option<String> {
    let prefix = format!("{KEY_ENV}=");
    if !req.env.contains(&prefix) {
        return None;
    }
    let mut key = None;
    let kept: Vec<&str> = req
        .env
        .split('\u{1f}')
        .filter(|kv| match kv.strip_prefix(&prefix) {
            Some(v) => {
                key = Some(v.to_string());
                false
            }
            None => true,
        })
        .collect();
    req.env = kept.join("\u{1f}");
    key.filter(|k| valid_key(k))
}

/// Shell-only presentation identity, excluded from provider cache revisions.
#[derive(Debug, Clone, Default)]
pub struct PresentationEnv {
    pairs: Vec<String>,
}
impl PresentationEnv {
    pub fn take(req: &mut PromptRequest) -> Self {
        const KEYS: &[&str] = &["TILDY_FONT_REVISION", "TERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "LC_ALL", "LC_CTYPE", "LANG"];
        let (pairs, kept): (Vec<_>, Vec<_>) = req.env.split('\u{1f}').map(str::to_string)
            .partition(|pair| pair.split_once('=').is_some_and(|(key, _)| KEYS.contains(&key)));
        req.env = kept.join("\u{1f}");
        Self { pairs }
    }
    pub fn restore(&self, req: &mut PromptRequest) {
        for pair in &self.pairs {
            if !req.env.is_empty() { req.env.push('\u{1f}'); }
            req.env.push_str(pair);
        }
    }
    pub fn instant_allowed(&self) -> bool {
        self.pairs.iter().find_map(|p| p.strip_prefix("TILDY_FONT_REVISION=")).map(|v| v.is_empty())
            .unwrap_or_else(|| std::env::var("TILDY_FONT_REVISION").unwrap_or_default().is_empty())
    }
    fn revision(&self) -> String {
        let mut pairs = self.pairs.clone(); pairs.sort();
        crate::config::content_revision(pairs.join("\u{1f}").as_bytes())
    }
}

/// True only when the shell says it is currently showing a real instant hit.
/// The flag is consumed, never exposed as a provider/render environment value.
pub fn take_shown(req: &mut PromptRequest) -> bool {
    let mut shown = false;
    req.env = req.env.split('\u{1f}').filter(|p| {
        if let Some(value) = p.strip_prefix("TILDY_INSTANT_SHOWN=") { shown = value == "1"; false } else { true }
    }).collect::<Vec<_>>().join("\u{1f}");
    shown
}

/// Identity accompanying cached text. Old cache files stay readable by the
/// shell, but cannot opt into handoff protection without this metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    revision: String,
    cwd: PathBuf,
    cols: u16,
    env: String,
    presentation: String,
}
impl Identity {
    pub fn new(req: &PromptRequest, revision: String, presentation: &PresentationEnv) -> Self {
        Self { revision, cwd: req.cwd.clone(), cols: req.cols,
            env: crate::config::content_revision(req.env.as_bytes()), presentation: presentation.revision() }
    }
    /// Metadata uses an ignored record in the data-only instant file. It is
    /// never sourced by Zsh and contains no environment values or commands.
    pub fn append_to(&self, text: &mut String) {
        if let Ok(metadata) = serde_json::to_string(self) { text.push_str("\nM\t"); text.push_str(&metadata); }
    }
    pub fn matches_file(&self, path: &Path) -> bool {
        const MAX_BYTES: u64 = 64 * 1024;
        if std::fs::metadata(path).map_or(true, |m| !m.is_file() || m.len() > MAX_BYTES) { return false; }
        let Ok(text) = std::fs::read_to_string(path) else { return false };
        text.lines().filter_map(|line| line.strip_prefix("M\t")).next_back()
            .and_then(|metadata| serde_json::from_str::<Identity>(metadata).ok()).as_ref() == Some(self)
    }
}

pub fn valid_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= MAX_KEY_LEN
        && !k.starts_with('.')
        && k.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Cache directory from explicit values (absolute paths only, as XDG asks).
pub fn cache_dir_with(dir: Option<OsString>, xdg_cache_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let abs = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(d) = abs(dir) {
        return Some(d);
    }
    if let Some(x) = abs(xdg_cache_home) {
        return Some(x.join("tildy").join("instant"));
    }
    abs(home).map(|h| h.join(".cache").join("tildy").join("instant"))
}

/// Cache directory from the process environment.
pub fn cache_dir() -> Option<PathBuf> {
    cache_dir_with(std::env::var_os(DIR_ENV), std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

pub fn file_name(key: &str) -> String {
    format!("{key}{FILE_SUFFIX}")
}

/// File contents for a rendered prompt (no trailing newline: the atomic writer
/// adds one). Lines are sanitized exactly like RES lines.
pub fn encode(prompt: &RenderedPrompt) -> String {
    let left: Vec<&str> = if prompt.left.is_empty() {
        vec![""]
    } else {
        prompt.left.iter().take(MAX_LEFT_LINES).map(String::as_str).collect()
    };
    let mut out = format!("{FORMAT_HEADER}\t{FORMAT_VERSION}\t{}", left.len());
    for l in left {
        out.push_str("\nL\t");
        out.push_str(&sanitize_prompt_text(l));
    }
    if let Some(r) = prompt.right.as_deref().filter(|r| !r.is_empty()) {
        out.push_str("\nR\t");
        out.push_str(&sanitize_prompt_text(r));
    }
    out
}

/// Remove cache files not modified for `max_age`, then the oldest files beyond
/// `max_entries`. Leftover temp files (`.*.tmp`) older than a minute go too.
/// Returns the number of files removed; errors on single files are ignored.
pub fn prune(dir: &Path, max_age: Duration, max_entries: usize) -> usize {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    let now = SystemTime::now();
    let mut removed = 0;
    let mut kept: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_tmp = name.starts_with('.') && name.ends_with(".tmp");
        if !(name.ends_with(FILE_SUFFIX) || is_tmp) {
            continue;
        }
        let Some(mtime) = entry.metadata().ok().and_then(|m| m.modified().ok()) else { continue };
        let age = now.duration_since(mtime).unwrap_or_default();
        let limit = if is_tmp { Duration::from_secs(60) } else { max_age };
        if age > limit {
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        } else if !is_tmp {
            kept.push((mtime, path));
        }
    }
    if kept.len() > max_entries {
        kept.sort();
        let excess = kept.len() - max_entries;
        for (_, path) in kept.into_iter().take(excess) {
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Background writer: never blocks the worker's frame path on disk.
pub struct InstantWriter {
    tx: Option<Sender<(PathBuf, String)>>,
    handle: Option<thread::JoinHandle<()>>,
    /// Last content submitted per path, to skip identical rewrites.
    last: HashMap<PathBuf, String>,
}

impl InstantWriter {
    /// Start the writer thread. `prune_dir` is pruned once, before the first write.
    pub fn start(prune_dir: Option<PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("tildy-instant".into())
            .spawn(move || writer_loop(rx, prune_dir))
            .ok();
        InstantWriter { tx: handle.as_ref().map(|_| tx), handle, last: HashMap::new() }
    }

    /// Queue `contents` for `path` unless it equals what was last queued there.
    /// Returns true when queued.
    pub fn submit(&mut self, path: PathBuf, contents: String) -> bool {
        if self.last.get(&path) == Some(&contents) {
            return false;
        }
        let Some(tx) = &self.tx else { return false };
        if tx.send((path.clone(), contents.clone())).is_err() {
            return false;
        }
        if self.last.len() >= 64 {
            self.last.clear();
        }
        self.last.insert(path, contents);
        true
    }

    /// Write everything queued, then stop the thread (tests and clean exits).
    pub fn finish(mut self) {
        self.tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for InstantWriter {
    fn drop(&mut self) {
        // Closing the channel lets the thread finish queued writes and exit;
        // a process that is exiting does not wait for it.
        self.tx.take();
    }
}

fn writer_loop(rx: Receiver<(PathBuf, String)>, prune_dir: Option<PathBuf>) {
    let mut pruned = false;
    while let Ok(first) = rx.recv() {
        // Coalesce a burst: only the newest contents per path are written.
        let mut batch: Vec<(PathBuf, String)> = vec![first];
        while let Ok(next) = rx.try_recv() {
            batch.retain(|(p, _)| *p != next.0);
            batch.push(next);
        }
        if !pruned {
            pruned = true;
            if let Some(dir) = &prune_dir {
                prune(dir, MAX_AGE, MAX_ENTRIES);
            }
        }
        for (path, contents) in batch {
            let _ = write_atomic(&path, contents.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RenderStatus;

    fn req(env: &str) -> PromptRequest {
        PromptRequest {
            gen: 1,
            session_id: "s".into(),
            cwd: "/r".into(),
            cols: 80,
            lines: 24,
            last_exit: 0,
            last_duration_ms: 0,
            env: env.into(),
            jobs: 0,
        }
    }

    #[test]
    fn handoff_identity_rejects_changed_context_and_legacy_cache() {
        let dir = tempfile::tempdir().unwrap(); let file = dir.path().join("cache.prompt");
        let request = req("USER=u"); let presentation = PresentationEnv::default();
        let identity = Identity::new(&request, "config-a".into(), &presentation);
        let mut text = "tildy-instant\t1\t1\nL\tNode 22".to_string();
        std::fs::write(&file, &text).unwrap(); assert!(!identity.matches_file(&file));
        identity.append_to(&mut text); std::fs::write(&file, &text).unwrap(); assert!(identity.matches_file(&file));
        assert!(!Identity::new(&request, "config-b".into(), &presentation).matches_file(&file));
        let mut changed = request.clone(); changed.cwd = "/other".into();
        assert!(!Identity::new(&changed, "config-a".into(), &presentation).matches_file(&file));
        changed = request.clone(); changed.cols += 1;
        assert!(!Identity::new(&changed, "config-a".into(), &presentation).matches_file(&file));
        changed = request.clone(); changed.env = "USER=other".into();
        assert!(!Identity::new(&changed, "config-a".into(), &presentation).matches_file(&file));
        changed = req("USER=u\u{1f}TILDY_FONT_REVISION=font-a\u{1f}TERM=xterm");
        let measured = PresentationEnv::take(&mut changed);
        assert_eq!(changed.env, "USER=u"); assert!(!measured.instant_allowed());
        assert!(!Identity::new(&changed, "config-a".into(), &measured).matches_file(&file));
        measured.restore(&mut changed); assert!(changed.env.contains("TERM=xterm"));
    }

    #[test]
    fn key_is_removed_from_env() {
        let mut r = req("USER=u\u{1f}TILDY_INSTANT_KEY=v0.1_c80_tnone_0badf00d\u{1f}PATH=/bin");
        assert_eq!(take_key(&mut r).as_deref(), Some("v0.1_c80_tnone_0badf00d"));
        assert_eq!(r.env, "USER=u\u{1f}PATH=/bin");
        let mut only = req("TILDY_INSTANT_KEY=k1");
        assert_eq!(take_key(&mut only).as_deref(), Some("k1"));
        assert_eq!(only.env, "");
        let mut none = req("USER=u");
        assert_eq!(take_key(&mut none), None);
        assert_eq!(none.env, "USER=u");
    }

    #[test]
    fn unsafe_keys_are_dropped_and_removed() {
        for bad in ["../x", ".hidden", "a/b", "", "sp ace", &"x".repeat(200)] {
            let mut r = req(&format!("TILDY_INSTANT_KEY={bad}\u{1f}USER=u"));
            assert_eq!(take_key(&mut r), None, "{bad:?}");
            assert_eq!(r.env, "USER=u");
        }
    }

    #[test]
    fn cache_dir_resolution() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(cache_dir_with(os("/d"), os("/x"), os("/h")), Some(PathBuf::from("/d")));
        assert_eq!(cache_dir_with(os("rel"), os("/x"), os("/h")), Some(PathBuf::from("/x/tildy/instant")));
        assert_eq!(cache_dir_with(None, os("rel"), os("/h")), Some(PathBuf::from("/h/.cache/tildy/instant")));
        assert_eq!(cache_dir_with(None, None, None), None);
    }

    #[test]
    fn encode_sanitizes_like_res_lines() {
        let p = RenderedPrompt {
            status: RenderStatus::Fresh,
            left: vec!["%F{blue}~/a\tb%f".into(), "%B❯%b ".into()],
            right: Some("%F{red}1%f".into()),
            transient: Some("❯ ".into()),
            history: None,
            observation_seq: None,
        };
        assert_eq!(encode(&p), "tildy-instant\t1\t2\nL\t%F{blue}~/ab%f\nL\t%B❯%b \nR\t%F{red}1%f");
        let empty = RenderedPrompt { status: RenderStatus::Fresh, left: vec![], right: Some(String::new()), transient: None, history: None, observation_seq: None };
        assert_eq!(encode(&empty), "tildy-instant\t1\t1\nL\t");
    }

    #[test]
    fn writer_writes_latest_atomically_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("tildy").join("instant");
        fs::create_dir_all(&d).unwrap();
        let mut w = InstantWriter::start(Some(d.clone()));
        let p = d.join(file_name("k1"));
        assert!(w.submit(p.clone(), "one".into()));
        assert!(!w.submit(p.clone(), "one".into()), "identical content is skipped");
        assert!(w.submit(p.clone(), "two".into()));
        w.finish();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        for i in 0..5 {
            fs::write(d.join(file_name(&format!("e{i}"))), "x").unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        fs::write(d.join("unrelated.txt"), "x").unwrap();
        let removed = prune(&d, MAX_AGE, 3);
        assert_eq!(removed, 3);
        let mut left: Vec<String> = fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        // k1 and e0..e4 are cache files; the three oldest (k1, e0, e1) go.
        assert_eq!(left, vec!["e2.prompt", "e3.prompt", "e4.prompt", "unrelated.txt"]);
        assert_eq!(prune(&d, Duration::from_secs(0), 200), 3, "max age removes the rest");
    }
}
