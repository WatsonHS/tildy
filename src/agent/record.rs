//! Agent status record (schema/agent-state.schema.json v1): the OSC 7501
//! (Program Status Protocol rev 0.4) report fields plus where to show the
//! record (`scope`), who published it and for how long it lives.
//!
//! Validation follows the 7501 limits: an over-limit or malformed record is
//! rejected whole ([`Reject::Invalid`]); `kind` outside `blocked` and
//! `progress` outside `working`/`blocked` are dropped ("used only with").
//!
//! [`parse_osc_body`] turns a raw 7501 body (`state=blocked:kind=permission:msg=<base64>`)
//! into a [`Report`] with the protocol's own leniency rules (malformed pairs
//! skipped, unknown keys ignored, last duplicate wins, invalid `kind`,
//! `progress` and `app` treated as absent).

use serde::{Deserialize, Serialize};
use std::fmt;

/// `protocol_version` of records written by this build.
pub const RECORD_VERSION: u32 = 1;

/// 7501 limits (rev 0.4, "Encoding and limits").
pub const MAX_SEGMENT_BYTES: usize = 32;
pub const MAX_ID_DEPTH: usize = 8;
pub const MAX_ID_BYTES: usize = 128;
pub const MAX_APP_BYTES: usize = 32;
pub const MAX_TITLE_BYTES: usize = 192;
pub const MAX_TITLE_ENCODED: usize = 256;
pub const MAX_MSG_BYTES: usize = 2048;
pub const MAX_MSG_ENCODED: usize = 2732;
pub const MAX_KEY_BYTES: usize = 16;
/// Maximum body with the shortest OSC introducer/terminator (BEL).
pub const MAX_BODY_BYTES: usize = super::osc::MAX_SEQUENCE - 8;

/// Tildy limits for fields 7501 does not have.
pub const MAX_TTL_S: u32 = 86_400;
/// Scope ids: a session id, a worktree identity (`<common-dir>@<toplevel>`) or `*`.
pub const MAX_SCOPE_ID_BYTES: usize = 4096;
pub const MAX_SESSION_BYTES: usize = 128;

/// Default `ttl_s` for idle/working/blocked records.
pub const DEFAULT_TTL_ACTIVE_S: u32 = 7200;
/// Default `ttl_s` for done/error records.
pub const DEFAULT_TTL_FINAL_S: u32 = 900;

/// 7501 states (`clear` is an operation, see [`Op`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

impl State {
    pub const ALL: [State; 5] = [State::Idle, State::Working, State::Done, State::Blocked, State::Error];

    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Done => "done",
            State::Blocked => "blocked",
            State::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        State::ALL.into_iter().find(|st| st.as_str() == s)
    }

    /// Display precedence of the highlighted record: blocked > error > done >
    /// working > idle (higher is more important).
    pub fn rank(self) -> u8 {
        match self {
            State::Blocked => 4,
            State::Error => 3,
            State::Done => 2,
            State::Working => 1,
            State::Idle => 0,
        }
    }

    /// done/error survive the origin shell's next prompt (shown once).
    pub fn is_final(self) -> bool {
        matches!(self, State::Done | State::Error)
    }

    pub fn default_ttl_s(self) -> u32 {
        if self.is_final() { DEFAULT_TTL_FINAL_S } else { DEFAULT_TTL_ACTIVE_S }
    }
}

/// Why a `blocked` program needs the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Permission,
    Question,
    Auth,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Permission, Kind::Question, Kind::Auth];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Permission => "permission",
            Kind::Question => "question",
            Kind::Auth => "auth",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// Collection channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `tildy agent wrap`: OSC 7501 reports read from a pty.
    Osc,
    /// `tildy agent publish`.
    #[default]
    Cli,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Osc => "osc",
            Source::Cli => "cli",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Session,
    Worktree,
    Global,
}

impl ScopeKind {
    pub const ALL: [ScopeKind; 3] = [ScopeKind::Session, ScopeKind::Worktree, ScopeKind::Global];

    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Session => "session",
            ScopeKind::Worktree => "worktree",
            ScopeKind::Global => "global",
        }
    }

    pub fn parse(s: &str) -> Option<ScopeKind> {
        ScopeKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// Where a record shows: prompts of one shell session (`TILDY_SESSION_ID`),
/// of one git worktree (`GitData.worktree_id`, see
/// `providers::git::worktree_identity`) or everywhere (`*`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub kind: ScopeKind,
    pub id: String,
}

