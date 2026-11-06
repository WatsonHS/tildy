//! `tildy theme`: minimal interactive picker (preset list with live preview
//! plus a few toggles) that writes `{"preset": ..., ...overrides}` to the user
//! config. Not an editor. Owner: M1b-configure task. See docs/theme.md.
//!
//! Every toggle is computed from the chosen preset's own layout: modules are
//! classified by type (character, time, duration/jobs, language versions), so
//! no module id is hardcoded and a preset without such a module shows "n/a".
//! The written file holds `schema_version`, `preset`, and only the fields that
//! differ from the preset: `palette` (role-based presets, cycled with ←/→ while
//! the list has focus), `symbols` and, when any toggle changed the rows, the
//! whole `layout` (arrays replace on merge). An explicit `add_newline` choice
//! is kept across preset changes.
//!
//! The interactive loop (`run_loop`) reads keys from any `Read` and draws to
//! any `Write`, so tests drive it with an injected byte stream; the real
//! terminal uses `tui::TtyIn` (poll with a timeout, giving ticks that let a
//! late `--here` data fetch refresh the preview).
use super::i18n::{description, text, tr, tr_eprintln};
use super::preview::{compose_lines, here_data, nerd_label, parse_tier, sample_data, sample_now, zsh_to_ansi};
use super::tui::{self, Event, Key, KeyReader, RawMode, TtyIn};
use clap::Args;
use serde_json::{json, Value};
use tildy::config::{self, Config, ConfigError, ModuleConfig, ModuleType, PresetInfo, SymbolTier, PALETTE_INFO, PRESET_INFO};
use tildy::config::transaction::{self as tx, Store};
use tildy::model::{ErrorCode, PromptData};
use tildy::render::{self, LocalTime};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Args)]
pub struct ConfigureArgs {
    /// Write the result to this path instead of the default config path.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Non-interactive: apply this preset and exit (used by tests and scripts).
    #[arg(long)]
    pub preset: Option<String>,
    /// Icon tier nerd|unicode|ascii (written as `symbols` only when it differs from the preset's).
    #[arg(long)]
    pub symbols: Option<String>,
    /// Palette for a role-based preset (written as `palette` only when it differs from the preset's default).
    #[arg(long)]
    pub palette: Option<String>,
    /// Single-line prompt: move the prompt character to the end of the first row.
    #[arg(long, conflicts_with = "two_line")]
    pub one_line: bool,
    /// Two-line prompt: move the prompt character to a row of its own.
    #[arg(long)]
    pub two_line: bool,
    /// Add a blank line before each prompt.
    #[arg(long, conflicts_with = "no_add_newline")]
    pub add_newline: bool,
    /// Remove the blank line before each prompt.
    #[arg(long)]
    pub no_add_newline: bool,
    /// Remove time modules from the layout.
    #[arg(long)]
    pub no_time: bool,
    /// Remove language version modules from the layout.
    #[arg(long)]
    pub no_langs: bool,
    /// Remove command duration and background job modules from the layout.
    #[arg(long)]
    pub no_duration: bool,
}

// ---------------------------------------------------------------------------
// Toggles, computed from a preset's layout.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rows {
    One,
    Two,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Icons,
    Rows,
    Newline,
    Time,
    Langs,
    DurationJobs,
}

pub const TOGGLES: [Toggle; 6] = [Toggle::Icons, Toggle::Rows, Toggle::Newline, Toggle::Time, Toggle::Langs, Toggle::DurationJobs];

impl Toggle {
    fn label(self) -> &'static str {
        match self {
            Toggle::Icons => text("Icons", "图标"),
            Toggle::Rows => text("Rows", "行数"),
            Toggle::Newline => text("Blank line", "提示符前空行"),
            Toggle::Time => text("Time", "时间"),
            Toggle::Langs => text("Versions", "语言版本"),
            Toggle::DurationJobs => text("Duration/jobs", "耗时与任务"),
        }
    }

    /// English name for CLI messages.
    fn name(self) -> &'static str {
        match self {
            Toggle::Icons => "icon",
            Toggle::Rows => "rows",
            Toggle::Newline => "blank line",
            Toggle::Time => "time",
            Toggle::Langs => "language version",
            Toggle::DurationJobs => "duration or jobs",
        }
    }
}

/// What the user changed, relative to whichever preset is selected. Kept
/// across preset switches (an explicit icon tier or "time off" carries over).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefs {
    /// None: the preset's own tier.
    pub symbols: Option<SymbolTier>,
    /// None: the preset's default palette. Only role-based presets use it.
    pub palette: Option<&'static str>,
    /// None: the preset's own row count.
    pub rows: Option<Rows>,
    /// None: inherit the preset; Some: keep this choice across preset changes.
    pub add_newline: Option<bool>,
    pub time: bool,
    pub langs: bool,
    pub duration_jobs: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { symbols: None, palette: None, rows: None, add_newline: None, time: true, langs: true, duration_jobs: true }
    }
}

/// A toggle's value for the selected preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The preset has no module this toggle acts on.
    Na,
    /// Icon tier; `true` when it is the preset's own (nothing written).
    Tier(SymbolTier, bool),
    Rows(Rows),
    On(bool),
}

/// Starship modules (bridge `type: "starship"`) that show a language or
/// runtime version.
const STARSHIP_LANGUAGES: &[&str] = &[
    "bun", "c", "cmake", "cobol", "cpp", "crystal", "daml", "dart", "deno", "dotnet", "elixir", "elm", "erlang", "fennel",
    "gleam", "golang", "gradle", "haskell", "haxe", "java", "julia", "kotlin", "lua", "mojo", "nim", "nodejs", "ocaml", "odin",
    "perl", "php", "purescript", "python", "quarto", "raku", "red", "rlang", "ruby", "rust", "scala", "solidity", "swift",
    "typst", "vlang", "zig",
];

fn is_version_flag(a: &str) -> bool {
    matches!(a, "--version" | "-version" | "version" | "-v" | "-V" | "--numeric-version")
}

/// language/python/command modules whose format shows `$version` (the
/// defaults of language and python do), command modules showing `$text` of a
/// `--version`-style command, and Starship bridge modules of languages.
/// A python module showing only `$env` (pure-preset) or a language module
/// showing only its icon (no-runtime-versions) is not a version module.
pub fn is_version_module(m: &ModuleConfig) -> bool {
    let has = |f: &str, var: &str| f.contains(&format!("${var}")) || f.contains(&format!("${{{var}}}"));
    match m.module_type {
        ModuleType::Language | ModuleType::Python => m.format.as_deref().is_none_or(|f| has(f, "version")),
        ModuleType::Command => {
            let f = m.format.as_deref().unwrap_or("$text");
            has(f, "version") || (has(f, "text") && m.argv.as_ref().is_some_and(|a| a.iter().skip(1).any(|x| is_version_flag(x))))
        }
        ModuleType::Starship => m.module.as_deref().is_some_and(|n| STARSHIP_LANGUAGES.contains(&n)),
        _ => false,
    }
}

/// A preset analysed for the toggles.
pub struct Base {
    pub info: &'static PresetInfo,
    /// The preset's effective config (no user layer); used for data fetches.
    cfg: Config,
    /// Effective `layout.rows` as raw JSON (written back verbatim).
    rows: Vec<Value>,
    /// Tier the preset shows with no `symbols` override (pinned, or `auto`
    /// resolved in this terminal).
    default_tier: SymbolTier,
    /// Default palette of a role-based preset (None: fixed colors).
    default_palette: Option<String>,
    char_ids: Vec<String>,
    time_ids: Vec<String>,
    lang_ids: Vec<String>,
    dj_ids: Vec<String>,
}

