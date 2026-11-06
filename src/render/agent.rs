//! `agent_status` module rendering: per-state symbols and styles, `$msg`
//! truncation, and the `agent.*` fields that `show_when` and `rules` see.
//! Owner: M2 task B. The module itself is rendered by `render_module`.
//!
//! Template variables: every provider field (`state`, `kind`, `app`,
//! `title`, `msg`, `progress`, `id`, `age_s`, `scope`, `count`, `blocked`,
//! `error`, `done`, `working`, `idle`, `more`) plus `$symbol` and `$text`.
//! `$msg` is cut to `max_width` columns (default 40) with the tier's
//! ellipsis; `$more` is empty when there are no other records, so
//! `( +$more)` disappears.

use super::{sanitize, tier3, FieldVal, Fields};
use crate::config::{ModuleConfig, Style, SymbolTier, AGENT_DEFAULT_MAX_WIDTH};
use crate::model::ModuleValue;
use unicode_segmentation::UnicodeSegmentation;

/// Built-in symbol per state and tier.
pub(super) fn default_symbol(state: &str, t: SymbolTier) -> &'static str {
    match state {
        "working" => tier3(t, "\u{f051f}", "◐", "~"),
        "blocked" => tier3(t, "\u{f059}", "⏳", "?"),
        "error" => tier3(t, "\u{f057}", "✘", "x"),
        "done" => tier3(t, "\u{f058}", "✔", "ok"),
        "idle" => tier3(t, "\u{f04b2}", "·", "-"),
        _ => "",
    }
}

/// `$symbol`: `state_symbols[state]`, else the module's `symbol`, else the
/// built-in symbol of the state.
pub(super) fn symbol(m: &ModuleConfig, v: &ModuleValue, t: SymbolTier) -> String {
    let state = v.fields.get("state").map(String::as_str).unwrap_or("");
    if let Some(s) = m.state_symbols.as_ref().and_then(|s| s.get(state)) {
        return s.resolve(t).to_string();
    }
    match &m.symbol {
        Some(s) => s.resolve(t).to_string(),
        None => default_symbol(state, t).to_string(),
    }
}

/// Default color per state (blocked yellow, error red, working cyan, done
/// green, idle grey) under the module's own `style`; `rules` apply on top.
pub(super) fn style(m: &ModuleConfig, state: Option<&str>) -> Style {
    let fg = match state {
        Some("blocked") => Some("yellow"),
        Some("error") => Some("red"),
        Some("working") => Some("cyan"),
        Some("done") => Some("green"),
        Some("idle") => Some("grey"),
        _ => None,
    };
    let mut s = Style { foreground: fg.map(str::to_string), ..Style::default() };
    s.overlay(&m.style);
    s
}

/// `s` cut to at most `max` display columns, ending in `ellipsis` when cut.
pub(super) fn truncate_cols(s: &str, max: usize, ellipsis: &str) -> String {
    if crate::width::width(s) <= max {
        return s.to_string();
    }
    let room = max.saturating_sub(crate::width::width(ellipsis));
    let mut out = String::new();
    let mut w = 0;
    for c in s.graphemes(true) {
        let cw = crate::width::width(c);
        if w + cw > room {
            break;
        }
        out.push_str(c);
        w += cw;
    }
    let out = out.trim_end().to_string();
    if max < crate::width::width(ellipsis) { out } else { out + ellipsis }
}

/// Template variables of the module (see the module docs).
pub(super) fn vars(m: &ModuleConfig, v: &ModuleValue, ellipsis: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = v.fields.iter().map(|(k, val)| (k.clone(), val.clone())).collect();
    out.push(("text".into(), v.text.clone()));
    let more = v.fields.get("more").filter(|n| n.as_str() != "0").cloned().unwrap_or_default();
    out.push(("more".into(), more));
    let max = m.max_width.unwrap_or(AGENT_DEFAULT_MAX_WIDTH) as usize;
    let msg = v.fields.get("msg").map(|s| truncate_cols(&sanitize(s), max, ellipsis)).unwrap_or_default();
    out.push(("msg".into(), msg));
    out
}

