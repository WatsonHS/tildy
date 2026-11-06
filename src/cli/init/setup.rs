//! Explicit, idempotent .zshrc installation. Script-emitting `init zsh` never
//! enters this module, including when called at every shell startup.
use super::super::i18n::tr_eprintln;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

const INSTANT_START: &str = "# >>> tildy instant >>>";
const INSTANT_END: &str = "# <<< tildy instant <<<";
const PROMPT_START: &str = "# >>> tildy prompt >>>";
const PROMPT_END: &str = "# <<< tildy prompt <<<";

pub(super) fn run(dry_run: bool) -> i32 {
    match install(dry_run) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("tildy init: {e:#}");
            tildy::model::ErrorCode::EnvironmentUnavailable.exit_code()
        }
    }
}

fn zshrc_path() -> Result<PathBuf> {
    let dir = std::env::var_os("ZDOTDIR")
        .or_else(|| std::env::var_os("HOME"))
        .context("set ZDOTDIR or HOME to the directory containing .zshrc")?;
    let dir = PathBuf::from(dir);
    if !dir.is_absolute() {
        bail!("ZDOTDIR or HOME must be an absolute path");
    }
    Ok(dir.join(".zshrc"))
}

/// Keep a stable PATH symlink (not a versioned Homebrew Cellar path), while
/// using an absolute path so the instant prompt works before PATH setup.
fn binary_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate the tildy executable")?;
    let canonical = fs::canonicalize(&exe)?;
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths).filter(|p| p.is_absolute()) {
            let candidate = dir.join("tildy");
            if fs::canonicalize(&candidate).is_ok_and(|p| p == canonical) {
                return Ok(candidate);
            }
        }
    }
    Ok(exe)
}

fn rewrite(original: &str, bin: &Path) -> Result<String> {
    let bin = bin.to_str().context("the tildy executable path must be UTF-8")?;
    let mut body = String::new();
    let mut end = None;
    // Only our delimited blocks are replaced. All other user content, including
    // manual initialization and other prompt configurations, stays untouched.
    for line in original.split_inclusive('\n') {
        let text = line.trim_end_matches(['\r', '\n']);
        match text {
            INSTANT_START | PROMPT_START => {
                if end.is_some() {
                    bail!("nested tildy blocks in .zshrc; repair their markers before running init");
                }
                end = Some(if text == INSTANT_START { INSTANT_END } else { PROMPT_END });
            }
            INSTANT_END | PROMPT_END => {
                if end != Some(text) {
                    bail!("unmatched tildy block marker in .zshrc; repair it before running init");
                }
                end = None;
            }
            _ if end.is_none() => body.push_str(line),
            _ => {}
        }
    }
    if end.is_some() {
        bail!("unterminated tildy block in .zshrc; repair its markers before running init");
    }
    let quoted = super::shell_quote(bin);
    let has_manual = |instant: bool| {
        let suffix = if instant { " --instant" } else { "" };
        // Reuse the documented manual lines, including this executable's path.
        let commands = ["tildy", "'tildy'", bin, quoted.as_str()];
        body.lines()
            .any(|line| commands.iter().any(|command| line.trim() == format!("eval \"$({command} init zsh{suffix})\"")))
    };
    let mut out = String::new();
    if !has_manual(true) {
        out.push_str(&format!("{INSTANT_START}\neval \"$({quoted} init zsh --instant)\"\n{INSTANT_END}\n"));
    }
    out.push_str(&body);
    if !has_manual(false) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{PROMPT_START}\neval \"$({quoted} init zsh)\"\n{PROMPT_END}\n"));
    }
    Ok(out)
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => bail!("{} is not a regular file", path.display()),
        Ok(_) => fs::read_to_string(path).map(Some).with_context(|| format!("cannot read {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

fn sibling(path: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = path.file_name().context(".zshrc target has no file name")?.to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

fn lock(target: &Path) -> Result<File> {
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(sibling(target, ".tildy.lock")?)?;
    // SAFETY: the descriptor is owned here; closing it releases the lock.
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(lock)
}

fn install(dry_run: bool) -> Result<()> {
    let path = zshrc_path()?;
    let target = tildy::config::transaction::resolve_target(&path);
    let bin = binary_path()?;
    // Read-only previews create neither directories, lock files nor backups.
    let _lock = if dry_run {
        None
    } else {
        fs::create_dir_all(target.parent().context(".zshrc has no parent directory")?)?;
        Some(lock(&target).with_context(|| format!("cannot lock {}", target.display()))?)
    };
    let original = read(&target)?;
    let updated = rewrite(original.as_deref().unwrap_or_default(), &bin)?;
    if dry_run {
        print!("{updated}");
        tr_eprintln!("Preview of {}; no files written.", "预览 {}；未写入文件。", path.display());
        return Ok(());
    }
    if original.as_deref() == Some(&updated) {
        tr_eprintln!("Tildy is already enabled in {}; no changes needed.", "{} 已启用 tildy，无需修改。", path.display());
        return Ok(());
    }

    let id = format!("{}-{}", tildy::diagnostics::now_ms(), std::process::id());
    let backup = sibling(&target, &format!(".tildy-backup-{id}"))?;
    if let Some(old) = &original {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&backup)?;
        file.write_all(old.as_bytes())?;
        file.sync_all()?;
        tr_eprintln!("Original file backed up: {}", "原文件已备份：{}", backup.display());
    }
    let staged = sibling(&target, &format!(".tildy-tmp-{id}"))?;
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&staged)?;
    let saved = (|| -> Result<()> {
        file.write_all(updated.as_bytes())?;
        if original.is_some() {
            file.set_permissions(fs::metadata(&target)?.permissions())?;
        }
        file.sync_all()?;
        // Detect external edits since the read. The lock serializes tildy
        // installers; ordinary editors need not participate in that lock.
        if read(&target)? != original || tildy::config::transaction::resolve_target(&path) != target {
            bail!(".zshrc changed during setup; rerun tildy init against the new contents");
        }
        fs::rename(&staged, &target)?;
        Ok(())
    })();
    if saved.is_err() {
        let _ = fs::remove_file(&staged);
    }
    saved.with_context(|| format!("cannot update {}", path.display()))?;
    tr_eprintln!("Configured {}. Open a new terminal to use it.", "已配置 {}。新开终端即可生效。", path.display());
    Ok(())
}
