//! `tildy config get|validate|apply|rollback|history`. Owner: M2 task A.
//!
//! Thin front end over `tildy::config::transaction`: argument checks, the
//! `CliEnvelope` for `--json` (stdout holds only the envelope, nothing goes to
//! stderr) and a human-readable text mode. Reference: docs/configuration.md.
use clap::{Args, Subcommand};
use serde::Serialize;
use serde_json::Value;
use tildy::config::transaction::{self as tx, Current, Entry, Problem, Sources, Store, TxError, Warning};
use tildy::config::{self, ConfigError};
use tildy::model::CliEnvelope;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct ConfigArgs {
    /// Print only the JSON result envelope on stdout.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub cmd: ConfigCmd,
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Print the user config with its revision (or, with --effective, the merged config with sources).
    Get {
        /// The merged config (built-in -> preset -> user -> project) and its sources.
        #[arg(long)]
        effective: bool,
        /// Repository context (default: current directory); only used with --effective.
        #[arg(long, requires = "effective")]
        cwd: Option<PathBuf>,
    },
    /// Validate a candidate config file (default: the current user config); `-` reads stdin.
    Validate {
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
        /// Validate display-only .tildy.json overrides against the current user config.
        #[arg(long, requires = "file")]
        project: bool,
    },
    /// Validate FILE and install it as the user config if the current revision is REV.
    Apply {
        /// Candidate config (`-` reads stdin). Required.
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
        /// Revision the edit was based on (`tildy config get`); `builtin` when there is no file.
        #[arg(long, value_name = "REV")]
        expect_revision: Option<String>,
        /// Skip the revision check (the current file is still saved to the history).
        #[arg(long)]
        force: bool,
    },
    /// Restore revision REV from the history (`builtin` removes the user file).
    Rollback {
        /// Revision to restore (from `tildy config history`) or `builtin`. Required.
        #[arg(long, value_name = "REV")]
        to: Option<String>,
        /// Current revision as last read (`tildy config get`). Required.
        #[arg(long, value_name = "REV")]
        expect_revision: Option<String>,
    },
    /// List saved copies of previous configs, newest first.
    History,
}

pub fn run(a: ConfigArgs) -> i32 {
    let json = a.json;
    let result = match a.cmd {
        ConfigCmd::Get { effective, cwd } => get(json, effective, cwd),
        ConfigCmd::Validate { file, project } => validate(json, file, project),
        ConfigCmd::Apply { file, expect_revision, force } => apply(json, file, expect_revision, force),
        ConfigCmd::Rollback { to, expect_revision } => rollback(json, to, expect_revision),
        ConfigCmd::History => history(json),
    };
    match result {
        Ok(()) => 0,
        Err(e) => fail(json, &e),
    }
}

fn fail(json: bool, e: &TxError) -> i32 {
    if json {
        let mut env = CliEnvelope::<()>::err(e.code, e.message.clone(), e.hint.clone());
        env.error.path = e.path.clone();
        print_json(&env);
    } else {
        eprintln!("tildy config: {e}");
        if let Some(h) = &e.hint {
            eprintln!("hint: {h}");
        }
    }
    e.code.exit_code()
}

fn print_json<T: Serialize>(v: &T) {
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        // Not reachable for these types; keep stdout a valid envelope anyway.
        Err(e) => println!(r#"{{"api_version":1,"ok":false,"data":null,"error":{{"code":"internal_error","path":null,"message":"cannot encode JSON: {}","hint":null}}}}"#, e.to_string().replace('"', "'")),
    }
}

fn required<T>(v: Option<T>, flag: &str, hint: &str) -> Result<T, TxError> {
    v.ok_or_else(|| TxError::validation(format!("{flag} is required"), Some(hint)))
}

/// Read a candidate file (`-` = stdin). A file that cannot be read is the
/// caller's input problem: `validation_failed`.
fn read_input(file: &Path) -> Result<Vec<u8>, TxError> {
    if file.as_os_str() == "-" {
        let mut b = vec![];
        std::io::stdin().read_to_end(&mut b).map_err(|e| TxError::validation(format!("cannot read stdin: {e}"), None))?;
        return Ok(b);
    }
    std::fs::read(file).map_err(|e| TxError::validation(format!("cannot read {}: {e}", file.display()), Some("--file takes a path to the candidate config, or `-` for stdin")))
}

fn warn_text(warnings: &[Warning]) {
    for w in warnings {
        match &w.path {
            Some(p) => eprintln!("warning: {p}: {}", w.message),
            None => eprintln!("warning: {}", w.message),
        }
    }
}

// ---- get ---------------------------------------------------------------

#[derive(Serialize)]
struct UserView {
    path: PathBuf,
    exists: bool,
    revision: String,
    bytes: Option<usize>,
    /// The file loads (false: the prompt falls back to the built-in config).
    valid: bool,
    problem: Option<Problem>,
    /// The file parsed as JSON; null when missing or not JSON.
    config: Option<Value>,
    /// The file's exact text (lossy UTF-8); null when missing.
    text: Option<String>,
}

