//! Providers: git, session info and config-driven modules. Owners: task D (M1
//! git/session), M1b task B (`from_config`, modules), M1b task D (starship).
use crate::model::{GitData, PromptRequest, ProviderSpec, Slot};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

pub mod agent;
pub mod directory;
pub mod exec;
pub mod git;
pub mod modules;
pub mod native;
pub mod package;
pub mod session_info;
pub mod starship;

use crate::config::Config;
use crate::model::ModuleValue;
use serde_json::Value;

/// A provider computes one slot for a request. Implementations must respect the
/// timeout in their spec and must not panic. `spec().key` identifies the
/// provider (scheduler cache, diagnostics) and must be unique in a provider set.
pub trait Provider: Send + Sync {
    fn spec(&self) -> &ProviderSpec;
    fn fetch(&self, req: &PromptRequest) -> ProviderValue;
    /// External configuration files that can change this provider while idle.
    fn watch_paths(&self, _req: &PromptRequest) -> Vec<std::path::PathBuf> { Vec::new() }
    /// Whether a filesystem event can affect this provider. Modules with
    /// known detection rules override the conservative cwd default.
    fn affected_by_paths(&self, _req: &PromptRequest, _paths: &[std::path::PathBuf]) -> bool {
        true
    }
    /// The `agent_status` provider behind this object, if it is one: the
    /// worker watches its store directories to push updates (M2).
    fn as_agent_status(&self) -> Option<&agent::AgentStatusProvider> {
        None
    }
}

#[derive(Debug, Clone)]
pub enum ProviderValue {
    Git(Slot<GitData>),
    Ssh(bool),
    Hostname(Option<String>),
    Username(Option<String>),
    /// Config-driven module instance: (module id, slot).
    Module(String, Slot<ModuleValue>),
}

use crate::model::Dependency;
use std::time::Duration;

/// Key of the git provider.
pub const GIT_KEY: &str = "git";

static GIT_SPEC: ProviderSpec = ProviderSpec {
    key: GIT_KEY,
    deps: &[Dependency::Cwd, Dependency::Env],
    ttl: Duration::from_secs(5),
    timeout: Duration::from_millis(1500),
};

const SESSION_DEPS: &[Dependency] = &[Dependency::Env];
const SESSION_TTL: Duration = Duration::from_secs(3600);
const SESSION_TIMEOUT: Duration = Duration::from_millis(50);

static SSH_SPEC: ProviderSpec =
    ProviderSpec { key: "session/ssh", deps: SESSION_DEPS, ttl: SESSION_TTL, timeout: SESSION_TIMEOUT };
static HOSTNAME_SPEC: ProviderSpec =
    ProviderSpec { key: "session/hostname", deps: SESSION_DEPS, ttl: SESSION_TTL, timeout: SESSION_TIMEOUT };
static USERNAME_SPEC: ProviderSpec =
    ProviderSpec { key: "session/username", deps: SESSION_DEPS, ttl: SESSION_TTL, timeout: SESSION_TIMEOUT };

pub struct Git;
/// The git provider with `--ignore-submodules=dirty` (a layout git module
/// sets `ignore_submodules`); same key and spec as [`Git`].
pub struct GitIgnoringSubmodules;
pub struct Ssh;
pub struct Hostname;
pub struct Username;

impl Provider for Git {
    fn spec(&self) -> &ProviderSpec {
        &GIT_SPEC
    }
    fn fetch(&self, req: &PromptRequest) -> ProviderValue {
        ProviderValue::Git(git::git_status(&req.cwd, GIT_SPEC.timeout))
    }
}
impl Provider for GitIgnoringSubmodules {
    fn spec(&self) -> &ProviderSpec {
        &GIT_SPEC
    }
    fn fetch(&self, req: &PromptRequest) -> ProviderValue {
        ProviderValue::Git(git::git_status_with(&req.cwd, GIT_SPEC.timeout, true))
    }
}
impl Provider for Ssh {
    fn spec(&self) -> &ProviderSpec {
        &SSH_SPEC
    }
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        ProviderValue::Ssh(session_info::is_ssh())
    }
}
impl Provider for Hostname {
    fn spec(&self) -> &ProviderSpec {
        &HOSTNAME_SPEC
    }
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        ProviderValue::Hostname(session_info::hostname())
    }
}
impl Provider for Username {
    fn spec(&self) -> &ProviderSpec {
        &USERNAME_SPEC
    }
    fn fetch(&self, _req: &PromptRequest) -> ProviderValue {
        ProviderValue::Username(session_info::username())
    }
}

