//! Shell frame encode/decode (protocol v1). Owner: task B.
//!
//! Frames are tab-separated text lines (see docs/protocol.md "Shell 协议 v1" and
//! docs/decisions/0001-shell-protocol.md):
//!
//! ```text
//! shell -> backend   REQ<TAB>gen<TAB>session_id<TAB>cwd<TAB>cols<TAB>lines<TAB>last_exit<TAB>last_duration_ms<TAB>env[<TAB>jobs]
//!                    BYE
//! backend -> shell   HELLO<TAB>protocol_version<TAB>binary_version<TAB>worker_epoch<TAB>pid
//!                    RES<TAB>gen<TAB>status<TAB>worker_epoch
//!                    L<TAB>line (1..n)   R<TAB>right (0..1)   T<TAB>transient line (0..n)
//!                    END<TAB>gen
//! ```
//!
//! Request fields `cwd` and `env` are escaped by the shell: `\` -> `\\`,
//! TAB -> `\t`, LF -> `\n`. All other bytes, including other control characters
//! and non-UTF-8 bytes, are passed through raw. `cwd` is decoded to raw bytes
//! (so non-UTF-8 paths survive on Unix).
//!
//! M1b: `env` is `KEY=VALUE` pairs joined by U+001F (decoded as one field, then
//! split by [`PromptRequest::env_map`]); the M1 positional form (values only) is
//! still accepted and simply yields no pairs. The whole decoded string is also
//! the env cache revision. The optional 10th field `jobs` is the shell's
//! background job count (decimal digits, u32); a 9-field REQ means `jobs = 0`.

use crate::model::{PromptRequest, RenderStatus, RenderedPrompt, BINARY_VERSION, PROTOCOL_VERSION};
use std::borrow::Cow;
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;

/// Upper bound for one request line (without newline). Longer lines are rejected.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// The shell treats frames above 64 KiB as `bad_frame`; keep every RES well below.
pub const MAX_LINE_BYTES: usize = 4000;
/// Maximum number of `L` lines in one RES frame.
pub const MAX_LEFT_LINES: usize = 12;

/// Field count of a M1 REQ (tag included); M1b adds the optional `jobs` field.
const REQ_FIELDS: usize = 9;
const REQ_FIELDS_WITH_JOBS: usize = 10;