#[derive(Serialize)]
struct EffectiveView {
    effective_revision: String,
    project: Option<config::project::Layer>,
    path: PathBuf,
    exists: bool,
    revision: String,
    valid: bool,
    problem: Option<Problem>,
    /// The user file is invalid and the built-in config is in effect.
    fallback: bool,
    preset: String,
    config: Value,
    sources: Sources,
}

fn load_current(cur: &Current) -> Option<Result<config::Config, ConfigError>> {
    let b = cur.bytes.as_ref()?;
    Some(match std::str::from_utf8(b) {
        Ok(t) => config::from_str(t),
        Err(e) => Err(ConfigError { kind: config::ErrorKind::Syntax, path: String::new(), message: format!("not valid UTF-8: {e}") }),
    })
}

fn get(json: bool, effective: bool, cwd: Option<PathBuf>) -> Result<(), TxError> {
    let store = Store::from_env()?;
    let cur = store.current()?;
    let loaded = load_current(&cur);
    let problem = match &loaded {
        Some(Err(e)) => Some(Problem::from(e)),
        _ => None,
    };
    if effective {
        let (cfg, fallback) = match loaded {
            Some(Ok(c)) => (c, false),
            Some(Err(_)) => (config::builtin(), true),
            None => (config::builtin(), false),
        };
        let cwd = cwd.or_else(|| std::env::current_dir().ok()).ok_or_else(|| TxError::validation("cannot resolve current directory", None))?;
        if !cwd.is_dir() { return Err(TxError::validation("--cwd must be an existing directory", None)); }
        let cfg = config::project::for_directory(cfg, &cwd);
        let view = EffectiveView {
            effective_revision: cfg.revision.clone(),
            project: cfg.project.clone(),
            path: cur.path.clone(),
            exists: cur.exists(),
            revision: cur.revision.clone(),
            valid: problem.is_none(),
            problem,
            fallback,
            preset: cfg.file.preset.clone().unwrap_or_else(|| "plain".into()),
            sources: tx::sources(&cfg),
            config: cfg.json,
        };
        if json {
            print_json(&CliEnvelope::ok(view));
        } else {
            println!("# {} ({})", view.path.display(), describe(&view.revision, view.exists, view.problem.as_ref()));
            if view.fallback {
                println!("# the file is invalid: the built-in config below is in effect");
            }
            if let Some(project) = &view.project {
                println!("# project {}: {}", project.path.display(), if !project.exists { "missing" } else if project.valid { "applied" } else { "ignored (invalid)" });
                if let Some(problem) = &project.problem { println!("# {}: {}", problem.path.as_deref().unwrap_or(""), problem.message); }
            }
            println!("# effective config, preset {}", view.preset);
            println!("{}", serde_json::to_string_pretty(&view.config).unwrap_or_default());
            println!("# sources");
            let w = view.sources.keys.keys().chain(view.sources.modules.keys()).map(|k| k.len()).max().unwrap_or(0) + 8;
            for (k, v) in &view.sources.keys {
                println!("{:<w$} {v}", k);
            }
            for (k, v) in &view.sources.modules {
                println!("{:<w$} {v}", format!("modules.{k}"));
            }
        }
        return Ok(());
    }
    let view = UserView {
        path: cur.path.clone(),
        exists: cur.exists(),
        revision: cur.revision.clone(),
        bytes: cur.bytes.as_ref().map(Vec::len),
        valid: problem.is_none(),
        config: cur.bytes.as_ref().and_then(|b| serde_json::from_slice(b).ok()),
        text: cur.bytes.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()),
        problem,
    };
    if json {
        print_json(&CliEnvelope::ok(view));
    } else {
        println!("# {} ({})", view.path.display(), describe(&view.revision, view.exists, view.problem.as_ref()));
        match &view.text {
            Some(t) => print!("{t}{}", if t.ends_with('\n') || t.is_empty() { "" } else { "\n" }),
            None => println!("# no user config: the built-in config is in effect (`tildy config get --effective` shows it)"),
        }
    }
    Ok(())
}

fn describe(revision: &str, exists: bool, problem: Option<&Problem>) -> String {
    let state = match (exists, problem) {
        (false, _) => "no file".to_string(),
        (true, None) => "valid".to_string(),
        (true, Some(p)) => format!("INVALID: {}{}", p.path.as_deref().map(|p| format!("{p}: ")).unwrap_or_default(), p.message),
    };
    format!("revision {revision}, {state}")
}

// ---- validate ----------------------------------------------------------

#[derive(Serialize)]
struct ValidateView {
    /// The validated file (`-` for stdin).
    file: PathBuf,
    exists: bool,
    /// Revision the file would have once applied.
    revision: String,
    preset: String,
    warnings: Vec<Warning>,
}