impl Scope {
    pub fn session(id: impl Into<String>) -> Scope {
        Scope { kind: ScopeKind::Session, id: id.into() }
    }
    pub fn worktree(id: impl Into<String>) -> Scope {
        Scope { kind: ScopeKind::Worktree, id: id.into() }
    }
    pub fn global() -> Scope {
        Scope { kind: ScopeKind::Global, id: "*".into() }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.as_str(), self.id)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One agent status record. Each publish replaces the record with the same
/// scope and `id` completely (7501: keys left out of a report are gone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub protocol_version: u32,
    /// 7501 id: segments of `[A-Za-z0-9_.+-]{1,32}` joined by `/`, at most 8
    /// levels and 128 bytes. Empty: the root record.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Stable program identifier `[A-Za-z0-9_.+-]{1,32}`; a record without one
    /// inherits it from its nearest ancestor record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub state: State,
    /// `blocked` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    /// 0-100, `working`/`blocked` only; absent = indeterminate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    /// Plain UTF-8 (7501's base64 is decoded by the collector), at most 192 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Plain UTF-8, at most 2048 bytes, one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    pub scope: Scope,
    /// `TILDY_SESSION_ID` of the shell the publisher ran in: that shell's next
    /// prompt drops idle/working/blocked records, its second one done/error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session: Option<String>,
    #[serde(default)]
    pub source: Source,
    /// Monotonic per scope and id; a publish needs a larger seq than the live
    /// record it replaces. 0 on input: the store assigns previous + 1.
    #[serde(default)]
    pub seq: u64,
    /// Lifetime in seconds (1-86400); absent on input: by state (idle,
    /// working, blocked 7200; done, error 900). Stored records carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_s: Option<u32>,
    /// Wall clock of the store write (set by the store; display and expiry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_at_ms: Option<u64>,
    /// Set by the store when the origin session showed this done/error record
    /// at a prompt; the next prompt of that session drops it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub prompt_seen: bool,
}

impl Record {
    /// A record with only the required fields; the rest default to absent.
    pub fn new(state: State, scope: Scope) -> Record {
        Record {
            protocol_version: RECORD_VERSION,
            id: String::new(),
            app: None,
            state,
            kind: None,
            progress: None,
            title: None,
            msg: None,
            scope,
            origin_session: None,
            source: Source::Cli,
            seq: 0,
            ttl_s: None,
            received_at_ms: None,
            prompt_seen: false,
        }
    }

    /// Effective lifetime in seconds.
    pub fn ttl(&self) -> u32 {
        self.ttl_s.unwrap_or_else(|| self.state.default_ttl_s())
    }

    /// Unix ms at which the record expires (`received_at_ms` + TTL; a record
    /// that was never stored counts from 0, i.e. is already expired).
    pub fn expires_at_ms(&self) -> u64 {
        self.received_at_ms.unwrap_or(0).saturating_add(self.ttl() as u64 * 1000)
    }

    /// Drop the keys 7501 uses only with some states: `kind` outside
    /// `blocked`, `progress` outside `working`/`blocked`.
    pub fn normalize(&mut self) {
        if self.state != State::Blocked {
            self.kind = None;
        }
        if !matches!(self.state, State::Working | State::Blocked) {
            self.progress = None;
        }
    }

    /// Check every field against the 7501 limits and the schema. The first
    /// problem rejects the whole record.
    pub fn validate(&self) -> Result<(), Reject> {
        if self.protocol_version != RECORD_VERSION {
            return Err(invalid("protocol_version", format!("must be {RECORD_VERSION}")));
        }
        check_id(&self.id).map_err(|m| invalid("id", m))?;
        if let Some(a) = &self.app {
            check_app(a).map_err(|m| invalid("app", m))?;
        }
        if let Some(p) = self.progress {
            if p > 100 {
                return Err(invalid("progress", "must be 0-100"));
            }
        }
        if let Some(t) = &self.title {
            check_text(t, MAX_TITLE_BYTES).map_err(|m| invalid("title", m))?;
        }
        if let Some(t) = &self.msg {
            check_text(t, MAX_MSG_BYTES).map_err(|m| invalid("msg", m))?;
        }
        check_scope(&self.scope)?;
        if let Some(s) = &self.origin_session {
            check_plain(s, MAX_SESSION_BYTES).map_err(|m| invalid("origin_session", m))?;
        }
        if let Some(t) = self.ttl_s {
            if t == 0 || t > MAX_TTL_S {
                return Err(invalid("ttl_s", format!("must be 1-{MAX_TTL_S}")));
            }
        }
        Ok(())
    }
}

