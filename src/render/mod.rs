//! Rules, layout (plain and powerline rows), width, Zsh escaping. Owner: M1b task A.
//!
//! Attribution: the directory module (home contraction, last-N truncation,
//! repo-root contraction, substitutions), the git status symbol set and the
//! Pastel Powerline look follow Starship (ISC licence, https://starship.rs).
//! The code here is an independent implementation; NOTICE credits Starship.
use crate::config::template::{self, Run};
use crate::config::{
    Condition, Config, ModuleConfig, ModuleType, Op, Row, RowStyle, StatusSymbols, Style, Sym, SymbolTier, GIT_OPERATIONS,
};
use crate::model::{Freshness, GitData, GitState, HistoryMetadata, ModuleReport, ModuleValue, PromptData, RenderStatus, RenderedPrompt, Slot};
use serde::Serialize;
use serde_json::Value;
use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::path::{Component, Path};
use unicode_segmentation::UnicodeSegmentation;

mod agent;
mod time;
pub use time::LocalTime;
pub mod report;

#[derive(Debug, Clone, Serialize)]
pub struct RenderOutput {
    pub prompt: RenderedPrompt,
    pub reports: Vec<ModuleReport>,
    /// Display columns of each `prompt.left` line (unicode-width; Nerd Font
    /// private-use glyphs and the powerline separator count as 1).
    pub columns: Vec<usize>,
}

struct Glyphs {
    prompt: &'static str,
    ellipsis: &'static str,
    pending: &'static str,
    up: &'static str,
    down: &'static str,
    pm: &'static str,
}

impl Glyphs {
    fn for_tier(t: SymbolTier) -> Glyphs {
        match t {
            SymbolTier::Ascii => Glyphs { prompt: ">", ellipsis: "...", pending: "...", up: "^", down: "v", pm: "+-" },
            SymbolTier::Unicode | SymbolTier::Nerd => {
                Glyphs { prompt: "❯", ellipsis: "…", pending: "…", up: "↑", down: "↓", pm: "±" }
            }
        }
    }
}

fn tier3(t: SymbolTier, nerd: &'static str, unicode: &'static str, ascii: &'static str) -> &'static str {
    match t {
        SymbolTier::Nerd => nerd,
        SymbolTier::Unicode => unicode,
        SymbolTier::Ascii => ascii,
    }
}

/// Built-in `$symbol` per module type (and language), by tier.
fn default_symbol(m: &ModuleConfig, t: SymbolTier) -> &'static str {
    match m.module_type {
        ModuleType::Language => match m.language.as_deref() {
            Some("nodejs") => tier3(t, "\u{e718}", "⬢", "node"),
            Some("rust") => tier3(t, "\u{e7a8}", "rust", "rust"),
            Some("golang") => tier3(t, "\u{e627}", "go", "go"),
            Some("java") => tier3(t, "\u{e256}", "java", "java"),
            Some("c") => tier3(t, "\u{e61e}", "c", "c"),
            Some("python") => tier3(t, "\u{e235}", "py", "py"),
            _ => "",
        },
        ModuleType::Python => tier3(t, "\u{e235}", "py", "py"),
        ModuleType::DockerContext => tier3(t, "\u{f308}", "docker", "docker"),
        ModuleType::Jobs => tier3(t, "✦", "✦", "&"),
        ModuleType::Character => tier3(t, "❯", "❯", ">"),
        ModuleType::ExitStatus => tier3(t, "✗", "✗", "x"),
        ModuleType::Fill => ".",
        ModuleType::Package => tier3(t, "\u{f487}", "📦", "pkg"),
        ModuleType::Shlvl => tier3(t, "↕", "↕", "lvl"),
        ModuleType::MemoryUsage => tier3(t, "\u{f2db}", "🐏", "mem"),
        ModuleType::Sudo => tier3(t, "\u{f132}", "🧙", "sudo"),
        ModuleType::Direnv => "direnv",
        ModuleType::NixShell => tier3(t, "\u{f313}", "❄", "nix"),
        ModuleType::Changes => tier3(t, "\u{f021} ", "⟳ ", "~ "),
        _ => "",
    }
}

/// Starship's `shell` indicators.
fn default_shell_indicator(name: &str) -> Option<&'static str> {
    Some(match name {
        "bash" => "bsh",
        "fish" => "fsh",
        "zsh" => "zsh",
        "powershell" | "pwsh" => "psh",
        "ion" => "ion",
        "elvish" => "esh",
        "tcsh" => "tsh",
        "xonsh" => "xsh",
        "cmd" => "cmd",
        "nu" => "nu",
        _ => return None,
    })
}

/// What an exit code means (Starship's `status` variables).
struct ExitMeaning {
    common: Option<&'static str>,
    signal: Option<i32>,
    signal_name: Option<String>,
}

fn exit_meaning(code: i32, recognize_signals: bool) -> ExitMeaning {
    let common = match code {
        0 => Some(""),
        1 => Some("ERROR"),
        2 | 64 => Some("USAGE"),
        65 => Some("DATAERR"),
        66 => Some("NOINPUT"),
        67 => Some("NOUSER"),
        68 => Some("NOHOST"),
        69 => Some("UNAVAILABLE"),
        70 => Some("SOFTWARE"),
        71 => Some("OSERR"),
        72 => Some("OSFILE"),
        73 => Some("CANTCREAT"),
        74 => Some("IOERR"),
        75 => Some("TEMPFAIL"),
        76 => Some("PROTOCOL"),
        77 | 126 => Some("NOPERM"),
        78 => Some("CONFIG"),
        127 => Some("NOTFOUND"),
        _ => None,
    };
    let signal = (recognize_signals && code > 128).then_some(code - 128);
    const NAMES: [&str; 22] = [
        "HUP", "INT", "QUIT", "ILL", "TRAP", "IOT", "BUS", "FPE", "KILL", "USR1", "SEGV", "USR2", "PIPE", "ALRM", "TERM", "STKFLT",
        "CHLD", "CONT", "STOP", "TSTP", "TTIN", "TTOU",
    ];
    let signal_name = signal.map(|s| usize::try_from(s - 1).ok().and_then(|i| NAMES.get(i)).map_or_else(|| s.to_string(), |n| n.to_string()));
    ExitMeaning { common, signal, signal_name }
}

/// Starship's `detect_env_vars` as `username` reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvDetect {
    Empty,
    /// A `!NAME` entry is set.
    Negated,
    Yes,
    No,
}

fn env_detect(list: &[String], env: &BTreeMap<String, String>) -> EnvDetect {
    if list.is_empty() {
        return EnvDetect::Empty;
    }
    let set = |k: &str| env.get(k).is_some_and(|v| !v.is_empty());
    if list.iter().filter_map(|v| v.strip_prefix('!')).any(set) {
        return EnvDetect::Negated;
    }
    if list.iter().filter(|v| !v.starts_with('!')).any(|v| set(v)) { EnvDetect::Yes } else { EnvDetect::No }
}

/// Built-in `os` symbol per `std::env::consts::OS` name and tier.
fn os_default_symbol(os: &str, t: SymbolTier) -> String {
    match os {
        "macos" => tier3(t, "\u{f179}", "mac", "mac").into(),
        "linux" => tier3(t, "\u{f17c}", "linux", "linux").into(),
        "windows" => tier3(t, "\u{f17a}", "win", "win").into(),
        "freebsd" => tier3(t, "\u{f30c}", "freebsd", "freebsd").into(),
        other => other.into(),
    }
}

fn default_status_symbol(key: &str, t: SymbolTier) -> &'static str {
    match key {
        "conflicted" => "=",
        "ahead" => tier3(t, "⇡", "⇡", "^"),
        "behind" => tier3(t, "⇣", "⇣", "v"),
        "diverged" => tier3(t, "⇕", "⇕", "^v"),
        "untracked" => "?",
        "stashed" => "$",
        "modified" => "!",
        "typechanged" => "~",
        "staged" => "+",
        "renamed" => tier3(t, "»", "»", ">"),
        "deleted" => tier3(t, "✘", "✘", "x"),
        _ => "",
    }
}

fn default_format(m: &ModuleConfig, symbol: &str) -> &'static str {
    match m.module_type {
        ModuleType::Directory => "$path",
        ModuleType::Git => "($branch_symbol )$branch( $status)",
        ModuleType::ExitStatus => "$symbol $code",
        ModuleType::Changes => "$symbol$text",
        ModuleType::Duration | ModuleType::Hostname | ModuleType::Starship | ModuleType::Text => "$text",
        ModuleType::AgentStatus => "$symbol( $app)( $msg)( +$more)",
        ModuleType::Os | ModuleType::Fill => "$symbol",
        ModuleType::EnvVar => "($symbol )$env_value",
        ModuleType::Command => "($symbol )$text",
        ModuleType::Language => "$symbol( $version)",
        ModuleType::Python => "$symbol( $version)( \\($env\\))",
        ModuleType::DockerContext => "$symbol( $context)",
        ModuleType::Character => "$symbol",
        ModuleType::Time => "($symbol )$time",
        ModuleType::Jobs => "$symbol$count",
        ModuleType::Package => "($symbol )$version",
        ModuleType::GitCommit => "\\($hash$tag\\)",
        ModuleType::GitMetrics => "$diff",
        ModuleType::Shlvl => "($symbol )$shlvl",
        ModuleType::Localip => "$localipv4",
        ModuleType::MemoryUsage => "($symbol )$ram( | $swap)",
        ModuleType::Sudo => "$symbol",
        ModuleType::Direnv => "($symbol )$loaded/$allowed",
        ModuleType::NixShell => "$symbol( $state)( \\($name\\))",
        ModuleType::Shell => "$indicator",
        ModuleType::Username => {
            if symbol.is_empty() {
                "$user"
            } else {
                "$symbol"
            }
        }
    }
}

/// Remove C0/C1 controls, DEL, ESC and bidi override characters.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .collect()
}

/// Escape untrusted text for inclusion in PROMPT (handles `%`, control chars).
/// Note: `$`/backticks are NOT escaped; the shell adapter must not enable
/// PROMPT_SUBST on this output.
pub fn escape_prompt_text(s: &str) -> String {
    crate::width::escape(&sanitize(s))
}

/// Add measured declarations to literal portions of an existing prompt, such
/// as an explicitly imported Starship bridge. Existing escape groups survive.
fn calibrate_prompt_literals(s: &str) -> String {
    let mut output = String::new();
    let mut literal = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' { literal.push(c); continue; }
        if chars.peek() == Some(&'%') { chars.next(); literal.push('%'); continue; }
        output.push_str(&crate::width::escape(&literal)); literal.clear();
        output.push('%');
        match chars.next() {
            Some(c @ ('F' | 'K')) => {
                output.push(c);
                if chars.peek() == Some(&'{') { for c in chars.by_ref() { output.push(c); if c == '}' { break; } } }
            }
            Some('{') => {
                output.push('{');
                let mut previous = '\0';
                for c in chars.by_ref() { output.push(c); if previous == '%' && c == '}' { break; } previous = c; }
            }
            Some(c) => output.push(c),
            None => {},
        }
    }
    output.push_str(&crate::width::escape(&literal)); output
}

/// Decode prompt text and its declared cell advances. Measured glyphs remain
/// visible inside `%{glyph%NG%}`; raw SGR groups remain zero-width/invisible.
fn prompt_text_and_width(s: &str) -> (String, usize) {
    let mut plain = String::new();
    let mut pending = String::new();
    let mut columns = 0;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '%' { pending.push(c); continue; }
        match it.next() {
            Some('%') => pending.push('%'),
            Some('{') => {
                columns += crate::width::width(&pending);
                plain.push_str(&pending); pending.clear();
                let mut inner = String::new();
                let mut declared = 0usize;
                let mut has_control = false;
                while let Some(c) = it.next() {
                    if c != '%' { has_control |= c.is_control(); inner.push(c); continue; }
                    match it.next() {
                        Some('}') => break,
                        Some('%') => inner.push('%'),
                        Some('G') => declared += 1,
                        Some(c) if c.is_ascii_digit() => {
                            let mut digits = c.to_string();
                            while it.peek().is_some_and(char::is_ascii_digit) { digits.push(it.next().unwrap()); }
                            if it.peek() == Some(&'G') {
                                it.next(); declared = declared.saturating_add(digits.parse::<usize>().unwrap_or(0).min(65535));
                            }
                        }
                        Some(c) => { inner.push('%'); inner.push(c); }
                        None => break,
                    }
                }
                if !has_control { plain.push_str(&inner); }
                columns = columns.saturating_add(declared);
            }
            Some('F') | Some('K') => {
                if it.peek() == Some(&'{') { for c in it.by_ref() { if c == '}' { break; } } }
            }
            Some(_) | None => {}
        }
    }
    columns += crate::width::width(&pending); plain.push_str(&pending);
    (plain, columns)
}

