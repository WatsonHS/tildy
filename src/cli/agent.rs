//! `tildy agent publish|list|clear|wrap`. Owner: M2 task B (everything except
//! `Wrap`, whose body lives in `super::wrap`, task D). See
//! docs/agent-status.md.
//!
//! Scope resolution (publish, and the default view of list/clear):
//! `--scope session` uses `--scope-id` or `$TILDY_SESSION_ID`; `--scope
//! worktree` uses `--scope-id` or the git worktree identity of `--cwd` (default
//! the current directory); `--scope global` is `*`. Without `--scope`,
//! publish uses the worktree when the cwd is in one, else global; list and
//! clear use the current session, the cwd's worktree and global, i.e. what
//! this shell's prompt shows.
//!
//! Exit codes follow the CLI envelope: 2 invalid input (`error.path` names
//! the field), 3 stale `seq`, 4 no state directory / not in a worktree / no
//! session id / store not writable.
use clap::{Args, Subcommand};
use serde::Serialize;
use tildy::agent::{self, Filter, Kind, Record, Reject, Scope, ScopeKind, Source, State, Store, Stored};
use tildy::model::{CliEnvelope, ErrorCode};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub cmd: AgentCmd,
}

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Store an agent status record (OSC 7501 vocabulary) for prompts to show.
    Publish(Box<PublishArgs>),
    /// List live agent status records (this shell's session, worktree, global).
    List(ListArgs),
    /// Remove agent status records.
    Clear(ClearArgs),
    /// Run a command in a pty and collect its OSC 7501 program-status reports.
    Wrap(super::wrap::WrapArgs),
}

