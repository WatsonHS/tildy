//! `format` templates.
//!
//! - `$name` or `${name}`: a variable. A `$` not followed by a name is literal.
//! - `(...)`: an optional group, dropped when no variable inside it (at any
//!   depth) has a non-empty value; a group without variables is always
//!   dropped. Groups nest.
//! - `[...](style)`: a styled span. `style` uses Starship's style syntax
//!   (`bold`, `dimmed`, `italic`, `underline`, `fg:COLOR`, `bg:COLOR`, a bare
//!   color = foreground; colors are names, `bright-<name>`, `purple`, 0-255 or
//!   `#rrggbb`). As in Starship, an inner span replaces the style of the spans
//!   around it (and a styled variable value, e.g. a symbol `[⬢](green)`,
//!   replaces the span it sits in); every span is drawn over the module's own
//!   `style`, so attributes the span does not set (a powerline segment's
//!   background, say) come from the module. Spans nest and may contain groups
//!   and vice versa.
//! - `\` escapes the next character (`\(`, `\)`, `\[`, `\]`, `\$`, `\\`).
//!
//! For compatibility with formats written before spans existed, a `[` whose
//! matching `]` is not immediately followed by `(` is a literal, and a `]`
//! outside a span is a literal.
use super::Style;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    Lit(String),
    Var(String),
    Group(Vec<Tok>),
    /// `[...](style)`: the inner tokens drawn in `style`.
    Span(Vec<Tok>, Style),
}

/// A piece of evaluated output. `style` is the innermost span style (`None`
/// = the module's own style); it is drawn over the module's style.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Run {
    pub text: String,
    pub style: Option<Style>,
}

impl Run {
    pub fn plain(text: impl Into<String>) -> Run {
        Run { text: text.into(), style: None }
    }
}

/// Concatenated text of runs.
pub fn text_of(runs: &[Run]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect()
}

fn is_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Starship color word -> tildy color string (validated by `parse_color`).
fn style_color(c: &str) -> Option<String> {
    let c = match c {
        "purple" => "magenta",
        "bright-purple" => "bright-magenta",
        other => other,
    };
    super::parse_color(c).map(|_| c.to_string())
}

/// Parse a Starship-style style string (`"bold fg:#769ff0 bg:#394260"`).
pub fn parse_style_spec(spec: &str) -> Result<Style, String> {
    let mut st = Style::default();
    for tok in spec.split_whitespace() {
        let l = tok.to_ascii_lowercase();
        match l.as_str() {
            "bold" => st.bold = Some(true),
            "dimmed" | "dim" => st.dim = Some(true),
            "italic" => st.italic = Some(true),
            "underline" => st.underline = Some(true),
            "none" => {}
            _ => {
                let (bg, c) = match (l.strip_prefix("fg:"), l.strip_prefix("bg:")) {
                    (Some(c), _) => (false, c),
                    (_, Some(c)) => (true, c),
                    _ => (false, l.as_str()),
                };
                let c = style_color(c).ok_or_else(|| {
                    format!(
                        "invalid style token '{tok}' (bold, dimmed, italic, underline, fg:COLOR, bg:COLOR or a color: \
                         names, bright-<name>, 0-255, #rrggbb)"
                    )
                })?;
                if bg {
                    st.background = Some(c);
                } else {
                    st.foreground = Some(c);
                }
            }
        }
    }
    Ok(st)
}

struct Parser {
    c: Vec<char>,
    i: usize,
    /// When set, record the char range of every span style spec and do not
    /// validate the specs ([`span_spec_ranges`]).
    specs: Option<Vec<(usize, usize)>>,
}

#[derive(Clone, Copy, PartialEq)]
enum End {
    Top,
    Group,
    Span,
}

