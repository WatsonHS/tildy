//! Starship bridge provider (`type: "starship"`): runs `starship module <name>` and
//! converts its ANSI output to Zsh prompt text. Owner: M1b task D.
//!
//! Output handling: SGR sequences (`ESC [ … m`) become Zsh prompt escapes
//! (`%F{…}`, `%K{…}`, `%B`, …; the protocol layer strips raw ESC); every other control sequence (CSI, OSC, DCS, lone ESC
//! pairs) and stray control characters are dropped; `%` is doubled; the column
//! width is computed on the stripped text with `unicode-width`.

use super::{Provider, ProviderValue};
use crate::model::{Dependency, Freshness, ModuleValue, PromptRequest, ProviderSpec, Slot};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const TTL: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_millis(500);

pub struct StarshipProvider {
    id: String,
    module: String,
    bin: String,
    config: Option<std::path::PathBuf>,
    spec: ProviderSpec,
    detect_files: Vec<String>,
    detect_extensions: Vec<String>,
    detect_folders: Vec<String>,
}

impl StarshipProvider {
    /// `id` is the tildy module id (also the provider key); `module` is the
    /// Starship module name passed to `starship module`.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(id: String, module: String) -> Box<dyn Provider> {
        Box::new(Self::with_binary(id, module, "starship".into()))
    }

    /// Like `new` but with an explicit executable (tests, odd installs).
    pub fn with_binary(id: String, module: String, bin: String) -> Self {
        let key = super::intern(&id);
        let spec = ProviderSpec {
            key,
            deps: &[Dependency::Cwd, Dependency::Env],
            ttl: TTL,
            timeout: TIMEOUT,
        };
        StarshipProvider {
            id,
            module,
            bin,
            config: None,
            spec,
            detect_files: vec![],
            detect_extensions: vec![],
            detect_folders: vec![],
        }
    }

    /// Only spawn starship when the cwd's direct entries (at most 500) match one of
    /// the lists. All lists empty disables the check. Extensions are given without
    /// the dot (a leading dot is tolerated).
    pub fn with_detect(mut self, files: Vec<String>, extensions: Vec<String>, folders: Vec<String>) -> Self {
        self.detect_files = files;
        self.detect_extensions = extensions.into_iter().map(|e| e.trim_start_matches('.').to_string()).collect();
        self.detect_folders = folders;
        self
    }

    /// Kill `starship module` after this long (module `timeout_ms`, else the
    /// config's `command_timeout_ms`, else 500 ms).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.spec.timeout = timeout;
        self
    }

    pub fn with_config(mut self, config: Option<std::path::PathBuf>) -> Self {
        self.config = config;
        self
    }

    fn detect_enabled(&self) -> bool {
        !(self.detect_files.is_empty() && self.detect_extensions.is_empty() && self.detect_folders.is_empty())
    }

    fn detected(&self, cwd: &std::path::Path) -> bool {
        let Ok(rd) = std::fs::read_dir(cwd) else { return false };
        for e in rd.flatten().take(500) {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if self.detect_folders.contains(&name) {
                    return true;
                }
            } else if self.detect_files.contains(&name) {
                return true;
            }
            if !is_dir {
                if let Some((_, ext)) = name.rsplit_once('.') {
                    if self.detect_extensions.iter().any(|x| x == ext) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn error(&self, freshness: Freshness, msg: String) -> ProviderValue {
        ProviderValue::Module(
            self.id.clone(),
            Slot { freshness, age_ms: None, value: None, error: Some(msg) },
        )
    }
}

impl Provider for StarshipProvider {
    fn spec(&self) -> &ProviderSpec {
        &self.spec
    }

    fn watch_paths(&self, _req: &PromptRequest) -> Vec<std::path::PathBuf> {
        self.config.iter().cloned().collect()
    }

    fn fetch(&self, req: &PromptRequest) -> ProviderValue {
        if !req.cwd.is_dir() {
            return self.error(Freshness::Error, format!("cwd {} is not a directory", req.cwd.display()));
        }
        if self.detect_enabled() && !self.detected(&req.cwd) {
            return ProviderValue::Module(
                self.id.clone(),
                Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: None, error: None },
            );
        }
        if let Some(path) = &self.config {
            let readable = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK)
                .open(path).and_then(|file| file.metadata()).is_ok_and(|m| m.is_file());
            if !readable {
                return self.error(Freshness::Error, format!("Starship config {} is missing, unreadable or not a regular file", path.display()));
            }
        }
        let deadline = Instant::now() + self.spec.timeout;
        let mut cmd = Command::new(&self.bin);
        if let Some(path) = &self.config { cmd.env("STARSHIP_CONFIG", path); }
        cmd.arg("module")
            .arg(&self.module)
            .current_dir(&req.cwd)
            .env("PWD", &req.cwd)
            .env("STARSHIP_SHELL", "zsh")
            .envs(req.env_map().get("PATH").filter(|p| !p.is_empty()).map(|p| ("PATH", p.clone())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return self.error(Freshness::Error, "starship not found in PATH".into());
            }
            Err(e) => return self.error(Freshness::Error, format!("cannot run starship: {e}")),
        };
        let pid = child.id() as libc::pid_t;
        let mut so = child.stdout.take().unwrap();
        let mut se = child.stderr.take().unwrap();
        let t_out = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = so.read_to_end(&mut v);
            v
        });
        let t_err = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = se.read_to_end(&mut v);
            v
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if Instant::now() >= deadline => break None,
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(e) => {
                    kill_group(pid);
                    let _ = child.wait();
                    return self.error(Freshness::Error, format!("wait failed: {e}"));
                }
            }
        };
        let Some(status) = status else {
            kill_group(pid);
            let _ = child.wait();
            let _ = t_out.join();
            let _ = t_err.join();
            return self.error(Freshness::Timeout, format!("starship module {} timed out", self.module));
        };
        let stdout = t_out.join().unwrap_or_default();
        let stderr = t_err.join().unwrap_or_default();
        if !status.success() {
            let line = String::from_utf8_lossy(&stderr).lines().next().unwrap_or("").trim().to_string();
            return self.error(Freshness::Error, format!("starship exited with {status}: {line}"));
        }
        // With STARSHIP_SHELL=zsh Starship already writes `%` as `%%`; undo that so
        // `convert_ansi` is the only place that escapes.
        let conv = convert_ansi(&String::from_utf8_lossy(&stdout).replace("%%", "%"));
        if conv.text.trim().is_empty() {
            return ProviderValue::Module(
                self.id.clone(),
                Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: None, error: None },
            );
        }
        ProviderValue::Module(
            self.id.clone(),
            Slot::fresh(ModuleValue {
                text: conv.text,
                fields: Default::default(),
                prerendered: Some((conv.zsh, conv.cols)),
            }),
        )
    }
}