pub fn strip_prompt_escapes(s: &str) -> String { prompt_text_and_width(s).0 }

/// Display columns, honoring measured `%G` declarations even outside a render.
pub fn visible_width(s: &str) -> usize { prompt_text_and_width(s).1 }

#[derive(Debug, Clone)]
enum FieldVal {
    Bool(bool),
    Num(f64),
    Str(String),
}

type Fields = BTreeMap<String, FieldVal>;

fn build_fields(d: &PromptData) -> Fields {
    use FieldVal::*;
    let mut f: Fields = BTreeMap::new();
    let mut put = |k: &str, v: FieldVal| {
        f.insert(k.to_string(), v);
    };
    put("session.ssh", Bool(d.ssh));
    if let Some(h) = &d.hostname { put("session.hostname", Str(h.clone())); }
    if let Some(u) = &d.username { put("session.username", Str(u.clone())); }
    put("session.jobs", Num(d.request.jobs as f64));
    put("command.exit", Num(d.request.last_exit as f64));
    put("command.duration_ms", Num(d.request.last_duration_ms as f64));
    put("cwd.path", Str(d.request.cwd.to_string_lossy().into_owned()));
    put("terminal.cols", Num(d.request.cols as f64));
    // Forwarded environment (`env.CONDA_DEFAULT_ENV`, ...).
    for (k, v) in d.request.env_map() {
        put(&format!("env.{k}"), Str(v));
    }
    put("git.freshness", Str(freshness_name(d.git.freshness).into()));
    if let Some(g) = &d.git.value {
        f.insert("git.in_repo".into(), Bool(g.in_repo));
        if g.in_repo {
            let state = match g.state {
                GitState::Clean => "clean",
                GitState::Dirty => "dirty",
                GitState::Conflicted => "conflicted",
                GitState::Unknown => "unknown",
            };
            f.insert("git.state".into(), Str(state.into()));
            if let Some(b) = &g.branch { f.insert("git.branch".into(), Str(b.clone())); }
            if let Some(o) = &g.operation { f.insert("git.operation".into(), Str(o.clone())); }
            f.insert("git.detached".into(), Bool(g.detached));
            f.insert("git.unborn".into(), Bool(g.unborn));
            f.insert("git.incomplete".into(), Bool(g.incomplete));
            for (k, v) in [
                ("git.ahead", g.ahead), ("git.behind", g.behind), ("git.staged", g.staged),
                ("git.unstaged", g.unstaged), ("git.untracked", g.untracked), ("git.conflicted", g.conflicted),
                ("git.renamed", g.renamed), ("git.deleted", g.deleted), ("git.typechanged", g.typechanged), ("git.stashed", g.stashed),
            ] {
                f.insert(k.into(), Num(v as f64));
            }
        }
    }
    f
}

fn freshness_name(f: Freshness) -> &'static str {
    match f {
        Freshness::Pending => "pending",
        Freshness::Fresh => "fresh",
        Freshness::Stale => "stale",
        Freshness::Timeout => "timeout",
        Freshness::Error => "error",
    }
}

fn scalar_eq(f: &FieldVal, v: &Value) -> Option<bool> {
    match (f, v) {
        (FieldVal::Bool(a), Value::Bool(b)) => Some(a == b),
        (FieldVal::Num(a), Value::Number(b)) => b.as_f64().map(|b| *a == b),
        (FieldVal::Str(a), Value::String(b)) => Some(a == b),
        _ => None,
    }
}

/// Typed comparison. Missing field or type mismatch => false for every operator
/// (including `ne`): unknown data never counts as "clean"/"success".
fn eval(c: &Condition, fields: &Fields) -> bool {
    let Some(f) = fields.get(c.field.as_str()) else { return false };
    match c.op {
        Op::Eq => scalar_eq(f, &c.value) == Some(true),
        Op::Ne => scalar_eq(f, &c.value) == Some(false),
        Op::In => c.value.as_array().is_some_and(|a| a.iter().any(|e| scalar_eq(f, e) == Some(true))),
        Op::Gt | Op::Lt | Op::Ge | Op::Le => {
            let (FieldVal::Num(a), Some(b)) = (f, c.value.as_f64()) else { return false };
            match c.op {
                Op::Gt => *a > b,
                Op::Lt => *a < b,
                Op::Ge => *a >= b,
                _ => *a <= b,
            }
        }
    }
}

fn color(c: Option<&str>) -> Option<String> {
    c.and_then(crate::config::parse_color)
}

fn fg_code(c: Option<&str>) -> String {
    color(c).map_or_else(|| "%f".into(), |c| format!("%F{{{c}}}"))
}

fn bg_code(c: Option<&str>) -> String {
    color(c).map_or_else(|| "%k".into(), |c| format!("%K{{{c}}}"))
}

thread_local! {
    /// Emit dim/italic as raw SGR inside `%{...%}` ([`render_preview`] only).
    static RAW_SGR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn raw_sgr() -> bool {
    RAW_SGR.with(|r| r.get())
}

/// Text attributes. Zsh has escapes for bold and underline only. Dim and
/// italic would need raw SGR inside `%{...%}`, but the protocol and the shell
/// adapter strip control characters from prompt lines (safe redraw), so a
/// real prompt would show `[3m` as text: they are emitted only by
/// [`render_preview`] (terminal previews), and dropped otherwise.
fn attrs(s: &Style) -> (String, String) {
    let (mut open, mut close) = (String::new(), String::new());
    if s.bold == Some(true) { open.push_str("%B"); close.insert_str(0, "%b"); }
    if s.underline == Some(true) { open.push_str("%U"); close.insert_str(0, "%u"); }
    if raw_sgr() {
        if s.dim == Some(true) { open.push_str("%{\x1b[2m%}"); close.insert_str(0, "%{\x1b[22m%}"); }
        if s.italic == Some(true) { open.push_str("%{\x1b[3m%}"); close.insert_str(0, "%{\x1b[23m%}"); }
    }
    (open, close)
}

/// Plain-row styling: attributes, then colors; closed in reverse order.
fn wrap_style(escaped: &str, s: &Style) -> String {
    let (mut open, mut close) = attrs(s);
    if let Some(c) = color(s.foreground.as_deref()) {
        open.push_str(&format!("%F{{{c}}}"));
        close.insert_str(0, "%f");
    }
    if let Some(c) = color(s.background.as_deref()) {
        open.push_str(&format!("%K{{{c}}}"));
        close.insert_str(0, "%k");
    }
    format!("{open}{escaped}{close}")
}

/// Zsh codes that move the terminal from style `from` to style `to` (used for
/// `[...](style)` spans inside a module, whose own style is already active).
/// Turning an attribute off resets all of them (`%b` resets raw SGR state in
/// Zsh) and re-applies what `to` needs, colors included.
fn switch_style(from: &Style, to: &Style) -> String {
    let raw = raw_sgr();
    let on_any = |o: Option<bool>| o == Some(true);
    // Without raw SGR, dim/italic do not exist in the output (see `attrs`).
    let strip = |s: &Style| Style { dim: s.dim.filter(|_| raw), italic: s.italic.filter(|_| raw), ..s.clone() };
    let (from, to) = (&strip(from), &strip(to));
    let on = on_any;
    let mut out = String::new();
    let off = (on(from.bold) && !on(to.bold))
        || (on(from.underline) && !on(to.underline))
        || (on(from.dim) && !on(to.dim))
        || (on(from.italic) && !on(to.italic));
    let (mut b, mut u, mut d, mut i) = (on(from.bold), on(from.underline), on(from.dim), on(from.italic));
    if off {
        if d || i { out.push_str("%{\x1b[22;23m%}"); }
        if b { out.push_str("%b"); }
        if u { out.push_str("%u"); }
        (b, u, d, i) = (false, false, false, false);
    }
    if on(to.bold) && !b { out.push_str("%B"); }
    if on(to.underline) && !u { out.push_str("%U"); }
    if on(to.dim) && !d { out.push_str("%{\x1b[2m%}"); }
    if on(to.italic) && !i { out.push_str("%{\x1b[3m%}"); }
    let (ff, tf) = (color(from.foreground.as_deref()), color(to.foreground.as_deref()));
    if off || ff != tf { out.push_str(&fg_code(to.foreground.as_deref())); }
    let (fb, tb) = (color(from.background.as_deref()), color(to.background.as_deref()));
    if off || fb != tb { out.push_str(&bg_code(to.background.as_deref())); }
    out
}

/// Zsh text of evaluated runs inside a module whose style is `base`. Without
/// any span this is exactly `escape_prompt_text` of the text.
fn runs_to_zsh(runs: &[Run], base: &Style) -> String {
    let mut out = String::new();
    for r in runs {
        let esc = escape_prompt_text(&r.text);
        match &r.style {
            None => out.push_str(&esc),
            Some(s) => {
                let mut eff = base.clone();
                eff.overlay(s);
                out.push_str(&switch_style(base, &eff));
                out.push_str(&esc);
                out.push_str(&switch_style(&eff, base));
            }
        }
    }
    out
}

/// `runs` with `style` on the runs that have no span style of their own (a
/// variable drawn in its own style, e.g. `$read_only` in `read_only_style`).
fn with_default_style(runs: Vec<Run>, style: &Style) -> Vec<Run> {
    runs.into_iter().map(|r| if r.style.is_some() { r } else { Run { style: Some(style.clone()), ..r } }).collect()
}

/// A symbol-like value as runs: parsed as a template when it contains
/// `[...](style)` spans, otherwise literal text.
fn sym_runs(s: &str) -> Vec<Run> {
    if s.is_empty() {
        vec![]
    } else if template::looks_styled(s) {
        template::render_runs(s, &|_| None)
    } else {
        vec![Run::plain(s)]
    }
}

/// Directory options resolved for one module and tier.
struct DirOpts {
    length: usize,
    truncation_symbol: String,
    home_symbol: String,
    /// (key, replacement) with empty replacements removed.
    substitutions: Vec<(String, String)>,
    to_repo: bool,
    /// Starship's `fish_style_pwd_dir_length` (0 = off; off with substitutions).
    fish: usize,
    /// `repo_root_style` is set: split at a visible repository root even
    /// when the path is not contracted to it.
    split_any_root: bool,
}

impl DirOpts {
    fn from(m: &ModuleConfig, t: SymbolTier, g: &Glyphs) -> DirOpts {
        let r = |s: &Option<Sym>| s.as_ref().map(|s| s.resolve(t).to_string());
        DirOpts {
            length: m.truncation_length.unwrap_or(3) as usize,
            truncation_symbol: r(&m.truncation_symbol).unwrap_or_else(|| format!("{}/", g.ellipsis)),
            home_symbol: r(&m.home_symbol).unwrap_or_else(|| "~".into()),
            substitutions: m
                .substitutions
                .iter()
                .flatten()
                .map(|(k, v)| (k.clone(), v.resolve(t).to_string()))
                .filter(|(_, v)| !v.is_empty())
                .collect(),
            to_repo: m.truncate_to_repo.unwrap_or(true),
            fish: m.fish_style_pwd_dir_length.unwrap_or(0) as usize,
            split_any_root: m.repo_root_style.is_some(),
        }
    }
}

/// Fish-style abbreviation of one path component: at most `n` characters
/// (one more for a dot-name).
fn fish_word(w: &str, n: usize) -> String {
    let chars: Vec<&str> = w.graphemes(true).collect();
    if chars.len() <= n {
        w.to_string()
    } else if w.starts_with('.') {
        chars[..=n].concat()
    } else {
        chars[..n].concat()
    }
}

/// The cwd is not writable (Starship's `read_only`); a missing directory is
/// not read-only.
fn is_readonly_dir(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return false };
    // SAFETY: `c` is a valid NUL-terminated path; access(2) only reads it.
    if unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0 {
        return false;
    }
    matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EACCES) | Some(libc::EROFS) | Some(libc::EPERM))
}

