//! .zshrc setup is isolated from the developer's HOME and exercised through
//! the real CLI, including backups, symlinks and the unchanged eval interface.
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_tildy");

fn command(home: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.env("HOME", home)
        .env_remove("ZDOTDIR")
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("TILDY_CONFIG", home.join("config.json"));
    c
}

fn success(out: Output) -> Output {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    out
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().contains(".tildy-backup-"))
        .collect()
}

fn assert_loaders(text: &str) {
    assert!(text.starts_with("# >>> tildy instant >>>\n"), "{text}");
    assert!(text.ends_with("# <<< tildy prompt <<<\n"), "{text}");
    assert_eq!(text.matches(" init zsh --instant)").count(), 1, "{text}");
    assert_eq!(text.matches(" init zsh)").count(), 1, "{text}");
}

#[test]
fn creates_zshrc_and_second_run_does_not_write_or_backup() {
    let dir = tempfile::tempdir().unwrap();
    let out = success(command(dir.path()).arg("init").output().unwrap());
    assert!(out.stdout.is_empty(), "setup status must not be executable stdout");
    let rc = dir.path().join(".zshrc");
    let text = fs::read_to_string(&rc).unwrap();
    assert_loaders(&text);
    let modified = fs::metadata(&rc).unwrap().modified().unwrap();
    success(command(dir.path()).arg("init").output().unwrap());
    assert_eq!(fs::read_to_string(&rc).unwrap(), text);
    assert_eq!(fs::metadata(&rc).unwrap().modified().unwrap(), modified);
    assert!(backups(dir.path()).is_empty());
}

#[test]
fn preserves_user_content_permissions_and_exact_backup() {
    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join(".zshrc");
    let original = "# my aliases\r\nexport EDITOR=vim\r\nalias ll='ls -al'";
    fs::write(&rc, original).unwrap();
    fs::set_permissions(&rc, fs::Permissions::from_mode(0o640)).unwrap();
    success(command(dir.path()).arg("init").output().unwrap());
    let text = fs::read_to_string(&rc).unwrap();
    assert_loaders(&text);
    assert!(text.contains(original));
    assert_eq!(fs::metadata(&rc).unwrap().permissions().mode() & 0o777, 0o640);
    let saved = backups(dir.path());
    assert_eq!(saved.len(), 1);
    assert_eq!(fs::read_to_string(&saved[0]).unwrap(), original);
    assert_eq!(fs::metadata(&saved[0]).unwrap().permissions().mode() & 0o777, 0o600);
    success(command(dir.path()).arg("init").output().unwrap());
    assert_eq!(backups(dir.path()), saved);
}

#[test]
fn dry_run_creates_nothing_even_with_missing_zdotdir() {
    let dir = tempfile::tempdir().unwrap();
    let zdotdir = dir.path().join("missing/nested");
    let out = success(command(dir.path()).env("ZDOTDIR", &zdotdir).args(["init", "--dry-run"]).output().unwrap());
    assert_loaders(std::str::from_utf8(&out.stdout).unwrap());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

    fs::write(dir.path().join(".zshrc"), "export FOO=bar\n").unwrap();
    success(command(dir.path()).args(["init", "--dry-run"]).output().unwrap());
    assert_eq!(fs::read_to_string(dir.path().join(".zshrc")).unwrap(), "export FOO=bar\n");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn honors_zdotdir_and_preserves_relative_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let zdotdir = dir.path().join("zsh");
    fs::create_dir(&zdotdir).unwrap();
    let target = dir.path().join("dotfiles.zsh");
    fs::write(&target, "export TEST=1\n").unwrap();
    symlink("../dotfiles.zsh", zdotdir.join(".zshrc")).unwrap();
    fs::write(dir.path().join(".zshrc"), "# home file\n").unwrap();
    success(command(dir.path()).env("ZDOTDIR", &zdotdir).arg("init").output().unwrap());
    assert_eq!(fs::read_link(zdotdir.join(".zshrc")).unwrap(), Path::new("../dotfiles.zsh"));
    assert_loaders(&fs::read_to_string(&target).unwrap());
    assert_eq!(fs::read_to_string(dir.path().join(".zshrc")).unwrap(), "# home file\n");
    assert_eq!(fs::read_to_string(&backups(dir.path())[0]).unwrap(), "export TEST=1\n");
}