/// The static providers, in scheduling order (session first, then git).
pub fn all() -> Vec<Box<dyn Provider>> {
    vec![Box::new(Ssh), Box::new(Hostname), Box::new(Username), Box::new(Git)]
}

/// Which `PromptData` field a provider fills, derived from its key: `git`, the
/// `session/…` providers, or a config module (whose key is the module id; module
/// ids are `[A-Za-z0-9_-]`, so they never contain `/`). Used for slots that have
/// no value yet; slots with a value are mapped by the value's variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotTarget<'a> {
    Git,
    Session,
    Module(&'a str),
}

pub fn slot_target(key: &str) -> SlotTarget<'_> {
    if key == GIT_KEY {
        SlotTarget::Git
    } else if key == "session" || key.starts_with("session/") {
        SlotTarget::Session
    } else {
        SlotTarget::Module(key)
    }
}

/// `ProviderSpec.key` is `&'static str` (frozen contract) but module ids come
/// from the config: each distinct id is leaked once and reused afterwards, so
/// config reloads do not leak again.
pub fn intern(s: &str) -> &'static str {
    static SET: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut set = SET.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(k) = set.get(s) {
        return k;
    }
    let k: &'static str = Box::leak(s.to_owned().into_boxed_str());
    set.insert(k);
    k
}

/// The static providers from [`all`] plus one provider per config module of a
/// provider type (`language`, `command`, `python`, `docker_context`,
/// `starship`, `agent_status`, and the M2 `package`, `git_commit` (only for tags or hashes
/// longer than 7), `git_metrics`, `localip`, `memory_usage`, `sudo`, `direnv`)
/// placed in the layout (`Config::layout_modules`). Their default timeout is
/// the config's `command_timeout_ms` (else 500 ms). Problems (unknown
/// language, bad regex, duplicate key) skip that module and are printed to
/// stderr.
pub fn from_config(config: &Config) -> Vec<Box<dyn Provider>> {
    let (providers, problems) = from_config_checked(config);
    for p in problems {
        eprintln!("tildy: {p}");
    }
    providers
}

/// [`from_config`] returning the problems instead of printing them.
///
/// Which modules get a provider comes from the typed layout; each module's
/// provider fields (`language`, `module`, `detect_files`, `detect_extensions`,
/// `detect_folders`, `argv`, `regex`, `timeout_ms`, `cache_ttl_s`) are read
/// from its entry in the merged raw JSON (`config.json["modules"][id]`).
/// Provider keys are the module ids and must be unique together with the
/// static keys (`git`, `session/…`). A module not placed in the layout gets no
/// provider (its command would never show).
pub fn from_config_checked(config: &Config) -> (Vec<Box<dyn Provider>>, Vec<String>) {
    let mut out = all();
    let mut problems = Vec::new();
    // Starship's `git_status.ignore_submodules`: one git status serves every
    // git module, so any placed git module asking for it switches it on.
    let ignore_sub = config
        .layout_modules()
        .iter()
        .any(|(_, m)| m.module_type == crate::config::ModuleType::Git && m.ignore_submodules == Some(true));
    if ignore_sub {
        for p in out.iter_mut() {
            if p.spec().key == GIT_KEY {
                *p = Box::new(GitIgnoringSubmodules);
            }
        }
    }
    let mut keys: HashSet<&'static str> = out.iter().map(|p| p.spec().key).collect();
    let raw = config.json.get("modules").and_then(Value::as_object);
    let default_timeout = config.file.command_timeout_ms.map(Duration::from_millis).unwrap_or(modules::DEFAULT_TIMEOUT);
    for (id, _) in config.layout_modules() {
        let Some(m) = raw.and_then(|r| r.get(id)) else { continue };
        match module_provider(id, m, default_timeout) {
            Ok(None) => {}
            Ok(Some(p)) => {
                if keys.insert(p.spec().key) {
                    out.push(p);
                } else {
                    problems.push(format!("modules.{id}: provider key '{id}' is already in use; module has no data"));
                }
            }
            Err(msg) => problems.push(format!("modules.{id}: {msg}; module has no data")),
        }
    }
    (out, problems)
}

