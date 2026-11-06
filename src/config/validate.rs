//! Hand-written validation of the merged config JSON. Errors carry a JSON path
//! such as `modules.git_main.colour` or `layout.rows[0].left[1]`.
use super::{
    ConfigError, ErrorKind, AGENT_STATES, GIT_OPERATIONS, HISTORY_MODES, LANGUAGES, MODULE_TYPES, OPERATORS, PRESETS, ROW_STYLES,
    STATUS_SYMBOL_KEYS, SYMBOL_SETTINGS, TIME_SPECIFIERS,
};
use crate::model::SCHEMA_VERSION;
use serde_json::{Map, Value};
use std::collections::HashSet;

fn join(path: &str, key: &str) -> String {
    if path.is_empty() { key.to_string() } else { format!("{path}.{key}") }
}

fn err(path: impl Into<String>, msg: impl Into<String>) -> ConfigError {
    ConfigError { kind: ErrorKind::Invalid, path: path.into(), message: msg.into() }
}

fn obj<'a>(v: &'a Value, path: &str) -> Result<&'a Map<String, Value>, ConfigError> {
    v.as_object().ok_or_else(|| err(path, "expected an object"))
}

fn arr<'a>(v: &'a Value, path: &str) -> Result<&'a Vec<Value>, ConfigError> {
    v.as_array().ok_or_else(|| err(path, "expected an array"))
}

fn string<'a>(v: &'a Value, path: &str) -> Result<&'a str, ConfigError> {
    v.as_str().ok_or_else(|| err(path, "expected a string"))
}

fn boolean(v: &Value, path: &str) -> Result<(), ConfigError> {
    if v.is_boolean() { Ok(()) } else { Err(err(path, "expected a boolean")) }
}

fn keys(m: &Map<String, Value>, path: &str, allowed: &[&str]) -> Result<(), ConfigError> {
    for (k, v) in m {
        if k == "$comment" {
            if !v.is_string() {
                return Err(err(join(path, k), "expected a string"));
            }
            continue;
        }
        if !allowed.contains(&k.as_str()) {
            return Err(err(join(path, k), format!("unknown field (allowed: {})", allowed.join(", "))));
        }
    }
    Ok(())
}

fn one_of(v: &Value, path: &str, set: &[&str]) -> Result<(), ConfigError> {
    let s = string(v, path)?;
    if set.contains(&s) { Ok(()) } else { Err(err(path, format!("invalid value '{s}' (expected one of: {})", set.join(", ")))) }
}

fn check_color(v: &Value, path: &str) -> Result<(), ConfigError> {
    let s = string(v, path)?;
    if super::parse_color(s).is_some() { Ok(()) } else {
        Err(err(path, format!("invalid color '{s}' (names, bright-<name>, grey, 0-255 or #rrggbb)")))
    }
}

fn check_style(v: &Value, path: &str) -> Result<(), ConfigError> {
    let m = obj(v, path)?;
    keys(m, path, &["foreground", "background", "bold", "dim", "italic", "underline"])?;
    for k in ["foreground", "background"] {
        if let Some(c) = m.get(k) { check_color(c, &join(path, k))?; }
    }
    for k in ["bold", "dim", "italic", "underline"] {
        if let Some(b) = m.get(k) { boolean(b, &join(path, k))?; }
    }
    Ok(())
}

/// A symbol-like value: a string, or `{nerd, unicode, ascii}` with string values.
fn check_sym(v: &Value, path: &str) -> Result<(), ConfigError> {
    match v {
        Value::String(_) => Ok(()),
        Value::Object(m) => {
            keys(m, path, &["nerd", "unicode", "ascii"])?;
            for k in ["nerd", "unicode", "ascii"] {
                if let Some(s) = m.get(k) { string(s, &join(path, k))?; }
            }
            Ok(())
        }
        _ => Err(err(path, "expected a string or an object {nerd, unicode, ascii}")),
    }
}