/// A decoded shell -> backend frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellFrame {
    Request(PromptRequest),
    Bye,
    Ack { gen: u64, epoch: u64, seq: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("request line exceeds {MAX_REQUEST_BYTES} bytes")]
    TooLong,
    #[error("unknown frame tag {0:?}")]
    UnknownTag(String),
    #[error("REQ needs {REQ_FIELDS} or {REQ_FIELDS_WITH_JOBS} tab-separated fields, got {0}")]
    FieldCount(usize),
    #[error("REQ field `{field}` is invalid: {value:?}")]
    BadField { field: &'static str, value: String },
    #[error("REQ field `{0}` has an invalid escape sequence")]
    BadEscape(&'static str),
    #[error("REQ field `{0}` is not valid UTF-8")]
    NotUtf8(&'static str),
}

/// Parse one shell -> backend line (without the trailing newline).
/// Returns `Ok(None)` for an empty line, which callers ignore.
pub fn parse_line(line: &[u8]) -> Result<Option<ShellFrame>, ProtocolError> {
    if line.len() > MAX_REQUEST_BYTES { return Err(ProtocolError::TooLong); }
    if line.is_empty() {
        return Ok(None);
    }
    if line == b"BYE" {
        return Ok(Some(ShellFrame::Bye));
    }
    if line.starts_with(b"ACK\t") {
        let fields: Vec<_> = line.split(|b| *b == b'\t').collect();
        if fields.len() != 4 { return Err(ProtocolError::FieldCount(fields.len())); }
        return Ok(Some(ShellFrame::Ack {
            gen: parse_unsigned("gen", fields[1])?,
            epoch: parse_unsigned("epoch", fields[2])?,
            seq: parse_unsigned("seq", fields[3])?,
        }));
    }
    if line.starts_with(b"REQ\t") {
        return parse_request_bytes(line).map(|r| Some(ShellFrame::Request(r)));
    }
    if line.len() > MAX_REQUEST_BYTES {
        return Err(ProtocolError::TooLong);
    }
    let tag = line.split(|&b| b == b'\t').next().unwrap_or_default();
    let tag = String::from_utf8_lossy(&tag[..tag.len().min(32)]).into_owned();
    Err(ProtocolError::UnknownTag(tag))
}

/// Parse one REQ line (without trailing newline). Errors are protocol errors.
pub fn parse_request(line: &str) -> anyhow::Result<PromptRequest> {
    Ok(parse_request_bytes(line.as_bytes())?)
}

/// Byte-level REQ parser; `cwd` may contain non-UTF-8 bytes.
pub fn parse_request_bytes(line: &[u8]) -> Result<PromptRequest, ProtocolError> {
    if line.len() > MAX_REQUEST_BYTES {
        return Err(ProtocolError::TooLong);
    }
    let fields: Vec<&[u8]> = line.split(|&b| b == b'\t').collect();
    if fields.first().copied() != Some(b"REQ".as_slice()) {
        let tag = String::from_utf8_lossy(fields.first().copied().unwrap_or_default()).into_owned();
        return Err(ProtocolError::UnknownTag(tag));
    }
    if fields.len() != REQ_FIELDS && fields.len() != REQ_FIELDS_WITH_JOBS {
        return Err(ProtocolError::FieldCount(fields.len()));
    }
    let gen = parse_unsigned::<u64>("gen", fields[1])?;
    let session_id = std::str::from_utf8(fields[2]).map_err(|_| ProtocolError::NotUtf8("session_id"))?;
    if session_id.is_empty() || session_id.chars().any(char::is_control) {
        return Err(bad_field("session_id", fields[2]));
    }
    let cwd = unescape(fields[3]).ok_or(ProtocolError::BadEscape("cwd"))?;
    if cwd.is_empty() || cwd.contains(&0) {
        return Err(bad_field("cwd", fields[3]));
    }
    let cols = parse_unsigned::<u16>("cols", fields[4])?;
    let lines = parse_unsigned::<u16>("lines", fields[5])?;
    let last_exit = parse_exit(fields[6])?;
    let last_duration_ms = parse_unsigned::<u64>("last_duration_ms", fields[7])?;
    let env = unescape(fields[8]).ok_or(ProtocolError::BadEscape("env"))?;
    if env.contains(&0) {
        return Err(bad_field("env", fields[8]));
    }
    let jobs = match fields.get(9) {
        Some(raw) => parse_unsigned::<u32>("jobs", raw)?,
        None => 0,
    };
    Ok(PromptRequest {
        gen,
        session_id: session_id.to_string(),
        cwd: bytes_to_path(cwd),
        cols,
        lines,
        last_exit,
        last_duration_ms,
        env: bytes_to_revision(&env),
        jobs,
    })
}

/// Encode a HELLO frame (worker -> shell) as a complete string with newlines.
pub fn encode_hello(worker_epoch: u64, pid: u32) -> String {
    format!("HELLO\t{PROTOCOL_VERSION}\t{BINARY_VERSION}\t{worker_epoch}\t{pid}\n")
}

/// Encode a RES ... END frame as a complete string with newlines.
///
/// Prompt text is expected to be Zsh prompt-escaped already (the renderer turns
/// user `%` into `%%`). This function only enforces framing safety: control
/// characters (C0, DEL, C1, which includes TAB/LF/CR) are removed, each line is
/// capped at [`MAX_LINE_BYTES`], and at most [`MAX_LEFT_LINES`] `L` lines are
/// written. A frame always carries at least one `L` line.
pub fn encode_result(gen: u64, worker_epoch: u64, prompt: &RenderedPrompt) -> String {
    let status = match prompt.status {
        RenderStatus::Fresh => "fresh",
        RenderStatus::Partial => "partial",
    };
    let mut out = String::with_capacity(128);
    out.push_str(&format!("RES\t{gen}\t{status}\t{worker_epoch}\n"));
    if prompt.left.is_empty() {
        out.push_str("L\t\n");
    }
    for line in prompt.left.iter().take(MAX_LEFT_LINES) {
        push_line(&mut out, 'L', line);
    }
    if let Some(r) = &prompt.right {
        push_line(&mut out, 'R', r);
    }
    // A multi-line transient (a blank line above the collapsed prompt with
    // `add_newline`) goes as one T line per line, joined by the shell.
    if let Some(t) = &prompt.transient {
        for line in t.split('\n').take(MAX_LEFT_LINES) {
            push_line(&mut out, 'T', line);
        }
    }
    if let Some(seq) = prompt.observation_seq { out.push_str(&format!("O\t{seq}\n")); }
    if let Some(h) = &prompt.history {
        if !h.context_key.is_empty() { push_line(&mut out, 'K', &h.context_key); }
        if let Some(t) = &h.context_transient {
            for line in t.split('\n').take(MAX_LEFT_LINES) { push_line(&mut out, 'H', line); }
        }
        if let Some(t) = &h.context_suffix { push_line(&mut out, 'U', t); }
        if h.summary { out.push_str(&format!("S\t{}\n", h.summary_threshold_ms)); }
    }
    out.push_str(&format!("END\t{gen}\n"));
    out
}

/// Encode confirmation only after the shell applied this exact result.
pub fn encode_ack(gen: u64, epoch: u64, seq: u64) -> String {
    format!("ACK\t{gen}\t{epoch}\t{seq}\n")
}

fn push_line(out: &mut String, tag: char, text: &str) {
    out.push(tag);
    out.push('\t');
    out.push_str(&sanitize_prompt_text(text));
    out.push('\n');
}

/// Remove control characters, cap the length at [`MAX_LINE_BYTES`] (on a char
/// boundary) and drop an unpaired trailing `%` (an incomplete prompt escape,
/// e.g. left by the cut).
pub fn sanitize_prompt_text(s: &str) -> Cow<'_, str> {
    if !s.chars().any(char::is_control) {
        return Cow::Borrowed(&s[..safe_prompt_prefix(s, MAX_LINE_BYTES)]);
    }
    let mut clean: String = s.chars().filter(|c| !c.is_control()).collect();
    let end = safe_prompt_prefix(&clean, MAX_LINE_BYTES);
    clean.truncate(end);
    Cow::Owned(clean)
}

/// Protocol truncation is allowed to omit content, never half a measured-width
/// group, a color escape or a plain Unicode grapheme. Otherwise an unclosed
/// `%{` could make ZLE treat the accepted command buffer as zero-width text.
fn safe_prompt_prefix(s: &str, limit: usize) -> usize {
    let bytes = s.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'%' {
            let end = s[at..].find('%').map_or(s.len(), |n| at + n);
            for cluster in s[at..end].graphemes(true) {
                if at + cluster.len() > limit { return at; }
                at += cluster.len();
            }
            continue;
        }
        let start = at;
        let Some(&next) = bytes.get(at + 1) else { return start; };
        at += 2;
        match next {
            b'%' => {},
            b'{' => {
                let mut depth = 1;
                while at < bytes.len() && depth > 0 {
                    if bytes[at] == b'%' {
                        match bytes.get(at + 1) {
                            Some(b'%') => { at += 2; continue; },
                            Some(b'{') => { depth += 1; at += 2; continue; },
                            Some(b'}') => { depth -= 1; at += 2; continue; },
                            _ => {},
                        }
                    }
                    at += s[at..].chars().next().unwrap().len_utf8();
                }
                if depth != 0 { return start; }
            }
            b'F' | b'K' if bytes.get(at) == Some(&b'{') => {
                let Some(close) = s[at..].find('}') else { return start; };
                at += close + 1;
            }
            b'0'..=b'9' => {
                while bytes.get(at).is_some_and(u8::is_ascii_digit) { at += 1; }
                if at == bytes.len() { return start; }
                at += s[at..].chars().next().unwrap().len_utf8();
            }
            _ => {
                if !next.is_ascii() { at = start + 1 + s[start + 1..].chars().next().unwrap().len_utf8(); }
            }
        }
        if at > limit { return start; }
    }
    at
}

