//! Incremental OSC 7501 (Program Status Protocol, rev 0.4, 2026-10-10)
//! parser for a terminal output byte stream. Owner: M2 task D.
//!
//! Spec: <https://www.superlogical.com/rex/docs/build/program-status>.
//! A report is `ESC ] 7501 ; body ST` with ST = `ESC \` or BEL; the body is
//! `key=value` pairs separated by `:`. [`Scanner`] is fed arbitrary chunks of
//! the stream (sequences may span chunk boundaries) and only *reads* them: the
//! caller forwards the original bytes untouched, so the real terminal still
//! sees and displays every sequence, including the `OSC 7501 ; ?` feature
//! query (whose reply comes from the terminal on the input side).
//!
//! Byte handling inside an OSC string follows the usual VT parser rules
//! (xterm, Ghostty): C0 controls other than BEL, ESC, CAN and SUB are ignored;
//! CAN/SUB abort the sequence; ESC followed by anything but `\` aborts it and
//! starts a new escape sequence. Only the 7-bit introducer and terminators are
//! recognised (8-bit C1 bytes are UTF-8 continuation bytes in practice).
//!
//! Besides reports the scanner recognises `OSC 133 ; A` (shell prompt start),
//! after which 7501 says working/blocked records end; see [`Event::Prompt`].
//!
//! [`parse_body`] is the pair parser on its own (for `tildy agent publish
//! --osc`), with every limit of the spec's table; a report that breaks a limit,
//! carries bad base64, invalid UTF-8 or a control character in `title`/`msg`,
//! an invalid `id`, or a missing/unknown `state` is rejected as a whole.

use serde::Serialize;

/// Whole sequence, `ESC ]` through ST inclusive.
pub const MAX_SEQUENCE: usize = 4096;
pub const MAX_KEY: usize = 16;
pub const MAX_APP: usize = 32;
pub const MAX_ID: usize = 128;
pub const MAX_ID_SEGMENT: usize = 32;
pub const MAX_ID_DEPTH: usize = 8;
pub const MAX_TITLE_ENCODED: usize = 256;
pub const MAX_TITLE: usize = 192;
pub const MAX_MSG_ENCODED: usize = 2732;
pub const MAX_MSG: usize = 2048;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    /// An operation, not a state: removes the addressed record and its
    /// descendants (all records when the report has no `id`).
    Clear,
}

impl State {
    pub fn parse(s: &str) -> Option<State> {
        Some(match s {
            "idle" => State::Idle,
            "working" => State::Working,
            "done" => State::Done,
            "blocked" => State::Blocked,
            "error" => State::Error,
            "clear" => State::Clear,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Done => "done",
            State::Blocked => "blocked",
            State::Error => "error",
            State::Clear => "clear",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Permission,
    Question,
    Auth,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "permission" => Kind::Permission,
            "question" => Kind::Question,
            "auth" => Kind::Auth,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Permission => "permission",
            Kind::Question => "question",
            Kind::Auth => "auth",
        }
    }
}

/// One accepted report. `title` and `msg` are decoded (plain UTF-8, no
/// control characters); empty ones are reported as absent. `kind` is only set
/// for `blocked`, `progress` only for `working`/`blocked`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
}

/// Why a complete report was discarded. Nothing from it applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", content = "field", rename_all = "snake_case")]
pub enum Reject {
    MissingState,
    UnknownState(String),
    /// `id` is not 1–8 `/`-separated `[A-Za-z0-9_.+-]{1,32}` segments.
    BadId,
    /// A limit from the spec's table was exceeded: `sequence`, `key`, `app`,
    /// `id`, `title` or `msg`.
    TooLong(&'static str),
    BadBase64(&'static str),
    BadUtf8(&'static str),
    ControlChar(&'static str),
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reject::MissingState => write!(f, "missing state"),
            Reject::UnknownState(s) => write!(f, "unknown state {s:?}"),
            Reject::BadId => write!(f, "invalid id"),
            Reject::TooLong(w) => write!(f, "{w} exceeds its limit"),
            Reject::BadBase64(w) => write!(f, "{w} is not valid base64"),
            Reject::BadUtf8(w) => write!(f, "{w} is not valid UTF-8"),
            Reject::ControlChar(w) => write!(f, "{w} contains a control character"),
        }
    }
}

/// What the scanner found in the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Report(Report),
    /// A complete `OSC 7501` report that was discarded.
    Rejected(Reject),
    /// `OSC 133 ; A`: a shell prompt started on this terminal. Per 7501 the
    /// terminal drops working and blocked records (and may drop idle ones).
    Prompt,
}

/// Which OSC we are inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Osc {
    /// Reading the command number (up to `;`).
    Num,
    /// `7501;` seen: collecting the report body.
    Status,
    /// `133;` seen: collecting the shell-integration body.
    Shell,
    /// Any other OSC, or one that grew past the limit: wait for its end.
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ground,
    /// ESC seen outside an OSC.
    Esc,
    Osc(Osc),
}