fn positive(m: &Value, key: &str) -> Result<Option<u64>, String> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().filter(|n| *n > 0).map(Some).ok_or_else(|| format!("`{key}` must be a positive integer")),
    }
}

fn strings(v: &Value, what: &str) -> Result<Vec<String>, String> {
    let a = v.as_array().ok_or_else(|| format!("`{what}` must be an array of strings"))?;
    a.iter()
        .map(|e| e.as_str().filter(|s| !s.is_empty()).map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| format!("`{what}` must be an array of non-empty strings"))
}

/// Apply `detect_files`, `detect_extensions`, `detect_folders` (Starship's
/// spelling, as in the config schema): each key present replaces that list of
/// the builtin rule (`[]` turns that kind of detection off); absent keys keep
/// the builtin list.
fn apply_detect(m: &Value, d: &mut modules::Detect) -> Result<(), String> {
    for (key, list) in [("detect_files", &mut d.files), ("detect_extensions", &mut d.extensions), ("detect_folders", &mut d.folders)] {
        if let Some(v) = m.get(key) {
            *list = strings(v, key)?;
        }
    }
    for e in &mut d.extensions {
        *e = e.trim_start_matches('.').to_string();
    }
    Ok(())
}

fn flag(m: &Value, key: &str) -> Option<bool> {
    m.get(key).and_then(Value::as_bool)
}

/// TTLs of the E1 providers (the scheduler shows a cached slot as `Fresh`
/// while younger, `Stale` after; `Cwd`/`Time` providers refetch every prompt
/// in the background either way).
const GIT_EXTRA_TTL: Duration = Duration::from_secs(5);
const SYSTEM_TTL: Duration = Duration::from_secs(5);
const LOCALIP_TTL: Duration = Duration::from_secs(30);
const DIRENV_TTL: Duration = Duration::from_secs(60);

