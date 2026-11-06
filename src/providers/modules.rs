//! Config-driven module providers: `language` (builtin table below), `command`
//! (user-defined), `python` and `docker_context`. Owner: M1b task B.
//!
//! Every fetch scans the direct entries of the request cwd (at most
//! [`MAX_SCAN_ENTRIES`]) and matches the module's detect rules (file names,
//! extensions, folder names). Not detected: a `Fresh` slot without a value, and
//! nothing runs. Detected: the version command runs through a process-wide
//! cache keyed by (cwd, argv, signature of the matched detect files' mtimes);
//! entries live for the module TTL (failures for at most [`FAILURE_TTL`]), and
//! concurrent runs of the same argv in the same cwd are coalesced into one.
//!
//! The worker never runs these on the prompt's critical path: the scheduler
//! sends the `partial` RES first and module fetches start afterwards (see
//! `session::worker`). The detection tables follow Starship's modules (ISC
//! license, Copyright (c) Starship Contributors); no Starship code is copied.

use super::exec;
use super::{intern, Provider, ProviderValue};
use crate::model::{Dependency, Freshness, ModuleValue, PromptRequest, ProviderSpec, Slot};
use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::fs::FileType;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// Directory entries looked at per detection scan.
pub const MAX_SCAN_ENTRIES: usize = 500;
pub const DEFAULT_TTL: Duration = Duration::from_secs(600);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(500);
/// Failed runs (timeout, missing binary, non-zero exit, no version in the
/// output) are retried after at most this long, even with a longer TTL.
pub const FAILURE_TTL: Duration = Duration::from_secs(30);
/// Bytes kept per output stream of a version command.
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_CACHE_ENTRIES: usize = 512;
/// A coalesced waiter gives the running command this much longer than its own
/// timeout before giving up.
const JOIN_GRACE: Duration = Duration::from_millis(100);

const CWD_DEPS: &[Dependency] = &[Dependency::Cwd];
const CWD_ENV_DEPS: &[Dependency] = &[Dependency::Cwd, Dependency::Env];

// ------------------------------------------------------------------ tables

/// Builtin detection rule and version command for one `language`.
#[derive(Debug)]
pub struct LanguageDef {
    pub name: &'static str,
    pub files: &'static [&'static str],
    pub extensions: &'static [&'static str],
    pub folders: &'static [&'static str],
    /// Candidate commands, tried in order; the next one only runs when the
    /// previous executable does not exist.
    pub commands: &'static [&'static [&'static str]],
    /// Version extraction: first participating capture group, searched in
    /// stdout, then stderr.
    pub regex: &'static str,
}

/// The builtin table (`docs/modules.md`, "语言与模块检测表").
pub const LANGUAGES: &[LanguageDef] = &[
    LanguageDef {
        name: "nodejs",
        files: &["package.json", ".node-version", ".nvmrc"],
        extensions: &["js", "mjs", "cjs", "ts", "mts", "cts"],
        folders: &[],
        commands: &[&["node", "--version"]],
        regex: r"v?(\d+\.\d+\.\d+)",
    },
    LanguageDef {
        name: "rust",
        files: &["Cargo.toml"],
        extensions: &["rs"],
        folders: &[],
        commands: &[&["rustc", "--version"]],
        regex: r"rustc (\S+)",
    },
    LanguageDef {
        name: "golang",
        files: &["go.mod", "go.sum", "go.work", "glide.yaml", "Gopkg.yml", ".go-version"],
        extensions: &["go"],
        folders: &[],
        commands: &[&["go", "version"]],
        regex: r"go(\S+)",
    },
    LanguageDef {
        name: "java",
        files: &[
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            ".java-version",
            "deps.edn",
            "project.clj",
            "build.boot",
            ".sdkmanrc",
        ],
        extensions: &["java", "class", "jar", "gradle", "clj", "cljc"],
        folders: &[],
        commands: &[&["java", "-Xinternalversion"]],
        // `OpenJDK 64-Bit Server VM (19.0.2+7-44) for bsd-aarch64 JRE (19.0.2+7-44), built on ...`:
        // the first bare number is "64", so anchor on the JRE (or OpenJ9) version
        // like Starship does.
        regex: r"(?:JRE.*?\(|OpenJ9 )(\d+(?:\.\d+){0,2})",
    },
    LanguageDef {
        name: "c",
        files: &[],
        extensions: &["c", "h"],
        folders: &[],
        commands: &[&["cc", "--version"]],
        // First version number on the first line (`Apple clang version 17.0.0 (...)`,
        // `cc (Ubuntu 11.4.0-1ubuntu1~22.04) 11.4.0`).
        regex: r"\A[^\n]*?(\d+\.\d+(?:\.\d+)*)",
    },
    LanguageDef {
        name: "python",
        files: &[
            "requirements.txt",
            ".python-version",
            "pyproject.toml",
            "Pipfile",
            "tox.ini",
            "setup.py",
            "__init__.py",
        ],
        extensions: &["py", "ipynb"],
        folders: &[],
        commands: &[&["python3", "--version"], &["python", "--version"]],
        regex: r"Python (\S+)",
    },
];