fn kill_group(pid: libc::pid_t) {
    // SAFETY: plain syscalls on a pid we spawned (group leader via process_group(0)).
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
        libc::kill(pid, libc::SIGKILL);
    }
}

/// Translate SGR parameters into Zsh prompt escapes (no raw ESC bytes).
/// Unsupported attributes (dim, italic, blink, inverse, …) are dropped.
fn sgr_to_zsh(params: &str) -> String {
    let codes: Vec<u32> = if params.is_empty() {
        vec![0]
    } else {
        params.split([';', ':']).map(|p| p.parse().unwrap_or(0)).collect()
    };
    let mut out = String::new();
    let mut i = 0;
    while i < codes.len() {
        let c = codes[i];
        i += 1;
        match c {
            0 => out.push_str("%f%k%b%u"),
            1 => out.push_str("%B"),
            22 => out.push_str("%b"),
            4 => out.push_str("%U"),
            24 => out.push_str("%u"),
            39 => out.push_str("%f"),
            49 => out.push_str("%k"),
            30..=37 => out.push_str(&format!("%F{{{}}}", c - 30)),
            90..=97 => out.push_str(&format!("%F{{{}}}", c - 90 + 8)),
            40..=47 => out.push_str(&format!("%K{{{}}}", c - 40)),
            100..=107 => out.push_str(&format!("%K{{{}}}", c - 100 + 8)),
            38 | 48 => {
                let tag = if c == 38 { 'F' } else { 'K' };
                match codes.get(i) {
                    Some(5) => {
                        if let Some(n) = codes.get(i + 1).filter(|n| **n <= 255) {
                            out.push_str(&format!("%{tag}{{{n}}}"));
                        }
                        i += 2;
                    }
                    Some(2) => {
                        if let (Some(r), Some(g), Some(b)) = (codes.get(i + 1), codes.get(i + 2), codes.get(i + 3)) {
                            if *r <= 255 && *g <= 255 && *b <= 255 {
                                out.push_str(&format!("%{tag}{{#{r:02x}{g:02x}{b:02x}}}"));
                            }
                        }
                        i += 4;
                    }
                    _ => i = codes.len(),
                }
            }
            _ => {}
        }
    }
    out
}