/// Escape a field the way the shell does (`\` -> `\\`, TAB -> `\t`, LF -> `\n`).
/// Used by tests and diagnostics; the inverse of the REQ field decoding.
pub fn escape_field(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for &b in raw {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            _ => out.push(b),
        }
    }
    out
}

/// Encode a REQ line (without newline) in the M1b 10-field form (`jobs` last,
/// always present, like the M1b shell sends it). Used by tests and tools that
/// act as a shell.
pub fn encode_request(req: &PromptRequest) -> Vec<u8> {
    let mut out = format!("REQ\t{}\t{}\t", req.gen, req.session_id).into_bytes();
    out.extend_from_slice(&escape_field(&path_to_bytes(&req.cwd)));
    out.extend_from_slice(
        format!("\t{}\t{}\t{}\t{}\t", req.cols, req.lines, req.last_exit, req.last_duration_ms).as_bytes(),
    );
    out.extend_from_slice(&escape_field(req.env.as_bytes()));
    out.extend_from_slice(format!("\t{}", req.jobs).as_bytes());
    out
}

/// Encode `KEY=VALUE` pairs into the decoded `env` field form (U+001F joined).
/// Pairs whose key is empty or contains `=`/U+001F, or whose value contains
/// U+001F, cannot be represented and are skipped.
pub fn encode_env<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    pairs
        .into_iter()
        .filter(|(k, v)| !k.is_empty() && !k.contains(['=', '\u{1f}']) && !v.contains('\u{1f}'))
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