impl Parser {
    /// Does the `[` at `self.i` open a span (matching `]` followed by `(`)?
    fn span_ahead(&self) -> bool {
        let mut depth = 0usize;
        let mut j = self.i;
        while j < self.c.len() {
            match self.c[j] {
                '\\' => j += 1,
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        return self.c.get(j + 1) == Some(&'(');
                    }
                }
                _ => {}
            }
            j += 1;
        }
        false
    }

    fn seq(&mut self, end: End) -> Result<Vec<Tok>, String> {
        let mut out: Vec<Tok> = vec![];
        let mut lit = String::new();
        fn flush(lit: &mut String, out: &mut Vec<Tok>) {
            if !lit.is_empty() {
                out.push(Tok::Lit(std::mem::take(lit)));
            }
        }
        while self.i < self.c.len() {
            let c = self.c[self.i];
            self.i += 1;
            match c {
                '\\' => match self.c.get(self.i) {
                    Some(&n) => {
                        lit.push(n);
                        self.i += 1;
                    }
                    None => return Err("trailing '\\' (escape the backslash as '\\\\')".into()),
                },
                '$' => {
                    let mut name = String::new();
                    if self.c.get(self.i) == Some(&'{') {
                        self.i += 1;
                        while let Some(&n) = self.c.get(self.i) {
                            self.i += 1;
                            if n == '}' {
                                break;
                            }
                            name.push(n);
                        }
                        if name.is_empty() || !name.chars().all(is_name) {
                            return Err(format!("invalid variable '${{{name}}}'"));
                        }
                    } else {
                        while let Some(&n) = self.c.get(self.i) {
                            if !is_name(n) {
                                break;
                            }
                            name.push(n);
                            self.i += 1;
                        }
                    }
                    if name.is_empty() {
                        lit.push('$');
                    } else {
                        flush(&mut lit, &mut out);
                        out.push(Tok::Var(name));
                    }
                }
                '(' => {
                    flush(&mut lit, &mut out);
                    let g = self.seq(End::Group)?;
                    out.push(Tok::Group(g));
                }
                ')' => {
                    if end == End::Group {
                        flush(&mut lit, &mut out);
                        return Ok(out);
                    }
                    return Err("unmatched ')' (escape it as '\\)')".into());
                }
                '[' => {
                    self.i -= 1;
                    if !self.span_ahead() {
                        self.i += 1;
                        lit.push('[');
                        continue;
                    }
                    self.i += 1;
                    flush(&mut lit, &mut out);
                    let inner = self.seq(End::Span)?;
                    // seq consumed the `]`; `(` follows (checked by span_ahead).
                    self.i += 1;
                    let mut spec = String::new();
                    let spec_start = self.i;
                    loop {
                        match self.c.get(self.i) {
                            Some(')') => {
                                self.i += 1;
                                break;
                            }
                            Some(&n) => {
                                spec.push(n);
                                self.i += 1;
                            }
                            None => return Err("unclosed span style '(...)'".into()),
                        }
                    }
                    let style = match &mut self.specs {
                        Some(ranges) => {
                            ranges.push((spec_start, spec_start + spec.chars().count()));
                            Style::default()
                        }
                        None => parse_style_spec(&spec)?,
                    };
                    out.push(Tok::Span(inner, style));
                }
                ']' if end == End::Span => {
                    flush(&mut lit, &mut out);
                    return Ok(out);
                }
                c => lit.push(c),
            }
        }
        match end {
            End::Top => {
                flush(&mut lit, &mut out);
                Ok(out)
            }
            End::Group => Err("unclosed '(' group".into()),
            End::Span => Err("unclosed '[' span".into()),
        }
    }
}

/// Parse a template. Errors are human-readable and position-free (the caller
/// adds the JSON path).
pub fn parse(src: &str) -> Result<Vec<Tok>, String> {
    Parser { c: src.chars().collect(), i: 0, specs: None }.seq(End::Top)
}

/// Char ranges `(start, end)` of the style spec of every `[...](spec)` span in
/// `src`, in source order. The specs themselves are not checked (palette
/// resolution rewrites them before the template is validated); other syntax
/// errors are returned as by [`parse`].
pub fn span_spec_ranges(src: &str) -> Result<Vec<(usize, usize)>, String> {
    let mut p = Parser { c: src.chars().collect(), i: 0, specs: Some(vec![]) };
    p.seq(End::Top)?;
    Ok(p.specs.unwrap_or_default())
}