/// Result of converting Starship's ANSI output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Converted {
    /// Plain text: no escapes, no control characters, `%` untouched.
    pub text: String,
    /// Zsh prompt text: SGR as `%F{}`/`%K{}`/`%B`/… escapes, `%` doubled; no bytes below 0x20.
    pub zsh: String,
    /// Display columns of `text`.
    pub cols: usize,
}

/// Convert ANSI-colored text to Zsh prompt syntax. Never panics.
pub fn convert_ansi(raw: &str) -> Converted {
    let mut text = String::new();
    let mut zsh = String::new();
    let mut it = raw.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.peek().copied() {
                Some('[') => {
                    it.next();
                    let mut params = String::new();
                    let mut fin = None;
                    while let Some(&n) = it.peek() {
                        it.next();
                        match n {
                            '\x20'..='\x3f' => params.push(n),
                            '\x40'..='\x7e' => {
                                fin = Some(n);
                                break;
                            }
                            _ => break, // malformed: drop what we consumed
                        }
                    }
                    if fin == Some('m') && params.chars().all(|p| p.is_ascii_digit() || p == ';' || p == ':') {
                        zsh.push_str(&sgr_to_zsh(&params));
                    }
                }
                Some(']') | Some('P') | Some('_') | Some('^') | Some('X') => {
                    it.next();
                    // String sequence: ends with BEL or ST (ESC \).
                    while let Some(n) = it.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            if it.peek() == Some(&'\\') {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                Some(n) => {
                    it.next();
                    // nF escapes (e.g. charset select `ESC ( B`) carry intermediates then a final byte.
                    if ('\x20'..='\x2f').contains(&n) {
                        while let Some(&m) = it.peek() {
                            it.next();
                            if !('\x20'..='\x2f').contains(&m) {
                                break;
                            }
                        }
                    }
                }
                None => {}
            },
            '%' => {
                text.push('%');
                zsh.push_str("%%");
            }
            c if c.is_control() => {}
            c => {
                text.push(c);
                zsh.push(c);
            }
        }
    }
    let cols = UnicodeWidthStr::width(text.as_str());
    Converted { text, zsh, cols }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passthrough() {
        let c = convert_ansi("hello");
        assert_eq!((c.text.as_str(), c.zsh.as_str(), c.cols), ("hello", "hello", 5));
    }

    #[test]
    fn sgr_basic_and_reset() {
        let c = convert_ansi("\x1b[1;36mdir\x1b[0m ");
        assert_eq!(c.zsh, "%B%F{6}dir%f%k%b%u ");
        assert_eq!(c.text, "dir ");
        assert_eq!(c.cols, 4);
    }

    #[test]
    fn bare_reset() {
        assert_eq!(convert_ansi("a\x1b[mb").zsh, "a%f%k%b%ub");
    }

    #[test]
    fn bright_and_basic_colors() {
        assert_eq!(convert_ansi("\x1b[91;102mx").zsh, "%F{9}%K{10}x");
        assert_eq!(convert_ansi("\x1b[31;47mx").zsh, "%F{1}%K{7}x");
        assert_eq!(convert_ansi("\x1b[39;49mx").zsh, "%f%kx");
    }

    #[test]
    fn color_256() {
        assert_eq!(convert_ansi("\x1b[38;5;208;48;5;17mx").zsh, "%F{208}%K{17}x");
    }

    #[test]
    fn truecolor_fg_and_bg() {
        let c = convert_ansi("\x1b[48;2;51;101;138;38;2;255;255;255m x \x1b[0m");
        assert_eq!(c.zsh, "%K{#33658a}%F{#ffffff} x %f%k%b%u");
        assert_eq!(c.cols, 3);
        let c = convert_ansi("\x1b[38;2;1;2;3mA\x1b[48;2;4;5;6mB");
        assert_eq!(c.zsh, "%F{#010203}A%K{#040506}B");
        assert_eq!(c.text, "AB");
    }

    #[test]
    fn attributes_and_dropped_codes() {
        assert_eq!(convert_ansi("\x1b[1;4mx\x1b[22;24my").zsh, "%B%Ux%b%uy");
        // dim, italic, blink, inverse, strike are dropped
        assert_eq!(convert_ansi("\x1b[2;3;5;7;9mx").zsh, "x");
    }

    #[test]
    fn nested_styles_keep_order() {
        let c = convert_ansi("\x1b[1m\x1b[31mred\x1b[22mplain\x1b[0m");
        assert_eq!(c.zsh, "%B%F{1}red%bplain%f%k%b%u");
        assert_eq!(c.text, "redplain");
    }

    #[test]
    fn malformed_extended_color_is_dropped() {
        assert_eq!(convert_ansi("\x1b[38;2;1;2mx").zsh, "x");
        assert_eq!(convert_ansi("\x1b[38;5;999mx").zsh, "x");
    }

    #[test]
    fn output_has_no_control_bytes() {
        let c = convert_ansi("\x1b[1;38;2;1;2;3m\x1b]0;t\x07a\x1b[0m\n");
        assert!(c.zsh.bytes().all(|b| b >= 0x20), "{:?}", c.zsh);
    }

    #[test]
    fn percent_is_doubled_and_not_counted_twice() {
        let c = convert_ansi("\x1b[32m100%\x1b[0m");
        assert_eq!(c.zsh, "%F{2}100%%%f%k%b%u");
        assert_eq!(c.text, "100%");
        assert_eq!(c.cols, 4);
    }

    #[test]
    fn non_sgr_sequences_are_stripped() {
        // cursor move, erase line, OSC title (BEL and ST), charset select, stray controls
        let c = convert_ansi("a\x1b[2Kb\x1b[10;5Hc\x1b]0;title\x07d\x1b]8;;http://x\x1b\\e\x1b(Bf\r\n\x01g");
        assert_eq!(c.text, "abcdefg");
        assert_eq!(c.zsh, "abcdefg");
    }

    #[test]
    fn private_mode_csi_is_not_sgr() {
        assert_eq!(convert_ansi("\x1b[?25lx").zsh, "x");
    }

    #[test]
    fn truncated_escape_does_not_panic() {
        for s in ["\x1b", "\x1b[", "\x1b[3", "\x1b]0;abc", "x\x1b[38;2;"] {
            let _ = convert_ansi(s);
        }
    }

    #[test]
    fn width_uses_unicode_width() {
        let c = convert_ansi("\x1b[1m日本\x1b[0m♥");
        assert_eq!(c.cols, 5);
    }

    #[test]
    fn spec_matches_contract() {
        let p = StarshipProvider::new("elixir".into(), "elixir".into());
        let s = p.spec();
        assert_eq!(s.key, "elixir");
        assert_eq!(s.deps, &[Dependency::Cwd, Dependency::Env]);
        assert_eq!(s.ttl, Duration::from_secs(60));
        assert_eq!(s.timeout, Duration::from_millis(500));
    }
}