fn check_condition(v: &Value, path: &str) -> Result<(), ConfigError> {
    let m = obj(v, path)?;
    keys(m, path, &["field", "op", "value"])?;
    let field = m.get("field").ok_or_else(|| err(join(path, "field"), "required"))?;
    if string(field, &join(path, "field"))?.is_empty() {
        return Err(err(join(path, "field"), "must not be empty"));
    }
    let op = m.get("op").ok_or_else(|| err(join(path, "op"), "required"))?;
    one_of(op, &join(path, "op"), OPERATORS)?;
    let value = m.get("value").ok_or_else(|| err(join(path, "value"), "required"))?;
    let vp = join(path, "value");
    match op.as_str().unwrap() {
        "gt" | "lt" | "ge" | "le" => {
            if !value.is_number() { return Err(err(vp, "ordering operators need a number")); }
        }
        "in" => {
            let a = arr(value, &vp)?;
            for (i, e) in a.iter().enumerate() {
                if !(e.is_string() || e.is_number() || e.is_boolean()) {
                    return Err(err(format!("{vp}[{i}]"), "expected a scalar"));
                }
            }
        }
        _ => {
            if !(value.is_string() || value.is_number() || value.is_boolean()) {
                return Err(err(vp, "expected a scalar"));
            }
        }
    }
    Ok(())
}

fn check_time_format(s: &str, path: &str) -> Result<(), ConfigError> {
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '%' { continue; }
        match it.next() {
            Some(n) if TIME_SPECIFIERS.contains(&n) => {}
            Some(n) => {
                let list: String = TIME_SPECIFIERS.iter().map(|c| format!("%{c}")).collect::<Vec<_>>().join(" ");
                return Err(err(path, format!("unsupported specifier '%{n}' (supported: {list})")));
            }
            None => return Err(err(path, "trailing '%' (write '%%' for a literal percent)")),
        }
    }
    Ok(())
}

fn check_string_list(v: &Value, path: &str) -> Result<(), ConfigError> {
    for (i, e) in arr(v, path)?.iter().enumerate() {
        if e.as_str().is_none_or(|s| s.is_empty()) {
            return Err(err(format!("{path}[{i}]"), "expected a non-empty string"));
        }
    }
    Ok(())
}

/// Fields every module accepts.
const COMMON: &[&str] = &["type", "priority", "style", "rules", "show_when", "format", "symbol", "padding", "icon_padding"];
/// Type-specific fields: (field, types that accept it).
/// Module types with a provider that runs something (timeouts, cache TTL).
const PROVIDER_TYPES: &[&str] = &[
    "command", "language", "python", "docker_context", "starship", "package", "git_commit", "git_metrics", "localip",
    "memory_usage", "sudo", "direnv",
];
/// Module types with file-based detection (`detect_files`, ...).
const DETECT_TYPES: &[&str] = &["command", "language", "python", "docker_context", "starship", "direnv"];
const SPECIFIC: &[(&str, &[&str])] = &[
    ("success_style", &["character", "exit_status"]),
    ("error_style", &["character", "exit_status"]),
    ("error_symbol", &["character"]),
    ("text", &["text"]),
    ("os_symbols", &["os"]),
    ("variable", &["env_var"]),
    ("number_threshold", &["jobs"]),
    ("symbol_threshold", &["jobs"]),
    ("show_always", &["username"]),
    ("time_format", &["time"]),
    ("use_12hr", &["time"]),
    ("utc_time_offset", &["time"]),
    ("time_range", &["time"]),
    ("truncation_length", &["directory", "git"]),
    ("truncation_symbol", &["directory", "git"]),
    ("home_symbol", &["directory"]),
    ("substitutions", &["directory"]),
    ("truncate_to_repo", &["directory"]),
    ("repo_root_format", &["directory"]),
    ("read_only", &["directory"]),
    ("read_only_style", &["directory"]),
    ("fish_style_pwd_dir_length", &["directory"]),
    ("use_logical_path", &["directory"]),
    ("repo_root_style", &["directory"]),
    ("before_repo_root_style", &["directory"]),
    ("ignore_branches", &["git"]),
    ("only_attached", &["git"]),
    ("always_show_remote", &["git"]),
    ("ignore_submodules", &["git", "git_metrics"]),
    ("branch_symbol", &["git"]),
    ("status_symbols", &["git"]),
    ("show_counts", &["git"]),
    ("show_state", &["git"]),
    ("state_labels", &["git"]),
    ("state_style", &["git"]),
    ("min_time", &["duration"]),
    ("show_milliseconds", &["duration"]),
    ("trim_at", &["hostname"]),
    ("aliases", &["hostname", "username"]),
    ("detect_env_vars", &["hostname", "username", "direnv"]),
    ("success_symbol", &["exit_status"]),
    ("not_executable_symbol", &["exit_status"]),
    ("not_found_symbol", &["exit_status"]),
    ("sigint_symbol", &["exit_status"]),
    ("signal_symbol", &["exit_status"]),
    ("map_symbol", &["exit_status"]),
    ("recognize_signal_code", &["exit_status"]),
    ("display_private", &["package"]),
    ("commit_hash_length", &["git_commit"]),
    ("only_detached", &["git_commit"]),
    ("tag_disabled", &["git_commit"]),
    ("tag_max_candidates", &["git_commit"]),
    ("tag_symbol", &["git_commit"]),
    ("added_style", &["git_metrics"]),
    ("deleted_style", &["git_metrics"]),
    ("only_nonzero_diffs", &["git_metrics"]),
    ("threshold", &["shlvl", "memory_usage"]),
    ("repeat", &["shlvl"]),
    ("repeat_offset", &["shlvl"]),
    ("ssh_only", &["localip"]),
    ("allowed_msg", &["direnv"]),
    ("not_allowed_msg", &["direnv"]),
    ("denied_msg", &["direnv"]),
    ("loaded_msg", &["direnv"]),
    ("unloaded_msg", &["direnv"]),
    ("pure_msg", &["nix_shell"]),
    ("impure_msg", &["nix_shell"]),
    ("unknown_msg", &["nix_shell"]),
    ("heuristic", &["nix_shell"]),
    ("indicators", &["shell"]),
    ("unknown_indicator", &["shell"]),
    ("language", &["language"]),
    ("version_format", &["language", "python", "command", "package"]),
    ("module", &["starship"]),
    ("starship_config", &["starship"]),
    ("detect_files", DETECT_TYPES),
    ("detect_extensions", DETECT_TYPES),
    ("detect_folders", DETECT_TYPES),
    ("argv", &["command"]),
    ("regex", &["command"]),
    ("timeout_ms", PROVIDER_TYPES),
    ("cache_ttl_s", PROVIDER_TYPES),
    ("state_symbols", &["agent_status"]),
    ("max_width", &["agent_status"]),
];