fn ids(row: &Value, side: &str) -> Vec<String> {
    row.get(side).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

fn set_ids(row: &mut Value, side: &str, list: Vec<String>) {
    if let Some(o) = row.as_object_mut() {
        if list.is_empty() {
            o.remove(side);
        } else {
            o.insert(side.into(), json!(list));
        }
    }
}

impl Base {
    pub fn new(info: &'static PresetInfo) -> Result<Base, ConfigError> {
        let cfg = config::preset(info.name)?;
        let rows = cfg.json.pointer("/layout/rows").and_then(Value::as_array).cloned().unwrap_or_default();
        let (mut char_ids, mut time_ids, mut lang_ids, mut dj_ids) = (vec![], vec![], vec![], vec![]);
        for (id, m) in cfg.layout_modules() {
            let bucket = match m.module_type {
                ModuleType::Character => &mut char_ids,
                ModuleType::Time => &mut time_ids,
                ModuleType::Duration | ModuleType::Jobs => &mut dj_ids,
                _ if is_version_module(m) => &mut lang_ids,
                _ => continue,
            };
            bucket.push(id.to_string());
        }
        Ok(Base { info, default_tier: cfg.symbols, default_palette: info.palette(), cfg, rows, char_ids, time_ids, lang_ids, dj_ids })
    }

    /// Role-based presets take a palette (←/→ in the list).
    pub fn is_role(&self) -> bool {
        self.default_palette.is_some()
    }

    /// The palette the preset shows with `prefs` (None: fixed colors).
    pub fn palette<'a>(&'a self, prefs: &Prefs) -> Option<&'a str> {
        self.default_palette.as_deref().map(|d| prefs.palette.unwrap_or(d))
    }

    pub fn by_name(name: &str) -> Option<Result<Base, ConfigError>> {
        PRESET_INFO.iter().find(|p| p.name == name).map(Base::new)
    }

    /// The preset's own row count, or None when it has no character module
    /// (the rows toggle is n/a).
    pub fn rows_default(&self) -> Option<Rows> {
        if self.char_ids.is_empty() {
            None
        } else if self.rows.len() >= 2 {
            Some(Rows::Two)
        } else {
            Some(Rows::One)
        }
    }

    fn ids_of(&self, t: Toggle) -> &[String] {
        match t {
            Toggle::Time => &self.time_ids,
            Toggle::Langs => &self.lang_ids,
            Toggle::DurationJobs => &self.dj_ids,
            Toggle::Icons | Toggle::Rows | Toggle::Newline => &[],
        }
    }

    pub fn state(&self, prefs: &Prefs, t: Toggle) -> State {
        let onoff = |on: bool| if self.ids_of(t).is_empty() { State::Na } else { State::On(on) };
        match t {
            Toggle::Icons => State::Tier(prefs.symbols.unwrap_or(self.default_tier), prefs.symbols.is_none_or(|s| s == self.default_tier)),
            Toggle::Rows => self.rows_default().map_or(State::Na, |d| State::Rows(prefs.rows.unwrap_or(d))),
            Toggle::Newline => State::On(prefs.add_newline.unwrap_or(self.cfg.file.add_newline == Some(true))),
            Toggle::Time => onoff(prefs.time),
            Toggle::Langs => onoff(prefs.langs),
            Toggle::DurationJobs => onoff(prefs.duration_jobs),
        }
    }

    /// Two rows -> one: every row's left modules in order on the first row
    /// (keeping its style and separators), character modules last; the right
    /// sides become the single row's (RPROMPT): the last row's first (it leads
    /// with the agent status), then the upper rows' in order.
    fn collapse(&self, rows: &[Value]) -> Vec<Value> {
        let mut first = rows[0].clone();
        let mut left: Vec<String> = rows.iter().flat_map(|r| ids(r, "left")).filter(|id| !self.char_ids.contains(id)).collect();
        left.extend(self.char_ids.iter().cloned());
        set_ids(&mut first, "left", left);
        let (last, upper) = rows.split_last().expect("collapse needs rows");
        let mut right = ids(last, "right");
        right.extend(upper.iter().flat_map(|r| ids(r, "right")));
        set_ids(&mut first, "right", right);
        vec![first]
    }

    /// One row -> two: the character modules move to a new plain second row,
    /// which also takes the right side (RPROMPT is drawn on the last line).
    fn split(&self, rows: &[Value]) -> Vec<Value> {
        let mut first = rows[0].clone();
        let left = ids(&first, "left").into_iter().filter(|id| !self.char_ids.contains(id)).collect();
        set_ids(&mut first, "left", left);
        let mut second = json!({});
        set_ids(&mut second, "left", self.char_ids.clone());
        set_ids(&mut second, "right", ids(&first, "right"));
        set_ids(&mut first, "right", vec![]);
        let mut out = vec![first, second];
        out.extend(rows[1..].iter().cloned());
        out
    }

    /// The preset's rows with the toggles applied.
    pub fn transform_rows(&self, prefs: &Prefs) -> Vec<Value> {
        let mut rows = self.rows.clone();
        if rows.is_empty() {
            return rows;
        }
        if let Some(def) = self.rows_default() {
            match (def, prefs.rows.unwrap_or(def)) {
                (Rows::Two, Rows::One) => rows = self.collapse(&rows),
                (Rows::One, Rows::Two) => rows = self.split(&rows),
                _ => {}
            }
        }
        let mut drop: Vec<&String> = vec![];
        for (on, list) in [(prefs.time, &self.time_ids), (prefs.langs, &self.lang_ids), (prefs.duration_jobs, &self.dj_ids)] {
            if !on {
                drop.extend(list);
            }
        }
        if drop.is_empty() {
            return rows;
        }
        let mut out = vec![];
        for mut r in rows {
            let (l, rt) = (ids(&r, "left"), ids(&r, "right"));
            let (nl, nr): (Vec<String>, Vec<String>) =
                (l.iter().filter(|i| !drop.contains(i)).cloned().collect(), rt.iter().filter(|i| !drop.contains(i)).cloned().collect());
            let touched = nl.len() != l.len() || nr.len() != rt.len();
            if touched && nl.is_empty() && nr.is_empty() {
                continue; // a row emptied by the toggles disappears
            }
            if touched {
                set_ids(&mut r, "left", nl);
                set_ids(&mut r, "right", nr);
            }
            out.push(r);
        }
        if out.is_empty() {
            out.push(json!({ "left": [] }));
        }
        out
    }
}

impl Prefs {
    /// The row/time/langs/duration choices that turn `base`'s layout into
    /// `rows` (a layout written by an earlier `tildy theme`), keeping
    /// `self.symbols`. None when no combination produces it (hand-edited).
    pub fn infer(&self, base: &Base, rows: &[Value]) -> Option<Prefs> {
        for r in [None, Some(Rows::One), Some(Rows::Two)] {
            for bits in 0..8u8 {
                let p = Prefs {
                    symbols: self.symbols,
                    palette: self.palette,
                    rows: r,
                    add_newline: self.add_newline,
                    time: bits & 1 == 0,
                    langs: bits & 2 == 0,
                    duration_jobs: bits & 4 == 0,
                };
                if base.transform_rows(&p) == rows {
                    return Some(p);
                }
            }
        }
        None
    }

    /// Step to the previous (`-1`) or next (`1`) built-in palette for a
    /// role-based `base`. False (and nothing changes) for fixed colors.
    pub fn cycle_palette(&mut self, base: &Base, step: isize) -> bool {
        let Some(cur) = base.palette(self) else { return false };
        let n = PALETTE_INFO.len() as isize;
        let i = PALETTE_INFO.iter().position(|p| p.name == cur).map_or(0, |i| i as isize);
        self.palette = Some(PALETTE_INFO[(i + step).rem_euclid(n) as usize].name);
        true
    }