/// Incremental scanner. Holds at most [`MAX_SEQUENCE`] bytes of a partial
/// sequence; a longer one is reported once as `TooLong("sequence")` and the
/// rest of it is skipped up to its terminator.
#[derive(Debug)]
pub struct Scanner {
    mode: Mode,
    /// Inside an OSC, the previous byte was ESC.
    osc_esc: bool,
    /// Bytes of the current OSC so far, from `ESC ]` inclusive.
    len: usize,
    num: [u8; 4],
    num_len: usize,
    buf: Vec<u8>,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner::new()
    }
}

impl Scanner {
    pub fn new() -> Scanner {
        Scanner { mode: Mode::Ground, osc_esc: false, len: 0, num: [0; 4], num_len: 0, buf: Vec::new() }
    }

    /// Bytes currently held for a partial sequence (never above MAX_SEQUENCE).
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Scan one chunk of output, calling `on` for every finished sequence of
    /// interest. Never fails; malformed input just yields no event.
    pub fn feed(&mut self, chunk: &[u8], mut on: impl FnMut(Event)) {
        let mut i = 0;
        while i < chunk.len() {
            if self.mode == Mode::Ground {
                match chunk[i..].iter().position(|&b| b == ESC) {
                    Some(p) => {
                        self.mode = Mode::Esc;
                        i += p + 1;
                    }
                    None => return,
                }
                continue;
            }
            self.step(chunk[i], &mut on);
            i += 1;
        }
    }

    fn start_osc(&mut self) {
        self.mode = Mode::Osc(Osc::Num);
        self.osc_esc = false;
        self.len = 2;
        self.num_len = 0;
        self.buf.clear();
    }

    fn step(&mut self, b: u8, on: &mut impl FnMut(Event)) {
        let osc = match self.mode {
            Mode::Ground => {
                if b == ESC {
                    self.mode = Mode::Esc;
                }
                return;
            }
            Mode::Esc => {
                match b {
                    b']' => self.start_osc(),
                    ESC => {}
                    _ => self.mode = Mode::Ground,
                }
                return;
            }
            Mode::Osc(o) => o,
        };
        self.len = self.len.saturating_add(1);
        if self.osc_esc {
            self.osc_esc = false;
            if b == b'\\' {
                self.finish(osc, on);
            } else {
                // ESC + anything else: the OSC is abandoned and this ESC
                // starts the next escape sequence.
                self.abandon();
                self.mode = Mode::Esc;
                self.step(b, on);
            }
            return;
        }
        match b {
            BEL => return self.finish(osc, on),
            ESC => {
                self.osc_esc = true;
                return;
            }
            CAN | SUB => return self.abandon(),
            0x00..=0x1f => return, // ignored inside OSC strings
            _ => {}
        }
        match osc {
            Osc::Num => {
                if b == b';' {
                    let n = &self.num[..self.num_len];
                    self.mode = Mode::Osc(match n {
                        b"7501" => Osc::Status,
                        b"133" => Osc::Shell,
                        _ => Osc::Skip,
                    });
                } else if b.is_ascii_digit() && self.num_len < self.num.len() {
                    self.num[self.num_len] = b;
                    self.num_len += 1;
                } else {
                    self.mode = Mode::Osc(Osc::Skip);
                }
            }
            Osc::Status | Osc::Shell => {
                // The terminator adds at least one more byte, so a body that
                // already fills the limit can never end in a legal sequence.
                if self.len >= MAX_SEQUENCE {
                    if osc == Osc::Status {
                        on(Event::Rejected(Reject::TooLong("sequence")));
                    }
                    self.buf.clear();
                    self.mode = Mode::Osc(Osc::Skip);
                } else {
                    self.buf.push(b);
                }
            }
            Osc::Skip => {}
        }
    }

    fn abandon(&mut self) {
        self.buf.clear();
        self.osc_esc = false;
        self.mode = Mode::Ground;
    }