#[derive(Args, Default)]
pub struct ScopeArgs {
    /// Where the record shows: session (this shell), worktree (this git
    /// worktree, every pane in it) or global.
    #[arg(long, value_name = "session|worktree|global")]
    pub scope: Option<String>,
    /// Scope id instead of $TILDY_SESSION_ID / the cwd's worktree identity.
    #[arg(long, value_name = "ID")]
    pub scope_id: Option<String>,
    /// Directory whose git worktree is the worktree scope (default: cwd).
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct PublishArgs {
    /// idle, working, done, blocked, error, or clear (remove the record with
    /// --id and its children; without --id every record of --app, or all, in
    /// the scope).
    #[arg(long, conflicts_with_all = ["file", "osc"])]
    pub state: Option<String>,
    /// Record id: segments of [A-Za-z0-9_.+-]{1,32} joined by '/' (default:
    /// the root record).
    #[arg(long)]
    pub id: Option<String>,
    /// Stable program name, [A-Za-z0-9_.+-]{1,32}.
    #[arg(long)]
    pub app: Option<String>,
    /// Why a blocked program waits: permission, question or auth.
    #[arg(long, conflicts_with_all = ["file", "osc"])]
    pub kind: Option<String>,
    /// 0-100 (working/blocked).
    #[arg(long, conflicts_with_all = ["file", "osc"])]
    pub progress: Option<u64>,
    /// Short label (at most 192 bytes, one line).
    #[arg(long, conflicts_with_all = ["file", "osc"])]
    pub title: Option<String>,
    /// What the program is doing or needs (at most 2048 bytes, one line).
    #[arg(long, conflicts_with_all = ["file", "osc"])]
    pub msg: Option<String>,
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// Sequence number; must exceed the live record's (default: previous + 1).
    #[arg(long)]
    pub seq: Option<u64>,
    /// Lifetime in seconds, 1-86400 (default: 7200 idle/working/blocked, 900 done/error).
    #[arg(long)]
    pub ttl: Option<u64>,
    /// Origin shell session instead of $TILDY_SESSION_ID (its next prompt
    /// drops idle/working/blocked records, its second one done/error).
    #[arg(long, value_name = "SESSION", conflicts_with = "no_origin")]
    pub origin: Option<String>,
    /// Publish without an origin session (only the TTL or a clear removes it).
    #[arg(long)]
    pub no_origin: bool,
    /// Read a record (schema/agent-state.schema.json) from a file ('-' = stdin).
    #[arg(long, conflicts_with = "osc")]
    pub file: Option<PathBuf>,
    /// A raw OSC 7501 report body, e.g. 'state=blocked:kind=permission:msg=<base64>'.
    #[arg(long, value_name = "BODY")]
    pub osc: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// Every scope in the store, not only this shell's.
    #[arg(long, conflicts_with = "scope")]
    pub all: bool,
    /// Include records whose TTL ran out (not yet removed).
    #[arg(long)]
    pub expired: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ClearArgs {
    /// Remove this record and its children.
    #[arg(long)]
    pub id: Option<String>,
    /// Remove only records of this app.
    #[arg(long)]
    pub app: Option<String>,
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// Every scope in the store, not only this shell's.
    #[arg(long, conflicts_with = "scope")]
    pub all: bool,
    /// Only records whose TTL ran out; alone, purges them from every scope.
    #[arg(long)]
    pub expired: bool,
    #[arg(long)]
    pub json: bool,
}

pub fn run(a: AgentArgs) -> i32 {
    match a.cmd {
        AgentCmd::Publish(p) => {
            let json = p.json;
            finish(json, "publish", publish(*p))
        }
        AgentCmd::List(l) => {
            let json = l.json;
            finish(json, "list", list(l))
        }
        AgentCmd::Clear(c) => {
            let json = c.json;
            finish(json, "clear", clear(c))
        }
        AgentCmd::Wrap(w) => super::wrap::run(w),
    }
}

// ------------------------------------------------------------------ output

struct Fail {
    code: ErrorCode,
    path: Option<String>,
    message: String,
    hint: Option<String>,
}

impl Fail {
    fn new(code: ErrorCode, message: impl Into<String>) -> Fail {
        Fail { code, path: None, message: message.into(), hint: None }
    }
    fn invalid(path: &str, message: impl Into<String>) -> Fail {
        Fail { path: Some(path.into()), ..Fail::new(ErrorCode::ValidationFailed, message) }
    }
    fn hint(mut self, h: &str) -> Fail {
        self.hint = Some(h.into());
        self
    }
}

impl From<Reject> for Fail {
    fn from(r: Reject) -> Fail {
        let code = match r {
            Reject::Invalid { .. } => ErrorCode::ValidationFailed,
            Reject::StaleSeq { .. } => ErrorCode::RevisionConflict,
            Reject::Io(_) => ErrorCode::EnvironmentUnavailable,
        };
        let hint = match &r {
            Reject::StaleSeq { current, .. } => Some(format!("publish with --seq {} or more, or omit --seq", current + 1)),
            _ => None,
        };
        // `path` names the field; the message does not repeat it.
        let message = match &r {
            Reject::Invalid { message, .. } => message.clone(),
            _ => r.to_string(),
        };
        Fail { code, path: r.field().map(str::to_string), message, hint }
    }
}

/// Output of a successful command: text for humans, data for `--json`.
struct Done {
    text: String,
    data: serde_json::Value,
}

fn finish(json: bool, cmd: &str, r: Result<Done, Fail>) -> i32 {
    match r {
        Ok(d) => {
            if json {
                print_json(&CliEnvelope::ok(d.data));
            } else {
                print!("{}", d.text);
            }
            0
        }
        Err(f) => {
            if json {
                let mut env = CliEnvelope::<()>::err(f.code, f.message, f.hint);
                env.error.path = f.path;
                print_json(&env);
            } else {
                let at = f.path.map(|p| format!("{p}: ")).unwrap_or_default();
                eprintln!("tildy agent {cmd}: {at}{}", f.message);
                if let Some(h) = f.hint {
                    eprintln!("hint: {h}");
                }
            }
            f.code.exit_code()
        }
    }
}

fn print_json<T: Serialize>(v: &T) {
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("tildy agent: cannot encode JSON: {e}"),
    }
}

/// A record as shown by list/clear/publish.
#[derive(Serialize)]
struct View<'a> {
    #[serde(flatten)]
    stored: &'a Stored,
    age_s: u64,
    expired: bool,
}

