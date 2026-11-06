//! Minimal raw-mode terminal support for `tildy theme` (libc termios only,
//! no TUI crates): a guard that enters raw mode plus the alternate screen and
//! restores everything on drop, on panic and on SIGINT/SIGTERM/SIGHUP; a key
//! decoder that works on any `Read` (so the configure loop can be driven by an
//! injected byte stream in tests); and terminal size queries.
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Entered after raw mode: alternate screen, hidden cursor, no autowrap
/// (long preview lines are clipped at the right edge instead of wrapping).
const ENTER_SEQ: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?7l";
/// Undoes `ENTER_SEQ` (autowrap on, cursor visible, main screen).
const LEAVE_SEQ: &[u8] = b"\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l";

/// (input fd, output fd, original termios of the input fd). Set once per
/// process by the first `RawMode::enter`; read by the panic hook and the
/// signal handler, so it must not need a lock.
static SAVED: OnceLock<(i32, i32, libc::termios)> = OnceLock::new();
/// True while the terminal is in raw mode; `restore` is a no-op otherwise.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOKS: std::sync::Once = std::sync::Once::new();

pub fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

/// (columns, rows) of the terminal on `fd`, or None when it is not a terminal.
pub fn size(fd: i32) -> Option<(usize, usize)> {
    // SAFETY: TIOCGWINSZ writes a winsize into the struct we own.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0;
    (ok && ws.ws_col > 0 && ws.ws_row > 0).then_some((ws.ws_col as usize, ws.ws_row as usize))
}

fn write_all_fd(fd: i32, mut b: &[u8]) {
    while !b.is_empty() {
        // SAFETY: writes from a valid slice; async-signal-safe.
        let n = unsafe { libc::write(fd, b.as_ptr().cast(), b.len()) };
        if n <= 0 {
            return;
        }
        b = &b[n as usize..];
    }
}

/// Restore the saved termios and leave the alternate screen. Idempotent and
/// async-signal-safe (atomics, tcsetattr, write).
fn restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    if let Some((fd_in, fd_out, orig)) = SAVED.get() {
        // SAFETY: restores the attributes saved from the same descriptor.
        unsafe { libc::tcsetattr(*fd_in, libc::TCSANOW, orig) };
        write_all_fd(*fd_out, LEAVE_SEQ);
    }
}

extern "C" fn on_signal(sig: libc::c_int) {
    restore();
    // SAFETY: reset to the default action and re-raise so the exit status
    // reports the signal, as if we had not caught it.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn install_hooks() {
    HOOKS.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Restore first so the panic message lands on the normal screen.
            restore();
            prev(info);
        }));
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: installs a handler that only calls async-signal-safe code.
            unsafe { libc::signal(sig, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t) };
        }
    });
}

/// Raw mode + alternate screen for as long as the guard lives.
pub struct RawMode {
    _private: (),
}

impl RawMode {
    /// Put `fd_in` in raw mode (no echo, no line buffering, no signals from
    /// Ctrl-C, no CR/LF translation on input) and switch `fd_out` to the
    /// alternate screen. Output processing stays on, so `\n` still returns the
    /// carriage.
    pub fn enter(fd_in: i32, fd_out: i32) -> io::Result<RawMode> {
        // SAFETY: tcgetattr fills the zeroed struct we own.
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd_in, &mut orig) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let saved = SAVED.get_or_init(|| (fd_in, fd_out, orig));
        if saved.0 != fd_in || saved.1 != fd_out {
            return Err(io::Error::other("raw mode already used on other descriptors"));
        }
        install_hooks();
        let mut raw = saved.2;
        raw.c_iflag &= !(libc::ICRNL | libc::INLCR | libc::IGNCR | libc::IXON | libc::ISTRIP | libc::BRKINT | libc::INPCK);
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL | libc::ISIG | libc::IEXTEN);
        raw.c_cflag |= libc::CS8;
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        ACTIVE.store(true, Ordering::SeqCst);
        // SAFETY: applies attributes derived from the ones just read.
        if unsafe { libc::tcsetattr(fd_in, libc::TCSANOW, &raw) } != 0 {
            ACTIVE.store(false, Ordering::SeqCst);
            return Err(io::Error::last_os_error());
        }
        write_all_fd(fd_out, ENTER_SEQ);
        Ok(RawMode { _private: () })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore();
    }
}

/// Terminal input with a read timeout: `read` waits at most `timeout_ms` and
/// then fails with `ErrorKind::TimedOut`, which `KeyReader` reports as a tick.
pub struct TtyIn {
    pub fd: i32,
    pub timeout_ms: i32,
}

impl Read for TtyIn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut p = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: polls one descriptor we own the struct for.
        let r = unsafe { libc::poll(&mut p, 1, self.timeout_ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Err(io::ErrorKind::TimedOut.into()) } else { Err(e) };
        }
        if r == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        // SAFETY: reads into the caller's buffer.
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Tab,
    BackTab,
    Enter,
    Space,
    Esc,
    CtrlC,
    Char(char),
    /// Anything else (function keys, Alt combinations, non-ASCII text).
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    /// No input within the reader's timeout (only from `TtyIn`).
    Tick,
    /// Input closed.
    Eof,
}

