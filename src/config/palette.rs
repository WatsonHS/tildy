//! Palettes: named colors usable wherever a style takes a color. Owner: M2 task E2.
//!
//! Starship semantics: the top-level `palette` names the active palette and
//! `palettes` defines palettes (`{name: {color_name: color}}`); a style color
//! (a style object's `foreground`/`background`, or a token of a
//! `[...](style)` span) that names a color of the active palette is replaced by
//! that palette's value. Palette names win over the 16 color names, so a palette
//! may redefine `red`. Values are concrete colors (names, `bright-<name>`,
//! 0-255, `#rrggbb`); they cannot refer to other palette colors.
//!
//! Tildy adds built-in palettes (`palettes/*.json`), which behave as if they
//! were part of `palettes`: `{"palette": "nord"}` works without defining it,
//! and a user `palettes.nord` merges over the built-in one key by key.
//!
//! Every built-in palette defines the [`ROLES`] (what the role-based presets
//! `powerline`, `lean`, `classic`, `rainbow`, `minimal` are colored with),
//! `background`/`foreground` (the scheme's terminal colors) and the scheme's
//! own color names. See docs/palettes.md.
//!
//! Resolution happens when a config is built ([`resolve`]): the typed
//! `Config::file` sees concrete colors only, so the renderer is unchanged;
//! `Config::json` keeps the names as written.
use super::{parse_color, template, ConfigError, ErrorKind};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::fmt;

/// Role colors every built-in palette defines; role-based presets use only these.
///
/// - `bg1`…`bg6`: segment backgrounds, in segment order (powerline/rainbow):
///   `bg1`…`bg5` accents, `bg6` a neutral that ends the row (time);
/// - `fg`: text drawn on `bg1`…`bg6` and on `accent`/`ok`/`warn`/`err`/`info`
///   used as backgrounds;
/// - `fg_dim`: muted text (separators, time, stale data);
/// - `surface`: neutral segment background (classic), a step off the terminal
///   background;
/// - `accent`, `ok`, `warn`, `err`, `info`: text on the terminal background
///   and on `surface`.
pub const ROLES: &[&str] =
    &["bg1", "bg2", "bg3", "bg4", "bg5", "bg6", "fg", "fg_dim", "surface", "accent", "ok", "warn", "err", "info"];

/// Words of the span style syntax; not usable as palette color names.
const STYLE_WORDS: &[&str] = &["bold", "dimmed", "dim", "italic", "underline", "none"];

/// A built-in palette (`src/config/palettes/<name>.json`).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PaletteInfo {
    pub name: &'static str,
    /// One-line description (Chinese, shown by `tildy palette list`).
    pub description: &'static str,
    /// `dark` or `light`: the terminal background the scheme is made for.
    pub variant: &'static str,
    /// Where the colors come from.
    pub source: &'static str,
    #[serde(skip)]
    pub json: &'static str,
}

macro_rules! palette {
    ($name:literal, $variant:literal, $source:literal, $desc:literal) => {
        PaletteInfo {
            name: $name,
            description: $desc,
            variant: $variant,
            source: $source,
            json: include_str!(concat!("palettes/", $name, ".json")),
        }
    };
}

/// Built-in palettes in display order.
pub const PALETTE_INFO: &[PaletteInfo] = &[
    palette!("dracula", "dark", "https://draculatheme.com/spec", "Dracula: dark purple-gray with vivid pink, purple, cyan and green"),
    palette!("nord", "dark", "https://www.nordtheme.com/docs/colors-and-palettes", "Nord: cool blue-gray with frost and aurora accents"),
    palette!("solarized-dark", "dark", "https://ethanschoonover.com/solarized", "Solarized Dark: deep green with the classic low-contrast palette"),
    palette!("solarized-light", "light", "https://ethanschoonover.com/solarized", "Solarized Light: cream backgrounds for light terminals"),
    palette!("one-dark", "dark", "https://github.com/joshdick/onedark.vim", "One Dark: Atom-style gray with soft blue and purple"),
    palette!("rose-pine", "dark", "https://rosepinetheme.com/palette", "Rose Pine: dark purple with rose, gold and pine accents"),
    palette!("everforest", "dark", "https://github.com/sainnhe/everforest", "Everforest: muted forest green and warm accents"),
    palette!("kanagawa", "dark", "https://github.com/rebelot/kanagawa.nvim", "Kanagawa Wave: indigo with gold and blossom pink"),
    palette!("gruvbox-dark", "dark", "https://github.com/morhetz/gruvbox", "Gruvbox Dark: warm retro browns with orange, yellow and green"),
    palette!("tokyo-night", "dark", "https://github.com/folke/tokyonight.nvim", "Tokyo Night: midnight blue with neon blue and purple"),
    palette!("catppuccin-latte", "light", "https://github.com/catppuccin/palette", "Catppuccin Latte: for light terminals"),
    palette!("catppuccin-frappe", "dark", "https://github.com/catppuccin/palette", "Catppuccin Frappe: medium-dark"),
    palette!("catppuccin-macchiato", "dark", "https://github.com/catppuccin/palette", "Catppuccin Macchiato: dark"),
    palette!("catppuccin-mocha", "dark", "https://github.com/catppuccin/palette", "Catppuccin Mocha: deep pastel dark"),
];

