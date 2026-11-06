use super::*;
use crate::config;
use serde_json::json;

fn dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Golden cases: fixtures/render/<name>.input.json -> <name>.expected.json.
/// Input keys: `symbols` (tier), `config` (null = builtin, a fixtures/ path, or
/// `preset:<name>`), optional `palette` (with `preset:<name>`: the preset with
/// that palette), optional `now` (`YYYY-MM-DDTHH:MM:SS`), `data`. The `os`
/// module renders as on macOS so the goldens do not depend on the host.
/// Run with TILDY_BLESS=1 to (re)write the expected files.
#[test]
fn golden() {
    let rd = dir().join("render");
    let mut names: Vec<_> = std::fs::read_dir(&rd)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".input.json").map(String::from))
        .collect();
    names.sort();
    assert!(names.len() >= 10);
    for n in names {
        let input: Value = serde_json::from_str(&std::fs::read_to_string(rd.join(format!("{n}.input.json"))).unwrap()).unwrap();
        let tier = match input["symbols"].as_str().unwrap() {
            "ascii" => SymbolTier::Ascii,
            "nerd" => SymbolTier::Nerd,
            _ => SymbolTier::Unicode,
        };
        let cfg = match input["config"].as_str() {
            // `palette`: the preset recolored, as `{"preset": P, "palette": X}` in a user file.
            Some(p) if p.starts_with("preset:") && input["palette"].is_string() => {
                let user = json!({"schema_version": 1, "preset": &p["preset:".len()..], "palette": input["palette"]});
                config::from_str(&user.to_string()).unwrap()
            }
            Some(p) if p.starts_with("preset:") => config::preset(&p["preset:".len()..]).unwrap(),
            Some(f) => config::load(Some(&dir().join(f))).unwrap(),
            None => config::builtin(),
        };
        let cfg = if input["config"].is_null() || input["symbols"] != "auto" { cfg.with_symbols(tier) } else { cfg };
        let now = Some(LocalTime::parse(input["now"].as_str().unwrap_or("2026-10-19T14:05:09")).expect("now: YYYY-MM-DDTHH:MM:SS"));
        let data: PromptData = serde_json::from_value(input["data"].clone()).unwrap();
        let rendered = render_with(&cfg, &data, now, "macos");
        // Column accounting must agree with the visible text of every line.
        for (line, cols) in rendered.prompt.left.iter().zip(&rendered.columns) {
            assert_eq!(visible_width(line), *cols, "{n}: column mismatch for {line:?}");
        }
        let out = serde_json::to_value(&rendered).unwrap();
        let p = rd.join(format!("{n}.expected.json"));
        if std::env::var("TILDY_BLESS").is_ok() {
            std::fs::write(&p, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
            continue;
        }
        let want: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|_| panic!("missing {n}.expected.json"))).unwrap();
        assert_eq!(out, want, "golden mismatch: {n}");
    }
}

fn base() -> PromptData {
    serde_json::from_value(json!({
        "request": {"gen":1,"session_id":"s","cwd":"/Users/u/code","cols":80,"lines":24,"last_exit":0,"last_duration_ms":0,"env":""},
        "home": "/Users/u", "git": {"freshness":"fresh","age_ms":0,"value":null,"error":null},
        "ssh": false, "hostname": "h", "username": "u", "modules": {}
    }))
    .unwrap()
}

fn repo(cwd: &str, root: &str, extra: Value) -> PromptData {
    let mut d = base();
    d.request.cwd = cwd.into();
    let mut g = json!({
        "in_repo": true, "git_dir": format!("{root}/.git"), "worktree_root": root, "worktree_id": "x",
        "branch": "main", "detached": false, "unborn": false, "head_short": "abc1234", "upstream": "origin/main",
        "ahead": 0, "behind": 0, "staged": 0, "unstaged": 0, "untracked": 0, "conflicted": 0,
        "incomplete": false, "state": "clean"
    });
    for (k, v) in extra.as_object().unwrap() {
        g[k] = v.clone();
    }
    d.git = Slot::fresh(serde_json::from_value(g).unwrap());
    d
}

fn module(text: &str, fields: &[(&str, &str)]) -> Slot<ModuleValue> {
    Slot::fresh(ModuleValue {
        text: text.into(),
        fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        prerendered: None,
    })
}

// Module/layout tests use an explicit plain base and no leading blank line.
// Zero-config behavior is covered separately below and by the golden cases.
fn explicit_test_config(src: &str) -> anyhow::Result<Config> {
    let mut value: Value = serde_json::from_str(src)?;
    let object = value.as_object_mut().unwrap();
    object.entry("schema_version").or_insert(json!(1));
    object.entry("preset").or_insert(json!("plain"));
    object.entry("add_newline").or_insert(json!(false));
    Ok(config::from_str(&value.to_string())?)
}

