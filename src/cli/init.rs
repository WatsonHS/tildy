//! `tildy init` sets up .zshrc; `tildy init zsh` prints an alias-safe loader
//! for shell/zsh/tildy.zsh with TILDY_BIN and version variables substituted.
//!
//! The output is consumed by `eval "$(tildy init zsh)"` inside the user's
//! `.zshrc`, where arbitrary aliases are already active. Aliases expand at
//! parse time, so a plain `alias kill='kill -9'` would rewrite the script's
//! function bodies. Every top-level word we emit is therefore quoted, and the
//! script itself is parsed inside an anonymous function with `no_aliases` set
//! locally. The script only uses `typeset -g` at top level, so running it in a
//! function scope is safe (see shell/zsh/README.md).
//!
//! `--instant` prints the instant-prompt snippet (shell/zsh/instant.zsh) for
//! the top of `.zshrc` in the same alias-safe wrapper, after the binary
//! version that keys its cache. See docs/decisions/0006-instant-prompt.md.
//!
//! The script itself is normally not inlined: it is written once per binary
//! build to `$XDG_CACHE_HOME/tildy/zsh/<version>-<hash>/tildy.zsh`, compiled
//! with `zcompile` (best effort, once), and the loader `source`s that file
//! inside the same alias-safe function. zsh then loads the word-code instead
//! of parsing 1700 lines at every shell start. Anything going wrong with the
//! cache (no HOME, unwritable directory, `TILDY_INIT_CACHE=0`) falls back to
//! the inline `eval` form, which is also what `--raw` documents.
//!
//! The header also carries `_tildy_agent_programs`, the agent programs the
//! script runs through `tildy agent wrap` (config `agent.programs`, empty
//! with `agent.wrap: false`; an unreadable config uses the built-in list).
//! The shell applies `TILDY_AGENT_WRAP=0` / `TILDY_AGENT_PROGRAMS` on top
//! when it loads (docs/agent-status.md).
use clap::Args;

mod setup;

#[derive(Args)]
pub struct InitArgs {
    /// Print a shell loader (zsh); omit to set up .zshrc automatically.
    pub shell: Option<String>,
    /// Print the raw script instead of the alias-safe loader (debugging aid).
    #[arg(long, requires = "shell")]
    pub raw: bool,
    /// Print the instant-prompt snippet for the top of .zshrc instead.
    #[arg(long, requires = "shell")]
    pub instant: bool,
    /// Preview the complete .zshrc without writing files.
    #[arg(long, conflicts_with = "shell")]
    pub dry_run: bool,
}

const ZSH_SCRIPT: &str = include_str!("../../shell/zsh/tildy.zsh");
const ZSH_INSTANT: &str = include_str!("../../shell/zsh/instant.zsh");

pub fn run(a: InitArgs) -> i32 {
    let Some(shell) = a.shell else {
        return setup::run(a.dry_run);
    };
    if shell != "zsh" {
        eprintln!("tildy init: unsupported shell '{shell}' (supports zsh only)");
        return 4;
    }
    if a.instant {
        print!("{}", instant_snippet(a.raw));
        return 0;
    }
    let bin = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "tildy".to_string());
    let header = format!(
        "'builtin' 'typeset' '-g' TILDY_BIN={}\n\
         'builtin' 'typeset' '-g' TILDY_VERSION={}\n\
         'builtin' 'typeset' '-g' TILDY_PROTOCOL_VERSION={}\n",
        shell_quote(&bin),
        shell_quote(tildy::model::BINARY_VERSION),
        tildy::model::PROTOCOL_VERSION
    );
    print!("{header}{}", agent_programs_lines());
    if a.raw {
        print!("{ZSH_SCRIPT}");
    } else if let Some(path) = cached_script() {
        // Same anonymous function as the inline form, but `source` of the
        // cached file: zsh uses the neighbouring .zwc when it is newer.
        print!(
            "() {{\n  'builtin' 'emulate' '-L' 'zsh' '-o' 'no_aliases'\n  'builtin' 'source' \"$1\"\n}} {}\n",
            shell_quote(&path.display().to_string())
        );
    } else {
        // Anonymous function: all words quoted so user aliases cannot touch
        // them; `emulate -L zsh -o no_aliases` is function-local, so the
        // user's own option state is untouched after loading.
        print!(
            "() {{\n  'builtin' 'emulate' '-L' 'zsh' '-o' 'no_aliases'\n  'builtin' 'eval' \"$1\"\n}} {}\n",
            shell_quote(ZSH_SCRIPT)
        );
    }
    0
}

/// `$XDG_CACHE_HOME/tildy/zsh/<version>-<hash>/tildy.zsh`, written if missing.
/// The hash is of the script bytes, so a rebuilt binary with a changed adapter
/// never reuses a stale file. `None` means: inline the script instead.
fn cached_script() -> Option<std::path::PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    if std::env::var_os("TILDY_INIT_CACHE").is_some_and(|v| v == "0") {
        return None;
    }
    let abs = |v: Option<std::ffi::OsString>| v.map(std::path::PathBuf::from).filter(|p| p.is_absolute());
    let root = abs(std::env::var_os("XDG_CACHE_HOME"))
        .or_else(|| abs(std::env::var_os("HOME")).map(|h| h.join(".cache")))?;
    let version: String = tildy::model::BINARY_VERSION
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' })
        .collect();
    let dir = root.join("tildy").join("zsh").join(format!("{version}-{:016x}", fnv1a(ZSH_SCRIPT.as_bytes())));
    let path = dir.join("tildy.zsh");
    if std::fs::read(&path).is_ok_and(|bytes| bytes == ZSH_SCRIPT.as_bytes()) {
        return Some(path);
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).ok()?;
    let tmp = dir.join(format!(".tildy.zsh.{}", std::process::id()));
    std::fs::write(&tmp, ZSH_SCRIPT).ok()?;
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    // Word-code next to it, once; zsh falls back to the text if this fails
    // or produces nothing. Other versions' directories are garbage.
    let _ = std::process::Command::new("zsh")
        .args(["-fc", "zcompile -- \"$1\"", "--"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Ok(entries) = std::fs::read_dir(dir.parent()?) {
        for e in entries.flatten() {
            if e.path() != dir {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    Some(path)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

/// The instant snippet: the version that keys the cache, then the snippet
/// itself (alias-safe unless `raw`).
fn instant_snippet(raw: bool) -> String {
    let mut out = format!(
        "'builtin' 'typeset' '-g' _tildy_instant_version={}\n",
        shell_quote(tildy::model::BINARY_VERSION)
    );
    if raw {
        out.push_str(ZSH_INSTANT);
        return out;
    }
    out.push_str(&format!(
        "() {{\n  'builtin' 'emulate' '-L' 'zsh' '-o' 'no_aliases'\n  'builtin' 'eval' \"$1\"\n}} {}\n",
        shell_quote(ZSH_INSTANT)
    ));
    out
}

/// `_tildy_agent_programs` from the effective config. Two statements: a
/// quoted `typeset` is not the reserved word, so it cannot take `=(...)`;
/// a plain assignment word is never alias-expanded.
fn agent_programs_lines() -> String {
    let programs = tildy::config::load(None).unwrap_or_else(|_| tildy::config::builtin()).agent_programs();
    let words: Vec<String> = programs.iter().map(|p| shell_quote(p)).collect();
    format!("'builtin' 'typeset' '-ga' _tildy_agent_programs\n_tildy_agent_programs=({})\n", words.join(" "))
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