/// A built-in palette by name.
pub fn builtin(name: &str) -> Option<&'static PaletteInfo> {
    PALETTE_INFO.iter().find(|p| p.name == name)
}

/// A JSON object's string entries in document order (`serde_json` sorts maps).
struct Ordered(Vec<(String, String)>);

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object of strings")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Ordered, A::Error> {
                let mut out = vec![];
                while let Some((k, v)) = m.next_entry::<String, String>()? {
                    out.push((k, v));
                }
                Ok(Ordered(out))
            }
        }
        d.deserialize_map(V)
    }
}

impl PaletteInfo {
    /// Colors in file order (`$comment` skipped).
    pub fn colors(&self) -> Vec<(String, String)> {
        let Ordered(v) = serde_json::from_str(self.json).expect("built-in palette is an object of strings");
        v.into_iter().filter(|(k, _)| k != "$comment").collect()
    }

    /// The scheme's terminal background (its `background` color).
    pub fn background(&self) -> String {
        self.colors().into_iter().find(|(k, _)| k == "background").map(|(_, v)| v).unwrap_or_default()
    }
}

/// An effective palette: color names and their concrete colors, in order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Palette {
    pub name: String,
    pub colors: Vec<(String, String)>,
}

impl Palette {
    /// A color by name: an exact match, else one that differs only in ASCII
    /// case (span style tokens are case-insensitive).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.colors
            .iter()
            .find(|(k, _)| k == name)
            .or_else(|| self.colors.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)))
            .map(|(_, v)| v.as_str())
    }

    fn set(&mut self, name: &str, color: String) {
        match self.colors.iter_mut().find(|(k, _)| k == name) {
            Some(e) => e.1 = color,
            None => self.colors.push((name.to_string(), color)),
        }
    }

    /// Role names this palette does not define.
    pub fn missing_roles(&self) -> Vec<&'static str> {
        ROLES.iter().copied().filter(|r| self.get(r).is_none()).collect()
    }
}

fn err(path: impl Into<String>, msg: impl Into<String>) -> ConfigError {
    ConfigError { kind: ErrorKind::Invalid, path: path.into(), message: msg.into() }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() { key.to_string() } else { format!("{path}.{key}") }
}

/// Palette names: 1-64 chars of `[A-Za-z0-9_-]`.
pub fn valid_palette_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Color names: a letter or `_`, then `[A-Za-z0-9_-]`, at most 64 chars, not a
/// style word (`bold`, ...).
pub fn valid_color_name(s: &str) -> bool {
    s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !STYLE_WORDS.contains(&s.to_ascii_lowercase().as_str())
}

/// A concrete color as stored in a palette: anything `parse_color` accepts,
/// plus Starship's `purple`/`bright-purple` (= magenta).
pub fn concrete(s: &str) -> Option<String> {
    let s = match s {
        "purple" => "magenta",
        "bright-purple" => "bright-magenta",
        other => other,
    };
    parse_color(s).map(|_| s.to_string())
}

/// A standard (non-palette) color word of the span syntax.
fn standard(word: &str) -> bool {
    concrete(&word.to_ascii_lowercase()).is_some()
}

/// Color names and concrete colors, in order.
type Colors = Vec<(String, String)>;