fn view(s: &Stored, now: u64) -> View<'_> {
    View { stored: s, age_s: now.saturating_sub(s.record.received_at_ms.unwrap_or(now)) / 1000, expired: s.is_expired(now) }
}

fn human_secs(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{}s", s / 60, s % 60),
        _ => format!("{}h{}m", s / 3600, (s / 60) % 60),
    }
}

fn line(s: &Stored, now: u64) -> String {
    let r = &s.record;
    let state = match r.kind {
        Some(k) => format!("{}/{}", r.state.as_str(), k.as_str()),
        None => r.state.as_str().to_string(),
    };
    let mut out = format!("{state:<18} {}", s.app().unwrap_or("-"));
    out.push_str(&format!(" {}", if r.id.is_empty() { "(root)" } else { &r.id }));
    if let Some(p) = r.progress {
        out.push_str(&format!(" {p}%"));
    }
    if let Some(t) = &r.title {
        out.push_str(&format!(" [{t}]"));
    }
    if let Some(m) = &r.msg {
        out.push_str(&format!(" {m}"));
    }
    let age = now.saturating_sub(r.received_at_ms.unwrap_or(now)) / 1000;
    let left = if s.is_expired(now) { "expired".to_string() } else { format!("expires in {}", human_secs((s.expires_at_ms - now).div_ceil(1000))) };
    out.push_str(&format!("  ({}, {} ago, {left}, seq {})", r.scope.kind.as_str(), human_secs(age), r.seq));
    out.push('\n');
    out
}

// ------------------------------------------------------------------ scopes

fn store() -> Result<Store, Fail> {
    Store::open().map_err(|e| Fail::new(ErrorCode::EnvironmentUnavailable, e.to_string()).hint("set HOME or XDG_STATE_HOME"))
}

fn session_env() -> Option<String> {
    std::env::var("TILDY_SESSION_ID").ok().filter(|s| !s.is_empty())
}

fn cwd_of(a: &ScopeArgs) -> Result<PathBuf, Fail> {
    match &a.cwd {
        Some(d) if d.is_dir() => Ok(d.clone()),
        Some(d) => Err(Fail::invalid("cwd", format!("{} is not a directory", d.display()))),
        None => std::env::current_dir().map_err(|e| Fail::new(ErrorCode::EnvironmentUnavailable, format!("cannot read the current directory: {e}"))),
    }
}

fn worktree_of(dir: &Path) -> Option<String> {
    tildy::providers::git::worktree_identity(dir)
}

/// The one scope named by `--scope`/`--scope-id`, or the publish default
/// (the cwd's worktree, else global) when `--scope` is absent.
fn resolve_scope(a: &ScopeArgs) -> Result<Scope, Fail> {
    let kind = match a.scope.as_deref() {
        None => {
            if a.scope_id.is_some() {
                return Err(Fail::invalid("scope", "--scope-id needs --scope"));
            }
            let cwd = cwd_of(a)?;
            return Ok(worktree_of(&cwd).map(Scope::worktree).unwrap_or_else(Scope::global));
        }
        Some(s) => ScopeKind::parse(s).ok_or_else(|| Fail::invalid("scope", format!("unknown scope '{s}' (session, worktree, global)")))?,
    };
    match kind {
        ScopeKind::Global => match a.scope_id.as_deref() {
            None | Some("*") => Ok(Scope::global()),
            Some(_) => Err(Fail::invalid("scope_id", "the global scope id is \"*\"")),
        },
        ScopeKind::Session => match a.scope_id.clone().or_else(session_env) {
            Some(id) => Ok(Scope::session(id)),
            None => Err(Fail::new(ErrorCode::EnvironmentUnavailable, "no session id: $TILDY_SESSION_ID is not set")
                .hint("run inside a shell with tildy loaded, or pass --scope-id")),
        },
        ScopeKind::Worktree => match a.scope_id.clone() {
            Some(id) => Ok(Scope::worktree(id)),
            None => {
                let cwd = cwd_of(a)?;
                worktree_of(&cwd).map(Scope::worktree).ok_or_else(|| {
                    Fail::new(ErrorCode::EnvironmentUnavailable, format!("{} is not inside a git worktree", cwd.display()))
                        .hint("pass --cwd DIR or --scope-id ID")
                })
            }
        },
    }
}