#[test]
fn zero_config_is_pastel_two_rows_with_a_leading_blank_line() {
    for c in [config::builtin(), config::from_str(r#"{"schema_version":1}"#).unwrap()] {
        assert_eq!(c.file.preset.as_deref(), Some("pastel-powerline"));
        assert_eq!(c.file.add_newline, Some(true));
        assert_eq!(c.file.layout.rows.len(), 2);
        let rendered = render(&c, &base());
        assert_eq!(rendered.prompt.left.len(), 3);
        assert_eq!(rendered.prompt.left[0], "");
    }
    let c = config::from_str(r#"{"schema_version":1,"preset":"plain","add_newline":false}"#).unwrap();
    assert_eq!(c.file.layout.rows.len(), 1);
    assert_eq!(render(&c, &base()).prompt.left.len(), 1);
}

fn uni() -> Config {
    explicit_test_config(r#"{"preset":"plain"}"#).unwrap().with_symbols(SymbolTier::Unicode)
}

fn pastel(t: SymbolTier) -> Config {
    explicit_test_config(r#"{"preset":"pastel-powerline"}"#).unwrap().with_symbols(t)
}

fn at() -> Option<LocalTime> {
    LocalTime::parse("2026-10-19T14:05:09")
}

fn reason(o: &RenderOutput, id: &str) -> Option<String> {
    o.reports.iter().find(|r| r.id == id).unwrap().hidden_reason.clone()
}

#[test]
fn escape_neutralizes_specials() {
    assert_eq!(escape_prompt_text("100%"), "100%%");
    assert_eq!(escape_prompt_text("a\u{1b}[31mb"), "a[31mb");
    assert_eq!(escape_prompt_text("a\nb\r\tc\u{7f}\u{85}"), "abc");
    assert_eq!(escape_prompt_text("日本語 ❯ é"), "日本語 ❯ é");
    assert_eq!(escape_prompt_text("\u{202e}x"), "x");
}

#[test]
fn builtin_scenarios_exact() {
    let c = uni();
    let o = render(&c, &base());
    assert_eq!(o.prompt.left, vec!["%B%F{blue}~/code%f%b ❯ "]);
    assert_eq!(o.prompt.right, None);
    assert_eq!(o.prompt.transient.as_deref(), Some("❯ "));
    assert_eq!(o.prompt.status, RenderStatus::Fresh);
    assert_eq!(o.columns, vec![9]);
    let mut d = base();
    d.request.last_exit = 2;
    d.request.last_duration_ms = 3200;
    d.ssh = true;
    let o = render(&c, &d);
    assert_eq!(o.prompt.right.as_deref(), Some("%F{yellow}took 3.2s%f %F{red}✗ 2%f"));
    assert!(o.prompt.left[0].contains("u@h"));
    let hidden: Vec<_> = o.reports.iter().filter(|r| !r.visible).map(|r| (r.id.as_str(), r.hidden_reason.clone().unwrap())).collect();
    assert_eq!(
        hidden,
        vec![
            ("git", "no_data".to_string()),
            ("changes", "no_changes".into()),
            ("docker", "no_data".into()),
            ("c", "no_data".into()),
            ("go", "no_data".into()),
            ("java", "no_data".into()),
            ("node", "no_data".into()),
            ("py", "no_data".into()),
            ("rust", "no_data".into()),
            ("jobs", "empty".into()),
        ]
    );
}

#[test]
fn hidden_reasons() {
    let o = render(&uni(), &base());
    assert_eq!(reason(&o, "ssh_host").as_deref(), Some("show_when:session.ssh"));
    assert_eq!(reason(&o, "elapsed").as_deref(), Some("show_when:command.duration_ms"));
    assert_eq!(reason(&o, "result").as_deref(), Some("show_when:command.exit"));
    assert_eq!(reason(&o, "git").as_deref(), Some("no_data"));
}

#[test]
fn module_slot_reasons() {
    let mut d = base();
    d.modules.insert("node".into(), Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: None, error: None });
    d.modules.insert("rust".into(), Slot::pending());
    d.modules.insert("go".into(), Slot { freshness: Freshness::Timeout, age_ms: None, value: None, error: Some("t".into()) });
    d.modules.insert("java".into(), Slot { freshness: Freshness::Error, age_ms: None, value: None, error: Some("e".into()) });
    let o = render(&uni(), &d);
    assert_eq!(reason(&o, "node").as_deref(), Some("not_detected"));
    assert_eq!(reason(&o, "rust").as_deref(), Some("pending"));
    assert_eq!(reason(&o, "go").as_deref(), Some("timeout"));
    assert_eq!(reason(&o, "java").as_deref(), Some("error"));
    assert_eq!(reason(&o, "c").as_deref(), Some("no_data"));
    // Languages stay hidden while pending, but the prompt is partial.
    assert_eq!(o.prompt.left, vec!["%B%F{blue}~/code%f%b ❯ "]);
    assert_eq!(o.prompt.status, RenderStatus::Partial);
}

#[test]
fn partial_when_git_pending() {
    let mut d = base();
    d.git = Slot::pending();
    let o = render(&uni(), &d);
    assert_eq!(o.prompt.status, RenderStatus::Partial);
    assert!(o.prompt.left[0].contains("%F{8}…%f"));
}

#[test]
fn duration_format() {
    assert_eq!(duration_text(3200).unwrap(), "took 3.2s");
    assert_eq!(duration_text(65_000).unwrap(), "took 1m 5s");
    assert_eq!(duration_text(3_725_000).unwrap(), "took 1h 2m");
    assert_eq!(duration_text(0), None);
}

#[test]
fn hostile_path_stays_literal_and_escaped() {
    let mut d = base();
    d.request.cwd = "/Users/u/$(touch x)/50%/a\u{1b}[31mb\nc".into();
    let o = render(&uni(), &d);
    let l = &o.prompt.left[0];
    assert!(l.contains("$(touch x)"));
    assert!(l.contains("50%%"));
    assert!(!l.contains('\u{1b}') && !l.contains('\n'));
}

fn legacy() -> DirOpts {
    DirOpts {
        length: 4,
        truncation_symbol: "…/".into(),
        home_symbol: "~".into(),
        substitutions: vec![],
        to_repo: false,
        fish: 0,
        split_any_root: false,
    }
}

#[test]
fn truncation_and_repo_root() {
    let o = legacy();
    let p = Path::new;
    let h = Some(p("/Users/u"));
    assert_eq!(directory_text(p("/Users/u"), h, None, &o), "~");
    assert_eq!(directory_text(p("/"), h, None, &o), "/");
    assert_eq!(directory_text(p("/Users/u/a/b/c"), h, None, &o), "~/a/b/c");
    // The home symbol counts as a component (Starship's rule).
    assert_eq!(directory_text(p("/Users/u/a/b/c/d"), h, None, &o), "…/a/b/c/d");
    assert_eq!(directory_text(p("/Users/u/a/b/c/d/e"), h, None, &o), "…/b/c/d/e");
    assert_eq!(directory_text(p("/usr/local/share/man/man1"), h, None, &o), "…/local/share/man/man1");
    let root = Some(p("/Users/u/a/b/proj"));
    assert_eq!(directory_text(p("/Users/u/a/b/proj/s/t/u/v/w"), h, root, &o), "…/proj/s/t/u/v/w");
    assert_eq!(directory_text(p("/Users/u/a/proj2"), h, Some(p("/Users/u/a/proj2")), &o), "~/a/proj2");
    assert_eq!(directory_text(p("/Users/u/x/y/z/w/v"), h, Some(p("/Users/u/x/y/z/w/v")), &o), "…/y/z/w/v");
}

#[test]
fn directory_starship_rules() {
    let p = Path::new;
    let h = Some(p("/Users/u"));
    let o = DirOpts { length: 3, to_repo: true, ..legacy() };
    // Repo contraction; a repo-relative path always carries the symbol.
    assert_eq!(directory_text(p("/Users/u/code/tildy"), h, Some(p("/Users/u/code/tildy")), &o), "…/tildy");
    assert_eq!(directory_text(p("/Users/u/code/tildy/src/a/b"), h, Some(p("/Users/u/code/tildy")), &o), "…/src/a/b");
    // A repo at $HOME is not contracted.
    assert_eq!(directory_text(p("/Users/u/x"), h, Some(p("/Users/u")), &o), "~/x");
    assert_eq!(directory_text(p("/Users/u/a/b"), h, None, &o), "~/a/b");
    assert_eq!(directory_text(p("/Users/u/a/b/c"), h, None, &o), "…/a/b/c");
    let none = DirOpts { truncation_symbol: String::new(), ..DirOpts { length: 3, to_repo: true, ..legacy() } };
    assert_eq!(directory_text(p("/Users/u/code/tildy"), h, Some(p("/Users/u/code/tildy")), &none), "tildy");
    let off = DirOpts { length: 0, ..legacy() };
    assert_eq!(directory_text(p("/a/b/c/d/e/f/g"), h, None, &off), "/a/b/c/d/e/f/g");
}

#[test]
fn substitutions() {
    let p = Path::new;
    let h = Some(p("/Users/u"));
    let o = DirOpts {
        length: 3,
        to_repo: true,
        substitutions: vec![
            ("Documents".into(), "\u{f718} ".into()),
            ("~/work/client".into(), "W".into()),
            ("~/work".into(), "w".into()),
        ],
        ..legacy()
    };
    assert_eq!(directory_text(p("/Users/u/Documents"), h, None, &o), "~/\u{f718} ");
    assert_eq!(directory_text(p("/Users/u/Documents/x/Documents"), h, None, &o), "…/\u{f718} /x/\u{f718} ");
    // Whole components only.
    assert_eq!(directory_text(p("/Users/u/MyDocuments"), h, None, &o), "~/MyDocuments");
    // Longest prefix wins.
    assert_eq!(directory_text(p("/Users/u/work/client/a"), h, None, &o), "W/a");
    assert_eq!(directory_text(p("/Users/u/work/other"), h, None, &o), "w/other");
    // Through the preset: nerd tier substitutes, unicode tier keeps the name.
    let mut d = base();
    d.request.cwd = "/Users/u/Documents".into();
    let n = render_at(&pastel(SymbolTier::Nerd), &d, at());
    assert_eq!(n.reports.iter().find(|r| r.id == "cwd").unwrap().text, "~/\u{f718} ");
    let u = render_at(&pastel(SymbolTier::Unicode), &d, at());
    assert_eq!(u.reports.iter().find(|r| r.id == "cwd").unwrap().text, "~/Documents");
}

#[test]
fn missing_data_is_not_success() {
    // git.state missing (pending) must not match `ne`/`eq` rules.
    let c = Condition { field: "git.state".into(), op: Op::Ne, value: json!("dirty") };
    assert!(!eval(&c, &build_fields(&base())));
    let c = Condition { field: "command.exit".into(), op: Op::Ne, value: json!("zero") };
    assert!(!eval(&c, &build_fields(&base()))); // type mismatch
}

#[test]
fn multi_row_and_full_history() {
    let cfg = explicit_test_config(
        r#"{"schema_version":1,"history":{"mode":"full"},"layout":{"rows":[
        {"left":["cwd"],"right":["elapsed"]},{"left":[],"right":["result"]}]}}"#,
    )
    .unwrap()
    .with_symbols(SymbolTier::Ascii);
    let mut d = base();
    d.request.last_exit = 1;
    d.request.last_duration_ms = 9000;
    let o = render(&cfg, &d);
    assert_eq!(o.prompt.left.len(), 2);
    assert_eq!(o.prompt.left[1], "> ");
    assert_eq!(o.prompt.right.as_deref(), Some("%F{red}x 1%f"));
    assert_eq!(o.prompt.transient, None);
    // The first row's right side is right-aligned on its own line, one column
    // short of the edge (where RPROMPT ends).
    assert!(o.prompt.left[0].starts_with("%B%F{blue}~/code%f%b ") && o.prompt.left[0].ends_with(" %F{yellow}took 9.0s%f"), "{:?}", o.prompt.left[0]);
    assert_eq!((visible_width(&o.prompt.left[0]), o.columns[0]), (79, 79));
    assert!(o.reports.iter().find(|r| r.id == "elapsed").unwrap().visible);
    // Without room for a one-column gap it is dropped.
    d.request.cols = 15;
    let o = render(&cfg, &d);
    assert_eq!(o.prompt.left[0], "%B%F{blue}~/code%f%b");
    assert_eq!(reason(&o, "elapsed").as_deref(), Some("no_room"));
}

#[test]
fn upper_right_powerline_is_mirrored() {
    let cfg = explicit_test_config(
        r##"{"schema_version":1,"modules":{
            "a":{"type":"text","text":"A","padding":true,"style":{"background":"#111111","foreground":"#eeeeee"}},
            "b":{"type":"text","text":"B","padding":true,"style":{"background":"#222222"}},
            "c":{"type":"text","text":"C","padding":true,"style":{"background":"#222222"}},
            "d":{"type":"text","text":"D"}},
          "layout":{"rows":[
            {"style":"powerline","separator":"","subseparator":"[](fg:#999999)","start":"░","left":["a","b","c"],"right":["a","b","c","d"]},
            {"left":["d"]}]}}"##,
    )
    .unwrap()
    .with_symbols(SymbolTier::Nerd);
    let mut d = base();
    d.request.cols = 40;
    let o = render(&cfg, &d);
    let line = &o.prompt.left[0];
    assert_eq!((visible_width(line), o.columns[0]), (39, 39));
    // Left: start cap, A, separator, B, subseparator in its own color, C, end cap.
    let left = "%k%F{#111111}\u{2591}%K{#111111}%F{#eeeeee} A %F{#111111}%K{#222222}\u{e0b0}%f B %f%F{#999999}\u{e0b1}%f%f C %k%F{#222222}\u{e0b0}%f";
    assert!(line.starts_with(left), "{line:?}");
    // Right: mirrored head, separator and subseparator; the start cap becomes the
    // mirrored tail; a segment without background follows as plain text.
    let right = "%F{#111111}\u{e0b2}%K{#111111}%F{#eeeeee} A %F{#222222}\u{e0b2}%K{#222222}%f B %f%F{#999999}\u{e0b3}%f%f C %k%F{#222222}\u{2591}%fD%k%f";
    assert!(line.ends_with(right), "{line:?}");
    assert_eq!(mirror("\u{2591}\u{2592}\u{2593}"), "\u{2593}\u{2592}\u{2591}");
    assert_eq!(mirror("[\u{e0b1}](fg:red)"), "[\u{e0b3}](fg:red)");
    assert_eq!(mirror(">"), "<");
}

#[test]
fn empty_upper_row_is_dropped() {
    let cfg = explicit_test_config(r#"{"schema_version":1,"layout":{"rows":[{"left":["git"]},{"left":["cwd"]}]}}"#).unwrap();
    let o = render(&cfg.with_symbols(SymbolTier::Unicode), &base());
    assert_eq!(o.prompt.left, vec!["%B%F{blue}~/code%f%b ❯ "]);
}

#[test]
fn deterministic_and_wide_columns() {
    let mut d = base();
    d.request.cwd = "/Users/u/日本語".into();
    let a = render(&uni(), &d);
    let b = render(&uni(), &d);
    assert_eq!(a.reports, b.reports);
    assert_eq!(a.reports[0].columns, "~/日本語".chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum::<usize>());
}

fn node_repo() -> PromptData {
    let mut d = repo("/Users/u/code/tildy", "/Users/u/code/tildy", json!({"unstaged": 2, "untracked": 1, "state": "dirty"}));
    d.modules.insert("node".into(), module("22.3.0", &[("version", "22.3.0")]));
    d
}

#[test]
fn pastel_nerd_row_exact() {
    let o = render_at(&pastel(SymbolTier::Nerd), &node_repo(), at());
    assert_eq!(
        o.prompt.left,
        vec![
            "%K{#9a348e}%f \u{f120} %F{#9a348e}%K{#da627d}\u{e0b0}%f …/tildy %F{#da627d}%K{#fca17d}\u{e0b0}%f \u{f418} main !? \
             %F{#fca17d}%K{#86bbd8}\u{e0b0}%f \u{e718} v22.3.0 %F{#86bbd8}%K{#33658a}\u{e0b0}%f ♥ 14:05 %k%F{#33658a}\u{e0b0}%f"
                .to_string(),
            "%B%F{green}❯%f%b ".to_string(),
        ]
    );
    // user 3 + sep 1 + cwd 9 + sep 1 + git 11 + sep 1 + node 11 + sep 1 + time 9 + trailing sep 1
    assert_eq!(o.columns, vec![48, 2]);
    assert_eq!(o.prompt.left.iter().map(|l| visible_width(l)).collect::<Vec<_>>(), o.columns);
    assert_eq!(o.prompt.transient.as_deref(), Some("%B%F{green}❯%f%b "));
    assert_eq!(o.prompt.right, None);
    // Undetected bridges and languages leave no segment.
    assert_eq!(reason(&o, "elixir").as_deref(), Some("no_data"));
    let mut d = node_repo();
    d.modules.insert("elixir".into(), Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: None, error: None });
    assert_eq!(reason(&render_at(&pastel(SymbolTier::Nerd), &d, at()), "elixir").as_deref(), Some("not_detected"));
}

