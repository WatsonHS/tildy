//! The same measured policy is used for layout, reports, truncation and prompt
//! declarations. No TTY queries occur in these deterministic profile tests.
use tildy::config::{self, SymbolTier};
use tildy::model::PromptData;
use tildy::render::{self, visible_width};
use tildy::width::{self, Entry, Identity, WidthPolicy, WidthProfile};
use serde_json::json;
use std::path::Path;
use std::process::Command;

fn profile() -> WidthProfile {
    WidthProfile { version: 1, identity:Identity { term:"xterm-256color".into(),program:"test-terminal".into(),program_version:"1".into(),locale:"en_US.UTF-8".into(),font_revision:"font-1".into() },entries:vec![Entry { cluster:"👩‍💻".into(),columns:4 }, Entry { cluster:"中".into(),columns:1 }, Entry { cluster:"%".into(),columns:1 }, Entry { cluster:"e\u{301}".into(),columns:1 }] }
}
fn data() -> PromptData {
    serde_json::from_value(json!({"request":{"gen":1,"session_id":"width-test","cwd":"/tmp/repo","cols":24,"lines":2,"last_exit":0,"last_duration_ms":0,"env":"TILDY_FONT_REVISION="},"git":{"freshness":"fresh","age_ms":0,"error":null,"value":null},"home":"/tmp","ssh":false,"hostname":null,"username":null,"modules":{}})).unwrap()
}
#[test]
fn scoped_profile_changes_real_layout_reports_and_right_collision() {
    let config=config::from_str(r#"{"schema_version":1,"preset":"plain","add_newline":false,"modules":{"a":{"type":"text","text":"👩‍💻中%","priority":100},"b":{"type":"text","text":"RIGHT","priority":1}},"layout":{"rows":[{"left":["a"],"right":["b"]}]}}"#).unwrap().with_symbols(SymbolTier::Unicode);
    let mut data=data(); data.request.cols=15;
    let measured=render::render_with_policy(&config,&data,None,"macos",WidthPolicy::from_profile(profile()).unwrap());
    let a=measured.reports.iter().find(|r|r.id=="a").unwrap();
    assert_eq!((a.text.as_str(),a.columns),("👩‍💻中%",6));
    assert!(measured.prompt.left[0].contains("%{👩‍💻%4G%}%{中%1G%}%{%%%1G%}"));
    assert_eq!(visible_width(&measured.prompt.left[0]),measured.columns[0]);
    assert!(measured.reports.iter().find(|r|r.id=="b").unwrap().hidden_reason.as_deref()==Some("no_room"));
    let normal=render::render(&config,&data);
    assert!(normal.prompt.right.is_some());
    assert!(!normal.prompt.left[0].contains("%4G"));
}
#[test]
fn graphemes_survive_branch_directory_agent_and_fill_truncation() {
    let config=config::from_str(r#"{"schema_version":1,"preset":"plain","add_newline":false,"modules":{"git":{"format":"$branch","truncation_length":1,"truncation_symbol":"…"},"fill":{"type":"fill","symbol":"👩‍💻"}},"layout":{"rows":[{"left":["git","fill"]}]}}"#).unwrap().with_symbols(SymbolTier::Unicode);
    let mut data=data();
    data.git=serde_json::from_value(json!({"freshness":"fresh","age_ms":0,"error":null,"value":{"in_repo":true,"git_dir":"/tmp/repo/.git","worktree_root":"/tmp/repo","worktree_id":"r","branch":"👩‍💻abc","detached":false,"unborn":false,"head_short":"abc123","upstream":null,"ahead":0,"behind":0,"staged":0,"unstaged":0,"untracked":0,"conflicted":0,"incomplete":false,"state":"clean"}})).unwrap();
    let output=render::render_with_policy(&config,&data,None,"macos",WidthPolicy::from_profile(profile()).unwrap());
    let git=output.reports.iter().find(|r|r.id=="git").unwrap();
    assert_eq!(git.text,"👩‍💻…");
    assert_eq!(output.columns[0],24);
    assert_eq!(visible_width(&output.prompt.left[0]),24);
    assert!(!render::strip_prompt_escapes(&output.prompt.left[0]).contains("👩…"));
}
fn command(cache: &Path) -> Command {
    let mut command=Command::new(env!("CARGO_BIN_EXE_tildy"));
    command.env("XDG_CACHE_HOME",cache).env("TERM","xterm-256color").env("TERM_PROGRAM","test-terminal").env("TERM_PROGRAM_VERSION","1").env("LC_ALL","en_US.UTF-8").env("TILDY_FONT_REVISION","font-1");
    command
}
#[test]
fn preview_loads_matching_bounded_profile_and_rejects_identity_drift() {
    let dir=tempfile::tempdir().unwrap();
    let cache=dir.path().join("cache");
    width::save_profile_at(&cache.join("tildy/width/profile.json"),&profile()).unwrap();
    let config=dir.path().join("config.json");
    std::fs::write(&config,r#"{"schema_version":1,"preset":"plain","add_newline":false,"modules":{"measured":{"type":"text","text":"👩‍💻中%"}},"layout":{"rows":[{"left":["measured"]}]}}"#).unwrap();
    let run=|font:&str| {
        let out=command(&cache).env("TILDY_FONT_REVISION",font).args(["preview","--scenario","plain-dir","--config",config.to_str().unwrap(),"--json"]).output().unwrap();
        assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let measured=run("font-1"); let fallback=run("other-font");
    let module=|value: &serde_json::Value| value["data"]["modules"].as_array().unwrap().iter().find(|r|r["id"]=="measured").unwrap()["columns"].as_u64().unwrap();
    assert_eq!(module(&measured),6); assert_eq!(module(&fallback),5);
    std::fs::write(cache.join("tildy/width/profile.json"),b"{\"version\":999}").unwrap();
    assert_eq!(module(&run("font-1")),5);
}

#[test]
fn fallback_characters_and_empty_layout_use_the_same_declarations() {
    let mut p=profile(); p.entries.push(Entry{cluster:"❯".into(),columns:2});
    let policy=WidthPolicy::from_profile(p).unwrap();
    let mut config=config::from_str(r#"{"schema_version":1,"preset":"plain","add_newline":false,"modules":{"x":{"type":"text","text":"hi"}},"layout":{"rows":[{"left":["x"]}]}}"#).unwrap().with_symbols(SymbolTier::Unicode);
    let data=data();
    let out=render::render_with_policy(&config,&data,None,"macos",policy.clone());
    assert!(out.prompt.left[0].contains("%{❯%2G%}"));
    assert!(out.prompt.transient.as_ref().unwrap().contains("%{❯%2G%}"));
    assert_eq!(visible_width(&out.prompt.left[0]),out.columns[0]);
    config.file.layout.rows.clear();
    let empty=render::render_with_policy(&config,&data,None,"macos",policy);
    assert_eq!(empty.prompt.left,vec!["%{❯%2G%} "]);
    assert_eq!(empty.columns,vec![3]);
}
#[test]
fn shell_measurement_preserves_percent_and_explicit_cell_advances() {
    let source=include_str!("../shell/zsh/tildy.zsh");
    let function=source.split("_tildy_width() {").nth(1).unwrap().split("\n# _tildy_shown_w").next().unwrap();
    let code=format!("_tildy_width() {{{function}\n_tildy_width \"$TILDY_TEST_WIDTH_TEXT\"\nprint -r -- $REPLY");
    for (text,columns) in [("%{👩‍💻%4G%}%{%%%1G%}%F{red}x%f",6),("%%{x%%}",5),("%{e\u{301}%1G%}",1),("%{\u{1b}[3m%}x",1)] {
        let out=Command::new("zsh").args(["-fc",&code]).env("TILDY_TEST_WIDTH_TEXT",text).output().unwrap();
        assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(),columns.to_string(),"{text:?}: {}",String::from_utf8_lossy(&out.stderr));
    }
}

#[test]
fn worker_and_oneshot_render_with_request_identity_and_safe_fallback() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    let dir=tempfile::tempdir().unwrap(); let cache=dir.path().join("cache");
    width::save_profile_at(&cache.join("tildy/width/profile.json"),&profile()).unwrap();
    let config=dir.path().join("config.json");
    std::fs::write(&config,r#"{"schema_version":1,"preset":"plain","add_newline":false,"modules":{"measured":{"type":"text","text":"👩‍💻中%"}},"layout":{"rows":[{"left":["measured"]}]}}"#).unwrap();
    let request=|font:&str,gen:u64| {
        let mut d=data(); d.request.gen=gen; d.request.cwd=dir.path().into();
        d.request.env=format!("TILDY_FONT_REVISION={font}\u{1f}TERM=xterm-256color\u{1f}TERM_PROGRAM=test-terminal\u{1f}TERM_PROGRAM_VERSION=1\u{1f}LC_ALL=en_US.UTF-8");
        let mut bytes=tildy::protocol::encode_request(&d.request); bytes.push(b'\n'); bytes
    };
    let mut oneshot=command(&cache).args(["render","--config",config.to_str().unwrap()]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    oneshot.stdin.take().unwrap().write_all(&request("font-1",1)).unwrap();
    let output=oneshot.wait_with_output().unwrap(); assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("%{👩‍💻%4G%}"));
    let mut worker=command(&cache).env("XDG_STATE_HOME",dir.path().join("state")).args(["worker","--config",config.to_str().unwrap()]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input=worker.stdin.take().unwrap(); let mut output=BufReader::new(worker.stdout.take().unwrap());
    let mut line=String::new(); output.read_line(&mut line).unwrap(); assert!(line.starts_with("HELLO\t"));
    for (font,gen,measured) in [("font-1",1,true),("other-font",2,false),("",3,false)] {
        input.write_all(&request(font,gen)).unwrap(); input.flush().unwrap();
        let mut frame=String::new();
        loop {
            line.clear();
            assert!(output.read_line(&mut line).unwrap()>0);
            // A late fresh frame from the previous generation may arrive
            // before this request's response. Never merge its width escapes
            // into the frame whose font identity we are checking.
            if line.starts_with("RES\t") { frame.clear(); }
            frame.push_str(&line);
            if line==format!("END\t{gen}\n") { break; }
        }
        assert!(frame.starts_with(&format!("RES\t{gen}\t")),"{frame}");
        assert_eq!(frame.contains("%{👩‍💻%4G%}"),measured,"{font}: {frame}");
    }
    input.write_all(b"BYE\n").unwrap(); drop(input); assert!(worker.wait().unwrap().success());
}