/// Files that make `docker_context` visible (it never runs a command).
pub const DOCKER_FILES: &[&str] =
    &["Dockerfile", "docker-compose.yml", "docker-compose.yaml", "compose.yml", "compose.yaml", ".dockerenv"];

/// Look up a builtin language (`node` and `go` are accepted as aliases).
pub fn language(name: &str) -> Option<&'static LanguageDef> {
    let canonical = match name {
        "node" => "nodejs",
        "go" => "golang",
        other => other,
    };
    LANGUAGES.iter().find(|l| l.name == canonical)
}

// --------------------------------------------------------------- detection

/// Detection rule: the module is visible when any direct entry of the cwd has
/// one of `files` as its name, one of `extensions` as its extension, or is a
/// directory named one of `folders`. An empty rule always matches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detect {
    pub files: Vec<String>,
    pub extensions: Vec<String>,
    pub folders: Vec<String>,
}

impl Detect {
    pub fn from_static(files: &[&str], extensions: &[&str], folders: &[&str]) -> Self {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        Detect { files: own(files), extensions: own(extensions), folders: own(folders) }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.extensions.is_empty() && self.folders.is_empty()
    }

    /// `None` when not detected. Otherwise a signature of the entries matched by
    /// name (files and folders) and their mtimes: it changes when one of them
    /// is created, removed or modified, which invalidates cached versions (a new
    /// `.nvmrc`, an edited `.python-version`). Extension matches are not part
    /// of it: source files do not select a toolchain.
    pub fn check(&self, scan: &DirScan, dir: &Path) -> Option<u64> {
        if !scan.readable {
            return None;
        }
        let mut detected = self.is_empty();
        let mut named: Vec<(&str, Option<SystemTime>)> = Vec::new();
        for e in &scan.entries {
            if self.files.iter().any(|f| f == &e.name) {
                detected = true;
                named.push((&e.name, mtime(&dir.join(&e.name))));
                continue;
            }
            if self.folders.iter().any(|f| f == &e.name) && e.is_dir(dir) {
                detected = true;
                named.push((&e.name, mtime(&dir.join(&e.name))));
                continue;
            }
            if !detected {
                if let Some(ext) = &e.ext {
                    detected = self.extensions.iter().any(|x| x == ext);
                }
            }
        }
        if !detected {
            return None;
        }
        named.sort();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        named.hash(&mut h);
        Some(h.finish())
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// The first [`MAX_SCAN_ENTRIES`] direct entries of a directory.
#[derive(Debug, Default)]
pub struct DirScan {
    entries: Vec<ScanEntry>,
    /// The directory could be listed at all.
    pub readable: bool,
    /// More entries existed than were looked at.
    pub truncated: bool,
}

#[derive(Debug)]
struct ScanEntry {
    name: String,
    ext: Option<String>,
    file_type: Option<FileType>,
}

impl ScanEntry {
    fn is_dir(&self, dir: &Path) -> bool {
        match self.file_type {
            Some(t) if t.is_symlink() => dir.join(&self.name).is_dir(),
            Some(t) => t.is_dir(),
            None => dir.join(&self.name).is_dir(),
        }
    }
}

impl DirScan {
    pub fn read(dir: &Path) -> DirScan {
        let Ok(rd) = std::fs::read_dir(dir) else { return DirScan::default() };
        let mut scan = DirScan { readable: true, ..DirScan::default() };
        for entry in rd.flatten() {
            if scan.entries.len() == MAX_SCAN_ENTRIES {
                scan.truncated = true;
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let ext = Path::new(&name).extension().map(|e| e.to_string_lossy().into_owned());
            scan.entries.push(ScanEntry { name, ext, file_type: entry.file_type().ok() });
        }
        scan
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ----------------------------------------------------------- command cache

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Done { success: bool, code: Option<i32>, stdout: String, stderr: String },
    Timeout,
    NotFound,
    Failed(String),
}

impl Outcome {
    fn validity(&self, ttl: Duration) -> Duration {
        match self {
            Outcome::Done { success: true, .. } => ttl,
            _ => ttl.min(FAILURE_TTL),
        }
    }
}

/// How a version lookup was served (tests and measurements).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheUse {
    Hit,
    Ran,
    Joined,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RunKey {
    cwd: PathBuf,
    argv: Vec<String>,
    /// PATH forwarded by the shell (nvm/conda switch it); None = worker's own.
    path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    run: RunKey,
    signature: u64,
}

#[derive(Default)]
struct Flight {
    done: Mutex<Option<Outcome>>,
    cv: Condvar,
}

impl Flight {
    fn wait(&self, deadline: Instant) -> Option<Outcome> {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(o) = done.as_ref() {
                return Some(o.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            done = self.cv.wait_timeout(done, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

#[derive(Default)]
struct CommandCache {
    entries: HashMap<CacheKey, (Outcome, Instant)>,
    inflight: HashMap<RunKey, Arc<Flight>>,
}

static CACHE: OnceLock<Mutex<CommandCache>> = OnceLock::new();
static RUNS: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
static JOINED: AtomicU64 = AtomicU64::new(0);

fn cache() -> std::sync::MutexGuard<'static, CommandCache> {
    CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// Process-wide counters of the version-command cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub runs: u64,
    pub hits: u64,
    pub joined: u64,
}

pub fn cache_stats() -> CacheStats {
    CacheStats {
        runs: RUNS.load(Ordering::Relaxed),
        hits: HITS.load(Ordering::Relaxed),
        joined: JOINED.load(Ordering::Relaxed),
    }
}

/// Removes the in-flight marker and wakes waiters even if the runner panics.
struct InflightGuard {
    run: RunKey,
    flight: Arc<Flight>,
    result: Option<Outcome>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        {
            let mut c = cache();
            if c.inflight.get(&self.run).is_some_and(|f| Arc::ptr_eq(f, &self.flight)) {
                c.inflight.remove(&self.run);
            }
        }
        let o = self.result.take().unwrap_or_else(|| Outcome::Failed("version command runner panicked".into()));
        *self.flight.done.lock().unwrap_or_else(|e| e.into_inner()) = Some(o);
        self.flight.cv.notify_all();
    }
}

fn run_cached(cwd: &Path, argv: &[String], signature: u64, ttl: Duration, timeout: Duration, path: Option<&str>) -> (Outcome, CacheUse) {
    let run = RunKey { cwd: cwd.to_path_buf(), argv: argv.to_vec(), path: path.map(str::to_owned) };
    let key = CacheKey { run: run.clone(), signature };
    let flight = {
        let mut c = cache();
        if let Some((o, at)) = c.entries.get(&key) {
            if at.elapsed() < o.validity(ttl) {
                HITS.fetch_add(1, Ordering::Relaxed);
                return (o.clone(), CacheUse::Hit);
            }
        }
        if let Some(f) = c.inflight.get(&run).cloned() {
            drop(c);
            JOINED.fetch_add(1, Ordering::Relaxed);
            // The runner caches under its own signature; a joiner whose
            // signature differs uses the result once and runs again next time.
            let o = f.wait(Instant::now() + timeout + JOIN_GRACE).unwrap_or(Outcome::Timeout);
            return (o, CacheUse::Joined);
        }
        let f = Arc::new(Flight::default());
        c.inflight.insert(run.clone(), Arc::clone(&f));
        f
    };
    let mut guard = InflightGuard { run, flight, result: None };
    let o = execute(cwd, argv, timeout, path);
    RUNS.fetch_add(1, Ordering::Relaxed);
    {
        let mut c = cache();
        c.entries.insert(key, (o.clone(), Instant::now()));
        while c.entries.len() > MAX_CACHE_ENTRIES {
            let oldest = c.entries.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    c.entries.remove(&k);
                }
                None => break,
            }
        }
    }
    guard.result = Some(o.clone());
    drop(guard);
    (o, CacheUse::Ran)
}

fn execute(cwd: &Path, argv: &[String], timeout: Duration, path: Option<&str>) -> Outcome {
    let Some((prog, args)) = argv.split_first() else {
        return Outcome::Failed("empty argv".into());
    };
    let mut cmd = Command::new(prog);
    cmd.args(args).current_dir(cwd);
    if let Some(p) = path.filter(|p| !p.is_empty()) {
        // The shell's current PATH: `nvm use` / `conda activate` must be visible here.
        cmd.env("PATH", p);
    }
    match exec::run(cmd, Instant::now() + timeout, MAX_OUTPUT) {
        Ok(o) => Outcome::Done {
            success: o.success,
            code: o.code,
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(exec::RunErr::Timeout) => Outcome::Timeout,
        Err(exec::RunErr::NotFound) => Outcome::NotFound,
        Err(exec::RunErr::Spawn(m)) => Outcome::Failed(m),
    }
}

// ----------------------------------------------------------------- version

/// A version command: candidates tried in order (the next one only when the
/// previous executable is missing) and the extraction rule.
#[derive(Debug, Clone)]
pub struct VersionCommand {
    pub argvs: Vec<Vec<String>>,
    /// First participating capture group (whole match if the regex has no
    /// groups), searched in stdout, then stderr. `None`: first non-empty line
    /// of stdout.
    pub regex: Option<Regex>,
}

type Failure = (Freshness, String);

fn show(argv: &[String]) -> String {
    argv.join(" ")
}

/// Extract the version from a finished command. `Ok(None)`: the command
/// succeeded with no output (no regex), which hides the module.
fn interpret(o: &Outcome, argv: &[String], regex: Option<&Regex>, timeout: Duration) -> Result<Option<String>, Failure> {
    match o {
        Outcome::Done { success: true, stdout, stderr, .. } => match regex {
            Some(re) => [stdout, stderr]
                .iter()
                .find_map(|text| first_group(re, text))
                .map(Some)
                .ok_or_else(|| (Freshness::Error, format!("no version in the output of `{}`", show(argv)))),
            None => Ok(stdout.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)),
        },
        Outcome::Done { success: false, code, stderr, .. } => {
            let code = code.map_or("a signal".to_string(), |c| format!("code {c}"));
            let first = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
            Err((Freshness::Error, format!("`{}` exited with {code}: {first}", show(argv)).trim_end_matches(": ").to_string()))
        }
        Outcome::Timeout => {
            Err((Freshness::Timeout, format!("`{}` timed out after {} ms", show(argv), timeout.as_millis())))
        }
        Outcome::NotFound => Err((Freshness::Error, format!("{} not found in PATH", argv.first().map_or("", |s| s)))),
        Outcome::Failed(m) => Err((Freshness::Error, format!("cannot run `{}`: {m}", show(argv)))),
    }
}

fn first_group(re: &Regex, text: &str) -> Option<String> {
    let caps = re.captures(text)?;
    let m = if caps.len() > 1 { caps.iter().skip(1).flatten().find(|m| !m.as_str().is_empty())? } else { caps.get(0)? };
    let s = m.as_str().trim();
    (!s.is_empty()).then(|| s.to_string())
}

impl VersionCommand {
    /// Try `first` (if any), then the configured candidates.
    fn lookup(
        &self,
        first: Option<&[String]>,
        cwd: &Path,
        signature: u64,
        ttl: Duration,
        timeout: Duration,
        path: Option<&str>,
    ) -> (Result<Option<String>, Failure>, CacheUse) {
        let mut last = (Err((Freshness::Error, "no version command".to_string())), CacheUse::Ran);
        for argv in first.into_iter().chain(self.argvs.iter().map(Vec::as_slice)) {
            let (o, used) = run_cached(cwd, argv, signature, ttl, timeout, path);
            last = (interpret(&o, argv, self.regex.as_ref(), timeout), used);
            if o != Outcome::NotFound {
                break;
            }
        }
        last
    }
}

// ---------------------------------------------------------------- provider

enum Kind {
    /// `language` (except python) and `command`.
    Command { detect: Detect, version: VersionCommand },
    /// Like `Command`, plus: visible when `VIRTUAL_ENV` is set, the venv's own
    /// interpreter is tried first, and `fields.env` names the environment.
    Python { detect: Detect, version: VersionCommand },
    DockerContext { detect: Detect },
}

/// One config module instance. Its provider key is the module id.
pub struct ModuleProvider {
    id: String,
    spec: ProviderSpec,
    kind: Kind,
}

fn spec(id: &str, deps: &'static [Dependency], ttl: Duration, timeout: Duration) -> ProviderSpec {
    ProviderSpec { key: intern(id), deps, ttl, timeout }
}

fn not_detected() -> Slot<ModuleValue> {
    Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: None, error: None }
}

fn failed((freshness, msg): Failure) -> Slot<ModuleValue> {
    Slot { freshness, age_ms: None, value: None, error: Some(msg) }
}

fn value(text: String, fields: BTreeMap<String, String>) -> Slot<ModuleValue> {
    Slot::fresh(ModuleValue { text, fields, prerendered: None })
}

fn compile_builtin(re: &str) -> Regex {
    Regex::new(re).expect("builtin version regex compiles")
}

impl ModuleProvider {
    /// A builtin language (`python` gets the python behaviour).
    pub fn language(id: &str, def: &LanguageDef, ttl: Duration, timeout: Duration) -> Self {
        let detect = Detect::from_static(def.files, def.extensions, def.folders);
        let version = VersionCommand {
            argvs: def.commands.iter().map(|c| c.iter().map(|s| s.to_string()).collect()).collect(),
            regex: Some(compile_builtin(def.regex)),
        };
        if def.name == "python" {
            return ModuleProvider {
                id: id.to_string(),
                spec: spec(id, CWD_ENV_DEPS, ttl, timeout),
                kind: Kind::Python { detect, version },
            };
        }
        ModuleProvider { id: id.to_string(), spec: spec(id, CWD_DEPS, ttl, timeout), kind: Kind::Command { detect, version } }
    }

    pub fn python(id: &str, ttl: Duration, timeout: Duration) -> Self {
        Self::language(id, language("python").expect("python is builtin"), ttl, timeout)
    }

    /// A user-defined command module. An empty `detect` runs in every directory.
    pub fn command(id: &str, argv: Vec<String>, detect: Detect, regex: Option<Regex>, ttl: Duration, timeout: Duration) -> Self {
        ModuleProvider {
            id: id.to_string(),
            spec: spec(id, CWD_DEPS, ttl, timeout),
            kind: Kind::Command { detect, version: VersionCommand { argvs: vec![argv], regex } },
        }
    }

    pub fn docker_context(id: &str, ttl: Duration, timeout: Duration) -> Self {
        ModuleProvider {
            id: id.to_string(),
            spec: spec(id, CWD_ENV_DEPS, ttl, timeout),
            kind: Kind::DockerContext { detect: Detect::from_static(DOCKER_FILES, &[], &[]) },
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// The detection rule (for config overrides of the builtin lists).
    pub fn detect_mut(&mut self) -> &mut Detect {
        match &mut self.kind {
            Kind::Command { detect, .. } | Kind::Python { detect, .. } | Kind::DockerContext { detect } => detect,
        }
    }

    /// The slot for `req`, plus how the version was served (`None` when no
    /// command was involved).
    pub fn fetch_slot(&self, req: &PromptRequest) -> (Slot<ModuleValue>, Option<CacheUse>) {
        let (ttl, timeout) = (self.spec.ttl, self.spec.timeout);
        match &self.kind {
            Kind::Command { detect, version } => {
                let scan = DirScan::read(&req.cwd);
                let Some(sig) = detect.check(&scan, &req.cwd) else { return (not_detected(), None) };
                let envm = req.env_map();
                let path = envm.get("PATH").map(String::as_str);
                let (r, used) = version.lookup(None, &req.cwd, sig, ttl, timeout, path);
                let slot = match r {
                    Ok(Some(v)) => value(v.clone(), BTreeMap::from([("version".to_string(), v)])),
                    Ok(None) => not_detected(),
                    Err(f) => failed(f),
                };
                (slot, Some(used))
            }
            Kind::Python { detect, version } => {
                let env = req.env_map();
                let venv = env.get("VIRTUAL_ENV").filter(|v| !v.is_empty());
                let scan = DirScan::read(&req.cwd);
                let sig = detect.check(&scan, &req.cwd);
                if sig.is_none() && venv.is_none() {
                    return (not_detected(), None);
                }
                let own = venv.map(|v| vec![Path::new(v).join("bin/python").to_string_lossy().into_owned(), "--version".into()]);
                let env_name = venv
                    .map(|v| venv_name(Path::new(v)))
                    .or_else(|| env.get("CONDA_DEFAULT_ENV").filter(|c| !c.is_empty()).cloned());
                let path = env.get("PATH").map(String::as_str);
                let (r, used) = version.lookup(own.as_deref(), &req.cwd, sig.unwrap_or(0), ttl, timeout, path);
                let mut fields = BTreeMap::new();
                if let Some(e) = &env_name {
                    fields.insert("env".to_string(), e.clone());
                }
                let slot = match r {
                    Ok(Some(v)) => {
                        fields.insert("version".to_string(), v.clone());
                        value(v, fields)
                    }
                    Ok(None) if env_name.is_some() => value(String::new(), fields),
                    Ok(None) => not_detected(),
                    // The environment name is still worth showing without a version.
                    Err((_, msg)) if env_name.is_some() => {
                        let mut s = value(String::new(), fields);
                        s.error = Some(msg);
                        s
                    }
                    Err(f) => failed(f),
                };
                (slot, Some(used))
            }
            Kind::DockerContext { detect } => {
                let scan = DirScan::read(&req.cwd);
                if detect.check(&scan, &req.cwd).is_none() {
                    return (not_detected(), None);
                }
                let slot = match docker_context(&req.env_map(), docker_config_path().as_deref()) {
                    Some(ctx) => value(ctx.clone(), BTreeMap::from([("context".to_string(), ctx)])),
                    None => not_detected(),
                };
                (slot, None)
            }
        }
    }
}

impl Provider for ModuleProvider {
    fn watch_paths(&self, _req: &PromptRequest) -> Vec<PathBuf> {
        if matches!(self.kind, Kind::DockerContext { .. }) { docker_config_path().map(|p| p.canonicalize().unwrap_or(p)).into_iter().collect() } else { Vec::new() }
    }
    fn affected_by_paths(&self, req: &PromptRequest, paths: &[PathBuf]) -> bool {
        if self.watch_paths(req).iter().any(|target| paths.iter().any(|path| path == target || target.starts_with(path))) { return true; }
        let detect = match &self.kind {
            Kind::Command { detect, .. }
            | Kind::Python { detect, .. }
            | Kind::DockerContext { detect } => detect,
        };
        if detect.is_empty() {
            return paths.iter().any(|p| p.starts_with(&req.cwd));
        }
        let cwd = req.cwd.canonicalize().unwrap_or_else(|_| req.cwd.clone());
        paths.iter().any(|path| {
            let Ok(relative) = path.strip_prefix(&cwd) else {
                return false;
            };
            if relative.as_os_str().is_empty() {
                return true;
            }
            let Some(first) = relative
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
            else {
                return false;
            };
            detect.folders.iter().any(|f| f == first)
                || (relative.components().count() == 1
                    && (detect.files.iter().any(|f| f == first)
                        || path
                            .extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| detect.extensions.iter().any(|x| x == e))))
        })
    }
    fn spec(&self) -> &ProviderSpec {
        &self.spec
    }
    fn fetch(&self, req: &PromptRequest) -> ProviderValue {
        ProviderValue::Module(self.id.clone(), self.fetch_slot(req).0)
    }
}

/// Virtualenv display name: `prompt` from `pyvenv.cfg` when set (quotes and
/// parentheses trimmed), else the directory's basename.
pub fn venv_name(venv: &Path) -> String {
    let from_cfg = std::fs::read_to_string(venv.join("pyvenv.cfg")).ok().and_then(|cfg| {
        cfg.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "prompt").then(|| v.trim().trim_matches(['"', '\'', '(', ')']).trim().to_string())
        })
    });
    from_cfg
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| venv.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
}

/// `$DOCKER_CONFIG/config.json`, else `~/.docker/config.json` (worker env).
fn docker_config_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("DOCKER_CONFIG").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("config.json"));
    }
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".docker/config.json"))
}

