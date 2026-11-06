//! Shared types across modules. Protocol and schema v1 remain additive; new
//! serialized fields must default when older backends or fixtures omit them.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

pub const PROTOCOL_VERSION: u32 = 1;
pub const API_VERSION: u32 = 1;
pub const SCHEMA_VERSION: u32 = 1;
pub const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// One prompt request from the shell (REQ frame, protocol v1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRequest {
    pub gen: u64,
    pub session_id: String,
    pub cwd: PathBuf,
    pub cols: u16,
    pub lines: u16,
    pub last_exit: i32,
    pub last_duration_ms: u64,
    /// Environment values the shell forwards: `KEY=VALUE` pairs joined by U+001F
    /// (SSH_CONNECTION, SSH_TTY, USER, HOST, VIRTUAL_ENV, CONDA_DEFAULT_ENV,
    /// DOCKER_CONTEXT, DOCKER_HOST). Also used as the env cache revision.
    pub env: String,
    /// Number of background jobs in the shell (`${#jobstates}`); 0 when absent.
    #[serde(default)]
    pub jobs: u32,
}

impl PromptRequest {
    /// Parse the `env` field into a map. Pairs without `=` are ignored.
    pub fn env_map(&self) -> BTreeMap<String, String> {
        self.env
            .split('\u{1f}')
            .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .collect()
    }
}

/// Freshness of a provider slot as seen by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Pending,
    Fresh,
    Stale,
    Timeout,
    Error,
}

/// A provider value together with its freshness and age.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot<T> {
    pub freshness: Freshness,
    pub age_ms: Option<u64>,
    pub value: Option<T>,
    pub error: Option<String>,
}

impl<T> Slot<T> {
    pub fn pending() -> Self {
        Slot { freshness: Freshness::Pending, age_ms: None, value: None, error: None }
    }
    pub fn fresh(value: T) -> Self {
        Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: Some(value), error: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum GitState {
    Clean,
    Dirty,
    Conflicted,
    #[default]
    Unknown,
}

/// Structured Git status, from `git --no-optional-locks status --porcelain=v2 -z --branch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GitData {
    pub in_repo: bool,
    pub git_dir: Option<PathBuf>,
    pub worktree_root: Option<PathBuf>,
    /// Stable identity of this worktree (e.g. canonical git_dir path).
    pub worktree_id: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub unborn: bool,
    pub head_short: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicted: u32,
    /// Renamed/copied entries (porcelain `2` records), also counted in staged/unstaged.
    #[serde(default)]
    pub renamed: u32,
    /// Deleted entries (X or Y == 'D'), also counted in staged/unstaged.
    #[serde(default)]
    pub deleted: u32,
    /// Entries whose file type changed (porcelain X/Y T).
    #[serde(default)]
    pub typechanged: u32,
    /// Stash entries (`refs/stash` reflog count).
    #[serde(default)]
    pub stashed: u32,
    /// In-progress operation detected from the git dir: "merge", "rebase",
    /// "cherry-pick", "revert", "bisect", or "am"; None when idle.
    #[serde(default)]
    pub operation: Option<String>,
    /// Progress for rebase as (current, total) when the git dir records it.
    #[serde(default)]
    pub operation_progress: Option<(u32, u32)>,
    /// True when untracked scan or submodule scan was skipped or cut short.
    pub incomplete: bool,
    pub state: GitState,
}


/// Everything the renderer needs for one prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptData {
    pub request: PromptRequest,
    pub home: Option<PathBuf>,
    pub git: Slot<GitData>,
    pub ssh: bool,
    pub hostname: Option<String>,
    pub username: Option<String>,
    /// Config-driven module data keyed by module id (command, python,
    /// docker_context, starship bridge). A module that was not detected in this
    /// directory has a `Fresh` slot with `value: None`.
    #[serde(default, alias = "commands")]
    pub modules: BTreeMap<String, Slot<ModuleValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<ChangeData>,
}

/// Idle changes since the accepted display baseline; never persisted in instant caches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChangeData {
    pub branch_from: Option<String>,
    pub branch_to: Option<String>,
    pub files_added: u32,
    pub environments: Vec<String>,
}

/// Additive protocol-v1 metadata. The shell chooses the contextual collapse only
/// when its previous submitted command had a different stable context key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMetadata {
    pub context_key: String,
    pub context_transient: Option<String>,
    pub context_suffix: Option<String>,
    pub summary: bool,
    pub summary_threshold_ms: u64,
}

/// Data produced by a config-driven provider for one module instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModuleValue {
    /// Primary display text (e.g. a version like "22.3.0", a context name).
    pub text: String,
    /// Named fields for `format` templates (e.g. version, env, context).
    pub fields: BTreeMap<String, String>,
    /// For the starship bridge: pre-rendered Zsh prompt text and its column width.
    pub prerendered: Option<(String, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderStatus {
    Fresh,
    Partial,
}

/// Rendered prompt in Zsh prompt-escape syntax. Each string is one line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedPrompt {
    pub status: RenderStatus,
    pub left: Vec<String>,
    pub right: Option<String>,
    /// Transient (collapsed) form of the prompt shown after a command is accepted.
    pub transient: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_seq: Option<u64>,
}

/// Per-module explanation emitted alongside a render (used by preview/explain).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleReport {
    pub id: String,
    pub module_type: String,
    pub visible: bool,
    pub hidden_reason: Option<String>,
    pub text: String,
    pub columns: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_columns: Option<usize>,
    pub rules_hit: Vec<String>,
    pub freshness: Option<Freshness>,
    /// Where the module definition comes from: `builtin`, `preset:<name>` or
    /// `user`. Filled by `render::report::render_annotated` (preview), not by
    /// the prompt path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Age of the provider data in milliseconds (preview annotation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
    /// Final style after rules, e.g. `fg=yellow bold` (preview annotation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Zero-based layout row (preview annotation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<usize>,
    /// `left` or `right` (preview annotation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
}

/// Dependencies a provider declares; the scheduler invalidates cache on change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dependency {
    Cwd,
    Env,
    Time,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderSpec {
    pub key: &'static str,
    pub deps: &'static [Dependency],
    pub ttl: Duration,
    pub timeout: Duration,
}

/// Unified CLI result envelope (`--json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliEnvelope<T> {
    pub api_version: u32,
    pub ok: bool,
    pub data: Option<T>,
    pub error: CliError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliError {
    pub code: ErrorCode,
    pub path: Option<String>,
    pub message: Option<String>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Ok,
    ValidationFailed,
    RevisionConflict,
    EnvironmentUnavailable,
    BackendUnavailable,
    InternalError,
}

impl ErrorCode {
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::Ok => 0,
            ErrorCode::ValidationFailed => 2,
            ErrorCode::RevisionConflict => 3,
            ErrorCode::EnvironmentUnavailable => 4,
            ErrorCode::BackendUnavailable => 5,
            ErrorCode::InternalError => 1,
        }
    }
}

impl<T> CliEnvelope<T> {
    pub fn ok(data: T) -> Self {
        CliEnvelope {
            api_version: API_VERSION,
            ok: true,
            data: Some(data),
            error: CliError { code: ErrorCode::Ok, path: None, message: None, hint: None },
        }
    }
    pub fn err(code: ErrorCode, message: impl Into<String>, hint: Option<String>) -> Self {
        CliEnvelope {
            api_version: API_VERSION,
            ok: false,
            data: None,
            error: CliError { code, path: None, message: Some(message.into()), hint },
        }
    }
}
