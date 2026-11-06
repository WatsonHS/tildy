//! Starship `format` strings and style strings.
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// One top-level element of a Starship `format`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    Module(String),
    Newline,
    /// `[text](style)` whose text contains no module references.
    Group { text: String, style: String },
    /// Bare literal text.
    Literal(String),
}

/// Tokenize a top-level format. `notes` receives (item, reason) pairs for
/// constructs that cannot be represented.
pub fn tokenize(fmt: &str, notes: &mut Vec<(String, String)>) -> Vec<Tok> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = Vec::new();
    let mut lit = String::new();
    let flush = |lit: &mut String, out: &mut Vec<Tok>| {
        if !lit.is_empty() {
            out.push(Tok::Literal(std::mem::take(lit)));
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                lit.push(chars[i + 1]);
                i += 2;
            }
            '\n' => {
                flush(&mut lit, &mut out);
                out.push(Tok::Newline);
                i += 1;
            }
            '$' => {
                let braced = chars.get(i + 1) == Some(&'{');
                let begin = i + if braced { 2 } else { 1 };
                let mut j = begin;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let prefix: String = chars[begin..j].iter().collect();
                if matches!(prefix.as_str(), "custom" | "env_var") && chars.get(j) == Some(&'.')
                    && chars.get(j + 1).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
                    j += 1;
                    while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') { j += 1; }
                }
                if j == begin || (braced && chars.get(j) != Some(&'}')) {
                    lit.push('$');
                    i += 1;
                    continue;
                }
                flush(&mut lit, &mut out);
                let name: String = chars[begin..j].iter().collect();
                out.push(Tok::Module(name));
                i = j + usize::from(braced);
            }
            '[' => {
                if let Some((inner, style, next)) = parse_group(&chars, i) {
                    flush(&mut lit, &mut out);
                    if inner.contains('$') {
                        // A style wrapper around module references: keep the modules.
                        let nested = tokenize(&inner, notes);
                        if nested.iter().any(|t| matches!(t, Tok::Module(_))) {
                            notes.push((
                                format!("format: [{inner}]({style})"),
                                "style wrapper around modules is ignored; the modules keep their own styles".into(),
                            ));
                        }
                        out.extend(nested);
                    } else {
                        out.push(Tok::Group { text: inner, style });
                    }
                    i = next;
                } else {
                    lit.push('[');
                    i += 1;
                }
            }
            '(' | ')' => {
                // Conditional groups at the top level: flatten.
                i += 1;
            }
            c => {
                lit.push(c);
                i += 1;
            }
        }
    }
    flush(&mut lit, &mut out);
    out
}