#[derive(Clone, Copy, PartialEq)]
enum Anchor {
    Home,
    Root,
    Repo,
    Literal,
}

fn names(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c { Component::Normal(s) => Some(s.to_string_lossy().into_owned()), _ => None })
        .collect()
}

/// Directory text, Starship-style:
/// 1. contract: with `to_repo` inside a repository whose root is not $HOME,
///    start at the repo root's name; else replace $HOME by `home_symbol`;
/// 2. substitute: keys containing `/` replace the longest matching leading
///    path (e.g. `~/work/x`); other keys replace whole path components;
/// 3. truncate to the last `length` components (the home symbol counts; 0 =
///    off) behind `truncation_symbol`. A repo-relative path always gets the
///    symbol (as Starship does). Without `to_repo` the truncation never cuts
///    above the repository root.
#[cfg(test)]
fn directory_text(cwd: &Path, home: Option<&Path>, root: Option<&Path>, o: &DirOpts) -> String {
    directory_parts(cwd, home, root, o).full
}

/// The directory text, plus `(before_root_path, repo_root, path_below_root)`
/// when the path starts at a visible repository root (for `repo_root_format`).
struct DirText {
    full: String,
    repo: Option<(String, String, String)>,
}

fn directory_parts(cwd: &Path, home: Option<&Path>, root: Option<&Path>, o: &DirOpts) -> DirText {
    let home = home.filter(|h| *h != Path::new("/") && !h.as_os_str().is_empty());
    // The worktree root comes from git and is canonical; the shell's cwd may go
    // through a symlink (macOS /tmp -> /private/tmp). Compare on the canonical
    // cwd when the plain one does not sit under the root, like Starship does.
    let canon;
    let cwd = match root {
        Some(r) if !cwd.starts_with(r) => {
            canon = std::fs::canonicalize(cwd).ok();
            match &canon {
                Some(c) if c.starts_with(r) => c.as_path(),
                _ => cwd,
            }
        }
        _ => cwd,
    };
    let root = root.filter(|r| cwd.starts_with(r) && r.file_name().is_some());
    let (mut anchor, mut segs, mut repo_idx): (Anchor, Vec<String>, Option<usize>);
    match (root, home) {
        (Some(r), h) if o.to_repo && Some(r) != h => {
            anchor = Anchor::Repo;
            segs = vec![r.file_name().unwrap_or_default().to_string_lossy().into_owned()];
            segs.extend(names(cwd.strip_prefix(r).unwrap_or(Path::new(""))));
            repo_idx = Some(0);
        }
        (_, Some(h)) if cwd.starts_with(h) => {
            anchor = Anchor::Home;
            segs = vec![o.home_symbol.clone()];
            segs.extend(names(cwd.strip_prefix(h).unwrap_or(Path::new(""))));
            repo_idx = root.and_then(|r| r.strip_prefix(h).ok()).map(|rel| names(rel).len());
        }
        _ => {
            anchor = Anchor::Root;
            segs = names(cwd);
            repo_idx = root.map(|r| names(r).len().saturating_sub(1));
        }
    }
    if !o.substitutions.is_empty() {
        let lookup = |key: &str, prefix: bool| {
            o.substitutions.iter().find(|(k, _)| k == key && k.contains('/') == prefix).map(|(_, v)| v.clone())
        };
        for i in (1..=segs.len()).rev() {
            let body = segs[..i].join("/");
            let pre = if anchor == Anchor::Root { format!("/{body}") } else { body };
            if let Some(v) = lookup(&pre, true) {
                segs.splice(0..i, [v]);
                anchor = Anchor::Literal;
                repo_idx = repo_idx.map(|r| r.saturating_sub(i - 1));
                break;
            }
        }
        let skip = usize::from(anchor == Anchor::Literal);
        for s in segs.iter_mut().skip(skip) {
            if let Some(v) = lookup(s, false) {
                *s = v;
            }
        }
    }
    let n = segs.len();
    let mut start = if o.length > 0 && n > o.length { n - o.length } else { 0 };
    if !o.to_repo {
        if let Some(ri) = repo_idx {
            start = start.min(ri);
        }
    }
    // What stands for the cut-off part: the truncation symbol, or with
    // `fish_style_pwd_dir_length` that part abbreviated (Starship: `~/D/N/`).
    let truncated = start > 0 || anchor == Anchor::Repo;
    let prefix = if truncated && o.fish > 0 && o.substitutions.is_empty() {
        let (rooted, full): (bool, Vec<String>) = match home.filter(|h| cwd.starts_with(h)) {
            Some(h) => (false, std::iter::once(o.home_symbol.clone()).chain(names(cwd.strip_prefix(h).unwrap_or(Path::new("")))).collect()),
            None => (true, names(cwd)),
        };
        let cut = full.len().saturating_sub(n - start);
        let words: Vec<String> = full[..cut].iter().map(|w| fish_word(w, o.fish)).collect();
        let lead = if rooted { "/" } else { "" };
        if words.is_empty() { lead.to_string() } else { format!("{lead}{}/", words.join("/")) }
    } else if truncated {
        o.truncation_symbol.clone()
    } else {
        String::new()
    };
    let body = segs[start..].join("/");
    let full = match anchor {
        _ if truncated => format!("{prefix}{body}"),
        Anchor::Home | Anchor::Literal | Anchor::Repo => body,
        Anchor::Root => format!("/{body}"),
    };
    let below = |ri: usize| segs[ri + 1..].iter().map(|s| format!("/{s}")).collect::<String>();
    let repo = match repo_idx {
        // Contracted to the repository root (the path starts with it).
        _ if anchor == Anchor::Repo => (start == 0).then(|| (prefix.clone(), segs[0].clone(), below(0))),
        // `repo_root_style`: split at the root wherever it shows.
        Some(ri) if o.split_any_root && start <= ri && ri < n => {
            let lead = if start > 0 { prefix.clone() } else if anchor == Anchor::Root { "/".to_string() } else { String::new() };
            let mid: String = segs[start..ri].iter().map(|s| format!("{s}/")).collect();
            Some((format!("{lead}{mid}"), segs[ri].clone(), below(ri)))
        }
        _ => None,
    };
    DirText { full, repo }
}

/// The branch text (`detached@abc1234`, `main (unborn)`), with `trunc`
/// applied to the branch name itself.
fn branch_text_with(d: &GitData, trunc: &dyn Fn(&str) -> String) -> String {
    if d.detached {
        format!("detached@{}", d.head_short.as_deref().unwrap_or("?"))
    } else {
        let b = trunc(d.branch.as_deref().or(d.head_short.as_deref()).unwrap_or("?"));
        if d.unborn { format!("{b} (unborn)") } else { b }
    }
}

/// Starship's git_branch truncation: the first `len` characters plus the
/// first character of `symbol`, only when something was cut.
fn truncate_name(s: &str, len: Option<u32>, symbol: &str) -> String {
    match len {
        Some(n) if n > 0 && s.graphemes(true).count() > n as usize => {
            let mut out: String = s.graphemes(true).take(n as usize).collect();
            out.push_str(symbol.graphemes(true).next().unwrap_or_default());
            out
        }
        _ => s.to_string(),
    }
}

/// Tildy's compact summary: `±changes =conflicts ↑ahead ↓behind`.
fn summary_status(d: &GitData, g: &Glyphs) -> String {
    let mut parts = vec![];
    let changes = d.staged + d.unstaged + d.untracked;
    if changes > 0 { parts.push(format!("{}{changes}", g.pm)); }
    if d.conflicted > 0 { parts.push(format!("={}", d.conflicted)); }
    if d.ahead > 0 { parts.push(format!("{}{}", g.up, d.ahead)); }
    if d.behind > 0 { parts.push(format!("{}{}", g.down, d.behind)); }
    parts.join(" ")
}

/// Git status pieces as runs: each Starship status variable
/// (`$conflicted`, `$stashed`, ..., `$ahead_behind`) plus `$all_status`.
struct StatusParts {
    vars: Vec<(&'static str, Vec<Run>)>,
    all_status: Vec<Run>,
    ahead_behind: Vec<Run>,
}

/// Starship's `$all_status$ahead_behind`: conflicted, stashed, deleted,
/// renamed, modified, staged, untracked, then diverged | ahead | behind.
/// GitData counts deletions and renames inside staged/unstaged; Starship does
/// not, so deletions are attributed to the worktree first, then the index.
///
/// A symbol may contain `$count` (`${count}`), and the diverged symbol
/// `$ahead_count`/`$behind_count`, replaced by the numbers (then
/// `show_counts` does not append them again), and `[...](style)` spans.
fn status_parts(d: &GitData, ss: &StatusSymbols, counts: bool, t: SymbolTier) -> StatusParts {
    let sym = |o: &Option<Sym>, key: &str| -> String {
        o.as_ref().map_or_else(|| default_status_symbol(key, t).to_string(), |s| s.resolve(t).to_string())
    };
    let one = |s: String, n: u32| -> Vec<Run> {
        if n == 0 || s.is_empty() {
            return vec![];
        }
        let has = s.contains("$count") || s.contains("${count}");
        let src = s.replace("${count}", &n.to_string()).replace("$count", &n.to_string());
        let mut runs = sym_runs(&src);
        if counts && !has {
            runs.push(Run::plain(n.to_string()));
        }
        runs
    };
    let wt_del = d.deleted.min(d.unstaged);
    let idx_del = (d.deleted - wt_del).min(d.staged.saturating_sub(d.renamed));
    let wt_type = d.typechanged.min(d.unstaged - wt_del);
    let idx_type = d.typechanged.saturating_sub(wt_type).min(d.staged.saturating_sub(d.renamed).saturating_sub(idx_del));
    let modified = d.unstaged - wt_del - wt_type;
    let staged = d.staged.saturating_sub(d.renamed).saturating_sub(idx_del).saturating_sub(idx_type);
    let vars: Vec<(&'static str, Vec<Run>)> = vec![
        ("conflicted", one(sym(&ss.conflicted, "conflicted"), d.conflicted)),
        ("stashed", one(sym(&ss.stashed, "stashed"), d.stashed)),
        ("deleted", one(sym(&ss.deleted, "deleted"), d.deleted)),
        ("renamed", one(sym(&ss.renamed, "renamed"), d.renamed)),
        ("typechanged", one(sym(&ss.typechanged, "typechanged"), d.typechanged)),
        ("modified", one(sym(&ss.modified, "modified"), modified)),
        ("staged", one(sym(&ss.staged, "staged"), staged)),
        ("untracked", one(sym(&ss.untracked, "untracked"), d.untracked)),
    ];
    let all_status: Vec<Run> = vars.iter().flat_map(|(_, r)| r.iter().cloned()).collect();
    let ahead_behind = if d.ahead > 0 && d.behind > 0 {
        let s = sym(&ss.diverged, "diverged");
        let has = s.contains("_count");
        let src = s
            .replace("${ahead_count}", &d.ahead.to_string())
            .replace("$ahead_count", &d.ahead.to_string())
            .replace("${behind_count}", &d.behind.to_string())
            .replace("$behind_count", &d.behind.to_string());
        let mut runs = sym_runs(&src);
        if counts && !has {
            runs.extend(sym_runs(&sym(&ss.ahead, "ahead")));
            runs.push(Run::plain(d.ahead.to_string()));
            runs.extend(sym_runs(&sym(&ss.behind, "behind")));
            runs.push(Run::plain(d.behind.to_string()));
        }
        runs
    } else if d.ahead > 0 {
        one(sym(&ss.ahead, "ahead"), d.ahead)
    } else if d.behind > 0 {
        one(sym(&ss.behind, "behind"), d.behind)
    } else if d.upstream.is_some() {
        sym_runs(&sym(&ss.up_to_date, "up_to_date"))
    } else {
        vec![]
    };
    StatusParts { vars, all_status, ahead_behind }
}

#[cfg(test)]
fn starship_status(d: &GitData, ss: &StatusSymbols, counts: bool, t: SymbolTier) -> String {
    let p = status_parts(d, ss, counts, t);
    template::text_of(&p.all_status) + &template::text_of(&p.ahead_behind)
}

/// Starship's humanized duration (`3s`, `1m5s`, `500ms`; with milliseconds `3s200ms`).
fn starship_duration(ms: u64, show_ms: bool) -> String {
    if ms == 0 {
        return "0ms".into();
    }
    let (millis, secs) = (ms % 1000, ms / 1000);
    let (s, mins) = (secs % 60, secs / 60);
    let (m, hours) = (mins % 60, mins / 60);
    let (h, days) = (hours % 24, hours / 24);
    let mut out = String::new();
    for (n, suf) in [(days, "d"), (h, "h"), (m, "m"), (s, "s")] {
        if n > 0 {
            out.push_str(&format!("{n}{suf}"));
        }
    }
    if show_ms || ms < 1000 {
        out.push_str(&format!("{millis}ms"));
    }
    out
}

/// Starship's `git_state`: `(REBASING 2/5)`.
fn git_state_text(m: &ModuleConfig, d: &GitData) -> Option<String> {
    if m.show_state == Some(false) { return None; }
    let op = d.operation.as_deref().filter(|o| !o.is_empty())?;
    let label = m
        .state_labels
        .as_ref()
        .and_then(|l| l.get(op).cloned())
        .or_else(|| GIT_OPERATIONS.iter().find(|(k, _)| *k == op).map(|(_, v)| v.to_string()))
        .unwrap_or_else(|| op.to_uppercase());
    let progress = d.operation_progress.map(|(c, t)| format!(" {c}/{t}")).unwrap_or_default();
    Some(format!("({label}{progress})"))
}

fn duration_value(ms: u64) -> Option<String> {
    if ms == 0 { return None; }
    Some(if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        let tenths = ms / 100;
        format!("{}.{}s", tenths / 10, tenths % 10)
    } else if ms < 3_600_000 {
        format!("{}m {}s", ms / 60_000, (ms / 1000) % 60)
    } else {
        format!("{}h {}m", ms / 3_600_000, (ms / 60_000) % 60)
    })
}