/// Fields for `show_when`/`rules` of an agent_status module: the prompt's
/// fields plus `agent.<field>` for every provider field (counts, `progress`
/// and `age_s` as numbers, the rest as strings), e.g. `agent.state`.
pub(super) fn rule_fields(base: &Fields, v: &ModuleValue) -> Fields {
    const NUMERIC: &[&str] = &["count", "blocked", "error", "done", "working", "idle", "more", "progress", "age_s"];
    let mut f = base.clone();
    for (k, val) in &v.fields {
        let fv = match val.parse::<f64>() {
            Ok(n) if NUMERIC.contains(&k.as_str()) => FieldVal::Num(n),
            _ => FieldVal::Str(val.clone()),
        };
        f.insert(format!("agent.{k}"), fv);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, SymbolTier};
    use crate::model::{Freshness, PromptData, Slot};
    use crate::render::{render, visible_width};
    use std::collections::BTreeMap;

    fn data(slot: Option<Slot<ModuleValue>>) -> PromptData {
        let mut d: PromptData = serde_json::from_value(serde_json::json!({
            "request": {"gen":1,"session_id":"s","cwd":"/Users/u/code","cols":80,"lines":24,"last_exit":0,"last_duration_ms":0,"env":""},
            "home": "/Users/u", "git": {"freshness":"fresh","age_ms":0,"value":null,"error":null},
            "ssh": false, "hostname": "h", "username": "u", "modules": {}
        }))
        .unwrap();
        if let Some(s) = slot {
            d.modules.insert("agent".into(), s);
        }
        d
    }

    fn value(pairs: &[(&str, &str)]) -> Slot<ModuleValue> {
        let fields: BTreeMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Slot::fresh(ModuleValue { text: fields.get("msg").cloned().unwrap_or_default(), fields, prerendered: None })
    }

    fn blocked() -> Slot<ModuleValue> {
        value(&[("state", "blocked"), ("kind", "permission"), ("app", "claude"), ("msg", "Bash: rm -rf target"), ("more", "1"), ("count", "2")])
    }

    /// Opt-in config: the built-in look plus an `agent` module on the right.
    fn agent_cfg() -> config::Config {
        config::from_str(r#"{"schema_version":1,"preset":"plain","add_newline":false,"modules":{"agent":{"type":"agent_status"}},"layout":{"rows":[{"left":["cwd","git","jobs"],"right":["agent","elapsed","result"]}]}}"#).unwrap()
    }

    fn report(o: &crate::render::RenderOutput) -> &crate::model::ModuleReport {
        o.reports.iter().find(|r| r.id == "agent").unwrap()
    }

    #[test]
    fn builtin_right_side_per_tier() {
        let c = agent_cfg();
        for (tier, want) in [
            (SymbolTier::Unicode, "%F{yellow}⏳ claude Bash: rm -rf target +1%f"),
            (SymbolTier::Ascii, "%F{yellow}? claude Bash: rm -rf target +1%f"),
            (SymbolTier::Nerd, "%F{yellow}\u{f059} claude Bash: rm -rf target +1%f"),
        ] {
            let o = render(&c.clone().with_symbols(tier), &data(Some(blocked())));
            assert_eq!(o.prompt.right.as_deref(), Some(want), "{tier:?}");
            assert!(report(&o).visible);
        }
        let o = render(&c.clone().with_symbols(SymbolTier::Unicode), &data(Some(blocked())));
        assert_eq!(report(&o).columns, visible_width(o.prompt.right.as_deref().unwrap()));
        assert_eq!(report(&o).columns, 2 + 1 + "claude Bash: rm -rf target +1".len(), "⏳ is two columns wide");
    }

    #[test]
    fn hidden_without_records_and_per_state_defaults() {
        let c = agent_cfg().with_symbols(SymbolTier::Unicode);
        let none = Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: None, error: None };
        assert_eq!(report(&render(&c, &data(Some(none)))).hidden_reason.as_deref(), Some("not_detected"));
        assert_eq!(report(&render(&c, &data(None))).hidden_reason.as_deref(), Some("no_data"));
        assert_eq!(render(&c, &data(None)).prompt.right, None);
        for (state, sym, color) in [("working", "◐", "cyan"), ("error", "✘", "red"), ("done", "✔", "green"), ("idle", "·", "8")] {
            let o = render(&c, &data(Some(value(&[("state", state), ("app", "codex"), ("more", "0")]))));
            assert_eq!(o.prompt.right, Some(format!("%F{{{color}}}{sym} codex%f")), "{state}: no msg, no +0");
        }
    }

    #[test]
    fn msg_truncation_and_hostile_text() {
        let c = agent_cfg().with_symbols(SymbolTier::Unicode);
        let long = "x".repeat(60);
        let o = render(&c, &data(Some(value(&[("state", "done"), ("msg", &long)]))));
        assert_eq!(report(&o).text, format!("✔ {}…", "x".repeat(39)));
        let c2 = config::from_str(r#"{"schema_version":1,"preset":"plain","add_newline":false,"modules":{"agent":{"type":"agent_status","max_width":10}},"layout":{"rows":[{"left":["cwd"],"right":["agent"]}]}}"#).unwrap().with_symbols(SymbolTier::Ascii);
        let o = render(&c2, &data(Some(value(&[("state", "done"), ("msg", "日本語のメッセージです")]))));
        assert_eq!(report(&o).text, "ok 日本語...");
        let o = render(&c, &data(Some(value(&[("state", "error"), ("msg", "100% \u{1b}[31mred\u{202e}")]))));
        assert_eq!(o.prompt.right.as_deref(), Some("%F{red}✘ 100%% [31mred%f"));
        assert_eq!(truncate_cols("abcdef", 4, "…"), "abc…");
        assert_eq!(truncate_cols("ab  cdef", 4, "…"), "ab…");
        assert_eq!(truncate_cols("abcd", 4, "…"), "abcd");
        assert_eq!(truncate_cols("日本語", 4, "…"), "日…");
        assert_eq!(truncate_cols("abcdef", 2, "..."), "");
    }

    #[test]
    fn state_symbols_rules_and_custom_format() {
        let c = config::from_str(
            r##"{"schema_version":1,"preset":"plain","add_newline":false,"layout":{"rows":[{"left":["cwd"],"right":["agent"]}]},"modules":{"agent":{"type":"agent_status","symbol":"A","state_symbols":{"blocked":{"unicode":"!!","ascii":"!"}},
               "format":"$symbol $state/$kind( $progress%)( [$count])",
               "rules":[{"id":"perm","when":{"field":"agent.kind","op":"eq","value":"permission"},"style":{"foreground":"magenta","bold":true}},
                        {"id":"many","when":{"field":"agent.count","op":"ge","value":2},"style":{"underline":true}}]}}}"##,
        )
        .unwrap()
        .with_symbols(SymbolTier::Unicode);
        let o = render(&c, &data(Some(blocked())));
        assert_eq!(o.prompt.right.as_deref(), Some("%B%U%F{magenta}!! blocked/permission [2]%f%u%b"));
        assert_eq!(report(&o).rules_hit, vec!["perm", "many"]);
        let o = render(&c, &data(Some(value(&[("state", "working"), ("progress", "40"), ("count", "1")]))));
        assert_eq!(report(&o).text, "A working/ 40% [1]");
        let c = config::from_str(r#"{"schema_version":1,"preset":"plain","add_newline":false,"layout":{"rows":[{"left":["cwd"],"right":["agent"]}]},"modules":{"agent":{"type":"agent_status","show_when":{"field":"agent.state","op":"in","value":["blocked","error"]}}}}"#)
            .unwrap()
            .with_symbols(SymbolTier::Unicode);
        assert!(report(&render(&c, &data(Some(blocked())))).visible);
        let o = render(&c, &data(Some(value(&[("state", "working")]))));
        assert_eq!(report(&o).hidden_reason.as_deref(), Some("show_when:agent.state"));
    }

    #[test]
    fn config_validation() {
        let path = |src: &str| config::from_str(src).unwrap_err().path;
        let m = |body: &str| format!(r#"{{"schema_version":1,"preset":"plain","add_newline":false,"modules":{{"agent":{body}}}}}"#);
        assert_eq!(path(&m(r#"{"type":"agent_status","state_symbols":{"sleeping":"z"}}"#)), "modules.agent.state_symbols.sleeping");
        assert_eq!(path(&m(r#"{"type":"agent_status","state_symbols":{"done":3}}"#)), "modules.agent.state_symbols.done");
        assert_eq!(path(&m(r#"{"type":"agent_status","max_width":0}"#)), "modules.agent.max_width");
        assert_eq!(path(&m(r#"{"type":"agent_status","max_width":"40"}"#)), "modules.agent.max_width");
        assert_eq!(path(&m(r#"{"type":"jobs","max_width":4}"#)), "modules.agent.max_width");
        assert_eq!(path(&m(r#"{"type":"git","state_symbols":{}}"#)), "modules.agent.state_symbols");
        let c = config::from_str(&m(r#"{"type":"agent_status","max_width":12,"state_symbols":{"idle":"z","done":{"nerd":"N","ascii":"d"}}}"#)).unwrap();
        assert_eq!(c.file.modules["agent"].max_width, Some(12));
        assert_eq!(c.file.modules["agent"].state_symbols.as_ref().unwrap()["done"].resolve(SymbolTier::Ascii), "d");
        assert_eq!(config::AGENT_STATES, crate::agent::State::ALL.map(|s| s.as_str()));
    }
}