fn validate(json: bool, file: Option<PathBuf>, project: bool) -> Result<(), TxError> {
    let (file, bytes) = match file {
        Some(f) => {
            let b = read_input(&f)?;
            (f, Some(b))
        }
        None => {
            let cur = Store::from_env()?.current()?;
            (cur.path, cur.bytes)
        }
    };
    let view = match &bytes {
        Some(b) => {
            let c = if project {
                let cur = Store::from_env()?.current()?;
                let base = load_current(&cur).transpose()?.unwrap_or_else(config::builtin);
                let text = std::str::from_utf8(b).map_err(|e| TxError::validation(format!("not UTF-8: {e}"), None))?;
                let config = config::project::parse(&base, text)?;
                tx::Checked { revision: config::content_revision(b), config, warnings: vec![] }
            } else { tx::check_bytes(b)? };
            ValidateView {
                file,
                exists: true,
                revision: c.revision,
                preset: c.config.file.preset.clone().unwrap_or_else(|| "plain".into()),
                warnings: c.warnings,
            }
        }
        None => ValidateView { file, exists: false, revision: tx::BUILTIN.into(), preset: "plain".into(), warnings: vec![] },
    };
    if json {
        print_json(&CliEnvelope::ok(view));
    } else {
        warn_text(&view.warnings);
        let what = if view.exists { "valid" } else { "missing (the built-in config is in effect)" };
        println!("{}: {what}, revision {}, preset {}{}", view.file.display(), view.revision, view.preset, plural_warnings(view.warnings.len()));
    }
    Ok(())
}

fn plural_warnings(n: usize) -> String {
    match n {
        0 => String::new(),
        1 => ", 1 warning".into(),
        n => format!(", {n} warnings"),
    }
}

// ---- apply / rollback --------------------------------------------------

fn apply(json: bool, file: Option<PathBuf>, expect: Option<String>, force: bool) -> Result<(), TxError> {
    let file = required(file, "--file", "pass the candidate config: --file FILE (or - for stdin)")?;
    let bytes = read_input(&file)?;
    let out = Store::from_env()?.apply(&bytes, expect.as_deref(), force)?;
    if json {
        print_json(&CliEnvelope::ok(out));
    } else {
        warn_text(&out.warnings);
        if !out.changed {
            println!("{}: unchanged (revision {})", out.path.display(), out.revision);
        } else {
            println!("{}: applied revision {} (was {}{})", out.path.display(), out.revision, out.previous_revision, if out.forced { ", forced" } else { "" });
            if let Some(id) = &out.history_id {
                println!("previous file saved as history entry {id}; undo: tildy config rollback --to {} --expect-revision {}", out.previous_revision, out.revision);
            } else {
                println!("undo: tildy config rollback --to builtin --expect-revision {}", out.revision);
            }
        }
    }
    Ok(())
}

fn rollback(json: bool, to: Option<String>, expect: Option<String>) -> Result<(), TxError> {
    let to = required(to, "--to", "pass a revision from `tildy config history`, or builtin")?;
    let expect = required(expect, "--expect-revision", "pass the current revision from `tildy config get --json`")?;
    let out = Store::from_env()?.rollback(&to, &expect)?;
    if json {
        print_json(&CliEnvelope::ok(out));
    } else if !out.changed {
        println!("{}: already at revision {}", out.path.display(), out.revision);
    } else {
        let what = match &out.restored_from {
            Some(id) => format!("restored revision {} from history entry {id}", out.revision),
            None => "removed the user file (built-in config in effect)".into(),
        };
        println!("{}: {what} (was {})", out.path.display(), out.previous_revision);
        if let Some(id) = &out.history_id {
            println!("previous file saved as history entry {id}");
        }
    }
    Ok(())
}

// ---- history -----------------------------------------------------------

#[derive(Serialize)]
struct HistoryView {
    dir: PathBuf,
    /// The configured user config path and its current revision.
    path: PathBuf,
    current_revision: String,
    keep: usize,
    entries: Vec<Entry>,
}

fn history(json: bool) -> Result<(), TxError> {
    let store = Store::from_env()?;
    let entries = store.history_entries()?;
    let cur = store.current()?;
    let view = HistoryView {
        dir: store.history_dir.clone().unwrap_or_default(),
        path: cur.path,
        current_revision: cur.revision,
        keep: tx::HISTORY_KEEP,
        entries,
    };
    if json {
        print_json(&CliEnvelope::ok(view));
        return Ok(());
    }
    println!("# {} ({} of at most {} entries); current {} revision {}", view.dir.display(), view.entries.len(), view.keep, view.path.display(), view.current_revision);
    if view.entries.is_empty() {
        println!("# no history yet: `tildy config apply` and `tildy config rollback` save the file they replace");
    }
    for e in &view.entries {
        let path = e.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "?".into());
        let by = e.replaced_by.as_deref().unwrap_or("?");
        println!("{}  {}  {:>7} B  {:<8}  -> {by}  {path}", local_time(e.at_ms), e.revision, e.bytes, e.op);
    }
    Ok(())
}

/// `YYYY-MM-DD HH:MM:SS` in local time (honours `TZ`).
fn local_time(ms: u64) -> String {
    let t = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r writes only into the zeroed `tm` we own and returns
    // NULL on failure.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return format!("@{}", ms / 1000);
        }
        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}