#[cfg(test)]
fn duration_text(ms: u64) -> Option<String> {
    duration_value(ms).map(|v| format!("took {v}"))
}

/// `$version` of a provider value: `fields.version` (else `text`), a leading
/// `v` dropped, then `version_format` (default `v${raw}`) applied.
fn version_of(m: &ModuleConfig, v: &ModuleValue) -> String {
    let raw = v.fields.get("version").unwrap_or(&v.text).trim();
    if raw.is_empty() { return String::new(); }
    let raw = raw.strip_prefix('v').filter(|r| r.starts_with(|c: char| c.is_ascii_digit())).unwrap_or(raw);
    let mut p = raw.splitn(3, '.');
    let (major, minor, patch) = (p.next().unwrap_or(""), p.next().unwrap_or(""), p.next().unwrap_or(""));
    m.version_format
        .as_deref()
        .unwrap_or("v${raw}")
        .replace("${raw}", raw)
        .replace("${major}", major)
        .replace("${minor}", minor)
        .replace("${patch}", patch)
}

/// The data of a provider-backed module, or why it is hidden.
fn slot_value(slot: Option<&Slot<ModuleValue>>) -> Result<&ModuleValue, &'static str> {
    let Some(s) = slot else { return Err("no_data") };
    match (&s.value, s.freshness) {
        (Some(v), _) => Ok(v),
        (None, Freshness::Fresh) => Err("not_detected"),
        (None, Freshness::Pending) => Err("pending"),
        (None, Freshness::Timeout) => Err("timeout"),
        (None, Freshness::Error) => Err("error"),
        (None, Freshness::Stale) => Err("no_data"),
    }
}

/// One visible module, ready for a row.
#[derive(Clone)]
struct Seg {
    /// Zsh prompt text without the module's own style.
    zsh: String,
    width: usize,
    style: Style,
    is_char: bool,
    /// Powerline rows: one space of padding on each side (never for
    /// pre-rendered Starship output, which carries its own padding).
    pad: bool,
    /// `fill` module: the symbol repeated to the terminal width (`zsh` holds
    /// `FILL_MARK` until the row is complete).
    fill: Option<String>,
    /// The visible text starts / ends with whitespace (Starship-style formats
    /// carry their own spacing; plain rows then add no join space).
    edge_ws: (bool, bool),
}

/// Placeholder for a fill segment inside a composed row. User text never
/// contains it (`sanitize` drops C0 controls).
const FILL_MARK: char = '\u{1}';

struct ModOut {
    report: ModuleReport,
    seg: Option<Seg>,
    pending: bool,
}

struct Ctx<'a> {
    data: &'a PromptData,
    fields: Fields,
    glyphs: Glyphs,
    tier: SymbolTier,
    now: OnceCell<Option<LocalTime>>,
    /// The time given to `render_at` (tests, previews); `utc_time_offset`
    /// reads it as UTC.
    injected_now: Option<LocalTime>,
    /// `std::env::consts::OS` (injected for tests).
    os: &'a str,
}

fn report(id: &str, ty: ModuleType, fresh: Option<Freshness>) -> ModuleReport {
    ModuleReport {
        id: id.into(),
        module_type: ty.as_str().into(),
        visible: false,
        hidden_reason: None,
        text: String::new(),
        columns: 0,
        additional_columns: None,
        rules_hit: vec![],
        freshness: fresh,
        source: None,
        age_ms: None,
        style: None,
        row: None,
        side: None,
    }
}

fn hidden(id: &str, ty: ModuleType, reason: &str, fresh: Option<Freshness>) -> ModOut {
    let mut r = report(id, ty, fresh);
    r.hidden_reason = Some(reason.into());
    ModOut { report: r, seg: None, pending: fresh == Some(Freshness::Pending) }
}

/// Module output before styling.
enum Content {
    /// Evaluated runs (escaped later) plus an optional styled tail (git state).
    Runs(Vec<Run>, Option<String>),
    /// Pre-rendered Zsh text and its width.
    Raw(String, usize),
}

