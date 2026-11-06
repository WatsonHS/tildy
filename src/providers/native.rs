//! Providers of the Starship-parity modules added in M2 (task E1): `package`,
//! `git_commit` (tags and long hashes), `git_metrics`, `localip`,
//! `memory_usage`, `sudo` and `direnv`. Each instance is a [`NativeProvider`]
//! whose key is the module id; `providers::from_config` builds them only for
//! modules placed in the layout.
//!
//! Commands run through `exec::run` (own process group, killed at the
//! timeout). The scheduler caches every slot (key: provider, cwd or global,
//! env revision) and revalidates `Cwd`/`Time` providers on each prompt in the
//! background, so nothing here is on the prompt's critical path. Behaviour
//! follows Starship's modules of the same names (ISC license, Copyright (c)
//! Starship Contributors); no code is copied.

use super::exec;
use super::modules::{DirScan, Detect};
use super::{intern, Provider, ProviderValue};
use crate::model::{Dependency, Freshness, ModuleValue, PromptRequest, ProviderSpec, Slot};
use regex::Regex;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Bytes kept per output stream of a command.
const MAX_OUTPUT: usize = 64 * 1024;

type FetchFn = dyn Fn(&PromptRequest, Duration) -> Slot<ModuleValue> + Send + Sync;

/// A module provider defined by a fetch function.
pub struct NativeProvider {
    id: String,
    spec: ProviderSpec,
    fetch: Box<FetchFn>,
}

impl NativeProvider {
    pub fn new(
        id: &str,
        deps: &'static [Dependency],
        ttl: Duration,
        timeout: Duration,
        fetch: impl Fn(&PromptRequest, Duration) -> Slot<ModuleValue> + Send + Sync + 'static,
    ) -> Self {
        NativeProvider { id: id.to_string(), spec: ProviderSpec { key: intern(id), deps, ttl, timeout }, fetch: Box::new(fetch) }
    }

    /// The slot for `req` (tests).
    pub fn fetch_slot(&self, req: &PromptRequest) -> Slot<ModuleValue> {
        (self.fetch)(req, self.spec.timeout)
    }
}

impl Provider for NativeProvider {
    fn spec(&self) -> &ProviderSpec {
        &self.spec
    }
    fn fetch(&self, req: &PromptRequest) -> ProviderValue {
        ProviderValue::Module(self.id.clone(), self.fetch_slot(req))
    }
}

pub const CWD_DEPS: &[Dependency] = &[Dependency::Cwd];
pub const CWD_ENV_DEPS: &[Dependency] = &[Dependency::Cwd, Dependency::Env];
pub const TIME_DEPS: &[Dependency] = &[Dependency::Time];
pub const ENV_TIME_DEPS: &[Dependency] = &[Dependency::Env, Dependency::Time];

pub fn not_detected() -> Slot<ModuleValue> {
    Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: None, error: None }
}

fn failed(freshness: Freshness, msg: impl Into<String>) -> Slot<ModuleValue> {
    Slot { freshness, age_ms: None, value: None, error: Some(msg.into()) }
}

fn value(text: impl Into<String>, fields: &[(&str, String)]) -> Slot<ModuleValue> {
    Slot::fresh(ModuleValue {
        text: text.into(),
        fields: fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        prerendered: None,
    })
}

// ------------------------------------------------------------------ package

/// `package`: the manifest version of the cwd (`fields.version`, cleaned;
/// the renderer applies `version_format`).
pub fn package_slot(req: &PromptRequest, display_private: bool) -> Slot<ModuleValue> {
    match super::package::package_version(&req.cwd, display_private) {
        Some(v) => value(v.clone(), &[("version", v)]),
        None => not_detected(),
    }
}

// --------------------------------------------------------------------- git

enum GitRun {
    Ok(String),
    /// Ran and exited non-zero (stderr's first line).
    Exit(String),
    Fail(Slot<ModuleValue>),
}