fn module_provider(id: &str, m: &Value, default_timeout: Duration) -> Result<Option<Box<dyn Provider>>, String> {
    use native::NativeProvider;
    let ty = m.get("type").and_then(Value::as_str).unwrap_or("");
    if ty == "agent_status" {
        return Ok(Some(Box::new(agent::AgentStatusProvider::new(id))));
    }
    let native_ttl = match ty {
        "package" => Some(modules::DEFAULT_TTL),
        "git_commit" | "git_metrics" => Some(GIT_EXTRA_TTL),
        "memory_usage" | "sudo" => Some(SYSTEM_TTL),
        "localip" => Some(LOCALIP_TTL),
        "direnv" => Some(DIRENV_TTL),
        _ => None,
    };
    if !matches!(ty, "language" | "python" | "command" | "docker_context" | "starship") && native_ttl.is_none() {
        return Ok(None);
    }
    let ttl = positive(m, "cache_ttl_s")?.map(Duration::from_secs).unwrap_or(native_ttl.unwrap_or(modules::DEFAULT_TTL));
    let timeout = positive(m, "timeout_ms")?.map(Duration::from_millis).unwrap_or(default_timeout);
    let p: Box<dyn Provider> = match ty {
        "package" => {
            let private = flag(m, "display_private") == Some(true);
            Box::new(NativeProvider::new(id, native::CWD_DEPS, ttl, timeout, move |req, _| native::package_slot(req, private)))
        }
        "git_commit" => {
            // The short hash comes from the git provider; a provider is only
            // needed for longer hashes or tags.
            let len = m.get("commit_hash_length").and_then(Value::as_u64).unwrap_or(7);
            let tags = (flag(m, "tag_disabled") == Some(false))
                .then(|| m.get("tag_max_candidates").and_then(Value::as_u64).unwrap_or(0).min(u32::MAX as u64) as u32);
            if len <= 7 && tags.is_none() {
                return Ok(None);
            }
            Box::new(NativeProvider::new(id, native::CWD_DEPS, ttl, timeout, move |req, t| native::git_commit_slot(req, t, len > 7, tags)))
        }
        "git_metrics" => {
            let ignore = flag(m, "ignore_submodules") == Some(true);
            Box::new(NativeProvider::new(id, native::CWD_DEPS, ttl, timeout, move |req, t| native::git_metrics_slot(req, t, ignore)))
        }
        "localip" => {
            let ssh_only = flag(m, "ssh_only").unwrap_or(true);
            Box::new(NativeProvider::new(id, native::ENV_TIME_DEPS, ttl, timeout, move |req, _| native::localip_slot(req, ssh_only)))
        }
        "memory_usage" => Box::new(NativeProvider::new(id, native::TIME_DEPS, ttl, timeout, |_, _| native::memory_slot())),
        "sudo" => Box::new(NativeProvider::new(id, native::ENV_TIME_DEPS, ttl, timeout, native::sudo_slot)),
        "direnv" => {
            let mut d = modules::Detect::from_static(&[".envrc"], &[], &[]);
            apply_detect(m, &mut d)?;
            let env_vars = match m.get("detect_env_vars") {
                Some(v) => strings(v, "detect_env_vars")?,
                None => vec!["DIRENV_FILE".to_string()],
            };
            Box::new(NativeProvider::new(id, native::CWD_ENV_DEPS, ttl, timeout, move |req, t| native::direnv_slot(req, t, &d, &env_vars)))
        }
        "language" => {
            let name = m.get("language").and_then(Value::as_str).ok_or("`language` is required")?;
            let def = modules::language(name).ok_or_else(|| {
                let known: Vec<_> = modules::LANGUAGES.iter().map(|l| l.name).collect();
                format!("unknown language '{name}' (builtin: {})", known.join(", "))
            })?;
            let mut p = modules::ModuleProvider::language(id, def, ttl, timeout);
            apply_detect(m, p.detect_mut())?;
            Box::new(p)
        }
        "python" => {
            let mut p = modules::ModuleProvider::python(id, ttl, timeout);
            apply_detect(m, p.detect_mut())?;
            Box::new(p)
        }
        "docker_context" => {
            let mut p = modules::ModuleProvider::docker_context(id, ttl, timeout);
            apply_detect(m, p.detect_mut())?;
            Box::new(p)
        }
        "command" => {
            let argv = strings(m.get("argv").ok_or("`argv` is required")?, "argv")?;
            if argv.is_empty() {
                return Err("`argv` must not be empty".into());
            }
            let regex = match m.get("regex") {
                None | Some(Value::Null) => None,
                Some(v) => {
                    let s = v.as_str().ok_or("`regex` must be a string")?;
                    Some(regex::Regex::new(s).map_err(|e| format!("invalid `regex`: {e}"))?)
                }
            };
            let mut d = modules::Detect::default();
            apply_detect(m, &mut d)?;
            Box::new(modules::ModuleProvider::command(id, argv, d, regex, ttl, timeout))
        }
        "starship" => {
            let module = m.get("module").and_then(Value::as_str).unwrap_or(id);
            // With detect lists an undetected bridge module never spawns
            // `starship`; without them it runs in every directory.
            let mut d = modules::Detect::default();
            apply_detect(m, &mut d)?;
            let p = starship::StarshipProvider::with_binary(id.to_string(), module.to_string(), "starship".into())
                .with_config(m.get("starship_config").and_then(Value::as_str).map(std::path::PathBuf::from))
                .with_detect(d.files, d.extensions, d.folders)
                .with_timeout(timeout);
            Box::new(p)
        }
        _ => unreachable!("filtered above"),
    };
    Ok(Some(p))
}

