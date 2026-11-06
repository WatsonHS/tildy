//! `tildy agent wrap` through the built binary, in real pseudo-terminals
//! (libc `openpty`, no extra crates). The binary gets the test pty as its
//! controlling terminal (setsid + TIOCSCTTY), like under a terminal emulator,
//! so the kernel delivers SIGWINCH and the raw-mode handling is the real one.
//! Every run uses its own XDG_STATE_HOME; records are read back through
//! `tildy::agent::Store`.
use tildy::agent::{Filter, Scope, State, Store, Stored};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_tildy");

struct Sandbox {
    _dir: tempfile::TempDir,
    state: PathBuf,
    cwd: PathBuf,
}

fn sandbox() -> Sandbox {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    Sandbox { state, cwd, _dir: dir }
}

impl Sandbox {
    fn records(&self) -> Vec<Stored> {
        let mut v = Store::open_with(&self.state).list(&Filter::default());
        v.sort_by(|a, b| a.record.id.cmp(&b.record.id));
        v
    }

    fn command(&self, prog: &str, args: &[&str]) -> Command {
        let mut c = Command::new(prog);
        c.args(args)
            .current_dir(&self.cwd)
            .env("XDG_STATE_HOME", &self.state)
            .env("TILDY_SESSION_ID", "test-session-1")
            .env("TERM", "xterm-256color")
            .env("LC_ALL", "en_US.UTF-8")
            .env_remove("TILDY_DEBUG_LOG")
            .env_remove("TILDY_DEBUG")
            .env_remove("TILDY_AGENT_WRAP");
        c
    }

    fn wrap(&self, args: &[&str]) -> Command {
        let mut all = vec!["agent", "wrap"];
        all.extend_from_slice(args);
        self.command(BIN, &all)
    }
}

struct Pty {
    child: Child,
    master: File,
    before: libc::termios,
    output: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
}

fn termios(fd: i32) -> libc::termios {
    // SAFETY: tcgetattr fills the zeroed struct we own.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0);
    t
}

/// The modes wrap must restore (PENDIN is the kernel's own bookkeeping).
fn modes(t: &libc::termios) -> [libc::tcflag_t; 4] {
    [t.c_iflag, t.c_oflag, t.c_cflag, t.c_lflag & !libc::PENDIN]
}

/// Run `cmd` with a fresh 40x110 pty as stdin/stdout/stderr and controlling terminal.
fn spawn(mut cmd: Command) -> Pty {
    let (mut m, mut s) = (-1, -1);
    let mut ws = libc::winsize { ws_row: 40, ws_col: 110, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty writes two descriptors into the provided ints.
    let r = unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::addr_of_mut!(ws)) };
    assert_eq!(r, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both descriptors were just opened and are owned here.
    let (master, slave) = unsafe { (File::from(OwnedFd::from_raw_fd(m)), File::from(OwnedFd::from_raw_fd(s))) };
    for fd in [m, s] {
        // SAFETY: fcntl on our descriptors (not inherited by unrelated children).
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let before = termios(s);
    cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().unwrap();
    drop(cmd);
    let output = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let mut reader = master.try_clone().unwrap();
    let (sink, stop2) = (output.clone(), stop.clone());
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        while !stop2.load(Ordering::SeqCst) {
            let mut p = libc::pollfd { fd: reader.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: polls one descriptor.
            if unsafe { libc::poll(&mut p, 1, 50) } <= 0 {
                continue;
            }
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
            }
        }
    });
    drop(slave);
    Pty { child, master, before, output, stop }
}

impl Pty {
    fn bytes(&self) -> Vec<u8> {
        self.output.lock().unwrap().clone()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }

    fn wait_for(&self, needle: &str, timeout: Duration) {
        self.wait_for_bytes(needle.as_bytes(), timeout);
    }

    fn wait_for_bytes(&self, needle: &[u8], timeout: Duration) {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.bytes().windows(needle.len()).any(|w| w == needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {:?}; output so far:\n{:?}", String::from_utf8_lossy(needle), self.text());
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    fn signal(&self, sig: i32) {
        // SAFETY: signals the wrap process we spawned.
        unsafe { libc::kill(self.child.id() as i32, sig) };
    }

    fn resize(&self, rows: u16, cols: u16) {
        let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: TIOCSWINSZ on our master; the kernel signals the foreground group.
        assert_eq!(unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) }, 0);
    }