fn render_module(ctx: &Ctx, id: &str, m: &ModuleConfig) -> ModOut {
    let (d, t, g) = (ctx.data, ctx.tier, &ctx.glyphs);
    let ty = m.module_type;
    let slot = ty.is_provider_backed().then(|| d.modules.get(id)).flatten();
    let fresh = match ty {
        ModuleType::Git | ModuleType::GitCommit => Some(d.git.freshness),
        _ if ty.is_provider_backed() => slot.map(|s| s.freshness),
        _ => None,
    };
    // agent_status: the highlighted record's fields are `agent.*` for show_when/rules.
    let agent_fields;
    let fields = match (ty, slot.and_then(|s| s.value.as_ref())) {
        (ModuleType::AgentStatus, Some(v)) => {
            agent_fields = agent::rule_fields(&ctx.fields, v);
            &agent_fields
        }
        _ => &ctx.fields,
    };
    if let Some(w) = &m.show_when {
        if !eval(w, fields) {
            return hidden(id, ty, &format!("show_when:{}", w.field), fresh);
        }
    }
    let req = &d.request;
    let mut symbol = m.symbol.as_ref().map_or_else(|| default_symbol(m, t).to_string(), |s| s.resolve(t).to_string());
    if ty == ModuleType::Character && req.last_exit != 0 {
        if let Some(e) = &m.error_symbol {
            symbol = e.resolve(t).to_string();
        }
    }
    if ty == ModuleType::Os {
        symbol = m
            .os_symbols
            .as_ref()
            .and_then(|o| o.get(ctx.os))
            .map(|s| s.resolve(t).to_string())
            .or_else(|| m.symbol.as_ref().map(|s| s.resolve(t).to_string()))
            .unwrap_or_else(|| os_default_symbol(ctx.os, t));
    }
    if !symbol.is_empty() { symbol.push_str(&" ".repeat(m.icon_padding.unwrap_or(0) as usize)); }
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    // Values that may carry their own styles (symbols, git status pieces).
    let mut meta: BTreeMap<String, Vec<Run>> = BTreeMap::new();
    let mut set = |k: &str, v: String| {
        vars.insert(k.to_string(), v);
    };
    let mut suffix = String::new();
    let mut tail = None;
    // A format chosen by the data (directory: the repository-root form).
    let mut fmt_owned: Option<String> = None;
    let content: Result<(), &str> = match ty {
        ModuleType::Changes => {
            if let Some(c) = &d.changes {
                // Symbols, not prose: `main → feature ✚3 venv → test` (ASCII:
                // `main -> feature +3 venv -> test`). ✚N is the number of
                // files that changed in the worktree while the prompt was idle.
                let arrow = tier3(t, "→", "→", "->");
                let mut parts = vec![];
                if let (Some(from), Some(to)) = (&c.branch_from, &c.branch_to) {
                    if from != to { parts.push(format!("{from} {arrow} {to}")); }
                }
                if c.files_added > 0 { parts.push(format!("{}{}", tier3(t, "✚", "✚", "+"), c.files_added)); }
                parts.extend(c.environments.iter().map(|e| e.replace(" → ", &format!(" {arrow} "))));
                if parts.is_empty() { Err("no_changes") } else { set("text", parts.join(" ")); Ok(()) }
            } else { Err("no_changes") }
        }
        ModuleType::Directory => {
            if req.cwd.as_os_str().is_empty() {
                Err("no_data")
            } else {
                let root = d.git.value.as_ref().filter(|g| g.in_repo).and_then(|g| g.worktree_root.as_deref());
                // `use_logical_path = false`: the physical path (symlinks resolved).
                let physical;
                let cwd = if m.use_logical_path == Some(false) {
                    physical = std::fs::canonicalize(&req.cwd).unwrap_or_else(|_| req.cwd.clone());
                    physical.as_path()
                } else {
                    req.cwd.as_path()
                };
                let dir = directory_parts(cwd, d.home.as_deref(), root, &DirOpts::from(m, t, g));
                let split_fmt = m.repo_root_format.clone().or_else(|| {
                    // Starship's default repo_root_format is its format with the
                    // path split in three; tildy derives it from `format`.
                    m.repo_root_style.as_ref().map(|_| {
                        m.format.as_deref().unwrap_or("$path").replace("${path}", "$path").replace("$path", "$before_root_path$repo_root$path")
                    })
                });
                match (split_fmt, dir.repo) {
                    (Some(f), Some((before, repo_root, below))) => {
                        let styled = |text: String, st: &Option<Style>| match st {
                            Some(st) if !text.is_empty() => vec![Run { text, style: Some(st.clone()) }],
                            _ => vec![Run::plain(text)],
                        };
                        meta.insert("before_root_path".into(), styled(before, &m.before_repo_root_style));
                        meta.insert("repo_root".into(), styled(repo_root, &m.repo_root_style));
                        set("path", below);
                        fmt_owned = Some(f);
                    }
                    _ => set("path", dir.full),
                }
                let fmt_text = fmt_owned.as_deref().or(m.format.as_deref()).unwrap_or("");
                if fmt_text.contains("read_only") && is_readonly_dir(cwd) {
                    let sym = m.read_only.as_ref().map_or_else(|| tier3(t, "🔒", "🔒", "RO").to_string(), |s| s.resolve(t).to_string());
                    // Without `read_only_style` the symbol takes the style of
                    // the span it sits in (imported formats carry Starship's).
                    let runs = match &m.read_only_style {
                        Some(st) => with_default_style(sym_runs(&sym), st),
                        None => sym_runs(&sym),
                    };
                    meta.insert("read_only".into(), runs);
                }
                Ok(())
            }
        }
        ModuleType::Git => {
            let marker = match d.git.freshness {
                Freshness::Pending => g.pending,
                Freshness::Timeout | Freshness::Error => "?",
                _ => "",
            };
            match d.git.value.as_ref().filter(|v| v.in_repo) {
                None if marker.is_empty() => Err("no_data"),
                None => {
                    // Nothing known yet: the marker alone stands in for the module.
                    set("text", marker.to_string());
                    Ok(())
                }
                Some(gd) => {
                    // Starship's `ignore_branches` / `only_attached` hide the branch part.
                    let ignored = (m.only_attached == Some(true) && gd.detached)
                        || (!gd.detached
                            && gd.branch.as_ref().is_some_and(|b| m.ignore_branches.iter().flatten().any(|i| i == b)));
                    let bsym = m.branch_symbol.as_ref().map(|s| s.resolve(t).to_string()).unwrap_or_else(|| symbol.clone());
                    let mut bsym = if ignored { String::new() } else { bsym };
                    if m.branch_symbol.is_some() && !bsym.is_empty() { bsym.push_str(&" ".repeat(m.icon_padding.unwrap_or(0) as usize)); }
                    meta.insert("branch_symbol".into(), sym_runs(&bsym));
                    meta.insert("symbol".into(), sym_runs(&bsym));
                    let tsym = m.truncation_symbol.as_ref().map_or_else(|| "…".to_string(), |s| s.resolve(t).to_string());
                    let trunc = |s: &str| truncate_name(s, m.truncation_length, &tsym);
                    set("branch", if ignored { String::new() } else { branch_text_with(gd, &trunc) });
                    // `$remote_name` / `$remote_branch` from the upstream
                    // (`origin/main`), shown when the remote branch differs
                    // from the local one or with `always_show_remote`.
                    if let Some((rn, rb)) = gd.upstream.as_deref().and_then(|u| u.split_once('/')) {
                        let local = gd.branch.as_deref().unwrap_or("");
                        if !ignored && !rb.is_empty() && (m.always_show_remote == Some(true) || rb != local) {
                            set("remote_name", trunc(rn));
                            set("remote_branch", trunc(rb));
                        }
                    }
                    match &m.status_symbols {
                        Some(ss) => {
                            let p = status_parts(gd, ss, m.show_counts == Some(true), t);
                            let mut status = p.all_status.clone();
                            status.extend(p.ahead_behind.iter().cloned());
                            for (k, r) in p.vars {
                                meta.insert(k.into(), r);
                            }
                            meta.insert("all_status".into(), p.all_status);
                            meta.insert("ahead_behind".into(), p.ahead_behind);
                            meta.insert("status".into(), status);
                        }
                        None => {
                            if gd.incomplete { suffix.push('~'); }
                            set("status", summary_status(gd, g));
                        }
                    }
                    suffix.push_str(marker);
                    tail = git_state_text(m, gd);
                    Ok(())
                }
            }
        }
        ModuleType::ExitStatus => {
            let code = req.last_exit;
            let success = m.success_symbol.as_ref().map(|s| s.resolve(t).to_string()).unwrap_or_default();
            if code == 0 && success.is_empty() {
                Err("empty")
            } else {
                let e = exit_meaning(code, m.recognize_signal_code != Some(false));
                for k in ["code", "status", "int"] {
                    set(k, code.to_string());
                }
                set("hex_status", format!("0x{code:X}"));
                set("common_meaning", e.common.unwrap_or_default().to_string());
                set("signal_number", e.signal.map(|s| s.to_string()).unwrap_or_default());
                set("signal_name", e.signal_name.unwrap_or_default());
                set("maybe_int", if e.common.is_none() && e.signal.is_none() { code.to_string() } else { String::new() });
                let mapped = |o: &Option<Sym>, n: &'static str, u: &'static str, a: &'static str| {
                    o.as_ref().map_or_else(|| tier3(t, n, u, a).to_string(), |s| s.resolve(t).to_string())
                };
                let map = m.map_symbol == Some(true);
                let recog = m.recognize_signal_code != Some(false);
                symbol = match code {
                    0 => success,
                    126 if map => mapped(&m.not_executable_symbol, "\u{f05e}", "🚫", "noexec"),
                    127 if map => mapped(&m.not_found_symbol, "\u{f002}", "🔍", "notfound"),
                    130 if map && recog => mapped(&m.sigint_symbol, "\u{f256}", "🧱", "^C"),
                    129..=255 if map && recog => mapped(&m.signal_symbol, "\u{f0e7}", "⚡", "SIG"),
                    _ => symbol,
                };
                Ok(())
            }
        }
        ModuleType::Duration => match duration_value(req.last_duration_ms) {
            Some(_) if req.last_duration_ms < m.min_time.unwrap_or(0) => Err("below_min_time"),
            Some(v) => {
                set("text", format!("took {v}"));
                set("duration", v);
                set("human", starship_duration(req.last_duration_ms, m.show_milliseconds == Some(true)));
                set("human_ms", starship_duration(req.last_duration_ms, true));
                Ok(())
            }
            None => Err("empty"),
        },
        ModuleType::Hostname => {
            let env = req.env_map();
            // The shell's $HOST is the full name; the session provider's is
            // already cut at the first dot.
            let full = env.get("HOST").filter(|h| !h.is_empty()).cloned().or_else(|| d.hostname.clone());
            match full {
                _ if crate::providers::native::env_vars_detected(m.detect_env_vars.as_deref().unwrap_or_default(), &env) == Some(false) => {
                    Err("detect_env_vars")
                }
                Some(full) => {
                    let trim_at = m.trim_at.as_deref().unwrap_or(".");
                    let mut h = match full.find(trim_at) {
                        Some(i) if !trim_at.is_empty() => full[..i].to_string(),
                        _ => full.clone(),
                    };
                    if let Some(a) = m.aliases.as_ref().and_then(|a| a.get(&h)) {
                        h = a.clone();
                    }
                    let u = d.username.clone().unwrap_or_default();
                    set("text", if u.is_empty() { h.clone() } else { format!("{u}@{h}") });
                    set("host", h.clone());
                    set("hostname", h);
                    set("user", u);
                    let ssh = env.get("SSH_CONNECTION").is_some_and(|v| !v.is_empty()) || d.ssh;
                    meta.insert("ssh_symbol".into(), if ssh { sym_runs(&symbol) } else { vec![] });
                    Ok(())
                }
                None => Err("no_data"),
            }
        }
        ModuleType::AgentStatus => slot_value(slot).map(|v| {
            for (k, val) in agent::vars(m, v, g.ellipsis) {
                set(&k, val);
            }
            symbol = agent::symbol(m, v, t);
        }),
        ModuleType::Command | ModuleType::Language | ModuleType::Python | ModuleType::DockerContext => {
            match slot_value(slot) {
                Err(r) => Err(r),
                Ok(v) => {
                    for (k, val) in &v.fields {
                        set(k, val.clone());
                    }
                    set("text", v.text.clone());
                    set("version", version_of(m, v));
                    if ty == ModuleType::DockerContext {
                        set("context", v.fields.get("context").unwrap_or(&v.text).clone());
                    }
                    Ok(())
                }
            }
        }
        ModuleType::Starship => match slot_value(slot) {
            Err(r) => Err(r),
            Ok(v) => match &v.prerendered {
                Some((s, _)) if !s.is_empty() => Ok(()),
                _ => Err("empty"),
            },
        },
        ModuleType::Character => Ok(()),
        ModuleType::Time => {
            let fmt = m.time_format.as_deref().unwrap_or(if m.use_12hr == Some(true) { "%r" } else { "%T" });
            match ctx.now.get_or_init(LocalTime::now) {
                Some(now) => {
                    let range = m.time_range.as_deref().and_then(crate::config::parse_time_range).unwrap_or((None, None));
                    let tod = now.seconds_of_day();
                    let inside = match range {
                        (None, None) => true,
                        (Some(a), None) => tod > a,
                        (None, Some(b)) => tod < b,
                        (Some(a), Some(b)) if a < b => a < tod && tod < b,
                        (Some(a), Some(b)) => tod > a || tod < b,
                    };
                    let offset = m.utc_time_offset.as_deref().and_then(crate::config::parse_utc_offset).flatten();
                    // An injected time (tests, previews) is read as UTC for offsets.
                    let shown = match offset {
                        Some(off) => ctx
                            .injected_now
                            .map(|n| n.as_utc_unix())
                            .or_else(LocalTime::unix_now)
                            .map(|u| LocalTime::utc_from_unix(u + off)),
                        None => Some(*now),
                    };
                    match shown {
                        _ if !inside => Err("outside_time_range"),
                        Some(s) => {
                            set("time", s.format(fmt));
                            Ok(())
                        }
                        None => Err("no_data"),
                    }
                }
                None => Err("no_data"),
            }
        }
        ModuleType::Username => {
            let mut user = d.username.clone().unwrap_or_default();
            let detected = env_detect(m.detect_env_vars.as_deref().unwrap_or_default(), &req.env_map());
            if detected == EnvDetect::Negated {
                Err("detect_env_vars")
            } else if m.show_always == Some(true) || d.ssh || user == "root" || detected == EnvDetect::Yes {
                if let Some(a) = m.aliases.as_ref().and_then(|a| a.get(&user)) {
                    user = a.clone();
                }
                set("user", user);
                Ok(())
            } else {
                Err("not_ssh_or_root")
            }
        }
        ModuleType::Jobs => {
            let (st, nt) = (m.symbol_threshold.unwrap_or(1), m.number_threshold.unwrap_or(1));
            if req.jobs == 0 && st > 0 && nt > 0 {
                Err("empty")
            } else {
                let n = if req.jobs >= nt { req.jobs.to_string() } else { String::new() };
                set("count", n.clone());
                set("number", n);
                if req.jobs < st {
                    meta.insert("symbol".into(), vec![]);
                }
                Ok(())
            }
        }
        ModuleType::Text => {
            let text = m.text.as_ref().map(|s| s.resolve(t).to_string()).unwrap_or_default();
            if text.is_empty() {
                Err("empty")
            } else {
                meta.insert("text".into(), sym_runs(&text));
                Ok(())
            }
        }
        ModuleType::Os => {
            set("name", ctx.os.to_string());
            Ok(())
        }
        ModuleType::Fill => Ok(()),
        ModuleType::EnvVar => {
            let name = m.variable.as_deref().unwrap_or("");
            match req.env_map().remove(name).filter(|v| !v.is_empty()) {
                Some(v) => {
                    set("env_value", v.clone());
                    set("text", v);
                    Ok(())
                }
                None => Err("empty"),
            }
        }
        ModuleType::Package => slot_value(slot).map(|v| {
            let raw = v.fields.get("version").unwrap_or(&v.text);
            let shown = if raw == crate::providers::package::SEMANTIC { raw.clone() } else { version_of(m, v) };
            set("text", v.text.clone());
            set("version", shown);
        }),
        ModuleType::GitCommit => match d.git.value.as_ref().filter(|g| g.in_repo) {
            None => Err("no_data"),
            Some(gd) if m.only_detached != Some(false) && !gd.detached => Err("not_detached"),
            Some(gd) => {
                let extra = slot.and_then(|s| s.value.as_ref());
                let len = m.commit_hash_length.unwrap_or(7) as usize;
                let full = extra.and_then(|v| v.fields.get("hash")).filter(|h| !h.is_empty()).or(gd.head_short.as_ref());
                match full {
                    None => Err("no_data"),
                    Some(h) => {
                        set("hash", h.chars().take(len).collect());
                        let tag = extra.and_then(|v| v.fields.get("tag")).filter(|_| m.tag_disabled == Some(false));
                        let runs = match tag {
                            Some(tag) => {
                                let ts = m.tag_symbol.as_ref().map_or_else(|| tier3(t, " \u{f412} ", " 🏷 ", " tag ").to_string(), |s| s.resolve(t).to_string());
                                let mut r = sym_runs(&ts);
                                r.push(Run::plain(tag.clone()));
                                r
                            }
                            None => vec![],
                        };
                        meta.insert("tag".into(), runs);
                        Ok(())
                    }
                }
            }
        },
        ModuleType::GitMetrics => slot_value(slot).map(|v| {
            let nonzero = m.only_nonzero_diffs != Some(false);
            let get = |k: &str| v.fields.get(k).cloned().filter(|n| !(nonzero && n == "0")).unwrap_or_default();
            let (a, del) = (get("added"), get("deleted"));
            let mut diff = vec![];
            if !a.is_empty() {
                let st = m.added_style.clone().unwrap_or(Style { foreground: Some("green".into()), bold: Some(true), ..Style::default() });
                diff.push(Run { text: format!("+{a}"), style: Some(st) });
            }
            if !del.is_empty() {
                if !diff.is_empty() {
                    diff.push(Run::plain(" "));
                }
                let st = m.deleted_style.clone().unwrap_or(Style { foreground: Some("red".into()), bold: Some(true), ..Style::default() });
                diff.push(Run { text: format!("-{del}"), style: Some(st) });
            }
            meta.insert("diff".into(), diff);
            set("added", a);
            set("deleted", del);
        }),
        ModuleType::Shlvl => {
            match req.env_map().get("SHLVL").and_then(|v| v.trim().parse::<i64>().ok()) {
                None => Err("not_detected"),
                Some(n) if n < m.threshold.unwrap_or(2) => Err("below_threshold"),
                Some(n) => {
                    let mut count = if m.repeat == Some(true) { usize::try_from(n).unwrap_or(1) } else { 1 };
                    count = count.saturating_sub(m.repeat_offset.unwrap_or(0) as usize);
                    if count == 0 {
                        Err("empty")
                    } else {
                        symbol = symbol.repeat(count);
                        set("shlvl", n.to_string());
                        Ok(())
                    }
                }
            }
        }
        ModuleType::Localip => slot_value(slot).map(|v| {
            let ip = v.fields.get("localipv4").unwrap_or(&v.text).clone();
            set("text", ip.clone());
            set("localipv4", ip);
        }),
        ModuleType::MemoryUsage => slot_value(slot).and_then(|v| {
            let pct: i64 = v.fields.get("pct").and_then(|p| p.parse().ok()).unwrap_or(0);
            if pct < m.threshold.unwrap_or(75) {
                return Err("below_threshold");
            }
            for (k, val) in &v.fields {
                set(k, val.clone());
            }
            set("text", v.text.clone());
            Ok(())
        }),
        ModuleType::Sudo => slot_value(slot).map(|_| ()),
        ModuleType::Direnv => slot_value(slot).map(|v| {
            let msg = |o: &Option<Sym>, default: &str| o.as_ref().map_or_else(|| default.to_string(), |s| s.resolve(t).to_string());
            let allowed = match v.fields.get("allowed").map(String::as_str) {
                Some("allowed") => msg(&m.allowed_msg, "allowed"),
                Some("denied") => msg(&m.denied_msg, "denied"),
                _ => msg(&m.not_allowed_msg, "not allowed"),
            };
            let loaded = if v.fields.get("loaded").map(String::as_str) == Some("true") {
                msg(&m.loaded_msg, "loaded")
            } else {
                msg(&m.unloaded_msg, "not loaded")
            };
            meta.insert("allowed".into(), sym_runs(&allowed));
            meta.insert("loaded".into(), sym_runs(&loaded));
            set("rc_path", v.fields.get("rc_path").cloned().unwrap_or_default());
        }),
        ModuleType::NixShell => {
            let env = req.env_map();
            let msg = |o: &Option<Sym>, default: &str| o.as_ref().map_or_else(|| default.to_string(), |s| s.resolve(t).to_string());
            let state = match env.get("IN_NIX_SHELL").map(String::as_str) {
                Some("pure") => Some(msg(&m.pure_msg, "pure")),
                Some("impure") => Some(msg(&m.impure_msg, "impure")),
                _ if m.heuristic == Some(true)
                    && env.get("PATH").is_some_and(|p| p.split(':').any(|e| e.starts_with("/nix/store"))) =>
                {
                    Some(msg(&m.unknown_msg, ""))
                }
                _ => None,
            };
            match state {
                None => Err("not_detected"),
                Some(s) => {
                    meta.insert("state".into(), sym_runs(&s));
                    set("name", env.get("name").cloned().unwrap_or_default());
                    Ok(())
                }
            }
        }
        ModuleType::Shell => {
            // The REQ comes from the Zsh adapter; another adapter would
            // forward its name as TILDY_SHELL.
            let env = req.env_map();
            let name = env.get("TILDY_SHELL").filter(|s| !s.is_empty()).map_or("zsh", String::as_str);
            let ind = m.indicators.as_ref();
            let pick = |n: &str| ind.and_then(|i| i.get(n)).map(|s| s.resolve(t).to_string());
            let indicator = pick(name)
                .or_else(|| (name == "pwsh").then(|| pick("powershell")).flatten())
                .or_else(|| default_shell_indicator(name).map(str::to_string))
                .unwrap_or_else(|| m.unknown_indicator.as_ref().map(|s| s.resolve(t).to_string()).unwrap_or_default());
            meta.insert("indicator".into(), sym_runs(&indicator));
            Ok(())
        }
    };
    if let Err(reason) = content {
        return hidden(id, ty, reason, fresh);
    }
    if ty != ModuleType::Git {
        meta.entry("symbol".into()).or_insert_with(|| sym_runs(&symbol));
    }

    let mut style = m.style.clone();
    if matches!(ty, ModuleType::Character | ModuleType::ExitStatus) {
        let s = if req.last_exit == 0 { &m.success_style } else { &m.error_style };
        if let Some(s) = s {
            style.overlay(s);
        }
    }
    if ty == ModuleType::AgentStatus {
        style = agent::style(m, vars.get("state").map(String::as_str));
    }
    let mut hits = vec![];
    for r in &m.rules {
        if eval(&r.when, fields) {
            style.overlay(&r.style);
            hits.push(r.id.clone());
        }
    }

    let pad = m.padding.unwrap_or(!matches!(ty, ModuleType::Text | ModuleType::Fill));
    if ty == ModuleType::Fill {
        let fill = template::text_of(meta.get("symbol").map(Vec::as_slice).unwrap_or_default());
        let mut r = report(id, ty, fresh);
        r.visible = true;
        return ModOut {
            report: r,
            seg: Some(Seg {
                zsh: FILL_MARK.to_string(),
                width: 0,
                style,
                is_char: false,
                pad: false,
                fill: Some(sanitize(&fill)),
                edge_ws: (false, false),
            }),
            pending: false,
        };
    }

    let content = match (ty, slot.and_then(|s| s.value.as_ref()).and_then(|v| v.prerendered.as_ref())) {
        (ModuleType::Starship, Some((s, w))) => Content::Raw(s.clone(), *w),
        _ => {
            let fmt = match (ty, m.format.as_deref()) {
                // A git module with nothing known shows only its marker.
                (ModuleType::Git, _) if !vars.contains_key("branch") => "$text",
                _ if fmt_owned.is_some() => fmt_owned.as_deref().unwrap_or_default(),
                (_, Some(f)) => f,
                (_, None) => default_format(m, &symbol),
            };
            let lookup = |k: &str| meta.get(k).cloned().or_else(|| vars.get(k).map(|v| vec![Run::plain(v.clone())]));
            let mut runs = template::render_runs(fmt, &lookup);
            if !suffix.is_empty() {
                runs.push(Run::plain(suffix));
            }
            Content::Runs(runs, tail)
        }
    };

    let (zsh, plain, width, raw) = match content {
        Content::Raw(s, w) => {
            let s = if crate::width::is_active() { calibrate_prompt_literals(&s) } else { s };
            let w = if crate::width::is_active() { visible_width(&s) } else { w };
            (s.clone(), strip_prompt_escapes(&s), w, true)
        },
        Content::Runs(runs, tail) => {
            let mut plain = sanitize(&template::text_of(&runs));
            // A text module is literal: whitespace-only text (a spacer) still shows.
            let empty = if ty == ModuleType::Text { plain.is_empty() } else { plain.trim().is_empty() };
            if empty {
                return hidden(id, ty, "empty", fresh);
            }
            let mut zsh = runs_to_zsh(&runs, &style);
            if let Some(tail) = tail {
                let mut st = Style { foreground: Some("yellow".into()), bold: Some(true), ..Style::default() };
                if let Some(s) = &m.state_style {
                    st = s.clone();
                }
                plain.push(' ');
                plain.push_str(&sanitize(&tail));
                zsh.push(' ');
                zsh.push_str(&wrap_style(&escape_prompt_text(&tail), &st));
            }
            let w = crate::width::width(plain.as_str());
            (zsh, plain, w, false)
        }
    };
    let edge_ws = (plain.starts_with(char::is_whitespace), plain.ends_with(char::is_whitespace));
    let mut r = report(id, ty, fresh);
    r.visible = true;
    r.text = plain;
    r.columns = width;
    r.rules_hit = hits;
    ModOut {
        report: r,
        seg: Some(Seg { zsh, width, style, is_char: ty == ModuleType::Character, pad: pad && !raw, fill: None, edge_ws }),
        pending: fresh == Some(Freshness::Pending),
    }
}