fn run_git(cwd: &Path, args: &[&str], deadline: Instant) -> GitRun {
    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks").arg("-C").arg(cwd).args(args).env("LC_ALL", "C").env("GIT_TERMINAL_PROMPT", "0");
    match exec::run(cmd, deadline, MAX_OUTPUT) {
        Ok(o) if o.success => GitRun::Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => GitRun::Exit(String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("").trim().to_string()),
        Err(exec::RunErr::Timeout) => GitRun::Fail(failed(Freshness::Timeout, format!("git {} timed out", args.join(" ")))),
        Err(exec::RunErr::NotFound) => GitRun::Fail(failed(Freshness::Error, "git not found in PATH")),
        Err(exec::RunErr::Spawn(m)) => GitRun::Fail(failed(Freshness::Error, format!("cannot run git: {m}"))),
    }
}

fn not_a_repo(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("not a git repository") || s.contains("must be run in a work tree")
}

/// `git_commit` extras: `fields.hash` (full object name, with `full_hash`)
/// and `fields.tag` (with `tags = Some(max_distance)`: the nearest tag at
/// most that many commits behind HEAD, from `git describe --tags --long`).
/// The short hash shown by default comes from the git provider instead.
pub fn git_commit_slot(req: &PromptRequest, timeout: Duration, full_hash: bool, tags: Option<u32>) -> Slot<ModuleValue> {
    let deadline = Instant::now() + timeout;
    let mut fields: Vec<(&str, String)> = vec![];
    if full_hash {
        match run_git(&req.cwd, &["rev-parse", "--verify", "-q", "HEAD"], deadline) {
            GitRun::Ok(out) => fields.push(("hash", out.trim().to_string())),
            GitRun::Exit(e) if not_a_repo(&e) => return not_detected(),
            GitRun::Exit(_) => {} // unborn HEAD: no hash
            GitRun::Fail(s) => return s,
        }
    }
    if let Some(max) = tags {
        match run_git(&req.cwd, &["describe", "--tags", "--long", "HEAD"], deadline) {
            GitRun::Ok(out) => {
                if let Some(tag) = describe_tag(out.trim(), max) {
                    fields.push(("tag", tag));
                }
            }
            GitRun::Exit(e) if not_a_repo(&e) => return not_detected(),
            GitRun::Exit(_) => {} // no tags
            GitRun::Fail(s) => return s,
        }
    }
    value(String::new(), &fields)
}

/// `TAG-N-gHASH` -> TAG when N <= max.
fn describe_tag(out: &str, max: u32) -> Option<String> {
    let mut it = out.rsplitn(3, '-');
    let (_hash, n, tag) = (it.next()?, it.next()?, it.next()?);
    let n: u32 = n.parse().ok()?;
    (n <= max && !tag.is_empty()).then(|| tag.to_string())
}

/// `git_metrics`: lines added and deleted between HEAD and the worktree
/// (staged and unstaged, untracked files excluded), from
/// `git diff HEAD --shortstat`; an unborn HEAD diffs the index instead.
/// `fields.added`/`fields.deleted` are decimal counts ("0" when none).
pub fn git_metrics_slot(req: &PromptRequest, timeout: Duration, ignore_submodules: bool) -> Slot<ModuleValue> {
    let deadline = Instant::now() + timeout;
    let mut args = vec!["diff", "HEAD", "--shortstat", "--no-ext-diff", "--no-textconv"];
    if ignore_submodules {
        args.push("--ignore-submodules");
    }
    let out = match run_git(&req.cwd, &args, deadline) {
        GitRun::Ok(o) => o,
        GitRun::Exit(e) if not_a_repo(&e) => return not_detected(),
        GitRun::Exit(_) => {
            args.remove(1);
            match run_git(&req.cwd, &args, deadline) {
                GitRun::Ok(o) => o,
                GitRun::Exit(e) if not_a_repo(&e) => return not_detected(),
                GitRun::Exit(e) => return failed(Freshness::Error, format!("git diff failed: {e}")),
                GitRun::Fail(s) => return s,
            }
        }
        GitRun::Fail(s) => return s,
    };
    let (added, deleted) = parse_shortstat(&out);
    value(String::new(), &[("added", added.to_string()), ("deleted", deleted.to_string())])
}

