//! `tildy import starship`: map a starship.toml to a candidate tildy config. Owner: M1b task D
//! (preset-porting improvements: M1b-presets task).
//!
//! The result is fully explicit (no `preset`). Modules with a native tildy type
//! are mapped with their symbol, style and options; everything else becomes a
//! `{"type":"starship","module":NAME}` bridge (or, with
//! [`ImportOptions::commands`], a `command` module for the languages in
//! [`COMMAND_LANGS`]). Anything that cannot be carried over is listed in
//! `unsupported` with a reason.
//!
//! Each module section is read over Starship's own defaults
//! (`starship_defaults.toml`), so a module the file does not configure looks
//! like it does in Starship. Rows:
//!
//! - a row whose format has powerline separators (`[](fg:A bg:B)`) becomes a
//!   `powerline` row (separators are U+E0B0..U+E0BF glyphs): module formats are flattened (their padding becomes
//!   tildy's segment padding), a leading `[cap](fg:X)` becomes the row's
//!   `start`, a trailing one its `end`, other literals `text` modules;
//! - other rows stay `plain`: module formats are kept verbatim, with
//!   `[...](style)` spans (Starship spacing included; tildy's plain join adds
//!   no space after a module that already ends with one). When some module
//!   does not end with whitespace the row gets `"separator": ""` so modules
//!   abut as in Starship.
//! - `right_format` becomes the `right` side of the last row.

mod format;

use format::{
    ends_ws, has_pua, parse_style, parse_tree, separator_glyph, to_tildy, tokenize, tree_has_spans, tree_has_vars, tree_text,
    tree_unstyled, wrapping_styles, El, StarshipStyle, Tok,
};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use toml::{Table, Value as Toml};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution { Ignored, Manual }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unsupported {
    pub item: String,
    pub reason: String,
    /// Structured report category; legacy unsupported JSON retains its shape.
    #[serde(skip)]
    pub resolution: Resolution,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportResult {
    pub candidate: Value,
    pub unsupported: Vec<Unsupported>,
    /// Modules named in the format but skipped (`disabled = true`, or disabled by
    /// Starship's own default).
    pub skipped: Vec<String>,
}

/// Import options.
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Palette to use instead of the file's `palette` key (e.g. one of the
    /// catppuccin flavors defined under `[palettes]`). `[palettes]` are kept
    /// as tildy `palettes` and colors that name a palette color stay by name;
    /// this sets the candidate's `palette`.
    pub palette: Option<String>,
    /// Map the languages in [`COMMAND_LANGS`] to `command` modules (version
    /// command + detection, styled by the imported config) instead of Starship
    /// bridges, and drop modules that only a bridge could show. Built-in presets
    /// use this: a bridge renders with whatever Starship config the user has.
    pub commands: bool,
}

/// Starship's defaults for the modules mapped here (subset of
/// `starship print-config --default`, Starship 1.24.2).
const STARSHIP_DEFAULTS: &str = include_str!("starship_defaults.toml");

/// Starship's default order, used when the file has no `format` (the modules
/// tildy maps or bridges, in Starship 1.24.2's `$all` order).
const DEFAULT_FORMAT: &str = "$username$hostname$localip$shlvl$directory$git_branch$git_commit$git_state$git_metrics$git_status$docker_context$package$c$cpp$elixir$golang$haskell$java$kotlin$lua$nodejs$php$python$ruby$rust$scala$swift$zig$deno$bun$nix_shell$conda$memory_usage$direnv$sudo$cmd_duration$line_break$jobs$time$status$shell$character";

/// Modules Starship itself leaves disabled unless the config enables them
/// (beyond those with `disabled = true` in `STARSHIP_DEFAULTS`).
const DEFAULT_DISABLED: &[&str] = &[
    "time", "memory_usage", "shell", "status", "os", "localip", "sudo", "git_metrics", "fossil_metrics", "direnv", "cpp", "shlvl",
];

/// Starship's own detection lists for modules we bridge rather than implement,
/// so an imported config never spawns `starship` in unrelated directories.
/// (name, detect_files, detect_extensions, detect_folders)
type BridgeDetect = (&'static str, &'static [&'static str], &'static [&'static str], &'static [&'static str]);
const BRIDGE_DETECT: &[BridgeDetect] = &[
    ("elixir", &["mix.exs"], &[], &[]),
    ("elm", &["elm.json", "elm-package.json", ".elm-version"], &["elm"], &["elm-stuff"]),
    ("haskell", &["stack.yaml", "cabal.project"], &["hs", "cabal", "hs-boot"], &[]),
    ("julia", &["Project.toml", "Manifest.toml"], &["jl"], &[]),
    ("nim", &["nim.cfg"], &["nim", "nims", "nimble"], &[]),
    ("bun", &["bun.lockb", "bun.lock", "bunfig.toml"], &[], &[]),
    ("deno", &["deno.json", "deno.jsonc", "deno.lock", "mod.ts", "mod.js", "deps.ts", "deps.js"], &[], &[]),
    ("swift", &["Package.swift"], &["swift"], &[]),
    ("ruby", &["Gemfile", ".ruby-version"], &["rb"], &[]),
    ("gradle", &["settings.gradle", "settings.gradle.kts", "build.gradle", "build.gradle.kts"], &[], &[]),
    ("kubernetes", &[], &[], &[]),
    ("php", &["composer.json", ".php-version"], &["php"], &[]),
    ("kotlin", &[], &["kt", "kts"], &[]),
    ("scala", &["build.sbt", ".scalaenv", ".sbtenv", "build.sc"], &["scala", "sbt", "sc"], &[".metals"]),
    ("cpp", &[], &["cpp", "cc", "cxx", "c++", "hpp", "hh", "hxx", "h++", "tcc"], &[]),
    ("lua", &[".lua-version"], &["lua"], &["lua"]),
    ("zig", &[], &["zig"], &[]),
    ("dart", &["pubspec.yaml", "pubspec.yml", "pubspec.lock"], &["dart"], &[".dart_tool"]),
    ("pixi", &["pixi.toml"], &[], &[".pixi"]),
];

/// A language shown through a `command` module ([`ImportOptions::commands`]).
pub struct CommandLang {
    pub name: &'static str,
    pub argv: &'static [&'static str],
    /// First capture group = the version.
    pub regex: &'static str,
    pub files: &'static [&'static str],
    pub extensions: &'static [&'static str],
    pub folders: &'static [&'static str],
}

/// Version commands and detection follow Starship's modules of the same name.
pub const COMMAND_LANGS: &[CommandLang] = &[
    CommandLang {
        name: "php",
        argv: &["php", "-nr", "echo PHP_MAJOR_VERSION.'.'.PHP_MINOR_VERSION.'.'.PHP_RELEASE_VERSION;"],
        regex: r"(\d+\.\d+\.\d+)",
        files: &["composer.json", ".php-version"],
        extensions: &["php"],
        folders: &[],
    },
    CommandLang { name: "ruby", argv: &["ruby", "-v"], regex: r"ruby (\d+\.\d+\.\d+)", files: &["Gemfile", ".ruby-version"], extensions: &["rb"], folders: &[] },
    CommandLang { name: "kotlin", argv: &["kotlin", "-version"], regex: r"version (\d+\.\d+\.\d+)", files: &[], extensions: &["kt", "kts"], folders: &[] },
    CommandLang {
        name: "haskell",
        argv: &["ghc", "--numeric-version"],
        regex: r"(\d+(?:\.\d+)+)",
        files: &["stack.yaml", "cabal.project"],
        extensions: &["hs", "cabal", "hs-boot"],
        folders: &[],
    },
    CommandLang { name: "lua", argv: &["lua", "-v"], regex: r"Lua (\d+\.\d+(?:\.\d+)?)", files: &[".lua-version"], extensions: &["lua"], folders: &["lua"] },
    CommandLang { name: "zig", argv: &["zig", "version"], regex: r"(\d+\.\d+\.\d+\S*)", files: &[], extensions: &["zig"], folders: &[] },
    CommandLang { name: "swift", argv: &["swift", "--version"], regex: r"version (\d+\.\d+(?:\.\d+)?)", files: &["Package.swift"], extensions: &["swift"], folders: &[] },
    CommandLang {
        name: "deno",
        argv: &["deno", "-V"],
        regex: r"deno (\d+\.\d+\.\d+)",
        files: &["deno.json", "deno.jsonc", "deno.lock", "mod.ts", "mod.js", "deps.ts", "deps.js"],
        extensions: &[],
        folders: &[],
    },
    CommandLang { name: "bun", argv: &["bun", "--version"], regex: r"(\d+\.\d+\.\d+)", files: &["bun.lockb", "bun.lock", "bunfig.toml"], extensions: &[], folders: &[] },
    CommandLang { name: "elixir", argv: &["elixir", "--version"], regex: r"Elixir (\d+\.\d+\.\d+)", files: &["mix.exs"], extensions: &[], folders: &[] },
    CommandLang {
        name: "scala",
        argv: &["scalac", "-version"],
        regex: r"version (\d+\.\d+\.\d+)",
        files: &["build.sbt", ".scalaenv", ".sbtenv", "build.sc"],
        extensions: &["scala", "sbt", "sc"],
        folders: &[".metals"],
    },
    CommandLang {
        name: "dart",
        argv: &["dart", "--version"],
        regex: r"version: (\d+\.\d+\.\d+)",
        files: &["pubspec.yaml", "pubspec.yml", "pubspec.lock"],
        extensions: &["dart"],
        folders: &[".dart_tool"],
    },
    CommandLang {
        name: "cpp",
        argv: &["c++", "--version"],
        regex: r"version (\d+\.\d+\.\d+)",
        files: &[],
        extensions: &["cpp", "cc", "cxx", "c++", "hpp", "hh", "hxx", "h++", "tcc"],
        folders: &[],
    },
];

/// Native languages: Starship module -> tildy id.
const LANGUAGES: &[(&str, &str)] = &[("nodejs", "node"), ("rust", "rust"), ("golang", "go"), ("java", "java"), ("c", "c")];

/// Starship modules mapped to the native types added in M2 (task E1); the
/// tildy module id is the Starship name. `status` becomes `exit_status`.
const E1_NATIVE: &[&str] = &[
    "package", "git_commit", "git_metrics", "shlvl", "localip", "memory_usage", "sudo", "direnv", "nix_shell", "shell", "status",
];

/// Top-level keys that are settings rather than module sections.
const TOP_KEYS: &[&str] = &[
    "format", "right_format", "add_newline", "scan_timeout", "command_timeout", "continuation_prompt", "palette", "palettes",
    "$schema", "follow_symlinks",
];

/// Starship OS names (`[os.symbols]`) -> `std::env::consts::OS`.
const OS_NAMES: &[(&str, &str)] = &[
    ("Macos", "macos"),
    ("Linux", "linux"),
    ("Windows", "windows"),
    ("FreeBSD", "freebsd"),
    ("OpenBSD", "openbsd"),
    ("NetBSD", "netbsd"),
    ("Android", "android"),
    ("DragonFly", "dragonfly"),
    ("Illumos", "illumos"),
    ("Solaris", "solaris"),
];

pub fn import_starship(src: &str) -> Result<ImportResult, String> {
    import_starship_with(src, &ImportOptions::default())
}

pub fn import_starship_with(src: &str, opts: &ImportOptions) -> Result<ImportResult, String> {
    let root: Table = src.parse::<Table>().map_err(|e| format!("invalid TOML: {e}"))?;
    let mut im = Importer::new(&root, opts)?;
    im.run();
    Ok(ImportResult { candidate: im.candidate(), unsupported: im.notes, skipped: im.skipped })
}

/// How a module is laid out: in a powerline row (flattened, padded segments)
/// or a plain row (verbatim formats).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct RowCtx {
    powerline: bool,
}

/// What happens to a Starship variable in a module format.
enum VarRule {
    Keep(&'static str),
    /// Dropped without a note (empty in practice, or expressed otherwise).
    Silent,
    /// Dropped with a note when the user wrote the format.
    Unknown,
}

/// A converted module format.
#[derive(Default)]
struct Fmt {
    format: Option<String>,
    style: Option<Value>,
    padding: Option<bool>,
    /// A Nerd glyph found as literal text in the format, moved to `symbol`.
    pua_symbol: Option<String>,
}

struct Importer<'a> {
    root: &'a Table,
    defaults: Table,
    opts: ImportOptions,
    palette: BTreeMap<String, String>,
    /// Candidate `palette` and `palettes` (Starship's, kept by name).
    palette_name: Option<String>,
    palettes: Map<String, Value>,
    notes: Vec<Unsupported>,
    skipped: Vec<String>,
    modules: Map<String, Value>,
    /// Starship module (or group) name -> tildy id, or None when skipped.
    resolved: BTreeMap<String, Option<String>>,
    rows: Vec<Value>,
    texts: usize,
    /// Top-level tildy options from Starship's prompt-wide settings
    /// (`add_newline`, `command_timeout_ms`).
    globals: Map<String, Value>,
}

fn get_str<'t>(t: &'t Table, k: &str) -> Option<&'t str> {
    t.get(k).and_then(Toml::as_str)
}
fn get_bool(t: &Table, k: &str) -> Option<bool> {
    t.get(k).and_then(Toml::as_bool)
}
fn get_int(t: &Table, k: &str) -> Option<i64> {
    t.get(k).and_then(Toml::as_integer)
}