    /// Flip toggle `t` for `base`. False (and nothing changes) when n/a.
    pub fn toggle(&mut self, base: &Base, t: Toggle) -> bool {
        match base.state(self, t) {
            State::Na => false,
            State::Tier(cur, _) => {
                self.symbols = Some(match cur {
                    SymbolTier::Nerd => SymbolTier::Unicode,
                    SymbolTier::Unicode => SymbolTier::Ascii,
                    SymbolTier::Ascii => SymbolTier::Nerd,
                });
                true
            }
            State::Rows(cur) => {
                self.rows = Some(if cur == Rows::One { Rows::Two } else { Rows::One });
                true
            }
            State::On(on) => {
                match t {
                    Toggle::Newline => self.add_newline = Some(!on),
                    Toggle::Time => self.time = !on,
                    Toggle::Langs => self.langs = !on,
                    Toggle::DurationJobs => self.duration_jobs = !on,
                    Toggle::Icons | Toggle::Rows => unreachable!("non-boolean toggle"),
                }
                true
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The user config that gets written.

/// Minimal preset overrides, including an explicit blank-line preference.
#[derive(Debug, Clone, PartialEq)]
pub struct UserConfig {
    pub preset: &'static str,
    pub palette: Option<&'static str>,
    pub symbols: Option<SymbolTier>,
    pub rows: Option<Vec<Value>>,
    pub add_newline: Option<bool>,
    /// Other top-level keys of the file being replaced (`modules`,
    /// `history`, ...), written after the keys above, unchanged.
    pub extra: Vec<(String, Value)>,
}

impl UserConfig {
    /// Pretty JSON in a fixed key order; arrays of plain values on one line.
    pub fn to_text(&self) -> String {
        let mut entries: Vec<(&str, Value)> = vec![("schema_version", json!(tildy::model::SCHEMA_VERSION)), ("preset", json!(self.preset))];
        if let Some(p) = self.palette {
            entries.push(("palette", json!(p)));
        }
        if let Some(t) = self.symbols {
            entries.push(("symbols", json!(t.as_str())));
        }
        if let Some(on) = self.add_newline {
            entries.push(("add_newline", json!(on)));
        }
        if let Some(rows) = &self.rows {
            entries.push(("layout", json!({ "rows": rows })));
        }
        for (k, v) in &self.extra {
            entries.push((k.as_str(), v.clone()));
        }
        let mut s = String::from("{\n");
        for (i, (k, v)) in entries.iter().enumerate() {
            s.push_str(&format!("  {}: ", json!(k)));
            pretty(v, 2, &mut s);
            s.push_str(if i + 1 < entries.len() { ",\n" } else { "\n" });
        }
        s.push_str("}\n");
        s
    }

    /// One-line summary for the footer.
    fn summary(&self) -> String {
        let mut s = format!("preset={}", self.preset);
        if let Some(p) = self.palette {
            s.push_str(&format!(" palette={p}"));
        }
        if let Some(t) = self.symbols {
            s.push_str(&format!(" symbols={}", t.as_str()));
        }
        if let Some(on) = self.add_newline {
            s.push_str(&format!(" add_newline={on}"));
        }
        if let Some(r) = &self.rows {
            s.push_str(&tr!(" layout=modified ({} rows)", " layout=改动（{} 行）", r.len()));
        }
        if !self.extra.is_empty() {
            s.push_str(&tr!(" preserved={}", " 保留={}", self.extra.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(text(", ", "、"))));
        }
        s
    }
}

fn pretty(v: &Value, indent: usize, out: &mut String) {
    let pad = |n: usize| " ".repeat(n);
    match v {
        Value::Object(m) if !m.is_empty() => {
            out.push_str("{\n");
            for (i, (k, x)) in m.iter().enumerate() {
                out.push_str(&format!("{}{}: ", pad(indent + 2), json!(k)));
                pretty(x, indent + 2, out);
                out.push_str(if i + 1 < m.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(indent));
            out.push('}');
        }
        Value::Array(a) if a.iter().all(|x| !x.is_object() && !x.is_array()) => {
            out.push('[');
            out.push_str(&a.iter().map(Value::to_string).collect::<Vec<_>>().join(", "));
            out.push(']');
        }
        Value::Array(a) => {
            out.push_str("[\n");
            for (i, x) in a.iter().enumerate() {
                out.push_str(&pad(indent + 2));
                pretty(x, indent + 2, out);
                out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(indent));
            out.push(']');
        }
        _ => out.push_str(&v.to_string()),
    }
}

pub struct Applied {
    pub user: UserConfig,
    pub text: String,
    /// The effective config of `text` (validated), used for the preview.
    pub cfg: Config,
}

/// The user config for `base` with `prefs`, holding only what differs from
/// the preset, plus its validated effective config.
#[cfg(test)]
pub fn apply(base: &Base, prefs: &Prefs) -> Result<Applied, ConfigError> {
    apply_with(base, prefs, Vec::new())
}

/// `apply`, keeping `extra`: the top-level keys of the file being replaced
/// that `tildy theme` does not manage (see `other_keys`). They are
/// validated together with the generated part.
pub fn apply_with(base: &Base, prefs: &Prefs, extra: Vec<(String, Value)>) -> Result<Applied, ConfigError> {
    let tier = prefs.symbols.unwrap_or(base.default_tier);
    let rows = base.transform_rows(prefs);
    let user = UserConfig {
        preset: base.info.name,
        palette: prefs.palette.filter(|p| base.default_palette.as_deref().is_some_and(|d| d != *p)),
        symbols: (tier != base.default_tier).then_some(tier),
        rows: (rows != base.rows).then_some(rows),
        add_newline: prefs.add_newline,
        extra: extra.into_iter().filter(|(k, _)| k != "add_newline" || prefs.add_newline.is_none()).collect(),
    };
    let text = user.to_text();
    let cfg = config::from_str(&text)?;
    Ok(Applied { user, text, cfg })
}

// ---------------------------------------------------------------------------
// Preview data: the current directory's real data, capped at 300 ms.

const HERE_CAP: Duration = Duration::from_millis(300);

struct Entry {
    started: Instant,
    data: Option<PromptData>,
}

type Shared = Arc<(Mutex<HashMap<&'static str, Entry>>, Condvar)>;

/// Per-preset preview data. In `here` mode each preset's data is fetched once
/// on a background thread; `get` waits for it at most 300 ms after the first
/// request and otherwise returns sample data with `pending` set (the loop
/// redraws when it arrives).
pub struct DataCache {
    here: Option<PathBuf>,
    shared: Shared,
}

pub struct Preview {
    pub data: PromptData,
    pub now: Option<LocalTime>,
    /// Real data was requested but is not ready yet.
    pub pending: bool,
    pub real: bool,
}

impl DataCache {
    /// Sample data only (tests).
    #[cfg(test)]
    pub fn sample() -> Self {
        DataCache { here: None, shared: Arc::default() }
    }

    pub fn here(cwd: PathBuf) -> Self {
        DataCache { here: Some(cwd), shared: Arc::default() }
    }

    fn sample_preview(base: &Base, cols: usize, pending: bool) -> Preview {
        Preview { data: sample_data(&base.cfg, cols), now: sample_now(), pending, real: false }
    }

    pub fn get(&self, base: &Base, cols: usize) -> Preview {
        let Some(cwd) = &self.here else { return Self::sample_preview(base, cols, false) };
        let name = base.info.name;
        let (lock, cv) = &*self.shared;
        let mut map = lock.lock().unwrap_or_else(|e| e.into_inner());
        if !map.contains_key(name) {
            map.insert(name, Entry { started: Instant::now(), data: None });
            let (shared, cfg, cwd) = (self.shared.clone(), base.cfg.clone(), cwd.clone());
            std::thread::spawn(move || {
                let (data, _problems) = here_data(&cfg, &cwd, cols);
                let (lock, cv) = &*shared;
                if let Some(e) = lock.lock().unwrap_or_else(|e| e.into_inner()).get_mut(name) {
                    e.data = Some(data);
                }
                cv.notify_all();
            });
        }
        let deadline = map[name].started + HERE_CAP;
        loop {
            if let Some(d) = &map[name].data {
                let mut data = d.clone();
                data.request.cols = cols.min(u16::MAX as usize) as u16;
                return Preview { data, now: None, pending: false, real: true };
            }
            let now = Instant::now();
            if now >= deadline {
                return Self::sample_preview(base, cols, true);
            }
            map = cv.wait_timeout(map, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    pub fn ready(&self, name: &str) -> bool {
        let map = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        map.get(name).is_some_and(|e| e.data.is_some())
    }
}

// ---------------------------------------------------------------------------
// Interactive state, keys and screen.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Toggles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Enter: write the selected preset with the current prefs.
    Write,
    /// q / Esc / end of input: leave without writing.
    Cancel,
    /// Ctrl-C: leave without writing.
    Interrupt,
}

pub struct App {
    pub bases: Vec<Base>,
    pub sel: usize,
    pub prefs: Prefs,
    pub focus: Focus,
    /// Index into `TOGGLES`.
    pub tcur: usize,
    pub help: bool,
    pub status: Option<String>,
    pub target: PathBuf,
    /// The target exists (it is backed up before writing).
    pub target_exists: bool,
    /// Top-level keys of the existing target that the new file will not keep.
    pub dropped_keys: Vec<String>,
    /// Top-level keys of the existing target the new file keeps unchanged.
    pub extra: Vec<(String, Value)>,
    /// Shown in the preview header for real data (the current directory).
    pub cwd_label: String,
}

impl App {
    pub fn base(&self) -> &Base {
        &self.bases[self.sel]
    }
}

/// Apply one key. Some(outcome) ends the loop.
pub fn handle(app: &mut App, key: Key) -> Option<Outcome> {
    let n = app.bases.len();
    app.status = None;
    match key {
        Key::Up | Key::Char('k') => app.sel = (app.sel + n - 1) % n,
        Key::Down | Key::Char('j') => app.sel = (app.sel + 1) % n,
        // In the list, ←/→ cycle the palette of a role-based preset.
        Key::Left | Key::Char('h') if app.focus == Focus::List && app.bases[app.sel].is_role() => {
            app.prefs.cycle_palette(&app.bases[app.sel], -1);
        }
        Key::Right | Key::Char('l') if app.focus == Focus::List && app.bases[app.sel].is_role() => {
            app.prefs.cycle_palette(&app.bases[app.sel], 1);
        }
        Key::Left | Key::Char('h') => {
            app.tcur = (app.tcur + TOGGLES.len() - 1) % TOGGLES.len();
            app.focus = Focus::Toggles;
        }
        Key::Right | Key::Char('l') => {
            app.tcur = (app.tcur + 1) % TOGGLES.len();
            app.focus = Focus::Toggles;
        }
        Key::Tab | Key::BackTab => app.focus = if app.focus == Focus::List { Focus::Toggles } else { Focus::List },
        Key::Space => {
            let t = TOGGLES[app.tcur];
            let base = &app.bases[app.sel];
            if !app.prefs.toggle(base, t) {
                app.status = Some(tr!("{} has no {} module to toggle (n/a)", "{} 没有可开关的{}模块（n/a）", base.info.name, t.label()));
            }
        }
        Key::Enter => return Some(Outcome::Write),
        Key::Esc | Key::Char('q') => return Some(Outcome::Cancel),
        Key::CtrlC => return Some(Outcome::Interrupt),
        Key::Char('?') => app.help = !app.help,
        _ => {}
    }
    None
}

/// Truncate plain text to `w` display columns (with `…` when cut).
fn fit(s: &str, w: usize) -> String {
    if UnicodeWidthStr::width(s) <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw + 1 > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

fn pad(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(UnicodeWidthStr::width(s))))
}

/// `── title ─────` across the width (dim).
fn rule(title: &str, cols: usize) -> String {
    let head = if title.is_empty() { String::new() } else { format!("── {} ", fit(title, cols.saturating_sub(4))) };
    let rest = cols.saturating_sub(UnicodeWidthStr::width(head.as_str()));
    format!("\x1b[2m{head}{}\x1b[0m", "─".repeat(rest))
}

fn home_short(p: &Path) -> String {
    if let Some(h) = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute()) {
        if let Ok(rest) = p.strip_prefix(&h) {
            return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) };
        }
    }
    p.display().to_string()
}

fn state_text(s: State) -> String {
    match s {
        State::Na => "n/a".into(),
        State::Tier(t, own) => format!("{}{}", t.as_str(), if own { "" } else { "*" }),
        State::Rows(Rows::One) => text("One", "单行").into(),
        State::Rows(Rows::Two) => text("Two", "两行").into(),
        State::On(true) => text("On", "开").into(),
        State::On(false) => text("Off", "关").into(),
    }
}

fn key_help() -> &'static str {
    text("↑↓/jk Preset · ←→/hl Control (palette in role-based preset list) · Space Toggle · Tab List/controls · Enter Save · q/Esc Cancel", "↑↓/jk 选预设 · ←→/hl 选开关（焦点在列表且预设按角色着色时换调色板）· 空格 切换 · Tab 列表/开关 · Enter 写入并退出 · q/Esc 不写入退出")
}

/// The full screen as lines (at most `height`), each fitting `cols`
/// (preview lines rely on autowrap being off to clip).
pub fn screen(app: &App, applied: &Result<Applied, ConfigError>, pv: &Preview, cols: usize, height: usize) -> Vec<String> {
    let base = app.base();
    let mut top = vec![format!(
        "\x1b[1mtildy theme\x1b[0m  \x1b[2m{}\x1b[0m",
        fit(text("Choose a preset and options; Enter to save", "选一个预设，调好开关，按 Enter 写入"), cols.saturating_sub(17))
    )];
    let source = if pv.real {
        tr!("Current directory {}", "当前目录 {}", app.cwd_label)
    } else if pv.pending {
        text("Sample data (current directory pending after 300 ms; refreshes when ready)", "示例数据（当前目录的数据 300 ms 内未就绪，就绪后自动刷新）").to_string()
    } else {
        text("Sample data", "示例数据").to_string()
    };
    let mut preview = vec![];
    match applied {
        Ok(a) => {
            let pal = match base.palette(&app.prefs) {
                Some(p) if app.focus == Focus::List => tr!(" · Palette {p} (←→ to change)", " · 调色板 {p}（←→ 切换）"),
                Some(p) => tr!(" · Palette {p} (Tab to list, then ←→)", " · 调色板 {p}（Tab 回列表后 ←→ 切换）"),
                None => String::new(),
            };
            top.push(rule(&tr!("Preview · {source} · Symbols {}{pal}", "预览 · {source} · 符号层 {}{pal}", a.cfg.symbols.as_str()), cols));
            let out = render::render_preview(&a.cfg, &pv.data, pv.now);
            preview.extend(compose_lines(&out, cols).into_iter().map(|l| format!("{l}\x1b[0m")));
            if let Some(t) = &out.prompt.transient {
                let transient = tr!("\x1b[2mAfter execution:\x1b[0m {}\x1b[0m", "\x1b[2m执行后收起为:\x1b[0m {}\x1b[0m", zsh_to_ansi(t));
                preview.extend(transient.split('\n').map(str::to_string));
            }
        }
        Err(e) => {
            top.push(rule(text("Preview", "预览"), cols));
            preview.push(format!("\x1b[31m{}\x1b[0m", fit(&tr!("Invalid config: {e}", "配置无效：{e}"), cols)));
        }
    }
    top.extend(preview);
    top.push(rule(text("Presets", "预设"), cols));

    let mut bottom = vec![rule(text("Controls", "开关"), cols)];
    let mut tl = String::from(" ");
    let mut used = 1;
    for (i, t) in TOGGLES.iter().enumerate() {
        let st = base.state(&app.prefs, *t);
        let item = tr!(" {}: {} ", " {}：{} ", t.label(), state_text(st));
        let w = UnicodeWidthStr::width(item.as_str()) + 1;
        if used + w > cols && used > 1 {
            bottom.push(tl);
            tl = String::from(" ");
            used = 1;
        }
        used += w;
        let sgr = match (i == app.tcur, app.focus) {
            (true, Focus::Toggles) => "\x1b[7m",
            (true, Focus::List) => "\x1b[4m",
            _ if st == State::Na => "\x1b[2m",
            _ => "",
        };
        tl.push_str(&format!("{sgr}{item}\x1b[0m "));
    }
    bottom.push(tl);
    bottom.push(rule("", cols));
    let write_line = match applied {
        Ok(a) => {
            // The path goes last: on a narrow screen it is what gets cut.
            let mut s = tr!("Will write {}", "将写入 {}", a.user.summary());
            if !app.dropped_keys.is_empty() {
                s.push_str(&tr!(" (will replace existing {})", "（原文件的 {} 不保留）", app.dropped_keys.join(text(", ", "、"))));
            }
            if !app.extra.is_empty() {
                s.push_str(&tr!(" (preserving existing {})", "（保留原文件的 {}）", app.extra.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(text(", ", "、"))));
            }
            s.push_str(&format!(" → {}", home_short(&app.target)));
            if app.target_exists {
                s.push_str(text(" (existing file saved to history)", "（已存在，原文件存入历史）"));
            }
            s
        }
        Err(_) => text("Invalid configuration; Enter will not write", "当前组合无法生成有效配置，Enter 不会写入").to_string(),
    };
    bottom.push(fit(&write_line, cols));
    bottom.push(match (&app.status, app.help) {
        (Some(s), _) => format!("\x1b[33m{}\x1b[0m", fit(s, cols)),
        (None, true) => fit(key_help(), cols),
        (None, false) => {
            let lr = if base.is_role() && app.focus == Focus::List { text("←→ Palette", "←→ 换调色板") } else { text("←→ Control", "←→ 选开关") };
            format!("\x1b[2m{}\x1b[0m", fit(&tr!("↑↓ Preset · {lr} · Space Toggle · Enter Save · q Cancel · ? Help", "↑↓ 选预设 · {lr} · 空格 切换 · Enter 写入 · q 退出 · ? 按键说明"), cols))
        }
    });

    // The list gets whatever height is left (at least 3 rows), scrolled so
    // the selection stays visible.
    let n = app.bases.len();
    let room = height.saturating_sub(top.len() + bottom.len()).max(3).min(n);
    let first = app.sel.saturating_sub(room / 2).min(n - room);
    let nw = app.bases.iter().map(|b| b.info.name.len()).max().unwrap_or(0);
    let mut list = vec![];
    for (i, b) in app.bases.iter().enumerate().skip(first).take(room) {
        let selected = i == app.sel;
        let text = format!("{}{}  {}  {}", if selected { "▸ " } else { "  " }, pad(b.info.name, nw), pad(nerd_label(b.info.nerd_font), 14), description(b.info.description));
        let text = fit(&text, cols);
        list.push(match (selected, app.focus) {
            (true, Focus::List) => format!("\x1b[7m{}\x1b[0m", pad(&text, cols)),
            (true, Focus::Toggles) => format!("\x1b[1m{text}\x1b[0m"),
            _ => text,
        });
    }
    let mut lines = top;
    lines.extend(list);
    lines.extend(bottom);
    lines.truncate(height.max(1));
    lines
}

struct Drawn {
    size: (usize, usize),
    pending: bool,
}

fn draw<W: Write>(app: &App, out: &mut W, cache: &DataCache, size: (usize, usize)) -> io::Result<Drawn> {
    let (cols, height) = size;
    let applied = apply_with(app.base(), &app.prefs, app.extra.clone());
    let pv = cache.get(app.base(), cols);
    let lines = screen(app, &applied, &pv, cols, height);
    let mut buf = String::from("\x1b[H");
    for (i, l) in lines.iter().enumerate() {
        buf.push_str(l);
        buf.push_str("\x1b[0m\x1b[K");
        if i + 1 < lines.len() {
            buf.push_str("\r\n");
        }
    }
    buf.push_str("\x1b[J");
    out.write_all(buf.as_bytes())?;
    out.flush()?;
    Ok(Drawn { size, pending: pv.pending })
}

/// The interactive loop: draw, read a key, apply it, redraw. Ticks (input
/// timeouts) redraw only when the terminal size changed or pending preview
/// data arrived. End of input cancels.
pub fn run_loop<R: Read, W: Write>(
    app: &mut App,
    keys: &mut KeyReader<R>,
    out: &mut W,
    cache: &DataCache,
    size: &dyn Fn() -> (usize, usize),
) -> io::Result<Outcome> {
    let mut last = draw(app, out, cache, size())?;
    loop {
        match keys.next_event()? {
            Event::Eof => return Ok(Outcome::Cancel),
            Event::Tick => {
                let s = size();
                if s != last.size || (last.pending && cache.ready(app.base().info.name)) {
                    last = draw(app, out, cache, s)?;
                }
            }
            Event::Key(k) => {
                if let Some(o) = handle(app, k) {
                    return Ok(o);
                }
                last = draw(app, out, cache, size())?;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Writing.

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+:@~".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// tildy looks initialized: TILDY_BIN is in the environment, or the user's
/// .zshrc has the managed prompt block or a manual `tildy init zsh` line.
fn shell_initialized() -> bool {
    if std::env::var_os("TILDY_BIN").is_some() {
        return true;
    }
    let dir = std::env::var_os("ZDOTDIR").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    dir.and_then(|d| std::fs::read_to_string(d.join(".zshrc")).ok())
        .is_some_and(|s| s.contains("tildy init zsh") || s.contains("# >>> tildy prompt >>>"))
}

/// The setup command, with the full path when `tildy` is not on PATH.
fn init_command() -> String {
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("tildy").is_file()));
    let bin = if on_path {
        "tildy".to_string()
    } else {
        std::env::current_exe().map(|p| shell_quote(&p.display().to_string())).unwrap_or_else(|_| "tildy".into())
    };
    format!("{bin} init")
}

/// Write through the config transaction (history + revision check, the same
/// path `tildy config apply` takes) and report: the JSON on stdout; where it
/// went, the rollback command and how to activate on stderr. `expect` is the
/// revision read when the command started, so a file changed meanwhile by an
/// Agent or another shell is refused instead of overwritten.
fn finish(applied: &Applied, store: &Store, expect: &str) -> i32 {
    let target = tx::resolve_target(&store.config_path);
    if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("tildy theme: cannot create {}: {e}", dir.display());
            return ErrorCode::EnvironmentUnavailable.exit_code();
        }
    }
    let r = match store.apply(applied.text.as_bytes(), Some(expect), false) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tildy theme: {e}");
            if let Some(h) = &e.hint {
                tr_eprintln!("Hint: {h}", "提示：{h}");
            }
            return e.code.exit_code();
        }
    };
    print!("{}", applied.text);
    let _ = io::stdout().flush();
    eprintln!();
    for w in &r.warnings {
        match &w.path {
            Some(p) => tr_eprintln!("Note: {p}: {}", "注意：{p}: {}", w.message),
            None => tr_eprintln!("Note: {}", "注意：{}", w.message),
        }
    }
    if r.changed {
        tr_eprintln!("Wrote {} (revision {})", "已写入 {}（修订 {}）", r.path.display(), r.revision);
        if let Some(id) = &r.history_id {
            tr_eprintln!("Previous file saved to history ({id}; see `tildy config history`)", "原文件已存入历史（{id}，`tildy config history` 可查）");
        }
        tr_eprintln!("Rollback: tildy config rollback --to {} --expect-revision {}", "回滚：tildy config rollback --to {} --expect-revision {}", r.previous_revision, r.revision);
    } else {
        tr_eprintln!("{} unchanged (revision {})", "{} 内容未变（修订 {}）", r.path.display(), r.revision);
    }
    let default = config::default_config_path();
    if default.as_deref() != Some(store.config_path.as_path()) {
        tr_eprintln!("Note: tildy reads {} by default; to use this file, add export TILDY_CONFIG={} to .zshrc", "注意：tildy 默认读取 {}；要用这个文件，在 .zshrc 里 export TILDY_CONFIG={}",
            default.map(|p| p.display().to_string()).unwrap_or_else(|| "~/.config/tildy/config.json".into()),
            shell_quote(&store.config_path.display().to_string())
        );
    }
    tr_eprintln!("Changes take effect at the next prompt, or in a new shell.", "生效：在已加载 tildy 的 Shell 里按一次 Enter（下一个提示符读取新配置），或重启 Shell。");
    if !shell_initialized() {
        tr_eprintln!("To enable tildy, run {} to set up .zshrc.", "还没有启用 tildy？运行 {} 自动配置 .zshrc。", init_command());
    }
    0
}

// ---------------------------------------------------------------------------
// Entry point.

fn prefs_from_args(a: &ConfigureArgs) -> Result<Prefs, String> {
    let symbols = match a.symbols.as_deref() {
        None => None,
        Some(s) => Some(parse_tier(s).ok_or_else(|| format!("unknown symbol tier '{s}' (nerd, unicode or ascii)"))?),
    };
    let palette = match a.palette.as_deref() {
        None => None,
        Some(p) => Some(config::palette::builtin(p).map(|i| i.name).ok_or_else(|| {
            format!("unknown palette '{p}' (built-in: {})", PALETTE_INFO.iter().map(|i| i.name).collect::<Vec<_>>().join(", "))
        })?),
    };
    let rows = if a.one_line {
        Some(Rows::One)
    } else if a.two_line {
        Some(Rows::Two)
    } else {
        None
    };
    let add_newline = if a.add_newline { Some(true) } else if a.no_add_newline { Some(false) } else { None };
    Ok(Prefs { symbols, palette, rows, add_newline, time: !a.no_time, langs: !a.no_langs, duration_jobs: !a.no_duration })
}

pub fn run(a: ConfigureArgs) -> i32 {
    let prefs = match prefs_from_args(&a) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("tildy theme: {e}");
            return ErrorCode::ValidationFailed.exit_code();
        }
    };
    let Some(target) = a.output.clone().or_else(config::default_config_path) else {
        eprintln!("tildy theme: cannot determine the config path (HOME unset); pass --output");
        return ErrorCode::EnvironmentUnavailable.exit_code();
    };
    let store = Store { config_path: target, history_dir: tx::history_dir() };
    let expect = match store.current() {
        Ok(c) => c.revision,
        Err(e) => {
            eprintln!("tildy theme: {e}");
            return e.code.exit_code();
        }
    };
    match a.preset.as_deref() {
        Some(name) => non_interactive(name, prefs, &store, &expect),
        None => interactive(prefs, &store, &expect),
    }
}