/// Modules joined by `sep` (default one space), each wrapped in its style. A
/// whitespace separator is left out next to a module whose text already ends
/// (or starts) with whitespace, so Starship-style formats that carry their
/// own trailing space are not double-spaced.
fn compose_plain(segs: &[Seg], sep: &str) -> (String, usize) {
    let sep_z = escape_prompt_text(sep);
    let sep_w = crate::width::width(sanitize(sep).as_str());
    let smart = sep.trim().is_empty();
    let mut text = String::new();
    let mut width = 0;
    for (i, s) in segs.iter().enumerate() {
        if i > 0 && !(smart && (segs[i - 1].edge_ws.1 || s.edge_ws.0)) {
            text.push_str(&sep_z);
            width += sep_w;
        }
        text.push_str(&wrap_style(&s.zsh, &s.style));
        width += s.width;
    }
    (text, width)
}

/// Powerline segments: each module on its background, padded with one space
/// per side unless `padding` is off; between two segments with different
/// backgrounds the separator in the previous background over the next one.
/// Neighbours with the same background get no separator (Starship's Pastel
/// Powerline just concatenates same-colored modules, e.g. its language group).
///
/// A run of background segments ends with the `end` cap (default: the
/// separator) in the last background over the default background, and starts
/// with the `start` cap (if set) in the first background. Segments without a
/// background sit outside the runs as plain text, with nothing between two
/// such segments. Without a `start` cap, a background segment that follows a
/// segment without one is introduced by the separator in the default color
/// (the pre-cap behaviour). `sub` (the row's `subseparator`, default empty)
/// goes between neighbours with the same background.
fn compose_powerline(segs: &[Seg], sep: &str, start: Option<&str>, end: &str, sub: &str) -> (String, usize) {
    let w = |s: &str| crate::width::width(sanitize(s).as_str());
    let (sep_z, end_z) = (escape_prompt_text(sep), escape_prompt_text(end));
    let start = start.filter(|s| !s.is_empty());
    let mut out = String::new();
    let mut width = 0;
    let mut prev_bg: Option<&str> = None;
    for (i, s) in segs.iter().enumerate() {
        let bg = s.style.background.as_deref();
        let has_bg = color(bg).is_some();
        let prev_has = i > 0 && color(prev_bg).is_some();
        let same = i > 0 && has_bg && color(bg) == color(prev_bg);
        if same {
            width += push_subseparator(&mut out, sub, &s.style);
        } else if has_bg && (i == 0 || !prev_has) && start.is_some() {
            let cap = start.unwrap_or_default();
            out.push_str(&format!("%k{}{}", fg_code(bg), escape_prompt_text(cap)));
            width += w(cap);
            out.push_str(&bg_code(bg));
        } else if i == 0 {
            out.push_str(&bg_code(bg));
        } else if prev_has && !has_bg {
            if end.is_empty() {
                out.push_str("%k");
            } else {
                out.push_str(&format!("{}%k{end_z}", fg_code(prev_bg)));
                width += w(end);
            }
        } else if !prev_has && !has_bg {
        } else if !sep.is_empty() {
            out.push_str(&fg_code(prev_bg));
            out.push_str(&bg_code(bg));
            out.push_str(&sep_z);
            width += w(sep);
        } else {
            out.push_str(&bg_code(bg));
        }
        out.push_str(&fg_code(s.style.foreground.as_deref()));
        let (open, close) = attrs(&s.style);
        let pad = if s.pad { " " } else { "" };
        out.push_str(&open);
        out.push_str(pad);
        out.push_str(&s.zsh);
        out.push_str(pad);
        out.push_str(&close);
        width += s.width + 2 * pad.len();
        prev_bg = bg;
    }
    match prev_bg.filter(|_| !end.is_empty() && color(prev_bg).is_some()) {
        Some(bg) => {
            out.push_str(&format!("%k{}{end_z}%f", fg_code(Some(bg))));
            width += w(end);
        }
        None if !segs.is_empty() => out.push_str("%k%f"),
        None => {}
    }
    (out, width)
}