/// ` 3 files changed, 10 insertions(+), 2 deletions(-)` -> (10, 2).
pub fn parse_shortstat(s: &str) -> (u64, u64) {
    let num = |re: &str| {
        Regex::new(re).expect("shortstat regex").captures(s).and_then(|c| c.get(1)).and_then(|m| m.as_str().parse().ok()).unwrap_or(0)
    };
    (num(r"(\d+) \w+\(\+\)"), num(r"(\d+) \w+\(-\)"))
}

// ---------------------------------------------------------------- localip

/// `localip`: the IPv4 address of the interface that routes to the outside
/// (a UDP socket "connected" to a TEST-NET address; no packet is sent).
/// With `ssh_only`, only when the shell forwards `SSH_CONNECTION`.
pub fn localip_slot(req: &PromptRequest, ssh_only: bool) -> Slot<ModuleValue> {
    if ssh_only && req.env_map().get("SSH_CONNECTION").is_none_or(|v| v.is_empty()) {
        return not_detected();
    }
    let ip = (|| {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        s.connect("192.0.2.0:80").ok()?;
        Some(s.local_addr().ok()?.ip().to_string())
    })();
    match ip {
        Some(ip) => value(ip.clone(), &[("localipv4", ip)]),
        None => failed(Freshness::Error, "no route to determine the local IPv4 address"),
    }
}

// ----------------------------------------------------------- memory_usage

/// Bytes of RAM and swap: (total, free) each; swap None when there is none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    pub ram: (u64, u64),
    pub swap: Option<(u64, u64)>,
}