#[test]
fn pastel_error_and_fallback_tiers() {
    let mut d = node_repo();
    d.request.last_exit = 1;
    let o = render_at(&pastel(SymbolTier::Nerd), &d, at());
    assert_eq!(o.prompt.left[1], "%B%F{red}✘ 1%f%b %B%F{red}❯%f%b ");
    assert_eq!(o.prompt.transient.as_deref(), Some("%B%F{red}❯%f%b "));
    let u = render_at(&pastel(SymbolTier::Unicode), &node_repo(), at());
    // No private-use glyph outside the nerd tier.
    for l in &u.prompt.left {
        assert!(!l.chars().any(|c| ('\u{e000}'..='\u{f8ff}').contains(&c)), "{l:?}");
    }
    assert!(u.prompt.left[0].contains(" u %F{#9a348e}%K{#da627d}>%f"), "{:?}", u.prompt.left[0]);
    assert!(u.prompt.left[0].contains(" ⎇ main !? "));
    assert!(u.prompt.left[0].contains(" ⬢ v22.3.0 "));
    let a = render_at(&pastel(SymbolTier::Ascii), &node_repo(), at());
    for l in a.prompt.left.iter().chain(a.prompt.transient.iter()) {
        assert!(strip_prompt_escapes(l).is_ascii(), "{l:?}");
    }
    assert!(a.prompt.left[0].contains(" 14:05 "));
    assert_eq!(a.prompt.left[1], "%B%F{green}>%f%b ");
}

#[test]
fn powerline_separator_colors() {
    let cfg = explicit_test_config(
        r##"{"schema_version":1,"modules":{
            "a":{"type":"username","show_always":true,"format":"A","style":{"background":"red","foreground":"#FFFFFF"}},
            "b":{"type":"jobs","style":{"background":"20"}},
            "c":{"type":"character","format":"C","style":{"background":"#00FF00","bold":true}}},
          "layout":{"rows":[{"style":"powerline","separator":">","left":["a","b","c"]},{"left":[]}]}}"##,
    )
    .unwrap()
    .with_symbols(SymbolTier::Unicode);
    let o = render(&cfg, &base());
    // b is hidden (no jobs): a joins c directly; separator fg = previous bg, bg = next bg.
    assert_eq!(o.prompt.left[0], "%K{red}%F{#ffffff} A %F{red}%K{#00ff00}>%f%B C %b%k%F{#00ff00}>%f");
    assert_eq!(o.columns[0], 3 + 1 + 3 + 1);
    let mut d = base();
    d.request.jobs = 2;
    let o = render(&cfg, &d);
    assert_eq!(
        o.prompt.left[0],
        "%K{red}%F{#ffffff} A %F{red}%K{20}>%f ✦2 %F{20}%K{#00ff00}>%f%B C %b%k%F{#00ff00}>%f"
    );
    // A character module in an upper row does not replace the prompt glyph.
    assert_eq!(o.prompt.left[1], "❯ ");
    // Same background: no separator between the two segments.
    let cfg = explicit_test_config(
        r##"{"schema_version":1,"modules":{
            "a":{"type":"username","show_always":true,"format":"A","style":{"background":"#86BBD8"}},
            "b":{"type":"jobs","style":{"background":"#86bbd8"}}},
          "layout":{"rows":[{"style":"powerline","left":["a","b"]}]}}"##,
    )
    .unwrap()
    .with_symbols(SymbolTier::Nerd);
    let o = render(&cfg, &d);
    assert_eq!(o.prompt.left[0], "%K{#86bbd8}%f A %f ✦2 %k%F{#86bbd8}\u{e0b0}%f ❯ ");
    assert_eq!(o.columns[0], 3 + 4 + 1 + 3);
}

#[test]
fn hex_colors_and_attributes() {
    let cfg = explicit_test_config(
        r##"{"schema_version":1,"modules":{"cwd":{"type":"directory","style":{"foreground":"#A1B2C3","background":"#0A0B0C","italic":true,"dim":true,"underline":true}}},
           "layout":{"rows":[{"left":["cwd"]}]}}"##,
    )
    .unwrap()
    .with_symbols(SymbolTier::Unicode);
    // Previews draw dim/italic as raw SGR...
    let o = render_preview(&cfg, &base(), None);
    assert_eq!(
        o.prompt.left[0],
        "%U%{\u{1b}[2m%}%{\u{1b}[3m%}%F{#a1b2c3}%K{#0a0b0c}~/code%k%f%{\u{1b}[23m%}%{\u{1b}[22m%}%u ❯ "
    );
    assert_eq!(o.columns[0], visible_width(&o.prompt.left[0]));
    assert_eq!(o.columns[0], 9);
    // ...but a real prompt cannot carry control characters (the protocol and
    // the shell strip them), so dim/italic are left out there.
    let o = render(&cfg, &base());
    assert_eq!(o.prompt.left[0], "%U%F{#a1b2c3}%K{#0a0b0c}~/code%k%f%u ❯ ");
    assert!(!o.prompt.left[0].contains('\u{1b}'));
}

#[test]
fn format_groups_are_omitted() {
    let cfg = explicit_test_config(
        r#"{"schema_version":1,"modules":{
            "py":{"type":"python","symbol":"py"},
            "x":{"type":"command","argv":["x"],"format":"[$text( @$host)( $nope)]"}},
           "layout":{"rows":[{"left":["py","x"]}]}}"#,
    )
    .unwrap()
    .with_symbols(SymbolTier::Unicode);
    let mut d = base();
    d.modules.insert("py".into(), module("3.12.1", &[("version", "3.12.1")]));
    d.modules.insert("x".into(), module("hi", &[]));
    let o = render(&cfg, &d);
    assert_eq!(o.prompt.left[0], "py v3.12.1 [hi] ❯ ");
    d.modules.insert("py".into(), module("3.12.1", &[("version", "3.12.1"), ("env", "venv")]));
    d.modules.insert("x".into(), module("hi", &[("host", "h1")]));
    let o = render(&cfg, &d);
    assert_eq!(o.prompt.left[0], "py v3.12.1 (venv) [hi @h1] ❯ ");
}

#[test]
fn git_starship_status_order_and_counts() {
    let ss = StatusSymbols::default();
    let g = |v: Value| -> GitData {
        let mut base = json!({"in_repo": true, "branch": "m", "detached": false, "unborn": false, "ahead": 0, "behind": 0,
            "staged": 0, "unstaged": 0, "untracked": 0, "conflicted": 0, "incomplete": false, "state": "dirty"});
        for (k, x) in v.as_object().unwrap() {
            base[k] = x.clone();
        }
        serde_json::from_value(base).unwrap()
    };
    let t = SymbolTier::Unicode;
    // Everything: Starship order = $ ✘ » ! + ? then ⇕.
    let all = g(json!({"conflicted":1,"stashed":1,"deleted":1,"renamed":1,"unstaged":2,"staged":2,"untracked":1,"ahead":1,"behind":2}));
    assert_eq!(starship_status(&all, &ss, false, t), "=$✘»!+?⇕");
    assert_eq!(starship_status(&all, &ss, true, t), "=1$1✘1»1!1+1?1⇕⇡1⇣2");
    assert_eq!(starship_status(&g(json!({"unstaged":1,"untracked":3})), &ss, false, t), "!?");
    assert_eq!(starship_status(&g(json!({"ahead":3})), &ss, true, t), "⇡3");
    assert_eq!(starship_status(&g(json!({"behind":1})), &ss, false, SymbolTier::Ascii), "v");
    // A worktree deletion is not also "modified"; a pure rename is not "staged".
    assert_eq!(starship_status(&g(json!({"unstaged":1,"deleted":1})), &ss, false, t), "✘");
    assert_eq!(starship_status(&g(json!({"staged":1,"renamed":1})), &ss, false, t), "»");
    assert_eq!(starship_status(&g(json!({"staged":1,"deleted":1})), &ss, false, t), "✘");
}