/// Draw a subseparator between two segments on the same background, in the
/// next segment's foreground (spans in `sub` override it). Returns its width.
fn push_subseparator(out: &mut String, sub: &str, next: &Style) -> usize {
    if sub.is_empty() {
        return 0;
    }
    let base = Style { foreground: next.foreground.clone(), background: next.background.clone(), ..Style::default() };
    let runs = sym_runs(sub);
    out.push_str(&fg_code(base.foreground.as_deref()));
    out.push_str(&runs_to_zsh(&runs, &base));
    crate::width::width(sanitize(&template::text_of(&runs)).as_str())
}

/// Powerline segments of an upper row's right side, drawn right to left like
/// Powerlevel10k's right prompt: a run of background segments starts with
/// `head` in its first background over the default one; between different
/// backgrounds `sep` in the next background over the previous one; between
/// equal backgrounds `sub`; after the run `tail` (if any) in the last
/// background over the default. Segments without a background sit between
/// runs as plain text.
fn compose_powerline_right(segs: &[Seg], sep: &str, head: &str, tail: &str, sub: &str) -> (String, usize) {
    let w = |s: &str| crate::width::width(sanitize(s).as_str());
    let mut out = String::new();
    let mut width = 0;
    let mut prev_bg: Option<&str> = None;
    let close_run = |out: &mut String, width: &mut usize, bg: Option<&str>| {
        if tail.is_empty() {
            out.push_str("%k");
        } else {
            out.push_str(&format!("%k{}{}", fg_code(bg), escape_prompt_text(tail)));
            *width += w(tail);
        }
    };
    for (i, s) in segs.iter().enumerate() {
        let bg = s.style.background.as_deref();
        let has_bg = color(bg).is_some();
        let prev_has = i > 0 && color(prev_bg).is_some();
        if has_bg && prev_has && color(bg) == color(prev_bg) {
            width += push_subseparator(&mut out, sub, &s.style);
        } else if has_bg && prev_has {
            out.push_str(&format!("{}{}{}", fg_code(bg), escape_prompt_text(sep), bg_code(bg)));
            width += w(sep);
        } else if has_bg {
            out.push_str(&format!("{}{}{}", fg_code(bg), escape_prompt_text(head), bg_code(bg)));
            width += w(head);
        } else if prev_has {
            close_run(&mut out, &mut width, prev_bg);
        }
        out.push_str(&fg_code(s.style.foreground.as_deref()));
        let (open, close) = attrs(&s.style);
        let pad = if s.pad && has_bg { " " } else { "" };
        out.push_str(&open);
        out.push_str(pad);
        out.push_str(&s.zsh);
        out.push_str(pad);
        out.push_str(&close);
        width += s.width + 2 * pad.len();
        prev_bg = bg;
    }
    if color(prev_bg).is_some() {
        close_run(&mut out, &mut width, prev_bg);
        out.push_str("%f");
    } else if !segs.is_empty() {
        out.push_str("%k%f");
    }
    (out, width)
}

/// The right-side counterpart of a powerline glyph string: Powerline
/// triangles, half circles and slants point the other way; plain text is also
/// reversed (`░▒▓` -> `▓▒░`, `>` -> `<`). Strings with `[...](style)` spans
/// keep their order and only the Powerline glyphs flip.
fn mirror(s: &str) -> String {
    fn glyph(c: char) -> Option<char> {
        Some(match c {
            '\u{e0b0}' => '\u{e0b2}',
            '\u{e0b2}' => '\u{e0b0}',
            '\u{e0b1}' => '\u{e0b3}',
            '\u{e0b3}' => '\u{e0b1}',
            '\u{e0b4}' => '\u{e0b6}',
            '\u{e0b6}' => '\u{e0b4}',
            '\u{e0b5}' => '\u{e0b7}',
            '\u{e0b7}' => '\u{e0b5}',
            '\u{e0b8}' => '\u{e0be}',
            '\u{e0be}' => '\u{e0b8}',
            '\u{e0bc}' => '\u{e0ba}',
            '\u{e0ba}' => '\u{e0bc}',
            '\u{e0b9}' => '\u{e0bf}',
            '\u{e0bf}' => '\u{e0b9}',
            '\u{e0bd}' => '\u{e0bb}',
            '\u{e0bb}' => '\u{e0bd}',
            _ => return None,
        })
    }
    if template::looks_styled(s) {
        return s.chars().map(|c| glyph(c).unwrap_or(c)).collect();
    }
    s.chars()
        .rev()
        .map(|c| match c {
            '>' => '<',
            '<' => '>',
            '(' => ')',
            ')' => '(',
            '[' => ']',
            ']' => '[',
            c => glyph(c).unwrap_or(c),
        })
        .collect()
}

/// Compose the right side of an upper row (`right` of a row that is not the
/// last; Zsh's RPROMPT only covers the last line) in the row's style, with
/// the powerline glyphs mirrored: head = mirrored `end`, separator =
/// mirrored `separator`, tail = mirrored `start`. Fills on this side collapse.
fn compose_upper_right(segs: &[Seg], row: &Row, t: SymbolTier, plain_sep: &str) -> (String, usize) {
    let (mut text, mut width) = match row.style {
        RowStyle::Plain => compose_plain(segs, plain_sep),
        RowStyle::Powerline => {
            let sep = row_separator(row, t);
            let end = row.end.as_ref().map_or_else(|| sep.clone(), |s| s.resolve(t).to_string());
            let tail = row.start.as_ref().map(|s| mirror(s.resolve(t))).unwrap_or_default();
            let sub = row.subseparator.as_ref().map(|s| mirror(s.resolve(t))).unwrap_or_default();
            compose_powerline_right(segs, &mirror(&sep), &mirror(&end), &tail, &sub)
        }
    };
    let fills: Vec<&str> = segs.iter().filter_map(|s| s.fill.as_deref()).collect();
    expand_fills(&mut text, &mut width, &fills, 0);
    (text, width)
}

fn row_separator(row: &Row, t: SymbolTier) -> String {
    match &row.separator {
        Some(s) => s.resolve(t).to_string(),
        None => tier3(t, "\u{e0b0}", ">", ">").to_string(),
    }
}

/// Replace the fill marks of a composed line so the line is `cols` wide.
/// Several fills share the space (the first ones get the remainder).
fn expand_fills(text: &mut String, width: &mut usize, fills: &[&str], cols: usize) {
    if fills.is_empty() {
        return;
    }
    let room = cols.saturating_sub(*width);
    let n = fills.len();
    for (i, sym) in fills.iter().enumerate() {
        let target = room / n + usize::from(i < room % n);
        let mut s = String::new();
        let mut w = 0;
        let clusters: Vec<&str> = if sym.is_empty() { vec![" "] } else { sym.graphemes(true).collect() };
        let mut k = 0;
        let mut zero_width = 0;
        while w < target {
            let cluster = clusters[k % clusters.len()];
            let cw = crate::width::width(cluster);
            if cw == 0 { zero_width += 1; } else { zero_width = 0; }
            if zero_width >= clusters.len() { s.push_str(&" ".repeat(target - w)); w = target; break; }
            if w + cw > target {
                s.push_str(&" ".repeat(target - w)); w = target; break;
            }
            s.push_str(cluster); w += cw; k += 1;
        }
        *width += w;
        if let Some(pos) = text.find(FILL_MARK) {
            text.replace_range(pos..pos + FILL_MARK.len_utf8(), &escape_prompt_text(&s));
        }
    }
}

/// A visible module of a row, with what shrinking to the terminal width needs.
struct Item {
    seg: Seg,
    /// Index of the module's report.
    report: usize,
    priority: i64,
    /// Never dropped: the prompt character, and fills (they have no width of
    /// their own, they take what is left).
    pinned: bool,
    /// On the right side (RPROMPT of the last row).
    right: bool,
}

/// A composed row before fill expansion.
struct RowText {
    text: String,
    width: usize,
    /// RPROMPT text and width (last row).
    right: Option<(String, usize)>,
    /// The left-side segments composed into `text`.
    segs: Vec<Seg>,
}

impl RowText {
    /// Columns the row needs: the left side, plus the right side and the gap
    /// Zsh leaves before RPROMPT.
    fn total(&self) -> usize {
        self.width + self.right.as_ref().map_or(0, |(_, w)| w + 1)
    }
}

/// Compose a row from its visible modules: the left side in the row's style
/// (powerline separators and caps follow whatever is still visible) and, on
/// the last row, the right side as plain RPROMPT text. `last` carries whether
/// the row has a character module and the tier's prompt glyph: without a
/// character module the glyph is appended, and the last row ends with a space.
fn compose_row(items: &[Item], row: &Row, tier: SymbolTier, plain_sep: &str, last: Option<(bool, &str)>) -> RowText {
    let segs: Vec<Seg> = items.iter().filter(|i| !i.right).map(|i| i.seg.clone()).collect();
    let (mut text, mut width) = match row.style {
        RowStyle::Plain => compose_plain(&segs, plain_sep),
        RowStyle::Powerline => {
            let sep = row_separator(row, tier);
            let start = row.start.as_ref().map(|s| s.resolve(tier).to_string());
            let end = row.end.as_ref().map_or_else(|| sep.clone(), |s| s.resolve(tier).to_string());
            let sub = row.subseparator.as_ref().map(|s| s.resolve(tier).to_string()).unwrap_or_default();
            compose_powerline(&segs, &sep, start.as_deref(), &end, &sub)
        }
    };
    let mut right = None;
    if let Some((has_char, glyph)) = last {
        if !has_char {
            if segs.last().is_some_and(|s| !s.edge_ws.1) {
                text.push(' ');
                width += 1;
            }
            text.push_str(&escape_prompt_text(glyph));
            width += crate::width::width(glyph);
        }
        text.push(' ');
        width += 1;
        let r: Vec<Seg> = items.iter().filter(|i| i.right).map(|i| i.seg.clone()).collect();
        if !r.is_empty() {
            let sep = if row.style == RowStyle::Plain { plain_sep } else { " " };
            let (mut rtext, mut rwidth) = compose_plain(&r, sep);
            let fills: Vec<&str> = r.iter().filter_map(|s| s.fill.as_deref()).collect();
            // A fill on the right side has no defined width; it collapses.
            expand_fills(&mut rtext, &mut rwidth, &fills, 0);
            right = Some((rtext, rwidth));
        }
    }
    RowText { text, width, right, segs }
}

/// Drop the module that goes first when a row is too wide: the lowest
/// `priority`, the rightmost of equal priorities (the right side counts as
/// right of the left side). Pinned modules never go, and neither does the last
/// droppable one, so the highest-priority module always stays. Its report
/// becomes hidden with "no_room". Returns false when nothing may be dropped.
fn drop_one(items: &mut Vec<Item>, reports: &mut [ModuleReport], additional: usize) -> bool {
    let droppable = || items.iter().enumerate().filter(|(_, i)| !i.pinned);
    if droppable().count() < 2 && !droppable().any(|(_, i)| reports[i.report].module_type == "changes") {
        return false;
    }
    let Some((at, _)) = droppable().min_by_key(|(at, i)| (i.priority, std::cmp::Reverse(*at))) else { return false };
    let item = items.remove(at);
    hide_no_room(&mut reports[item.report]);
    reports[item.report].additional_columns = Some(additional);
    true
}

fn hide_no_room(r: &mut ModuleReport) {
    r.visible = false;
    r.hidden_reason = Some("no_room".into());

}

