//! Language is a presentation choice: JSON and shell bindings stay stable.
use std::path::Path;
use std::process::{Command, Output};

fn command(home: &Path, language: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tildy"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("TILDY_CONFIG", home.join("config.json"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("LANG", "zh_CN.UTF-8")
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env_remove("TILDY_LANG")
        .env_remove("ZDOTDIR");
    if let Some(language) = language {
        command.env("TILDY_LANG", language);
    }
    command
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn has_chinese(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes)
        .chars()
        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

#[test]
fn human_lists_translate_but_json_is_language_independent() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        &["preset", "list"][..],
        &["palette", "list"],
        &["preview", "--list"],
    ] {
        let english = success(command(dir.path(), None).args(args).output().unwrap());
        assert!(
            !has_chinese(&english.stdout),
            "{:?}",
            String::from_utf8_lossy(&english.stdout)
        );
        let chinese = success(command(dir.path(), Some("zh")).args(args).output().unwrap());
        assert!(has_chinese(&chinese.stdout));
        let auto = success(
            command(dir.path(), Some("auto"))
                .args(args)
                .output()
                .unwrap(),
        );
        assert_eq!(chinese.stdout, auto.stdout);
        let en_json = success(
            command(dir.path(), None)
                .args(args)
                .arg("--json")
                .output()
                .unwrap(),
        );
        let zh_json = success(
            command(dir.path(), Some("zh"))
                .args(args)
                .arg("--json")
                .output()
                .unwrap(),
        );
        assert_eq!(en_json.stdout, zh_json.stdout);
    }
}

#[test]
fn setup_and_theme_default_to_english_even_in_a_chinese_locale() {
    for (language, chinese) in [(None, false), (Some("en"), false), (Some("zh"), true)] {
        let dir = tempfile::tempdir().unwrap();
        let setup = success(command(dir.path(), language).arg("init").output().unwrap());
        assert!(setup.stdout.is_empty());
        assert_eq!(has_chinese(&setup.stderr), chinese);
        let theme = success(
            command(dir.path(), language)
                .args(["theme", "--preset", "pastel-powerline"])
                .output()
                .unwrap(),
        );
        assert_eq!(has_chinese(&theme.stderr), chinese);
        let config: serde_json::Value = serde_json::from_slice(&theme.stdout).unwrap();
        assert_eq!(
            config,
            serde_json::json!({"schema_version": 1, "preset": "pastel-powerline"})
        );
    }
}

#[test]
fn reload_notice_translates_and_keeps_the_loaded_adapter_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = Path::new(env!("CARGO_MANIFEST_DIR")).join("shell/zsh/tildy.zsh");
    for (language, lc_all, chinese) in [
        (None, None, false),
        (Some("en"), None, false),
        (Some("zh-CN"), None, true),
        (Some("auto"), None, true),
        (Some("auto"), Some("C"), false),
        (Some("fr"), None, false),
    ] {
        let mut command = Command::new("zsh");
        command.args(["-dfi", "-c", r#"
            typeset -g _tildy_loaded=1 _tildy_loaded_bin=/old/tildy _tildy_loaded_version=0.1.0 _tildy_loaded_protocol=1
            typeset -g TILDY_BIN=/new/tildy TILDY_VERSION=0.2.0 TILDY_PROTOCOL_VERSION=2
            source "$1"
            print -r -- "$TILDY_BIN|$TILDY_VERSION|$TILDY_PROTOCOL_VERSION"
        "#, "i18n-test"]).arg(&adapter)
            .env("HOME", dir.path()).env("LANG", "zh_CN.UTF-8")
            .env_remove("TILDY_LANG").env_remove("LC_ALL").env_remove("LC_MESSAGES");
        if let Some(language) = language {
            command.env("TILDY_LANG", language);
        }
        if let Some(lc_all) = lc_all {
            command.env("LC_ALL", lc_all);
        }
        let output = success(command.output().unwrap());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "/old/tildy|0.1.0|1"
        );
        let notice = String::from_utf8_lossy(&output.stderr);
        assert!(
            notice.contains("0.1.0") && notice.contains("0.2.0") && notice.contains("exec zsh"),
            "{notice}"
        );
        assert_eq!(has_chinese(&output.stderr), chinese, "{notice}");
    }
}