/// Check the `palettes` table; returns it as (name, colors).
fn user_palettes(root: &Map<String, Value>) -> Result<Vec<(String, Colors)>, ConfigError> {
    let Some(v) = root.get("palettes") else { return Ok(vec![]) };
    let m = v.as_object().ok_or_else(|| err("palettes", "expected an object {palette name: {color name: color}}"))?;
    let mut out = vec![];
    for (name, p) in m {
        let pp = join("palettes", name);
        if name == "$comment" {
            if !p.is_string() { return Err(err(pp, "expected a string")); }
            continue;
        }
        if !valid_palette_name(name) {
            return Err(err(pp, "palette name must be 1-64 chars of [A-Za-z0-9_-]"));
        }
        let pm = p.as_object().ok_or_else(|| err(&pp, "expected an object {color name: color}"))?;
        let mut colors = vec![];
        for (c, val) in pm {
            let cp = join(&pp, c);
            let Some(s) = val.as_str() else { return Err(err(cp, "expected a string")) };
            if c == "$comment" { continue; }
            if !valid_color_name(c) {
                return Err(err(cp, "color name must start with a letter or '_', use [A-Za-z0-9_-] (at most 64), and not be a style word (bold, dimmed, italic, underline, none)"));
            }
            let v = concrete(s).ok_or_else(|| {
                err(&cp, format!("invalid color '{s}' (names, bright-<name>, grey, 0-255 or #rrggbb; palette colors cannot refer to other palette colors)"))
            })?;
            colors.push((c.clone(), v));
        }
        out.push((name.clone(), colors));
    }
    Ok(out)
}

/// The effective palette of a merged config (None when `palette` is unset):
/// the built-in palette of that name, overlaid with `palettes.<name>`.
pub fn effective(json: &Value) -> Result<Option<Palette>, ConfigError> {
    let Some(root) = json.as_object() else { return Ok(None) };
    let users = user_palettes(root)?;
    let name = match root.get("palette") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(err("palette", "expected a palette name (string)")),
    };
    let user = users.iter().find(|(n, _)| n == name);
    let mut p = Palette { name: name.to_string(), colors: builtin(name).map(|b| b.colors()).unwrap_or_default() };
    match (builtin(name).is_some(), user) {
        (false, None) => {
            let mut known: Vec<&str> = PALETTE_INFO.iter().map(|p| p.name).collect();
            known.extend(users.iter().map(|(n, _)| n.as_str()));
            return Err(err("palette", format!("unknown palette '{name}' (known: {}; define it under `palettes`)", known.join(", "))));
        }
        (_, Some((_, colors))) => {
            for (k, v) in colors {
                p.set(k, v.clone());
            }
        }
        (true, None) => {}
    }
    Ok(Some(p))
}

fn unknown(name: &str, pal: Option<&Palette>) -> String {
    match pal {
        Some(p) => format!("unknown color '{name}': not a color name and not defined in palette '{}' (see `tildy palette show {}`)", p.name, p.name),
        None => format!(
            "unknown color '{name}': not a color name (names, bright-<name>, grey, 0-255 or #rrggbb), and no `palette` is set to define it"
        ),
    }
}

/// A style object's color: Some(replacement) for a palette color, an error
/// for a name that is neither a color nor in the palette, None otherwise
/// (concrete colors, and malformed values left to the validator).
fn resolve_color(s: &str, pal: Option<&Palette>) -> Result<Option<String>, String> {
    if let Some(c) = pal.and_then(|p| p.get(s)) {
        return Ok(Some(c.to_string()));
    }
    if parse_color(s).is_none() && valid_color_name(s) {
        return Err(unknown(s, pal));
    }
    Ok(None)
}

fn resolve_style(v: &mut Value, path: &str, pal: Option<&Palette>) -> Result<(), ConfigError> {
    let Some(m) = v.as_object_mut() else { return Ok(()) };
    for k in ["foreground", "background"] {
        if let Some(Value::String(s)) = m.get_mut(k) {
            if let Some(c) = resolve_color(s, pal).map_err(|e| err(join(path, k), e))? {
                *s = c;
            }
        }
    }
    Ok(())
}

