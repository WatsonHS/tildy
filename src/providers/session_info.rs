//! SSH / hostname / username. Owner: task D. Each lookup is cheap and infallible.

use std::ffi::CStr;

/// True when any of SSH_CONNECTION, SSH_TTY, SSH_CLIENT is set and non-empty.
pub fn is_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// Short hostname (text before the first dot), lossy for non-UTF-8.
pub fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for buf.len() bytes; we force NUL termination below.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len() - 1) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len() - 1);
    let full = String::from_utf8_lossy(&buf[..end]);
    let short = full.split('.').next().unwrap_or("");
    if short.is_empty() { None } else { Some(short.to_string()) }
}

/// `$USER` if non-empty, else the passwd entry for the real uid.
pub fn username() -> Option<String> {
    if let Some(u) = std::env::var_os("USER") {
        let u = u.to_string_lossy().into_owned();
        if !u.is_empty() {
            return Some(u);
        }
    }
    // SAFETY: getpwuid returns a pointer to static storage or null; we copy immediately.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_name.is_null() {
            return None;
        }
        let name = CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned();
        if name.is_empty() { None } else { Some(name) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        if let Some(h) = hostname() {
            assert!(!h.contains('.') && !h.is_empty());
        }
        assert!(username().is_some());
        let _ = is_ssh();
    }
}