/// Does the template use any span (and so need per-run styling)?
pub fn has_spans(toks: &[Tok]) -> bool {
    toks.iter().any(|t| match t {
        Tok::Span(..) => true,
        Tok::Group(g) => has_spans(g),
        _ => false,
    })
}

/// Does a string look like a styled template (contains a `[...](...)` span)?
/// Symbols use this to decide whether they are parsed as templates.
pub fn looks_styled(s: &str) -> bool {
    s.contains('[') && s.contains("](") && parse(s).is_ok_and(|t| has_spans(&t))
}

/// The style of text inside `outer` that carries its own `inner` style: the
/// inner one wins entirely (Starship's rule).
fn combine(outer: Option<&Style>, inner: Option<&Style>) -> Option<Style> {
    inner.or(outer).cloned()
}

fn push(out: &mut Vec<Run>, text: &str, style: Option<Style>) {
    if text.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(last) if last.style == style => last.text.push_str(text),
        _ => out.push(Run { text: text.to_string(), style }),
    }
}

type VarFn<'a> = dyn Fn(&str) -> Option<Vec<Run>> + 'a;

fn has_value(toks: &[Tok], var: &VarFn) -> bool {
    toks.iter().any(|t| match t {
        Tok::Var(n) => var(n).is_some_and(|r| r.iter().any(|r| !r.text.is_empty())),
        Tok::Group(g) | Tok::Span(g, _) => has_value(g, var),
        Tok::Lit(_) => false,
    })
}

fn eval_into(toks: &[Tok], var: &VarFn, style: Option<&Style>, out: &mut Vec<Run>) {
    for t in toks {
        match t {
            Tok::Lit(s) => push(out, s, style.cloned()),
            Tok::Var(n) => {
                for r in var(n).unwrap_or_default() {
                    push(out, &r.text, combine(style, r.style.as_ref()));
                }
            }
            Tok::Group(g) => {
                if has_value(g, var) {
                    eval_into(g, var, style, out);
                }
            }
            Tok::Span(inner, s) => {
                let st = combine(style, Some(s));
                eval_into(inner, var, st.as_ref(), out);
            }
        }
    }
}

/// Evaluate parsed tokens into styled runs. `var` returns a variable's runs;
/// unknown variables are empty.
pub fn eval_runs(toks: &[Tok], var: &VarFn) -> Vec<Run> {
    let mut out = vec![];
    eval_into(toks, var, None, &mut out);
    out
}

/// Evaluate parsed tokens to plain text (styles dropped).
pub fn eval(toks: &[Tok], var: &dyn Fn(&str) -> Option<String>) -> String {
    text_of(&eval_runs(toks, &|n| var(n).map(|s| vec![Run::plain(s)])))
}

/// Parse and evaluate to plain text; an invalid template renders literally
/// (validation rejects those before rendering, so this only guards odd callers).
pub fn render(src: &str, var: &dyn Fn(&str) -> Option<String>) -> String {
    match parse(src) {
        Ok(t) => eval(&t, var),
        Err(_) => src.to_string(),
    }
}