/// Convenience used by oneshot render: run git status synchronously with timeout.
pub fn git_status(cwd: &Path, timeout: Duration) -> Slot<GitData> {
    git::git_status(cwd, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_and_specs() {
        let a = all();
        let keys: Vec<_> = a.iter().map(|p| p.spec().key).collect();
        assert_eq!(keys, ["session/ssh", "session/hostname", "session/username", "git"]);
        assert_eq!(a[3].spec().timeout, Duration::from_millis(1500));
        assert_eq!(a[0].spec().ttl, Duration::from_secs(3600));
        for k in &keys {
            assert_ne!(slot_target(k), SlotTarget::Module(k));
        }
        assert_eq!(slot_target("git"), SlotTarget::Git);
        assert_eq!(slot_target("session/ssh"), SlotTarget::Session);
        assert_eq!(slot_target("node"), SlotTarget::Module("node"));
        assert_eq!(slot_target("github"), SlotTarget::Module("github"), "only the exact key is git");
    }

    #[test]
    fn agent_status_modules_get_a_provider() {
        let c = crate::config::builtin();
        let (ps, problems) = from_config_checked(&c);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(!ps.iter().any(|p| p.spec().key == "agent"), "builtin no longer places `agent` (opt-in since 2026-10-20)");
        let c = crate::config::from_str(
            r#"{"schema_version":1,"modules":{"a1":{"type":"agent_status"},"a2":{"type":"agent_status","max_width":20}},"layout":{"rows":[{"left":["a1"],"right":["a2"]}]}}"#,
        )
        .unwrap();
        let keys: Vec<_> = from_config(&c).iter().map(|p| p.spec().key).collect();
        assert!(keys.contains(&"a1") && keys.contains(&"a2"));
        assert_eq!(slot_target("a1"), SlotTarget::Module("a1"));
    }

    #[test]
    fn e1_modules_get_providers_with_timeouts() {
        let c = crate::config::from_str(
            r#"{"schema_version":1,"command_timeout_ms":800,"modules":{
                "pkg":{"type":"package"},"gc":{"type":"git_commit"},"gct":{"type":"git_commit","tag_disabled":false},
                "gm":{"type":"git_metrics","timeout_ms":1200},"ip":{"type":"localip"},"mem":{"type":"memory_usage"},
                "su":{"type":"sudo"},"de":{"type":"direnv"},"lvl":{"type":"shlvl"},"nix":{"type":"nix_shell"},"sh":{"type":"shell"},
                "node":{"type":"language","language":"nodejs"},"g":{"type":"git","ignore_submodules":true}},
                "layout":{"rows":[{"left":["pkg","gc","gct","gm","ip","mem","su","de","lvl","nix","sh","node","g"]}]}}"#,
        )
        .unwrap();
        let (ps, problems) = from_config_checked(&c);
        assert!(problems.is_empty(), "{problems:?}");
        let spec = |k: &str| ps.iter().find(|p| p.spec().key == k).map(|p| p.spec().clone());
        for k in ["pkg", "gct", "gm", "ip", "mem", "su", "de", "node"] {
            assert!(spec(k).is_some(), "{k} has a provider");
        }
        // Env-only modules and a default git_commit read existing data.
        for k in ["gc", "lvl", "nix", "sh"] {
            assert!(spec(k).is_none(), "{k} has no provider");
        }
        assert_eq!(spec("node").unwrap().timeout, Duration::from_millis(800), "command_timeout_ms is the default");
        assert_eq!(spec("gm").unwrap().timeout, Duration::from_millis(1200), "timeout_ms wins");
        assert_eq!(spec("mem").unwrap().deps, &[Dependency::Time]);
        assert_eq!(ps.iter().filter(|p| p.spec().key == GIT_KEY).count(), 1);
    }

    #[test]
    fn interned_keys_are_shared() {
        let a = intern("some-module");
        let b = intern(&String::from("some-module"));
        assert!(std::ptr::eq(a, b));
        assert_eq!(a, "some-module");
    }
}