    fn finish(&mut self, osc: Osc, on: &mut impl FnMut(Event)) {
        let too_long = self.len > MAX_SEQUENCE;
        match osc {
            Osc::Status if too_long => on(Event::Rejected(Reject::TooLong("sequence"))),
            Osc::Status => {
                let body = trim(&self.buf);
                // `OSC 7501 ; ?` is the feature query (or, echoed back, its
                // reply): the terminal answers it, nothing to record.
                if body.first() != Some(&b'?') {
                    on(match parse_body(&self.buf) {
                        Ok(r) => Event::Report(r),
                        Err(e) => Event::Rejected(e),
                    });
                }
            }
            Osc::Shell if !too_long => {
                let body = trim(&self.buf);
                if body == b"A" || body.starts_with(b"A;") {
                    on(Event::Prompt);
                }
            }
            _ => {}
        }
        self.abandon();
    }
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if !first.is_ascii_whitespace() {
            break;
        }
        s = rest;
    }
    while let [rest @ .., last] = s {
        if !last.is_ascii_whitespace() {
            break;
        }
        s = rest;
    }
    s
}

fn is_value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-')
}

/// `[A-Za-z0-9_.+-]{1,32}` (the `app` grammar and one id segment).
pub fn valid_app(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_APP && s.bytes().all(is_name_byte)
}

/// Check a 7501 id: 1–8 `/`-separated segments of `[A-Za-z0-9_.+-]{1,32}`,
/// at most 128 bytes in total.
pub fn check_id(s: &str) -> Result<(), Reject> {
    if s.len() > MAX_ID {
        return Err(Reject::TooLong("id"));
    }
    let mut depth = 0;
    for seg in s.split('/') {
        depth += 1;
        if depth > MAX_ID_DEPTH || seg.len() > MAX_ID_SEGMENT {
            return Err(Reject::TooLong("id"));
        }
        if seg.is_empty() || !seg.bytes().all(is_name_byte) {
            return Err(Reject::BadId);
        }
    }
    Ok(())
}

/// True when `s` contains U+0000–U+001F, U+007F or U+0080–U+009F.
pub fn has_control(s: &str) -> bool {
    s.chars().any(|c| c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c))
}

/// Standard-alphabet base64 with optional (but then complete) `=` padding.
/// Rejects stray characters, impossible lengths and non-zero trailing bits.
pub fn decode_base64(s: &[u8]) -> Option<Vec<u8>> {
    let mut end = s.len();
    while end > 0 && s[end - 1] == b'=' && s.len() - end < 2 {
        end -= 1;
    }
    if end < s.len() && !s.len().is_multiple_of(4) {
        return None;
    }
    let data = &s[..end];
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 2);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in data {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (acc == 0).then_some(out)
}

/// Decode a `title`/`msg` value: encoded size first, then base64, decoded
/// size, UTF-8, control characters.
fn decode_text(v: &[u8], what: &'static str, max_enc: usize, max_dec: usize) -> Result<String, Reject> {
    if v.len() > max_enc {
        return Err(Reject::TooLong(what));
    }
    let raw = decode_base64(v).ok_or(Reject::BadBase64(what))?;
    if raw.len() > max_dec {
        return Err(Reject::TooLong(what));
    }
    let s = String::from_utf8(raw).map_err(|_| Reject::BadUtf8(what))?;
    if has_control(&s) {
        return Err(Reject::ControlChar(what));
    }
    Ok(s)
}