/// Strings of a TOML array (non-strings skipped).
fn get_strings(t: &Table, k: &str) -> Option<Vec<String>> {
    t.get(k).and_then(Toml::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
}

/// Collapse runs of spaces, drop empty groups, trim.
fn normalize(s: &str) -> String {
    let s = Regex::new(r" {2,}").unwrap().replace_all(s, " ");
    let s = Regex::new(r"\(\s*\)").unwrap().replace_all(&s, "");
    s.trim().to_string()
}

/// Formats that differ only in spaces are treated as equal (tildy's defaults
/// put spaces inside groups, Starship's around them).
fn canon(s: &str) -> String {
    s.chars().filter(|c| *c != ' ').collect()
}

/// Leading and trailing whitespace of `s`.
fn edges(s: &str) -> (&str, &str) {
    let lead = &s[..s.len() - s.trim_start().len()];
    let trail = &s[s.trim_end().len()..];
    (lead, trail)
}

/// Fallback words for a symbol in the unicode and ascii tiers (unknown keys:
/// the module name).
fn fallbacks(key: &str) -> (String, String) {
    let (u, a): (&str, &str) = match key {
        "username" => ("", "u"),
        "git" | "git_branch" => ("\u{2387}", "git"),
        "nodejs" => ("\u{2b22}", "node"),
        "golang" => ("go", "go"),
        "python" => ("py", "py"),
        "docker_context" => ("docker", "docker"),
        "jobs" => ("\u{2726}", "*"),
        "character" => ("\u{276f}", ">"),
        "hostname" => ("", "ssh"),
        "ruby" => ("rb", "rb"),
        "package" => ("📦", "pkg"),
        "shlvl" => ("↕", "lvl"),
        "memory_usage" => ("🐏", "mem"),
        "sudo" => ("🧙", "sudo"),
        "nix_shell" => ("❄", "nix"),
        "status" => ("✗", "x"),
        "tag" => ("🏷", "tag"),
        "os" | "time" | "cmd_duration" | "directory" => ("", ""),
        other => (other, other),
    };
    (u.to_string(), a.to_string())
}

/// Drop and rename variables per `rule`; groups left without variables and
/// empty spans disappear (Starship never shows them). Unknown variables are
/// collected in `dropped`.
fn prune(els: &[El], rule: &dyn Fn(&str) -> VarRule, dropped: &mut Vec<String>) -> Vec<El> {
    let mut out = vec![];
    for e in els {
        match e {
            El::Var(v) => match rule(v) {
                VarRule::Keep(n) => out.push(El::Var(n.to_string())),
                VarRule::Silent => {}
                VarRule::Unknown => dropped.push(v.clone()),
            },
            El::Group(g) => {
                let g2 = prune(g, rule, dropped);
                if tree_has_vars(&g2) {
                    out.push(El::Group(g2));
                }
            }
            El::Span(inner, st) => {
                let i2 = prune(inner, rule, dropped);
                if !i2.is_empty() {
                    out.push(El::Span(i2, st.clone()));
                }
            }
            El::Lit(t) => out.push(El::Lit(t.clone())),
        }
    }
    out
}

/// A text with Nerd glyphs as a tiered symbol: the glyphs only in the nerd
/// tier; elsewhere the text without them (empty when nothing else is left).
fn tier_text(s: &str) -> Value {
    if !has_pua(s) {
        return json!(s);
    }
    let rest: String = s.chars().filter(|c| !has_pua(&c.to_string())).collect();
    let rest = if rest.trim().is_empty() { String::new() } else { normalize_spaces(&rest) };
    if rest.is_empty() {
        json!({ "nerd": s })
    } else {
        json!({ "nerd": s, "ascii": rest })
    }
}

fn normalize_spaces(s: &str) -> String {
    Regex::new(r" {2,}").unwrap().replace_all(s, " ").to_string()
}

/// Resolve `$name` references inside a Starship style string: `$style` is
/// `base`, other names are keys of the module section.
fn expand_style_vars(raw: &str, base: Option<&str>, sec: &Table) -> String {
    raw.split_whitespace()
        .map(|t| match t.strip_prefix('$') {
            Some("style") => base.unwrap_or("").to_string(),
            Some(k) => get_str(sec, k).unwrap_or("").to_string(),
            None => t.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl<'a> Importer<'a> {
    fn new(root: &'a Table, opts: &ImportOptions) -> Result<Self, String> {
        let defaults: Table = STARSHIP_DEFAULTS.parse().expect("starship_defaults.toml is valid TOML");
        let mut palette = BTreeMap::new();
        let name = opts.palette.as_deref().or_else(|| get_str(root, "palette"));
        if let Some(name) = name {
            match root.get("palettes").and_then(|p| p.get(name)) {
                Some(Toml::Table(p)) => {
                    for (k, v) in p {
                        if let Some(s) = v.as_str() {
                            palette.insert(k.clone(), s.to_string());
                        }
                    }
                }
                _ if opts.palette.is_some() => return Err(format!("palette '{name}' is not defined under [palettes]")),
                _ => {}
            }
        }
        // `[palettes]` become tildy `palettes` (colors converted; names tildy
        // cannot use are resolved where they are referenced, see `format::color`).
        let mut palettes = Map::new();
        let mut notes = vec![];
        for (pname, t) in root.get("palettes").and_then(Toml::as_table).into_iter().flatten() {
            let Some(t) = t.as_table() else { continue };
            if !crate::config::palette::valid_palette_name(pname) {
                notes.push(Unsupported { resolution: Resolution::Ignored, item: format!("palettes.{pname}"), reason: "palette name is not [A-Za-z0-9_-]; colors resolved instead".into() });
                continue;
            }
            let mut colors = Map::new();
            for (k, v) in t {
                match v.as_str().and_then(|s| format::palette_color(k, s)) {
                    Some(c) => {
                        colors.insert(k.clone(), json!(c));
                    }
                    None => notes.push(Unsupported {
                        resolution: Resolution::Ignored,
                        item: format!("palettes.{pname}.{k}"),
                        reason: "not a tildy palette color (name or value); resolved where referenced".into(),
                    }),
                }
            }
            palettes.insert(pname.clone(), Value::Object(colors));
        }
        let palette_name = name.filter(|n| palettes.contains_key(*n)).map(str::to_string);
        Ok(Importer {
            root,
            defaults,
            opts: opts.clone(),
            palette,
            palette_name,
            palettes,
            notes,
            skipped: vec![],
            modules: Map::new(),
            resolved: BTreeMap::new(),
            rows: vec![],
            texts: 0,
            globals: Map::new(),
        })
    }

    fn note(&mut self, item: impl Into<String>, reason: impl Into<String>) {
        self.note_with(item, reason, Resolution::Ignored);
    }

    fn manual(&mut self, item: impl Into<String>, reason: impl Into<String>) {
        self.note_with(item, reason, Resolution::Manual);
    }

    fn note_with(&mut self, item: impl Into<String>, reason: impl Into<String>, resolution: Resolution) {
        let n = Unsupported { item: item.into(), reason: reason.into(), resolution };
        if !self.notes.contains(&n) {
            self.notes.push(n);
        }
    }

    /// The section as written in the file.
    fn user(&self, name: &str) -> Option<&'a Table> {
        match self.root.get(name) {
            Some(Toml::Table(t)) => Some(t),
            _ => None,
        }
    }

    /// The section over Starship's defaults (nested tables merged one level).
    fn section(&self, name: &str) -> Table {
        let mut t = match self.defaults.get(name) {
            Some(Toml::Table(d)) => d.clone(),
            _ => Table::new(),
        };
        if let Some(u) = self.user(name) {
            for (k, v) in u {
                match (t.get_mut(k), v) {
                    (Some(Toml::Table(dt)), Toml::Table(ut)) => {
                        for (k2, v2) in ut {
                            dt.insert(k2.clone(), v2.clone());
                        }
                    }
                    _ => {
                        t.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        t
    }

    fn user_has(&self, name: &str, key: &str) -> bool {
        self.user(name).is_some_and(|u| u.contains_key(key))
    }

    fn disabled(&self, name: &str) -> bool {
        let explicit = self.user(name).and_then(|s| get_bool(s, "disabled"));
        let default = match self.defaults.get(name) {
            Some(Toml::Table(d)) => get_bool(d, "disabled"),
            _ => None,
        };
        explicit.or(default).unwrap_or_else(|| DEFAULT_DISABLED.contains(&name))
    }

    fn candidate(&self) -> Value {
        let mut rows = self.rows.clone();
        if rows.is_empty() {
            rows.push(json!({ "left": [] }));
        }
        let mut c = json!({
            "schema_version": 1,
            "symbols": "auto",
            "modules": Value::Object(self.modules.clone()),
            "layout": { "rows": rows },
            "history": { "mode": "compact" },
        });
        if !self.palettes.is_empty() {
            c["palettes"] = Value::Object(self.palettes.clone());
        }
        if let Some(p) = &self.palette_name {
            c["palette"] = json!(p);
        }
        for (k, v) in &self.globals {
            c[k] = v.clone();
        }
        c
    }

    // ------------------------------------------------------------ styles

    /// Parse a style string (palette colors resolved); unsupported tokens are noted.
    fn parse_style_noted(&mut self, owner: &str, raw: &str) -> StarshipStyle {
        let st = parse_style(raw, &self.palette);
        for u in &st.unsupported {
            self.note(format!("{owner}.style"), format!("style token '{u}' is not supported; ignored"));
        }
        st
    }

    fn style_value(&mut self, owner: &str, raw: &str) -> Option<Value> {
        self.parse_style_noted(owner, raw).to_json()
    }

    // ------------------------------------------------------------ symbols

    /// A Starship symbol (itself a format string: escapes, spans, `${count}`)
    /// as a tildy symbol. `trim` drops surrounding whitespace (powerline rows
    /// pad segments themselves). Nerd glyphs get unicode/ascii fallbacks;
    /// with `tier_unicode`, other non-ASCII symbols get an ascii fallback.
    fn symbol(&mut self, key: &str, raw: &str, trim: bool, tier_unicode: bool) -> Value {
        let els = parse_tree(raw);
        let plain = tree_text(&els);
        let s = if tree_has_spans(&els) || tree_has_vars(&els) {
            let palette = self.palette.clone();
            let mut bad = vec![];
            let out = to_tildy(&els, &mut |v| Some(v.to_string()), &mut |st| {
                let p = parse_style(st, &palette);
                bad.extend(p.unsupported.clone());
                Some(p.to_spec())
            });
            for b in bad {
                self.note(format!("{key}.symbol"), format!("style token '{b}' is not supported; ignored"));
            }
            if tree_has_spans(&els) {
                out
            } else {
                // `⇡${count}`: variables are filled in by the renderer; the
                // text is literal otherwise.
                tree_text_with_vars(&els)
            }
        } else {
            plain.clone()
        };
        let s = if trim { s.trim().to_string() } else { s };
        let (_, trail) = edges(&s);
        let trail = trail.to_string();
        if has_pua(&plain) {
            let (u, a) = fallbacks(key);
            let with = |w: &str| if w.is_empty() || trim { w.to_string() } else { format!("{w}{trail}") };
            let mut o = Map::new();
            o.insert("nerd".into(), json!(s));
            o.insert("unicode".into(), json!(with(&u)));
            o.insert("ascii".into(), json!(with(&a)));
            return Value::Object(o);
        }
        if tier_unicode && !plain.is_ascii() {
            let (_, a) = fallbacks(key);
            let a = if a.is_empty() || trim { a } else { format!("{a}{trail}") };
            return json!({ "unicode": s, "ascii": a });
        }
        json!(s)
    }

    // ------------------------------------------------------------ formats

    /// Convert a module format (see the module docs for the two row kinds).
    /// `style_raw` is the module's `style` (what `$style` means), `defaults`
    /// the tildy formats equivalent to "no format".
    fn convert(&mut self, owner: &str, sec: &Table, style_raw: Option<&str>, rule: &dyn Fn(&str) -> VarRule, defaults: &[&str], ctx: RowCtx) -> Fmt {
        let Some(raw) = get_str(sec, "format") else {
            let style = style_raw.and_then(|s| self.style_value(owner, s));
            return Fmt { style, ..Fmt::default() };
        };
        let user_fmt = self.user_has(owner, "format");
        let mut dropped = vec![];
        let els = prune(&parse_tree(raw), rule, &mut dropped);
        let mut var = |v: &str| Some(v.to_string());
        let mut out = Fmt::default();
        if ctx.powerline {
            // Starship styles are not inherited: the innermost span wrapping the
            // whole format decides; a format without spans is unstyled.
            let chain = wrapping_styles(&els);
            let eff = match chain.last() {
                Some(s) => Some(expand_style_vars(s, style_raw, sec)),
                None if tree_has_spans(&els) => style_raw.map(str::to_string),
                None => None,
            };
            let mut flat = to_tildy(&tree_unstyled(&els), &mut var, &mut |_| None);
            out.pua_symbol = extract_pua(&mut flat);
            let padded = flat.starts_with(' ') && flat.ends_with(' ');
            let norm = normalize(&flat);
            if padded || norm.is_empty() {
                out.format = (!defaults.iter().any(|d| canon(d) == canon(&norm))).then_some(norm);
            } else {
                out.format = (!defaults.contains(&flat.as_str())).then_some(flat);
                out.padding = Some(false);
            }
            out.style = eff.and_then(|s| self.style_value(owner, &s));
        } else {
            // Verbatim with spans; a single span (whitespace around it aside)
            // without background/underline becomes the module style.
            let mut els = els;
            let core: Vec<usize> =
                (0..els.len()).filter(|&i| !matches!(&els[i], El::Lit(t) if t.trim().is_empty())).collect();
            if let [i] = core.as_slice() {
                let span = match &els[*i] {
                    El::Span(inner, st) => Some((inner.clone(), st.clone())),
                    _ => None,
                };
                if let Some((inner, st)) = span {
                    let expanded = expand_style_vars(&st, style_raw, sec);
                    let p = parse_style(&expanded, &self.palette);
                    if p.bg.is_none() && !p.underline {
                        out.style = self.style_value(owner, &expanded);
                        els.splice(*i..*i + 1, inner);
                    }
                }
            }
            let palette = self.palette.clone();
            let mut bad = vec![];
            let mut s = to_tildy(&els, &mut var, &mut |st| {
                let p = parse_style(&expand_style_vars(st, style_raw, sec), &palette);
                bad.extend(p.unsupported.clone());
                Some(p.to_spec())
            });
            for b in bad {
                self.note(format!("{owner}.style"), format!("style token '{b}' is not supported; ignored"));
            }
            out.pua_symbol = extract_pua(&mut s);
            out.format = (!defaults.contains(&s.as_str())).then_some(s);
        }
        if user_fmt {
            for d in dropped {
                self.manual(format!("{owner}.format"), format!("variable ${d} is not available in tildy; dropped"));
            }
        }
        out
    }

    fn check_keys(&mut self, owner: &str, allowed: &[&str]) {
        let Some(s) = self.user(owner) else { return };
        let keys: Vec<String> = s.keys().cloned().collect();
        for k in keys {
            if k != "disabled" && !allowed.contains(&k.as_str()) {
                self.note(format!("{owner}.{k}"), "option not supported by tildy; ignored");
            }
        }
    }

    /// Apply a converted format to a module object.
    fn apply_fmt(&mut self, m: &mut Map<String, Value>, f: Fmt, sym_key: &str) {
        if let Some(fmt) = f.format {
            m.insert("format".into(), json!(fmt));
        }
        if let Some(st) = f.style {
            m.insert("style".into(), st);
        }
        if let Some(p) = f.padding {
            m.insert("padding".into(), json!(p));
        }
        if let Some(g) = f.pua_symbol {
            if !m.contains_key("symbol") {
                let (u, a) = if matches!(sym_key, "time" | "cmd_duration" | "directory" | "os") {
                    (String::new(), String::new())
                } else {
                    fallbacks(sym_key)
                };
                let mut o = Map::new();
                o.insert("nerd".into(), json!(g));
                o.insert("unicode".into(), json!(u));
                o.insert("ascii".into(), json!(a));
                m.insert("symbol".into(), Value::Object(o));
            }
        }
    }

    // ------------------------------------------------------------ driver

    fn run(&mut self) {
        for (k, v) in self.root {
            if TOP_KEYS.contains(&k.as_str()) || matches!(v, Toml::Table(_)) {
                continue;
            }
            self.note(k.clone(), "unknown top-level key");
        }
        // Preserve an explicit false now that both defaults are true.
        self.globals.insert("add_newline".into(), json!(!matches!(self.root.get("add_newline"), Some(Toml::Boolean(false)))));
        if let Some(v) = self.root.get("command_timeout") {
            match v.as_integer().filter(|n| (1..=60_000).contains(n)) {
                Some(n) => {
                    self.globals.insert("command_timeout_ms".into(), json!(n));
                }
                None => self.note("command_timeout", "expected milliseconds from 1 to 60000; ignored"),
            }
        }
        if self.root.contains_key("continuation_prompt") {
            self.note("continuation_prompt", "tildy does not draw PS2 (Zsh's own PS2 shows); ignored");
        }
        if self.root.contains_key("scan_timeout") {
            self.note("scan_timeout", "tildy scans at most 500 directory entries instead of a time budget; ignored");
        }
        if matches!(self.root.get("follow_symlinks"), Some(Toml::Boolean(false))) {
            self.note("follow_symlinks", "tildy always follows symlinks when detecting folders; ignored");
        }
        let fmt = match get_str(self.root, "format") {
            Some(f) => f.to_string(),
            None => {
                self.note("format", "not set; using an approximation of Starship's default module order");
                DEFAULT_FORMAT.to_string()
            }
        };
        let mut fnotes = vec![];
        let toks = tokenize(&fmt, &mut fnotes);
        let right_toks = match get_str(self.root, "right_format") {
            Some(r) if !r.trim().is_empty() => tokenize(r, &mut fnotes),
            _ => vec![],
        };
        for (i, r) in fnotes {
            self.manual(i, r);
        }
        // Split into rows at newlines and enabled `$line_break`s.
        let line_break = !self.disabled("line_break");
        let mut rows_toks: Vec<Vec<Tok>> = vec![vec![]];
        for t in toks {
            match &t {
                Tok::Newline => rows_toks.push(vec![]),
                Tok::Module(n) if n == "line_break" => {
                    if line_break {
                        rows_toks.push(vec![]);
                    }
                }
                _ => rows_toks.last_mut().expect("non-empty").push(t),
            }
        }
        let n = rows_toks.len();
        let mut placed: BTreeSet<String> = BTreeSet::new();
        let mut rows = vec![];
        for (i, rt) in rows_toks.into_iter().enumerate() {
            let right = if i + 1 == n { right_toks.clone() } else { vec![] };
            if let Some(r) = self.build_row(&rt, &right, &mut placed) {
                rows.push(r);
            }
        }
        if rows.is_empty() {
            self.note("format", "no visible modules");
        }
        self.rows = rows;
    }

    /// Module names of a token list (`$all` expanded).
    fn module_names(toks: &[Tok]) -> Vec<String> {
        let mut out = vec![];
        for t in toks {
            if let Tok::Module(n) = t {
                if n == "all" {
                    let mut sub = vec![];
                    for t in tokenize(DEFAULT_FORMAT, &mut sub) {
                        if let Tok::Module(n) = t {
                            out.push(n);
                        }
                    }
                } else {
                    out.push(n.clone());
                }
            }
        }
        out
    }

    /// Is the module built natively (its own format decides the spacing)?
    fn native(&self, name: &str) -> bool {
        matches!(
            name,
            "username" | "hostname" | "directory" | "git_branch" | "git_status" | "python" | "docker_context" | "time" | "cmd_duration"
                | "jobs" | "os" | "conda"
        ) || E1_NATIVE.contains(&name)
            || LANGUAGES.iter().any(|(n, _)| *n == name)
            || (self.opts.commands && COMMAND_LANGS.iter().any(|c| c.name == name))
    }

    fn build_row(&mut self, toks: &[Tok], right: &[Tok], placed: &mut BTreeSet<String>) -> Option<Value> {
        let powerline = toks.iter().any(|t| match t {
            Tok::Group { text, style } => {
                let st = parse_style(style, &self.palette);
                separator_glyph(text).is_some() && st.fg.is_some() && st.bg.is_some()
            }
            _ => false,
        });
        let ctx = RowCtx { powerline };
        // Plain rows: modules abut (`separator: ""`) unless every module's
        // output ends with whitespace (then tildy's join adds nothing more).
        let mut names = Self::module_names(toks);
        names.extend(Self::module_names(right));
        let abut = !powerline
            && names.iter().any(|n| {
                if self.disabled(n) || !self.native(n) {
                    return false;
                }
                let sec = self.section(n);
                get_str(&sec, "format").is_some_and(|f| !ends_ws(&parse_tree(f)))
            });

        let mut left: Vec<String> = vec![];
        let mut sep: Option<char> = None;
        let mut start = String::new();
        let mut start_style: Option<String> = None;
        let mut end: Option<(String, usize)> = None;
        for tok in toks {
            match tok {
                Tok::Newline => {}
                Tok::Module(name) => {
                    for n in Self::module_names(std::slice::from_ref(tok)) {
                        if n == "line_break" { continue; }
                        if let Some(id) = self.resolve(&n, ctx) {
                            if placed.insert(id.clone()) {
                                left.push(id);
                            }
                        }
                    }
                    if name == "all" {
                        self.note("format: $all", "expanded to the modules tildy knows, in Starship's default order");
                    }
                }
                Tok::Group { text, style } => {
                    let st = parse_style(style, &self.palette);
                    let txt = tree_text(&parse_tree(text));
                    if powerline {
                        match separator_glyph(text) {
                            Some(g) if st.fg.is_some() && st.bg.is_some() => {
                                if sep.is_none() {
                                    sep = Some(g);
                                }
                                continue;
                            }
                            _ => {}
                        }
                        if st.bg.is_none() && left.is_empty() {
                            start.push_str(&txt);
                            start_style = start_style.or(Some(style.clone()));
                            continue;
                        }
                        if st.bg.is_none() && separator_glyph(text).is_some() && end.is_none() {
                            end = Some((txt, left.len()));
                            continue;
                        }
                    }
                    if txt.is_empty() {
                        continue;
                    }
                    let id = self.text_module(text, Some(style));
                    left.push(id);
                }
                Tok::Literal(t) => {
                    if !t.trim().is_empty() || abut {
                        let id = self.text_module(&format::escape_template(t), None);
                        left.push(id);
                    }
                }
            }
        }
        let mut right_ids = vec![];
        for tok in right {
            match tok {
                Tok::Module(_) => {
                    for n in Self::module_names(std::slice::from_ref(tok)) {
                        if matches!(n.as_str(), "line_break" | "fill") {
                            continue;
                        }
                        if let Some(id) = self.resolve(&n, RowCtx { powerline: false }) {
                            if placed.insert(id.clone()) {
                                right_ids.push(id);
                            }
                        }
                    }
                }
                Tok::Group { text, style } if !tree_text(&parse_tree(text)).is_empty() => {
                    let id = self.text_module(text, Some(style));
                    right_ids.push(id);
                }
                Tok::Literal(t) if !t.trim().is_empty() => {
                    let id = self.text_module(&format::escape_template(t), None);
                    right_ids.push(id);
                }
                _ => {}
            }
        }
        if left.is_empty() && right_ids.is_empty() {
            return None;
        }
        let mut row = Map::new();
        if powerline {
            let g = sep.unwrap_or('\u{e0b0}');
            row.insert("style".into(), json!("powerline"));
            row.insert("separator".into(), json!({ "nerd": g.to_string(), "ascii": ">" }));
            if !start.is_empty() {
                let mut o = Map::new();
                o.insert("nerd".into(), json!(start));
                o.insert("unicode".into(), json!(if has_pua(&start) { "" } else { start.as_str() }));
                o.insert("ascii".into(), json!(if start.is_ascii() { start.as_str() } else { "" }));
                row.insert("start".into(), Value::Object(o));
            }
            if let Some((e, at)) = end {
                // Spaces after the cap matter only when something follows it.
                let e = if at < left.len() { e } else { e.trim_end().to_string() };
                if e != g.to_string() {
                    let (_, trail) = edges(&e);
                    row.insert("end".into(), json!({ "nerd": e.clone(), "ascii": format!(">{trail}") }));
                }
            }
        } else if abut {
            row.insert("separator".into(), json!(""));
        }
        row.insert("left".into(), json!(left));
        if !right_ids.is_empty() {
            row.insert("right".into(), json!(right_ids));
        }
        Some(Value::Object(row))
    }

    /// A `text` module for a literal `[text](style)` (or bare text).
    fn text_module(&mut self, text: &str, style: Option<&str>) -> String {
        let els = parse_tree(text);
        let lit = if tree_has_spans(&els) {
            let palette = self.palette.clone();
            to_tildy(&els, &mut |_| None, &mut |st| Some(parse_style(st, &palette).to_spec()))
        } else {
            tree_text(&els)
        };
        self.texts += 1;
        let id = format!("text{}", self.texts);
        let mut m = Map::new();
        m.insert("type".into(), json!("text"));
        m.insert("text".into(), tier_text(&lit));
        if let Some(st) = style.and_then(|s| self.style_value(&id, s)) {
            m.insert("style".into(), st);
        }
        self.modules.insert(id.clone(), Value::Object(m));
        id
    }

    // ------------------------------------------------------------ modules

    /// Map a Starship module name to a tildy id, creating the module on first use.
    fn resolve(&mut self, name: &str, ctx: RowCtx) -> Option<String> {
        if name == "custom" || name.starts_with("custom.") {
            self.manual(format!("format: ${name}"), "custom commands are not imported; add command modules by hand");
            return None;
        }
        if name.contains('.') {
            self.manual(format!("format: ${name}"), "named modules need a manually configured module");
            return None;
        }
        let group = if matches!(name, "git_branch" | "git_status" | "git_state") { "git" } else { name };
        if let Some(r) = self.resolved.get(group) {
            return r.clone();
        }
        let r = self.build(group, ctx);
        self.resolved.insert(group.to_string(), r.clone());
        r
    }

    fn skip(&mut self, name: &str) -> Option<String> {
        self.skipped.push(name.to_string());
        None
    }

    fn put(&mut self, id: &str, v: Value) -> Option<String> {
        self.modules.insert(id.to_string(), v);
        Some(id.to_string())
    }

    fn build(&mut self, name: &str, ctx: RowCtx) -> Option<String> {
        match name {
            "git" => {
                let (bd, sd) = (self.disabled("git_branch"), self.disabled("git_status"));
                if bd && sd {
                    return self.skip("git_branch, git_status");
                }
                self.build_git(bd, sd, ctx)
            }
            "line_break" => None,
            _ if self.disabled(name) => self.skip(name),
            "username" => self.build_username(ctx),
            "directory" => self.build_directory(ctx),
            "python" => self.build_python(ctx),
            "docker_context" => self.build_docker(ctx),
            "time" => self.build_time(ctx),
            "character" => self.build_character(ctx),
            "cmd_duration" => self.build_duration(ctx),
            "hostname" => self.build_hostname(ctx),
            "jobs" => self.build_jobs(ctx),
            "os" => self.build_os(ctx),
            "fill" => self.build_fill(),
            "conda" => self.build_conda(ctx),
            "package" => self.build_package(ctx),
            "git_commit" => self.build_git_commit(ctx),
            "git_metrics" => self.build_git_metrics(ctx),
            "shlvl" => self.build_shlvl(ctx),
            "localip" => self.build_localip(ctx),
            "memory_usage" => self.build_memory(ctx),
            "sudo" => self.build_sudo(ctx),
            "direnv" => self.build_direnv(ctx),
            "nix_shell" => self.build_nix_shell(ctx),
            "shell" => self.build_shell(ctx),
            "status" => self.build_status(ctx),
            _ => {
                if let Some((_, id)) = LANGUAGES.iter().find(|(n, _)| *n == name) {
                    return self.build_language(name, id, ctx);
                }
                if self.opts.commands {
                    if let Some(c) = COMMAND_LANGS.iter().find(|c| c.name == name) {
                        return self.build_command_lang(c, ctx);
                    }
                    self.note(format!("format: ${name}"), "needs the Starship bridge; left out (commands mode)");
                    return None;
                }
                let id = name.to_string();
                let mut m = json!({ "type": "starship", "module": name });
                // ANSI colors style the bridge text, but powerline separators
                // also need the segment background in the tildy module.
                let section = self.section(name);
                let style = get_str(&section, "style");
                let wrapped = get_str(&section, "format").and_then(|f| wrapping_styles(&parse_tree(f)).last().cloned());
                let style = wrapped.map(|s| expand_style_vars(&s, style, &section)).or_else(|| style.map(str::to_string));
                if let Some(style) = style.and_then(|s| self.style_value(name, &s)) { m["style"] = style; }
                // Custom (including negative) detection belongs to Starship.
                // A stale approximation here could suppress a valid module.
                let custom_detection = self.user(name).is_some_and(|s| ["detect_files", "detect_extensions", "detect_folders", "detect_env_vars"].iter().any(|key| s.contains_key(*key)));
                if let Some((_, files, exts, folders)) = BRIDGE_DETECT.iter().find(|(n, ..)| *n == name).filter(|_| !custom_detection) {
                    if !files.is_empty() { m["detect_files"] = json!(files); }
                    if !exts.is_empty() { m["detect_extensions"] = json!(exts); }
                    if !folders.is_empty() { m["detect_folders"] = json!(folders); }
                }
                self.put(&id, m)
            }
        }
    }

    fn build_username(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("username", &["show_always", "style_user", "style_root", "format", "aliases", "detect_env_vars"]);
        let s = self.section("username");
        let mut m = Map::new();
        m.insert("type".into(), json!("username"));
        m.insert("show_always".into(), json!(get_bool(&s, "show_always").unwrap_or(false)));
        self.aliases_and_env_vars("username", &s, &mut m);
        let style_raw = get_str(&s, "style_user").or(get_str(&s, "style")).map(str::to_string);
        // style_root: a rule over the user style when the user is root.
        if let (Some(a), Some(b)) = (get_str(&s, "style_user"), get_str(&s, "style_root").map(str::to_string)) {
            if a != b && self.user_has("username", "style_root") {
                if let Some(st) = self.style_value("username", &b) {
                    m.insert(
                        "rules".into(),
                        json!([{ "id": "root", "when": { "field": "session.username", "op": "eq", "value": "root" }, "style": st }]),
                    );
                }
            }
        }
        let raw = get_str(&s, "format").unwrap_or("[$user]($style)");
        let els = parse_tree(raw);
        if !tree_has_vars(&els) {
            // A fixed symbol instead of the name (`format = '[ <glyph> ]($style)'`).
            let t = tree_text(&els);
            if !t.trim().is_empty() {
                m.insert("symbol".into(), self.symbol("username", &t, ctx.powerline, true));
            }
            let chain = wrapping_styles(&els);
            let eff = chain.last().map(|c| expand_style_vars(c, style_raw.as_deref(), &s)).or(style_raw.clone());
            if let Some(st) = eff.and_then(|e| self.style_value("username", &e)) {
                m.insert("style".into(), st);
            }
            return self.put("user", Value::Object(m));
        }
        let rule = |v: &str| match v {
            "user" => VarRule::Keep("user"),
            _ => VarRule::Unknown,
        };
        let f = self.convert("username", &s, style_raw.as_deref(), &rule, &["$user"], ctx);
        self.apply_fmt(&mut m, f, "username");
        self.put("user", Value::Object(m))
    }

    fn build_directory(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys(
            "directory",
            &[
                "style", "format", "truncation_length", "truncation_symbol", "substitutions", "home_symbol", "truncate_to_repo",
                "repo_root_format", "repo_root_style", "before_repo_root_style", "read_only", "read_only_style",
                "fish_style_pwd_dir_length", "use_logical_path", "use_os_path_sep",
            ],
        );
        let s = self.section("directory");
        let mut m = Map::new();
        m.insert("type".into(), json!("directory"));
        m.insert("truncation_length".into(), json!(get_int(&s, "truncation_length").unwrap_or(3)));
        if let Some(n) = get_int(&s, "fish_style_pwd_dir_length").filter(|n| *n > 0) {
            m.insert("fish_style_pwd_dir_length".into(), json!(n.min(1000)));
        }
        if get_bool(&s, "use_logical_path") == Some(false) {
            m.insert("use_logical_path".into(), json!(false));
        }
        // `$read_only` keeps its style through the format's span
        // (`[$read_only]($read_only_style)`); only the symbol is carried.
        if let Some(v) = get_str(&s, "read_only").filter(|v| *v != "🔒").map(str::to_string) {
            m.insert("read_only".into(), self.symbol("directory", &v, false, false));
        }
        if let Some(v) = get_str(&s, "truncation_symbol") {
            m.insert("truncation_symbol".into(), tier_text(v));
        }
        if let Some(v) = get_str(&s, "home_symbol").filter(|v| *v != "~") {
            m.insert("home_symbol".into(), tier_text(v));
        }
        if let Some(false) = get_bool(&s, "truncate_to_repo") {
            m.insert("truncate_to_repo".into(), json!(false));
        }
        if let Some(Toml::Table(sub)) = s.get("substitutions") {
            let mut o = Map::new();
            for (k, v) in sub {
                if let Some(v) = v.as_str() {
                    // Nerd glyphs only in the nerd tier (elsewhere the name stays).
                    let val = if has_pua(v) { json!({ "nerd": v }) } else { json!(v) };
                    o.insert(k.clone(), val);
                }
            }
            if !o.is_empty() {
                // TOML tables lose key order here; tildy matches the longest prefix.
                m.insert("substitutions".into(), Value::Object(o));
            }
        }
        let style_raw = get_str(&s, "style").map(str::to_string);
        let rule = |v: &str| match v {
            "path" => VarRule::Keep("path"),
            "read_only" => VarRule::Keep("read_only"),
            _ => VarRule::Unknown,
        };
        let f = self.convert("directory", &s, style_raw.as_deref(), &rule, &["$path"], ctx);
        self.apply_fmt(&mut m, f, "directory");
        let user_rf = self.user("directory").and_then(|u| get_str(u, "repo_root_format"));
        if user_rf.is_none() && self.user_has("directory", "repo_root_style") {
            // Starship splits the path at the repository root once
            // repo_root_style is set; tildy derives that form from `format`.
            for k in ["repo_root_style", "before_repo_root_style"] {
                let raw = get_str(&s, k).map(str::to_string).or_else(|| style_raw.clone());
                if let Some(v) = raw.and_then(|r| self.style_value("directory", &r)) {
                    m.insert(k.into(), v);
                }
            }
        }
        if let Some(rf) = user_rf {
            // Root styles default to the directory style, as in Starship.
            let mut sec = s.clone();
            for k in ["repo_root_style", "before_repo_root_style"] {
                if !sec.contains_key(k) {
                    if let Some(st) = &style_raw {
                        sec.insert(k.into(), Toml::String(st.clone()));
                    }
                }
            }
            let rule = |v: &str| match v {
                "before_root_path" => VarRule::Keep("before_root_path"),
                "repo_root" => VarRule::Keep("repo_root"),
                "path" => VarRule::Keep("path"),
                "read_only" => VarRule::Keep("read_only"),
                _ => VarRule::Unknown,
            };
            let mut dropped = vec![];
            let els = prune(&parse_tree(rf), &rule, &mut dropped);
            let palette = self.palette.clone();
            let out = to_tildy(&els, &mut |v| Some(v.to_string()), &mut |st| {
                Some(parse_style(&expand_style_vars(st, style_raw.as_deref(), &sec), &palette).to_spec())
            });
            for d in dropped {
                self.note("directory.repo_root_format", format!("variable ${d} is not available in tildy; dropped"));
            }
            m.insert("repo_root_format".into(), json!(out));
        }
        self.put("cwd", Value::Object(m))
    }

    fn build_git(&mut self, branch_off: bool, status_off: bool, ctx: RowCtx) -> Option<String> {
        const SYMS: &[&str] =
            &["conflicted", "ahead", "behind", "diverged", "untracked", "stashed", "modified", "staged", "renamed", "deleted", "up_to_date"];
        self.check_keys(
            "git_branch",
            &["symbol", "style", "format", "ignore_branches", "only_attached", "always_show_remote", "truncation_length", "truncation_symbol"],
        );
        let mut allowed: Vec<&str> = vec!["style", "format", "ignore_submodules", "use_git_executable", "typechanged"];
        allowed.extend(SYMS);
        self.check_keys("git_status", &allowed);
        if self.user("git_status").and_then(|u| get_str(u, "typechanged")).is_some_and(|t| !t.is_empty()) {
            self.note("git_status.typechanged", "tildy's git data has no type-change count yet; type changes count as modified/staged");
        }
        let b = self.section("git_branch");
        let st = self.section("git_status");

        let mut m = Map::new();
        m.insert("type".into(), json!("git"));
        if !branch_off {
            if let Some(sym) = get_str(&b, "symbol").map(str::to_string) {
                m.insert("branch_symbol".into(), self.symbol("git", &sym, ctx.powerline, true));
            }
            // Starship's default truncation_length is 2^63-1 (no truncation).
            if let Some(n) = get_int(&b, "truncation_length").filter(|n| (1..=1000).contains(n)) {
                m.insert("truncation_length".into(), json!(n));
                if let Some(ts) = get_str(&b, "truncation_symbol").filter(|t| *t != "…") {
                    m.insert("truncation_symbol".into(), json!(ts));
                }
            }
            if get_bool(&b, "always_show_remote") == Some(true) {
                m.insert("always_show_remote".into(), json!(true));
            }
        }
        if !status_off && get_bool(&st, "ignore_submodules") == Some(true) {
            m.insert("ignore_submodules".into(), json!(true));
        }
        let ignore = get_strings(&b, "ignore_branches").filter(|v| !v.is_empty());
        let only_attached = get_bool(&b, "only_attached") == Some(true);
        if let Some(v) = &ignore {
            m.insert("ignore_branches".into(), json!(v));
        }
        if only_attached {
            m.insert("only_attached".into(), json!(true));
        }
        let hide_branch = ignore.is_some() || only_attached;
        if !status_off {
            let mut o = Map::new();
            for k in SYMS {
                // up_to_date is empty by default, like tildy's.
                if let Some(raw) = get_str(&st, k).filter(|r| !(*k == "up_to_date" && r.is_empty())).map(str::to_string) {
                    o.insert((*k).into(), self.symbol(k, &raw, false, false));
                }
            }
            m.insert("status_symbols".into(), Value::Object(o));
        }
        if !self.disabled("git_state") {
            let gs = self.section("git_state");
            if let Some(raw) = get_str(&gs, "style").filter(|r| *r != "bold yellow").map(str::to_string) {
                if let Some(v) = self.style_value("git_state", &raw) {
                    m.insert("state_style".into(), v);
                }
            }
            // Operation labels (plain text; a styled label keeps its text).
            let mut labels = Map::new();
            for (key, op) in [("rebase", "rebase"), ("merge", "merge"), ("revert", "revert"), ("cherry_pick", "cherry-pick"), ("bisect", "bisect"), ("am", "am")] {
                if let Some(raw) = self.user("git_state").and_then(|u| get_str(u, key)) {
                    let els = parse_tree(raw);
                    if tree_has_spans(&els) {
                        self.note(format!("git_state.{key}"), "label styles are not supported; the text is kept");
                    }
                    labels.insert(op.into(), json!(tree_text(&els)));
                }
            }
            if !labels.is_empty() {
                m.insert("state_labels".into(), Value::Object(labels));
            }
            if self.user("git_state").is_some_and(|u| u.contains_key("am_or_rebase")) {
                self.note("git_state.am_or_rebase", "tildy tells am and rebase apart; ignored");
            }
        } else if self.user("git_state").and_then(|u| get_bool(u, "disabled")) == Some(true) {
            m.insert("show_state".into(), json!(false));
        }
        let branch_rule = |v: &str| match v {
            "symbol" => VarRule::Keep("branch_symbol"),
            "branch" => VarRule::Keep("branch"),
            "remote_name" => VarRule::Keep("remote_name"),
            "remote_branch" => VarRule::Keep("remote_branch"),
            _ => VarRule::Unknown,
        };
        if ctx.powerline {
            // Flatten both parts; `$all_status$ahead_behind` is tildy's `$status`.
            let status_rule = |v: &str| match v {
                "all_status" | "ahead_behind" => VarRule::Keep("status"),
                "count" | "ahead" | "behind" | "diverged" | "conflicted" | "untracked" | "stashed" | "modified" | "staged"
                | "renamed" | "deleted" | "typechanged" => VarRule::Silent,
                _ => VarRule::Unknown,
            };
            let bf = if branch_off { Fmt::default() } else { self.convert("git_branch", &b, get_str(&b, "style"), &branch_rule, &[], ctx) };
            let sf = if status_off { Fmt::default() } else { self.convert("git_status", &st, get_str(&st, "style"), &status_rule, &[], ctx) };
            let bpart = match (branch_off, bf.format) {
                (true, _) => String::new(),
                (false, Some(f)) => f,
                (false, None) => "$branch_symbol $branch".into(),
            };
            let spart = match (status_off, sf.format) {
                (true, _) => String::new(),
                (false, Some(f)) => f.replace("$status$status", "$status"),
                (false, None) => "$status".into(),
            };
            let spart = Regex::new(r"\(\s*\$status\s*\)").unwrap().replace_all(&spart, "$$status").to_string();
            let combined = format!("{bpart} {spart}").trim().to_string();
            if combined != "$branch_symbol $branch $status" {
                m.insert("format".into(), json!(combined));
            }
            if let Some(style) = if branch_off { sf.style } else { bf.style } {
                m.insert("style".into(), style);
            }
            return self.put("git", Value::Object(m));
        }
        // Plain row: both formats verbatim, concatenated as Starship does.
        let status_rule = |v: &str| match v {
            "all_status" => VarRule::Keep("all_status"),
            "ahead_behind" => VarRule::Keep("ahead_behind"),
            "conflicted" => VarRule::Keep("conflicted"),
            "untracked" => VarRule::Keep("untracked"),
            "stashed" => VarRule::Keep("stashed"),
            "modified" => VarRule::Keep("modified"),
            "staged" => VarRule::Keep("staged"),
            "renamed" => VarRule::Keep("renamed"),
            "deleted" => VarRule::Keep("deleted"),
            "typechanged" => VarRule::Silent,
            _ => VarRule::Unknown,
        };
        let mut parts = vec![];
        let mut style = None;
        if !branch_off {
            let f = self.convert("git_branch", &b, get_str(&b, "style"), &branch_rule, &[], ctx);
            let mut bf = f.format.unwrap_or_default();
            if hide_branch && !bf.is_empty() {
                // Starship hides the whole git_branch module for ignored branches.
                bf = format!("({bf})");
            }
            parts.push((bf, f.style));
        }
        if !status_off {
            let f = self.convert("git_status", &st, get_str(&st, "style"), &status_rule, &[], ctx);
            parts.push((f.format.unwrap_or_default(), f.style));
        }
        // A part whose single span was unwrapped into a style gets it back as a span.
        let mut fmt = String::new();
        let n = parts.len();
        for (f, st) in parts {
            match st {
                Some(st) if n == 1 => {
                    style = Some(st);
                    fmt.push_str(&f);
                }
                Some(st) => fmt.push_str(&wrap_span(&f, &st)),
                None => fmt.push_str(&f),
            }
        }
        m.insert("format".into(), json!(fmt));
        if let Some(s) = style {
            m.insert("style".into(), s);
        }
        self.put("git", Value::Object(m))
    }

    /// Language-like modules: symbol, `$version`, `version_format`, detection.
    fn language_like(&mut self, name: &str, m: &mut Map<String, Value>, ctx: RowCtx, defaults: &[&str]) {
        let s = self.section(name);
        if let Some(vf) = get_str(&s, "version_format").filter(|v| *v != "v${raw}") {
            m.insert("version_format".into(), json!(vf));
        }
        if let Some(u) = self.user(name) {
            let keys = ["detect_files", "detect_extensions", "detect_folders"];
            for k in keys {
                if let Some(v) = get_strings(u, k) {
                    m.insert(k.into(), json!(v));
                }
            }
            // Starship: all lists empty = never detected (python still shows in a
            // virtualenv). tildy treats an empty rule as "always", so use a file
            // name no directory entry can have.
            let all_empty = keys.iter().all(|k| m.get(*k).is_none_or(|v| v.as_array().is_some_and(|a| a.is_empty())));
            if all_empty && keys.iter().any(|k| m.contains_key(*k)) {
                for k in keys {
                    m.remove(k);
                }
                m.insert("detect_files".into(), json!(["/"]));
                m.insert("$comment".into(), json!("detect_files [\"/\"] never matches (Starship's empty detection lists)"));
            }
        }
        let python = name == "python";
        let elixir = name == "elixir";
        let rule = move |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            "version" => VarRule::Keep("version"),
            "virtualenv" if python => VarRule::Keep("env"),
            "otp_version" if elixir => VarRule::Silent,
            "pyenv_prefix" | "name" => VarRule::Silent,
            _ => VarRule::Unknown,
        };
        let style_raw = get_str(&s, "style").map(str::to_string);
        let mut f = self.convert(name, &s, style_raw.as_deref(), &rule, defaults, ctx);
        if elixir {
            // Without `$otp_version` the "OTP" label has nothing to label.
            f.format = f.format.map(|x| {
                let x = Regex::new(r"\s*\\\(OTP\s*\\\)").unwrap().replace_all(&x, "");
                Regex::new(r"\s*OTP\s+").unwrap().replace_all(&x, " ").to_string()
            });
        }
        self.insert_symbol(name, &s, m, ctx, &f);
        self.apply_fmt(m, f, name);
    }

    /// The section's `symbol`, trimmed when the segment is padded (powerline).
    fn insert_symbol(&mut self, name: &str, s: &Table, m: &mut Map<String, Value>, ctx: RowCtx, f: &Fmt) {
        if let Some(sym) = get_str(s, "symbol").map(str::to_string) {
            let trim = ctx.powerline && f.padding != Some(false);
            m.insert("symbol".into(), self.symbol(name, &sym, trim, true));
        }
    }

    fn build_language(&mut self, name: &str, id: &str, ctx: RowCtx) -> Option<String> {
        self.check_keys(name, &["symbol", "style", "format", "version_format", "detect_files", "detect_extensions", "detect_folders"]);
        let mut m = Map::new();
        m.insert("type".into(), json!("language"));
        m.insert("language".into(), json!(name));
        let defaults: &[&str] = if ctx.powerline { &["$symbol( $version)", "$symbol ($version)"] } else { &["$symbol( $version)"] };
        self.language_like(name, &mut m, ctx, defaults);
        self.put(id, Value::Object(m))
    }

    fn build_command_lang(&mut self, c: &CommandLang, ctx: RowCtx) -> Option<String> {
        self.check_keys(c.name, &["symbol", "style", "format", "version_format", "detect_files", "detect_extensions", "detect_folders"]);
        let mut m = Map::new();
        m.insert("type".into(), json!("command"));
        m.insert("argv".into(), json!(c.argv));
        m.insert("regex".into(), json!(c.regex));
        if !c.files.is_empty() { m.insert("detect_files".into(), json!(c.files)); }
        if !c.extensions.is_empty() { m.insert("detect_extensions".into(), json!(c.extensions)); }
        if !c.folders.is_empty() { m.insert("detect_folders".into(), json!(c.folders)); }
        // `command` defaults to `($symbol )$text`; Starship shows `$version`.
        let defaults: &[&str] = if ctx.powerline { &["$symbol( $version)", "$symbol ($version)"] } else { &["$symbol( $version)"] };
        self.language_like(c.name, &mut m, ctx, defaults);
        if !m.contains_key("format") {
            m.insert("format".into(), json!("$symbol( $version)"));
        }
        self.put(c.name, Value::Object(m))
    }

    fn build_python(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("python", &["symbol", "style", "format", "version_format", "detect_files", "detect_extensions", "detect_folders"]);
        let mut m = Map::new();
        m.insert("type".into(), json!("python"));
        // tildy's python default also shows the environment; a format without
        // `$env` must stay explicit.
        let defaults: &[&str] = &["$symbol( $version)( \\($env\\))"];
        self.language_like("python", &mut m, ctx, defaults);
        self.put("py", Value::Object(m))
    }

    fn build_docker(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("docker_context", &["symbol", "style", "format", "only_with_files", "detect_files", "detect_extensions", "detect_folders"]);
        let s = self.section("docker_context");
        let mut m = Map::new();
        m.insert("type".into(), json!("docker_context"));
        if get_bool(&s, "only_with_files") == Some(false) {
            // Shown everywhere: tildy's empty detection rule always matches.
            for k in ["detect_files", "detect_extensions", "detect_folders"] {
                m.insert(k.into(), json!([]));
            }
        } else if let Some(u) = self.user("docker_context") {
            for k in ["detect_files", "detect_extensions", "detect_folders"] {
                if let Some(v) = get_strings(u, k) {
                    m.insert(k.into(), json!(v));
                }
            }
            // Starship: all lists empty never matches (tildy: always).
            let lists = ["detect_files", "detect_extensions", "detect_folders"];
            if lists.iter().all(|k| m.get(*k).is_some_and(|v| v.as_array().is_some_and(|a| a.is_empty()))) {
                m.insert("detect_files".into(), json!(["/"]));
            }
        }
        let rule = |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            "context" => VarRule::Keep("context"),
            "path" => VarRule::Unknown,
            _ => VarRule::Unknown,
        };
        let defaults: &[&str] = if ctx.powerline { &["$symbol( $context)", "$symbol $context"] } else { &["$symbol( $context)"] };
        let style_raw = get_str(&s, "style").map(str::to_string);
        let f = self.convert("docker_context", &s, style_raw.as_deref(), &rule, defaults, ctx);
        self.insert_symbol("docker_context", &s, &mut m, ctx, &f);
        self.apply_fmt(&mut m, f, "docker_context");
        self.put("docker", Value::Object(m))
    }

    fn build_time(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("time", &["time_format", "style", "format", "use_12hr", "utc_time_offset", "time_range"]);
        let s = self.section("time");
        let mut m = Map::new();
        m.insert("type".into(), json!("time"));
        let mut tf = match (get_str(&s, "time_format"), get_bool(&s, "use_12hr")) {
            (Some(f), _) => f.to_string(),
            (None, Some(true)) => "%I:%M:%S %p".to_string(),
            _ => "%T".to_string(),
        };
        let supported = Regex::new(r"%[^RHMSIpYmdFTr%]").unwrap();
        if supported.is_match(&tf) {
            self.note("time.time_format", format!("'{tf}' uses specifiers tildy does not support; using %T"));
            tf = "%T".into();
        }
        if let Some(o) = get_str(&s, "utc_time_offset").filter(|o| *o != "local") {
            if crate::config::parse_utc_offset(o).is_some() {
                m.insert("utc_time_offset".into(), json!(o));
            } else {
                self.note("time.utc_time_offset", format!("'{o}' is not local or hours between -24 and 24; local time is shown"));
            }
        }
        if let Some(r) = get_str(&s, "time_range").filter(|r| *r != "-") {
            if crate::config::parse_time_range(r).is_some() {
                m.insert("time_range".into(), json!(r));
            } else {
                self.note("time.time_range", format!("'{r}' is not HH:MM:SS-HH:MM:SS; the time always shows"));
            }
        }
        m.insert("time_format".into(), json!(tf));
        let rule = |v: &str| match v {
            "time" => VarRule::Keep("time"),
            _ => VarRule::Unknown,
        };
        let style_raw = get_str(&s, "style").map(str::to_string);
        let f = self.convert("time", &s, style_raw.as_deref(), &rule, &["($symbol )$time", "$time"], ctx);
        self.apply_fmt(&mut m, f, "time");
        self.put("time", Value::Object(m))
    }

    fn build_character(&mut self, ctx: RowCtx) -> Option<String> {
        const VIMCMD: [&str; 4] = ["vimcmd_symbol", "vimcmd_visual_symbol", "vimcmd_replace_symbol", "vimcmd_replace_one_symbol"];
        let mut allowed = vec!["success_symbol", "error_symbol", "format"];
        allowed.extend(VIMCMD);
        self.check_keys("character", &allowed);
        for k in VIMCMD {
            if self.user_has("character", k) {
                self.note(format!("character.{k}"), "needs the Zsh keymap, which the shell does not send yet; ignored");
            }
        }
        let s = self.section("character");
        if let Some(f) = get_str(&s, "format").filter(|f| f.trim() != "$symbol") {
            self.note("character.format", format!("'{f}': only the symbol is shown"));
        }
        let ok = get_str(&s, "success_symbol").unwrap_or("[❯](bold green)").to_string();
        let err = get_str(&s, "error_symbol").unwrap_or("[❯](bold red)").to_string();
        let (ok_els, err_els) = (parse_tree(&ok), parse_tree(&err));
        let (ok_t, err_t) = (tree_text(&ok_els), tree_text(&err_els));
        let mut m = Map::new();
        m.insert("type".into(), json!("character"));
        m.insert("symbol".into(), self.symbol("character", ok_t.trim(), true, true));
        if ok_t.trim() != err_t.trim() {
            m.insert("error_symbol".into(), self.symbol("character", err_t.trim(), true, true));
        }
        let a = wrapping_styles(&ok_els).last().cloned().unwrap_or_default();
        let b = wrapping_styles(&err_els).last().cloned().unwrap_or_default();
        if let Some(v) = self.style_value("character.success_symbol", &a) {
            m.insert("success_style".into(), v);
        }
        if let Some(v) = self.style_value("character.error_symbol", &b) {
            m.insert("error_style".into(), v);
        }
        if ctx.powerline {
            m.insert("padding".into(), json!(false));
        }
        self.put("char", Value::Object(m))
    }

    fn build_duration(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys(
            "cmd_duration",
            &["min_time", "style", "format", "show_milliseconds", "show_notifications", "min_time_to_notify", "notification_timeout"],
        );
        let s = self.section("cmd_duration");
        if get_bool(&s, "show_notifications") == Some(true) {
            self.note("cmd_duration.show_notifications", "desktop notifications are not supported");
        }
        let min = get_int(&s, "min_time").unwrap_or(2000);
        let ms = get_bool(&s, "show_milliseconds") == Some(true);
        let mut m = Map::new();
        m.insert("type".into(), json!("duration"));
        m.insert("show_when".into(), json!({ "field": "command.duration_ms", "op": "ge", "value": min }));
        let human = if ms { "human_ms" } else { "human" };
        let rule = move |v: &str| match v {
            "duration" => VarRule::Keep(human),
            _ => VarRule::Unknown,
        };
        let style_raw = get_str(&s, "style").map(str::to_string);
        let f = self.convert("cmd_duration", &s, style_raw.as_deref(), &rule, &[], ctx);
        self.apply_fmt(&mut m, f, "cmd_duration");
        if !m.contains_key("format") {
            m.insert("format".into(), json!(format!("${human}")));
        }
        self.put("duration", Value::Object(m))
    }

    fn build_hostname(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("hostname", &["ssh_only", "style", "format", "ssh_symbol", "trim_at", "aliases", "detect_env_vars"]);
        let s = self.section("hostname");
        let mut m = Map::new();
        m.insert("type".into(), json!("hostname"));
        if get_bool(&s, "ssh_only").unwrap_or(true) {
            m.insert("show_when".into(), json!({ "field": "session.ssh", "op": "eq", "value": true }));
        }
        if let Some(t) = get_str(&s, "trim_at").filter(|t| *t != ".") {
            m.insert("trim_at".into(), json!(t));
        }
        self.aliases_and_env_vars("hostname", &s, &mut m);
        // `$ssh_symbol` is the symbol in an SSH session only, as in Starship.
        let rule = |v: &str| match v {
            "hostname" => VarRule::Keep("host"),
            "ssh_symbol" => VarRule::Keep("ssh_symbol"),
            _ => VarRule::Unknown,
        };
        let style_raw = get_str(&s, "style").map(str::to_string);
        let f = self.convert("hostname", &s, style_raw.as_deref(), &rule, &[], ctx);
        if let Some(sym) = get_str(&s, "ssh_symbol").map(str::to_string) {
            let trim = ctx.powerline && f.padding != Some(false);
            m.insert("symbol".into(), self.symbol("hostname", &sym, trim, true));
        }
        self.apply_fmt(&mut m, f, "hostname");
        self.put("host", Value::Object(m))
    }

    fn build_jobs(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("jobs", &["symbol", "style", "format", "number_threshold", "symbol_threshold"]);
        if self.user_has("jobs", "threshold") {
            self.note("jobs.threshold", "deprecated in Starship; use number_threshold and symbol_threshold; ignored");
        }
        let s = self.section("jobs");
        let mut m = Map::new();
        m.insert("type".into(), json!("jobs"));
        if let Some(n) = get_int(&s, "number_threshold").filter(|n| *n != 1 && *n >= 0) {
            m.insert("number_threshold".into(), json!(n.min(1000)));
        }
        if let Some(n) = get_int(&s, "symbol_threshold").filter(|n| *n != 1 && *n >= 0) {
            m.insert("symbol_threshold".into(), json!(n.min(1000)));
        }
        let rule = |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            "number" => VarRule::Keep("number"),
            _ => VarRule::Unknown,
        };
        let style_raw = get_str(&s, "style").map(str::to_string);
        let f = self.convert("jobs", &s, style_raw.as_deref(), &rule, &["$symbol$count", "$symbol$number"], ctx);
        self.insert_symbol("jobs", &s, &mut m, ctx, &f);
        self.apply_fmt(&mut m, f, "jobs");
        self.put("jobs", Value::Object(m))
    }

    fn build_os(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("os", &["symbols", "style", "format"]);
        let s = self.section("os");
        let mut m = Map::new();
        m.insert("type".into(), json!("os"));
        if let Some(Toml::Table(syms)) = s.get("symbols") {
            let mut o = Map::new();
            let mut distros = false;
            for (k, v) in syms {
                let Some(v) = v.as_str() else { continue };
                match OS_NAMES.iter().find(|(st, _)| st == k) {
                    Some((_, os)) => {
                        let v = v.to_string();
                        o.insert((*os).into(), self.symbol("os", &v, ctx.powerline, true));
                    }
                    None => distros = true,
                }
            }
            if distros && self.user("os").is_some_and(|u| u.contains_key("symbols")) {
                self.note("os.symbols", "distribution symbols are not used (no distribution detection); Linux uses `Linux`");
            }
            if !o.is_empty() {
                m.insert("os_symbols".into(), Value::Object(o));
            }
        }
        let rule = |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            "name" | "type" => VarRule::Keep("name"),
            "codename" | "edition" | "version" => VarRule::Silent,
            _ => VarRule::Unknown,
        };
        let style_raw = get_str(&s, "style").map(str::to_string);
        let f = self.convert("os", &s, style_raw.as_deref(), &rule, &["$symbol"], ctx);
        self.apply_fmt(&mut m, f, "os");
        self.put("os", Value::Object(m))
    }

    fn build_fill(&mut self) -> Option<String> {
        self.check_keys("fill", &["symbol", "style"]);
        let s = self.section("fill");
        let mut m = Map::new();
        m.insert("type".into(), json!("fill"));
        if let Some(sym) = get_str(&s, "symbol") {
            m.insert("symbol".into(), json!(sym));
        }
        if let Some(st) = get_str(&s, "style").map(str::to_string).and_then(|r| self.style_value("fill", &r)) {
            m.insert("style".into(), st);
        }
        self.put("fill", Value::Object(m))
    }

    /// `conda` shows `CONDA_DEFAULT_ENV` (forwarded by the shell).
    fn build_conda(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("conda", &["symbol", "style", "format", "ignore_base"]);
        let s = self.section("conda");
        let mut m = Map::new();
        m.insert("type".into(), json!("env_var"));
        m.insert("variable".into(), json!("CONDA_DEFAULT_ENV"));
        if get_bool(&s, "ignore_base").unwrap_or(true) {
            m.insert("show_when".into(), json!({ "field": "env.CONDA_DEFAULT_ENV", "op": "ne", "value": "base" }));
        }
        let rule = |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            "environment" => VarRule::Keep("env_value"),
            _ => VarRule::Unknown,
        };
        let style_raw = get_str(&s, "style").map(str::to_string);
        let f = self.convert("conda", &s, style_raw.as_deref(), &rule, &["($symbol )$env_value"], ctx);
        self.insert_symbol("conda", &s, &mut m, ctx, &f);
        self.apply_fmt(&mut m, f, "conda");
        self.put("conda", Value::Object(m))
    }

    // ------------------------------------------------- M2 task E1 modules

    /// `aliases` and `detect_env_vars` (hostname, username).
    fn aliases_and_env_vars(&mut self, owner: &str, s: &Table, m: &mut Map<String, Value>) {
        if let Some(Toml::Table(a)) = s.get("aliases") {
            let o: Map<String, Value> =
                a.iter().filter_map(|(k, v)| v.as_str().filter(|_| !k.is_empty()).map(|v| (k.clone(), json!(v)))).collect();
            if !o.is_empty() {
                m.insert("aliases".into(), Value::Object(o));
            }
        }
        self.env_vars(owner, s, m);
    }

    fn env_vars(&mut self, owner: &str, s: &Table, m: &mut Map<String, Value>) {
        if let Some(v) = get_strings(s, "detect_env_vars").filter(|v| !v.is_empty()) {
            let ok = |e: &String| {
                let n = e.trim_start_matches('!');
                !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            };
            let (good, bad): (Vec<String>, Vec<String>) = v.into_iter().partition(ok);
            for b in bad {
                self.note(format!("{owner}.detect_env_vars"), format!("'{b}' is not an environment variable name; dropped"));
            }
            if !good.is_empty() {
                m.insert("detect_env_vars".into(), json!(good));
            }
        }
    }

    /// A Starship message option (`pure_msg`, `allowed_msg`, ...) as a tildy
    /// symbol (spans kept), when it differs from Starship's default.
    fn msg(&mut self, owner: &str, s: &Table, key: &str, default: &str, m: &mut Map<String, Value>) {
        if let Some(v) = get_str(s, key).filter(|v| *v != default).map(str::to_string) {
            m.insert(key.into(), self.symbol(owner, &v, false, false));
        }
    }

    /// The common tail of a simple module: format conversion with `rule`,
    /// the section's symbol, then the module is stored under `id`.
    fn finish(&mut self, name: &str, id: &str, mut m: Map<String, Value>, rule: &dyn Fn(&str) -> VarRule, defaults: &[&str], ctx: RowCtx) -> Option<String> {
        let s = self.section(name);
        let style_raw = get_str(&s, "style").map(str::to_string);
        let f = self.convert(name, &s, style_raw.as_deref(), rule, defaults, ctx);
        self.insert_symbol(name, &s, &mut m, ctx, &f);
        self.apply_fmt(&mut m, f, name);
        self.put(id, Value::Object(m))
    }

    fn build_package(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("package", &["format", "symbol", "version_format", "style", "display_private"]);
        let s = self.section("package");
        let mut m = Map::new();
        m.insert("type".into(), json!("package"));
        if get_bool(&s, "display_private") == Some(true) {
            m.insert("display_private".into(), json!(true));
        }
        if let Some(vf) = get_str(&s, "version_format").filter(|v| *v != "v${raw}") {
            m.insert("version_format".into(), json!(vf));
        }
        let rule = |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            "version" => VarRule::Keep("version"),
            _ => VarRule::Unknown,
        };
        self.finish("package", "package", m, &rule, &["($symbol )$version", "$symbol $version"], ctx)
    }

    fn build_git_commit(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys(
            "git_commit",
            &["commit_hash_length", "format", "style", "only_detached", "tag_disabled", "tag_max_candidates", "tag_symbol"],
        );
        let s = self.section("git_commit");
        let mut m = Map::new();
        m.insert("type".into(), json!("git_commit"));
        if let Some(n) = get_int(&s, "commit_hash_length").filter(|n| *n != 7 && (0..=1000).contains(n)) {
            m.insert("commit_hash_length".into(), json!(n));
        }
        if get_bool(&s, "only_detached") == Some(false) {
            m.insert("only_detached".into(), json!(false));
        }
        if get_bool(&s, "tag_disabled") == Some(false) {
            m.insert("tag_disabled".into(), json!(false));
            if let Some(n) = get_int(&s, "tag_max_candidates").filter(|n| *n > 0) {
                m.insert("tag_max_candidates".into(), json!(n.min(1000)));
            }
            if let Some(ts) = self.user("git_commit").and_then(|u| get_str(u, "tag_symbol")).map(str::to_string) {
                m.insert("tag_symbol".into(), self.symbol("tag", &ts, false, true));
            }
        }
        let rule = |v: &str| match v {
            "hash" => VarRule::Keep("hash"),
            "tag" => VarRule::Keep("tag"),
            _ => VarRule::Unknown,
        };
        self.finish("git_commit", "git_commit", m, &rule, &["\\($hash$tag\\)"], ctx)
    }

    fn build_git_metrics(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("git_metrics", &["added_style", "deleted_style", "only_nonzero_diffs", "format", "ignore_submodules"]);
        let s = self.section("git_metrics");
        let mut m = Map::new();
        m.insert("type".into(), json!("git_metrics"));
        if get_bool(&s, "only_nonzero_diffs") == Some(false) {
            m.insert("only_nonzero_diffs".into(), json!(false));
        }
        if get_bool(&s, "ignore_submodules") == Some(true) {
            m.insert("ignore_submodules".into(), json!(true));
        }
        // `$added_style`/`$deleted_style` resolve inside the format's spans.
        let rule = |v: &str| match v {
            "added" => VarRule::Keep("added"),
            "deleted" => VarRule::Keep("deleted"),
            _ => VarRule::Unknown,
        };
        self.finish("git_metrics", "git_metrics", m, &rule, &["$diff"], ctx)
    }

    fn build_shlvl(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("shlvl", &["threshold", "format", "symbol", "repeat", "repeat_offset", "style"]);
        let s = self.section("shlvl");
        let mut m = Map::new();
        m.insert("type".into(), json!("shlvl"));
        if let Some(n) = get_int(&s, "threshold").filter(|n| *n != 2) {
            m.insert("threshold".into(), json!(n.clamp(-1000, 1000)));
        }
        if get_bool(&s, "repeat") == Some(true) {
            m.insert("repeat".into(), json!(true));
        }
        if let Some(n) = get_int(&s, "repeat_offset").filter(|n| *n > 0) {
            m.insert("repeat_offset".into(), json!(n.min(1000)));
        }
        let rule = |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            "shlvl" => VarRule::Keep("shlvl"),
            _ => VarRule::Unknown,
        };
        self.finish("shlvl", "shlvl", m, &rule, &["($symbol )$shlvl"], ctx)
    }

    fn build_localip(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("localip", &["ssh_only", "format", "style"]);
        let s = self.section("localip");
        let mut m = Map::new();
        m.insert("type".into(), json!("localip"));
        if get_bool(&s, "ssh_only") == Some(false) {
            m.insert("ssh_only".into(), json!(false));
        }
        let rule = |v: &str| match v {
            "localipv4" => VarRule::Keep("localipv4"),
            _ => VarRule::Unknown,
        };
        self.finish("localip", "localip", m, &rule, &["$localipv4"], ctx)
    }

    fn build_memory(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("memory_usage", &["threshold", "format", "symbol", "style"]);
        let s = self.section("memory_usage");
        let mut m = Map::new();
        m.insert("type".into(), json!("memory_usage"));
        if let Some(n) = get_int(&s, "threshold").filter(|n| *n != 75) {
            m.insert("threshold".into(), json!(n.clamp(-1000, 1000)));
        }
        let rule = |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            "ram" => VarRule::Keep("ram"),
            "ram_pct" => VarRule::Keep("ram_pct"),
            "swap" => VarRule::Keep("swap"),
            "swap_pct" => VarRule::Keep("swap_pct"),
            _ => VarRule::Unknown,
        };
        self.finish("memory_usage", "memory_usage", m, &rule, &["($symbol )$ram( | $swap)"], ctx)
    }

    fn build_sudo(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("sudo", &["format", "symbol", "style", "allow_windows"]);
        let mut m = Map::new();
        m.insert("type".into(), json!("sudo"));
        let rule = |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            _ => VarRule::Unknown,
        };
        self.finish("sudo", "sudo", m, &rule, &["$symbol"], ctx)
    }

    fn build_direnv(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys(
            "direnv",
            &[
                "format", "symbol", "style", "detect_extensions", "detect_files", "detect_folders", "detect_env_vars", "allowed_msg",
                "not_allowed_msg", "denied_msg", "loaded_msg", "unloaded_msg",
            ],
        );
        let s = self.section("direnv");
        let mut m = Map::new();
        m.insert("type".into(), json!("direnv"));
        if let Some(u) = self.user("direnv") {
            for k in ["detect_files", "detect_extensions", "detect_folders"] {
                if let Some(v) = get_strings(u, k) {
                    m.insert(k.into(), json!(v));
                }
            }
        }
        if self.user_has("direnv", "detect_env_vars") {
            let u = self.user("direnv").cloned().unwrap_or_default();
            self.env_vars("direnv", &u, &mut m);
            if !m.contains_key("detect_env_vars") {
                // An empty list: no env detection (tildy's default is DIRENV_FILE).
                m.insert("detect_env_vars".into(), json!([]));
            }
        }
        for (k, d) in [("allowed_msg", "allowed"), ("not_allowed_msg", "not allowed"), ("denied_msg", "denied"), ("loaded_msg", "loaded"), ("unloaded_msg", "not loaded")] {
            self.msg("direnv", &s, k, d, &mut m);
        }
        let rule = |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            "loaded" => VarRule::Keep("loaded"),
            "allowed" => VarRule::Keep("allowed"),
            "rc_path" => VarRule::Keep("rc_path"),
            _ => VarRule::Unknown,
        };
        self.finish("direnv", "direnv", m, &rule, &["($symbol )$loaded/$allowed"], ctx)
    }

    fn build_nix_shell(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys("nix_shell", &["format", "symbol", "style", "impure_msg", "pure_msg", "unknown_msg", "heuristic"]);
        let s = self.section("nix_shell");
        let mut m = Map::new();
        m.insert("type".into(), json!("nix_shell"));
        for (k, d) in [("pure_msg", "pure"), ("impure_msg", "impure"), ("unknown_msg", "")] {
            self.msg("nix_shell", &s, k, d, &mut m);
        }
        if get_bool(&s, "heuristic") == Some(true) {
            m.insert("heuristic".into(), json!(true));
        }
        let rule = |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            "state" => VarRule::Keep("state"),
            "name" => VarRule::Keep("name"),
            _ => VarRule::Unknown,
        };
        self.finish("nix_shell", "nix_shell", m, &rule, &["$symbol( $state)( \\($name\\))"], ctx)
    }

    fn build_shell(&mut self, ctx: RowCtx) -> Option<String> {
        let keys: Vec<String> = crate::config::SHELL_NAMES.iter().map(|n| format!("{n}_indicator")).collect();
        let mut allowed: Vec<&str> = vec!["format", "style", "unknown_indicator"];
        allowed.extend(keys.iter().map(String::as_str));
        self.check_keys("shell", &allowed);
        let mut m = Map::new();
        m.insert("type".into(), json!("shell"));
        let mut ind = Map::new();
        for n in crate::config::SHELL_NAMES {
            if let Some(v) = self.user("shell").and_then(|u| get_str(u, &format!("{n}_indicator"))).map(str::to_string) {
                ind.insert((*n).into(), self.symbol("shell", &v, false, false));
            }
        }
        if !ind.is_empty() {
            m.insert("indicators".into(), Value::Object(ind));
        }
        if let Some(v) = self.user("shell").and_then(|u| get_str(u, "unknown_indicator")).map(str::to_string) {
            m.insert("unknown_indicator".into(), self.symbol("shell", &v, false, false));
        }
        let rule = |v: &str| match v {
            "indicator" => VarRule::Keep("indicator"),
            _ => VarRule::Unknown,
        };
        self.finish("shell", "shell", m, &rule, &["$indicator"], ctx)
    }

    /// Starship's `status` is tildy's `exit_status`.
    fn build_status(&mut self, ctx: RowCtx) -> Option<String> {
        self.check_keys(
            "status",
            &[
                "format", "symbol", "success_symbol", "not_executable_symbol", "not_found_symbol", "sigint_symbol", "signal_symbol",
                "style", "success_style", "failure_style", "recognize_signal_code", "map_symbol",
            ],
        );
        for k in ["pipestatus", "pipestatus_separator", "pipestatus_format", "pipestatus_segment_format"] {
            if self.user_has("status", k) {
                self.note(format!("status.{k}"), "needs $pipestatus, which the shell does not send yet; ignored");
            }
        }
        let s = self.section("status");
        let mut m = Map::new();
        m.insert("type".into(), json!("exit_status"));
        // Symbols the file sets; tildy's defaults are Starship's with tiers.
        for k in ["success_symbol", "not_executable_symbol", "not_found_symbol", "sigint_symbol", "signal_symbol"] {
            if let Some(v) = self.user("status").and_then(|u| get_str(u, k)).filter(|v| !v.is_empty()).map(str::to_string) {
                m.insert(k.into(), self.symbol("status", &v, ctx.powerline, true));
            }
        }
        if get_bool(&s, "map_symbol") == Some(true) {
            m.insert("map_symbol".into(), json!(true));
        }
        if get_bool(&s, "recognize_signal_code") == Some(false) {
            m.insert("recognize_signal_code".into(), json!(false));
        }
        for (from, to) in [("success_style", "success_style"), ("failure_style", "error_style")] {
            if let Some(v) = get_str(&s, from).map(str::to_string).and_then(|r| self.style_value("status", &r)) {
                m.insert(to.into(), v);
            }
        }
        let rule = |v: &str| match v {
            "symbol" => VarRule::Keep("symbol"),
            "status" | "int" => VarRule::Keep("status"),
            "hex_status" => VarRule::Keep("hex_status"),
            "common_meaning" => VarRule::Keep("common_meaning"),
            "signal_number" => VarRule::Keep("signal_number"),
            "signal_name" => VarRule::Keep("signal_name"),
            "maybe_int" => VarRule::Keep("maybe_int"),
            _ => VarRule::Unknown,
        };
        self.finish("status", "status", m, &rule, &["$symbol $code"], ctx)
    }
}