#[cfg(target_os = "macos")]
pub fn read_memory() -> Option<Memory> {
    use std::mem::size_of;
    fn sysctl<T: Copy>(name: &str) -> Option<T> {
        let cname = std::ffi::CString::new(name).ok()?;
        let mut v = std::mem::MaybeUninit::<T>::zeroed();
        let mut len = size_of::<T>();
        // SAFETY: `v` has room for `len` bytes; sysctlbyname writes at most that.
        let rc = unsafe { libc::sysctlbyname(cname.as_ptr(), v.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
        (rc == 0 && len == size_of::<T>()).then(|| unsafe { v.assume_init() })
    }
    let total: u64 = sysctl("hw.memsize")?;
    let page: u64 = sysctl::<libc::c_int>("hw.pagesize").map(|p| p as u64).filter(|p| *p > 0)?;
    // SAFETY: host_statistics64 fills at most `count` integers of `stat`.
    let stat = unsafe {
        let mut stat: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        #[allow(deprecated)]
        let host = libc::mach_host_self();
        let rc = libc::host_statistics64(host, libc::HOST_VM_INFO64, (&mut stat as *mut libc::vm_statistics64).cast(), &mut count);
        if rc != 0 {
            return None;
        }
        stat
    };
    // Free memory as Starship (systemstat) counts it: free + inactive pages.
    let free = (u64::from(stat.free_count) + u64::from(stat.inactive_count)) * page;
    let swap = sysctl::<libc::xsw_usage>("vm.swapusage").filter(|s| s.xsu_total > 0).map(|s| (s.xsu_total, s.xsu_avail));
    Some(Memory { ram: (total, free.min(total)), swap })
}

#[cfg(target_os = "linux")]
pub fn read_memory() -> Option<Memory> {
    let src = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib = |key: &str| -> Option<u64> {
        src.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')?.split_whitespace().next()?.parse::<u64>().ok()).map(|k| k * 1024)
    };
    let total = kib("MemTotal")?;
    let free = kib("MemAvailable").or_else(|| kib("MemFree"))?;
    let swap = match (kib("SwapTotal"), kib("SwapFree")) {
        (Some(t), Some(f)) if t > 0 => Some((t, f.min(t))),
        _ => None,
    };
    Some(Memory { ram: (total, free.min(total)), swap })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn read_memory() -> Option<Memory> {
    None
}

/// Starship's byte display: binary units, decimals dropped (`21GiB`, `0B`).
pub fn display_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if b < 1024 {
        return format!("{b}B");
    }
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    let shown = format!("{v:.1}");
    format!("{}{}", shown.split('.').next().unwrap_or("0"), UNITS[i - 1])
}

fn pct(total: u64, free: u64) -> f64 {
    if total == 0 { 0.0 } else { 100.0 * total.saturating_sub(free) as f64 / total as f64 }
}

/// `memory_usage` fields from a reading: `ram`, `ram_pct`, `swap`,
/// `swap_pct` (`pct` = RAM percentage, for the threshold).
pub fn memory_fields(m: &Memory) -> Vec<(&'static str, String)> {
    let (t, f) = m.ram;
    let mut out = vec![
        ("ram", format!("{}/{}", display_bytes(t.saturating_sub(f)), display_bytes(t))),
        ("ram_pct", format!("{:.0}%", pct(t, f))),
        ("pct", format!("{}", pct(t, f).round() as i64)),
    ];
    if let Some((st, sf)) = m.swap {
        out.push(("swap", format!("{}/{}", display_bytes(st.saturating_sub(sf)), display_bytes(st))));
        out.push(("swap_pct", format!("{:.0}%", pct(st, sf))));
    }
    out
}

pub fn memory_slot() -> Slot<ModuleValue> {
    match read_memory() {
        Some(m) => {
            let f = memory_fields(&m);
            let text = f[0].1.clone();
            value(text, &f)
        }
        None => failed(Freshness::Error, "memory statistics are not available on this system"),
    }
}

// ------------------------------------------------------------------- sudo

/// `sudo`: visible while `sudo -n true` succeeds (cached credentials). The
/// worker shares the shell's session and controlling terminal, so sudo's
/// per-terminal tickets apply as they would to a command typed there.
pub fn sudo_slot(req: &PromptRequest, timeout: Duration) -> Slot<ModuleValue> {
    let mut cmd = Command::new("sudo");
    cmd.args(["-n", "true"]);
    if let Some(p) = req.env_map().get("PATH").filter(|p| !p.is_empty()) {
        cmd.env("PATH", p);
    }
    match exec::run(cmd, Instant::now() + timeout, 4096) {
        Ok(o) if o.success => value(String::new(), &[]),
        Ok(_) | Err(exec::RunErr::NotFound) => not_detected(),
        Err(exec::RunErr::Timeout) => failed(Freshness::Timeout, format!("`sudo -n true` timed out after {} ms", timeout.as_millis())),
        Err(exec::RunErr::Spawn(m)) => failed(Freshness::Error, format!("cannot run sudo: {m}")),
    }
}

// ----------------------------------------------------------------- direnv

/// Starship's `detect_env_vars` over the request env: empty list = false
/// here (the caller decides what an empty list means); a set `!NAME` = false;
/// otherwise true when no positive name is listed or one of them is set.
pub fn env_vars_detected(list: &[String], env: &BTreeMap<String, String>) -> Option<bool> {
    if list.is_empty() {
        return None;
    }
    let set = |k: &str| env.get(k).is_some_and(|v| !v.is_empty());
    if list.iter().filter_map(|v| v.strip_prefix('!')).any(set) {
        return Some(false);
    }
    let mut pos = list.iter().filter(|v| !v.starts_with('!')).peekable();
    Some(pos.peek().is_none() || pos.any(|v| set(v)))
}

/// Request env keys passed to `direnv status`: DIRENV_FILE is what it
/// reports as the loaded rc file (DIRENV_DIFF / DIRENV_WATCHES are large and
/// not needed for the status, so the shell need not forward them).
const DIRENV_ENV: &[&str] = &["DIRENV_DIR", "DIRENV_FILE"];

/// `direnv`: shown when the cwd matches `detect` or a `detect_env_vars`
/// entry is set; runs `direnv status --json` with the forwarded PATH,
/// DIRENV_DIR and DIRENV_FILE. Fields: `rc_path`, `allowed` (`allowed`,
/// `not_allowed`, `denied`) and `loaded` (`true`/`false`).
pub fn direnv_slot(req: &PromptRequest, timeout: Duration, detect: &Detect, env_vars: &[String]) -> Slot<ModuleValue> {
    let env = req.env_map();
    let by_env = env_vars_detected(env_vars, &env) == Some(true);
    if !by_env && detect.check(&DirScan::read(&req.cwd), &req.cwd).is_none() {
        return not_detected();
    }
    let mut cmd = Command::new("direnv");
    cmd.args(["status", "--json"]).current_dir(&req.cwd);
    // The worker's own DIRENV_* are from whenever it started: drop them all.
    for k in ["DIRENV_DIR", "DIRENV_FILE", "DIRENV_DIFF", "DIRENV_WATCHES"] {
        cmd.env_remove(k);
    }
    for k in DIRENV_ENV {
        if let Some(v) = env.get(*k) {
            cmd.env(k, v);
        }
    }
    if let Some(p) = env.get("PATH").filter(|p| !p.is_empty()) {
        cmd.env("PATH", p);
    }
    let out = match exec::run(cmd, Instant::now() + timeout, MAX_OUTPUT) {
        Ok(o) if o.success => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => return failed(Freshness::Error, format!("direnv status exited with {:?}", o.code)),
        Err(exec::RunErr::NotFound) => return failed(Freshness::Error, "direnv not found in PATH"),
        Err(exec::RunErr::Timeout) => return failed(Freshness::Timeout, format!("direnv status timed out after {} ms", timeout.as_millis())),
        Err(exec::RunErr::Spawn(m)) => return failed(Freshness::Error, format!("cannot run direnv: {m}")),
    };
    match parse_direnv_status(&out) {
        Some((rc, allowed, loaded)) => value(
            String::new(),
            &[("rc_path", rc.to_string_lossy().into_owned()), ("allowed", allowed.into()), ("loaded", loaded.to_string())],
        ),
        None => not_detected(),
    }
}

/// `direnv status --json` -> (found rc path, allow status, loaded). None when
/// no rc file was found (or the output is not direnv's JSON).
pub fn parse_direnv_status(s: &str) -> Option<(PathBuf, &'static str, bool)> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let state = v.get("state")?;
    let found = state.get("foundRC").filter(|f| !f.is_null())?;
    let allowed = match found.get("allowed")?.as_u64()? {
        0 => "allowed",
        1 => "not_allowed",
        2 => "denied",
        _ => return None,
    };
    let path = PathBuf::from(found.get("path")?.as_str()?);
    let loaded = state.get("loadedRC").and_then(|l| l.get("allowed")).and_then(serde_json::Value::as_u64) == Some(0);
    Some((path, allowed, loaded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(cwd: &Path, env: &str) -> PromptRequest {
        PromptRequest {
            gen: 1,
            session_id: "s".into(),
            cwd: cwd.to_path_buf(),
            cols: 80,
            lines: 24,
            last_exit: 0,
            last_duration_ms: 0,
            env: env.into(),
            jobs: 0,
        }
    }

    #[test]
    fn shortstat_and_describe() {
        assert_eq!(parse_shortstat(" 3 files changed, 10 insertions(+), 2 deletions(-)\n"), (10, 2));
        assert_eq!(parse_shortstat(" 1 file changed, 1 insertion(+)\n"), (1, 0));
        assert_eq!(parse_shortstat(" 1 file changed, 4 deletions(-)\n"), (0, 4));
        assert_eq!(parse_shortstat(""), (0, 0));
        assert_eq!(describe_tag("v1.0-0-gabc1234", 0).as_deref(), Some("v1.0"));
        assert_eq!(describe_tag("release-2-1-3-gabc1234", 5).as_deref(), Some("release-2-1"));
        assert_eq!(describe_tag("v1.0-3-gabc1234", 2), None);
        assert_eq!(describe_tag("garbage", 9), None);
    }

    #[test]
    fn bytes_and_memory_fields() {
        assert_eq!(display_bytes(0), "0B");
        assert_eq!(display_bytes(1023), "1023B");
        assert_eq!(display_bytes(512 * 1024 * 1024), "512MiB");
        assert_eq!(display_bytes(34_359_738_368), "32GiB");
        let gib = 1u64 << 30;
        let m = Memory { ram: (32 * gib, 11 * gib), swap: Some((5 * gib, 2 * gib)) };
        let f: BTreeMap<_, _> = memory_fields(&m).into_iter().collect();
        assert_eq!(f["ram"], "21GiB/32GiB");
        assert_eq!(f["ram_pct"], "66%");
        assert_eq!(f["pct"], "66");
        assert_eq!(f["swap"], "3GiB/5GiB");
        assert_eq!(f["swap_pct"], "60%");
        let m = Memory { ram: (gib, gib), swap: None };
        let f: BTreeMap<_, _> = memory_fields(&m).into_iter().collect();
        assert_eq!((f["ram"].as_str(), f.contains_key("swap")), ("0B/1GiB", false));
        // The host has numbers (macOS / Linux CI).
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            let r = read_memory().expect("memory statistics");
            assert!(r.ram.0 > 0 && r.ram.1 <= r.ram.0);
        }
    }

    #[test]
    fn env_detection_rules() {
        let env: BTreeMap<String, String> = [("A".to_string(), "1".to_string()), ("TMUX".to_string(), "x".to_string())].into();
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(env_vars_detected(&[], &env), None);
        assert_eq!(env_vars_detected(&l(&["A"]), &env), Some(true));
        assert_eq!(env_vars_detected(&l(&["B"]), &env), Some(false));
        assert_eq!(env_vars_detected(&l(&["!TMUX", "A"]), &env), Some(false));
        assert_eq!(env_vars_detected(&l(&["!B"]), &env), Some(true));
    }

    #[test]
    fn direnv_json() {
        let found = r#"{"config":{},"state":{"foundRC":{"allowed":1,"path":"/p/.envrc"},"loadedRC":null}}"#;
        assert_eq!(parse_direnv_status(found), Some((PathBuf::from("/p/.envrc"), "not_allowed", false)));
        let loaded = r#"{"state":{"foundRC":{"allowed":0,"path":"/p/.envrc"},"loadedRC":{"allowed":0,"path":"/p/.envrc"}}}"#;
        assert_eq!(parse_direnv_status(loaded), Some((PathBuf::from("/p/.envrc"), "allowed", true)));
        assert_eq!(parse_direnv_status(r#"{"state":{"foundRC":null,"loadedRC":null}}"#), None);
        assert_eq!(parse_direnv_status("Found RC path /p/.envrc"), None);
    }

    #[test]
    fn env_and_file_based_slots() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("Cargo.toml"), "[package]\nversion = \"1.2.3\"\n").unwrap();
        let s = package_slot(&req(td.path(), ""), false);
        assert_eq!(s.value.unwrap().fields["version"], "1.2.3");
        assert_eq!(package_slot(&req(&td.path().join("missing"), ""), false).value, None);
        // localip: ssh_only without SSH_CONNECTION is not detected (no socket).
        assert_eq!(localip_slot(&req(td.path(), ""), true), not_detected());
        // direnv: nothing detected, nothing runs.
        let d = Detect::from_static(&[".envrc"], &[], &[]);
        assert_eq!(direnv_slot(&req(td.path(), ""), Duration::from_millis(100), &d, &[]), not_detected());
    }

    #[test]
    fn git_slots_in_a_repo() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path();
        let git = |args: &[&str]| {
            let ok = Command::new("git").arg("-C").arg(p).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").output().map(|o| o.status.success());
            ok.unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return; // no git on this machine
        }
        let t = Duration::from_secs(5);
        // Outside a repository: not detected.
        let outside = tempfile::tempdir().unwrap();
        assert_eq!(git_metrics_slot(&req(outside.path(), ""), t, false).value, None);
        // Unborn HEAD: index diff.
        std::fs::write(p.join("a.txt"), "1\n2\n").unwrap();
        assert!(git(&["add", "a.txt"]));
        let s = git_metrics_slot(&req(p, ""), t, false).value.unwrap();
        assert_eq!((s.fields["added"].as_str(), s.fields["deleted"].as_str()), ("0", "0"));
        assert!(git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "x"]));
        assert!(git(&["tag", "v0.1.0"]));
        std::fs::write(p.join("a.txt"), "1\n3\n4\n").unwrap();
        let s = git_metrics_slot(&req(p, ""), t, false).value.unwrap();
        assert_eq!((s.fields["added"].as_str(), s.fields["deleted"].as_str()), ("2", "1"));
        let c = git_commit_slot(&req(p, ""), t, true, Some(0)).value.unwrap();
        assert_eq!(c.fields["hash"].len(), 40);
        assert_eq!(c.fields["tag"], "v0.1.0");
    }
}