#[test]
fn git_state_label() {
    let mut d = repo("/Users/u/code/tildy", "/Users/u/code/tildy", json!({"operation": "rebase", "operation_progress": [2, 5], "unstaged": 1, "state": "dirty"}));
    let o = render(&uni(), &d);
    assert_eq!(o.prompt.left[0], "%B%F{blue}~/code/tildy%f%b %F{yellow}main ±1 %B%F{yellow}(REBASING 2/5)%f%b%f ❯ ");
    assert_eq!(o.reports[1].text, "main ±1 (REBASING 2/5)");
    assert_eq!(o.columns[0], visible_width(&o.prompt.left[0]));
    d.git.value.as_mut().unwrap().operation = Some("cherry-pick".into());
    d.git.value.as_mut().unwrap().operation_progress = None;
    let o = render_at(&pastel(SymbolTier::Nerd), &d, at());
    assert!(o.prompt.left[0].contains(" \u{f418} main ! %B%F{yellow}(CHERRY-PICKING)%f%b "), "{:?}", o.prompt.left[0]);
    let cfg = explicit_test_config(r#"{"schema_version":1,"modules":{"git":{"show_state":false}}}"#).unwrap().with_symbols(SymbolTier::Unicode);
    assert!(!render(&cfg, &d).prompt.left[0].contains("CHERRY"));
    let cfg = explicit_test_config(r#"{"schema_version":1,"modules":{"git":{"state_labels":{"cherry-pick":"PICK"},"state_style":{"foreground":"red"}}}}"#)
        .unwrap()
        .with_symbols(SymbolTier::Unicode);
    assert!(render(&cfg, &d).prompt.left[0].contains("%F{red}(PICK)%f"));
}

#[test]
fn time_username_jobs_python_docker_starship() {
    let cfg = explicit_test_config(
        r#"{"schema_version":1,"modules":{
            "t":{"type":"time","time_format":"%H:%M:%S %p"},
            "u":{"type":"username"},
            "py":{"type":"python"},
            "dk":{"type":"docker_context"},
            "ex":{"type":"starship","module":"elixir","style":{"background":"blue"}}},
           "layout":{"rows":[{"left":["t","u","jobs","py","dk"]},{"style":"powerline","left":["ex","t"]}]}}"#,
    )
    .unwrap()
    .with_symbols(SymbolTier::Nerd);
    let mut d = base();
    d.request.jobs = 1;
    d.modules.insert("py".into(), module("Python 3.12.1", &[("version", "v3.12.1"), ("env", "conda-base")]));
    d.modules.insert("dk".into(), module("colima", &[("context", "colima")]));
    d.modules.insert(
        "ex".into(),
        Slot::fresh(ModuleValue { text: String::new(), fields: BTreeMap::new(), prerendered: Some(("%{\u{1b}[44m%} \u{e275} 1.15 %{\u{1b}[0m%}".into(), 8)) }),
    );
    let o = render_at(&cfg, &d, at());
    assert_eq!(
        o.prompt.left[0],
        "14:05:09 PM %F{blue}✦1%f \u{e235} v3.12.1 (conda-base) \u{f308} colima"
    );
    assert_eq!(reason(&o, "u").as_deref(), Some("not_ssh_or_root"));
    // Bridge output is inserted as-is (no padding), the time segment is padded.
    assert_eq!(
        o.prompt.left[1],
        "%K{blue}%f%{\u{1b}[44m%} \u{e275} 1.15 %{\u{1b}[0m%}%F{blue}%K{%k}\u{e0b0}%f 14:05:09 PM %k%f ❯ "
            .replace("%K{%k}", "%k")
    );
    assert_eq!(o.columns[1], 8 + 1 + 13 + 1 + 1 + 1);
    d.ssh = true;
    let o = render_at(&cfg, &d, at());
    assert!(o.prompt.left[0].contains(" u "));
}

#[test]
fn template_and_version_helpers() {
    let m: ModuleConfig = serde_json::from_value(json!({"type": "language", "language": "rust", "version_format": "${major}.${minor}"})).unwrap();
    assert_eq!(version_of(&m, &ModuleValue { text: "1.80.1".into(), ..Default::default() }), "1.80");
    let m: ModuleConfig = serde_json::from_value(json!({"type": "language", "language": "rust"})).unwrap();
    assert_eq!(version_of(&m, &ModuleValue { text: "v22.3.0".into(), ..Default::default() }), "v22.3.0");
    assert_eq!(version_of(&m, &ModuleValue { text: "".into(), ..Default::default() }), "");
    assert_eq!(strip_prompt_escapes("%B%F{#fff}a%%b%f%{\u{1b}[3m%}c%K{1}d%k%b"), "a%bcd");
}

// ---- M1b presets: spans, text/os/fill/env_var modules, caps, padding

fn cfg(src: &str, t: SymbolTier) -> Config {
    explicit_test_config(src).unwrap_or_else(|e| panic!("{e}")).with_symbols(t)
}

#[test]
fn spans_style_parts_of_a_module() {
    let c = cfg(
        r#"{"schema_version":1,"modules":{"git":{"type":"git","branch_symbol":"B ","format":"on [$branch_symbol$branch](bold purple)( [\\[$status\\]](red))"}},
           "layout":{"rows":[{"left":["git"]}]}}"#,
        SymbolTier::Unicode,
    );
    let d = repo("/Users/u/code/tildy", "/Users/u/code/tildy", json!({"unstaged": 1, "state": "dirty"}));
    let o = render(&c, &d);
    // Text outside spans keeps the module style (none here); spans switch and switch back.
    assert_eq!(o.prompt.left[0], "on %B%F{magenta}B main%b%f%k [!] ❯ ".replace("[!]", "%F{red}[±1]%f"));
    assert_eq!(o.columns[0], visible_width(&o.prompt.left[0]));
    assert_eq!(o.reports[0].text, "on B main [±1]");
    // Inside a powerline segment a span keeps the segment background.
    let c = cfg(
        r##"{"schema_version":1,"modules":{"u":{"type":"username","show_always":true,"format":"[$user](italic fg:#ffffff) x","style":{"background":"blue","bold":true}}},
           "layout":{"rows":[{"style":"powerline","separator":">","left":["u"]}]}}"##,
        SymbolTier::Unicode,
    );
    let o = render_preview(&c, &base(), None);
    assert_eq!(o.prompt.left[0], "%K{blue}%f%B %{\u{1b}[3m%}%F{#ffffff}u%{\u{1b}[22;23m%}%b%B%f%K{blue} x %b%k%F{blue}>%f ❯ ");
    assert_eq!(o.columns[0], visible_width(&o.prompt.left[0]));
    let o = render(&c, &base());
    assert_eq!(o.prompt.left[0], "%K{blue}%f%B %F{#ffffff}u%f x %b%k%F{blue}>%f ❯ ");
}

#[test]
fn styled_symbols_and_status_counts() {
    let c = cfg(
        r#"{"schema_version":1,"modules":{
            "node":{"type":"language","language":"nodejs","symbol":"[⬢](bold green) ","format":"via [$symbol($version )](bold green)"},
            "git":{"type":"git","format":"$branch( $all_status)( $ahead_behind)","status_symbols":{"modified":"[!$count](yellow)","ahead":"⇡${count}","untracked":""}}},
           "layout":{"rows":[{"left":["git","node"]}]}}"#,
        SymbolTier::Unicode,
    );
    let mut d = repo("/Users/u/code/tildy", "/Users/u/code/tildy", json!({"unstaged": 2, "untracked": 1, "ahead": 3, "state": "dirty"}));
    d.modules.insert("node".into(), module("22.3.0", &[("version", "22.3.0")]));
    let o = render(&c, &d);
    assert_eq!(o.reports[0].text, "main !2 ⇡3");
    assert!(o.prompt.left[0].contains("%F{yellow}!2%f"), "{:?}", o.prompt.left[0]);
    assert_eq!(o.reports[1].text, "via ⬢ v22.3.0 ");
    assert!(o.prompt.left[0].contains("via %B%F{green}⬢ v22.3.0 %b%f%k"), "{:?}", o.prompt.left[0]);
}

#[test]
fn text_os_env_var_and_jobs_threshold() {
    let c = cfg(
        r#"{"schema_version":1,"modules":{
            "t":{"type":"text","text":{"nerd":"","ascii":"[mac]"},"style":{"foreground":"red"}},
            "sp":{"type":"text","text":" "},
            "os":{"type":"os","os_symbols":{"linux":"L"}},
            "os2":{"type":"os"},
            "conda":{"type":"env_var","variable":"CONDA_DEFAULT_ENV","format":"($symbol)$env_value","symbol":"C:",
                     "show_when":{"field":"env.CONDA_DEFAULT_ENV","op":"ne","value":"base"}},
            "jobs":{"type":"jobs","number_threshold":2}},
           "layout":{"rows":[{"left":["t","sp","os","os2","conda","jobs"]}]}}"#,
        SymbolTier::Ascii,
    );
    let mut d = base();
    d.request.env = "CONDA_DEFAULT_ENV=ml\u{1f}USER=u".into();
    d.request.jobs = 1;
    let o = render_with(&c, &d, None, "linux");
    // The spacer text module carries its own space: no join space around it.
    assert_eq!(o.prompt.left[0], "%F{red}[mac]%f L linux C:ml & > ");
    assert_eq!(o.columns[0], visible_width(&o.prompt.left[0]));
    let o = render_with(&c.clone().with_symbols(SymbolTier::Nerd), &d, None, "macos");
    assert!(o.prompt.left[0].starts_with("%F{red}\u{e711}%f \u{f179} \u{f179} C:ml ✦ "), "{:?}", o.prompt.left[0]);
    d.request.jobs = 2;
    d.request.env = "CONDA_DEFAULT_ENV=base".into();
    let o = render_with(&c, &d, None, "linux");
    assert_eq!(reason(&o, "conda").as_deref(), Some("show_when:env.CONDA_DEFAULT_ENV"));
    assert!(o.prompt.left[0].contains("&2"));
    d.request.env = String::new();
    let o = render_with(&c, &d, None, "linux");
    assert_eq!(reason(&o, "conda").as_deref(), Some("show_when:env.CONDA_DEFAULT_ENV"));
}

#[test]
fn fill_pads_the_row_to_the_terminal_width() {
    let c = cfg(
        r#"{"schema_version":1,"modules":{"f":{"type":"fill","symbol":"-"},"g":{"type":"fill"}},
           "layout":{"rows":[{"left":["cwd","f","jobs"]},{"left":["g"]},{"left":["cwd"]}]}}"#,
        SymbolTier::Unicode,
    );
    let mut d = base();
    d.request.cols = 30;
    d.request.jobs = 1;
    let o = render(&c, &d);
    // A row with only a fill is dropped; the first row is exactly 30 columns.
    assert_eq!(o.prompt.left.len(), 2);
    assert_eq!(o.columns[0], 30);
    assert_eq!(visible_width(&o.prompt.left[0]), 30);
    assert_eq!(strip_prompt_escapes(&o.prompt.left[0]), format!("~/code {} ✦1", "-".repeat(30 - 6 - 1 - 1 - 2)));
    d.request.cols = 0;
    let o = render(&c, &d);
    assert_eq!(strip_prompt_escapes(&o.prompt.left[0]), "~/code  ✦1");
}