fn unescape(field: &[u8]) -> Option<Vec<u8>> {
    if !field.contains(&b'\\') {
        return Some(field.to_vec());
    }
    let mut out = Vec::with_capacity(field.len());
    let mut it = field.iter();
    while let Some(&b) = it.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match it.next()? {
            b't' => out.push(b'\t'),
            b'n' => out.push(b'\n'),
            b'\\' => out.push(b'\\'),
            _ => return None,
        }
    }
    Some(out)
}

fn parse_unsigned<T: std::str::FromStr>(field: &'static str, raw: &[u8]) -> Result<T, ProtocolError> {
    // `str::parse` accepts a leading '+'; the protocol only allows plain digits.
    if raw.is_empty() || raw.len() > 20 || !raw.iter().all(u8::is_ascii_digit) {
        return Err(bad_field(field, raw));
    }
    std::str::from_utf8(raw).ok().and_then(|s| s.parse().ok()).ok_or_else(|| bad_field(field, raw))
}

fn parse_exit(raw: &[u8]) -> Result<i32, ProtocolError> {
    let digits = raw.strip_prefix(b"-").unwrap_or(raw);
    if digits.is_empty() || digits.len() > 10 || !digits.iter().all(u8::is_ascii_digit) {
        return Err(bad_field("last_exit", raw));
    }
    std::str::from_utf8(raw).ok().and_then(|s| s.parse().ok()).ok_or_else(|| bad_field("last_exit", raw))
}

fn bad_field(field: &'static str, raw: &[u8]) -> ProtocolError {
    let shown = &raw[..raw.len().min(64)];
    ProtocolError::BadField { field, value: String::from_utf8_lossy(shown).into_owned() }
}