fn check_scope(s: &Scope) -> Result<(), Reject> {
    match s.kind {
        ScopeKind::Global if s.id != "*" => Err(invalid("scope.id", "must be \"*\" for the global scope")),
        _ => check_plain(&s.id, MAX_SCOPE_ID_BYTES).map_err(|m| invalid("scope.id", m)),
    }
}

/// Why a record or report was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    /// A field breaks a 7501 limit or the schema; nothing was stored.
    Invalid { field: String, message: String },
    /// The live record with this scope and id already has `current` >= `got`.
    StaleSeq { id: String, current: u64, got: u64 },
    /// The store could not be read or written.
    Io(String),
}

impl Reject {
    /// Stable machine code: `invalid`, `stale_seq`, `io`.
    pub fn code(&self) -> &'static str {
        match self {
            Reject::Invalid { .. } => "invalid",
            Reject::StaleSeq { .. } => "stale_seq",
            Reject::Io(_) => "io",
        }
    }

    /// The offending field for `invalid` (`seq` for `stale_seq`).
    pub fn field(&self) -> Option<&str> {
        match self {
            Reject::Invalid { field, .. } => Some(field),
            Reject::StaleSeq { .. } => Some("seq"),
            Reject::Io(_) => None,
        }
    }
}

impl fmt::Display for Reject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reject::Invalid { field, message } => write!(f, "{field}: {message}"),
            Reject::StaleSeq { id, current, got } => {
                let id = if id.is_empty() { "(root)" } else { id };
                write!(f, "seq {got} is not newer than {current} for record {id}")
            }
            Reject::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Reject {}

pub(crate) fn invalid(field: &str, message: impl Into<String>) -> Reject {
    Reject::Invalid { field: field.to_string(), message: message.into() }
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-')
}

/// 7501 id grammar; empty is the root record.
pub fn check_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Ok(());
    }
    if id.len() > MAX_ID_BYTES {
        return Err(format!("longer than {MAX_ID_BYTES} bytes"));
    }
    let segs: Vec<&str> = id.split('/').collect();
    if segs.len() > MAX_ID_DEPTH {
        return Err(format!("more than {MAX_ID_DEPTH} levels"));
    }
    for s in segs {
        if s.is_empty() || s.len() > MAX_SEGMENT_BYTES || !s.bytes().all(is_name_byte) {
            return Err(format!("segment '{s}' must be 1-{MAX_SEGMENT_BYTES} characters of [A-Za-z0-9_.+-]"));
        }
    }
    Ok(())
}

/// 7501 app grammar.
pub fn check_app(app: &str) -> Result<(), String> {
    if app.is_empty() || app.len() > MAX_APP_BYTES || !app.bytes().all(is_name_byte) {
        return Err(format!("must be 1-{MAX_APP_BYTES} characters of [A-Za-z0-9_.+-]"));
    }
    Ok(())
}