fn non_interactive(name: &str, mut prefs: Prefs, store: &Store, expect: &str) -> i32 {
    let target = store.config_path.as_path();
    let base = match Base::by_name(name) {
        Some(Ok(b)) => b,
        Some(Err(e)) => {
            eprintln!("tildy theme: preset '{name}': {e}");
            return ErrorCode::InternalError.exit_code();
        }
        None => {
            eprintln!("tildy theme: unknown preset '{name}'");
            eprintln!("hint: tildy preset list");
            return ErrorCode::ValidationFailed.exit_code();
        }
    };
    if prefs.palette.is_some() && !base.is_role() {
        eprintln!(
            "tildy theme: preset '{name}' has fixed colors; --palette applies to the role-based presets ({})",
            config::ROLE_PRESETS.join(", ")
        );
        return ErrorCode::ValidationFailed.exit_code();
    }
    if let Some(want) = prefs.rows {
        if base.rows_default().is_none() && want != if base.rows.len() >= 2 { Rows::Two } else { Rows::One } {
            eprintln!("tildy theme: preset '{name}' has no character module; its row count cannot be changed");
            return ErrorCode::ValidationFailed.exit_code();
        }
    }
    for (flag, off, t) in [("--no-time", !prefs.time, Toggle::Time), ("--no-langs", !prefs.langs, Toggle::Langs), ("--no-duration", !prefs.duration_jobs, Toggle::DurationJobs)] {
        if off && base.state(&prefs, t) == State::Na {
            eprintln!("tildy theme: note: preset '{name}' has no {} module; {flag} changes nothing", t.name());
        }
    }
    // Keys the existing file has beyond what this command manages stay.
    let extra = std::fs::read_to_string(target).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).map(|v| other_keys(&v, &mut prefs)).unwrap_or_default();
    match apply_with(&base, &prefs, extra) {
        Ok(applied) => finish(&applied, store, expect),
        Err(e) => {
            eprintln!("tildy theme: generated config does not validate: {e}");
            ErrorCode::InternalError.exit_code()
        }
    }
}