/// Text of a tree with variables written as `${name}` (unstyled symbols such
/// as `⇡${count}`, filled in by the renderer).
fn tree_text_with_vars(els: &[El]) -> String {
    let mut s = String::new();
    for e in els {
        match e {
            El::Lit(t) => s.push_str(t),
            El::Var(v) => s.push_str(&format!("${{{v}}}")),
            El::Group(g) | El::Span(g, _) => s.push_str(&tree_text_with_vars(g)),
        }
    }
    s
}

/// `[fmt](spec)` from a style object (for a part whose span was unwrapped).
fn wrap_span(fmt: &str, st: &Value) -> String {
    let mut v: Vec<String> = vec![];
    for (k, w) in [("bold", "bold"), ("dim", "dimmed"), ("italic", "italic"), ("underline", "underline")] {
        if st.get(k) == Some(&json!(true)) {
            v.push(w.into());
        }
    }
    if let Some(f) = st.get("foreground").and_then(Value::as_str) {
        v.push(format!("fg:{f}"));
    }
    if let Some(b) = st.get("background").and_then(Value::as_str) {
        v.push(format!("bg:{b}"));
    }
    // Keep whitespace outside the span, as the unwrapped original had it.
    let (lead, trail) = edges(fmt);
    let core = fmt.trim();
    if core.is_empty() {
        return fmt.to_string();
    }
    format!("{lead}[{core}]({}){trail}", v.join(" "))
}