/// Decodes keys from a byte stream: ASCII, Ctrl-C, CR/LF, Tab, CSI/SS3
/// arrow sequences (`ESC [ A`, `ESC O A`, with optional parameters) and
/// Shift-Tab (`ESC [ Z`). A lone ESC is the Esc key when no further byte
/// arrives (timeout or end of input).
pub struct KeyReader<R: Read> {
    inner: R,
    buf: Vec<u8>,
    eof: bool,
}

impl<R: Read> KeyReader<R> {
    pub fn new(inner: R) -> Self {
        KeyReader { inner, buf: Vec::new(), eof: false }
    }

    /// Read more bytes. Ok(false) on timeout or end of input.
    fn fill(&mut self) -> io::Result<bool> {
        if self.eof {
            return Ok(false);
        }
        let mut b = [0u8; 256];
        loop {
            match self.inner.read(&mut b) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(false);
                }
                Ok(n) => {
                    self.buf.extend_from_slice(&b[..n]);
                    return Ok(true);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) => return Err(e),
            }
        }
    }

    fn take(&mut self, n: usize, k: Key) -> Event {
        self.buf.drain(..n);
        Event::Key(k)
    }

    pub fn next_event(&mut self) -> io::Result<Event> {
        if self.buf.is_empty() && !self.fill()? {
            return Ok(if self.eof { Event::Eof } else { Event::Tick });
        }
        let b0 = self.buf[0];
        let ev = match b0 {
            0x1b => return self.escape(),
            0x03 => self.take(1, Key::CtrlC),
            b'\r' | b'\n' => self.take(1, Key::Enter),
            b'\t' => self.take(1, Key::Tab),
            b' ' => self.take(1, Key::Space),
            0x21..=0x7e => self.take(1, Key::Char(b0 as char)),
            0x80..=0xff => {
                // A UTF-8 sequence: consume it whole.
                let len = match b0 {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => 1,
                };
                while self.buf.len() < len && self.fill()? {}
                self.take(len.min(self.buf.len()), Key::Other)
            }
            _ => self.take(1, Key::Other),
        };
        Ok(ev)
    }

    fn escape(&mut self) -> io::Result<Event> {
        if self.buf.len() < 2 && !self.fill()? {
            return Ok(self.take(1, Key::Esc));
        }
        if self.buf[1] != b'[' && self.buf[1] != b'O' {
            // Alt+key or a lone Esc followed by typing: report Esc only.
            return Ok(self.take(1, Key::Esc));
        }
        // CSI / SS3: parameters 0x30-0x3f, intermediates 0x20-0x2f, final 0x40-0x7e.
        let mut i = 2;
        loop {
            while i >= self.buf.len() {
                if !self.fill()? {
                    let n = self.buf.len();
                    return Ok(self.take(n, Key::Other));
                }
            }
            let c = self.buf[i];
            if (0x40..=0x7e).contains(&c) {
                let k = match c {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'Z' => Key::BackTab,
                    _ => Key::Other,
                };
                return Ok(self.take(i + 1, k));
            }
            if !(0x20..=0x3f).contains(&c) {
                return Ok(self.take(i, Key::Other));
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Event> {
        let mut r = KeyReader::new(bytes);
        let mut out = vec![];
        loop {
            let e = r.next_event().unwrap();
            out.push(e);
            if e == Event::Eof {
                return out;
            }
        }
    }

    #[test]
    fn decodes_arrows_and_controls() {
        use Key::*;
        let k = |k| Event::Key(k);
        assert_eq!(
            keys(b"\x1b[A\x1b[B\x1bOC\x1b[1;2D\x1b[Z\t \r\nq\x03"),
            vec![k(Up), k(Down), k(Right), k(Left), k(BackTab), k(Tab), k(Space), k(Enter), k(Enter), k(Char('q')), k(CtrlC), Event::Eof]
        );
        assert_eq!(keys(b"\x1b"), vec![k(Esc), Event::Eof]);
        assert_eq!(keys(b"\x1bq"), vec![k(Esc), k(Char('q')), Event::Eof]);
        assert_eq!(keys("é?".as_bytes()), vec![k(Other), k(Char('?')), Event::Eof]);
        assert_eq!(keys(b"\x1b[15~j"), vec![k(Other), k(Char('j')), Event::Eof]);
    }

    struct Chunks(Vec<Vec<u8>>);
    impl Read for Chunks {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let c = self.0.remove(0);
            if c.is_empty() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            buf[..c.len()].copy_from_slice(&c);
            Ok(c.len())
        }
    }

    #[test]
    fn timeouts_are_ticks_and_split_sequences_join() {
        let mut r = KeyReader::new(Chunks(vec![vec![], b"\x1b".to_vec(), b"[B".to_vec(), b"\x1b".to_vec(), vec![], b"k".to_vec()]));
        assert_eq!(r.next_event().unwrap(), Event::Tick);
        assert_eq!(r.next_event().unwrap(), Event::Key(Key::Down));
        // ESC then a timeout: the Esc key.
        assert_eq!(r.next_event().unwrap(), Event::Key(Key::Esc));
        assert_eq!(r.next_event().unwrap(), Event::Key(Key::Char('k')));
        assert_eq!(r.next_event().unwrap(), Event::Eof);
    }
}