/// Rewrite the palette colors of one span style spec (`bold fg:bg1`).
/// `strict`: an unknown color name is an error; otherwise it stays.
fn rewrite_spec(spec: &str, pal: Option<&Palette>, strict: bool) -> Result<String, String> {
    let mut out = vec![];
    let mut changed = false;
    for tok in spec.split_whitespace() {
        let lower = tok.to_ascii_lowercase();
        if STYLE_WORDS.contains(&lower.as_str()) {
            out.push(tok.to_string());
            continue;
        }
        let (prefix, color) = match (lower.starts_with("fg:"), lower.starts_with("bg:")) {
            (true, _) | (_, true) => tok.split_at(3),
            _ => ("", tok),
        };
        match pal.and_then(|p| p.get(color)) {
            Some(c) => {
                out.push(format!("{prefix}{c}"));
                changed = true;
            }
            None => {
                if strict && !standard(color) && valid_color_name(color) {
                    return Err(format!("{} in span style '({spec})'", unknown(color, pal)));
                }
                out.push(tok.to_string());
            }
        }
    }
    Ok(if changed { out.join(" ") } else { spec.to_string() })
}

/// Rewrite the span styles of a template (or a symbol that may hold spans).
/// Ok(None): nothing to change (or not a template; the validator reports
/// syntax errors of `format` fields).
fn rewrite_template(src: &str, pal: Option<&Palette>, strict: bool) -> Result<Option<String>, String> {
    if !src.contains("](") {
        return Ok(None);
    }
    let Ok(ranges) = template::span_spec_ranges(src) else { return Ok(None) };
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::new();
    let mut last = 0;
    let mut changed = false;
    for (a, b) in ranges {
        out.extend(&chars[last..a]);
        let spec: String = chars[a..b].iter().collect();
        let new = rewrite_spec(&spec, pal, strict)?;
        changed |= new != spec;
        out.push_str(&new);
        last = b;
    }
    out.extend(&chars[last..]);
    Ok(changed.then_some(out))
}

/// Symbol-like strings anywhere inside `v` (plain strings, tiered symbols,
/// symbol maps): palette colors in their spans are replaced, nothing fails.
fn rewrite_strings(v: &mut Value, pal: Option<&Palette>) {
    match v {
        Value::String(s) => {
            if let Ok(Some(n)) = rewrite_template(s, pal, false) {
                *s = n;
            }
        }
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                if k != "$comment" {
                    rewrite_strings(x, pal);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| rewrite_strings(x, pal)),
        _ => {}
    }
}

/// Module fields that are never templates or symbols.
const NOT_STYLED: &[&str] = &[
    "$comment", "type", "argv", "regex", "detect_files", "detect_extensions", "detect_folders", "variable", "module", "language",
    "time_format", "version_format", "ignore_branches", "show_when", "state_labels",
];
/// Template fields: an unknown color in a span is an error with the field's path.
const TEMPLATES: &[&str] = &["format", "repo_root_format"];

fn resolve_module(m: &mut Map<String, Value>, path: &str, pal: Option<&Palette>) -> Result<(), ConfigError> {
    for (k, v) in m.iter_mut() {
        let p = join(path, k);
        if NOT_STYLED.contains(&k.as_str()) {
            continue;
        }
        if k == "rules" {
            for (i, r) in v.as_array_mut().into_iter().flatten().enumerate() {
                if let Some(s) = r.get_mut("style") {
                    resolve_style(s, &format!("{p}[{i}].style"), pal)?;
                }
            }
        } else if k.ends_with("style") && v.is_object() {
            resolve_style(v, &p, pal)?;
        } else if TEMPLATES.contains(&k.as_str()) {
            if let Value::String(s) = v {
                if let Some(n) = rewrite_template(s, pal, true).map_err(|e| err(&p, e))? {
                    *s = n;
                }
            }
        } else {
            rewrite_strings(v, pal);
        }
    }
    Ok(())
}