/// Parse and evaluate to runs; an invalid template is one literal run.
pub fn render_runs(src: &str, var: &VarFn) -> Vec<Run> {
    match parse(src) {
        Ok(t) => eval_runs(&t, var),
        Err(_) => vec![Run::plain(src)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(k: &str) -> Option<String> {
        match k {
            "symbol" => Some("S".into()),
            "version" => Some("v1".into()),
            "empty" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn variables_groups_and_escapes() {
        assert_eq!(render("$symbol( $version)", &vars), "S v1");
        assert_eq!(render("$symbol( $empty)", &vars), "S");
        assert_eq!(render("$symbol( $nope and $empty)x", &vars), "Sx");
        assert_eq!(render("( $empty $version)", &vars), "  v1");
        assert_eq!(render("(static)", &vars), "");
        assert_eq!(render("${symbol}x $ \\$ \\(a\\) \\\\", &vars), "Sx $ $ (a) \\");
        assert_eq!(render("♥ $time", &vars), "♥ ");
        assert_eq!(render("$symbol(\\($version\\))", &vars), "S(v1)");
        // Nested groups: the outer group shows when any inner variable has a value.
        assert_eq!(render("(via $symbol( $version))", &vars), "via S v1");
        assert_eq!(render("(via ( $empty))", &vars), "");
        assert_eq!(render("(a(b$empty)c$version)", &vars), "acv1");
    }

    #[test]
    fn brackets_without_style_stay_literal() {
        assert_eq!(render("[$version( @$nope)]", &vars), "[v1]");
        assert_eq!(render("a]b[c", &vars), "a]b[c");
        assert_eq!(render("\\[$symbol\\]", &vars), "[S]");
    }

    fn st(spec: &str) -> Option<Style> {
        Some(parse_style_spec(spec).unwrap())
    }

    #[test]
    fn spans_produce_styled_runs() {
        let v = |k: &str| vars(k).map(|s| vec![Run::plain(s)]);
        let r = render_runs("on [$symbol$version](bold purple) ", &v);
        assert_eq!(
            r,
            vec![Run::plain("on "), Run { text: "Sv1".into(), style: st("bold magenta") }, Run::plain(" ")]
        );
        // An inner span replaces the outer one; groups inside spans and spans inside groups.
        let r = render_runs("[a[b](red)c](bg:blue bold)", &v);
        assert_eq!(r[0], Run { text: "a".into(), style: st("bold bg:blue") });
        assert_eq!(r[1], Run { text: "b".into(), style: st("red") });
        assert_eq!(r[2], Run { text: "c".into(), style: st("bold bg:blue") });
        let r = render_runs("([\\[$empty\\]](red) )x", &v);
        assert_eq!(r, vec![Run::plain("x")]);
        let r = render_runs("(via [$symbol( $version )](green))", &v);
        assert_eq!(text_of(&r), "via S v1 ");
        // Styled variable values keep their own style inside the span.
        let sym = |k: &str| (k == "symbol").then(|| vec![Run { text: "⬢".into(), style: st("bold green") }, Run::plain(" ")]);
        let r = render_runs("[$symbol](italic)", &sym);
        assert_eq!(r[0], Run { text: "⬢".into(), style: st("bold green") });
        assert_eq!(r[1], Run { text: " ".into(), style: st("italic") });
        assert!(looks_styled("[⬢](bold green) "));
        assert!(!looks_styled("[x]"));
        assert!(!looks_styled("⬢"));
    }

    #[test]
    fn style_specs() {
        let s = parse_style_spec("bold fg:#769ff0 bg:purple dimmed italic underline").unwrap();
        assert_eq!(s.foreground.as_deref(), Some("#769ff0"));
        assert_eq!(s.background.as_deref(), Some("magenta"));
        assert_eq!((s.bold, s.dim, s.italic, s.underline), (Some(true), Some(true), Some(true), Some(true)));
        assert_eq!(parse_style_spec("218").unwrap().foreground.as_deref(), Some("218"));
        assert_eq!(parse_style_spec("bright-black").unwrap().foreground.as_deref(), Some("bright-black"));
        assert!(parse_style_spec("blink").is_err());
        assert!(parse_style_spec("fg:nope").is_err());
        assert_eq!(parse_style_spec("").unwrap(), Style::default());
    }

    #[test]
    fn syntax_errors() {
        assert!(parse("a)").unwrap_err().contains("unmatched"));
        assert!(parse("(a").unwrap_err().contains("unclosed"));
        assert!(parse("((a)").unwrap_err().contains("unclosed"));
        assert!(parse("a\\").unwrap_err().contains("trailing"));
        assert!(parse("${a-b}").unwrap_err().contains("invalid variable"));
        assert!(parse("[a](").unwrap_err().contains("unclosed span style"));
        assert!(parse("[a](wobbly)").unwrap_err().contains("invalid style token"));
        assert_eq!(parse("$a$b").unwrap(), vec![Tok::Var("a".into()), Tok::Var("b".into())]);
        assert!(parse("((a))").is_ok());
    }
}