/// What this shell's prompt shows: session, the cwd's worktree, global; or
/// the single `--scope`.
fn view_scopes(a: &ScopeArgs) -> Result<Vec<Scope>, Fail> {
    if a.scope.is_some() {
        return Ok(vec![resolve_scope(a)?]);
    }
    if a.scope_id.is_some() {
        return Err(Fail::invalid("scope", "--scope-id needs --scope"));
    }
    let cwd = cwd_of(a)?;
    Ok(tildy::providers::agent::prompt_scopes(&session_env().unwrap_or_default(), worktree_of(&cwd)))
}

// ----------------------------------------------------------------- publish

fn opt_check(field: &str, v: Option<&str>, check: fn(&str) -> Result<(), String>) -> Result<(), Fail> {
    match v {
        Some(s) => check(s).map_err(|m| Fail::invalid(field, m)),
        None => Ok(()),
    }
}

fn read_file(p: &Path) -> Result<Record, Fail> {
    let text = if p == Path::new("-") {
        let mut s = String::new();
        std::io::stdin().take(1 << 20).read_to_string(&mut s).map_err(|e| Fail::invalid("file", format!("cannot read stdin: {e}")))?;
        s
    } else {
        std::fs::read_to_string(p).map_err(|e| Fail::invalid("file", format!("cannot read {}: {e}", p.display())))?
    };
    serde_json::from_str(&text).map_err(|e| Fail::invalid("file", format!("not a valid record: {e}")))
}

/// What a publish does: store a record, or (`state=clear`) remove the record
/// `id` and its children, or without an id every record of `app` (or all) in
/// the scope.
enum Action {
    Store(Record),
    Clear { scope: Scope, id: Option<String>, app: Option<String> },
}