/// `HH:MM:SS` (24-hour clock).
pub(crate) fn parse_hms(s: &str) -> Option<u32> {
    let mut it = s.split(':');
    let (h, m, sec) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || [h, m, sec].iter().any(|p| p.len() != 2 || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let (h, m, sec): (u32, u32, u32) = (h.parse().ok()?, m.parse().ok()?, sec.parse().ok()?);
    (h < 24 && m < 60 && sec < 60).then_some(h * 3600 + m * 60 + sec)
}

/// Starship's `time_range`: `START-END`, either side `HH:MM:SS` or empty.
/// Returns (start, end) in seconds of the day; `None` means invalid.
pub(crate) fn parse_time_range(s: &str) -> Option<(Option<u32>, Option<u32>)> {
    if s.matches('-').count() != 1 {
        return None;
    }
    let (a, b) = s.split_once('-')?;
    let side = |p: &str| if p.is_empty() { Some(None) } else { parse_hms(p).map(Some) };
    Some((side(a)?, side(b)?))
}

/// Starship's `utc_time_offset`: `local`, or hours in (-24, 24) (fractions allowed).
pub(crate) fn parse_utc_offset(s: &str) -> Option<Option<i64>> {
    if s == "local" {
        return Some(None);
    }
    let h: f64 = s.trim().parse().ok()?;
    (h.is_finite() && h > -24.0 && h < 24.0).then_some(Some((h * 3600.0) as i64))
}

fn check_module(id: &str, v: &Value, path: &str) -> Result<(), ConfigError> {
    let m = obj(v, path)?;
    let mut allowed: Vec<&str> = COMMON.to_vec();
    allowed.extend(SPECIFIC.iter().map(|(k, _)| *k));
    keys(m, path, &allowed)?;
    let ty = m.get("type").ok_or_else(|| err(join(path, "type"), "required"))?;
    one_of(ty, &join(path, "type"), MODULE_TYPES)?;
    let ty = ty.as_str().unwrap();
    for (k, types) in SPECIFIC {
        if m.contains_key(*k) && !types.contains(&ty) {
            return Err(err(join(path, k), format!("only valid for {} modules", types.join("/"))));
        }
    }
    let p = |k: &str| join(path, k);
    if let Some(v) = m.get("priority") {
        if !v.is_i64() { return Err(err(p("priority"), "expected an integer")); }
    }
    for k in [
        "style", "success_style", "error_style", "state_style", "read_only_style", "repo_root_style", "before_repo_root_style",
        "added_style", "deleted_style",
    ] {
        if let Some(s) = m.get(k) { check_style(s, &p(k))?; }
    }
    if let Some(w) = m.get("show_when") { check_condition(w, &p("show_when"))?; }
    if let Some(rules) = m.get("rules") {
        let rp = p("rules");
        let mut seen = HashSet::new();
        for (i, r) in arr(rules, &rp)?.iter().enumerate() {
            let p = format!("{rp}[{i}]");
            let rm = obj(r, &p)?;
            keys(rm, &p, &["id", "when", "style"])?;
            let rid = rm.get("id").ok_or_else(|| err(join(&p, "id"), "required"))?;
            let rid = string(rid, &join(&p, "id"))?;
            if rid.is_empty() || !seen.insert(rid.to_string()) {
                return Err(err(join(&p, "id"), format!("empty or duplicate rule id '{rid}' in module '{id}'")));
            }
            check_condition(rm.get("when").ok_or_else(|| err(join(&p, "when"), "required"))?, &join(&p, "when"))?;
            if let Some(s) = rm.get("style") { check_style(s, &join(&p, "style"))?; }
        }
    }
    for k in ["format", "repo_root_format"] {
        if let Some(f) = m.get(k) {
            let fs = string(f, &p(k))?;
            super::template::parse(fs).map_err(|e| err(p(k), e))?;
        }
    }
    if let Some(v) = m.get("icon_padding") {
        if v.as_u64().is_none_or(|n| n > 4) { return Err(err(p("icon_padding"), "expected an integer from 0 to 4")); }
    }
    if let Some(v) = m.get("ignore_branches") { check_string_list(v, &p("ignore_branches"))?; }
    for k in [
        "symbol", "truncation_symbol", "home_symbol", "branch_symbol", "error_symbol", "text", "read_only", "success_symbol",
        "not_executable_symbol", "not_found_symbol", "sigint_symbol", "signal_symbol", "tag_symbol", "allowed_msg",
        "not_allowed_msg", "denied_msg", "loaded_msg", "unloaded_msg", "pure_msg", "impure_msg", "unknown_msg",
        "unknown_indicator",
    ] {
        if let Some(s) = m.get(k) { check_sym(s, &p(k))?; }
    }
    for k in [
        "show_always", "truncate_to_repo", "show_counts", "show_state", "padding", "only_attached", "use_logical_path",
        "always_show_remote", "ignore_submodules", "show_milliseconds", "use_12hr", "map_symbol", "recognize_signal_code",
        "display_private", "only_detached", "tag_disabled", "only_nonzero_diffs", "repeat", "ssh_only", "heuristic",
    ] {
        if let Some(b) = m.get(k) { boolean(b, &p(k))?; }
    }
    for k in ["symbol_threshold", "fish_style_pwd_dir_length", "commit_hash_length", "tag_max_candidates", "repeat_offset"] {
        if let Some(t) = m.get(k) {
            if t.as_u64().is_none_or(|n| n > 1000) {
                return Err(err(p(k), "expected an integer from 0 to 1000"));
            }
        }
    }
    if let Some(t) = m.get("min_time") {
        if t.as_u64().is_none() { return Err(err(p("min_time"), "expected a non-negative integer (milliseconds)")); }
    }
    if let Some(t) = m.get("threshold") {
        if t.as_i64().is_none_or(|n| !(-1000..=1000).contains(&n)) {
            return Err(err(p("threshold"), "expected an integer from -1000 to 1000"));
        }
    }
    if let Some(t) = m.get("trim_at") { string(t, &p("trim_at"))?; }
    if let Some(a) = m.get("aliases") {
        let ap = p("aliases");
        for (k, v) in obj(a, &ap)? {
            if k == "$comment" { string(v, &join(&ap, k))?; continue; }
            if k.is_empty() { return Err(err(ap, "alias keys must not be empty")); }
            string(v, &join(&ap, k))?;
        }
    }
    if let Some(v) = m.get("detect_env_vars") {
        let vp = p("detect_env_vars");
        check_string_list(v, &vp)?;
        for (i, e) in arr(v, &vp)?.iter().enumerate() {
            let name = e.as_str().unwrap_or("").trim_start_matches('!');
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(err(format!("{vp}[{i}]"), "expected an environment variable name, optionally prefixed with '!'"));
            }
        }
    }
    if let Some(v) = m.get("utc_time_offset") {
        let s = string(v, &p("utc_time_offset"))?;
        if parse_utc_offset(s).is_none() {
            return Err(err(p("utc_time_offset"), "expected \"local\" or hours east of UTC between -24 and 24 (e.g. \"-5\", \"5.5\")"));
        }
    }
    if let Some(v) = m.get("time_range") {
        let s = string(v, &p("time_range"))?;
        if parse_time_range(s).is_none() {
            return Err(err(p("time_range"), "expected START-END with HH:MM:SS times (either side may be empty, \"-\" = always)"));
        }
    }
    if let Some(s) = m.get("indicators") {
        let sp = p("indicators");
        for (k, v) in obj(s, &sp)? {
            if k == "$comment" { string(v, &join(&sp, k))?; continue; }
            if !super::SHELL_NAMES.contains(&k.as_str()) {
                return Err(err(join(&sp, k), format!("unknown shell (expected one of: {})", super::SHELL_NAMES.join(", "))));
            }
            check_sym(v, &join(&sp, k))?;
        }
    }
    if ty == "text" && !m.contains_key("text") {
        return Err(err(p("text"), "required for text modules"));
    }
    if let Some(s) = m.get("os_symbols") {
        let sp = p("os_symbols");
        for (k, v) in obj(s, &sp)? {
            if k == "$comment" { string(v, &join(&sp, k))?; continue; }
            if k.is_empty() { return Err(err(sp, "os_symbols keys must not be empty")); }
            check_sym(v, &join(&sp, k))?;
        }
    }
    if ty == "env_var" {
        let v = m.get("variable").ok_or_else(|| err(p("variable"), "required for env_var modules"))?;
        let v = string(v, &p("variable"))?;
        if v.is_empty() || !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(err(p("variable"), "expected an environment variable name ([A-Za-z0-9_])"));
        }
    }
    if let Some(t) = m.get("number_threshold") {
        if t.as_u64().is_none_or(|n| n > 1000) {
            return Err(err(p("number_threshold"), "expected a non-negative integer"));
        }
    }
    if let Some(t) = m.get("time_format") { check_time_format(string(t, &p("time_format"))?, &p("time_format"))?; }
    if let Some(t) = m.get("truncation_length") {
        if t.as_u64().is_none_or(|n| n > 1000) {
            return Err(err(p("truncation_length"), "expected a non-negative integer (0 = no truncation)"));
        }
    }
    if let Some(s) = m.get("substitutions") {
        let sp = p("substitutions");
        for (k, v) in obj(s, &sp)? {
            if k == "$comment" { string(v, &join(&sp, k))?; continue; }
            if k.is_empty() { return Err(err(sp, "substitution keys must not be empty")); }
            check_sym(v, &join(&sp, k))?;
        }
    }
    if let Some(s) = m.get("status_symbols") {
        let sp = p("status_symbols");
        let sm = obj(s, &sp)?;
        keys(sm, &sp, STATUS_SYMBOL_KEYS)?;
        for k in STATUS_SYMBOL_KEYS {
            if let Some(v) = sm.get(*k) { check_sym(v, &join(&sp, k))?; }
        }
    }
    if let Some(s) = m.get("state_labels") {
        let sp = p("state_labels");
        let ops: Vec<&str> = GIT_OPERATIONS.iter().map(|(k, _)| *k).collect();
        let sm = obj(s, &sp)?;
        keys(sm, &sp, &ops)?;
        for k in &ops {
            if let Some(v) = sm.get(*k) { string(v, &join(&sp, k))?; }
        }
    }
    if ty == "language" {
        let l = m.get("language").ok_or_else(|| err(p("language"), "required for language modules"))?;
        one_of(l, &p("language"), LANGUAGES)?;
    }
    if let Some(f) = m.get("version_format") { string(f, &p("version_format"))?; }
    if let Some(v) = m.get("starship_config") {
        let path = string(v, &p("starship_config"))?;
        if !std::path::Path::new(path).is_absolute() || path.contains('\0') {
            return Err(err(p("starship_config"), "expected an absolute config path without NUL"));
        }
    }
    if ty == "starship" {
        let n = m.get("module").ok_or_else(|| err(p("module"), "required for starship modules"))?;
        let n = string(n, &p("module"))?;
        if n.is_empty() || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
            return Err(err(p("module"), "expected a Starship module name ([A-Za-z0-9_.])"));
        }
    }
    for k in ["detect_files", "detect_extensions", "detect_folders"] {
        if let Some(v) = m.get(k) { check_string_list(v, &p(k))?; }
    }
    if ty == "command" {
        let a = m.get("argv").ok_or_else(|| err(p("argv"), "required for command modules"))?;
        if arr(a, &p("argv"))?.is_empty() { return Err(err(p("argv"), "must not be empty")); }
        check_string_list(a, &p("argv"))?;
    }
    if let Some(r) = m.get("regex") {
        let rs = string(r, &p("regex"))?;
        regex::Regex::new(rs).map_err(|e| err(p("regex"), format!("invalid regex: {e}")))?;
    }
    for k in ["timeout_ms", "cache_ttl_s"] {
        if let Some(t) = m.get(k) {
            if t.as_u64().is_none_or(|n| n < 1) { return Err(err(p(k), "expected a positive integer")); }
        }
    }
    if let Some(s) = m.get("state_symbols") {
        let sp = p("state_symbols");
        let sm = obj(s, &sp)?;
        keys(sm, &sp, AGENT_STATES)?;
        for k in AGENT_STATES {
            if let Some(v) = sm.get(*k) { check_sym(v, &join(&sp, k))?; }
        }
    }
    if let Some(w) = m.get("max_width") {
        if w.as_u64().is_none_or(|n| !(1..=1000).contains(&n)) {
            return Err(err(p("max_width"), "expected an integer from 1 to 1000 (columns)"));
        }
    }
    Ok(())
}

