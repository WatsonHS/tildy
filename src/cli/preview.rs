//! Shared preview helpers for `tildy preset show` and `tildy theme`:
//! sample data, the current directory's real data (`--here`), Zsh prompt
//! escape -> ANSI expansion, terminal width, and composing rendered lines with
//! the right prompt for a terminal preview.
use super::i18n::{description, text, tr, tr_println};
use clap::Args;
use super::scenarios::{self, SCENARIOS};
use serde::Serialize;
use tildy::config::{self, Config, ConfigError, SymbolTier};
use tildy::model::{CliEnvelope, ErrorCode, Freshness, GitData, GitState, ModuleReport, ModuleValue, PromptData, PromptRequest, Slot};
use tildy::render::report;
use tildy::providers::{self, ProviderValue};
use tildy::render::{self, LocalTime, RenderOutput};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Human label for a preset's Nerd Font requirement (`PresetInfo::nerd_font`).
pub(crate) fn nerd_label(n: &str) -> &'static str {
    match n {
        "required" => text("Nerd Font required", "需要 Nerd 字体"),
        "optional" => text("Nerd Font optional", "Nerd 字体可选"),
        _ => text("No Nerd Font needed", "无需 Nerd 字体"),
    }
}

pub(crate) fn parse_tier(s: &str) -> Option<SymbolTier> {
    match s.to_ascii_lowercase().as_str() {
        "nerd" => Some(SymbolTier::Nerd),
        "unicode" => Some(SymbolTier::Unicode),
        "ascii" => Some(SymbolTier::Ascii),
        _ => None,
    }
}

/// Fixed wall-clock time used with sample data (14:05).
pub(crate) fn sample_now() -> Option<LocalTime> {
    LocalTime::parse("2026-10-19T14:05:00")
}

/// Terminal width of stdout (COLUMNS, then 100 when not a terminal).
pub(crate) fn terminal_cols() -> usize {
    // SAFETY: TIOCGWINSZ writes a winsize into the provided struct; fd 1 may not
    // be a terminal, in which case the call fails and we fall back.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 {
        return ws.ws_col as usize;
    }
    std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).filter(|c| *c > 0).unwrap_or(100)
}

fn env_string() -> String {
    const KEYS: &[&str] =
        &["SSH_CONNECTION", "SSH_TTY", "USER", "HOST", "VIRTUAL_ENV", "CONDA_DEFAULT_ENV", "DOCKER_CONTEXT", "DOCKER_HOST", "PATH", "SHLVL", "IN_NIX_SHELL", "name", "DIRENV_FILE", "DIRENV_DIR"];
    KEYS.iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| format!("{k}={v}")))
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

fn request(cwd: &Path, cols: usize, env: String) -> PromptRequest {
    PromptRequest {
        gen: 1,
        session_id: "preset-show".into(),
        cwd: cwd.to_path_buf(),
        cols: cols.min(u16::MAX as usize) as u16,
        lines: 40,
        last_exit: 0,
        last_duration_ms: 3200,
        env,
        jobs: 1,
    }
}

fn not_detected() -> Slot<ModuleValue> {
    Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: None, error: None }
}

fn module(text: &str, fields: &[(&str, &str)]) -> Slot<ModuleValue> {
    Slot::fresh(ModuleValue {
        text: text.into(),
        fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        prerendered: None,
    })
}

/// Sample data: `~/code/tildy`, git dirty on `main`, node 22.3.0, python
/// 3.12.1 in venv `demo`, last command 3.2 s, one background job.
pub(crate) fn sample_data(cfg: &Config, cols: usize) -> PromptData {
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute()).unwrap_or_else(|| PathBuf::from("/Users/you"));
    let cwd = home.join("code").join("tildy");
    let env = format!("USER=you\u{1f}VIRTUAL_ENV={}", cwd.join("demo").display());
    let git = GitData {
        in_repo: true,
        git_dir: Some(cwd.join(".git")),
        worktree_root: Some(cwd.clone()),
        worktree_id: Some("sample".into()),
        branch: Some("main".into()),
        detached: false,
        unborn: false,
        head_short: Some("1a2b3c4".into()),
        upstream: Some("origin/main".into()),
        ahead: 0,
        behind: 0,
        staged: 0,
        unstaged: 2,
        untracked: 1,
        conflicted: 0,
        renamed: 0,
        deleted: 0,
        typechanged: 0,
        stashed: 0,
        operation: None,
        operation_progress: None,
        incomplete: false,
        state: GitState::Dirty,
    };
    let mut modules = BTreeMap::new();
    for (id, m) in cfg.layout_modules() {
        if !m.module_type.is_provider_backed() {
            continue;
        }
        let slot = match (m.module_type.as_str(), m.language.as_deref()) {
            ("language", Some("nodejs")) => module("22.3.0", &[("version", "22.3.0")]),
            ("python", _) => module("3.12.1", &[("version", "3.12.1"), ("env", "demo")]),
            _ => not_detected(),
        };
        modules.insert(id.to_string(), slot);
    }
    PromptData {
        request: request(&cwd, cols, env),
        home: Some(home),
        git: Slot::fresh(git),
        ssh: false,
        hostname: Some("mbp".into()),
        username: Some("you".into()),
        modules,
        changes: None,
    }
}