#[test]
fn powerline_caps_padding_and_plain_rows() {
    let c = cfg(
        r##"{"schema_version":1,"modules":{
            "icon":{"type":"text","text":" A ","style":{"background":"#a3aed2","foreground":"#090c0c"}},
            "cwd":{"type":"directory","style":{"background":"#769ff0"}},
            "time":{"type":"time","time_format":"%R","style":{"background":"#1d2230"},"padding":false},
            "d":{"type":"duration","format":"in $human_ms ","padding":false},
            "char":{"type":"character","padding":false}},
          "layout":{"rows":[{"style":"powerline","separator":"","start":"░▒▓","end":" ","left":["icon","cwd","time","d","char"]}]}}"##,
        SymbolTier::Nerd,
    );
    let mut d = base();
    d.request.last_duration_ms = 3200;
    let o = render_at(&c, &d, at());
    assert_eq!(
        o.prompt.left[0],
        "%k%F{#a3aed2}░▒▓%K{#a3aed2}%F{#090c0c} A %F{#a3aed2}%K{#769ff0}\u{e0b4}%f ~/code %F{#769ff0}%K{#1d2230}\u{e0b4}%f14:05\
         %F{#1d2230}%k\u{e0b4} %fin 3s200ms %f❯%k%f "
    );
    assert_eq!(strip_prompt_escapes(&o.prompt.left[0]), "░▒▓ A \u{e0b4} ~/code \u{e0b4}14:05\u{e0b4} in 3s200ms ❯ ");
    assert_eq!(o.columns[0], visible_width(&o.prompt.left[0]));
    // Plain rows join with the row separator.
    let c = cfg(r#"{"schema_version":1,"layout":{"rows":[{"separator":"","left":["cwd","jobs"]}]}}"#, SymbolTier::Unicode);
    d.request.jobs = 1;
    let o = render(&c, &d);
    assert_eq!(strip_prompt_escapes(&o.prompt.left[0]), "~/code✦1 ❯ ");
    assert_eq!(o.columns[0], 11);
}

#[test]
fn character_error_symbol_and_durations() {
    let c = cfg(
        r#"{"schema_version":1,"modules":{"char":{"type":"character","symbol":"◎","error_symbol":"○"},
            "d":{"type":"duration","format":"$human|$human_ms|$duration"}},"layout":{"rows":[{"left":["d","char"]}]}}"#,
        SymbolTier::Unicode,
    );
    let mut d = base();
    d.request.last_duration_ms = 65_200;
    assert_eq!(strip_prompt_escapes(&render(&c, &d).prompt.left[0]), "1m5s|1m5s200ms|1m 5s ◎ ");
    d.request.last_exit = 1;
    d.request.last_duration_ms = 500;
    let o = render(&c, &d);
    assert_eq!(strip_prompt_escapes(&o.prompt.left[0]), "500ms|500ms|500ms ○ ");
    assert_eq!(o.prompt.transient.as_deref(), Some("○ "));
    assert_eq!(starship_duration(3_600_000 * 25 + 1000, false), "1d1h1s");
}

#[test]
fn repo_root_format_and_ignored_branches() {
    let c = cfg(
        r#"{"schema_version":1,"modules":{
            "cwd":{"type":"directory","truncation_symbol":"□ ","repo_root_format":"$before_root_path[$repo_root](bold blue)$path [△](blue)"},
            "git":{"type":"git","ignore_branches":["main"],"only_attached":true,"status_symbols":{},"format":"( [$branch](italic))$status"}},
           "layout":{"rows":[{"left":["cwd","git"]}]}}"#,
        SymbolTier::Unicode,
    );
    let d = repo("/Users/u/code/tildy/src", "/Users/u/code/tildy", json!({"unstaged": 1, "state": "dirty"}));
    let o = render(&c, &d);
    assert_eq!(o.reports[0].text, "□ tildy/src △");
    assert_eq!(o.reports[1].text, "!");
    let mut d = d;
    d.git.value.as_mut().unwrap().branch = Some("feat".into());
    assert_eq!(render(&c, &d).reports[1].text, " feat!");
    d.git.value.as_mut().unwrap().detached = true;
    assert_eq!(render(&c, &d).reports[1].text, "!");
    // Outside a repository the normal format applies.
    let o = render(&c, &base());
    assert_eq!(o.reports[0].text, "~/code");
}

// ---- Starship option parity (M2 task E1)