/// Top-level `agent` section: `wrap` (boolean), `programs` (command names).
fn check_agent(v: &Value) -> Result<(), ConfigError> {
    let m = obj(v, "agent")?;
    keys(m, "agent", &["wrap", "programs"])?;
    if let Some(w) = m.get("wrap") { boolean(w, "agent.wrap")?; }
    if let Some(list) = m.get("programs") {
        for (i, e) in arr(list, "agent.programs")?.iter().enumerate() {
            let ep = format!("agent.programs[{i}]");
            let name = string(e, &ep)?;
            let ok = !name.is_empty()
                && name.len() <= 64
                && !name.starts_with('-')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'));
            if !ok {
                return Err(err(ep, "expected a command name (1-64 characters of [A-Za-z0-9_.+-], not starting with '-')"));
            }
        }
    }
    Ok(())
}

pub fn validate(v: &Value) -> Result<(), ConfigError> {
    let root = obj(v, "")?;
    keys(root, "", &["schema_version", "preset", "symbols", "modules", "layout", "history", "agent", "extensions", "palette", "palettes", "add_newline", "command_timeout_ms"])?;
    if let Some(b) = root.get("add_newline") { boolean(b, "add_newline")?; }
    if let Some(t) = root.get("command_timeout_ms") {
        if t.as_u64().is_none_or(|n| !(1..=60_000).contains(&n)) {
            return Err(err("command_timeout_ms", "expected milliseconds from 1 to 60000"));
        }
    }
    let sv = root.get("schema_version").ok_or_else(|| err("schema_version", "required"))?;
    let n = sv.as_u64().filter(|n| *n >= 1).ok_or_else(|| err("schema_version", "expected a positive integer"))?;
    if n > SCHEMA_VERSION as u64 {
        return Err(ConfigError {
            kind: ErrorKind::Unsupported,
            path: "schema_version".into(),
            message: format!("schema_version {n} is newer than supported version {SCHEMA_VERSION}; upgrade tildy"),
        });
    }
    if let Some(p) = root.get("preset") { one_of(p, "preset", PRESETS)?; }
    if let Some(s) = root.get("symbols") { one_of(s, "symbols", SYMBOL_SETTINGS)?; }
    if let Some(e) = root.get("extensions") { obj(e, "extensions")?; }
    if let Some(a) = root.get("agent") { check_agent(a)?; }

    let modules = obj(root.get("modules").ok_or_else(|| err("modules", "required"))?, "modules")?;
    for (id, m) in modules {
        if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(err(join("modules", id), "module id must be 1-64 chars of [A-Za-z0-9_-]"));
        }
        check_module(id, m, &join("modules", id))?;
    }

    let layout = obj(root.get("layout").ok_or_else(|| err("layout", "required"))?, "layout")?;
    keys(layout, "layout", &["rows"])?;
    let rows = arr(layout.get("rows").ok_or_else(|| err("layout.rows", "required"))?, "layout.rows")?;
    if rows.is_empty() { return Err(err("layout.rows", "needs at least one row")); }
    for (i, r) in rows.iter().enumerate() {
        let p = format!("layout.rows[{i}]");
        let rm = obj(r, &p)?;
        keys(rm, &p, &["left", "right", "style", "separator", "start", "end", "subseparator"])?;
        if let Some(s) = rm.get("style") { one_of(s, &join(&p, "style"), ROW_STYLES)?; }
        for k in ["separator", "start", "end", "subseparator"] {
            if let Some(s) = rm.get(k) { check_sym(s, &join(&p, k))?; }
        }
        for side in ["left", "right"] {
            let Some(list) = rm.get(side) else { continue };
            let sp = join(&p, side);
            for (j, e) in arr(list, &sp)?.iter().enumerate() {
                let ep = format!("{sp}[{j}]");
                let id = string(e, &ep)?;
                if !modules.contains_key(id) {
                    return Err(err(ep, format!("unknown module id '{id}'")));
                }
            }
        }
    }
    if let Some(h) = root.get("history") {
        let hm = obj(h, "history")?;
        keys(hm, "history", &["mode", "add_newline", "context_separator", "summary"])?;
        if let Some(mode) = hm.get("mode") { one_of(mode, "history.mode", HISTORY_MODES)?; }
        for k in ["add_newline", "context_separator", "summary"] { if let Some(v) = hm.get(k) { boolean(v, &format!("history.{k}"))?; } }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// schema/config.v1.schema.json describes the same module fields, types
    /// and type restrictions as this validator.
    #[test]
    fn schema_matches_the_validator() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/config.v1.schema.json");
        let schema: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let module = &schema["$defs"]["module"];
        let types: Vec<&str> = module["properties"]["type"]["enum"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert_eq!(types, MODULE_TYPES);
        let props: HashSet<&str> = module["properties"].as_object().unwrap().keys().map(String::as_str).filter(|k| *k != "$comment").collect();
        let mut fields: HashSet<&str> = COMMON.iter().copied().collect();
        fields.extend(SPECIFIC.iter().map(|(k, _)| *k));
        assert_eq!(props, fields, "schema module properties vs validator fields");
        for (k, tys) in SPECIFIC {
            let rule = module["allOf"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["if"]["required"] == serde_json::json!(["type", k]))
                .unwrap_or_else(|| panic!("no schema rule for {k}"));
            let want: Vec<&str> = rule["then"]["properties"]["type"]["enum"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
            assert_eq!(&want, tys, "types of {k}");
        }
        let ss: HashSet<&str> =
            module["properties"]["status_symbols"]["properties"].as_object().unwrap().keys().map(String::as_str).filter(|k| *k != "$comment").collect();
        assert_eq!(ss, STATUS_SYMBOL_KEYS.iter().copied().collect::<HashSet<_>>());
        for k in ["add_newline", "command_timeout_ms"] {
            assert!(schema["properties"].get(k).is_some(), "{k}");
        }
    }

    #[test]
    fn time_helpers() {
        assert_eq!(parse_hms("09:30:00"), Some(9 * 3600 + 30 * 60));
        assert_eq!(parse_hms("24:00:00"), None);
        assert_eq!(parse_hms("9:30:00"), None);
        assert_eq!(parse_time_range("-"), Some((None, None)));
        assert_eq!(parse_time_range("10:00:00-"), Some((Some(36_000), None)));
        assert_eq!(parse_time_range("10:00:00"), None);
        assert_eq!(parse_time_range("a-b"), None);
        assert_eq!(parse_utc_offset("local"), Some(None));
        assert_eq!(parse_utc_offset("-5"), Some(Some(-18_000)));
        assert_eq!(parse_utc_offset("5.75"), Some(Some(20_700)));
        assert_eq!(parse_utc_offset("24"), None);
        assert_eq!(parse_utc_offset("x"), None);
    }
}