/// Parse a report body (the bytes between `7501;` and ST). Every pair is
/// checked before anything is returned: malformed pairs (no `=`, key not
/// `[a-z]+`, value outside `[A-Za-z0-9_.,+/=-]*`) are skipped, unknown keys
/// ignored, and a repeated key's last value wins; any limit, id, base64 or
/// text violation in any pair rejects the whole report.
pub fn parse_body(body: &[u8]) -> Result<Report, Reject> {
    if body.len() > MAX_SEQUENCE - 8 {
        return Err(Reject::TooLong("sequence"));
    }
    let mut state: Option<&[u8]> = None;
    let mut id: Option<String> = None;
    let mut kind: Option<&[u8]> = None;
    let mut progress: Option<&[u8]> = None;
    let mut app: Option<String> = None;
    let mut title: Option<String> = None;
    let mut msg: Option<String> = None;
    for pair in body.split(|&b| b == b':') {
        let Some(eq) = pair.iter().position(|&b| b == b'=') else { continue };
        let key = trim(&pair[..eq]);
        let val = trim(&pair[eq + 1..]);
        if key.is_empty() || !key.iter().all(u8::is_ascii_lowercase) || !val.iter().all(|&b| is_value_byte(b)) {
            continue;
        }
        if key.len() > MAX_KEY {
            return Err(Reject::TooLong("key"));
        }
        // Values are ASCII from here on (checked by is_value_byte).
        let text = || String::from_utf8_lossy(val).into_owned();
        match key {
            b"state" => state = Some(val),
            b"id" => {
                let s = text();
                check_id(&s)?;
                id = Some(s);
            }
            b"kind" => kind = Some(val),
            b"progress" => progress = Some(val),
            b"app" => {
                if val.len() > MAX_APP {
                    return Err(Reject::TooLong("app"));
                }
                // An invalid app is treated as absent, not as an error.
                let s = text();
                app = valid_app(&s).then_some(s);
            }
            b"title" => title = Some(decode_text(val, "title", MAX_TITLE_ENCODED, MAX_TITLE)?),
            b"msg" => msg = Some(decode_text(val, "msg", MAX_MSG_ENCODED, MAX_MSG)?),
            _ => {}
        }
    }
    let raw_state = state.ok_or(Reject::MissingState)?;
    let state_str = String::from_utf8_lossy(raw_state);
    let state = State::parse(&state_str).ok_or_else(|| Reject::UnknownState(state_str.into_owned()))?;
    let kind = match state {
        State::Blocked => kind.and_then(|k| Kind::parse(std::str::from_utf8(k).ok()?)),
        _ => None,
    };
    let progress = match state {
        State::Working | State::Blocked => progress.and_then(parse_progress),
        _ => None,
    };
    Ok(Report {
        id,
        state,
        kind,
        progress,
        app,
        title: title.filter(|s| !s.is_empty()),
        msg: msg.filter(|s| !s.is_empty()),
    })
}