#[cfg(unix)]
fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(unix)]
fn path_to_bytes(p: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_to_bytes(p: &std::path::Path) -> Vec<u8> {
    p.to_string_lossy().into_owned().into_bytes()
}

/// Turn the decoded env bytes into a `String` revision without merging distinct
/// inputs: valid UTF-8 is kept as is; if any byte is invalid, backslashes are
/// doubled and each invalid byte becomes `\xNN`.
fn bytes_to_revision(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let mut out = String::with_capacity(bytes.len() + 8);
    for chunk in bytes.utf8_chunks() {
        out.push_str(&chunk.valid().replace('\\', "\\\\"));
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RenderStatus;

    fn req_line(fields: &[&str]) -> String {
        format!("REQ\t{}", fields.join("\t"))
    }

    fn sample() -> PromptRequest {
        PromptRequest {
            gen: 7,
            session_id: "4242.123".into(),
            cwd: PathBuf::from("/tmp/a b"),
            cols: 120,
            lines: 40,
            last_exit: 0,
            last_duration_ms: 15,
            env: String::new(),
            jobs: 0,
        }
    }

    #[test]
    fn parses_basic_request() {
        let r = parse_request("REQ\t7\t4242.123\t/tmp/a b\t120\t40\t0\t15\t").unwrap();
        assert_eq!(r, sample());
    }

    #[test]
    fn tenth_field_is_jobs() {
        let r = parse_request("REQ\t7\t4242.123\t/tmp/a b\t120\t40\t0\t15\t\t3").unwrap();
        assert_eq!(r, PromptRequest { jobs: 3, ..sample() });
        let r = parse_request("REQ\t7\t4242.123\t/tmp/a b\t120\t40\t0\t15\t\t0").unwrap();
        assert_eq!(r, sample());
        let max = parse_request(&format!("REQ\t1\ts\t/\t80\t24\t0\t0\t\t{}", u32::MAX)).unwrap();
        assert_eq!(max.jobs, u32::MAX);
    }

    #[test]
    fn env_pairs_and_legacy_env() {
        // M1b: KEY=VALUE pairs joined by U+001F; values unescaped as before.
        let line = "REQ\t1\ts\t/\t80\t24\t0\t0\tUSER=me\x1fVIRTUAL_ENV=/v/a\\tb\x1fHOST=h=x\t2";
        let r = parse_request(line).unwrap();
        let m = r.env_map();
        assert_eq!(m.get("USER").map(String::as_str), Some("me"));
        assert_eq!(m.get("VIRTUAL_ENV").map(String::as_str), Some("/v/a\tb"));
        assert_eq!(m.get("HOST").map(String::as_str), Some("h=x"), "split at the first '='");
        assert_eq!(r.jobs, 2);
        // M1 positional env still parses; it yields no pairs.
        let r = parse_request("REQ\t1\ts\t/\t80\t24\t0\t0\t\x1f\x1fme\x1fbox\x1f").unwrap();
        assert!(r.env_map().is_empty());
        assert_eq!(r.env, "\x1f\x1fme\x1fbox\x1f");
        // encode_env is the inverse for representable pairs.
        let env = encode_env([("USER", "me"), ("VIRTUAL_ENV", "/v/a\tb"), ("BAD=K", "x"), ("K", "a\x1fb"), ("", "x")]);
        assert_eq!(env, "USER=me\x1fVIRTUAL_ENV=/v/a\tb");
        let mut req = sample();
        req.env = env;
        req.jobs = 4;
        let back = parse_request_bytes(&encode_request(&req)).unwrap();
        assert_eq!(back, req);
        assert_eq!(back.env_map().len(), 2);
    }

    #[test]
    fn unescapes_cwd_and_env() {
        let line = "REQ\t1\ts\t/tmp/tab\\there\\nnew\\\\back\t80\t24\t-1\t0\tssh\x1f\\tx\x1fuser";
        let r = parse_request(line).unwrap();
        assert_eq!(r.cwd, PathBuf::from("/tmp/tab\there\nnew\\back"));
        assert_eq!(r.last_exit, -1);
        assert_eq!(r.env, "ssh\x1f\tx\x1fuser");
    }

    #[test]
    fn keeps_other_control_chars_raw() {
        let r = parse_request("REQ\t1\ts\t/tmp/cr\rx\x1b[31m\t80\t24\t0\t0\t").unwrap();
        assert_eq!(r.cwd, PathBuf::from("/tmp/cr\rx\x1b[31m"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_cwd_round_trips() {
        use std::os::unix::ffi::OsStrExt;
        let mut line = b"REQ\t3\ts\t/tmp/\xff\xfe\\tz\t80\t24\t0\t0\t\xc3(".to_vec();
        let r = parse_request_bytes(&line).unwrap();
        assert_eq!(r.cwd.as_os_str().as_bytes(), b"/tmp/\xff\xfe\tz");
        assert_eq!(r.env, "\\xc3(");
        // The encoder used by test shells is the inverse (up to the env field,
        // whose invalid bytes became a textual revision).
        assert_eq!(encode_request(&r)[..29], line[..29]);
        // Distinct invalid inputs keep distinct revisions.
        line.pop();
        line.push(b')');
        assert_ne!(parse_request_bytes(&line).unwrap().env, r.env);
        // Through the frozen &str API, non-UTF-8 cannot arrive at all.
        assert!(std::str::from_utf8(&line).is_err());
    }

    #[test]
    fn encode_request_round_trips_odd_paths() {
        let mut r = sample();
        r.cwd = PathBuf::from("/x/\t\n\\\\n%~$(rm)\u{1b}");
        r.env = "a\x1fb\tc\nd\\".into();
        let line = encode_request(&r);
        assert!(!line.contains(&b'\n'));
        assert_eq!(parse_request_bytes(&line).unwrap(), r);
    }

    #[test]
    fn rejects_bad_requests() {
        let ok = ["1", "s", "/", "80", "24", "0", "0", ""];
        assert!(parse_request(&req_line(&ok)).is_ok());
        let cases: Vec<(Vec<&str>, &str)> = vec![
            (vec!["1", "s", "/", "80", "24", "0", "0"], "FieldCount(8)"),
            (vec!["1", "s", "/", "80", "24", "0", "0", "", "1", "x"], "FieldCount(11)"),
            (vec!["1", "s", "/", "80", "24", "0", "0", "", "x"], "jobs"),
            (vec!["1", "s", "/", "80", "24", "0", "0", "", ""], "jobs"),
            (vec!["1", "s", "/", "80", "24", "0", "0", "", "-1"], "jobs"),
            (vec!["1", "s", "/", "80", "24", "0", "0", "", "+1"], "jobs"),
            (vec!["1", "s", "/", "80", "24", "0", "0", "", "4294967296"], "jobs"),
            (vec!["-1", "s", "/", "80", "24", "0", "0", ""], "gen"),
            (vec!["+1", "s", "/", "80", "24", "0", "0", ""], "gen"),
            (vec!["", "s", "/", "80", "24", "0", "0", ""], "gen"),
            (vec!["99999999999999999999", "s", "/", "80", "24", "0", "0", ""], "gen"),
            (vec!["1", "", "/", "80", "24", "0", "0", ""], "session_id"),
            (vec!["1", "s", "", "80", "24", "0", "0", ""], "cwd"),
            (vec!["1", "s", "/a\\q", "80", "24", "0", "0", ""], "BadEscape(\"cwd\")"),
            (vec!["1", "s", "/a\\", "80", "24", "0", "0", ""], "BadEscape(\"cwd\")"),
            (vec!["1", "s", "/", "70000", "24", "0", "0", ""], "cols"),
            (vec!["1", "s", "/", "80", " 24", "0", "0", ""], "lines"),
            (vec!["1", "s", "/", "80", "24", "--1", "0", ""], "last_exit"),
            (vec!["1", "s", "/", "80", "24", "0", "1.5", ""], "last_duration_ms"),
            (vec!["1", "s", "/", "80", "24", "0", "0", "\\"], "BadEscape(\"env\")"),
        ];
        for (fields, want) in cases {
            let err = parse_request(&req_line(&fields)).unwrap_err();
            let msg = format!("{:?}", err.downcast_ref::<ProtocolError>().unwrap());
            assert!(msg.contains(want), "{fields:?}: {msg}");
        }
        assert!(parse_request("RES\t1\tfresh\t0").is_err());
        let long = format!("REQ\t1\ts\t/{}\t80\t24\t0\t0\t", "a".repeat(MAX_REQUEST_BYTES));
        assert_eq!(parse_request_bytes(long.as_bytes()), Err(ProtocolError::TooLong));
    }

    #[test]
    fn parse_line_handles_bye_empty_and_unknown() {
        assert_eq!(parse_line(b"BYE"), Ok(Some(ShellFrame::Bye)));
        assert_eq!(parse_line(b""), Ok(None));
        assert!(matches!(parse_line(b"BYE\t1"), Err(ProtocolError::UnknownTag(_))));
        assert!(matches!(parse_line(b"bye"), Err(ProtocolError::UnknownTag(_))));
        assert!(matches!(parse_line(b"HELLO\t1"), Err(ProtocolError::UnknownTag(t)) if t == "HELLO"));
        assert!(matches!(
            parse_line(b"REQ\t1\ts\t/\t80\t24\t0\t0\t"),
            Ok(Some(ShellFrame::Request(r))) if r.gen == 1
        ));
    }

    #[test]
    fn additive_ack_is_strict_and_bounded() {
        assert_eq!(parse_line(b"ACK\t1\t2\t3"), Ok(Some(ShellFrame::Ack { gen: 1, epoch: 2, seq: 3 })));
        assert_eq!(encode_ack(1, 2, 3), "ACK\t1\t2\t3\n");
        for bad in ["ACK\t1\t2", "ACK\t1\t2\t3\textra", "ACK\t-1\t2\t3", "ACK\t1\t2\t+3"] {
            assert!(parse_line(bad.as_bytes()).is_err());
        }
        let huge = format!("ACK\t{}", "0".repeat(MAX_REQUEST_BYTES));
        assert_eq!(parse_line(huge.as_bytes()), Err(ProtocolError::TooLong));
    }

    #[test]
    fn encodes_hello() {
        assert_eq!(encode_hello(42, 777), format!("HELLO\t1\t{BINARY_VERSION}\t42\t777\n"));
    }

    #[test]
    fn encodes_result_frames() {
        let p = RenderedPrompt {
            status: RenderStatus::Partial,
            left: vec!["%F{blue}~/src%f %F{yellow}main%f".into(), "%# ".into()],
            right: Some("took 3.2s".into()),
            history: None, observation_seq: None,
            transient: Some("❯ ".into()),
        };
        assert_eq!(
            encode_result(9, 5, &p),
            "RES\t9\tpartial\t5\nL\t%F{blue}~/src%f %F{yellow}main%f\nL\t%# \nR\ttook 3.2s\nT\t❯ \nEND\t9\n"
        );
        let p = RenderedPrompt { status: RenderStatus::Fresh, left: vec![], right: None, transient: None, history: None, observation_seq: None };
        assert_eq!(encode_result(1, 0, &p), "RES\t1\tfresh\t0\nL\t\nEND\t1\n");
    }

    #[test]
    fn result_strips_control_chars_and_bounds_size() {
        let p = RenderedPrompt {
            status: RenderStatus::Fresh,
            left: vec!["a\tb\nc\r\u{1b}[31md\u{7}\u{9b}e 100%%".into()],
            right: Some("\u{0}x".into()),
            history: None, observation_seq: None,
            transient: None,
        };
        let out = encode_result(2, 3, &p);
        assert_eq!(out, "RES\t2\tfresh\t3\nL\tabc[31mde 100%%\nR\tx\nEND\t2\n");
        assert_eq!(out.lines().count(), 4);

        let huge = RenderedPrompt {
            status: RenderStatus::Fresh,
            left: vec![format!("{}%%", "é".repeat(MAX_LINE_BYTES)); 40],
            right: Some("%".repeat(MAX_LINE_BYTES + 3)),
            history: None, observation_seq: None,
            transient: Some("x".repeat(10 * MAX_LINE_BYTES)),
        };
        let out = encode_result(4, 1, &huge);
        assert!(out.len() < 64 * 1024, "{}", out.len());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.iter().filter(|l| l.starts_with("L\t")).count(), MAX_LEFT_LINES);
        for l in &lines {
            assert!(l.len() <= MAX_LINE_BYTES + 2);
        }
        let r = lines.iter().find(|l| l.starts_with("R\t")).unwrap();
        assert_eq!((r.len() - 2) % 2, 0, "no dangling % after truncation");
    }

    #[test]
    fn calibrated_frames_truncate_only_at_complete_prompt_and_grapheme_boundaries() {
        let unit = "%{👩‍💻%3G%}";
        let prompt = RenderedPrompt { status: RenderStatus::Fresh, left:vec![format!("%F{{blue}}{}%f", unit.repeat(1000))], right:None, transient:None, history:None, observation_seq:None };
        let frame = encode_result(1, 0, &prompt);
        let line = frame.lines().find_map(|line| line.strip_prefix("L\t")).unwrap();
        assert!(line.len() <= MAX_LINE_BYTES);
        assert_eq!(line.matches("%{").count(), line.matches("%}").count());
        assert_eq!(crate::render::visible_width(line), line.matches(unit).count() * 3);
        assert_eq!(crate::render::strip_prompt_escapes(line), "👩‍💻".repeat(line.matches(unit).count()));
        for tail in ["👩‍💻", "e\u{301}", "%F{#abcdef}", "%K{#abcdef}", unit] {
            let prefix = "a".repeat(MAX_LINE_BYTES - 1);
            assert_eq!(sanitize_prompt_text(&(prefix.clone() + tail)), prefix, "tail={tail:?}");
        }
        assert_eq!(sanitize_prompt_text("x%{👩‍💻%3G"), "x");
        assert_eq!(sanitize_prompt_text("x%F{red"), "x");
        assert_eq!(sanitize_prompt_text("x%K{"), "x");
    }

    #[test]
    fn sanitize_borrows_clean_text() {
        assert!(matches!(sanitize_prompt_text("%F{red}ok%f"), Cow::Borrowed(_)));
        assert!(matches!(sanitize_prompt_text("100%%"), Cow::Borrowed(_)));
        assert_eq!(sanitize_prompt_text("é\u{85}x"), "éx");
        assert_eq!(sanitize_prompt_text("50%"), "50");
        assert_eq!(sanitize_prompt_text("50%%%"), "50%%");
        assert_eq!(sanitize_prompt_text("%"), "");
    }

    #[test]
    fn rejects_nul_in_cwd_and_env() {
        assert!(parse_request("REQ\t1\ts\t/a\0b\t80\t24\t0\t0\t").is_err());
        assert!(parse_request("REQ\t1\ts\t/a\t80\t24\t0\t0\tx\0").is_err());
    }

    /// Cross-check against task E's wire samples (fixtures/protocol): REQ/BYE
    /// lines parse, and each RES block re-encodes byte for byte from its parts.
    fn history_slot(h: &mut Option<crate::model::HistoryMetadata>) -> &mut crate::model::HistoryMetadata {
    h.get_or_insert_with(|| crate::model::HistoryMetadata { context_key: String::new(), context_transient: None, context_suffix: None, summary: false, summary_threshold_ms: 3000 })
}

#[test]
    fn wire_samples_round_trip() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/protocol/wire-samples.txt");
        let Ok(text) = std::fs::read_to_string(path) else {
            eprintln!("skipping: {path} not present");
            return;
        };
        let mut kinds = Vec::new();
        for block in text.split("\n\n").map(str::trim_end).filter(|b| !b.is_empty()) {
            let first = block.lines().next().unwrap();
            let tag = first.split('\t').next().unwrap();
            kinds.push(tag.to_string());
            match tag {
                "REQ" => {
                    let r = parse_request(first).unwrap();
                    // The encoder always writes the M1b `jobs` field; a 9-field
                    // sample re-encodes identically plus `\t0`.
                    let expected = if first.split('\t').count() == 9 { format!("{first}\t0") } else { first.to_string() };
                    assert_eq!(encode_request(&r), expected.as_bytes(), "REQ re-encodes identically");
                }
                "ACK" => assert!(matches!(parse_line(first.as_bytes()), Ok(Some(ShellFrame::Ack { .. })))),
                "BYE" => assert_eq!(parse_line(first.as_bytes()), Ok(Some(ShellFrame::Bye))),
                "HELLO" => assert_eq!(first.split('\t').count(), 5),
                "RES" => {
                    let head: Vec<&str> = first.split('\t').collect();
                    let mut p = RenderedPrompt {
                        status: if head[2] == "fresh" { RenderStatus::Fresh } else { RenderStatus::Partial },
                        left: vec![],
                        right: None,
                        history: None, observation_seq: None,
            transient: None,
                    };
                    for l in block.lines().skip(1) {
                        let (t, rest) = l.split_once('\t').unwrap();
                        match t {
                            "L" => p.left.push(rest.into()),
                            "R" => p.right = Some(rest.into()),
                            "T" => match &mut p.transient {
                                Some(t) => {
                                    t.push('\n');
                                    t.push_str(rest);
                                }
                                None => p.transient = Some(rest.into()),
                            },
                            "O" => p.observation_seq = Some(rest.parse().unwrap()),
                            "K" => p.history = Some(crate::model::HistoryMetadata { context_key:rest.into(), context_transient:None, context_suffix:None, summary:false, summary_threshold_ms:3000 }),
                            "U" => history_slot(&mut p.history).context_suffix = Some(rest.into()),
                            "S" => { let h=history_slot(&mut p.history); h.summary=true; h.summary_threshold_ms=rest.parse().unwrap(); },
                            "END" => assert_eq!(rest, head[1]),
                            other => panic!("unexpected tag {other}"),
                        }
                    }
                    let encoded = encode_result(head[1].parse().unwrap(), head[3].parse().unwrap(), &p);
                    assert_eq!(encoded, format!("{block}\n"));
                }
                other => panic!("unexpected frame {other}"),
            }
        }
        assert!(kinds.iter().any(|k| k == "REQ") && kinds.iter().any(|k| k == "RES"), "{kinds:?}");
    }
}
