//! `tildy theme` in a real pseudo-terminal (libc `openpty`, no extra
//! crates): wait for the first screen, type Down, Space, Enter and check the
//! written file, that the terminal was restored (alternate screen left,
//! cursor shown, autowrap back on), and that q / Ctrl-C write nothing.
use serde_json::{json, Value};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Pty {
    child: Child,
    writer: File,
    output: Arc<Mutex<Vec<u8>>>,
}

fn spawn(dir: &Path, args: &[&str]) -> Pty {
    spawn_sized(dir, args, 110, 40)
}

fn spawn_sized(dir: &Path, args: &[&str], cols: u16, rows: u16) -> Pty {
    spawn_sized_language(dir, args, cols, rows, "zh")
}

fn spawn_sized_language(dir: &Path, args: &[&str], cols: u16, rows: u16, language: &str) -> Pty {
    let (mut master, mut slave) = (-1, -1);
    let mut ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty writes two descriptors into the provided ints.
    let r = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::addr_of_mut!(ws)) };
    assert_eq!(r, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both descriptors were just opened and are owned here.
    let (master, slave) = unsafe { (File::from(OwnedFd::from_raw_fd(master)), File::from(OwnedFd::from_raw_fd(slave))) };
    let child = Command::new(env!("CARGO_BIN_EXE_tildy"))
        .args(args)
        .current_dir(dir)
        .env("TILDY_CONFIG", dir.join("default-config.json"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("TERM", "xterm-256color")
        .env_remove("TILDY_SYMBOLS")
        .env("TILDY_LANG", language)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave))
        .spawn()
        .unwrap();
    // The Command (and our copies of the slave) are gone: the master sees EOF/EIO when the child exits.
    let output = Arc::new(Mutex::new(Vec::new()));
    let writer = master.try_clone().unwrap();
    let sink = output.clone();
    std::thread::spawn(move || {
        let mut master = master;
        let mut buf = [0u8; 4096];
        while let Ok(n) = master.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    Pty { child, writer, output }
}

impl Pty {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn wait_for(&self, needle: &str, timeout: Duration) {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.text().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {needle:?}; output so far:\n{}", self.text());
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        std::thread::sleep(Duration::from_millis(150));
    }

    fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if let Some(s) = self.child.try_wait().unwrap() {
                // Let the reader drain what the child wrote last.
                std::thread::sleep(Duration::from_millis(100));
                return s;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        panic!("child did not exit; output:\n{}", self.text());
    }
}

#[test]
fn fresh_theme_starts_with_pastel_and_space_selects_unicode_icons() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("config.json");
    let mut pty = spawn(dir.path(), &["theme", "--output", out.to_str().unwrap()]);
    pty.wait_for("tildy theme", Duration::from_secs(15));
    pty.wait_for("plain-text-symbols", Duration::from_secs(5)); // the whole first screen is drawn
    pty.send(b" "); // Space on the first toggle: icons nerd -> unicode
    pty.send(b"\r"); // Enter: write
    let status = pty.wait_exit(Duration::from_secs(15));
    let screen = pty.text();
    assert!(status.success(), "{status:?}\n{screen}");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(v, json!({"schema_version": 1, "preset": "pastel-powerline", "symbols": "unicode"}));
    // Entered and left the alternate screen; cursor and autowrap restored; the
    // JSON is printed on the normal screen after leaving.
    let enter = screen.find("\x1b[?1049h").expect("alternate screen entered");
    let leave = screen.rfind("\x1b[?1049l").expect("alternate screen left");
    assert!(enter < leave);
    assert!(screen[leave..].contains("\"preset\": \"pastel-powerline\""), "{:?}", &screen[leave..]);
    assert!(screen.contains("\x1b[?25h") && screen.contains("\x1b[?7h"));
    assert!(screen.contains("图标：unicode*"), "the toggle row showed the change");
}

#[test]
fn newline_toggle_saves_restores_and_cancels_in_an_80_column_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("default-config.json");
    std::fs::write(&out, r#"{"schema_version":1,"preset":"pastel-powerline","add_newline":true}"#).unwrap();
    let mut pty = spawn_sized(dir.path(), &["theme"], 80, 24);
    pty.wait_for("提示符前空行：开", Duration::from_secs(15));
    pty.wait_for("耗时与任务", Duration::from_secs(5));
    pty.send(b"\tll "); // Focus controls, move to blank line, turn it off.
    pty.wait_for("提示符前空行：关", Duration::from_secs(5));
    pty.send(b"j\r"); // Change preset, save; the blank-line choice carries over.
    assert!(pty.wait_exit(Duration::from_secs(15)).success());
    let saved = std::fs::read(&out).unwrap();
    let v: Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(v["add_newline"], false);
    assert_ne!(v["preset"], "pastel-powerline");

    let mut pty = spawn_sized(dir.path(), &["theme"], 80, 24);
    pty.wait_for("提示符前空行：关", Duration::from_secs(15));
    pty.send(b"\tll ");
    pty.wait_for("提示符前空行：开", Duration::from_secs(5));
    pty.send(b"q");
    assert!(pty.wait_exit(Duration::from_secs(15)).success());
    assert_eq!(std::fs::read(&out).unwrap(), saved, "cancel keeps the saved preference");
}

#[test]
fn interactive_newline_flag_preserves_existing_layout() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("default-config.json");
    let setup = Command::new(env!("CARGO_BIN_EXE_tildy"))
        .args(["theme", "--preset", "tokyo-night", "--one-line", "--no-time", "--add-newline"])
        .env("TILDY_CONFIG", &out)
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .current_dir(dir.path())
        .output().unwrap();
    assert!(setup.status.success());
    let mut expected: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    expected["add_newline"] = json!(false);
    let mut pty = spawn_sized(dir.path(), &["theme", "--no-add-newline"], 80, 24);
    pty.wait_for("提示符前空行：关", Duration::from_secs(15));
    pty.send(b"\r");
    assert!(pty.wait_exit(Duration::from_secs(15)).success());
    let written: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(written, expected);
}

#[test]
fn q_and_ctrl_c_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("config.json");
    for (key, code) in [(&b"q"[..], 0), (&b"\x03"[..], 130)] {
        let mut pty = spawn(dir.path(), &["theme", "--output", out.to_str().unwrap()]);
        pty.wait_for("plain-text-symbols", Duration::from_secs(15));
        pty.send(b"j");
        pty.send(key);
        let status = pty.wait_exit(Duration::from_secs(15));
        assert_eq!(status.code(), Some(code), "{:?}", pty.text());
        assert!(!out.exists());
        assert!(pty.text().contains("\x1b[?1049l"), "terminal restored");
    }
}

#[test]
fn english_theme_at_80_columns_saves_default_pastel_with_blank_line_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("config.json");
    let mut pty = spawn_sized_language(dir.path(), &["theme", "--output", out.to_str().unwrap()], 80, 24, "en");
    pty.wait_for("Blank line: On", Duration::from_secs(15));
    pty.wait_for("Duration/jobs", Duration::from_secs(5));
    pty.send(b"\t\x1b[C\x1b[C ");
    pty.wait_for("Blank line: Off", Duration::from_secs(5));
    pty.send(b"\r");
    let status = pty.wait_exit(Duration::from_secs(15));
    assert!(status.success(), "{}", pty.text());
    let written: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(written["preset"], "pastel-powerline");
    assert_eq!(written["add_newline"], false);
    let config = tildy::config::load(Some(&out)).unwrap();
    assert_eq!(config.file.layout.rows.len(), 2);
}