/// Real data for `cwd`: git status, session info and every provider-backed
/// module of the preset, fetched in parallel. Also returns the provider
/// configuration problems (the caller decides where to report them).
pub(crate) fn here_data(cfg: &Config, cwd: &Path, cols: usize) -> (PromptData, Vec<String>) {
    let (data, problems, _) = here_data_with_timings(cfg, cwd, cols);
    (data, problems)
}

pub(crate) fn here_data_with_timings(cfg: &Config, cwd: &Path, cols: usize) -> (PromptData, Vec<String>, Vec<tildy::diagnostics::ProviderCycleTiming>) {
    let req = request(cwd, cols, env_string());
    let (list, problems) = providers::from_config_checked(cfg);
    let values: Vec<(ProviderValue, tildy::diagnostics::ProviderCycleTiming)> = std::thread::scope(|s| {
        let handles: Vec<_> = list.iter().map(|p| s.spawn(|| {
            let started = std::time::Instant::now();
            let value = p.fetch(&req);
            let timed_out = match &value {
                ProviderValue::Git(slot) => slot.freshness == Freshness::Timeout,
                ProviderValue::Module(_, slot) => slot.freshness == Freshness::Timeout,
                _ => false,
            };
            let timing = tildy::diagnostics::ProviderCycleTiming { provider: p.spec().key.into(), duration_ms: started.elapsed().as_millis() as u64, cache_hit: false, executed: true, completed: true, timed_out, coalesced: false, coalesced_count: 0 };
            (value, timing)
        })).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut data = PromptData {
        request: req.clone(),
        home: std::env::var_os("HOME").map(PathBuf::from),
        git: Slot::pending(),
        ssh: false,
        hostname: None,
        username: None,
        modules: BTreeMap::new(),
        changes: None,
    };
    let mut timings = vec![];
    for (v, timing) in values {
        timings.push(timing);
        match v {
            ProviderValue::Git(g) => data.git = g,
            ProviderValue::Ssh(b) => data.ssh = b,
            ProviderValue::Hostname(h) => data.hostname = h,
            ProviderValue::Username(u) => data.username = u,
            ProviderValue::Module(id, slot) => {
                data.modules.insert(id, slot);
            }
        }
    }
    (data, problems, timings)
}

/// The rendered left lines expanded to ANSI, with the right prompt placed on
/// the last line the way Zsh draws RPROMPT (right-aligned, one column from the
/// edge), or appended after a dim marker when it does not fit in `cols`.
pub(crate) fn compose_lines(out: &RenderOutput, cols: usize) -> Vec<String> {
    let n = out.prompt.left.len();
    let mut lines = Vec::with_capacity(n);
    for (i, line) in out.prompt.left.iter().enumerate() {
        let mut s = zsh_to_ansi(line);
        if i + 1 == n {
            if let Some(r) = &out.prompt.right {
                // Zsh puts RPROMPT one column from the right edge (ZLE_RPROMPT_INDENT=1).
                let (lw, rw) = (out.columns[i], render::visible_width(r));
                if lw + rw + 2 <= cols {
                    s.push_str(&" ".repeat(cols - lw - rw - 1));
                    s.push_str(&zsh_to_ansi(r));
                } else {
                    s.push_str(&tr!("   \x1b[2m(right:)\x1b[0m {}", "   \x1b[2m(右侧:)\x1b[0m {}", zsh_to_ansi(r)));
                }
            }
        }
        lines.push(s);
    }
    lines
}

/// ANSI color parameters for a Zsh color argument (`%F{..}`/`%K{..}`).
fn color_sgr(c: &str, bg: bool) -> Option<String> {
    const NAMES: [&str; 8] = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    let base = if bg { 40 } else { 30 };
    if c == "default" {
        return Some((base + 9).to_string());
    }
    if let Some(i) = NAMES.iter().position(|n| *n == c) {
        return Some((base + i).to_string());
    }
    if let Some(hex) = c.strip_prefix('#') {
        if hex.len() == 6 {
            let p = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
            if let (Some(r), Some(g), Some(b)) = (p(0), p(2), p(4)) {
                return Some(format!("{};2;{r};{g};{b}", base + 8));
            }
        }
        return None;
    }
    let n: u8 = c.parse().ok()?;
    Some(match n {
        0..=7 => (base + n as usize).to_string(),
        8..=15 => (base + 60 + (n as usize - 8)).to_string(),
        _ => format!("{};5;{n}", base + 8),
    })
}

/// Expand Zsh prompt escapes (`%F{..}` `%K{..}` `%f` `%k` `%B` `%b` `%U` `%u`
/// `%S` `%s` `%{..%}` `%%`) to ANSI escape sequences for a terminal preview.
pub(crate) fn zsh_to_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('%') => out.push('%'),
            Some('{') => {
                // Width declarations are metadata; emit the cluster itself.
                while let Some(c) = it.next() {
                    if c != '%' { out.push(c); continue; }
                    match it.next() {
                        Some('}') => break,
                        Some('%') => out.push('%'),
                        Some('G') => {},
                        Some(c) if c.is_ascii_digit() => {
                            let mut digits = c.to_string();
                            while it.peek().is_some_and(char::is_ascii_digit) { digits.push(it.next().unwrap()); }
                            if it.peek() == Some(&'G') { it.next(); } else { out.push('%'); out.push_str(&digits); }
                        }
                        Some(c) => { out.push('%'); out.push(c); }
                        None => break,
                    }
                }
            }
            Some(k @ ('F' | 'K')) => {
                if it.peek() == Some(&'{') {
                    it.next();
                    let mut arg = String::new();
                    for n in it.by_ref() {
                        if n == '}' {
                            break;
                        }
                        arg.push(n);
                    }
                    if let Some(p) = color_sgr(&arg, k == 'K') {
                        out.push_str(&format!("\x1b[{p}m"));
                    }
                }
            }
            Some('f') => out.push_str("\x1b[39m"),
            Some('k') => out.push_str("\x1b[49m"),
            Some('B') => out.push_str("\x1b[1m"),
            Some('b') => out.push_str("\x1b[22m"),
            Some('U') => out.push_str("\x1b[4m"),
            Some('u') => out.push_str("\x1b[24m"),
            Some('S') => out.push_str("\x1b[7m"),
            Some('s') => out.push_str("\x1b[27m"),
            Some(_) | None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tildy::config::{self, PRESET_INFO};

    #[test]
    fn zsh_escapes_become_ansi() {
        assert_eq!(zsh_to_ansi("%F{red}a%f%K{#FF8800}b%k%%"), "\x1b[31ma\x1b[39m\x1b[48;2;255;136;0mb\x1b[49m%");
        assert_eq!(zsh_to_ansi("%B%F{9}x%b%U%F{208}y%u"), "\x1b[1m\x1b[91mx\x1b[22m\x1b[4m\x1b[38;5;208my\x1b[24m");
        assert_eq!(zsh_to_ansi("%{\x1b[3m%}i%{\x1b[23m%}"), "\x1b[3mi\x1b[23m");
        assert_eq!(zsh_to_ansi("%{👩‍💻%3G%}%{%%%2G%}"), "👩‍💻%");
        assert_eq!(zsh_to_ansi("%K{default}%F{default}"), "\x1b[49m\x1b[39m");
    }

    #[test]
    fn every_preset_renders_sample_data_in_every_tier() {
        for p in PRESET_INFO {
            for t in [SymbolTier::Nerd, SymbolTier::Unicode, SymbolTier::Ascii] {
                let cfg = config::preset(p.name).unwrap().with_symbols(t);
                let d = sample_data(&cfg, 100);
                let out = render::render_at(&cfg, &d, sample_now());
                assert!(!out.prompt.left.is_empty(), "{}", p.name);
                for l in &out.prompt.left {
                    let a = zsh_to_ansi(l);
                    assert!(!a.contains("%F") && !a.contains("%K"), "{}: {a:?}", p.name);
                }
                if t == SymbolTier::Ascii {
                    for l in out.prompt.left.iter().chain(out.prompt.right.iter()) {
                        let v = render::strip_prompt_escapes(l);
                        assert!(!v.chars().any(|c| ('\u{e000}'..='\u{f8ff}').contains(&c) || c > '\u{f0000}'), "{} ascii: {v:?}", p.name);
                    }
                }
            }
        }
    }
}

/// `tildy preview --scenario NAME`: render a fixed scenario (no repository
/// access, no command execution, time fixed at 14:05) with a structured
/// per-module report. Scenario data lives in `scenarios.rs`.
#[derive(Args)]
pub struct PreviewArgs {
    /// Scenario name (see --list).
    #[arg(long)]
    pub scenario: Option<String>,
    /// Render every scenario in turn.
    #[arg(long)]
    pub all: bool,
    /// List the scenario names.
    #[arg(long)]
    pub list: bool,
    /// Config file to preview (default: the active config, TILDY_CONFIG or ~/.config/tildy/config.json).
    #[arg(long, conflicts_with = "preset")]
    pub config: Option<PathBuf>,
    /// Built-in preset to preview instead of the active config.
    #[arg(long)]
    pub preset: Option<String>,
    /// Terminal width (default: the scenario's own, 100 except `narrow`).
    #[arg(long)]
    pub cols: Option<u16>,
    /// Symbol tier override: nerd|unicode|ascii.
    #[arg(long)]
    pub symbols: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct ScenarioEntry {
    name: &'static str,
    description: &'static str,
    cols: u16,
}

#[derive(Serialize)]
struct PreviewData {
    scenario: String,
    cols: u16,
    symbols: &'static str,
    lines: Vec<String>,
    right: Option<String>,
    transient: Option<String>,
    modules: Vec<ModuleReport>,
}

fn fail(json: bool, code: ErrorCode, path: &str, message: String, hint: Option<String>) -> i32 {
    if json {
        let mut env = CliEnvelope::<()>::err(code, message, hint);
        env.error.path = Some(path.to_string()).filter(|p| !p.is_empty());
        println!("{}", serde_json::to_string_pretty(&env).unwrap_or_default());
    } else {
        eprintln!("tildy preview: {message}");
        if let Some(h) = hint {
            eprintln!("hint: {h}");
        }
    }
    code.exit_code()
}

fn emit<T: Serialize>(data: &T) -> i32 {
    match serde_json::to_string_pretty(&CliEnvelope::ok(data)) {
        Ok(s) => {
            println!("{s}");
            0
        }
        Err(e) => {
            eprintln!("tildy preview: {e}");
            ErrorCode::InternalError.exit_code()
        }
    }
}

fn load_config(a: &PreviewArgs) -> Result<(Config, Option<String>), (String, String)> {
    if let Some(name) = &a.preset {
        return config::preset(name)
            .map(|c| (c, Some(name.clone())))
            .map_err(|e| (e.path.clone(), format!("{e}")));
    }
    config::load(a.config.as_deref()).map(|c| {
        let c = if a.config.is_none() {
            config::project::for_directory(c, &std::env::current_dir().unwrap_or_default())
        } else { c };
        (c, None)
    }).map_err(|e| match e.downcast_ref::<ConfigError>() {
        Some(ce) => (ce.path.clone(), format!("{ce}")),
        None => (String::new(), e.to_string()),
    })
}

pub fn run(a: PreviewArgs) -> i32 {
    if a.list {
        let list: Vec<ScenarioEntry> =
            SCENARIOS.iter().map(|s| ScenarioEntry { name: s.name, description: s.description, cols: s.cols }).collect();
        if a.json {
            return emit(&list);
        }
        let w = list.iter().map(|s| s.name.len()).max().unwrap_or(0);
        for s in &list {
            println!("{:<w$}  {}", s.name, description(s.description));
        }
        return 0;
    }
    let names: Vec<&str> = match (&a.scenario, a.all) {
        (_, true) => SCENARIOS.iter().map(|s| s.name).collect(),
        (Some(n), false) => match scenarios::find(n) {
            Some(s) => vec![s.name],
            None => {
                return fail(
                    a.json,
                    ErrorCode::ValidationFailed,
                    "scenario",
                    format!("unknown scenario '{n}'"),
                    Some("tildy preview --list".into()),
                )
            }
        },
        (None, false) => {
            return fail(
                a.json,
                ErrorCode::ValidationFailed,
                "scenario",
                "name a scenario or pass --all".into(),
                Some("tildy preview --list".into()),
            )
        }
    };
    let tier = match a.symbols.as_deref().map(|s| (s, parse_tier(s))) {
        None => None,
        Some((_, Some(t))) => Some(t),
        Some((s, None)) => {
            return fail(a.json, ErrorCode::ValidationFailed, "symbols", format!("unknown symbol tier '{s}' (nerd, unicode or ascii)"), None)
        }
    };
    if a.cols == Some(0) {
        return fail(a.json, ErrorCode::ValidationFailed, "cols", "--cols must be at least 1".into(), None);
    }
    let (cfg, preset) = match load_config(&a) {
        Ok(c) => c,
        Err((path, msg)) => return fail(a.json, ErrorCode::ValidationFailed, &path, msg, Some("tildy config validate --file FILE".into())),
    };
    let cfg = match tier {
        Some(t) => cfg.with_symbols(t),
        None => cfg,
    };
    let mut all = vec![];
    for (i, n) in names.iter().enumerate() {
        let Some(data) = scenarios::build(n, &cfg, a.cols) else {
            return fail(a.json, ErrorCode::InternalError, "", format!("scenario '{n}' has no data"), None);
        };
        let cols = data.request.cols as usize;
        if a.json {
            let out = report::render_annotated(&cfg, preset.as_deref(), &data, sample_now(), false);
            all.push(PreviewData {
                scenario: n.to_string(),
                cols: data.request.cols,
                symbols: cfg.symbols.as_str(),
                lines: out.prompt.left,
                right: out.prompt.right,
                transient: out.prompt.transient,
                modules: out.reports,
            });
        } else {
            let out = report::render_annotated(&cfg, preset.as_deref(), &data, sample_now(), true);
            if i > 0 {
                println!();
            }
            print_text(n, &cfg, &out, cols);
        }
    }
    if a.json {
        return if a.all { emit(&all) } else { all.pop().map_or(1, |d| emit(&d)) };
    }
    0
}

fn print_text(name: &str, cfg: &Config, out: &RenderOutput, cols: usize) {
    let desc = scenarios::find(name).map_or("", |s| description(s.description));
    tr_println!("\x1b[1m── {name}\x1b[0m  {desc}  \x1b[2m[{} columns, symbols {}]\x1b[0m", "\x1b[1m── {name}\x1b[0m  {desc}  \x1b[2m[{} 列，符号层 {}]\x1b[0m", cols, cfg.symbols.as_str());
    for line in compose_lines(out, cols) {
        println!("{line}\x1b[0m");
    }
    if let Some(t) = &out.prompt.transient {
        tr_println!("\x1b[2mAfter execution:\x1b[0m {}\x1b[0m", "\x1b[2m执行后收起为:\x1b[0m {}\x1b[0m", zsh_to_ansi(t));
    }
    for r in &out.reports {
        let state = match (r.visible, &r.hidden_reason) {
            (true, _) => "visible".to_string(),
            (false, Some(why)) => format!("hidden({why})"),
            (false, None) => "hidden".to_string(),
        };
        let mut extra = String::new();
        if let Some(src) = &r.source {
            extra.push_str(&format!("  source={src}"));
        }
        if let (Some(row), Some(side)) = (r.row, &r.side) {
            extra.push_str(&format!("  at={row}/{side}"));
        }
        if let Some(f) = r.freshness.filter(|f| *f != Freshness::Fresh) {
            extra.push_str(&format!("  freshness={}", serde_json::to_value(f).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()));
        }
        if let Some(age) = r.age_ms.filter(|a| *a > 0) {
            extra.push_str(&format!("  age={age}ms"));
        }
        if let Some(st) = r.style.as_deref().filter(|s| !s.is_empty()) {
            extra.push_str(&format!("  style=[{st}]"));
        }
        println!(
            "  {:<12} {:<14} {:<26} rules=[{}]  cols={}{extra}",
            r.id,
            r.module_type,
            state,
            r.rules_hit.join(","),
            r.columns
        );
    }
}