/// Render a prompt. Must be deterministic for the same inputs and must never
/// panic on odd data; untrusted text is escaped for Zsh prompt expansion.
pub fn render(config: &Config, data: &PromptData) -> RenderOutput {
    render_at(config, data, None)
}

/// `render_at` for terminal previews (`tildy preset show`): dim and italic are
/// emitted as raw SGR inside `%{...%}`. Never send this output to the shell.
pub fn render_preview(config: &Config, data: &PromptData, now: Option<LocalTime>) -> RenderOutput {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            RAW_SGR.with(|r| r.set(false));
        }
    }
    RAW_SGR.with(|r| r.set(true));
    let _reset = Reset;
    render_at(config, data, now)
}

/// `render` with an injected wall-clock time for the `time` module (None =
/// the current local time, read once per render when a time module shows).
///
/// Layout: each row is `plain` (visible modules joined by the row's
/// `separator`, default one space) or `powerline` (see `compose_powerline`);
/// hidden modules leave nothing. A non-last row with nothing visible (fills
/// do not count) produces no line. A `fill` module expands so its line is
/// `request.cols` wide. The last row ends with the prompt character and a
/// space: its `character` module if it has one, otherwise the tier's prompt
/// glyph. Zsh's RPROMPT sits on the last line only, so the last row's `right`
/// is emitted as `prompt.right` (always plain). The `right` of an earlier row
/// is drawn right-aligned on that row's own line, ending one column short of
/// the edge like RPROMPT, in the row's style (powerline glyphs mirrored, see
/// `compose_upper_right`); when the line has no room for it (one column of
/// gap) it is dropped and its modules are reported hidden with "no_room".
///
/// Every row is shrunk to `request.cols`: while it is wider (an upper row's
/// left side wider than `cols`; the last row's left side plus, when it has
/// one, its right side and the one-column gap Zsh leaves before RPROMPT,
/// wider than `cols - 1`, the cursor's column), modules are dropped by
/// ascending `priority`, the rightmost of equal priorities first, and
/// reported hidden with "no_room"; the row is composed again after each drop,
/// so powerline separators and caps follow the remaining segments. The
/// character module and fills are never dropped, and the highest-priority
/// module always stays (a row that is still too wide then wraps). Nothing is
/// truncated here beyond the directory module's own rules. `cols` 0 (width
/// unknown) shrinks nothing.
///
/// `transient` (compact history) is the rendered character module (or
/// glyph) plus a space.
pub fn render_at(config: &Config, data: &PromptData, now: Option<LocalTime>) -> RenderOutput {
    render_with(config, data, now, std::env::consts::OS)
}

/// `render_at` with an injected operating system name for the `os` module.
pub fn render_with(config: &Config, data: &PromptData, now: Option<LocalTime>, os: &str) -> RenderOutput {
    render_with_policy(config, data, now, os, crate::width::policy_for_request_env(&data.request.env))
}

pub fn render_with_policy(config: &Config, data: &PromptData, now: Option<LocalTime>, os: &str, policy: crate::width::WidthPolicy) -> RenderOutput {
    crate::width::with_policy(policy, || render_current(config, data, now, os))
}

fn render_current(config: &Config, data: &PromptData, now: Option<LocalTime>, os: &str) -> RenderOutput {
    let tier = config.symbols;
    let ctx = Ctx {
        data,
        fields: build_fields(data),
        glyphs: Glyphs::for_tier(tier),
        tier,
        now: now.map(|n| OnceCell::from(Some(n))).unwrap_or_default(),
        injected_now: now,
        os,
    };
    let rows = &config.file.layout.rows;
    let mut reports = vec![];
    let mut left_lines = vec![];
    let mut columns = vec![];
    let mut right = None;
    let mut partial = false;
    let mut transient_char: Option<String> = None;
    let cols = data.request.cols as usize;
    for (ri, row) in rows.iter().enumerate() {
        let last = ri + 1 == rows.len();
        let mut run = |id: &String, right: bool, reports: &mut Vec<ModuleReport>| -> Option<Item> {
            let m = config.file.modules.get(id)?;
            let out = render_module(&ctx, id, m);
            partial |= out.pending;
            reports.push(out.report);
            let seg = out.seg?;
            let pinned = seg.is_char || seg.fill.is_some();
            Some(Item { seg, report: reports.len() - 1, priority: m.priority, pinned, right })
        };
        let mut items: Vec<Item> = row.left.iter().filter_map(|id| run(id, false, &mut reports)).collect();
        if last {
            items.extend(row.right.iter().filter_map(|id| run(id, true, &mut reports)));
        }
        let plain_sep = row.separator.as_ref().map_or_else(|| " ".to_string(), |s| s.resolve(tier).to_string());
        // Right side of an upper row: right-aligned on its own line.
        let mut upper_right = None;
        if !last && !row.right.is_empty() {
            let first = reports.len();
            let r: Vec<Seg> = row.right.iter().filter_map(|id| run(id, true, &mut reports)).map(|i| i.seg).collect();
            if !r.is_empty() {
                let (text, width) = compose_upper_right(&r, row, tier, &plain_sep);
                upper_right = Some((text, width, first));
            }
        }
        let has_char = row.left.iter().any(|id| config.file.modules.get(id).is_some_and(|m| m.module_type == ModuleType::Character));
        let compose = |items: &[Item]| compose_row(items, row, tier, &plain_sep, last.then_some((has_char, ctx.glyphs.prompt)));
        // Shrink to the terminal width. Upper rows may fill the line; the last
        // row keeps one column for the cursor, and its right side (RPROMPT)
        // needs one more for the gap Zsh leaves before it.
        // `cols` 0 means the width is unknown: nothing is dropped.
        let budget = if last { cols.saturating_sub(1) } else { cols };
        let mut composed = compose(&items);

        while cols > 0 && composed.total() > budget && drop_one(&mut items, &mut reports, composed.total().saturating_sub(budget)) {
            composed = compose(&items);
        }

        let RowText { mut text, mut width, right: rprompt, segs } = composed;
        if last {
            right = rprompt.map(|(t, _)| t);
            if let Some(c) = segs.iter().find(|s| s.is_char) {
                transient_char = Some(wrap_style(&c.zsh, &c.style));
            }
        } else if segs.iter().all(|s| s.fill.is_some()) && upper_right.is_none() {
            continue;
        }
        let fills: Vec<&str> = segs.iter().filter_map(|s| s.fill.as_deref()).collect();
        match upper_right {
            Some((rtext, rwidth, first)) => {
                // End where Zsh ends RPROMPT (ZLE_RPROMPT_INDENT=1): one column
                // short of the edge. Without room for a one-column gap the
                // right side is dropped, like Powerlevel10k's.
                let target = cols.saturating_sub(1);
                if width + 1 + rwidth <= target {
                    expand_fills(&mut text, &mut width, &fills, target - rwidth);
                    text.push_str(&" ".repeat(target - rwidth - width));
                    text.push_str(&rtext);
                    width = target;
                } else {
                    for r in reports[first..].iter_mut().filter(|r| r.visible) {
                        hide_no_room(r);
                        r.additional_columns = Some((width + 1 + rwidth).saturating_sub(target));
                    }
                    expand_fills(&mut text, &mut width, &fills, cols);
                }
            }
            None => expand_fills(&mut text, &mut width, &fills, cols),
        }
        left_lines.push(text);
        columns.push(width);
    }
    if left_lines.is_empty() {
        left_lines.push(format!("{} ", escape_prompt_text(ctx.glyphs.prompt)));
        columns.push(crate::width::width(ctx.glyphs.prompt) + 1);
    }
    // Starship's `add_newline`: a blank line between prompts, as an empty
    // first line (the shell joins L lines with newlines). The transient form
    // keeps that blank line (p10k's behaviour: command blocks stay separated
    // in the history) unless `history.add_newline` is false; it is sent as
    // an empty first T line.
    if config.file.add_newline == Some(true) {
        left_lines.insert(0, String::new());
        columns.insert(0, 0);
    }
    let transient = config.history_compact().then(|| {
        let nl = if config.transient_newline() { "\n" } else { "" };
        format!("{nl}{} ", transient_char.unwrap_or_else(|| escape_prompt_text(ctx.glyphs.prompt)))
    });
    let history = config.history_compact().then(|| history_metadata(config, data, &left_lines, transient.as_deref().unwrap_or_default()));
    RenderOutput {
        prompt: RenderedPrompt {
            status: if partial { RenderStatus::Partial } else { RenderStatus::Fresh },
            left: left_lines,
            right,
            transient,
            history,
            observation_seq: None,
        },
        reports,
        columns,
    }
}

/// Fit untrusted plain text by complete grapheme boundaries. Width is
/// computed on each full prefix so combining marks and emoji sequences agree
/// with the same unicode-width policy as normal prompt composition.
fn fit_columns(text: &str, max: usize) -> String {
    let safe = sanitize(text);
    let mut end = 0;
    for (offset, cluster) in safe.grapheme_indices(true) {
        let next = offset + cluster.len();
        if crate::width::width(&safe[..next]) > max { break; }
        end = next;
    }
    safe[..end].to_string()
}

/// Context identity is independent of visible modules and excludes ordinary cd.
fn history_metadata(config: &Config, data: &PromptData, left: &[String], transient: &str) -> HistoryMetadata {
    let git = data.git.value.as_ref().filter(|g| g.in_repo);
    let repo = git.and_then(|g| g.worktree_root.as_ref()).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let branch = git.map(|g| if g.detached { g.head_short.clone().unwrap_or_default() } else { g.branch.clone().unwrap_or_default() }).unwrap_or_default();
    let host = if data.ssh { data.hostname.clone().unwrap_or_default() } else { String::new() };
    // Hex JSON has unambiguous field boundaries and is safe on the wire.
    let bytes = serde_json::to_vec(&(&repo, &branch, &host)).unwrap_or_default();
    // While git is still pending the tuple is a placeholder: no key, so the
    // shell keeps its previous one and the next command (same branch) is not
    // separated from this one just because this one was submitted early.
    let key = if data.git.freshness == Freshness::Pending && data.git.value.is_none() {
        String::new()
    } else if bytes.len() * 2 <= crate::protocol::MAX_LINE_BYTES {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut key = String::with_capacity(bytes.len() * 2);
        for byte in &bytes {
            key.push(HEX[(byte >> 4) as usize] as char);
            key.push(HEX[(byte & 15) as usize] as char);
        }
        key
    } else {
        // A very long path must not turn distinct suffixes into an identical
        // truncated wire key. DefaultHasher is deterministic for this build;
        // sessions do not survive a binary-version handshake change.
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hash);
        format!("{:016x}", hash.finish())
    };
    let label = [branch, host].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
    let label = if label.is_empty() && !repo.is_empty() { std::path::Path::new(&repo).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default() } else { label };
    let sep = config.file.history.context_separator != Some(false) && !label.is_empty();
    let (context_transient, context_suffix) = if sep && left.len() > 1 {
        let mut rows: Vec<String> = transient.split('\n').map(String::from).collect();
        let at = rows.len().saturating_sub(1);
        let divider = escape_prompt_text(&fit_columns(&format!("── tildy · {label} ──"), (data.request.cols as usize).saturating_sub(1)));
        rows.insert(at, divider);
        // Reuse only rows already available in the full prompt.
        while rows.len() > left.len() { rows.remove(0); }
        (Some(rows.join("\n")), None)
    } else if sep { (None, Some(format!("  · {}", escape_prompt_text(&label)))) } else { (None, None) };
    let threshold = config.file.modules.iter().find(|(id, m)| id.as_str() == "elapsed" && m.module_type == ModuleType::Duration)
        .or_else(|| config.file.modules.iter().find(|(_, m)| m.module_type == ModuleType::Duration))
        .map(|(_, m)| m.min_time.or_else(|| m.show_when.as_ref().filter(|c| c.field == "command.duration_ms").and_then(|c| c.value.as_u64())).unwrap_or(3000)).unwrap_or(3000);
    HistoryMetadata { context_key: key, context_transient, context_suffix, summary: config.file.history.summary != Some(false), summary_threshold_ms: threshold }
}

#[cfg(test)]
mod tests;
