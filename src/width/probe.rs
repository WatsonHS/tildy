//! Explicit foreground CPR measurements. Never used by prompt renderers.
use super::{Entry, MAX_COLUMNS};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

static CANCELLED: AtomicI32 = AtomicI32::new(0);
extern "C" fn cancelled(signal: i32) {
    CANCELLED.store(signal, Ordering::Relaxed);
}

struct Signals(Vec<(i32, libc::sigaction)>);
impl Signals {
    fn install() -> Result<Self, String> {
        CANCELLED.store(0, Ordering::Relaxed);
        let mut saved = Self(Vec::new());
        for signal in [
            libc::SIGINT,
            libc::SIGTERM,
            libc::SIGHUP,
            libc::SIGQUIT,
            libc::SIGTSTP,
        ] {
            // SAFETY: both actions are initialized, owned structures and the
            // handler only performs a lock-free atomic store.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                let mut old: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = cancelled as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, &mut old) != 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                saved.0.push((signal, old));
            }
        }
        Ok(saved)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for (signal, old) in &self.0 {
            // SAFETY: restoring actions obtained from sigaction above.
            unsafe {
                libc::sigaction(*signal, old, std::ptr::null_mut());
            }
        }
    }
}

struct Terminal {
    tty: File,
    saved: libc::termios,
    _signals: Signals,
    active: bool,
    alternate: bool,
}
impl Terminal {
    fn open(deadline: Instant) -> Result<Self, String> {
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/tty")
            .map_err(|e| format!("a foreground controlling terminal is required: {e}"))?;
        let fd = tty.as_raw_fd();
        // SAFETY: descriptors and output structures are valid.
        unsafe {
            if libc::tcgetpgrp(fd) != libc::getpgrp() {
                return Err(
                    "calibration must run in the terminal's foreground process group".into(),
                );
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let mut size: libc::winsize = std::mem::zeroed();
            if libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) != 0
                || size.ws_row < 4
                || size.ws_col < 16
            {
                return Err(
                    "calibration requires a terminal of at least 16 columns and 4 rows".into(),
                );
            }
            let signals = Signals::install()?;
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let mut terminal = Self {
                tty,
                saved,
                _signals: signals,
                active: true,
                alternate: false,
            };
            if terminal.ready(0)? {
                return Err("calibration cancelled: pending terminal input".into());
            }
            terminal.alternate = true;
            terminal.write_bytes(
                b"\x1b[?1049h\x1b[2J\x1b[HTildy width calibration (any input cancels)",
                deadline,
                true,
            )?;
            Ok(terminal)
        }
    }
    fn ready(&self, timeout_ms: i32) -> Result<bool, String> {
        self.poll(libc::POLLIN, timeout_ms)
    }
    fn poll(&self, events: i16, timeout_ms: i32) -> Result<bool, String> {
        #[cfg(target_os = "macos")]
        {
            // Darwin /dev/tty is a controlling-terminal indirection: poll(2)
            // reports POLLNVAL for it even though tcgetattr/read/write work.
            // select(2) supports that descriptor and the same bounded waits.
            let fd = self.tty.as_raw_fd();
            if fd >= libc::FD_SETSIZE as i32 {
                return Err("terminal descriptor exceeds select capacity".into());
            }
            // SAFETY: initialized fd sets and a bounded timeval, one valid fd.
            let result = unsafe {
                let mut input: libc::fd_set = std::mem::zeroed();
                let mut output: libc::fd_set = std::mem::zeroed();
                libc::FD_ZERO(&mut input);
                libc::FD_ZERO(&mut output);
                if events & libc::POLLIN != 0 {
                    libc::FD_SET(fd, &mut input);
                }
                if events & libc::POLLOUT != 0 {
                    libc::FD_SET(fd, &mut output);
                }
                let mut wait = libc::timeval {
                    tv_sec: (timeout_ms / 1000) as libc::time_t,
                    tv_usec: (timeout_ms % 1000) * 1000,
                };
                libc::select(
                    fd + 1,
                    &mut input,
                    &mut output,
                    std::ptr::null_mut(),
                    &mut wait,
                )
            };
            if result >= 0 {
                return Ok(result > 0);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error.to_string())
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut poll = libc::pollfd {
                fd: self.tty.as_raw_fd(),
                events,
                revents: 0,
            };
            // SAFETY: one initialized pollfd for this open terminal.
            let n = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    return Ok(false);
                }
                return Err(std::io::Error::last_os_error().to_string());
            }
            if poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err("terminal disconnected during calibration".into());
            }
            Ok(n > 0 && poll.revents & events != 0)
        }
    }
    fn write_bytes(
        &mut self,
        bytes: &[u8],
        deadline: Instant,
        cancellable: bool,
    ) -> Result<(), String> {
        let mut offset = 0;
        while offset < bytes.len() {
            if cancellable {
                self.check_cancelled()?;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("terminal write timed out".into());
            }
            match self.tty.write(&bytes[offset..]) {
                Ok(0) => return Err("terminal closed during write".into()),
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    self.poll(libc::POLLOUT, left.as_millis().clamp(1, 20) as i32)?;
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }
    fn check_cancelled(&self) -> Result<(), String> {
        if CANCELLED.load(Ordering::Relaxed) != 0 {
            Err("calibration cancelled by signal".into())
        } else {
            Ok(())
        }
    }
    fn query(&mut self, deadline: Instant) -> Result<(usize, usize), String> {
        self.check_cancelled()?;
        let deadline = deadline.min(Instant::now() + Duration::from_millis(250));
        self.write_bytes(b"\x1b[6n", deadline, true)?;
        let mut reply = Vec::new();
        loop {
            self.check_cancelled()?;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("terminal CPR reply timed out".into());
            }
            if !self.ready(left.as_millis().clamp(1, 20) as i32)? {
                continue;
            }
            let mut chunk = [0u8; 64];
            let n = match self.tty.read(&mut chunk) {
                Ok(0) => return Err("terminal closed during read".into()),
                Ok(n) => n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue
                }
                Err(e) => return Err(e.to_string()),
            };
            reply.extend_from_slice(&chunk[..n]);
            if reply.len() > 32 {
                return Err(
                    "calibration cancelled: oversized or interfering terminal input".into(),
                );
            }
            match parse_cpr(&reply)? {
                Some(position) => return Ok(position),
                None => continue,
            }
        }
    }
    fn measure(&mut self, cluster: &str, deadline: Instant) -> Result<usize, String> {
        let mut measured = None;
        for _ in 0..2 {
            self.check_cancelled()?;
            self.write_bytes(b"\x1b[2;2H\x1b[2K", deadline, true)?;
            if self.query(deadline)? != (2, 2) {
                return Err("terminal CPR did not confirm the known starting row/column".into());
            }
            self.write_bytes(cluster.as_bytes(), deadline, true)?;
            let (row, col) = self.query(deadline)?;
            if row != 2 || !(2..=2 + MAX_COLUMNS).contains(&col) {
                return Err("terminal CPR wrapped or reported an out-of-range advance".into());
            }
            let width = col - 2;
            if measured.is_some_and(|previous| previous != width) {
                return Err("repeated terminal CPR measurements disagree".into());
            }
            measured = Some(width);
        }
        Ok(measured.unwrap_or(0))
    }
    fn finish(&mut self) -> Result<(), String> {
        if !self.active {
            return Ok(());
        }
        // Bounded cleanup drains outstanding replies instead of replaying any
        // captured bytes into the shell. CPR has no request IDs, so arbitrarily
        // late replies beyond this window cannot be perfectly isolated.
        let until = Instant::now() + Duration::from_millis(200);
        let mut input = false;
        let mut bytes = 0;
        while Instant::now() < until && bytes < 4096 {
            match self.ready(10) {
                Ok(true) => {
                    let mut buf = [0; 128];
                    match self.tty.read(&mut buf) {
                        Ok(n) => {
                            input |= n > 0;
                            bytes += n;
                        }
                        Err(_) => break,
                    }
                }
                Ok(false) => {}
                Err(_) => break,
            }
        }
        let output = if self.alternate {
            self.write_bytes(
                b"\x1b[?1049l",
                Instant::now() + Duration::from_millis(100),
                false,
            )
        } else {
            Ok(())
        };
        // SAFETY: restoring exactly the termios obtained before raw mode.
        let restored = unsafe { libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.saved) };
        if restored != 0 {
            // A transient EINTR/failure still gets one immediate best-effort retry.
            // SAFETY: saved attributes and the descriptor remain valid.
            if unsafe { libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.saved) } == 0 {
                self.active = false;
            }
            return Err("failed to restore terminal attributes on the first attempt".into());
        }
        self.active = false;
        output?;
        self.check_cancelled()?;
        if input {
            return Err("calibration cancelled: unexpected input during cleanup".into());
        }
        Ok(())
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn parse_cpr(reply: &[u8]) -> Result<Option<(usize, usize)>, String> {
    if !b"\x1b[".starts_with(reply) && !reply.starts_with(b"\x1b[") {
        return Err("calibration cancelled: keyboard or unrelated terminal input".into());
    }
    if reply.len() < 3 {
        return Ok(None);
    }
    let body = &reply[2..];
    if body
        .iter()
        .any(|b| !b.is_ascii_digit() && !matches!(b, b';' | b'R'))
    {
        return Err("calibration cancelled: keyboard or unrelated terminal input".into());
    }
    if !body.ends_with(b"R") {
        return Ok(None);
    }
    let text = std::str::from_utf8(&body[..body.len() - 1]).map_err(|_| "invalid terminal CPR")?;
    let (row, col) = text.split_once(';').ok_or("invalid terminal CPR")?;
    if row.is_empty()
        || col.is_empty()
        || row.len() > 5
        || col.len() > 5
        || !row.bytes().all(|b| b.is_ascii_digit())
        || !col.bytes().all(|b| b.is_ascii_digit())
    {
        return Err("invalid terminal CPR".into());
    }
    let (row, col) = (
        row.parse().map_err(|_| "invalid terminal CPR")?,
        col.parse().map_err(|_| "invalid terminal CPR")?,
    );
    if row == 0 || col == 0 {
        return Err("invalid terminal CPR".into());
    }
    Ok(Some((row, col)))
}

pub fn measure(clusters: &[String]) -> Result<Vec<Entry>, String> {
    let deadline = Instant::now() + Duration::from_millis(4600);
    let mut terminal = Terminal::open(deadline)?;
    let mut entries = Vec::new();
    for cluster in clusters {
        if Instant::now() >= deadline {
            return Err("calibration exceeded its total five-second budget".into());
        }
        entries.push(Entry {
            cluster: cluster.clone(),
            columns: terminal.measure(cluster, deadline)?,
        });
    }
    terminal.finish()?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpr_parser_rejects_keys_extra_replies_and_malformed_positions() {
        assert_eq!(parse_cpr(b"\x1b[2;4R").unwrap(), Some((2, 4)));
        assert_eq!(parse_cpr(b"\x1b[2;").unwrap(), None);
        for bad in [
            b"x".as_slice(),
            b"\x03",
            b"\x1b[2;4Rx",
            b"\x1b[0;4R",
            b"\x1b[2;4R\x1b[2;4R",
            b"\x1b[2;4;5R",
        ] {
            assert!(parse_cpr(bad).is_err(), "{bad:?}");
        }
    }
}