/// Integer 0–100 written as plain digits; anything else is indeterminate.
fn parse_progress(v: &[u8]) -> Option<u8> {
    if v.is_empty() || v.len() > 3 || !v.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = v.iter().fold(0u32, |n, &d| n * 10 + u32::from(d - b'0'));
    (n <= 100).then_some(n as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in s.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..4 {
                if i <= c.len() {
                    out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn scan(chunks: &[&[u8]]) -> Vec<Event> {
        let mut s = Scanner::new();
        let mut v = Vec::new();
        for c in chunks {
            s.feed(c, |e| v.push(e));
        }
        v
    }

    fn one(input: &[u8]) -> Vec<Event> {
        scan(&[input])
    }

    fn report(body: &str) -> Report {
        match parse_body(body.as_bytes()) {
            Ok(r) => r,
            Err(e) => panic!("{body:?} rejected: {e}"),
        }
    }

    fn rejected(body: &str) -> Reject {
        match parse_body(body.as_bytes()) {
            Ok(r) => panic!("{body:?} accepted: {r:?}"),
            Err(e) => e,
        }
    }

    fn st(state: State) -> Report {
        Report { id: None, state, kind: None, progress: None, app: None, title: None, msg: None }
    }

    fn ev(r: Report) -> Event {
        Event::Report(r)
    }

    #[test]
    fn base64_decoder() {
        assert_eq!(decode_base64(b"").unwrap(), b"");
        assert_eq!(decode_base64(b"YQ==").unwrap(), b"a");
        assert_eq!(decode_base64(b"YQ").unwrap(), b"a");
        assert_eq!(decode_base64(b"YWI=").unwrap(), b"ab");
        assert_eq!(decode_base64(b"YWI").unwrap(), b"ab");
        assert_eq!(decode_base64(b"YWJj").unwrap(), b"abc");
        assert_eq!(decode_base64(b"+/+/").unwrap(), [0xfb, 0xff, 0xbf]);
        for s in ["Y", "YQ=", "YQ===", "Y===", "====", "=", "YWJj=", "YWJj====", "YW=j", "YR==", "YWJ-", "YW,j", "YW Jj"] {
            assert_eq!(decode_base64(s.as_bytes()), None, "{s:?}");
        }
        // Round trip against the test encoder for every length and byte value.
        let data: Vec<u8> = (0..=255u8).collect();
        for n in 0..data.len() {
            let enc = b64(&data[..n]);
            assert_eq!(decode_base64(enc.as_bytes()).unwrap(), &data[..n]);
            assert_eq!(decode_base64(enc.trim_end_matches('=').as_bytes()).unwrap(), &data[..n]);
        }
    }

    #[test]
    fn bel_and_esc_backslash_terminators() {
        let want = Report { app: Some("cargo".into()), ..st(State::Working) };
        assert_eq!(one(b"\x1b]7501;state=working:app=cargo\x07"), vec![ev(want.clone())]);
        assert_eq!(one(b"\x1b]7501;state=working:app=cargo\x1b\\"), vec![ev(want)]);
    }

    #[test]
    fn full_report_with_text_around() {
        let msg = b64("Allow Bash: rm -rf target?".as_bytes());
        let title = b64("构建 ✓".as_bytes());
        let input = format!(
            "hello\r\n\x1b[1mbold\x1b[0m\x1b]7501;state=blocked:kind=permission:progress=40:app=claude-code:id=task/1:title={title}:msg={msg}\x1b\\tail\n"
        );
        assert_eq!(
            one(input.as_bytes()),
            vec![ev(Report {
                id: Some("task/1".into()),
                state: State::Blocked,
                kind: Some(Kind::Permission),
                progress: Some(40),
                app: Some("claude-code".into()),
                title: Some("构建 ✓".into()),
                msg: Some("Allow Bash: rm -rf target?".into()),
            })]
        );
    }

    #[test]
    fn split_at_every_boundary_and_byte_by_byte() {
        let msg = b64(b"done in 3s");
        let input = format!(
            "a\x1b]0;title\x07b\x1b]7501;state=working:progress=5\x1b\\c\x1b[31mX\x1b]7501;state=done:msg={msg}\x07d\x1b]7501;?\x1b\\"
        );
        let input = input.as_bytes();
        let want = vec![
            ev(Report { progress: Some(5), ..st(State::Working) }),
            ev(Report { msg: Some("done in 3s".into()), ..st(State::Done) }),
        ];
        assert_eq!(one(input), want);
        for cut in 0..=input.len() {
            assert_eq!(scan(&[&input[..cut], &input[cut..]]), want, "cut at {cut}");
        }
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(scan(&bytes), want);
        for n in [2, 3, 7, 13] {
            let chunks: Vec<&[u8]> = input.chunks(n).collect();
            assert_eq!(scan(&chunks), want, "chunks of {n}");
        }
    }

    #[test]
    fn feature_query_and_reply_are_not_reports() {
        assert_eq!(one(b"\x1b]7501;?\x1b\\\x1b]7501;?\x07\x1b]7501; ?\x07"), vec![]);
        assert_eq!(one(b"\x1b]7501;?:states=idle,working\x1b\\"), vec![]);
        // Query immediately followed by DA1, as the spec suggests.
        assert_eq!(one(b"\x1b]7501;?\x1b\\\x1b[c"), vec![]);
    }

    #[test]
    fn interleaved_with_other_sequences() {
        let input = b"\x1b[?2004h\x1b[?1049h\x1b]8;;https://e.x/7501;state=error\x1b\\link\x1b]8;;\x1b\\\
\x1b]52;c;c3RhdGU9ZXJyb3I=\x07\x1bP1$r0m\x1b\\\x1b[>1u\x1b[?u\x1b]133;C\x07\
\x1b]7501;state=idle\x07\x1b]2;7501;state=error\x07\x1b]75010;state=error\x07\x1b]750;state=error\x07";
        assert_eq!(one(input), vec![ev(st(State::Idle))]);
    }

    #[test]
    fn text_that_looks_like_a_body_is_ignored() {
        assert_eq!(one(b"]7501;state=working\x07 7501;state=done\x1b\\"), vec![]);
        assert_eq!(one(b"\x1b 7501;state=done\x07"), vec![]);
        // 8-bit OSC/ST are not recognised.
        assert_eq!(one(b"\x9d7501;state=done\x9c"), vec![]);
    }

    #[test]
    fn esc_esc_bracket_starts_an_osc() {
        assert_eq!(one(b"\x1b\x1b]7501;state=done\x07"), vec![ev(st(State::Done))]);
    }

    #[test]
    fn abandoned_sequences() {
        // ESC + something other than `\` abandons the OSC and starts a new escape.
        assert_eq!(one(b"\x1b]7501;state=working\x1b]7501;state=done\x07"), vec![ev(st(State::Done))]);
        assert_eq!(one(b"\x1b]7501;state=working\x1b[0m\x07"), vec![]);
        // CAN and SUB abort.
        assert_eq!(one(b"\x1b]7501;state=working\x18\x07"), vec![]);
        assert_eq!(one(b"\x1b]7501;state=wor\x1aking\x07\x1b]7501;state=idle\x07"), vec![ev(st(State::Idle))]);
        // The OSC that never terminates does not swallow later reports' ESC.
        assert_eq!(one(b"\x1b]0;unterminated\x1b]7501;state=idle\x07"), vec![ev(st(State::Idle))]);
    }

    #[test]
    fn c0_controls_inside_the_string_are_ignored() {
        assert_eq!(one(b"\x1b]75\n01;state=wo\r\nrking:pro\tgress=7\x07"), vec![ev(Report {
            progress: Some(7),
            ..st(State::Working)
        })]);
    }

    #[test]
    fn whitespace_is_trimmed_around_keys_and_values() {
        assert_eq!(report("  state = blocked : kind= question :progress =100 "), Report {
            kind: Some(Kind::Question),
            progress: Some(100),
            ..st(State::Blocked)
        });
        // Inside a value it makes the pair malformed.
        assert_eq!(rejected("state=work ing"), Reject::MissingState);
    }

    #[test]
    fn malformed_pairs_are_skipped_and_the_rest_processed() {
        let r = report("garbage:=x:STATE=error:Key=1:a1=2:msg=héllo:app=ok:id:state=done::progress");
        assert_eq!(r, Report { app: Some("ok".into()), ..st(State::Done) });
        assert_eq!(report("state=idle:app=a$b"), st(State::Idle));
        assert_eq!(report(":::state=idle:::"), st(State::Idle));
    }

    #[test]
    fn unknown_keys_ignored_and_last_value_wins() {
        assert_eq!(report("future=1,2:state=idle:x=y"), st(State::Idle));
        assert_eq!(report("state=bogus:state=working:app=a:app=b"), Report { app: Some("b".into()), ..st(State::Working) });
        assert_eq!(report("id=a:id=b/c:state=done"), Report { id: Some("b/c".into()), ..st(State::Done) });
        assert_eq!(report(&format!("msg={}:msg={}:state=done", b64(b"one"), b64(b"two"))).msg.as_deref(), Some("two"));
        // A later invalid app clears an earlier valid one (it is treated as absent).
        assert_eq!(report("app=a:app=b,c:state=idle").app, None);
    }

    #[test]
    fn state_is_required_and_must_be_known() {
        assert_eq!(rejected("app=x"), Reject::MissingState);
        assert_eq!(rejected(""), Reject::MissingState);
        assert_eq!(rejected("state=paused"), Reject::UnknownState("paused".into()));
        assert_eq!(rejected("state="), Reject::UnknownState(String::new()));
        assert_eq!(rejected("state=working:state=Working"), Reject::UnknownState("Working".into()));
        assert_eq!(report("state=clear:id=a/b"), Report { id: Some("a/b".into()), ..st(State::Clear) });
        assert_eq!(report("state=clear"), st(State::Clear));
        // Through the scanner: rejections are reported, nothing else.
        assert_eq!(one(b"\x1b]7501;state=nope\x07"), vec![Event::Rejected(Reject::UnknownState("nope".into()))]);
    }

    #[test]
    fn kind_only_with_blocked() {
        assert_eq!(report("state=blocked:kind=auth").kind, Some(Kind::Auth));
        assert_eq!(report("state=blocked:kind=other").kind, None);
        assert_eq!(report("state=working:kind=permission").kind, None);
        assert_eq!(report("state=done:kind=question").kind, None);
    }

    #[test]
    fn progress_only_with_working_or_blocked() {
        assert_eq!(report("state=working:progress=0").progress, Some(0));
        assert_eq!(report("state=blocked:progress=100").progress, Some(100));
        assert_eq!(report("state=working:progress=050").progress, Some(50));
        for bad in ["101", "-1", "+5", "5.5", "", "1000", "0x10"] {
            assert_eq!(report(&format!("state=working:progress={bad}")).progress, None, "{bad:?}");
        }
        assert_eq!(report("state=done:progress=50").progress, None);
        assert_eq!(report("state=idle:progress=50").progress, None);
    }

    #[test]
    fn app_grammar_and_limit() {
        assert_eq!(report("state=idle:app=a.b+c_d-1").app.as_deref(), Some("a.b+c_d-1"));
        assert_eq!(report(&format!("state=idle:app={}", "a".repeat(32))).app.unwrap().len(), 32);
        assert_eq!(rejected(&format!("state=idle:app={}", "a".repeat(33))), Reject::TooLong("app"));
        assert_eq!(report("state=idle:app=a/b").app, None);
        assert_eq!(report("state=idle:app=a=b").app, None);
        assert_eq!(report("state=idle:app=").app, None);
    }

    #[test]
    fn id_grammar_and_limits() {
        let ok = |id: &str| assert_eq!(report(&format!("state=idle:id={id}")).id.as_deref(), Some(id));
        ok("a");
        ok("deploy/us-east-1/step.2+x_y");
        ok(&["s"; 8].join("/"));
        ok(&"x".repeat(32));
        // Exactly 128 bytes.
        ok(&format!("{}/{}/{}/{}", "a".repeat(32), "b".repeat(32), "c".repeat(32), "d".repeat(29)));
    }

    #[test]
    fn id_rejections() {
        let bad = |id: &str| rejected(&format!("state=idle:id={id}"));
        assert_eq!(bad(&["s"; 9].join("/")), Reject::TooLong("id"));
        assert_eq!(bad(&"x".repeat(33)), Reject::TooLong("id"));
        assert_eq!(bad(&format!("{}/{}/{}/{}", "a".repeat(32), "b".repeat(32), "c".repeat(32), "d".repeat(30))), Reject::TooLong("id"));
        assert_eq!(bad(""), Reject::BadId);
        assert_eq!(bad("a//b"), Reject::BadId);
        assert_eq!(bad("/a"), Reject::BadId);
        assert_eq!(bad("a/"), Reject::BadId);
        assert_eq!(bad("a,b"), Reject::BadId);
        assert_eq!(bad("a=b"), Reject::BadId);
        // An invalid id never falls back to the root record, even when a
        // later pair repeats the key with a valid value.
        assert_eq!(rejected("id=a//b:id=ok:state=idle"), Reject::BadId);
    }

    #[test]
    fn title_and_msg_decoding() {
        let r = report(&format!("state=done:title={}:msg={}", b64("标题".as_bytes()), b64("🚀 shipped".as_bytes())));
        assert_eq!((r.title.as_deref(), r.msg.as_deref()), (Some("标题"), Some("🚀 shipped")));
        // Unpadded base64 works; empty text is absent.
        assert_eq!(report("state=done:msg=YWI").msg.as_deref(), Some("ab"));
        assert_eq!(report("state=done:msg=:title=").msg, None);
        for v in ["YQ=", "Y", "YW,j", "YR=="] {
            assert_eq!(rejected(&format!("state=done:msg={v}")), Reject::BadBase64("msg"), "{v}");
            assert_eq!(rejected(&format!("state=done:title={v}")), Reject::BadBase64("title"), "{v}");
        }
        assert_eq!(rejected(&format!("state=done:msg={}", b64(&[0xff, 0xfe]))), Reject::BadUtf8("msg"));
    }

    #[test]
    fn control_characters_discard_the_whole_report() {
        for s in ["a\nb", "tab\there", "\0", "del\x7f", "nel\u{85}", "csi\u{9b}", "esc\x1b[31m", "\u{9f}"] {
            assert_eq!(
                rejected(&format!("state=blocked:kind=permission:msg={}", b64(s.as_bytes()))),
                Reject::ControlChar("msg"),
                "{s:?}"
            );
            assert_eq!(rejected(&format!("state=done:title={}", b64(s.as_bytes()))), Reject::ControlChar("title"));
        }
        // U+00A0 and other non-control characters are fine.
        assert_eq!(report(&format!("state=done:msg={}", b64("a\u{a0}b\u{200b}".as_bytes()))).msg.as_deref(), Some("a\u{a0}b\u{200b}"));
    }

    #[test]
    fn text_limits() {
        let title_ok = "t".repeat(MAX_TITLE);
        assert_eq!(report(&format!("state=done:title={}", b64(title_ok.as_bytes()))).title.unwrap().len(), 192);
        assert_eq!(rejected(&format!("state=done:title={}", b64("t".repeat(193).as_bytes()))), Reject::TooLong("title"));
        let msg_ok = "m".repeat(MAX_MSG);
        assert_eq!(report(&format!("state=done:msg={}", b64(msg_ok.as_bytes()))).msg.unwrap().len(), 2048);
        // 2049 bytes encode to exactly 2732 characters: the decoded check catches it.
        let enc = b64("m".repeat(2049).as_bytes());
        assert_eq!(enc.len(), MAX_MSG_ENCODED);
        assert_eq!(rejected(&format!("state=done:msg={enc}")), Reject::TooLong("msg"));
        // Encoded size is checked before decoding (even if it is garbage).
        assert_eq!(rejected(&format!("state=done:msg={}", "A".repeat(2733))), Reject::TooLong("msg"));
        assert_eq!(rejected(&format!("state=done:title={}", "A".repeat(257))), Reject::TooLong("title"));
    }

    #[test]
    fn key_limit() {
        assert_eq!(report(&format!("{}=1:state=idle", "k".repeat(16))), st(State::Idle));
        assert_eq!(rejected(&format!("{}=1:state=idle", "k".repeat(17))), Reject::TooLong("key"));
    }

    #[test]
    fn a_limit_in_any_pair_discards_the_report() {
        assert_eq!(rejected(&format!("state=done:msg={}:msg=YQ", "A".repeat(2800))), Reject::TooLong("msg"));
        assert_eq!(rejected(&format!("state=done:title={}:title=YQ", b64(b"\n"))), Reject::ControlChar("title"));
    }

    fn seq_of_len(total: usize) -> Vec<u8> {
        // ESC ] 7501 ; state=idle:x=AAA… BEL
        let head = b"\x1b]7501;state=idle:x=";
        let mut v = head.to_vec();
        v.extend(std::iter::repeat_n(b'A', total - head.len() - 1));
        v.push(BEL);
        assert_eq!(v.len(), total);
        v
    }

    #[test]
    fn whole_sequence_limit() {
        assert_eq!(one(&seq_of_len(MAX_SEQUENCE)), vec![ev(st(State::Idle))]);
        assert_eq!(one(&seq_of_len(MAX_SEQUENCE + 1)), vec![Event::Rejected(Reject::TooLong("sequence"))]);
        // With ESC \ the terminator counts two bytes.
        let mut v = seq_of_len(MAX_SEQUENCE);
        v.pop();
        v.extend_from_slice(b"\x1b\\");
        assert_eq!(one(&v), vec![Event::Rejected(Reject::TooLong("sequence"))]);
        v.remove(v.len() - 3);
        assert_eq!(one(&v), vec![ev(st(State::Idle))]);
    }

    #[test]
    fn overflow_is_bounded_and_resyncs() {
        let mut s = Scanner::new();
        let mut events = Vec::new();
        s.feed(b"\x1b]7501;state=working:x=", |e| events.push(e));
        for _ in 0..100 {
            s.feed(&[b'A'; 1000], |e| events.push(e));
            assert!(s.buffered() <= MAX_SEQUENCE);
        }
        assert_eq!(events, vec![Event::Rejected(Reject::TooLong("sequence"))]);
        s.feed(b"AAAA\x07plain text\x1b]7501;state=done\x1b\\", |e| events.push(e));
        assert_eq!(events, vec![Event::Rejected(Reject::TooLong("sequence")), ev(st(State::Done))]);
        // Huge unrelated OSCs (OSC 52 clipboard) are never buffered.
        let mut s = Scanner::new();
        s.feed(b"\x1b]52;c;", |_| {});
        s.feed(&vec![b'Q'; 100_000], |_| panic!("no event"));
        assert_eq!(s.buffered(), 0);
        let mut got = Vec::new();
        s.feed(b"\x07\x1b]7501;state=idle\x07", |e| got.push(e));
        assert_eq!(got, vec![ev(st(State::Idle))]);
    }

    #[test]
    fn shell_prompt_marks() {
        assert_eq!(one(b"\x1b]133;A\x07"), vec![Event::Prompt]);
        assert_eq!(one(b"\x1b]133;A;cl=m;aid=12\x1b\\"), vec![Event::Prompt]);
        assert_eq!(one(b"\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;AB\x07"), vec![]);
    }

    #[test]
    fn many_reports_in_one_chunk() {
        let mut input = Vec::new();
        for p in 0..=100u8 {
            input.extend_from_slice(format!("\x1b]7501;state=working:progress={p}\x07.").as_bytes());
        }
        let got = one(&input);
        assert_eq!(got.len(), 101);
        assert_eq!(got[100], ev(Report { progress: Some(100), ..st(State::Working) }));
    }

    #[test]
    fn serializes_for_debug_output() {
        let r = Report { kind: Some(Kind::Question), msg: Some("hi".into()), ..st(State::Blocked) };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"state":"blocked","kind":"question","msg":"hi"}"#);
        assert_eq!(serde_json::to_string(&Reject::TooLong("id")).unwrap(), r#"{"reason":"too_long","field":"id"}"#);
    }
}