    /// Wait for exit, let the reader drain, check the terminal was restored.
    fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if let Some(s) = self.child.try_wait().unwrap() {
                let mut last = usize::MAX;
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let n = self.output.lock().unwrap().len();
                    if n == last {
                        break;
                    }
                    last = n;
                }
                // The slave is revoked once its session leader (wrap) exits;
                // the master still reports the pair's attributes.
                let after = termios(self.master.as_raw_fd());
                assert_eq!(modes(&after), modes(&self.before), "terminal modes restored after exit\n{:?}", self.text());
                return s;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("did not exit within {timeout:?}; output:\n{:?}", self.text());
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn sh(script: &str) -> Vec<String> {
    vec!["--".into(), "/bin/sh".into(), "-c".into(), script.into()]
}

fn args(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

const T: Duration = Duration::from_secs(20);

/// A fake agent: ordinary output around 7501 reports, a report split across
/// two writes, a rejected report (control character in msg), unrelated OSC
/// and CSI sequences, 8-bit bytes and a large burst.
const FAKE_AGENT: &str = r#"
printf 'starting \342\234\223\n'
printf '\033]7501;state=working:progress=10:app=fake\007'
printf 'line one\n\033[1;31mred\033[0m\n'
printf '\033]7501;state=blocked:kind=permission:msg=QWxsb3cgQmFzaD8=\033\\'
printf '\033]8;;https://example.com\033\\link\033]8;;\033\\ \033]52;c;aGk=\007\n'
printf '\033]7501;id=step/1:state=working\007\033]7501;id=step/2:sta'
sleep 0.2
printf 'te=done:title=c3RlcCAy\033\\'
printf '\033]7501;state=error:msg=YQpi\007'
printf '\200\377\001bytes\n'
head -c 100000 /dev/zero | tr '\0' 'x'
printf '\n\033]7501;state=done:msg=YWxsIGRvbmU=\033\\bye\n'
"#;

#[test]
fn output_is_byte_identical_and_reports_become_records() {
    let sb = sandbox();
    let mut direct = spawn(sb.command("/bin/sh", &["-c", FAKE_AGENT]));
    assert!(direct.wait_exit(T).success());
    let want = direct.bytes();
    assert!(want.len() > 100_000, "{}", want.len());

    let mut w = spawn(sb.wrap(&args(&sh(FAKE_AGENT))));
    let pid = w.child.id();
    let status = w.wait_exit(T);
    assert!(status.success(), "{status:?}");
    let got = w.bytes();
    assert!(got == want, "wrapped output differs ({} vs {} bytes)\n{:?}", got.len(), want.len(), String::from_utf8_lossy(&got[..got.len().min(600)]));

    // Inner child pid: the records are keyed by the wrapped program's pid.
    let recs = sb.records();
    assert_eq!(recs.len(), 2, "{recs:#?}");
    let r = &recs[0].record;
    assert!(r.id.starts_with("wrap/sh/") && r.id != format!("wrap/sh/{pid}"), "{}", r.id);
    assert_eq!((r.state, r.msg.as_deref(), r.app.as_deref()), (State::Done, Some("all done"), Some("sh")));
    assert_eq!(r.scope, Scope::global());
    assert_eq!(r.origin_session.as_deref(), Some("test-session-1"));
    assert_eq!(r.source, tildy::agent::Source::Osc);
    // step/1 was working: dropped at exit; step/2 is done: kept. Its app is
    // inherited from the root record as it was then: the blocked report
    // replaced the root without an app, so the default ("sh") applied.
    let s2 = &recs[1].record;
    assert_eq!(s2.id, format!("{}/step/2", r.id));
    assert_eq!((s2.state, s2.title.as_deref(), s2.app.as_deref()), (State::Done, Some("step 2"), Some("sh")));
}

#[test]
fn start_record_shows_while_running_and_exit_synthesizes_the_outcome() {
    let sb = sandbox();
    let mut w = spawn(sb.wrap(&args(&sh("echo ready; read line; exit 7"))));
    w.wait_for("ready", T);
    wait_until(|| sb.records().len() == 1, "the start record");
    let recs = sb.records();
    assert_eq!((recs[0].record.state, recs[0].record.msg.as_deref()), (State::Working, None));
    w.send(b"go\r");
    assert_eq!(w.wait_exit(T).code(), Some(7));
    let recs = sb.records();
    assert_eq!(recs.len(), 1, "{recs:#?}");
    assert_eq!((recs[0].record.state, recs[0].record.msg.as_deref()), (State::Error, Some("exit 7")));

    let mut w = spawn(sb.wrap(&["--app", "my agent!", "--", "/usr/bin/true"]));
    assert_eq!(w.wait_exit(T).code(), Some(0));
    let done: Vec<_> = sb.records().into_iter().filter(|s| s.record.id.starts_with("wrap/my_agent_/")).collect();
    assert_eq!(done.len(), 1);
    assert_eq!((done[0].record.state, done[0].record.app.as_deref(), done[0].record.msg.as_deref()), (State::Done, Some("my_agent_"), None));
}

#[test]
fn exit_status_and_signal_passthrough() {
    let sb = sandbox();
    for (script, code, msg) in [("exit 0", 0, None), ("exit 42", 42, Some("exit 42")), ("kill -TERM $$", 143, Some("signal SIGTERM")), ("kill -KILL $$", 137, Some("signal SIGKILL"))] {
        let mut w = spawn(sb.wrap(&["--app", "t", "--", "/bin/sh", "-c", script]));
        let status = w.wait_exit(T);
        assert_eq!(status.code(), Some(code), "{script}: {status:?} {:?}", w.text());
        let recs: Vec<_> = sb.records().into_iter().filter(|s| s.record.app.as_deref() == Some("t")).collect();
        let last = recs.iter().max_by_key(|s| s.record.received_at_ms).unwrap();
        assert_eq!(last.record.msg.as_deref(), msg, "{script}");
        assert_eq!(last.record.state, if code == 0 { State::Done } else { State::Error });
    }
}

#[test]
fn int_term_hup_are_forwarded_to_the_child() {
    let sb = sandbox();
    for (sig, name) in [(libc::SIGINT, "INT"), (libc::SIGTERM, "TERM"), (libc::SIGHUP, "HUP")] {
        let script = format!("trap 'echo got-{name}; exit 9' {name}; echo ready; while :; do sleep 0.05; done");
        let mut w = spawn(sb.wrap(&args(&sh(&script))));
        w.wait_for("ready", T);
        std::thread::sleep(Duration::from_millis(100));
        w.signal(sig);
        let status = w.wait_exit(T);
        assert_eq!(status.code(), Some(9), "{name}: {status:?} {:?}", w.text());
        assert!(w.text().contains(&format!("got-{name}")), "{:?}", w.text());
    }
    // A child that does not trap the signal dies of it: 128+N.
    let mut w = spawn(sb.wrap(&args(&sh("echo ready; while :; do sleep 0.05; done"))));
    w.wait_for("ready", T);
    w.signal(libc::SIGTERM);
    assert_eq!(w.wait_exit(T).code(), Some(143));
}

#[test]
fn not_a_terminal_runs_the_command_directly() {
    let sb = sandbox();
    // Pipes everywhere: exec, so the child is the wrap process itself.
    let mut child = sb
        .wrap(&args(&sh("if [ -t 0 ]; then echo tty; else echo notty; fi; cat; echo \"pid=$$\"; exit 5")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.stdin.take().unwrap().write_all(b"hello\nworld").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(5));
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("notty\nhello\nworldpid={pid}\n"));
    assert_eq!(out.stderr, b"", "nothing but the child's bytes");
    assert!(sb.records().is_empty());

    // Terminal stdin but redirected stdout: also direct (the child must see the pipe).
    let (mut m, mut s) = (-1, -1);
    // SAFETY: openpty writes two descriptors.
    assert_eq!(unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }, 0);
    // SAFETY: just opened, owned here.
    let (_master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    let child = sb
        .wrap(&args(&sh("[ -t 0 ] && echo in-tty; [ -t 1 ] || echo out-pipe; echo \"pid=$$\"")))
        .stdin(Stdio::from(slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("in-tty\nout-pipe\npid={pid}\n"));
    assert_eq!(out.stderr, b"");
}

#[test]
fn unusable_store_or_opt_out_runs_the_command_directly() {
    let sb = sandbox();
    let file = sb.state.join("not-a-dir");
    std::fs::write(&file, "x").unwrap();
    let mut cmd = sb.wrap(&args(&sh("echo \"pid=$$\"; [ -t 1 ] && echo tty")));
    cmd.env("XDG_STATE_HOME", &file);
    let mut w = spawn(cmd);
    let pid = w.child.id();
    assert!(w.wait_exit(T).success());
    assert_eq!(w.text(), format!("pid={pid}\r\ntty\r\n"), "exec'd in place, nothing else printed");

    let mut cmd = sb.wrap(&args(&sh("echo \"pid=$$\"")));
    cmd.env("TILDY_AGENT_WRAP", "0");
    let mut w = spawn(cmd);
    let pid = w.child.id();
    assert!(w.wait_exit(T).success());
    assert_eq!(w.text(), format!("pid={pid}\r\n"));
    assert!(sb.records().is_empty());

    // Wrapped (for contrast): the child is a separate process.
    let mut w = spawn(sb.wrap(&args(&sh("echo \"pid=$$\""))));
    let pid = w.child.id();
    assert!(w.wait_exit(T).success());
    assert!(!w.text().contains(&format!("pid={pid}\r")), "{:?}", w.text());
}

#[test]
fn non_utf8_arguments_pass_through() {
    use std::os::unix::ffi::OsStrExt;
    let sb = sandbox();
    let mut cmd = sb.wrap(&["--", "/bin/echo"]);
    cmd.arg(std::ffi::OsStr::from_bytes(b"a\xffb"));
    let mut w = spawn(cmd);
    assert!(w.wait_exit(T).success());
    assert_eq!(w.bytes(), b"a\xffb\r\n");
}

#[test]
fn missing_command_is_reported_like_a_shell() {
    let sb = sandbox();
    let mut w = spawn(sb.wrap(&["--", "/definitely/not/a/command"]));
    assert_eq!(w.wait_exit(T).code(), Some(127));
    assert!(w.text().contains("command not found: /definitely/not/a/command"), "{:?}", w.text());
    assert!(sb.records().is_empty(), "no record for a program that never ran");
}

#[test]
fn interactive_cat_gets_stdin() {
    let sb = sandbox();
    let mut w = spawn(sb.wrap(&["--", "cat"]));
    std::thread::sleep(Duration::from_millis(300));
    w.send(b"hello wrap\r");
    // Echo from the inner pty's line discipline, then cat's own output.
    w.wait_for("hello wrap\r\nhello wrap\r\n", T);
    w.send(b"\x04");
    assert!(w.wait_exit(T).success(), "{:?}", w.text());
}

#[test]
fn typed_ctrl_c_and_ctrl_z_reach_the_child() {
    let sb = sandbox();
    // Ctrl-C is a byte for the inner pty, whose line discipline signals the child.
    let mut w = spawn(sb.wrap(&args(&sh("trap 'echo got-int; exit 4' INT; echo ready; while :; do sleep 0.05; done"))));
    w.wait_for("ready", T);
    std::thread::sleep(Duration::from_millis(100));
    w.send(b"\x03");
    assert_eq!(w.wait_exit(T).code(), Some(4), "{:?}", w.text());
    assert!(w.text().contains("got-int"));
    // Ctrl-Z stops the child; with no job-control shell above (the test makes
    // wrap a session leader, an orphaned process group) wrap cannot suspend
    // itself, so the child is continued and keeps working.
    let mut w = spawn(sb.wrap(&["--", "cat"]));
    std::thread::sleep(Duration::from_millis(300));
    w.send(b"one\r");
    w.wait_for("one\r\none\r\n", T);
    w.send(b"\x1a");
    std::thread::sleep(Duration::from_millis(300));
    w.send(b"two\r");
    w.wait_for("two\r\ntwo\r\n", T);
    w.send(b"\x04");
    assert!(w.wait_exit(T).success(), "{:?}", w.text());
}

#[test]
fn ctrl_z_suspends_the_job_in_the_users_shell_and_fg_resumes_it() {
    let sb = sandbox();
    let mut cmd = sb.command("/bin/zsh", &["-f"]);
    cmd.env("HOME", &sb.cwd);
    let mut z = spawn(cmd);
    std::thread::sleep(Duration::from_millis(300));
    z.send(b"PS1='P''> '\r");
    z.wait_for("P> ", T);
    z.send(format!("{BIN} agent wrap -- cat\r").as_bytes());
    std::thread::sleep(Duration::from_millis(500));
    z.send(b"abc\r");
    z.wait_for("abc\r\nabc\r\n", T);
    let mark = z.bytes().len();
    z.send(b"\x1a");
    let back_at_prompt = || {
        let b = z.bytes();
        let tail = String::from_utf8_lossy(&b[mark..]).into_owned();
        tail.contains("suspended") && tail.contains("P> ")
    };
    wait_until(back_at_prompt, "zsh reports the job suspended and prompts again");
    z.send(b"fg\r");
    std::thread::sleep(Duration::from_millis(500));
    z.send(b"def\r");
    z.wait_for("def\r\ndef\r\n", T);
    z.send(b"\x04");
    std::thread::sleep(Duration::from_millis(300));
    z.send(b"print rc=$?\r");
    z.wait_for("rc=0", T);
    z.send(b"exit\r");
    assert!(z.wait_exit(T).success(), "{:?}", z.text());
    let recs = sb.records();
    assert_eq!((recs.len(), recs[0].record.state), (1, State::Done), "{recs:#?}");
}

#[test]
fn interactive_zsh_runs_typed_commands() {
    let sb = sandbox();
    let mut cmd = sb.wrap(&["--", "/bin/zsh", "-f"]);
    cmd.env("HOME", &sb.cwd);
    let mut w = spawn(cmd);
    std::thread::sleep(Duration::from_millis(500));
    // Split quoting so the echoed command line does not contain the prompt.
    w.send(b"PS1='RE''ADY> '\r");
    w.wait_for("READY> ", T);
    w.send(b"print -r -- $(( 6 * 7 ))xyz; exit 3\r");
    w.wait_for("42xyz", T);
    assert_eq!(w.wait_exit(T).code(), Some(3), "{:?}", w.text());
    let recs = sb.records();
    assert_eq!((recs[0].record.state, recs[0].record.msg.as_deref()), (State::Error, Some("exit 3")));
}

#[test]
fn window_size_at_start_and_on_resize() {
    let sb = sandbox();
    let script = "stty size; trap 'stty size' WINCH; echo ready; while :; do sleep 0.05; done";
    let mut w = spawn(sb.wrap(&args(&sh(script))));
    w.wait_for("40 110\r\n", T);
    w.wait_for("ready", T);
    w.resize(50, 132);
    w.wait_for("50 132\r\n", T);
    w.resize(24, 80);
    w.wait_for("24 80\r\n", T);
    w.signal(libc::SIGTERM);
    assert_eq!(w.wait_exit(T).code(), Some(143));
}

#[test]
fn terminal_queries_and_replies_pass_both_ways_untouched() {
    let sb = sandbox();
    let queries: &[u8] = b"\x1b[c\x1b[>c\x1b[?u\x1b]7501;?\x1b\\\x1b]52;c;?\x07\x1b[?2026$p\x1b[?2004h\x1b[?1006;1000h\x1b[?1049h";
    let replies: &[u8] = b"\x1b[?62;22c\x1b[>1;10;0c\x1b[?0u\x1b]7501;?\x1b\\\x1b]52;c;aGk=\x07\x1b[?2026;2$y\
\x1b[200~pasted \xe4\xb8\xad\x1b[201~\x1b[<0;12;7M\x1b[<0;12;7m\x00\xff\x03\x1a";
    let script = format!(
        r#"stty raw -echo; printf '\033[c\033[>c\033[?u\033]7501;?\033\\\033]52;c;?\007\033[?2026$p\033[?2004h\033[?1006;1000h\033[?1049h'; r=$(dd bs=1 count={} 2>/dev/null | od -An -v -tx1 | tr -d ' \n'); printf '\033[?1049l'; stty sane; printf '\nGOT:%s\n' "$r""#,
        replies.len()
    );
    let mut w = spawn(sb.wrap(&args(&sh(&script))));
    w.wait_for_bytes(queries, T);
    w.send(replies);
    let hex: String = replies.iter().map(|b| format!("{b:02x}")).collect();
    w.wait_for(&format!("GOT:{hex}"), T);
    assert!(w.wait_exit(T).success(), "{:?}", w.text());
    assert!(sb.records().iter().all(|s| s.record.state == State::Done), "the 7501 query is not a report");
}

#[test]
fn shell_prompt_mark_drops_active_records() {
    let sb = sandbox();
    let dbg = sb.state.join("dbg.jsonl");
    let script = "printf '\\033]7501;id=job:state=working\\007\\033]7501;id=old:state=done\\007'; \
echo one; read a; printf '\\033]133;A\\007'; echo two; read b";
    let mut w = spawn(sb.wrap(&["--debug-reports", dbg.to_str().unwrap(), "--", "/bin/sh", "-c", script]));
    w.wait_for("one", T);
    let ids = |sb: &Sandbox| sb.records().into_iter().map(|s| s.record.id.rsplit('/').next().unwrap().to_string()).collect::<Vec<_>>();
    wait_until(|| ids(&sb).len() == 3, "three records");
    w.send(b"x\r");
    w.wait_for("two", T);
    wait_until(|| ids(&sb) == ["old"], "working records dropped at the prompt mark");
    w.send(b"y\r");
    assert!(w.wait_exit(T).success());
    let log = std::fs::read_to_string(&dbg).unwrap();
    assert!(log.contains(r#"{"event":"prompt"}"#), "{log}");
    assert_eq!(ids(&sb).len(), 2, "{:#?}", sb.records()); // root done (synthesized) + old
}

#[test]
fn records_go_to_the_worktree_scope_inside_a_repository() {
    let sb = sandbox();
    let ok = Command::new("git").args(["init", "-q"]).current_dir(&sb.cwd).status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        eprintln!("git not available; skipping");
        return;
    }
    let mut w = spawn(sb.wrap(&["--", "/usr/bin/true"]));
    assert!(w.wait_exit(T).success());
    let wt = tildy::providers::git::worktree_identity(&sb.cwd).unwrap();
    assert_eq!(sb.records()[0].record.scope, Scope::worktree(wt));

    let mut w = spawn(sb.wrap(&["--scope", "session", "--app", "s", "--", "/usr/bin/true"]));
    assert!(w.wait_exit(T).success());
    let mut w = spawn(sb.wrap(&["--scope", "global", "--app", "g", "--", "/usr/bin/true"]));
    assert!(w.wait_exit(T).success());
    let mut w = spawn(sb.wrap(&["--scope", "worktree", "--scope-id", "custom", "--app", "c", "--", "/usr/bin/true"]));
    assert!(w.wait_exit(T).success());
    let scope_of = |app: &str| sb.records().into_iter().find(|s| s.record.app.as_deref() == Some(app)).unwrap().record.scope;
    assert_eq!(scope_of("s"), Scope::session("test-session-1"));
    assert_eq!(scope_of("g"), Scope::global());
    assert_eq!(scope_of("c"), Scope::worktree("custom"));
}

#[test]
fn less_and_vim_do_not_hang() {
    let sb = sandbox();
    let file = sb.cwd.join("text.txt");
    std::fs::write(&file, (1..200).map(|i| format!("line {i}\n")).collect::<String>()).unwrap();
    let f = file.to_str().unwrap();
    if Path::new("/usr/bin/less").exists() {
        let mut w = spawn(sb.wrap(&["--", "/usr/bin/less", f]));
        w.wait_for("line 1", T);
        w.send(b" ");
        std::thread::sleep(Duration::from_millis(200));
        w.send(b"q");
        assert!(w.wait_exit(T).success(), "{:?}", w.text());
    }
    if Path::new("/usr/bin/vim").exists() {
        let mut cmd = sb.wrap(&["--", "/usr/bin/vim", "-u", "NONE", "-i", "NONE", "-N", "-n", f]);
        cmd.env("HOME", &sb.cwd);
        let mut w = spawn(cmd);
        w.wait_for("line 1", T);
        w.send(b"\x1b:q!\r");
        assert!(w.wait_exit(T).success(), "{:?}", w.text());
    }
}

fn wait_until(mut f: impl FnMut() -> bool, what: &str) {
    let end = Instant::now() + T;
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out: {what}");
}
