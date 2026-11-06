//! `tildy import starship`. Owner: M1b task D.
use clap::Args;
use serde::Serialize;
use serde_json::Value;
use tildy::import::{import_starship, Resolution, Unsupported};
use tildy::config::transaction::Store;
use tildy::model::{CliEnvelope, CliError, ErrorCode, API_VERSION};
use std::path::PathBuf;

#[derive(Args)]
pub struct ImportArgs {
    /// Source format (only "starship" in M1b).
    pub source: String,
    /// Source file (default: $STARSHIP_CONFIG or ~/.config/starship.toml).
    #[arg(long)]
    pub file: Option<std::path::PathBuf>,
    /// Write the candidate to the tildy config path if no config exists yet.
    #[arg(long)]
    pub write: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct ImportData {
    candidate: Value,
    unsupported: Vec<Unsupported>,
    skipped: Vec<String>,
    source: PathBuf,
    written: Option<PathBuf>,
    report: ImportReport,
}

#[derive(Serialize)]
struct Bridge {
    id: String,
    module: String,
    config: PathBuf,
    requirement: &'static str,
}

#[derive(Serialize)]
struct ImportReport {
    ignored: Vec<Unsupported>,
    bridged: Vec<Bridge>,
    manual: Vec<Unsupported>,
}

fn default_source() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("STARSHIP_CONFIG").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".config").join("starship.toml"))
}

fn fail(json: bool, code: ErrorCode, message: String, hint: Option<String>) -> i32 {
    if json {
        print_json(&CliEnvelope::<()>::err(code, message, hint));
    } else {
        eprintln!("tildy import: {message}");
        if let Some(h) = hint {
            eprintln!("hint: {h}");
        }
    }
    code.exit_code()
}

fn print_json<T: Serialize>(v: &T) {
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("tildy import: cannot encode JSON: {e}"),
    }
}

fn print_unsupported(items: &[Unsupported]) {
    if !items.is_empty() {
        eprintln!("tildy import: {} item(s) not imported:", items.len());
        for u in items {
            eprintln!("  {}: {}", u.item, u.reason);
        }
    }
}

pub fn run(a: ImportArgs) -> i32 {
    if a.source != "starship" {
        return fail(a.json, ErrorCode::ValidationFailed, format!("unknown import source '{}'", a.source), Some("only 'starship' is supported".into()));
    }
    let Some(source) = a.file.clone().or_else(default_source) else {
        return fail(a.json, ErrorCode::EnvironmentUnavailable, "cannot locate starship.toml".into(), Some("pass --file or set STARSHIP_CONFIG".into()));
    };
    let text = match std::fs::read_to_string(&source) {
        Ok(t) => t,
        Err(e) => {
            return fail(a.json, ErrorCode::EnvironmentUnavailable, format!("cannot read {}: {e}", source.display()), Some("pass --file".into()));
        }
    };
    let mut res = match import_starship(&text) {
        Ok(r) => r,
        Err(e) => return fail(a.json, ErrorCode::ValidationFailed, format!("{}: {e}", source.display()), None),
    };
    let canonical = match source.canonicalize() {
        Ok(p) => p,
        Err(e) => return fail(a.json, ErrorCode::EnvironmentUnavailable, format!("cannot resolve {}: {e}", source.display()), None),
    };
    let Some(config_path) = canonical.to_str() else {
        return fail(a.json, ErrorCode::ValidationFailed, "source path must be UTF-8".into(), None);
    };
    let mut report = ImportReport {
        ignored: res.unsupported.iter().filter(|n| n.resolution == Resolution::Ignored).cloned().collect(),
        manual: res.unsupported.iter().filter(|n| n.resolution == Resolution::Manual).cloned().collect(),
        bridged: vec![],
    };
    for id in &res.skipped {
        report.ignored.push(Unsupported { item: id.clone(), reason: "module disabled in source or Starship defaults".into(), resolution: Resolution::Ignored });
    }
    for (id, module) in res.candidate["modules"].as_object_mut().expect("importer modules") {
        if module["type"] == "starship" {
            module["starship_config"] = Value::String(config_path.into());
            report.bridged.push(Bridge { id: id.clone(), module: module["module"].as_str().unwrap().into(), config: canonical.clone(), requirement: "requires Starship on PATH and the original TOML; its module settings remain live" });
        }
    }
    let pretty = serde_json::to_string_pretty(&res.candidate).expect("candidate serializes");

    // The candidate must load as a tildy config before it can be written.
    let validation = tildy::config::from_str(&pretty).err();

    let mut data = ImportData {
        candidate: res.candidate,
        unsupported: res.unsupported,
        skipped: res.skipped,
        source,
        written: None,
        report,
    };

    if let Some(e) = validation {
        let msg = format!("candidate does not validate as a tildy config: {e}");
        if a.json {
            let env = CliEnvelope {
                api_version: API_VERSION,
                ok: false,
                data: Some(data),
                error: CliError {
                    code: ErrorCode::ValidationFailed,
                    path: Some(e.path.clone()),
                    message: Some(msg),
                    hint: Some("the candidate is included in data; nothing was written".into()),
                },
            };
            print_json(&env);
        } else {
            println!("{pretty}");
            eprintln!("tildy import: warning: {msg}");
            print_unsupported(&data.unsupported);
            if a.write {
                eprintln!("tildy import: nothing was written");
            }
        }
        return ErrorCode::ValidationFailed.exit_code();
    }

    if a.write {
        let result = Store::from_env().and_then(|store| store.apply(format!("{pretty}\n").as_bytes(), Some("builtin"), false));
        match result {
            Ok(applied) => data.written = Some(applied.path),
            Err(e) => {
                if a.json {
                    let mut env = CliEnvelope::<()>::err(e.code, e.message, e.hint);
                    env.error.path = e.path;
                    print_json(&env);
                    return e.code.exit_code();
                }
                return fail(false, e.code, e.message, e.hint);
            }
        }
    }

    if a.json {
        print_json(&CliEnvelope::ok(data));
    } else {
        match &data.written {
            Some(p) => eprintln!("tildy import: wrote {}", p.display()),
            None => println!("{pretty}"),
        }
        print_unsupported(&data.report.ignored);
        if !data.report.manual.is_empty() {
            eprintln!("tildy import: manual adaptation required:");
            for item in &data.report.manual { eprintln!("  {}: {}", item.item, item.reason); }
        }
        for bridge in &data.report.bridged {
            eprintln!("tildy import: bridge {} -> Starship module {} ({})", bridge.id, bridge.module, bridge.config.display());
        }
    }
    0
}