/// One-row config with the given modules, in the given order.
fn row_cfg(modules: &str, order: &[&str], t: SymbolTier) -> Config {
    let left: Vec<String> = order.iter().map(|s| format!("\"{s}\"")).collect();
    cfg(&format!(r#"{{"schema_version":1,"modules":{{{modules}}},"layout":{{"rows":[{{"left":[{}]}}]}}}}"#, left.join(",")), t)
}

fn text_of_report(o: &RenderOutput, id: &str) -> String {
    o.reports.iter().find(|r| r.id == id).unwrap().text.clone()
}

#[test]
fn add_newline_is_an_empty_first_line() {
    let c = cfg(r#"{"schema_version":1,"add_newline":true}"#, SymbolTier::Unicode);
    let o = render(&c, &base());
    assert_eq!(o.prompt.left, vec!["".to_string(), "%B%F{blue}~/code%f%b ❯ ".to_string()]);
    assert_eq!(o.columns, vec![0, 9]);
    assert_eq!(o.prompt.transient.as_deref(), Some("\n❯ "), "the transient form keeps the blank line (p10k)");
    let c = cfg(r#"{"schema_version":1,"add_newline":true,"history":{"add_newline":false}}"#, SymbolTier::Unicode);
    assert_eq!(render(&c, &base()).prompt.transient.as_deref(), Some("❯ "), "history.add_newline=false: one line");
    let c = cfg(r#"{"schema_version":1,"history":{"add_newline":true}}"#, SymbolTier::Unicode);
    assert_eq!(render(&c, &base()).prompt.transient.as_deref(), Some("❯ "), "history.add_newline alone adds nothing");
    // Two-row presets get a third line; false and absent change nothing.
    let c = cfg(r#"{"schema_version":1,"preset":"pastel-powerline","add_newline":true}"#, SymbolTier::Nerd);
    assert_eq!(render_at(&c, &base(), at()).prompt.left.len(), 3);
    let c = cfg(r#"{"schema_version":1,"add_newline":false}"#, SymbolTier::Unicode);
    assert_eq!(render(&c, &base()).prompt.left.len(), 1);
}

#[test]
fn directory_fish_style_logical_path_and_read_only() {
    let c = row_cfg(r#""cwd":{"type":"directory","fish_style_pwd_dir_length":1}"#, &["cwd"], SymbolTier::Unicode);
    let mut d = base();
    d.request.cwd = "/Users/u/Dev/Nix/nixpkgs/pkgs/top".into();
    assert_eq!(text_of_report(&render(&c, &d), "cwd"), "~/D/N/nixpkgs/pkgs/top");
    // Starship's own example: the repository root keeps everything below it.
    let d = repo("/Users/u/Dev/Nix/nixpkgs/pkgs", "/Users/u/Dev/Nix/nixpkgs", json!({}));
    assert_eq!(text_of_report(&render(&c, &d), "cwd"), "~/D/N/nixpkgs/pkgs");
    let c = row_cfg(r#""cwd":{"type":"directory","fish_style_pwd_dir_length":2,"truncation_length":2}"#, &["cwd"], SymbolTier::Unicode);
    let mut d = base();
    d.request.cwd = "/built/this/.city/on/rock".into();
    assert_eq!(text_of_report(&render(&c, &d), "cwd"), "/bu/th/.ci/on/rock");
    // Substitutions turn fish style off.
    let c = row_cfg(r#""cwd":{"type":"directory","fish_style_pwd_dir_length":1,"substitutions":{"x":"y"}}"#, &["cwd"], SymbolTier::Unicode);
    assert_eq!(text_of_report(&render(&c, &d), "cwd"), "…/.city/on/rock");

    // Read-only directory and the physical path.
    let td = tempfile::tempdir().unwrap();
    let real = td.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, td.path().join("link")).unwrap();
    let c = row_cfg(
        r#""cwd":{"type":"directory","truncation_length":0,"format":"$path[$read_only](red)","read_only":" RO"},
           "phys":{"type":"directory","truncation_length":0,"use_logical_path":false}"#,
        &["cwd", "phys"],
        SymbolTier::Unicode,
    );
    let mut d = base();
    d.home = None;
    d.request.cwd = td.path().join("link");
    // Two long paths on one row: wide enough that nothing is dropped.
    d.request.cols = 400;
    let canon = std::fs::canonicalize(&real).unwrap();
    let o = render(&c, &d);
    assert_eq!(text_of_report(&o, "cwd"), td.path().join("link").to_string_lossy());
    assert_eq!(text_of_report(&o, "phys"), canon.to_string_lossy());
    let is_root = unsafe { libc::geteuid() } == 0;
    std::fs::set_permissions(&real, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
    let o = render(&c, &d);
    std::fs::set_permissions(&real, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    if !is_root {
        assert_eq!(text_of_report(&o, "cwd"), format!("{} RO", td.path().join("link").display()));
        assert!(o.prompt.left[0].contains("%F{red} RO"), "{}", o.prompt.left[0]);
    }
    // A missing directory is not read-only.
    d.request.cwd = td.path().join("missing");
    assert!(!text_of_report(&render(&c, &d), "cwd").ends_with("RO"));
}

#[test]
fn directory_repo_root_style_splits_the_path() {
    let c = row_cfg(
        r#""cwd":{"type":"directory","repo_root_style":{"foreground":"red","bold":true},"before_repo_root_style":{"foreground":"blue"}}"#,
        &["cwd"],
        SymbolTier::Unicode,
    );
    let d = repo("/Users/u/code/tildy/src", "/Users/u/code/tildy", json!({}));
    let o = render(&c, &d);
    assert_eq!(text_of_report(&o, "cwd"), "…/tildy/src");
    assert!(o.prompt.left[0].contains("%B%F{red}tildy"), "{}", o.prompt.left[0]);
    // Without truncate_to_repo the root still splits where it shows.
    let c = row_cfg(
        r#""cwd":{"type":"directory","truncate_to_repo":false,"truncation_length":0,"format":"<$path>","repo_root_style":{"underline":true}}"#,
        &["cwd"],
        SymbolTier::Unicode,
    );
    let o = render(&c, &d);
    assert_eq!(text_of_report(&o, "cwd"), "<~/code/tildy/src>");
    assert!(o.prompt.left[0].contains("%U"), "{}", o.prompt.left[0]);
}

#[test]
fn git_branch_truncation_remote_and_up_to_date() {
    let c = row_cfg(
        r#""git":{"type":"git","truncation_length":4,"format":"$branch(:$remote_branch)( $status)","status_symbols":{"up_to_date":"✓"}}"#,
        &["git"],
        SymbolTier::Unicode,
    );
    let d = repo("/r", "/r", json!({"branch": "feature-long", "upstream": "origin/feature-long"}));
    assert_eq!(text_of_report(&render(&c, &d), "git"), "feat… ✓");
    let d = repo("/r", "/r", json!({"branch": "main", "upstream": "upstream/trunk", "ahead": 1}));
    assert_eq!(text_of_report(&render(&c, &d), "git"), "main:trun… ⇡");
    let d = repo("/r", "/r", json!({"branch": "main", "upstream": null}));
    assert_eq!(text_of_report(&render(&c, &d), "git"), "main", "no upstream: no up_to_date");
    let c = row_cfg(
        r#""git":{"type":"git","always_show_remote":true,"truncation_symbol":"","format":"$branch $remote_name/$remote_branch"}"#,
        &["git"],
        SymbolTier::Unicode,
    );
    let d = repo("/r", "/r", json!({"branch": "main", "upstream": "origin/main"}));
    assert_eq!(text_of_report(&render(&c, &d), "git"), "main origin/main");
}

#[test]
fn exit_status_symbols_meanings_and_styles() {
    let c = row_cfg(
        r#""st":{"type":"exit_status","format":"$symbol$status|$common_meaning|$signal_name|$maybe_int|$hex_status","symbol":"E",
           "success_symbol":"OK","map_symbol":true,"not_found_symbol":"NF","success_style":{"foreground":"green"},"error_style":{"foreground":"red"}}"#,
        &["st"],
        SymbolTier::Unicode,
    );
    let mut d = base();
    let t = |d: &PromptData| text_of_report(&render(&c, d), "st");
    assert_eq!(t(&d), "OK0||||0x0", "0 has a (blank) common meaning, so no maybe_int");
    assert!(render(&c, &d).prompt.left[0].contains("%F{green}"));
    d.request.last_exit = 127;
    assert_eq!(t(&d), "NF127|NOTFOUND|||0x7F");
    d.request.last_exit = 130;
    assert_eq!(t(&d), "🧱130||INT||0x82");
    d.request.last_exit = 137;
    assert_eq!(t(&d), "⚡137||KILL||0x89");
    d.request.last_exit = 3;
    assert_eq!(t(&d), "E3|||3|0x3");
    assert!(render(&c, &d).prompt.left[0].contains("%F{red}"));
    // recognize_signal_code = false: 130 is a plain code.
    let c = row_cfg(r#""st":{"type":"exit_status","format":"$symbol$maybe_int","recognize_signal_code":false,"map_symbol":true}"#, &["st"], SymbolTier::Ascii);
    d.request.last_exit = 130;
    assert_eq!(text_of_report(&render(&c, &d), "st"), "x130");
    // Default: hidden on success.
    let c = row_cfg(r#""st":{"type":"exit_status"}"#, &["st"], SymbolTier::Unicode);
    d.request.last_exit = 0;
    assert_eq!(reason(&render(&c, &d), "st").as_deref(), Some("empty"));
}

#[test]
fn duration_min_time_hostname_username_options() {
    let c = row_cfg(
        r#""dur":{"type":"duration","min_time":500,"show_milliseconds":true,"format":"$human"},
           "host":{"type":"hostname","trim_at":"-","aliases":{"box":"home"},"format":"$ssh_symbol$hostname","symbol":"@"},
           "user":{"type":"username","aliases":{"u":"me"},"detect_env_vars":["SHOWME"]}"#,
        &["dur", "host", "user"],
        SymbolTier::Unicode,
    );
    let mut d = base();
    d.request.last_duration_ms = 400;
    d.request.env = "HOST=box-2.example.com".into();
    let o = render(&c, &d);
    assert_eq!(reason(&o, "dur").as_deref(), Some("below_min_time"));
    assert_eq!(text_of_report(&o, "host"), "home", "trimmed at '-', then aliased; no ssh symbol outside SSH");
    assert_eq!(reason(&o, "user").as_deref(), Some("not_ssh_or_root"));
    d.request.last_duration_ms = 61_250;
    d.request.env = "HOST=box.lan\u{1f}SSH_CONNECTION=1 2 3 4\u{1f}SHOWME=1".into();
    let o = render(&c, &d);
    assert_eq!(text_of_report(&o, "dur"), "1m1s250ms");
    assert_eq!(text_of_report(&o, "host"), "@box.lan");
    assert_eq!(text_of_report(&o, "user"), "me");
    // trim_at "" shows the full $HOST; without HOST the session name is used.
    let c = row_cfg(r#""host":{"type":"hostname","trim_at":"","detect_env_vars":["!TMUX"]}"#, &["host"], SymbolTier::Unicode);
    assert_eq!(text_of_report(&render(&c, &d), "host"), "u@box.lan");
    d.request.env = "TMUX=x".into();
    assert_eq!(reason(&render(&c, &d), "host").as_deref(), Some("detect_env_vars"));
    d.request.env = String::new();
    assert_eq!(text_of_report(&render(&c, &d), "host"), "u@h");
}

#[test]
fn time_12hr_offset_and_range() {
    let c = row_cfg(
        r#""a":{"type":"time","use_12hr":true},
           "b":{"type":"time","utc_time_offset":"5.5","time_format":"%R"},
           "c":{"type":"time","time_range":"15:00:00-","time_format":"%R"},
           "d":{"type":"time","time_range":"22:00:00-06:00:00","time_format":"%R"}"#,
        &["a", "b", "c", "d"],
        SymbolTier::Unicode,
    );
    let o = render_at(&c, &base(), at());
    assert_eq!(text_of_report(&o, "a"), "02:05:09 PM");
    assert_eq!(text_of_report(&o, "b"), "19:35", "the injected time is read as UTC");
    assert_eq!(reason(&o, "c").as_deref(), Some("outside_time_range"));
    assert_eq!(reason(&o, "d").as_deref(), Some("outside_time_range"));
    let late = LocalTime::parse("2026-10-19T23:30:00");
    let o = render_at(&c, &base(), late);
    assert_eq!(text_of_report(&o, "c"), "23:30");
    assert_eq!(text_of_report(&o, "d"), "23:30", "a range across midnight");
}

#[test]
fn jobs_symbol_threshold() {
    let c = row_cfg(r#""j":{"type":"jobs","symbol_threshold":2,"number_threshold":1,"symbol":"*"}"#, &["j"], SymbolTier::Unicode);
    let mut d = base();
    d.request.jobs = 1;
    assert_eq!(text_of_report(&render(&c, &d), "j"), "1");
    d.request.jobs = 2;
    assert_eq!(text_of_report(&render(&c, &d), "j"), "*2");
    let c = row_cfg(r#""j":{"type":"jobs","symbol_threshold":0,"number_threshold":0,"symbol":"*"}"#, &["j"], SymbolTier::Unicode);
    d.request.jobs = 0;
    assert_eq!(text_of_report(&render(&c, &d), "j"), "*0", "thresholds 0 always show");
}

#[test]
fn new_env_modules_shlvl_nix_shell_shell() {
    let c = row_cfg(
        r#""lvl":{"type":"shlvl"},
           "rep":{"type":"shlvl","repeat":true,"repeat_offset":1,"symbol":"❯","format":"$symbol","threshold":1},
           "nix":{"type":"nix_shell"},
           "heur":{"type":"nix_shell","heuristic":true,"unknown_msg":"?"},
           "sh":{"type":"shell","indicators":{"zsh":"Z"}},
           "sh2":{"type":"shell"}"#,
        &["lvl", "rep", "nix", "heur", "sh", "sh2"],
        SymbolTier::Unicode,
    );
    let mut d = base();
    let o = render(&c, &d);
    assert_eq!(reason(&o, "lvl").as_deref(), Some("not_detected"));
    assert_eq!(reason(&o, "nix").as_deref(), Some("not_detected"));
    assert_eq!(text_of_report(&o, "sh"), "Z");
    assert_eq!(text_of_report(&o, "sh2"), "zsh");
    d.request.env = "SHLVL=1".into();
    let o = render(&c, &d);
    assert_eq!(reason(&o, "lvl").as_deref(), Some("below_threshold"));
    assert_eq!(reason(&o, "rep").as_deref(), Some("empty"), "1 - offset 1 = nothing to repeat");
    d.request.env = "SHLVL=3\u{1f}IN_NIX_SHELL=impure\u{1f}name=lorri\u{1f}TILDY_SHELL=fish".into();
    let o = render(&c, &d);
    assert_eq!(text_of_report(&o, "lvl"), "↕ 3");
    assert_eq!(text_of_report(&o, "rep"), "❯❯");
    assert_eq!(text_of_report(&o, "nix"), "❄ impure (lorri)");
    assert_eq!(text_of_report(&o, "sh2"), "fsh");
    d.request.env = "PATH=/nix/store/abc-x/bin:/usr/bin".into();
    let o = render(&c, &d);
    assert_eq!(reason(&o, "nix").as_deref(), Some("not_detected"));
    assert_eq!(text_of_report(&o, "heur"), "❄ ?");
    // ASCII tier symbols.
    let c = row_cfg(r#""lvl":{"type":"shlvl"},"nix":{"type":"nix_shell"}"#, &["lvl", "nix"], SymbolTier::Ascii);
    d.request.env = "SHLVL=2\u{1f}IN_NIX_SHELL=pure".into();
    let o = render(&c, &d);
    assert_eq!((text_of_report(&o, "lvl"), text_of_report(&o, "nix")), ("lvl 2".to_string(), "nix pure".to_string()));
}

#[test]
fn new_provider_modules_render_their_slots() {
    let c = row_cfg(
        r#""pkg":{"type":"package"},
           "gc":{"type":"git_commit","only_detached":false,"tag_disabled":false,"commit_hash_length":10},
           "gm":{"type":"git_metrics"},
           "gm0":{"type":"git_metrics","only_nonzero_diffs":false,"format":"+$added -$deleted"},
           "ip":{"type":"localip"},
           "mem":{"type":"memory_usage","threshold":50},
           "su":{"type":"sudo"},
           "de":{"type":"direnv","loaded_msg":"[on](green)"}"#,
        &["pkg", "gc", "gm", "gm0", "ip", "mem", "su", "de"],
        SymbolTier::Unicode,
    );
    let mut d = repo("/r", "/r", json!({}));
    d.request.cols = 200;
    d.modules.insert("pkg".into(), module("1.4.0", &[("version", "1.4.0")]));
    d.modules.insert("gc".into(), module("", &[("hash", "0123456789abcdef0123456789abcdef01234567"), ("tag", "v1.0")]));
    d.modules.insert("gm".into(), module("", &[("added", "12"), ("deleted", "0")]));
    d.modules.insert("gm0".into(), module("", &[("added", "0"), ("deleted", "0")]));
    d.modules.insert("ip".into(), module("10.0.0.7", &[("localipv4", "10.0.0.7")]));
    d.modules.insert(
        "mem".into(),
        module("21GiB/32GiB", &[("ram", "21GiB/32GiB"), ("ram_pct", "66%"), ("pct", "66"), ("swap", "3GiB/5GiB"), ("swap_pct", "60%")]),
    );
    d.modules.insert("su".into(), module("", &[]));
    d.modules.insert("de".into(), module("", &[("rc_path", "/r/.envrc"), ("allowed", "allowed"), ("loaded", "true")]));
    let o = render(&c, &d);
    assert_eq!(text_of_report(&o, "pkg"), "📦 v1.4.0");
    assert_eq!(text_of_report(&o, "gc"), "(0123456789 🏷 v1.0)");
    assert_eq!(text_of_report(&o, "gm"), "+12");
    assert!(o.prompt.left[0].contains("%B%F{green}+12"), "{}", o.prompt.left[0]);
    assert_eq!(text_of_report(&o, "gm0"), "+0 -0");
    assert_eq!(text_of_report(&o, "ip"), "10.0.0.7");
    assert_eq!(text_of_report(&o, "mem"), "🐏 21GiB/32GiB | 3GiB/5GiB");
    assert_eq!(text_of_report(&o, "su"), "🧙");
    assert_eq!(text_of_report(&o, "de"), "direnv on/allowed");
    // git_commit: the short hash from the git data, only on a detached HEAD
    // by default; no provider slot needed.
    let c = row_cfg(r#""gc":{"type":"git_commit"}"#, &["gc"], SymbolTier::Unicode);
    let d = repo("/r", "/r", json!({}));
    assert_eq!(reason(&render(&c, &d), "gc").as_deref(), Some("not_detached"));
    let d = repo("/r", "/r", json!({"detached": true, "branch": null}));
    assert_eq!(text_of_report(&render(&c, &d), "gc"), "(abc1234)");
    assert_eq!(reason(&render(&c, &base()), "gc").as_deref(), Some("no_data"));
    // memory below the threshold, package semantic marker, nothing detected.
    let c = row_cfg(r#""mem":{"type":"memory_usage"},"pkg":{"type":"package","version_format":"${major}"}"#, &["mem", "pkg"], SymbolTier::Ascii);
    let mut d = base();
    d.modules.insert("mem".into(), module("x", &[("pct", "40"), ("ram", "x")]));
    d.modules.insert("pkg".into(), module("2.5.1", &[("version", "2.5.1")]));
    let o = render(&c, &d);
    assert_eq!(reason(&o, "mem").as_deref(), Some("below_threshold"));
    assert_eq!(text_of_report(&o, "pkg"), "pkg 2");
    d.modules.insert("pkg".into(), module("semantic", &[("version", "semantic")]));
    assert_eq!(text_of_report(&render(&c, &d), "pkg"), "pkg semantic");
    d.modules.insert("pkg".into(), Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: None, error: None });
    assert_eq!(reason(&render(&c, &d), "pkg").as_deref(), Some("not_detected"));
}

// ---- Shrinking a row to the terminal width (pulled forward from M3)

/// The author's daily situation: a deep directory in a repository with a long
/// branch, node and python versions, a docker context, a slow last command
/// and a background job.
fn busy_repo(cols: u16) -> PromptData {
    let mut d = repo(
        "/Users/u/code/work/ai-notes/services/api",
        "/Users/u/code/work/ai-notes",
        json!({"branch": "feature/resize-redraw", "unstaged": 2, "untracked": 1, "state": "dirty"}),
    );
    d.request.cols = cols;
    d.request.last_duration_ms = 3200;
    d.request.jobs = 1;
    d.modules.insert("node".into(), module("22.3.0", &[("version", "22.3.0")]));
    d.modules.insert("py".into(), module("3.12.1", &[("version", "3.12.1"), ("env", "demo")]));
    d.modules.insert("docker".into(), module("colima", &[("context", "colima")]));
    d
}

fn dropped(o: &RenderOutput) -> Vec<String> {
    o.reports.iter().filter(|r| r.hidden_reason.as_deref() == Some("no_room")).map(|r| r.id.clone()).collect()
}

#[test]
fn rows_shrink_by_priority_and_powerline_caps_follow() {
    let c = pastel(SymbolTier::Nerd);
    let full = render_at(&c, &busy_repo(200), at());
    assert!(dropped(&full).is_empty());
    let wide = full.columns[0];
    // Preset order: time, language versions, docker/python (rightmost first),
    // elapsed, jobs, git, user; the directory is the highest and stays.
    let order = ["time", "node", "docker", "py", "elapsed", "jobs", "git", "user"];
    let mut seen = 0;
    for cols in (10..=wide as u16 + 1).rev() {
        let o = render_at(&c, &busy_repo(cols), at());
        let gone = dropped(&o);
        assert!(gone.len() >= seen, "cols {cols}: a module came back: {gone:?}");
        seen = gone.len();
        let mut want: Vec<&str> = order[..gone.len()].to_vec();
        want.sort_by_key(|id| o.reports.iter().position(|r| r.id == *id));
        assert_eq!(gone, want, "cols {cols}");
        // The first row fits unless only the directory is left; the second row
        // (the prompt character) always leaves the cursor a column.
        if gone.len() < order.len() {
            assert!(o.columns[0] <= cols as usize, "cols {cols}: row 1 is {} wide", o.columns[0]);
        }
        assert!(o.columns[1] < cols as usize);
        // The run of segments still ends with a cap in the last visible
        // segment's background; no separator points at a dropped segment.
        let last_bg = o
            .reports
            .iter()
            .rfind(|r| r.visible && r.id != "char")
            .map(|r| c.file.modules[&r.id].style.background.clone().unwrap().to_lowercase())
            .unwrap();
        assert!(o.prompt.left[0].ends_with(&format!("%k%F{{{last_bg}}}\u{e0b0}%f")), "cols {cols}: {:?}", o.prompt.left[0]);
        // One separator per change of background, plus the end cap.
        let bgs: Vec<String> =
            o.reports.iter().filter(|r| r.visible && r.id != "char").map(|r| c.file.modules[&r.id].style.background.clone().unwrap()).collect();
        let runs = 1 + bgs.windows(2).filter(|w| w[0] != w[1]).count();
        assert_eq!(o.prompt.left[0].matches('\u{e0b0}').count(), runs, "cols {cols}: {:?}", o.prompt.left[0]);
        assert_eq!(o.prompt.transient.as_deref(), Some("%B%F{green}❯%f%b "), "transient is never shrunk");
    }
    assert_eq!(seen, order.len());
    assert!(order.len() < full.reports.iter().filter(|r| r.visible).count());
}

#[test]
fn equal_priorities_drop_the_rightmost_first() {
    let c = row_cfg(
        r#""a":{"type":"text","text":"aaaa"},"b":{"type":"text","text":"bbbb"},"z":{"type":"text","text":"zzzz","priority":10}"#,
        &["a", "z", "b"],
        SymbolTier::Unicode,
    );
    let mut d = base();
    // "aaaa zzzz bbbb ❯ " is 17 wide; the last row keeps a column for the cursor.
    for (cols, gone) in [(18, vec![]), (17, vec!["z"]), (13, vec!["z"]), (12, vec!["z", "b"]), (3, vec!["z", "b"])] {
        d.request.cols = cols;
        let o = render(&c, &d);
        assert_eq!(dropped(&o), gone, "cols {cols}");
    }
    d.request.cols = 3;
    let o = render(&c, &d);
    // The highest-priority module stays even though the row still wraps.
    assert_eq!(strip_prompt_escapes(&o.prompt.left[0]), "aaaa ❯ ");
}

#[test]
fn the_character_and_the_last_module_are_never_dropped() {
    let c = cfg(
        r#"{"schema_version":1,"modules":{"char":{"type":"character","priority":0},"f":{"type":"fill"}},
           "layout":{"rows":[{"left":["cwd","f"]},{"left":["jobs","char"]}]}}"#,
        SymbolTier::Unicode,
    );
    let mut d = base();
    d.request.cwd = "/Users/u/a/very/long/directory/name".into();
    d.request.jobs = 3;
    d.request.cols = 4;
    let o = render(&c, &d);
    assert!(dropped(&o).is_empty(), "{:?}", dropped(&o));
    assert_eq!(strip_prompt_escapes(&o.prompt.left[1]), "✦3 ❯ ");
    assert!(o.columns[0] > 4, "the directory alone is wider and wraps");
    // cols 0: width unknown, nothing is dropped anywhere.
    assert!(dropped(&render_at(&pastel(SymbolTier::Nerd), &busy_repo(0), at())).is_empty());
}

#[test]
fn the_last_row_counts_rprompt_and_its_gap() {
    let c = uni();
    let mut d = base();
    d.request.last_exit = 2;
    d.request.last_duration_ms = 3200;
    // "~/code ❯ " (9) + "took 3.2s ✗ 2" (13): Zsh shows RPROMPT when
    // left + right <= cols - 2, i.e. at 24 columns.
    d.request.cols = 24;
    let o = render(&c, &d);
    assert!(dropped(&o).is_empty());
    assert_eq!(o.prompt.right.as_deref(), Some("%F{yellow}took 3.2s%f %F{red}✗ 2%f"));
    d.request.cols = 23;
    let o = render(&c, &d);
    assert_eq!(dropped(&o), vec!["elapsed"]);
    assert_eq!(o.prompt.right.as_deref(), Some("%F{red}✗ 2%f"));
    // Equal priority 100: the exit status (rightmost) goes before the directory.
    d.request.cols = 12;
    let o = render(&c, &d);
    assert_eq!(dropped(&o), vec!["elapsed", "result"]);
    assert_eq!(o.prompt.right, None);
    assert_eq!(o.prompt.left, vec!["%B%F{blue}~/code%f%b ❯ "]);
}

#[test]
fn upper_right_is_still_all_or_nothing_after_the_left_shrinks() {
    let c = explicit_test_config(r#"{"preset":"lean"}"#).unwrap().with_symbols(SymbolTier::Unicode);
    let o = render_at(&c, &busy_repo(80), at());
    assert!(dropped(&o).is_empty(), "{:?}", dropped(&o));
    let o = render_at(&c, &busy_repo(40), at());
    // git goes so the directory fits; the right side has no room as a whole.
    assert_eq!(dropped(&o), vec!["git", "elapsed", "jobs", "time"]);
    assert!(o.columns[0] <= 40);
}

#[test]
fn m3_history_context_is_stable_and_bounded() {
    let cfg = explicit_test_config(r#"{"schema_version":1,"preset":"pastel-powerline","add_newline":true}"#).unwrap();
    let mut d = repo("/tmp/repo/a", "/tmp/repo", json!({}));
    let first = render(&cfg, &d);
    let h = first.prompt.history.unwrap();
    assert!(h.context_key.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(h.context_transient.as_ref().unwrap().contains("── tildy · main ──"));
    assert!(h.context_transient.unwrap().lines().count() <= first.prompt.left.len());
    d.request.cwd = "/tmp/repo/b".into();
    assert_eq!(render(&cfg, &d).prompt.history.unwrap().context_key, h.context_key);
    d.git.value.as_mut().unwrap().branch = Some("feature".into());
    assert_ne!(render(&cfg, &d).prompt.history.unwrap().context_key, h.context_key);
    // The protocol caps a metadata line at 4000 bytes; a long repository
    // prefix must not truncate away the differing branch at the tuple's end.
    d.git.value.as_mut().unwrap().worktree_root = Some(format!("/{}", "r".repeat(3000)).into());
    let long_key = render(&cfg, &d).prompt.history.unwrap().context_key;
    assert!(long_key.len() <= crate::protocol::MAX_LINE_BYTES);
    d.git.value.as_mut().unwrap().branch = Some("other".into());
    assert_ne!(render(&cfg, &d).prompt.history.unwrap().context_key, long_key);
    d.git.value.as_mut().unwrap().branch = Some("feature".into());
    let cfg = explicit_test_config(r#"{"schema_version":1,"history":{"mode":"full"}}"#).unwrap();
    let out = render(&cfg, &d);
    assert!(out.prompt.history.is_none() && out.prompt.transient.is_none());
    let cfg = uni();
    let h = render(&cfg, &d).prompt.history.unwrap();
    assert!(h.context_transient.is_none());
    assert_eq!(h.context_suffix.as_deref(), Some("  · feature"));
}

#[test]
fn m3_history_key_waits_for_git() {
    let cfg = explicit_test_config(r#"{"schema_version":1,"preset":"pastel-powerline","add_newline":true}"#).unwrap();
    let mut d = repo("/tmp/repo", "/tmp/repo", json!({}));
    let settled = render(&cfg, &d).prompt.history.unwrap().context_key;
    assert!(!settled.is_empty());
    d.git.value = None;
    d.git.freshness = crate::model::Freshness::Pending;
    let out = render(&cfg, &d);
    let h = out.prompt.history.clone().unwrap();
    assert!(h.context_key.is_empty());
    assert!(h.summary, "summary stays on without a key");
    let frame = crate::protocol::encode_result(1, 1, &out.prompt);
    assert!(!frame.contains("\nK\t") && frame.contains("\nS\t"), "{frame}");
}

#[test]
fn m3_changes_are_ordinary_high_priority_segments() {
    let mut d = repo("/tmp/repo", "/tmp/repo", json!({}));
    d.request.cols = 160;
    d.changes = Some(crate::model::ChangeData {
        branch_from: Some("main".into()), branch_to: Some("feature".into()),
        files_added: 3, environments: vec!["venv → test".into()],
    });
    let out = render(&config::builtin(), &d);
    let r = out.reports.iter().find(|r| r.id == "changes").unwrap();
    assert!(r.visible);
    assert!(r.text.contains("main → feature") && r.text.contains("✚3") && r.text.contains("venv → test"), "{}", r.text);
    d.changes = None;
    assert_eq!(reason(&render(&config::builtin(), &d), "changes").as_deref(), Some("no_changes"));
}

#[test]
fn m3_unicode_sequences_and_collision_thresholds() {
    for (s, width) in [("中文", 4), ("e\u{301}", 1), ("👩\u{200d}💻", 2), ("☀\u{fe0f}", 2), ("\u{e0b0}", 1)] {
        assert_eq!(visible_width(s), width, "{s:?}");
    }
    let cfg = explicit_test_config(r#"{"schema_version":1,"modules":{
        "a":{"type":"text","text":"aaaaaa","priority":1},
        "b":{"type":"text","text":"bbbb","priority":2},
        "c":{"type":"text","text":"c","priority":99},
        "char":{"type":"character","priority":100}},
        "layout":{"rows":[{"left":["a","b","c","char"]}]}}"#).unwrap();
    let mut d = base(); d.request.cols = 4;
    let small = render(&cfg, &d);
    let mut amounts = vec![];
    for id in ["a", "b"] {
        d.request.cols = 4;
        let r = small.reports.iter().find(|r| r.id == id).unwrap();
        assert_eq!(r.hidden_reason.as_deref(), Some("no_room"));
        let extra = r.additional_columns.unwrap(); amounts.push(extra);
        d.request.cols = (4 + extra) as u16;
        assert!(render(&cfg, &d).reports.iter().find(|r| r.id == id).unwrap().visible);
        d.request.cols -= 1;
        assert!(!render(&cfg, &d).reports.iter().find(|r| r.id == id).unwrap().visible);
    }
    assert!(amounts[0] > amounts[1]);
}

#[test]
fn m3_typechanged_and_icon_padding() {
    let d = repo("/tmp/repo", "/tmp/repo", json!({"typechanged":2,"unstaged":2}));
    let cfg = explicit_test_config(r#"{"schema_version":1,"modules":{"git":{"format":"$typechanged","status_symbols":{"typechanged":"T$count"}},"n":{"type":"text","text":"unused"}},"layout":{"rows":[{"left":["git"]}]}}"#).unwrap();
    assert!(strip_prompt_escapes(&render(&cfg, &d).prompt.left[0]).contains("T2"));
    assert_eq!(starship_status(d.git.value.as_ref().unwrap(), &StatusSymbols::default(), true, SymbolTier::Unicode), "~2");
    let cfg = explicit_test_config(r#"{"schema_version":1,"modules":{"x":{"type":"command","symbol":"X","icon_padding":2,"format":"$symbol$text","argv":["true"]}},"layout":{"rows":[{"left":["x"]}]}}"#).unwrap();
    let mut d=base(); d.modules.insert("x".into(),Slot::fresh(ModuleValue { text:"ok".into(), ..Default::default() }));
    assert!(strip_prompt_escapes(&render(&cfg,&d).prompt.left[0]).contains("X  ok"));
}

#[test]
fn m3_long_context_and_changes_do_not_wrap() {
    let cfg = explicit_test_config(r#"{"schema_version":1,"preset":"pastel-powerline","add_newline":true}"#).unwrap();
    let mut d = repo("/tmp/r", "/tmp/r", json!({"branch":"你好👩‍💻超长分支abcdefghijklmnopqrstuvwxyz"}));
    d.request.cols=24;
    let out=render(&cfg,&d);
    let history=out.prompt.history.unwrap();
    assert!(history.context_transient.unwrap().split('\n').all(|line| visible_width(line)<24));
    d.changes=Some(crate::model::ChangeData { branch_from:Some("main".into()), branch_to:d.git.value.as_ref().unwrap().branch.clone(), ..Default::default() });
    let out=render(&cfg,&d);
    let r=out.reports.iter().find(|r|r.id=="changes").unwrap();
    assert!(!r.visible || r.columns<=24);
}

#[test]
fn m3_width_grapheme_boundaries_are_kept_during_all_truncation() {
    let profile=crate::width::WidthProfile { version:1,identity:crate::width::Identity {term:"xterm".into(),program:String::new(),program_version:String::new(),locale:"UTF-8".into(),font_revision:"test".into()},entries:vec![crate::width::Entry{cluster:"👩‍💻".into(),columns:3}] };
    crate::width::with_policy(crate::width::WidthPolicy::from_profile(profile).unwrap(),|| {
        assert_eq!(fit_columns("e\u{301}👩‍💻x",3),"e\u{301}");
        assert_eq!(fit_columns("e\u{301}👩‍💻x",4),"e\u{301}👩‍💻");
        assert_eq!(fish_word("👩‍💻abc",1),"👩‍💻");
        assert_eq!(truncate_name("👩‍💻abc",Some(1),"…"),"👩‍💻…");
        assert_eq!(agent::truncate_cols("👩‍💻abcdef",4,"…"),"👩‍💻…");
        assert_eq!(strip_prompt_escapes("%{👩‍💻%3G%}%{%%%1G%}"),"👩‍💻%");
        assert_eq!(visible_width("%{👩‍💻%3G%}%{%%%1G%}"),4);
    });
}

#[test]
fn borrowed_prefix_fitting_preserves_unicode_prefix_semantics() {
    for text in ["e\u{301}👩‍💻%abc", "🇦🇧🇨🇩🇪", "ا\u{0644}\u{0623}x", "\u{301}x", "\u{1b}unsafe\n👩‍💻", &"你好e\u{301}👩‍💻".repeat(1000)] {
        let safe = sanitize(text);
        for max in [0,1,2,3,4,7,24,80,250] {
            let mut expected = String::new();
            for cluster in safe.graphemes(true) {
                let next = format!("{expected}{cluster}");
                if crate::width::width(&next) > max { break; }
                expected = next;
            }
            assert_eq!(fit_columns(text,max),expected);
        }
    }
}

#[test]
fn context_key_hex_keeps_exact_json_bytes_and_long_path_distinction() {
    let cfg = config::builtin();
    let mut d = repo("/tmp/你好%", "/tmp/你好%", json!({"branch":"👩‍💻e\u{301}%"}));
    d.ssh = true;
    d.hostname = Some("主机".into());
    let out = render(&cfg,&d);
    let expected = serde_json::to_vec(&("/tmp/你好%", "👩‍💻e\u{301}%", "主机")).unwrap().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    assert_eq!(out.prompt.history.unwrap().context_key,expected);
    d.git.value.as_mut().unwrap().worktree_root = Some(Path::new("/").join("路径".repeat(1000)));
    let a = render(&cfg,&d).prompt.history.unwrap().context_key;
    d.git.value.as_mut().unwrap().branch = Some("different".into());
    let b = render(&cfg,&d).prompt.history.unwrap().context_key;
    assert!(a.len() <= crate::protocol::MAX_LINE_BYTES);
    assert_ne!(a,b);
}