fn publish(a: PublishArgs) -> Result<Done, Fail> {
    opt_check("id", a.id.as_deref(), agent::check_id)?;
    opt_check("app", a.app.as_deref(), agent::check_app)?;
    let origin = if a.no_origin { None } else { a.origin.clone().or_else(session_env) };
    let action = if let Some(f) = &a.file {
        let mut r = read_file(f)?;
        if r.origin_session.is_none() && !a.no_origin {
            r.origin_session = origin;
        }
        Action::Store(r)
    } else if let Some(body) = &a.osc {
        let rep = agent::parse_osc_body(body).map_err(|r| {
            let f = Fail::from(r);
            Fail { path: f.path.map(|p| format!("osc.{p}")), ..f }
        })?;
        let scope = resolve_scope(&a.scope)?;
        let id = if rep.id.is_empty() { a.id.clone().unwrap_or_default() } else { rep.id.clone() };
        let app = rep.app.clone().or(a.app.clone());
        match rep.op {
            agent::Op::Clear => Action::Clear { scope, id: Some(id).filter(|i| !i.is_empty()), app },
            agent::Op::Set(_) => {
                let mut r = rep.into_record(scope).expect("set report");
                r.id = id;
                r.app = app;
                r.source = Source::Osc;
                r.origin_session = origin;
                Action::Store(r)
            }
        }
    } else {
        let Some(state) = a.state.as_deref() else {
            return Err(Fail::invalid("state", "one of --state, --file or --osc is required"));
        };
        let scope = resolve_scope(&a.scope)?;
        if state == "clear" {
            Action::Clear { scope, id: a.id.clone(), app: a.app.clone() }
        } else {
            let st = State::parse(state).ok_or_else(|| Fail::invalid("state", format!("unknown state '{state}' (idle, working, done, blocked, error, clear)")))?;
            let mut r = Record::new(st, scope);
            r.id = a.id.clone().unwrap_or_default();
            r.app = a.app.clone();
            if let Some(k) = a.kind.as_deref() {
                r.kind = Some(Kind::parse(k).ok_or_else(|| Fail::invalid("kind", format!("unknown kind '{k}' (permission, question, auth)")))?);
            }
            if let Some(p) = a.progress {
                r.progress = Some(u8::try_from(p).ok().filter(|p| *p <= 100).ok_or_else(|| Fail::invalid("progress", "must be 0-100"))?);
            }
            r.title = a.title.clone();
            r.msg = a.msg.clone();
            r.origin_session = origin;
            Action::Store(r)
        }
    };
    let st = store()?;
    let now = agent::now_ms();
    let mut r = match action {
        Action::Store(r) => r,
        Action::Clear { scope, id, app } => {
            let filter = Filter { scopes: vec![scope.clone()], app: if id.is_none() { app } else { None }, id, ..Filter::default() };
            let removed = st.clear(&filter)?;
            let views: Vec<_> = removed.iter().map(|s| view(s, now)).collect();
            let data = serde_json::json!({"action": "cleared", "scope": scope, "removed": views, "count": removed.len()});
            return Ok(Done { text: format!("cleared {} record(s) in {}\n", removed.len(), scope.kind.as_str()), data });
        }
    };
    if let Some(s) = a.seq {
        if s == 0 {
            return Err(Fail::invalid("seq", "must be at least 1"));
        }
        r.seq = s;
    }
    if let Some(t) = a.ttl {
        r.ttl_s = Some(u32::try_from(t).ok().filter(|t| (1..=agent::record::MAX_TTL_S).contains(t)).ok_or_else(|| Fail::invalid("ttl_s", "must be 1-86400"))?);
    }
    let stored = st.publish(r)?;
    let data = serde_json::json!({"action": "published", "record": view(&stored, now)});
    Ok(Done { text: format!("published {}", line(&stored, now)), data })
}

// ------------------------------------------------------------- list, clear

fn list(a: ListArgs) -> Result<Done, Fail> {
    let st = store()?;
    let scopes = if a.all { vec![] } else { view_scopes(&a.scope)? };
    let now = agent::now_ms();
    let filter = Filter { scopes: scopes.clone(), expired: if a.expired { None } else { Some(false) }, now_ms: Some(now), ..Filter::default() };
    let recs = st.list(&filter);
    let text = if recs.is_empty() { "no agent status records\n".to_string() } else { recs.iter().map(|s| line(s, now)).collect() };
    let views: Vec<_> = recs.iter().map(|s| view(s, now)).collect();
    let data = serde_json::json!({"scopes": scopes, "all": a.all, "records": views, "count": recs.len(), "now_ms": now});
    Ok(Done { text, data })
}

fn clear(a: ClearArgs) -> Result<Done, Fail> {
    opt_check("id", a.id.as_deref(), agent::check_id)?;
    opt_check("app", a.app.as_deref(), agent::check_app)?;
    let st = store()?;
    let now = agent::now_ms();
    let only_expired = a.expired && a.id.is_none() && a.app.is_none() && a.scope.scope.is_none() && a.scope.scope_id.is_none();
    if only_expired {
        // Housekeeping: expired records (and garbage) of every scope.
        let n = st.expire(now);
        let data = serde_json::json!({"action": "expired", "count": n});
        return Ok(Done { text: format!("removed {n} expired record(s)\n"), data });
    }
    let scopes = if a.all { vec![] } else { view_scopes(&a.scope)? };
    let filter = Filter { scopes: scopes.clone(), id: a.id, app: a.app, expired: a.expired.then_some(true), now_ms: Some(now) };
    let removed = st.clear(&filter)?;
    let views: Vec<_> = removed.iter().map(|s| view(s, now)).collect();
    let data = serde_json::json!({"action": "cleared", "scopes": scopes, "all": a.all, "removed": views, "count": removed.len()});
    Ok(Done { text: format!("cleared {} record(s)\n", removed.len()), data })
}