/// Resolve palette colors in a merged config. Returns the JSON for the typed
/// view: palette colors replaced by their values and `$comment` entries
/// dropped from `palettes`. Errors carry JSON paths (`palette`,
/// `palettes.<name>.<color>`, `modules.<id>.style.foreground`, ...).
pub(crate) fn resolve(effective_json: &Value) -> Result<Value, ConfigError> {
    let pal = effective(effective_json)?;
    let mut out = effective_json.clone();
    let Some(root) = out.as_object_mut() else { return Ok(out) };
    if let Some(Value::Object(ps)) = root.get_mut("palettes") {
        ps.remove("$comment");
        for p in ps.values_mut() {
            if let Some(m) = p.as_object_mut() {
                m.remove("$comment");
            }
        }
    }
    let pal = pal.as_ref();
    if let Some(Value::Object(mods)) = root.get_mut("modules") {
        for (id, m) in mods.iter_mut() {
            if let Some(mm) = m.as_object_mut() {
                resolve_module(mm, &join("modules", id), pal)?;
            }
        }
    }
    if let Some(rows) = root.get_mut("layout").and_then(|l| l.get_mut("rows")).and_then(Value::as_array_mut) {
        for r in rows {
            for k in ["separator", "subseparator", "start", "end"] {
                if let Some(v) = r.get_mut(k) {
                    rewrite_strings(v, pal);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtin_palettes_define_every_role_with_concrete_colors() {
        let mut names: Vec<&str> = PALETTE_INFO.iter().map(|p| p.name).collect();
        names.sort();
        let mut files: Vec<String> = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src/config/palettes"))
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok()?.strip_suffix(".json").map(String::from))
            .collect();
        files.sort();
        assert_eq!(names, files, "every palette file is registered");
        for p in PALETTE_INFO {
            let colors = p.colors();
            let pal = Palette { name: p.name.into(), colors: colors.clone() };
            assert!(pal.missing_roles().is_empty(), "{}: missing {:?}", p.name, pal.missing_roles());
            for k in ["background", "foreground"] {
                assert!(pal.get(k).is_some(), "{}: {k}", p.name);
            }
            for (k, v) in &colors {
                assert!(valid_color_name(k), "{}: name {k}", p.name);
                assert!(v.starts_with('#') && concrete(v).is_some(), "{}: {k} = {v} (built-ins use #rrggbb)", p.name);
            }
            let mut seen = std::collections::HashSet::new();
            assert!(colors.iter().all(|(k, _)| seen.insert(k.clone())), "{}: duplicate names", p.name);
            assert!(["dark", "light"].contains(&p.variant) && !p.description.is_empty() && p.source.starts_with("https://"));
            assert_eq!(p.background(), pal.get("background").unwrap());
        }
        // File order is kept (roles first).
        assert_eq!(builtin("nord").unwrap().colors()[0].0, "bg1");
        assert_eq!(builtin("catppuccin-mocha").unwrap().colors().last().unwrap().0, "crust");
    }

    #[test]
    fn names_and_concrete_colors() {
        assert!(valid_color_name("bg1") && valid_color_name("_x") && valid_color_name("nord3_bright") && valid_color_name("a-b"));
        assert!(!valid_color_name("1x") && !valid_color_name("bold") && !valid_color_name("fg:x") && !valid_color_name(""));
        assert_eq!(concrete("purple").as_deref(), Some("magenta"));
        assert_eq!(concrete("#AbCdEf").as_deref(), Some("#AbCdEf"));
        assert!(concrete("bg1").is_none() && concrete("256").is_none());
        assert!(valid_palette_name("catppuccin_mocha") && !valid_palette_name("a b"));
    }

    fn cfg(v: Value) -> Result<Value, ConfigError> {
        resolve(&v)
    }

    #[test]
    fn styles_spans_and_symbols_resolve() {
        let v = json!({
            "palette": "mine",
            "palettes": {"$comment": "x", "mine": {"$comment": "y", "brand": "#112233", "red": "#ff0000", "Mixed": "purple"}},
            "modules": {"m": {
                "type": "text", "style": {"foreground": "brand", "background": "red", "bold": true},
                "success_style": {"foreground": "green"},
                "rules": [{"id": "r", "when": {"field": "a", "op": "eq", "value": "[x](brand)"}, "style": {"background": "brand"}}],
                "format": "[a](bold fg:brand bg:RED)[b](mixed) [c](fg:green)",
                "text": {"nerd": "[N](brand)", "ascii": "[A](nope)"},
                "status_symbols": {"modified": "[!$count](fg:brand)"}
            }},
            "layout": {"rows": [{"left": ["m"], "separator": "[|](brand)"}]}
        });
        let out = cfg(v).unwrap();
        let m = &out["modules"]["m"];
        assert_eq!(m["style"], json!({"foreground": "#112233", "background": "#ff0000", "bold": true}));
        assert_eq!(m["success_style"]["foreground"], "green");
        assert_eq!(m["rules"][0]["style"]["background"], "#112233");
        assert_eq!(m["rules"][0]["when"]["value"], "[x](brand)", "conditions are data, not templates");
        assert_eq!(m["format"], "[a](bold fg:#112233 bg:#ff0000)[b](magenta) [c](fg:green)");
        assert_eq!(m["text"], json!({"nerd": "[N](#112233)", "ascii": "[A](nope)"}), "symbols never fail");
        assert_eq!(m["status_symbols"]["modified"], "[!$count](fg:#112233)");
        assert_eq!(out["layout"]["rows"][0]["separator"], "[|](#112233)");
        assert_eq!(out["palettes"], json!({"mine": {"brand": "#112233", "red": "#ff0000", "Mixed": "purple"}}));
    }

    #[test]
    fn builtin_palettes_merge_with_user_overrides() {
        let v = json!({"palette": "nord", "palettes": {"nord": {"accent": "#000001", "extra": "1"}},
            "modules": {"a": {"type": "text", "style": {"foreground": "accent", "background": "nord0"}}, "b": {"type": "text", "style": {"foreground": "extra"}}}});
        let out = cfg(v.clone()).unwrap();
        assert_eq!(out["modules"]["a"]["style"], json!({"foreground": "#000001", "background": "#2E3440"}));
        assert_eq!(out["modules"]["b"]["style"]["foreground"], "1");
        let p = effective(&v).unwrap().unwrap();
        assert_eq!(p.get("accent"), Some("#000001"));
        assert_eq!(p.colors.last().unwrap().0, "extra");
        assert!(effective(&json!({})).unwrap().is_none());
    }

    #[test]
    fn errors_carry_paths() {
        let path = |v: Value| cfg(v).unwrap_err().path;
        assert_eq!(path(json!({"palette": "neon"})), "palette");
        assert!(cfg(json!({"palette": "neon"})).unwrap_err().message.contains("catppuccin-mocha"));
        assert_eq!(path(json!({"palette": 3})), "palette");
        assert_eq!(path(json!({"palettes": []})), "palettes");
        assert_eq!(path(json!({"palettes": {"a b": {}}})), "palettes.a b");
        assert_eq!(path(json!({"palettes": {"p": {"bold": "#000000"}}})), "palettes.p.bold");
        assert_eq!(path(json!({"palettes": {"p": {"x": "y"}}})), "palettes.p.x");
        assert_eq!(path(json!({"palettes": {"p": {"x": 1}}})), "palettes.p.x");
        let m = |body: Value| json!({"palette": "nord", "modules": {"x": body}});
        assert_eq!(path(m(json!({"type": "text", "style": {"foreground": "bg9"}}))), "modules.x.style.foreground");
        assert_eq!(path(m(json!({"type": "text", "rules": [{"id": "a", "style": {"background": "nope"}}]}))), "modules.x.rules[0].style.background");
        assert_eq!(path(m(json!({"type": "git", "state_style": {"foreground": "nope"}}))), "modules.x.state_style.foreground");
        assert_eq!(path(m(json!({"type": "text", "format": "[a](fg:nope)"}))), "modules.x.format");
        let e = cfg(m(json!({"type": "text", "format": "[a](fg:nope)"}))).unwrap_err();
        assert!(e.message.contains("palette 'nord'"), "{}", e.message);
        // Without a palette the message says so; malformed colors are left to the validator.
        let e = cfg(json!({"modules": {"x": {"type": "text", "style": {"foreground": "accent"}}}})).unwrap_err();
        assert!(e.message.contains("no `palette`"), "{}", e.message);
        assert!(cfg(json!({"modules": {"x": {"type": "text", "style": {"foreground": "#12"}}}})).is_ok());
    }

    #[test]
    fn role_presets_use_only_roles_and_take_every_palette() {
        use crate::config::{from_str, preset, PRESET_INFO, ROLE_PRESETS};
        for name in ROLE_PRESETS {
            let info = PRESET_INFO.iter().find(|p| p.name == *name).unwrap();
            let default = info.palette().unwrap_or_else(|| panic!("{name}: no default palette"));
            assert!(builtin(&default).is_some(), "{name}: {default}");
            assert_eq!(preset(name).unwrap().file.palette.as_deref(), Some(default.as_str()));
            // A palette with nothing but the roles is enough.
            let roles: Map<String, Value> = ROLES.iter().map(|r| (r.to_string(), json!("#010203"))).collect();
            let user = json!({"schema_version": 1, "preset": name, "palette": "roles", "palettes": {"roles": roles}});
            from_str(&user.to_string()).unwrap_or_else(|e| panic!("{name}: uses a non-role color: {e}"));
            for p in PALETTE_INFO {
                let c = from_str(&format!(r#"{{"schema_version":1,"preset":"{name}","palette":"{}"}}"#, p.name))
                    .unwrap_or_else(|e| panic!("{name} + {}: {e}", p.name));
                for (id, m) in &c.file.modules {
                    for color in [&m.style.foreground, &m.style.background].into_iter().flatten() {
                        assert!(parse_color(color).is_some(), "{name} + {}: {id}: {color}", p.name);
                    }
                }
            }
        }
        for p in PRESET_INFO.iter().filter(|p| !ROLE_PRESETS.contains(&p.name)) {
            assert!(p.palette().is_none(), "{}: fixed-color presets set no palette", p.name);
        }
        // One line recolors: powerline's directory segment is bg2.
        let bg = |pal: &str| {
            let c = from_str(&format!(r#"{{"schema_version":1,"preset":"powerline"{pal}}}"#)).unwrap();
            c.file.modules["cwd"].style.background.clone().unwrap()
        };
        assert_eq!(bg(""), builtin("catppuccin-mocha").unwrap().colors().iter().find(|(k, _)| k == "bg2").unwrap().1);
        assert_eq!(bg(r#","palette":"nord""#), "#BF616A");
        // The user layer may tweak one role of a built-in palette.
        assert_eq!(bg(r##","palette":"nord","palettes":{"nord":{"bg2":"#123456"}}"##), "#123456");
        // `Config::json` keeps the names.
        let c = from_str(r#"{"schema_version":1,"preset":"powerline","palette":"nord"}"#).unwrap();
        assert_eq!(c.json["modules"]["cwd"]["style"]["background"], "bg2");
    }

    #[test]
    fn palette_fixtures_load_or_fail_with_paths() {
        let fx = |n: &str| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(n);
        let c = crate::config::load(Some(&fx("config.v1.valid-palette.json"))).unwrap();
        let m = &c.file.modules;
        assert_eq!(m["time"].style.background.as_deref(), Some("#ff8800"));
        assert_eq!(m["time"].style.foreground.as_deref(), Some("#2E3440"));
        assert_eq!(m["time"].format.as_deref(), Some("[$time](bold fg:#ECEFF4)"));
        assert_eq!(m["cwd"].style.background.as_deref(), Some("#5E81AC"), "palettes.nord overrides bg2");
        assert_eq!(m["git"].style.background.as_deref(), Some("#D08770"), "other roles from the built-in nord");
        let c = crate::config::load(Some(&fx("config.v1.valid-palette-user.json"))).unwrap();
        assert_eq!(c.file.modules["cwd"].style.foreground.as_deref(), Some("magenta"));
        assert_eq!(c.file.modules["result"].style.foreground.as_deref(), Some("#e06c75"), "palette wins over the 16 names");
        assert_eq!(c.file.palettes["mine"]["dim_text"], "244");
        let path = |n: &str| crate::config::load(Some(&fx(n))).unwrap_err().downcast::<ConfigError>().unwrap().path;
        assert_eq!(path("config.v1.invalid-palette-value.json"), "palettes.mine.accent");
        assert_eq!(path("config.v1.invalid-palette-unknown.json"), "palette");
        assert_eq!(path("config.v1.invalid-palette-color-name.json"), "palettes.mine.bold");
    }

    #[test]
    fn nothing_changes_without_palette_colors() {
        let v = json!({"modules": {"x": {"type": "text", "style": {"foreground": "red"}, "format": "[a](bold green) $x", "text": "[q](wobbly)"}}});
        assert_eq!(cfg(v.clone()).unwrap(), v);
    }
}