/// Move the first Nerd glyph in a converted format (literal text) out to
/// `$symbol`: `<glyph> $time` -> `($symbol )$time`. Formats that already use
/// `$symbol` are left alone.
fn extract_pua(fmt: &mut String) -> Option<String> {
    if fmt.contains("$symbol") || fmt.contains("${symbol}") || !has_pua(fmt) {
        return None;
    }
    let start = fmt.char_indices().find(|(_, c)| has_pua(&c.to_string()))?.0;
    let mut end = start;
    for (i, c) in fmt[start..].char_indices() {
        if has_pua(&c.to_string()) {
            end = start + i + c.len_utf8();
        } else {
            break;
        }
    }
    let glyph = fmt[start..end].to_string();
    let rest = &fmt[end..];
    let (replacement, skip) = if rest.starts_with(' ') { ("($symbol )", 1) } else { ("$symbol", 0) };
    let new = format!("{}{}{}", &fmt[..start], replacement, &rest[skip..]);
    *fmt = new;
    Some(glyph)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imp(src: &str) -> ImportResult {
        import_starship(src).unwrap()
    }

    #[test]
    fn invalid_toml_is_an_error() {
        assert!(import_starship("format = ").is_err());
    }

    #[test]
    fn empty_format_gives_empty_row() {
        let r = imp("format = \"\"\n");
        assert_eq!(r.candidate["layout"]["rows"], json!([{ "left": [] }]));
        assert!(r.unsupported.iter().any(|u| u.item == "format"));
    }

    #[test]
    fn disabled_modules_are_skipped() {
        let r = imp("format = \"$directory$nodejs$elixir\"\n[nodejs]\ndisabled = true\n[elixir]\ndisabled = true\n");
        assert_eq!(r.candidate["layout"]["rows"], json!([{ "left": ["cwd"] }]));
        assert!(r.candidate["modules"].get("node").is_none());
        assert_eq!(r.skipped, vec!["nodejs", "elixir"]);
    }

    #[test]
    fn unknown_module_becomes_bridge() {
        let r = imp("format = \"$aws\"\n");
        assert_eq!(r.candidate["modules"]["aws"], json!({ "type": "starship", "module": "aws" }));
        assert_eq!(r.candidate["layout"]["rows"], json!([{ "left": ["aws"] }]));
    }

    #[test]
    fn plain_rows_have_no_powerline() {
        let r = imp("format = \"$directory$git_branch\\n$character\"\n");
        assert_eq!(r.candidate["layout"]["rows"], json!([{ "left": ["cwd", "git"] }, { "left": ["char"] }]));
        // Starship's default look: text outside spans unstyled, trailing spaces kept.
        let m = &r.candidate["modules"];
        // `$read_only` keeps Starship's read_only_style through its span.
        assert_eq!(m["cwd"]["format"], "[$path](bold fg:cyan)[$read_only](fg:red) ");
        assert!(m["cwd"].get("style").is_none());
        assert_eq!(
            m["git"]["format"],
            "on [$branch_symbol$branch(:$remote_branch)](bold fg:magenta) ([\\[$all_status$ahead_behind\\]](bold fg:red) )"
        );
        assert_eq!(m["cwd"]["truncation_symbol"], "");
        // Starship's add_newline default comes along.
        assert_eq!(r.candidate["add_newline"], true);
        assert!(!r.unsupported.iter().any(|u| u.item == "add_newline"));
        let r = imp("add_newline = false\nformat = \"$directory\"\n");
        assert_eq!(r.candidate["add_newline"], false);
        assert_eq!(crate::config::from_str(&r.candidate.to_string()).unwrap().file.add_newline, Some(false));
    }

    #[test]
    fn time_is_off_by_default() {
        let r = imp("format = \"$time\"\n");
        assert!(r.candidate["modules"].get("time").is_none());
        assert_eq!(r.skipped, vec!["time"]);
    }

    #[test]
    fn unsupported_options_listed() {
        let r = imp("format = \"$directory$git_branch\"\n[git_branch]\nignore_bare_repo = true\n[directory]\nread_only = \"x\"\n");
        assert!(r.unsupported.iter().any(|u| u.item == "git_branch.ignore_bare_repo"));
        assert!(!r.unsupported.iter().any(|u| u.item == "directory.read_only"));
        assert_eq!(r.candidate["modules"]["cwd"]["read_only"], "x");
    }

    #[test]
    fn palettes_are_kept_and_colors_stay_by_name() {
        let r = imp("format = \"$directory\"\npalette = \"p\"\n[palettes.p]\nbrand = \"#112233\"\n[directory]\nstyle = \"fg:brand\"\nformat = \"[$path]($style)\"\n");
        assert_eq!(r.candidate["modules"]["cwd"]["style"], json!({ "foreground": "brand" }));
        assert_eq!((&r.candidate["palette"], &r.candidate["palettes"]), (&json!("p"), &json!({"p": {"brand": "#112233"}})));
        let c = crate::config::from_str(&r.candidate.to_string()).unwrap();
        assert_eq!(c.file.modules["cwd"].style.foreground.as_deref(), Some("#112233"));
        let src = "format = \"[$directory](bg:x)\"\npalette = \"a\"\n[palettes.a]\nx = \"#000001\"\n[palettes.b]\nx = \"#000002\"\nbad = \"blink\"\n\
                   [directory]\nformat = \"[ $path ](fg:x bg:x)\"\n";
        let b = import_starship_with(src, &ImportOptions { palette: Some("b".into()), commands: false }).unwrap();
        assert_eq!(b.candidate["modules"]["cwd"]["format"], "[ $path ](fg:x bg:x)");
        assert_eq!(b.candidate["palette"], "b");
        assert_eq!(b.candidate["palettes"], json!({"a": {"x": "#000001"}, "b": {"x": "#000002"}}));
        assert!(b.unsupported.iter().any(|u| u.item == "palettes.b.bad"));
        let c = crate::config::from_str(&b.candidate.to_string()).unwrap();
        assert_eq!(c.file.modules["cwd"].format.as_deref(), Some("[ $path ](fg:#000002 bg:#000002)"));
        assert!(import_starship_with(src, &ImportOptions { palette: Some("nope".into()), commands: false }).is_err());
        // A palette color tildy cannot name is resolved where it is used.
        let r = imp("format = \"$directory\"\npalette = \"p\"\n[palettes.p]\n9lives = \"#445566\"\n[directory]\nstyle = \"fg:9lives\"\nformat = \"[$path]($style)\"\n");
        assert_eq!(r.candidate["modules"]["cwd"]["style"], json!({ "foreground": "#445566" }));
    }

    #[test]
    fn powerline_caps_text_and_os() {
        // TOML escapes keep the Nerd glyphs visible in this source.
        let src = r##"
format = "[░▒▓](#a3aed2)[ \uE711 ](bg:#a3aed2 fg:#090c0c)[\uE0B4](bg:#769ff0 fg:#a3aed2)$directory[\uE0B4](fg:#769ff0 bg:#394260)$time[\uE0B4 ](fg:#394260)$cmd_duration\n$os$character"
[directory]
style = "fg:#e3e5e5 bg:#769ff0"
format = "[ $path ]($style)"
[time]
disabled = false
style = "bg:#394260"
format = "[[ \uF43A $time ](fg:#a0a9cb bg:#394260)]($style)"
[cmd_duration]
format = "in $duration "
[os]
disabled = false
format = "[$symbol]($style)"
[os.symbols]
Macos = "M"
Ubuntu = "U"
"##;
        let r = imp(src);
        let rows = &r.candidate["layout"]["rows"];
        assert_eq!(rows[0]["start"], json!({ "nerd": "░▒▓", "unicode": "░▒▓", "ascii": "" }));
        assert_eq!(rows[0]["separator"]["nerd"], "\u{e0b4}");
        assert_eq!(rows[0]["end"], json!({ "nerd": "\u{e0b4} ", "ascii": "> " }));
        assert_eq!(rows[0]["left"], json!(["text1", "cwd", "time", "duration"]));
        let m = &r.candidate["modules"];
        assert_eq!(
            m["text1"],
            json!({ "type": "text", "text": { "nerd": " \u{e711} " }, "style": { "foreground": "#090C0C", "background": "#A3AED2" } })
        );
        assert_eq!(m["time"]["style"], json!({ "foreground": "#A0A9CB", "background": "#394260" }));
        assert_eq!(m["time"]["symbol"]["nerd"], "\u{f43a}");
        assert!(m["time"].get("format").is_none());
        assert_eq!(m["duration"]["format"], "in $human ");
        assert_eq!(m["duration"]["padding"], false);
        assert_eq!(m["os"]["os_symbols"]["macos"], "M");
        assert!(m["os"]["os_symbols"]["linux"].is_object());
        assert!(tildy_validates(&r.candidate));
    }

    #[test]
    fn plain_rows_abut_when_modules_do() {
        let src = "format = \"$directory$git_branch$git_status\\n$jobs$character\"\n\
                   [git_branch]\nformat = '\\[[$symbol$branch]($style)\\]'\n\
                   [git_status]\nformat = '([\\[$all_status$ahead_behind\\]]($style))'\n\
                   [jobs]\nformat = '\\[[$symbol$number]($style)\\]'\n\
                   [character]\nsuccess_symbol = '[>](bold green)'\nerror_symbol = '[x](bold red)'\n";
        let r = imp(src);
        let rows = &r.candidate["layout"]["rows"];
        assert_eq!(rows[0]["separator"], "");
        assert_eq!(rows[1]["separator"], "");
        let m = &r.candidate["modules"];
        assert_eq!(m["git"]["format"], "\\[[$branch_symbol$branch](bold fg:magenta)\\]([\\[$all_status$ahead_behind\\]](bold fg:red))");
        assert_eq!(m["char"]["symbol"], ">");
        assert_eq!(m["char"]["error_symbol"], "x");
        assert_eq!(m["jobs"]["number_threshold"], 2);
        assert!(tildy_validates(&r.candidate));
    }

    #[test]
    fn right_format_fill_conda_and_commands() {
        let src = "format = \"$fill$conda\\n$character\"\nright_format = \"$directory$php\"\n[fill]\nsymbol = ' '\n";
        let r = imp(src);
        let rows = &r.candidate["layout"]["rows"];
        assert_eq!(rows[1]["right"], json!(["cwd", "php"]));
        let m = &r.candidate["modules"];
        assert_eq!(m["fill"], json!({ "type": "fill", "symbol": " ", "style": { "foreground": "black", "bold": true } }));
        assert_eq!(m["conda"]["type"], "env_var");
        assert_eq!(m["conda"]["show_when"]["value"], "base");
        assert_eq!(m["php"]["type"], "starship");
        let c = import_starship_with(src, &ImportOptions { palette: None, commands: true }).unwrap();
        assert_eq!(c.candidate["modules"]["php"]["type"], "command");
        assert_eq!(c.candidate["modules"]["php"]["format"], "via [$symbol($version )](bold fg:147)");
        assert!(tildy_validates(&c.candidate));
    }

    #[test]
    fn styled_status_symbols_keep_counts() {
        let src = "format = \"$git_status\"\n[git_status]\nahead = '[▴│[${count}](bold white)│](italic green)'\nbehind = '⇣${count}'\nstashed = '\\$'\n";
        let r = imp(src);
        let ss = &r.candidate["modules"]["git"]["status_symbols"];
        assert_eq!(ss["ahead"], "[▴│[$count](bold fg:white)│](italic fg:green)");
        assert_eq!(ss["behind"], "⇣${count}");
        assert_eq!(ss["stashed"], "$");
    }

    #[test]
    fn pua_moves_to_symbol() {
        let mut f = " \u{f43a} $time ".to_string();
        assert_eq!(extract_pua(&mut f).as_deref(), Some("\u{f43a}"));
        assert_eq!(f, " ($symbol )$time ");
        let mut g = "$symbol x".to_string();
        assert_eq!(extract_pua(&mut g), None);
    }

    #[test]
    fn e1_options_are_mapped() {
        let src = r#"
command_timeout = 900
scan_timeout = 10
continuation_prompt = "> "
format = "$username$hostname$directory$git_branch$git_status$cmd_duration$time$jobs$character"
[username]
aliases = { "corpuser034g" = "me" }
detect_env_vars = ["SSH_X"]
[hostname]
trim_at = "-"
aliases = { "box" = "home" }
detect_env_vars = ["!TMUX", "SSH_CONNECTION"]
format = "[$ssh_symbol$hostname]($style) "
[directory]
fish_style_pwd_dir_length = 1
use_logical_path = false
read_only = " RO"
repo_root_style = "bold red"
[git_branch]
truncation_length = 4
truncation_symbol = ""
always_show_remote = true
[git_status]
up_to_date = "✓"
ignore_submodules = true
typechanged = "T"
[git_state]
rebase = "RB"
[time]
disabled = false
utc_time_offset = "-5"
time_range = "10:00:00-14:00:00"
[jobs]
symbol_threshold = 0
[character]
vimcmd_symbol = "[V](green)"
"#;
        let r = imp(src);
        let c = &r.candidate;
        let m = &c["modules"];
        assert_eq!(c["command_timeout_ms"], 900);
        assert_eq!(m["user"]["aliases"], json!({ "corpuser034g": "me" }));
        assert_eq!(m["user"]["detect_env_vars"], json!(["SSH_X"]));
        assert_eq!(m["host"]["trim_at"], "-");
        assert_eq!(m["host"]["aliases"], json!({ "box": "home" }));
        assert_eq!(m["host"]["detect_env_vars"], json!(["!TMUX", "SSH_CONNECTION"]));
        assert_eq!(m["host"]["format"], "$ssh_symbol$host ");
        assert_eq!(m["cwd"]["fish_style_pwd_dir_length"], 1);
        assert_eq!(m["cwd"]["use_logical_path"], false);
        assert_eq!(m["cwd"]["read_only"], " RO");
        assert_eq!(m["cwd"]["repo_root_style"], json!({ "foreground": "red", "bold": true }));
        assert_eq!(m["git"]["truncation_length"], 4);
        assert_eq!(m["git"]["truncation_symbol"], "");
        assert_eq!(m["git"]["always_show_remote"], true);
        assert_eq!(m["git"]["ignore_submodules"], true);
        assert_eq!(m["git"]["status_symbols"]["up_to_date"], "✓");
        assert_eq!(m["git"]["state_labels"], json!({ "rebase": "RB" }));
        assert_eq!(m["time"]["utc_time_offset"], "-5");
        assert_eq!(m["time"]["time_range"], "10:00:00-14:00:00");
        assert_eq!(m["jobs"]["symbol_threshold"], 0);
        let items: Vec<&str> = r.unsupported.iter().map(|u| u.item.as_str()).collect();
        for i in ["scan_timeout", "continuation_prompt", "git_status.typechanged", "character.vimcmd_symbol"] {
            assert!(items.contains(&i), "{i} noted: {items:?}");
        }
        for i in ["command_timeout", "username.aliases", "hostname.trim_at", "directory.fish_style_pwd_dir_length", "git_branch.always_show_remote", "time.utc_time_offset"] {
            assert!(!items.contains(&i), "{i} mapped: {items:?}");
        }
        assert!(tildy_validates(c));
    }

    #[test]
    fn root_style_git_state_off_and_docker_everywhere() {
        let src = "format = \"$username$git_branch$git_state$docker_context\"\n[username]\nstyle_user = \"yellow\"\nstyle_root = \"bold red\"\n\
                   [git_state]\ndisabled = true\n[docker_context]\nonly_with_files = false\n";
        let r = imp(src);
        let m = &r.candidate["modules"];
        assert_eq!(m["user"]["rules"][0]["when"], json!({ "field": "session.username", "op": "eq", "value": "root" }));
        assert_eq!(m["user"]["rules"][0]["style"], json!({ "foreground": "red", "bold": true }));
        assert_eq!(m["git"]["show_state"], false);
        assert_eq!(m["docker"]["detect_files"], json!([]));
        assert!(r.unsupported.is_empty(), "{:?}", r.unsupported);
        assert!(tildy_validates(&r.candidate));
    }

    #[test]
    fn e1_modules_are_native() {
        let src = r#"
format = "$package$git_commit$git_metrics$shlvl$localip$memory_usage$sudo$direnv$nix_shell$shell$status"
[package]
display_private = true
[git_commit]
only_detached = false
tag_disabled = false
tag_max_candidates = 3
commit_hash_length = 9
[git_metrics]
disabled = false
only_nonzero_diffs = false
[shlvl]
disabled = false
threshold = 3
repeat = true
[localip]
disabled = false
ssh_only = false
[memory_usage]
disabled = false
threshold = -1
[sudo]
disabled = false
[direnv]
disabled = false
detect_env_vars = []
denied_msg = "[nope](red)"
[nix_shell]
impure_msg = "dirty"
heuristic = true
[shell]
disabled = false
zsh_indicator = "Z"
[status]
disabled = false
map_symbol = true
failure_style = "bold yellow"
pipestatus = true
"#;
        let r = imp(src);
        let m = &r.candidate["modules"];
        let ty = |id: &str| m[id]["type"].as_str().unwrap_or("").to_string();
        for (id, t) in [
            ("package", "package"), ("git_commit", "git_commit"), ("git_metrics", "git_metrics"), ("shlvl", "shlvl"),
            ("localip", "localip"), ("memory_usage", "memory_usage"), ("sudo", "sudo"), ("direnv", "direnv"),
            ("nix_shell", "nix_shell"), ("shell", "shell"), ("status", "exit_status"),
        ] {
            assert_eq!(ty(id), t, "{id}");
        }
        assert_eq!(m["package"]["display_private"], true);
        assert_eq!(m["package"]["format"], "is [$symbol$version](bold fg:208) ");
        assert_eq!(m["git_commit"]["tag_max_candidates"], 3);
        assert_eq!(m["git_commit"]["commit_hash_length"], 9);
        assert_eq!(m["git_metrics"]["format"], "([+$added](bold fg:green) )([-$deleted](bold fg:red) )");
        assert_eq!(m["shlvl"]["threshold"], 3);
        assert_eq!(m["localip"]["ssh_only"], false);
        assert_eq!(m["memory_usage"]["threshold"], -1);
        assert_eq!(m["direnv"]["detect_env_vars"], json!([]));
        assert_eq!(m["direnv"]["denied_msg"], "[nope](fg:red)");
        assert_eq!(m["nix_shell"]["impure_msg"], "dirty");
        assert_eq!(m["nix_shell"]["heuristic"], true);
        assert_eq!(m["shell"]["indicators"], json!({ "zsh": "Z" }));
        assert_eq!(m["status"]["map_symbol"], true);
        assert_eq!(m["status"]["error_style"], json!({ "foreground": "yellow", "bold": true }));
        assert!(r.unsupported.iter().any(|u| u.item == "status.pipestatus"));
        assert!(r.skipped.is_empty(), "{:?}", r.skipped);
        assert!(tildy_validates(&r.candidate));
        // Disabled by default in Starship: skipped unless enabled.
        let r = imp("format = \"$shlvl$sudo$memory_usage$package\"\n");
        assert_eq!(r.skipped, vec!["shlvl", "sudo", "memory_usage"]);
        assert_eq!(r.candidate["modules"]["package"]["type"], "package");
    }

    fn tildy_validates(c: &Value) -> bool {
        match crate::config::from_str(&c.to_string()) {
            Ok(_) => true,
            Err(e) => panic!("candidate does not validate: {e}\n{c:#}"),
        }
    }
}
