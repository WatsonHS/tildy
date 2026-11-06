//! Bounded child-process runner shared by providers (git, command modules).
//! Owner: M1b task B.
//!
//! The child gets its own process group so that on timeout the whole tree (a
//! wrapper script and its `sleep`, a version manager shim and the real binary)
//! is killed with one signal. Output is drained on two threads so a chatty
//! child never blocks on a full pipe; each stream keeps at most `max_output`
//! bytes and discards the rest.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunErr {
    /// Still running at the deadline; the process group was killed.
    Timeout,
    /// The executable does not exist (ENOENT from spawn).
    NotFound,
    Spawn(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run `cmd` until it exits or `deadline` passes. stdin is `/dev/null`;
/// stdout and stderr are captured (each capped at `max_output` bytes).
pub fn run(mut cmd: Command, deadline: Instant, max_output: usize) -> Result<Output, RunErr> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RunErr::NotFound
        } else {
            RunErr::Spawn(e.to_string())
        }
    })?;
    let pid = child.id() as libc::pid_t;
    let so = child.stdout.take();
    let se = child.stderr.take();
    let t_out = std::thread::spawn(move || drain(so, max_output));
    let t_err = std::thread::spawn(move || drain(se, max_output));
    // Poll with a short backoff: fast commands (`node --version`) are noticed
    // within ~0.1 ms, long ones cost at most one wakeup every 2 ms.
    let mut nap = Duration::from_micros(100);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                let now = Instant::now();
                if now >= deadline {
                    break None;
                }
                std::thread::sleep(nap.min(deadline - now));
                nap = (nap * 2).min(Duration::from_millis(2));
            }
            Err(e) => {
                kill_group(pid);
                let _ = child.wait();
                return Err(RunErr::Spawn(format!("wait failed: {e}")));
            }
        }
    };
    match status {
        Some(s) => Ok(Output {
            success: s.success(),
            code: s.code(),
            stdout: t_out.join().unwrap_or_default(),
            stderr: t_err.join().unwrap_or_default(),
        }),
        None => {
            kill_group(pid);
            let _ = child.wait();
            let _ = t_out.join();
            let _ = t_err.join();
            Err(RunErr::Timeout)
        }
    }
}

fn drain<R: Read>(r: Option<R>, max: usize) -> Vec<u8> {
    let Some(mut r) = r else { return Vec::new() };
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let room = max.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    kept
}

pub fn kill_group(pid: libc::pid_t) {
    // SAFETY: plain syscalls on a pid we spawned (group leader via process_group(0)).
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
        libc::kill(pid, libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c").arg(script);
        c
    }

    #[test]
    fn captures_output_and_status() {
        let o = run(sh("echo out; echo err >&2; exit 3"), Instant::now() + Duration::from_secs(5), 1 << 20).unwrap();
        assert_eq!((o.success, o.code), (false, Some(3)));
        assert_eq!(o.stdout, b"out\n");
        assert_eq!(o.stderr, b"err\n");
    }

    #[test]
    fn caps_output_without_blocking_the_child() {
        let o = run(sh("yes | head -c 300000"), Instant::now() + Duration::from_secs(5), 1000).unwrap();
        assert!(o.success);
        assert_eq!(o.stdout.len(), 1000);
    }

    #[test]
    fn timeout_and_missing_binary() {
        let t = Instant::now();
        let r = run(sh("sleep 5"), t + Duration::from_millis(100), 100);
        assert_eq!(r, Err(RunErr::Timeout));
        assert!(t.elapsed() < Duration::from_millis(1000), "{:?}", t.elapsed());
        let r = run(Command::new("/definitely/not/a/binary"), Instant::now() + Duration::from_secs(1), 100);
        assert_eq!(r, Err(RunErr::NotFound));
    }
}