#[test]
fn reuses_documented_manual_init_without_duplicating_it() {
    let dir = tempfile::tempdir().unwrap();
    let original = "eval \"$(tildy init zsh --instant)\"\nexport EDITOR=vim\neval \"$(tildy init zsh)\"\n";
    fs::write(dir.path().join(".zshrc"), original).unwrap();
    success(command(dir.path()).arg("init").output().unwrap());
    assert_eq!(fs::read_to_string(dir.path().join(".zshrc")).unwrap(), original);
    assert!(backups(dir.path()).is_empty());

    // An existing normal loader needs only the missing instant loader.
    fs::write(dir.path().join(".zshrc"), "# my config\neval \"$(tildy init zsh)\"\n").unwrap();
    success(command(dir.path()).arg("init").output().unwrap());
    let text = fs::read_to_string(dir.path().join(".zshrc")).unwrap();
    assert!(text.starts_with("# >>> tildy instant >>>\n"));
    assert_eq!(text.matches(" init zsh)").count(), 1);
    assert!(text.ends_with("# my config\neval \"$(tildy init zsh)\"\n"));
}

#[test]
fn configure_recognizes_the_managed_installation() {
    let dir = tempfile::tempdir().unwrap();
    success(command(dir.path()).arg("init").output().unwrap());
    let out = success(
        command(dir.path())
            .env_remove("TILDY_BIN")
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .args(["theme", "--preset", "plain"])
            .output()
            .unwrap(),
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains("还没有启用"));
}

#[test]
fn incomplete_markers_invalid_utf8_and_nonfiles_are_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join(".zshrc");
    for original in [
        b"# >>> tildy instant >>>\nimportant content\n".as_slice(),
        b"# <<< tildy prompt <<<\n",
        b"# >>> tildy instant >>>\n# <<< tildy prompt <<<\n",
        b"# >>> tildy instant >>>\n# >>> tildy prompt >>>\n",
        b"# non UTF-8: \xff\n",
    ] {
        fs::write(&rc, original).unwrap();
        let out = command(dir.path()).arg("init").output().unwrap();
        assert_eq!(out.status.code(), Some(4));
        assert_eq!(fs::read(&rc).unwrap(), original);
        assert!(backups(dir.path()).is_empty());
    }
    fs::remove_file(&rc).unwrap();
    fs::create_dir(&rc).unwrap();
    assert_eq!(command(dir.path()).arg("init").output().unwrap().status.code(), Some(4));
    assert!(rc.is_dir());
}

#[test]
fn loader_modes_remain_read_only_and_invalid_modes_fail() {
    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join(".zshrc");
    fs::write(&rc, "# original\n").unwrap();
    for args in [vec!["init", "zsh"], vec!["init", "zsh", "--instant"], vec!["init", "zsh", "--raw"]] {
        let out = success(command(dir.path()).args(args).output().unwrap());
        assert!(!out.stdout.is_empty());
        assert_eq!(fs::read_to_string(&rc).unwrap(), "# original\n");
    }
    for args in [vec!["init", "--raw"], vec!["init", "--instant"], vec!["init", "zsh", "--dry-run"]] {
        assert_eq!(command(dir.path()).args(args).output().unwrap().status.code(), Some(2));
    }
    assert_eq!(command(dir.path()).args(["init", "bash"]).output().unwrap().status.code(), Some(4));
    assert!(backups(dir.path()).is_empty());
}

#[test]
fn stable_binary_path_with_shell_metacharacters_loads_before_path_setup() {
    let dir = tempfile::tempdir().unwrap();
    let bindir = dir.path().join("brew's $bin (test)");
    fs::create_dir(&bindir).unwrap();
    let link = bindir.join("tildy");
    symlink(BIN, &link).unwrap();
    success(command(dir.path()).env("PATH", &bindir).arg("init").output().unwrap());
    let rc = dir.path().join(".zshrc");
    let text = fs::read_to_string(&rc).unwrap();
    assert!(text.contains("brew'\\''s $bin (test)/tildy"), "{text}");
    // Real zsh parses the quoting and executes the saved absolute path with no
    // tildy on PATH. Non-interactive shell guards avoid starting a worker.
    let out = Command::new("/bin/zsh")
        .args(["-fc", "source \"$1\" || exit; print -r -- $TILDY_VERSION", "--"])
        .arg(&rc)
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env("TILDY_CONFIG", dir.path().join("config.json"))
        .output()
        .unwrap();
    let out = success(out);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), env!("CARGO_PKG_VERSION"));
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn concurrent_installers_add_one_pair_of_loaders_and_one_backup() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".zshrc"), "# original\n").unwrap();
    let spawn = || {
        command(dir.path())
            .arg("init")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn();
    let second = spawn();
    success(first.wait_with_output().unwrap());
    success(second.wait_with_output().unwrap());
    assert_loaders(&fs::read_to_string(dir.path().join(".zshrc")).unwrap());
    assert_eq!(backups(dir.path()).len(), 1);
}

#[test]
fn missing_or_relative_home_is_an_error_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let out = command(dir.path()).env_remove("HOME").current_dir(dir.path()).arg("init").output().unwrap();
    assert_eq!(out.status.code(), Some(4));
    let out = command(dir.path()).env("ZDOTDIR", "relative").current_dir(dir.path()).arg("init").output().unwrap();
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}