/// Parse `[inner](style)` starting at `chars[start] == '['`. Returns
/// (inner, style, index after the closing paren).
fn parse_group(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let mut depth = 0usize;
    let mut i = start;
    let mut close = None;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let close = close?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let mut j = close + 2;
    let mut pdepth = 1usize;
    while j < chars.len() {
        match chars[j] {
            '(' => pdepth += 1,
            ')' => {
                pdepth -= 1;
                if pdepth == 0 {
                    let inner: String = chars[start + 1..close].iter().collect();
                    let style: String = chars[close + 2..j].iter().collect();
                    return Some((inner, style, j + 1));
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

// ---------------------------------------------------------------- format trees

/// A parsed Starship format string (module formats, symbols).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum El {
    Lit(String),
    Var(String),
    /// `(...)`: shown when a variable inside has a value.
    Group(Vec<El>),
    /// `[...](style)`.
    Span(Vec<El>, String),
}

/// Parse a Starship format. Lenient: a `[` without `](style)` and an
/// unmatched `)` are literals, an unclosed `(` closes at the end.
pub fn parse_tree(fmt: &str) -> Vec<El> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    tree_seq(&chars, &mut i, false)
}

fn tree_seq(chars: &[char], i: &mut usize, in_group: bool) -> Vec<El> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let flush = |lit: &mut String, out: &mut Vec<El>| {
        if !lit.is_empty() {
            out.push(El::Lit(std::mem::take(lit)));
        }
    };
    while *i < chars.len() {
        let c = chars[*i];
        match c {
            '\\' if *i + 1 < chars.len() => {
                lit.push(chars[*i + 1]);
                *i += 2;
            }
            '$' => {
                let mut j = *i + 1;
                let braced = chars.get(j) == Some(&'{');
                if braced {
                    j += 1;
                }
                let s = j;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j == s || (braced && chars.get(j) != Some(&'}')) {
                    lit.push('$');
                    *i += 1;
                    continue;
                }
                flush(&mut lit, &mut out);
                out.push(El::Var(chars[s..j].iter().collect()));
                *i = if braced { j + 1 } else { j };
            }
            '[' => match parse_group(chars, *i) {
                Some((inner, style, next)) => {
                    flush(&mut lit, &mut out);
                    out.push(El::Span(parse_tree(&inner), style));
                    *i = next;
                }
                None => {
                    lit.push('[');
                    *i += 1;
                }
            },
            '(' => {
                flush(&mut lit, &mut out);
                *i += 1;
                let g = tree_seq(chars, i, true);
                out.push(El::Group(g));
            }
            ')' if in_group => {
                *i += 1;
                flush(&mut lit, &mut out);
                return out;
            }
            c => {
                lit.push(c);
                *i += 1;
            }
        }
    }
    flush(&mut lit, &mut out);
    out
}

/// Plain text of a tree (variables dropped, groups and spans inlined).
pub fn tree_text(els: &[El]) -> String {
    let mut s = String::new();
    for e in els {
        match e {
            El::Lit(t) => s.push_str(t),
            El::Var(_) => {}
            El::Group(g) | El::Span(g, _) => s.push_str(&tree_text(g)),
        }
    }
    s
}

/// Does the tree use any variable?
pub fn tree_has_vars(els: &[El]) -> bool {
    els.iter().any(|e| match e {
        El::Var(_) => true,
        El::Group(g) | El::Span(g, _) => tree_has_vars(g),
        El::Lit(_) => false,
    })
}

/// Does the tree contain a span?
pub fn tree_has_spans(els: &[El]) -> bool {
    els.iter().any(|e| match e {
        El::Span(..) => true,
        El::Group(g) => tree_has_spans(g),
        _ => false,
    })
}

/// Replace spans by their content (styles dropped), keeping groups.
pub fn tree_unstyled(els: &[El]) -> Vec<El> {
    let mut out = Vec::new();
    for e in els {
        match e {
            El::Span(inner, _) => out.extend(tree_unstyled(inner)),
            El::Group(g) => out.push(El::Group(tree_unstyled(g))),
            other => out.push(other.clone()),
        }
    }
    out
}

/// Styles of the spans that wrap all visible content, outermost first
/// (`[[ $x ](inner)]($style)` -> `["$style", "inner"]`).
pub fn wrapping_styles(els: &[El]) -> Vec<String> {
    let visible: Vec<&El> = els.iter().filter(|e| !matches!(e, El::Lit(t) if t.trim().is_empty())).collect();
    match visible.as_slice() {
        [El::Span(inner, style)] => {
            let mut v = vec![style.clone()];
            v.extend(wrapping_styles(inner));
            v
        }
        _ => vec![],
    }
}

/// Does the output end with whitespace whenever it ends with a literal (the
/// last element is a literal ending in whitespace, looking inside groups and
/// spans)?
pub fn ends_ws(els: &[El]) -> bool {
    match els.last() {
        Some(El::Lit(t)) => t.ends_with(char::is_whitespace),
        Some(El::Group(g)) | Some(El::Span(g, _)) => ends_ws(g),
        _ => false,
    }
}

/// Escape literal text for a tildy template.
pub fn escape_template(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if matches!(c, '\\' | '$' | '(' | ')' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Serialize a tree as a tildy template. `var` maps a Starship variable to a
/// tildy one (None = dropped); `style` maps a span style string to a tildy
/// style spec (None or empty = no span, content kept).
pub fn to_tildy(els: &[El], var: &mut dyn FnMut(&str) -> Option<String>, style: &mut dyn FnMut(&str) -> Option<String>) -> String {
    let mut out = String::new();
    for (i, e) in els.iter().enumerate() {
        match e {
            El::Lit(t) => out.push_str(&escape_template(t)),
            El::Var(v) => {
                if let Some(n) = var(v) {
                    let next_is_name = match els.get(i + 1) {
                        Some(El::Lit(t)) => t.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'),
                        _ => false,
                    };
                    if next_is_name {
                        out.push_str(&format!("${{{n}}}"));
                    } else {
                        out.push('$');
                        out.push_str(&n);
                    }
                }
            }
            El::Group(g) => {
                let inner = to_tildy(g, var, style);
                if !inner.is_empty() {
                    out.push('(');
                    out.push_str(&inner);
                    out.push(')');
                }
            }
            El::Span(inner, st) => {
                let body = to_tildy(inner, var, style);
                if body.is_empty() {
                    continue;
                }
                match style(st).filter(|s| !s.is_empty()) {
                    Some(spec) => out.push_str(&format!("[{body}]({spec})")),
                    None => out.push_str(&body),
                }
            }
        }
    }
    out
}

/// Is `s` a powerline separator glyph (Powerline/Nerd arrows, U+E0B0..U+E0BF)?
pub fn separator_glyph(text: &str) -> Option<char> {
    let t = text.trim();
    let mut cs = t.chars();
    let c = cs.next()?;
    if cs.next().is_none() && ('\u{e0b0}'..='\u{e0bf}').contains(&c) {
        Some(c)
    } else {
        None
    }
}

pub fn has_pua(s: &str) -> bool {
    s.chars().any(|c| {
        ('\u{e000}'..='\u{f8ff}').contains(&c) || ('\u{f0000}'..='\u{ffffd}').contains(&c)
    })
}

// ---------------------------------------------------------------- styles

/// Parsed Starship style string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StarshipStyle {
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    /// Attributes that have no tildy equivalent (inverted, blink, …).
    pub unsupported: Vec<String>,
}

const NAMED: &[(&str, &str)] = &[
    ("black", "black"),
    ("red", "red"),
    ("green", "green"),
    ("yellow", "yellow"),
    ("blue", "blue"),
    ("purple", "magenta"),
    ("magenta", "magenta"),
    ("cyan", "cyan"),
    ("white", "white"),
    ("bright-black", "8"),
    ("bright-red", "9"),
    ("bright-green", "10"),
    ("bright-yellow", "11"),
    ("bright-blue", "12"),
    ("bright-purple", "13"),
    ("bright-magenta", "13"),
    ("bright-cyan", "14"),
    ("bright-white", "15"),
];

/// A Starship palette entry as a tildy palette color: Some(color) when tildy
/// can keep it by name (valid name, concrete value).
pub fn palette_color(name: &str, value: &str) -> Option<String> {
    crate::config::palette::valid_color_name(name).then(|| color(value, &BTreeMap::new())).flatten()
}

fn color(raw: &str, palette: &BTreeMap<String, String>) -> Option<String> {
    let raw = raw.trim();
    if let Some(p) = palette.get(raw) {
        // Palette colors stay by name (the candidate keeps `palettes`); a name
        // tildy cannot keep is resolved (palette entries are never recursive).
        if palette_color(raw, p).is_some() {
            return Some(raw.to_string());
        }
        return color(p, &BTreeMap::new());
    }
    if let Some(h) = raw.strip_prefix('#') {
        if h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(format!("#{}", h.to_ascii_uppercase()));
        }
        return None;
    }
    if let Ok(n) = raw.parse::<u16>() {
        return (n <= 255).then(|| n.to_string());
    }
    NAMED.iter().find(|(k, _)| *k == raw).map(|(_, v)| v.to_string())
}

pub fn parse_style(s: &str, palette: &BTreeMap<String, String>) -> StarshipStyle {
    let mut st = StarshipStyle::default();
    for tok in s.split_whitespace() {
        let lower = tok.to_ascii_lowercase();
        match lower.as_str() {
            "bold" => st.bold = true,
            "dimmed" => st.dim = true,
            "italic" => st.italic = true,
            "underline" => st.underline = true,
            "none" => {}
            "inverted" | "blink" | "hidden" | "strikethrough" => st.unsupported.push(lower),
            _ => {
                if let Some(c) = lower.strip_prefix("fg:").or_else(|| tok.strip_prefix("fg:")) {
                    match color(c, palette) {
                        Some(c) => st.fg = Some(c),
                        None => st.unsupported.push(tok.to_string()),
                    }
                } else if let Some(c) = tok.strip_prefix("bg:") {
                    match color(c, palette) {
                        Some(c) => st.bg = Some(c),
                        None => st.unsupported.push(tok.to_string()),
                    }
                } else {
                    match color(tok, palette) {
                        Some(c) => st.fg = Some(c),
                        None => st.unsupported.push(tok.to_string()),
                    }
                }
            }
        }
    }
    st
}

impl StarshipStyle {
    /// The style as a tildy span spec (`bold italic fg:#769FF0 bg:magenta`).
    pub fn to_spec(&self) -> String {
        let mut v: Vec<String> = vec![];
        if self.bold { v.push("bold".into()); }
        if self.dim { v.push("dimmed".into()); }
        if self.italic { v.push("italic".into()); }
        if self.underline { v.push("underline".into()); }
        if let Some(f) = &self.fg { v.push(format!("fg:{f}")); }
        if let Some(b) = &self.bg { v.push(format!("bg:{b}")); }
        v.join(" ")
    }

    /// Tildy style object, or None when empty.
    pub fn to_json(&self) -> Option<Value> {
        let mut m = Map::new();
        if let Some(f) = &self.fg {
            m.insert("foreground".into(), json!(f));
        }
        if let Some(b) = &self.bg {
            m.insert("background".into(), json!(b));
        }
        if self.bold {
            m.insert("bold".into(), json!(true));
        }
        if self.dim {
            m.insert("dim".into(), json!(true));
        }
        if self.italic {
            m.insert("italic".into(), json!(true));
        }
        if self.underline {
            m.insert("underline".into(), json!(true));
        }
        (!m.is_empty()).then_some(Value::Object(m))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_modules_newlines_and_groups() {
        let mut n = vec![];
        let t = tokenize("$a[\u{e0b0}](bg:#111 fg:#222)$b\n$character", &mut n);
        assert_eq!(
            t,
            vec![
                Tok::Module("a".into()),
                Tok::Group { text: "\u{e0b0}".into(), style: "bg:#111 fg:#222".into() },
                Tok::Module("b".into()),
                Tok::Newline,
                Tok::Module("character".into()),
            ]
        );
    }

    #[test]
    fn style_parsing() {
        let p = BTreeMap::new();
        let s = parse_style("bold fg:#abcdef bg:purple", &p);
        assert_eq!(s.fg.as_deref(), Some("#ABCDEF"));
        assert_eq!(s.bg.as_deref(), Some("magenta"));
        assert!(s.bold);
        let s = parse_style("bright-red inverted", &p);
        assert_eq!(s.fg.as_deref(), Some("9"));
        assert_eq!(s.unsupported, vec!["inverted"]);
    }

    #[test]
    fn separator_detection() {
        assert_eq!(separator_glyph(" \u{e0b0} "), Some('\u{e0b0}'));
        assert_eq!(separator_glyph("x"), None);
    }
}