/// The top-level keys of an existing config that `tildy theme` does not
/// manage (`modules`, `palettes`, ...), with their values. Restore the blank-line
/// preference unless a CLI flag overrides it. Invalid values stay in `extra`
/// so validation rejects them instead of silently dropping them.
fn other_keys(v: &Value, prefs: &mut Prefs) -> Vec<(String, Value)> {
    prefs.add_newline = prefs.add_newline.or_else(|| v.get("add_newline").and_then(Value::as_bool));
    v.as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !matches!(k.as_str(), "schema_version" | "preset" | "palette" | "symbols" | "layout"))
                .filter(|(k, _)| k.as_str() != "add_newline" || prefs.add_newline.is_none())
                .map(|(k, x)| (k.clone(), x.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Start from the existing target: select its preset, take its icon tier and,
/// when its layout is one `tildy theme` would write for that preset, the
/// toggles that produce it. Returns the keys the new file will not keep (a
/// hand-edited `layout`) and the ones it keeps unchanged (`other_keys`).
fn start_from_existing(bases: &[Base], v: &Value, prefs: &mut Prefs, sel: &mut usize, flags_given: bool) -> (Vec<String>, Vec<(String, Value)>) {
    let extra = other_keys(v, prefs);
    let mut dropped: Vec<String> = Vec::new();
    let name = v.get("preset").and_then(Value::as_str).unwrap_or(config::DEFAULT_PRESET);
    let Some(i) = bases.iter().position(|b| b.info.name == name) else { return (dropped, extra) };
    *sel = i;
    if prefs.symbols.is_none() {
        prefs.symbols = v.get("symbols").and_then(Value::as_str).and_then(parse_tier);
    }
    if prefs.palette.is_none() {
        prefs.palette = v.get("palette").and_then(Value::as_str).and_then(config::palette::builtin).map(|i| i.name);
    }
    if let Some(rows) = v.pointer("/layout/rows").and_then(Value::as_array) {
        match prefs.infer(&bases[i], rows) {
            Some(p) if !flags_given => *prefs = p,
            Some(_) => {}
            None => dropped.push("layout".into()),
        }
    }
    (dropped, extra)
}

fn interactive(mut prefs: Prefs, store: &Store, expect: &str) -> i32 {
    let target = store.config_path.as_path();
    if !(tui::is_tty(0) && tui::is_tty(1)) {
        eprintln!("tildy theme: needs an interactive terminal (stdin and stdout must be a TTY)");
        eprintln!("hint: for scripts use `tildy theme --preset NAME [--palette P] [--symbols nerd|unicode|ascii] [--one-line|--two-line] [--add-newline|--no-add-newline] [--no-time] [--no-langs] [--no-duration]`");
        return ErrorCode::EnvironmentUnavailable.exit_code();
    }
    let mut bases = vec![];
    for p in PRESET_INFO {
        match Base::new(p) {
            Ok(b) => bases.push(b),
            Err(e) => {
                eprintln!("tildy theme: preset '{}': {e}", p.name);
                return ErrorCode::InternalError.exit_code();
            }
        }
    }
    let target_exists = target.exists();
    let mut sel = bases.iter().position(|b| b.info.name == config::DEFAULT_PRESET).unwrap_or(0);
    let existing: Option<Value> = std::fs::read_to_string(target).ok().and_then(|t| serde_json::from_str(&t).ok());
    let flags_given = prefs != Prefs { symbols: prefs.symbols, palette: prefs.palette, add_newline: prefs.add_newline, ..Prefs::default() };
    let (dropped_keys, extra) = existing.map(|v| start_from_existing(&bases, &v, &mut prefs, &mut sel, flags_given)).unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut app = App {
        bases,
        sel,
        prefs,
        focus: Focus::List,
        tcur: 0,
        help: false,
        status: None,
        target: target.to_path_buf(),
        target_exists,
        dropped_keys,
        extra,
        cwd_label: home_short(&cwd),
    };
    let cache = DataCache::here(cwd);
    let outcome = {
        let _raw = match RawMode::enter(0, 1) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("tildy theme: cannot switch the terminal to raw mode: {e}");
                return ErrorCode::EnvironmentUnavailable.exit_code();
            }
        };
        let mut keys = KeyReader::new(TtyIn { fd: 0, timeout_ms: 50 });
        let stdout = io::stdout();
        let mut out = stdout.lock();
        run_loop(&mut app, &mut keys, &mut out, &cache, &|| tui::size(1).unwrap_or((80, 24)))
        // `_raw` drops here: cooked mode, cursor and main screen are back.
    };
    match outcome {
        Ok(Outcome::Write) => match apply_with(app.base(), &app.prefs, app.extra.clone()) {
            Ok(applied) => finish(&applied, store, expect),
            Err(e) => {
                eprintln!("tildy theme: generated config does not validate: {e}");
                ErrorCode::InternalError.exit_code()
            }
        },
        Ok(Outcome::Cancel) => {
            tr_eprintln!("tildy theme: cancelled; no files written", "tildy theme: 已退出，未写入任何文件");
            0
        }
        Ok(Outcome::Interrupt) => {
            tr_eprintln!("tildy theme: interrupted; no files written", "tildy theme: 已中断，未写入任何文件");
            130
        }
        Err(e) => {
            eprintln!("tildy theme: terminal error: {e}");
            ErrorCode::InternalError.exit_code()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(name: &str) -> Base {
        Base::by_name(name).unwrap().unwrap()
    }

    fn rows_ids(rows: &[Value]) -> Vec<(Vec<String>, Vec<String>)> {
        rows.iter().map(|r| (ids(r, "left"), ids(r, "right"))).collect()
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn user_json(b: &Base, p: &Prefs) -> Value {
        serde_json::from_str(&apply(b, p).unwrap().text).unwrap()
    }

    #[test]
    fn pastel_powerline_toggles() {
        let b = base("pastel-powerline");
        assert_eq!(b.rows_default(), Some(Rows::Two));
        assert_eq!(b.char_ids, s(&["char"]));
        assert_eq!(b.time_ids, s(&["time"]));
        assert_eq!(b.dj_ids, s(&["jobs", "elapsed"]));
        // Native language modules and the Starship-bridged ones; docker is not a version.
        assert_eq!(b.lang_ids, s(&["c", "elixir", "elm", "go", "haskell", "java", "julia", "node", "nim", "py", "rust"]));
        for t in TOGGLES {
            assert_ne!(b.state(&Prefs::default(), t), State::Na, "{t:?}");
        }
        let one = Prefs { rows: Some(Rows::One), ..Prefs::default() };
        let r = b.transform_rows(&one);
        assert_eq!(r.len(), 1);
        let left = ids(&r[0], "left");
        assert_eq!(left.first().map(String::as_str), Some("user"));
        assert_eq!(left.last().map(String::as_str), Some("char"));
        assert_eq!(left.len(), ids(&b.rows[0], "left").len() + ids(&b.rows[1], "left").len());
        assert_eq!(r[0]["style"], "powerline", "first row keeps its style and separators");
        assert_eq!(r[0]["separator"], b.rows[0]["separator"]);

        let off = Prefs { time: false, langs: false, duration_jobs: false, ..Prefs::default() };
        let r = b.transform_rows(&off);
        assert_eq!(rows_ids(&r), vec![(s(&["user", "host", "cwd", "git", "changes", "commit", "docker"]), vec![]), (s(&["status", "char"]), vec![])]);
    }

    #[test]
    fn tokyo_night_toggles() {
        let b = base("tokyo-night");
        assert_eq!(b.rows_default(), Some(Rows::Two));
        assert_eq!(b.lang_ids, s(&["node", "rust", "go", "php"]), "php is a command module showing $version");
        assert_eq!(b.state(&Prefs::default(), Toggle::DurationJobs), State::Na);
        let p = Prefs { rows: Some(Rows::One), time: false, ..Prefs::default() };
        assert_eq!(rows_ids(&b.transform_rows(&p)), vec![(s(&["text1", "cwd", "git", "changes", "node", "rust", "go", "php", "char"]), vec![])]);
        let p = Prefs { langs: false, ..Prefs::default() };
        assert_eq!(rows_ids(&b.transform_rows(&p)), vec![(s(&["text1", "cwd", "git", "changes", "time"]), vec![]), (s(&["char"]), vec![])]);
    }

    #[test]
    fn pure_preset_toggles() {
        let b = base("pure-preset");
        assert_eq!(b.rows_default(), Some(Rows::Two));
        // `py` shows only the virtualenv (`$env`), not a version.
        assert_eq!(b.state(&Prefs::default(), Toggle::Langs), State::Na);
        assert_eq!(b.state(&Prefs::default(), Toggle::Time), State::Na);
        assert_eq!(b.state(&Prefs::default(), Toggle::DurationJobs), State::On(true));
        let p = Prefs { rows: Some(Rows::One), duration_jobs: false, ..Prefs::default() };
        assert_eq!(rows_ids(&b.transform_rows(&p)), vec![(s(&["user", "host", "cwd", "git", "changes", "py", "char"]), vec![])]);
        // Toggling an n/a toggle changes nothing.
        let mut q = Prefs::default();
        assert!(!q.toggle(&b, Toggle::Time));
        assert_eq!(q, Prefs::default());
    }

    #[test]
    fn plain_toggles() {
        let b = base("plain");
        assert_eq!(b.rows_default(), None, "the builtin layout has no character module");
        assert_eq!(b.state(&Prefs::default(), Toggle::Rows), State::Na);
        assert_eq!(b.state(&Prefs::default(), Toggle::Time), State::Na);
        assert_eq!(b.lang_ids, s(&["c", "go", "java", "node", "py", "rust"]));
        assert_eq!(b.dj_ids, s(&["jobs", "elapsed"]));
        let p = Prefs { duration_jobs: false, langs: false, rows: Some(Rows::Two), ..Prefs::default() };
        assert_eq!(rows_ids(&b.transform_rows(&p)), vec![(s(&["cwd", "git", "changes", "ssh_host", "docker"]), s(&["result"]))]);
    }

    #[test]
    fn single_row_presets_split_into_two() {
        let b = base("catppuccin-powerline");
        assert_eq!(b.rows_default(), Some(Rows::One));
        let r = b.transform_rows(&Prefs { rows: Some(Rows::Two), ..Prefs::default() });
        assert_eq!(r.len(), 2);
        assert!(!ids(&r[0], "left").contains(&"char".to_string()));
        assert_eq!(r[0]["style"], "powerline");
        assert_eq!(rows_ids(&r[1..]), vec![(s(&["char"]), vec![])]);
        // jetpack: the right side moves to the last row, where Zsh draws RPROMPT.
        let b = base("jetpack");
        let r = b.transform_rows(&Prefs { rows: Some(Rows::Two), ..Prefs::default() });
        assert_eq!(ids(&r[0], "left"), s(&["duration", "host", "jobs", "user"]));
        assert!(ids(&r[0], "right").is_empty());
        assert_eq!(ids(&r[1], "left"), s(&["char"]));
        assert_eq!(ids(&r[1], "right"), ids(&b.rows[0], "right"));
    }

    #[test]
    fn overrides_are_minimal() {
        for name in ["pastel-powerline", "tokyo-night", "pure-preset", "plain", "jetpack"] {
            let b = base(name);
            // Untouched: only schema_version and preset.
            assert_eq!(user_json(&b, &Prefs::default()), json!({"schema_version": 1, "preset": name}), "{name}");
            // Choosing the preset's own tier writes nothing.
            let own = Prefs { symbols: Some(b.default_tier), ..Prefs::default() };
            assert_eq!(user_json(&b, &own), json!({"schema_version": 1, "preset": name}), "{name}");
            // A layout toggle flipped twice is back to the preset: no layout.
            let mut p = Prefs::default();
            for t in TOGGLES {
                if !matches!(t, Toggle::Icons | Toggle::Newline) && p.toggle(&b, t) {
                    assert!(user_json(&b, &p).get("layout").is_some(), "{name} {t:?}");
                    p.toggle(&b, t);
                }
            }
            assert_eq!(user_json(&b, &p), json!({"schema_version": 1, "preset": name}), "{name}");
        }
        let b = base("pastel-powerline");
        let v = user_json(&b, &Prefs { symbols: Some(SymbolTier::Ascii), ..Prefs::default() });
        assert_eq!(v, json!({"schema_version": 1, "preset": "pastel-powerline", "symbols": "ascii"}));
        let v = user_json(&b, &Prefs { time: false, ..Prefs::default() });
        assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), ["layout", "preset", "schema_version"]);
    }

    #[test]
    fn written_text_has_fixed_key_order_and_inline_arrays() {
        let b = base("tokyo-night");
        let a = apply(&b, &Prefs { symbols: Some(SymbolTier::Unicode), rows: Some(Rows::One), ..Prefs::default() }).unwrap();
        let t = &a.text;
        let pos = |k: &str| t.find(&format!("\"{k}\"")).unwrap();
        assert!(pos("schema_version") < pos("preset") && pos("preset") < pos("symbols") && pos("symbols") < pos("layout"), "{t}");
        assert!(t.contains(r#""left": ["text1", "cwd", "git", "changes", "node", "rust", "go", "php", "time", "char"]"#), "{t}");
        assert_eq!(a.cfg.file.layout.rows.len(), 1);
        assert_eq!(a.cfg.symbols, SymbolTier::Unicode);
    }

    #[test]
    fn every_preset_and_combination_validates_and_renders() {
        // The tier does not change the layout; every tier's rendering is covered in preview.rs.
        let tiers = [None, Some(SymbolTier::Ascii)];
        let rows = [None, Some(Rows::One), Some(Rows::Two)];
        for p in PRESET_INFO {
            let b = Base::new(p).unwrap();
            for symbols in tiers {
                for r in rows {
                    for bits in 0..8u8 {
                        let prefs = Prefs {
                            symbols,
                            palette: None,
                            rows: r,
                            add_newline: None,
                            time: bits & 1 == 0,
                            langs: bits & 2 == 0,
                            duration_jobs: bits & 4 == 0,
                        };
                        let a = apply(&b, &prefs).unwrap_or_else(|e| panic!("{} {prefs:?}: {e}", p.name));
                        let out = render::render_at(&a.cfg, &sample_data(&a.cfg, 100), sample_now());
                        assert!(!out.prompt.left.is_empty(), "{} {prefs:?}", p.name);
                        if b.rows_default().is_some() {
                            let want = r.unwrap_or(b.rows_default().unwrap());
                            assert_eq!(a.cfg.file.layout.rows.len() == 1, want == Rows::One, "{} {prefs:?}", p.name);
                        }
                    }
                }
            }
        }
    }

    fn test_app() -> App {
        App {
            bases: PRESET_INFO.iter().map(|p| Base::new(p).unwrap()).collect(),
            sel: 0,
            prefs: Prefs::default(),
            focus: Focus::List,
            tcur: 0,
            help: false,
            status: None,
            target: PathBuf::from("/tmp/tildy-test/config.json"),
            target_exists: false,
            dropped_keys: vec![],
            extra: vec![],
            cwd_label: "~/x".into(),
        }
    }

    #[test]
    fn keys_move_toggle_and_finish() {
        let mut app = test_app();
        assert_eq!(handle(&mut app, Key::Up), None);
        assert_eq!(app.sel, PRESET_INFO.len() - 1, "wraps around");
        handle(&mut app, Key::Char('j'));
        handle(&mut app, Key::Down);
        assert_eq!(app.base().info.name, "pastel-powerline");
        handle(&mut app, Key::Right);
        assert_eq!((app.focus, TOGGLES[app.tcur]), (Focus::Toggles, Toggle::Rows));
        handle(&mut app, Key::Space);
        assert_eq!(app.prefs.rows, Some(Rows::One));
        handle(&mut app, Key::Tab);
        assert_eq!(app.focus, Focus::List);
        handle(&mut app, Key::Char('?'));
        assert!(app.help);
        // plain: rows is n/a -> status message, prefs unchanged.
        app.sel = 0;
        handle(&mut app, Key::Space);
        assert!(app.status.as_deref().unwrap().contains("n/a"));
        assert_eq!(app.prefs.rows, Some(Rows::One));
        assert_eq!(handle(&mut app, Key::Enter), Some(Outcome::Write));
        assert_eq!(handle(&mut app, Key::Esc), Some(Outcome::Cancel));
        assert_eq!(handle(&mut app, Key::Char('q')), Some(Outcome::Cancel));
        assert_eq!(handle(&mut app, Key::CtrlC), Some(Outcome::Interrupt));
    }

    #[test]
    fn palettes_cycle_for_role_presets_and_are_written_only_when_changed() {
        let b = base("powerline");
        assert!(b.is_role());
        assert_eq!(b.palette(&Prefs::default()), Some("catppuccin-mocha"));
        assert_eq!(user_json(&b, &Prefs::default()), json!({"schema_version": 1, "preset": "powerline"}));
        let mut p = Prefs::default();
        assert!(p.cycle_palette(&b, 1), "mocha is last: wraps to the first");
        assert_eq!(p.palette, Some(PALETTE_INFO[0].name));
        let a = apply(&b, &Prefs { symbols: Some(SymbolTier::Ascii), ..p }).unwrap();
        let pos = |k: &str| a.text.find(&format!("\"{k}\"")).unwrap();
        assert!(pos("preset") < pos("palette") && pos("palette") < pos("symbols"), "{}", a.text);
        assert_eq!(a.cfg.file.palette.as_deref(), Some(PALETTE_INFO[0].name));
        assert!(a.user.summary().contains(&format!("palette={}", PALETTE_INFO[0].name)));
        // Back to the default: nothing written.
        assert!(p.cycle_palette(&b, -1));
        assert_eq!(user_json(&b, &p), json!({"schema_version": 1, "preset": "powerline"}));
        // The choice carries over to another role-based preset, not to fixed colors.
        let p = Prefs { palette: Some("nord"), ..Prefs::default() };
        assert_eq!(user_json(&base("lean"), &p)["palette"], "nord");
        let fixed = base("pastel-powerline");
        assert!(!fixed.is_role() && fixed.palette(&p).is_none());
        assert_eq!(user_json(&fixed, &p), json!({"schema_version": 1, "preset": "pastel-powerline"}));
        assert!(!Prefs::default().cycle_palette(&fixed, 1));
        // Every role preset with every palette and both row counts validates.
        for name in config::ROLE_PRESETS {
            let b = base(name);
            for pal in PALETTE_INFO {
                for rows in [Some(Rows::One), Some(Rows::Two)] {
                    let prefs = Prefs { palette: Some(pal.name), rows, ..Prefs::default() };
                    let a = apply(&b, &prefs).unwrap_or_else(|e| panic!("{name} {}: {e}", pal.name));
                    let out = render::render_at(&a.cfg, &sample_data(&a.cfg, 100), sample_now());
                    assert!(!out.prompt.left.is_empty());
                }
            }
        }
    }

    #[test]
    fn p10k_presets_collapse_with_their_upper_right_side() {
        let b = base("lean");
        let one = b.transform_rows(&Prefs { rows: Some(Rows::One), ..Prefs::default() });
        assert_eq!(rows_ids(&one), vec![(s(&["cwd", "git", "changes", "char"]), s(&["status", "elapsed", "jobs", "venv", "context", "time"]))]);
        // The toggles act on the upper row's right side too.
        let r = b.transform_rows(&Prefs { time: false, duration_jobs: false, ..Prefs::default() });
        assert_eq!(rows_ids(&r), vec![(s(&["cwd", "git", "changes"]), s(&["status", "venv", "context"])), (s(&["char"]), vec![])]);
        assert_eq!(Prefs::default().infer(&b, &one).map(|p| p.rows), Some(Some(Rows::One)));
    }

    #[test]
    fn arrows_cycle_palettes_in_the_list_and_toggles_elsewhere() {
        let mut app = test_app();
        app.sel = PRESET_INFO.iter().position(|p| p.name == "lean").unwrap();
        handle(&mut app, Key::Right);
        assert_eq!((app.focus, app.prefs.palette), (Focus::List, Some("catppuccin-latte")), "lean's default tokyo-night, then the next");
        handle(&mut app, Key::Char('h'));
        handle(&mut app, Key::Char('h'));
        assert_eq!(app.prefs.palette, Some("gruvbox-dark"));
        let pv = DataCache::sample().get(app.base(), 100);
        let lines = screen(&app, &apply(app.base(), &app.prefs), &pv, 100, 40);
        assert!(lines[1].contains("Palette gruvbox-dark (←→ to change)"), "{:?}", lines[1]);
        // With the toggles focused, ←/→ move between toggles again.
        handle(&mut app, Key::Tab);
        handle(&mut app, Key::Right);
        assert_eq!((app.focus, TOGGLES[app.tcur], app.prefs.palette), (Focus::Toggles, Toggle::Rows, Some("gruvbox-dark")));
        // Restarting from a written file restores the palette.
        let bases: Vec<Base> = PRESET_INFO.iter().map(|p| Base::new(p).unwrap()).collect();
        let v = json!({"schema_version": 1, "preset": "classic", "palette": "nord", "palettes": {"x": {}}});
        let (mut prefs, mut sel) = (Prefs::default(), 0);
        let (dropped, extra) = start_from_existing(&bases, &v, &mut prefs, &mut sel, false);
        assert!(dropped.is_empty());
        assert_eq!(extra, [("palettes".to_string(), json!({"x": {}}))]);
        assert_eq!((bases[sel].info.name, prefs.palette), ("classic", Some("nord")));
    }

    /// The loop driven by an injected key stream (Down, Space, Enter) and a
    /// byte buffer as the screen.
    #[test]
    fn loop_with_injected_keys() {
        let mut app = test_app();
        let mut keys = KeyReader::new(&b"\x1b[B \r"[..]);
        let mut screen = Vec::new();
        let o = run_loop(&mut app, &mut keys, &mut screen, &DataCache::sample(), &|| (100, 40)).unwrap();
        assert_eq!(o, Outcome::Write);
        assert_eq!(app.base().info.name, "pastel-powerline");
        assert_eq!(app.prefs.symbols, Some(SymbolTier::Unicode), "Space cycles the icon tier nerd -> unicode");
        let a = apply(app.base(), &app.prefs).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&a.text).unwrap(), json!({"schema_version": 1, "preset": "pastel-powerline", "symbols": "unicode"}));
        let s = String::from_utf8_lossy(&screen);
        assert_eq!(s.matches("\x1b[H").count(), 3, "initial draw plus one per non-final key");
        for p in PRESET_INFO {
            assert!(s.contains(p.name), "{} listed", p.name);
        }
        assert!(s.contains("Icons: unicode*") && s.contains("Sample data"));
        // End of input cancels.
        let mut keys = KeyReader::new(&b"j"[..]);
        assert_eq!(run_loop(&mut app, &mut keys, &mut Vec::new(), &DataCache::sample(), &|| (80, 24)).unwrap(), Outcome::Cancel);
    }

    #[test]
    fn screen_fits_small_terminals() {
        let mut app = test_app();
        app.sel = 12;
        for (cols, height) in [(80, 24), (60, 12), (40, 8), (120, 50)] {
            let applied = apply(app.base(), &app.prefs);
            let pv = DataCache::sample().get(app.base(), cols);
            let lines = screen(&app, &applied, &pv, cols, height);
            assert!(lines.len() <= height, "{cols}x{height}: {}", lines.len());
            assert!(lines.iter().any(|l| l.contains(app.base().info.name)) || height < 10, "{cols}x{height}: selection visible");
            if cols >= 80 {
                for t in TOGGLES {
                    assert!(lines.iter().any(|l| l.contains(t.label())), "{cols}x{height}: {t:?} visible");
                }
            }
        }
    }

    #[test]
    fn existing_layout_restores_the_toggles() {
        for name in ["pastel-powerline", "tokyo-night", "pure-preset", "plain", "jetpack", "bracketed-segments"] {
            let b = base(name);
            for r in [None, Some(Rows::One), Some(Rows::Two)] {
                for bits in 0..8u8 {
                    let p = Prefs {
                        symbols: None,
                        palette: None,
                        rows: r,
                        add_newline: None,
                        time: bits & 1 == 0,
                        langs: bits & 2 == 0,
                        duration_jobs: bits & 4 == 0,
                    };
                    let rows = b.transform_rows(&p);
                    let q = Prefs::default().infer(&b, &rows).unwrap_or_else(|| panic!("{name} {p:?}"));
                    assert_eq!(b.transform_rows(&q), rows, "{name} {p:?}");
                }
            }
        }
        let bases: Vec<Base> = PRESET_INFO.iter().map(|p| Base::new(p).unwrap()).collect();
        let b = base("tokyo-night");
        let written = Prefs { symbols: Some(SymbolTier::Ascii), rows: Some(Rows::One), time: false, ..Prefs::default() };
        let mut v: Value = serde_json::from_str(&apply(&b, &written).unwrap().text).unwrap();
        v["modules"] = json!({"cwd": {"truncation_length": 5}});
        let (mut prefs, mut sel) = (Prefs::default(), 0);
        let (dropped, extra) = start_from_existing(&bases, &v, &mut prefs, &mut sel, false);
        assert_eq!(bases[sel].info.name, "tokyo-night");
        assert_eq!(prefs, written);
        assert!(dropped.is_empty());
        assert_eq!(extra, [("modules".to_string(), json!({"cwd": {"truncation_length": 5}}))]);
        // The kept keys go after the managed ones and validate with them.
        let a = apply_with(&b, &prefs, extra.clone()).unwrap();
        let w: Value = serde_json::from_str(&a.text).unwrap();
        assert_eq!(w["modules"], json!({"cwd": {"truncation_length": 5}}));
        assert!(a.text.find("\"preset\"").unwrap() < a.text.find("\"modules\"").unwrap());
        assert!(a.user.summary().ends_with(" preserved=modules"), "{}", a.user.summary());
        // A key the schema rejects fails the write instead of being dropped.
        assert!(apply_with(&b, &prefs, vec![("add_newline".into(), json!("yes"))]).is_err());
        let a = apply_with(&b, &prefs, vec![("add_newline".into(), json!(true))]).unwrap();
        assert!(a.cfg.file.add_newline == Some(true));
        // A hand-edited layout cannot be reproduced: it is reported as not kept.
        v["layout"]["rows"][0]["left"] = json!(["cwd", "char"]);
        let (mut prefs, mut sel) = (Prefs::default(), 0);
        let (dropped, extra) = start_from_existing(&bases, &v, &mut prefs, &mut sel, false);
        assert_eq!(dropped, ["layout"]);
        assert_eq!(extra.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["modules"]);
        assert_eq!(prefs.symbols, Some(SymbolTier::Ascii));
    }

    #[test]
    fn newline_toggle_updates_preview_without_changing_layout() {
        for info in PRESET_INFO {
            let b = Base::new(info).unwrap();
            let mut prefs = Prefs { add_newline: Some(false), ..Prefs::default() };
            assert!(prefs.toggle(&b, Toggle::Newline));
            assert_eq!(b.state(&prefs, Toggle::Newline), State::On(true));
            let on = apply(&b, &prefs).unwrap();
            assert!(on.user.rows.is_none(), "{}: newline is independent of layout", info.name);
            let rendered = render::render_preview(&on.cfg, &sample_data(&on.cfg, 80), sample_now());
            assert!(rendered.prompt.left.first().is_some_and(String::is_empty), "{}", info.name);
            assert_eq!(user_json(&b, &prefs)["add_newline"], true);
            assert!(prefs.toggle(&b, Toggle::Newline));
            let off = apply(&b, &prefs).unwrap();
            let rendered = render::render_preview(&off.cfg, &sample_data(&off.cfg, 80), sample_now());
            assert!(!rendered.prompt.left.first().is_some_and(String::is_empty), "{}", info.name);
            assert_eq!(user_json(&b, &prefs)["add_newline"], false);
        }
        let mut app = test_app();
        app.sel = app.bases.iter().position(|b| b.info.name == "pastel-powerline").unwrap();
        app.prefs.add_newline = Some(true);
        let pv = DataCache::sample().get(app.base(), 80);
        let lines = screen(&app, &apply(app.base(), &app.prefs), &pv, 80, 24);
        assert!(lines.iter().all(|l| !l.contains('\n')), "each screen entry must occupy exactly one physical line");
        assert!(lines.iter().any(|l| l.contains("Blank line: On")));
        assert!(lines.iter().any(|l| l.contains("Duration/jobs")), "wrapped controls remain visible");
    }

    #[test]
    fn existing_newline_is_restored_with_layout_and_cli_override_wins() {
        let bases: Vec<Base> = PRESET_INFO.iter().map(|p| Base::new(p).unwrap()).collect();
        let b = base("tokyo-night");
        let original = Prefs { rows: Some(Rows::One), time: false, add_newline: Some(true), ..Prefs::default() };
        let v = user_json(&b, &original);
        let (mut prefs, mut sel) = (Prefs::default(), 0);
        let (dropped, extra) = start_from_existing(&bases, &v, &mut prefs, &mut sel, false);
        assert!(dropped.is_empty() && extra.is_empty());
        assert_eq!(prefs, original);
        // Only the blank-line flag was supplied; restore the existing layout too.
        let mut prefs = Prefs { add_newline: Some(false), ..Prefs::default() };
        start_from_existing(&bases, &v, &mut prefs, &mut sel, false);
        assert_eq!(prefs, Prefs { add_newline: Some(false), ..original });
        assert_eq!(base("pastel-powerline").state(&prefs, Toggle::Newline), State::On(false));
        // A previously invalid/null value can be replaced by the interactive toggle.
        let extra = vec![("add_newline".into(), json!("yes"))];
        assert_eq!(apply_with(&b, &prefs, extra).unwrap().cfg.file.add_newline, Some(false));
    }

    #[test]
    fn here_data_waits_at_most_300ms_then_falls_back_to_sample() {
        let b = base("plain");
        let cache = DataCache::here(PathBuf::from("/"));
        // Simulate a fetch that is still running.
        cache.shared.0.lock().unwrap().insert("plain", Entry { started: Instant::now(), data: None });
        let t = Instant::now();
        let pv = cache.get(&b, 80);
        let waited = t.elapsed();
        assert!(pv.pending && !pv.real);
        assert!(waited >= Duration::from_millis(250) && waited < Duration::from_secs(2), "{waited:?}");
        // Asking again does not wait another 300 ms.
        let t = Instant::now();
        assert!(cache.get(&b, 80).pending);
        assert!(t.elapsed() < Duration::from_millis(100));
        let mut app = test_app();
        let lines = screen(&app, &apply(app.base(), &app.prefs), &pv, 100, 30);
        assert!(lines[1].contains("Sample data") && lines[1].contains("300 ms"), "{:?}", lines[1]);
        // The late result is used once it arrives, at the current width.
        cache.shared.0.lock().unwrap().get_mut("plain").unwrap().data = Some(sample_data(&b.cfg, 80));
        assert!(cache.ready("plain"));
        let pv = cache.get(&b, 120);
        assert!(pv.real && !pv.pending);
        assert_eq!(pv.data.request.cols, 120);
        app.sel = 0;
        assert!(screen(&app, &apply(app.base(), &app.prefs), &pv, 100, 30)[1].contains("Current directory ~/x"));
    }

    #[test]
    fn symlinked_target_stays_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles-config.json");
        std::fs::write(&real, "{\"schema_version\": 1}\n").unwrap();
        let link = dir.path().join("config.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let a = apply(&base("jetpack"), &Prefs::default()).unwrap();
        let store = Store { config_path: link.clone(), history_dir: Some(dir.path().join("history")) };
        let expect = store.current().unwrap().revision;
        let r = store.apply(a.text.as_bytes(), Some(&expect), false).unwrap();
        assert!(r.changed && r.history_id.is_some());
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), a.text);
        // A stale revision is refused, the file untouched.
        assert!(store.apply(b"{\"schema_version\": 1}", Some(&expect), false).is_err());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), a.text);
    }
}