/// 7501 refuses C0 controls (incl. newline and tab), DEL and C1 controls.
pub fn is_forbidden_char(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

fn check_text(s: &str, max: usize) -> Result<(), String> {
    if s.len() > max {
        return Err(format!("longer than {max} bytes"));
    }
    if s.chars().any(is_forbidden_char) {
        return Err("contains a control character".into());
    }
    Ok(())
}

fn check_plain(s: &str, max: usize) -> Result<(), String> {
    if s.is_empty() {
        return Err("must not be empty".into());
    }
    check_text(s, max)
}

/// True when `id` is `ancestor` or below it (`ancestor/...`). The root id
/// (empty) is the ancestor of every record.
pub fn id_within(id: &str, ancestor: &str) -> bool {
    ancestor.is_empty() || id == ancestor || (id.len() > ancestor.len() && id.starts_with(ancestor) && id.as_bytes()[ancestor.len()] == b'/')
}

/// The longest prefix of `s` that fits in `max` bytes, at a char boundary.
pub fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Remove characters 7501 forbids (for collectors turning arbitrary text into
/// a `title` or `msg`).
pub fn strip_controls(s: &str) -> String {
    s.chars().filter(|c| !is_forbidden_char(*c)).collect()
}

// ------------------------------------------------------------------ base64

/// Standard base64 (RFC 4648 alphabet, padding optional). `None` when the
/// input does not decode.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    super::osc::decode_base64(s.as_bytes())
}

/// Standard base64 with padding (tests and tools producing 7501 bodies).
pub fn base64_encode(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// --------------------------------------------------------------- 7501 body

/// What a 7501 report asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Set(State),
    /// Remove the addressed record and its descendants (no id: every record
    /// of the reporter).
    Clear,
}

/// A parsed 7501 report body: the record fields a report carries. The
/// collector adds scope, origin, source and seq to make a [`Record`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub op: Op,
    pub id: String,
    pub app: Option<String>,
    pub kind: Option<Kind>,
    pub progress: Option<u8>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

impl Report {
    /// The record for a `Set` report (`None` for `Clear`), normalized.
    pub fn into_record(self, scope: Scope) -> Option<Record> {
        let Op::Set(state) = self.op else { return None };
        let mut r = Record::new(state, scope);
        r.id = self.id;
        r.app = self.app;
        r.kind = self.kind;
        r.progress = self.progress;
        r.title = self.title;
        r.msg = self.msg;
        r.normalize();
        Some(r)
    }
}

/// Parse a 7501 report body (`key=value` pairs joined by `:`; a leading
/// `7501;` is accepted). Rules from rev 0.4: whitespace around keys and values
/// is stripped; a pair without `=`, with an empty or non-lowercase key or a
/// value byte outside `[A-Za-z0-9_.,+/=-]` is skipped; unknown keys are
/// ignored; the last duplicate wins. The whole report is rejected when the
/// body or a key is over the limit, `state` is missing or unknown, `id` is
/// malformed, or `title`/`msg` do not decode or break their limits. An
/// invalid `kind`, `progress` or `app` is treated as absent.
pub fn parse_osc_body(body: &str) -> Result<Report, Reject> {
    use super::osc;
    let body = body.trim().strip_prefix("7501;").unwrap_or(body.trim());
    let parsed = osc::parse_body(body.as_bytes()).map_err(|e| {
        let field = match &e {
            osc::Reject::MissingState | osc::Reject::UnknownState(_) => "state",
            osc::Reject::BadId => "id",
            osc::Reject::TooLong("sequence" | "key") => "osc",
            osc::Reject::TooLong(field) | osc::Reject::BadBase64(field) | osc::Reject::BadUtf8(field) | osc::Reject::ControlChar(field) => field,
        };
        invalid(field, e.to_string())
    })?;
    let op = match parsed.state {
        osc::State::Clear => Op::Clear,
        state => Op::Set(State::parse(state.as_str()).expect("shared state vocabulary")),
    };
    Ok(Report {
        op,
        id: parsed.id.unwrap_or_default(),
        app: parsed.app,
        kind: parsed.kind.and_then(|k| Kind::parse(k.as_str())),
        progress: parsed.progress,
        title: parsed.title,
        msg: parsed.msg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_grammar() {
        assert!(check_id("").is_ok());
        assert!(check_id("claude-code/1a2b3c4d").is_ok());
        assert!(check_id(&["a"; 8].join("/")).is_ok());
        assert!(check_id(&["a"; 9].join("/")).is_err(), "9 levels");
        assert!(check_id(&"a".repeat(32)).is_ok());
        assert!(check_id(&"a".repeat(33)).is_err());
        assert!(check_id(&[&"a".repeat(32)[..]; 4].join("/")).is_err(), "131 bytes");
        for bad in ["a//b", "/a", "a/", "a b", "a:b", "é", "a\nb"] {
            assert!(check_id(bad).is_err(), "{bad:?}");
        }
        assert!(id_within("a/b/c", "a/b") && id_within("a/b", "a/b") && id_within("x", ""));
        assert!(!id_within("a/bc", "a/b") && !id_within("a", "a/b"));
    }

    #[test]
    fn record_limits() {
        let mut r = Record::new(State::Working, Scope::global());
        assert!(r.validate().is_ok());
        r.msg = Some("x".repeat(2048));
        assert!(r.validate().is_ok());
        r.msg = Some("x".repeat(2049));
        assert_eq!(r.validate().unwrap_err().field(), Some("msg"));
        r.msg = Some("日".repeat(683)); // 2049 bytes
        assert!(r.validate().is_err());
        r.msg = Some("tab\there".into());
        assert_eq!(r.validate().unwrap_err().code(), "invalid");
        for c in ['\u{0}', '\n', '\u{1b}', '\u{7f}', '\u{85}', '\u{9f}'] {
            r.msg = Some(format!("a{c}b"));
            assert!(r.validate().is_err(), "{c:?}");
        }
        r.msg = Some("ok \u{a0} 日本".into());
        assert!(r.validate().is_ok());
        r.title = Some("t".repeat(193));
        assert_eq!(r.validate().unwrap_err().field(), Some("title"));
        r.title = None;
        r.app = Some("a".repeat(33));
        assert_eq!(r.validate().unwrap_err().field(), Some("app"));
        r.app = Some("claude-code".into());
        r.ttl_s = Some(0);
        assert_eq!(r.validate().unwrap_err().field(), Some("ttl_s"));
        r.ttl_s = Some(86_401);
        assert!(r.validate().is_err());
        r.ttl_s = Some(86_400);
        r.progress = Some(101);
        assert_eq!(r.validate().unwrap_err().field(), Some("progress"));
        r.progress = Some(100);
        r.scope = Scope { kind: ScopeKind::Global, id: "x".into() };
        assert_eq!(r.validate().unwrap_err().field(), Some("scope.id"));
        r.scope = Scope::session("");
        assert!(r.validate().is_err());
        r.scope = Scope::session("123.456");
        assert!(r.validate().is_ok());
        r.protocol_version = 2;
        assert_eq!(r.validate().unwrap_err().field(), Some("protocol_version"));
    }

    #[test]
    fn normalize_and_defaults() {
        let mut r = Record::new(State::Done, Scope::global());
        r.kind = Some(Kind::Auth);
        r.progress = Some(50);
        r.normalize();
        assert_eq!((r.kind, r.progress), (None, None));
        assert_eq!(r.ttl(), 900);
        let mut r = Record::new(State::Blocked, Scope::global());
        r.kind = Some(Kind::Question);
        r.progress = Some(5);
        r.normalize();
        assert_eq!((r.kind, r.progress), (Some(Kind::Question), Some(5)));
        assert_eq!(r.ttl(), 7200);
        r.received_at_ms = Some(1000);
        assert_eq!(r.expires_at_ms(), 7_201_000);
    }

    #[test]
    fn serde_round_trip_and_unknown_fields() {
        let mut r = Record::new(State::Blocked, Scope::worktree("/r/.git@/r"));
        r.id = "wrap/claude/42".into();
        r.kind = Some(Kind::Permission);
        r.msg = Some("Bash: rm -rf target".into());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["state"], "blocked");
        assert_eq!(v["scope"]["kind"], "worktree");
        assert!(v.get("prompt_seen").is_none() && v.get("title").is_none());
        assert_eq!(serde_json::from_value::<Record>(v).unwrap(), r);
        let e = serde_json::from_str::<Record>(r#"{"protocol_version":1,"state":"idle","scope":{"kind":"global","id":"*"},"colour":1}"#);
        assert!(e.is_err());
        let e = serde_json::from_str::<Record>(r#"{"protocol_version":1,"state":"clear","scope":{"kind":"global","id":"*"}}"#);
        assert!(e.is_err(), "clear is not a state");
    }

    /// fixtures/agent-state.*.json: valid samples load and validate; invalid
    /// ones are refused, except `kind`/`progress` on the wrong state, which a
    /// publish drops (7501 "used only with") instead of refusing.
    #[test]
    fn schema_fixtures_agree() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let mut seen = 0;
        for e in std::fs::read_dir(&dir).unwrap() {
            let name = e.unwrap().file_name().into_string().unwrap();
            let Some(rest) = name.strip_prefix("agent-state.") else { continue };
            seen += 1;
            let text = std::fs::read_to_string(dir.join(&name)).unwrap();
            let parsed = serde_json::from_str::<Record>(&text).map_err(|e| e.to_string()).and_then(|r| r.validate().map(|_| r).map_err(|e| e.to_string()));
            if rest.starts_with("valid") {
                parsed.unwrap_or_else(|e| panic!("{name}: {e}"));
            } else if name.contains("progress-on-done") || name == "agent-state.invalid.json" {
                let mut r = parsed.unwrap_or_else(|e| panic!("{name}: {e}"));
                r.normalize();
                assert!(r.kind.is_none() && r.progress.is_none(), "{name}");
            } else {
                assert!(parsed.is_err(), "{name} should be refused");
            }
        }
        assert!(seen >= 10);
    }

    #[test]
    fn base64() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar", "日本語 ✔"] {
            let e = base64_encode(s.as_bytes());
            assert_eq!(base64_decode(&e).unwrap(), s.as_bytes(), "{s}");
            assert_eq!(base64_decode(e.trim_end_matches('=')).unwrap(), s.as_bytes(), "unpadded {s}");
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        for bad in ["Z", "Zm9v!", "Zm=8", "Zm8===", "Zm8=Zm8=", "Zg="] {
            assert_eq!(base64_decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn osc_bodies() {
        let msg = base64_encode(b"Bash: rm -rf target");
        let r = parse_osc_body(&format!("state=blocked:kind=permission:msg={msg}:app=claude")).unwrap();
        assert_eq!(r.op, Op::Set(State::Blocked));
        assert_eq!((r.kind, r.app.as_deref(), r.msg.as_deref()), (Some(Kind::Permission), Some("claude"), Some("Bash: rm -rf target")));
        // Leading 7501;, whitespace, malformed pairs, unknown keys, duplicates.
        let r = parse_osc_body(" 7501;state = working : progress=40 :junk: =x : Key=1 :future=1:state=done:x=a;b").unwrap();
        assert_eq!(r.op, Op::Set(State::Done));
        assert_eq!(r.progress, None, "the shared parser normalizes fields unused by done");
        // Lenient keys: invalid kind/progress/app are absent.
        let r = parse_osc_body("state=blocked:kind=bribe:progress=101:app=a b").unwrap();
        assert_eq!((r.kind, r.progress, r.app), (None, None, None));
        let r = parse_osc_body("state=working:progress=-1:app=x/y").unwrap();
        assert_eq!((r.progress, r.app), (None, None));
        assert_eq!(parse_osc_body("state=clear:id=a/b").unwrap(), Report { op: Op::Clear, id: "a/b".into(), app: None, kind: None, progress: None, title: None, msg: None });
        // Whole-report rejections.
        assert_eq!(parse_osc_body("kind=auth").unwrap_err().field(), Some("state"));
        assert_eq!(parse_osc_body("state=sleeping").unwrap_err().field(), Some("state"));
        assert_eq!(parse_osc_body("state=idle:id=a//b").unwrap_err().field(), Some("id"));
        assert_eq!(parse_osc_body("state=idle:msg=!!!").map(|r| r.msg), Ok(None), "! is not a value byte: pair skipped");
        assert_eq!(parse_osc_body("state=idle:msg=Zm9v!").map(|r| r.msg), Ok(None));
        assert_eq!(parse_osc_body("state=idle:msg=Zg").unwrap().msg.as_deref(), Some("f"));
        assert_eq!(parse_osc_body("state=idle:msg=Z").unwrap_err().field(), Some("msg"));
        let nl = base64_encode(b"two\nlines");
        assert_eq!(parse_osc_body(&format!("state=idle:msg={nl}")).unwrap_err().field(), Some("msg"));
        let c1 = base64_encode("x\u{85}".as_bytes());
        assert_eq!(parse_osc_body(&format!("state=idle:title={c1}")).unwrap_err().field(), Some("title"));
        let latin1 = base64_encode(&[0xff, 0xfe]);
        assert_eq!(parse_osc_body(&format!("state=idle:msg={latin1}")).unwrap_err().field(), Some("msg"));
        let long = base64_encode("t".repeat(193).as_bytes());
        assert_eq!(parse_osc_body(&format!("state=idle:title={long}")).unwrap_err().field(), Some("title"));
        let ok = base64_encode("m".repeat(2048).as_bytes());
        assert!(parse_osc_body(&format!("state=idle:msg={ok}")).is_ok());
        assert!(parse_osc_body(&format!("state=idle:{}=1", "k".repeat(17))).is_err());
        assert!(parse_osc_body(&format!("state=idle:x={}", "a".repeat(4096))).is_err());
        // into_record normalizes.
        let rec = parse_osc_body("state=done:kind=auth:progress=3:id=t").unwrap().into_record(Scope::global()).unwrap();
        assert_eq!((rec.kind, rec.progress, rec.id.as_str()), (None, None, "t"));
        assert!(parse_osc_body("state=clear").unwrap().into_record(Scope::global()).is_none());
    }
}