/// Docker context name from the request env (`DOCKER_HOST`, then
/// `DOCKER_CONTEXT`), else `currentContext` in the docker config file.
/// Contexts Starship hides (`default`, `desktop-linux`, `unix://…` sockets,
/// empty) give `None`.
pub fn docker_context(env: &BTreeMap<String, String>, config: Option<&Path>) -> Option<String> {
    let from_env = ["DOCKER_HOST", "DOCKER_CONTEXT"].iter().find_map(|k| env.get(*k).filter(|v| !v.is_empty()).cloned());
    let ctx = from_env.or_else(|| {
        let path = config?;
        if std::fs::metadata(path).ok()?.len() > 1 << 20 {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        v.get("currentContext")?.as_str().map(str::to_string)
    })?;
    let ctx = ctx.trim().to_string();
    if ctx.is_empty() || ctx == "default" || ctx == "desktop-linux" || ctx.starts_with("unix://") {
        return None;
    }
    Some(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(def: &str) -> Regex {
        compile_builtin(language(def).unwrap().regex)
    }

    #[test]
    fn builtin_regexes_extract_real_outputs() {
        let cases = [
            ("nodejs", "v24.14.0\n", "24.14.0"),
            ("rust", "rustc 1.95.0 (59807616e 2026-04-14)\n", "1.95.0"),
            ("golang", "go version go1.26.5 darwin/arm64\n", "1.26.5"),
            (
                "java",
                "OpenJDK 64-Bit Server VM (19.0.2+7-44) for bsd-aarch64 JRE (19.0.2+7-44), built on 2022-11-30T18:02:09Z by \"mach5one\" with clang Apple LLVM 12.0.0 (clang-1200.0.32.29)\n",
                "19.0.2",
            ),
            ("java", "OpenJDK 64-Bit Server VM (25.392-b08) for bsd-amd64 JRE (1.8.0_392-b08), built on ...\n", "1.8.0"),
            ("java", "Eclipse OpenJ9 VM (17.0.2+8) ... OpenJ9 17.0.2 ...\n", "17.0.2"),
            ("c", "Apple clang version 21.0.0 (clang-2100.3.34.2)\nTarget: arm64-apple-darwin\n", "21.0.0"),
            ("c", "cc (Ubuntu 11.4.0-1ubuntu1~22.04) 11.4.0\nCopyright (C) 2021\n", "11.4.0"),
            ("python", "Python 3.12.7\n", "3.12.7"),
        ];
        for (lang, out, want) in cases {
            assert_eq!(first_group(&re(lang), out).as_deref(), Some(want), "{lang}: {out}");
        }
        // c: only the first line counts.
        assert_eq!(first_group(&re("c"), "no digits here\nversion 1.2.3\n"), None);
        assert!(language("node").is_some() && language("go").is_some() && language("ruby").is_none());
    }

    #[test]
    fn interpret_outcomes() {
        let argv = vec!["tool".to_string(), "--version".to_string()];
        let t = Duration::from_millis(500);
        let done = |success, stdout: &str, stderr: &str| Outcome::Done {
            success,
            code: Some(if success { 0 } else { 2 }),
            stdout: stdout.into(),
            stderr: stderr.into(),
        };
        let py = re("python");
        assert_eq!(interpret(&done(true, "", "Python 2.7.18\n"), &argv, Some(&py), t), Ok(Some("2.7.18".into())), "stderr fallback");
        assert_eq!(interpret(&done(true, "\n  hello  \nx", ""), &argv, None, t), Ok(Some("hello".into())));
        assert_eq!(interpret(&done(true, "", ""), &argv, None, t), Ok(None));
        let e = interpret(&done(true, "garbage", ""), &argv, Some(&py), t).unwrap_err();
        assert_eq!(e.0, Freshness::Error);
        let e = interpret(&done(false, "Python 3.1.1", "boom\nmore"), &argv, Some(&py), t).unwrap_err();
        assert_eq!(e, (Freshness::Error, "`tool --version` exited with code 2: boom".into()));
        let e = interpret(&Outcome::Timeout, &argv, None, t).unwrap_err();
        assert_eq!(e, (Freshness::Timeout, "`tool --version` timed out after 500 ms".into()));
        assert_eq!(interpret(&Outcome::NotFound, &argv, None, t).unwrap_err().1, "tool not found in PATH");
    }

    #[test]
    fn docker_context_sources_and_hidden_values() {
        let td = tempfile::tempdir().unwrap();
        let cfg = td.path().join("config.json");
        std::fs::write(&cfg, r#"{"auths":{},"currentContext":"colima"}"#).unwrap();
        let env = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        assert_eq!(docker_context(&env(&[]), Some(&cfg)).as_deref(), Some("colima"));
        assert_eq!(docker_context(&env(&[("DOCKER_CONTEXT", "ctx")]), Some(&cfg)).as_deref(), Some("ctx"));
        assert_eq!(
            docker_context(&env(&[("DOCKER_CONTEXT", "ctx"), ("DOCKER_HOST", "tcp://h:2375")]), Some(&cfg)).as_deref(),
            Some("tcp://h:2375"),
            "DOCKER_HOST wins, as in Starship"
        );
        for hidden in ["default", "desktop-linux", "unix:///var/run/docker.sock", ""] {
            assert_eq!(docker_context(&env(&[("DOCKER_HOST", hidden)]), None), None, "{hidden:?}");
        }
        std::fs::write(&cfg, r#"{"currentContext":"desktop-linux"}"#).unwrap();
        assert_eq!(docker_context(&env(&[]), Some(&cfg)), None);
        std::fs::write(&cfg, "not json").unwrap();
        assert_eq!(docker_context(&env(&[]), Some(&cfg)), None);
        assert_eq!(docker_context(&env(&[]), Some(&td.path().join("missing.json"))), None);
    }

    #[test]
    fn venv_names() {
        let td = tempfile::tempdir().unwrap();
        let v = td.path().join("myenv");
        std::fs::create_dir(&v).unwrap();
        assert_eq!(venv_name(&v), "myenv");
        std::fs::write(v.join("pyvenv.cfg"), "home = /usr/bin\nprompt = '(proj)'\n").unwrap();
        assert_eq!(venv_name(&v), "proj");
        std::fs::write(v.join("pyvenv.cfg"), "prompt =   \n").unwrap();
        assert_eq!(venv_name(&v), "myenv");
    }

    #[test]
    fn detection_rules() {
        let td = tempfile::tempdir().unwrap();
        let d = td.path();
        let node = Detect::from_static(&["package.json"], &["ts"], &["node_modules"]);
        let scan = DirScan::read(d);
        assert!(scan.readable && scan.is_empty());
        assert_eq!(node.check(&scan, d), None);
        assert!(Detect::default().check(&scan, d).is_some(), "empty rule always matches");
        assert_eq!(Detect::default().check(&DirScan::read(&d.join("missing")), d), None, "unreadable dir");

        std::fs::write(d.join("x.d.ts"), "").unwrap();
        std::fs::write(d.join(".ts"), "").unwrap();
        let ext_sig = node.check(&DirScan::read(d), d).expect("extension match");
        std::fs::remove_file(d.join("x.d.ts")).unwrap();
        assert_eq!(node.check(&DirScan::read(d), d), None, "a dotfile named .ts has no extension");

        std::fs::write(d.join("node_modules"), "").unwrap();
        assert_eq!(node.check(&DirScan::read(d), d), None, "folder rule needs a directory");
        std::fs::remove_file(d.join("node_modules")).unwrap();
        std::fs::create_dir(d.join("node_modules")).unwrap();
        let dir_sig = node.check(&DirScan::read(d), d).expect("folder match");
        assert_ne!(dir_sig, ext_sig);

        std::fs::write(d.join("package.json"), "{}").unwrap();
        let s1 = node.check(&DirScan::read(d), d).unwrap();
        assert_eq!(node.check(&DirScan::read(d), d), Some(s1), "stable signature");
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(d.join("package.json")).unwrap().set_modified(later).unwrap();
        assert_ne!(node.check(&DirScan::read(d), d), Some(s1), "mtime change invalidates");
    }

    #[test]
    fn scan_is_capped() {
        let td = tempfile::tempdir().unwrap();
        for i in 0..(MAX_SCAN_ENTRIES + 20) {
            std::fs::write(td.path().join(format!("f{i}")), "").unwrap();
        }
        let scan = DirScan::read(td.path());
        assert_eq!(scan.len(), MAX_SCAN_ENTRIES);
        assert!(scan.truncated);
    }
}
