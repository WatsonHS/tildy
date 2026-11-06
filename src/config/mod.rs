//! Config loading, built-in defaults, presets, symbol tiers. Owner: M1b task A.
//!
//! User documents are built from three layers; runtime callers may then apply
//! the restricted repository layer in `project`:
//!
//! 1. `builtin.json` (shared defaults);
//! 2. the user's `preset`, or `pastel-powerline` when omitted (`presets/<name>.json`). A
//!    preset is a complete look: its `modules` and `layout` *replace* the
//!    built-in ones, other keys merge;
//! 3. the user file, deep-merged (objects merge by key, arrays replace). A user
//!    module that sets `type` is a full definition and replaces the module of
//!    the same id; one without `type` is a partial override merged into it.
//!
//! The result is validated by a hand-written validator (`validate.rs`, errors
//! carry JSON paths), then deserialized into typed structs. The merged raw JSON
//! (including every `$comment`) is kept in `Config::json`, and the untouched
//! user layer in `Config::user`.
use crate::model::SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

pub mod palette;
pub mod project;
pub mod template;
pub mod transaction;
mod validate;
pub(crate) use validate::{parse_time_range, parse_utc_offset};

pub use palette::{Palette, PaletteInfo, PALETTE_INFO, ROLES};

pub const MODULE_TYPES: &[&str] = &[
    "directory", "git", "exit_status", "duration", "hostname", "agent_status", "changes", "command",
    "character", "time", "username", "jobs", "python", "docker_context", "language", "starship",
    "text", "os", "fill", "env_var", "package", "git_commit", "git_metrics", "shlvl", "localip", "memory_usage", "sudo",
    "direnv", "nix_shell", "shell",
];
/// Languages known to `type: "language"` (symbol defaults here; detection
/// rules and version commands live with the provider, see docs/modules.md).
pub const LANGUAGES: &[&str] = &["nodejs", "rust", "golang", "java", "c", "python"];
pub const ROW_STYLES: &[&str] = &["plain", "powerline"];
/// `time_format` conversion specifiers supported by the renderer.
pub const TIME_SPECIFIERS: &[char] = &['R', 'H', 'M', 'S', 'I', 'p', 'Y', 'm', 'd', 'F', 'T', 'r', '%'];
/// Shells the `shell` module knows indicators for (Starship's list).
pub const SHELL_NAMES: &[&str] =
    &["bash", "fish", "zsh", "powershell", "pwsh", "ion", "elvish", "tcsh", "xonsh", "cmd", "nu"];
pub const OPERATORS: &[&str] = &["eq", "ne", "gt", "lt", "ge", "le", "in"];
pub const SYMBOL_TIERS: &[&str] = &["unicode", "nerd", "ascii"];
pub const SYMBOL_SETTINGS: &[&str] = &["auto", "unicode", "nerd", "ascii"];
pub const HISTORY_MODES: &[&str] = &["compact", "full"];
/// `agent_status` states (OSC 7501 vocabulary), keys of `state_symbols`.
pub const AGENT_STATES: &[&str] = &["idle", "working", "done", "blocked", "error"];
/// Default `agent_status` `max_width` (columns of `$msg`).
pub const AGENT_DEFAULT_MAX_WIDTH: u32 = 40;
/// Agent programs the Zsh adapter runs through `tildy agent wrap` unless the
/// config sets `agent.programs` (see docs/agent-status.md).
pub const AGENT_DEFAULT_PROGRAMS: &[&str] = &["claude", "codex", "gemini", "aider", "opencode", "amp", "goose", "copilot", "cursor-agent"];

const BUILTIN_JSON: &str = include_str!("builtin.json");
/// The config JSON Schema (`schema/config.v1.schema.json`), printed by `tildy schema`.
pub const SCHEMA_V1_JSON: &str = include_str!("../../schema/config.v1.schema.json");

/// A built-in preset (`src/config/presets/<name>.json`).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PresetInfo {
    pub name: &'static str,
    /// One-line description (Chinese, shown by `tildy preset list`).
    pub description: &'static str,
    /// Nerd Font requirement: `required` (the preset pins the nerd symbol
    /// tier), `optional` (icons in the nerd tier, fallbacks elsewhere) or `no`.
    pub nerd_font: &'static str,
    /// Where the look comes from.
    pub source: &'static str,
    #[serde(skip)]
    pub json: &'static str,
}

impl PresetInfo {
    /// The default palette of a role-based preset (its `palette` key); None
    /// for the presets with fixed colors.
    pub fn palette(&self) -> Option<String> {
        serde_json::from_str::<Value>(self.json).ok().and_then(|v| v.get("palette").and_then(Value::as_str).map(str::to_string))
    }

    /// The preset's pinned `symbols` setting (`auto` when it pins none).
    pub fn symbols(&self) -> String {
        serde_json::from_str::<Value>(self.json)
            .ok()
            .and_then(|v| v.get("symbols").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "auto".into())
    }
}

macro_rules! preset {
    ($name:literal, $nerd:literal, $source:literal, $desc:literal) => {
        PresetInfo { name: $name, description: $desc, nerd_font: $nerd, source: $source, json: include_str!(concat!("presets/", $name, ".json")) }
    };
}

/// The zero-config preset. Explicit preset and layout choices still override it.
pub const DEFAULT_PRESET: &str = "pastel-powerline";

/// Built-in presets in display order.
pub const PRESET_INFO: &[PresetInfo] = &[
    preset!("plain", "optional", "tildy", "Plain single-line layout without background colors; icons with Nerd Font"),
    preset!("pastel-powerline", "required", "starship 1.24.2 pastel-powerline (author variant)", "Pastel powerline with user, directory, Git, languages and time; prompt on a second row"),
    preset!("tokyo-night", "required", "starship 1.24.2 tokyo-night", "Tokyo Night blue powerline with shaded start, Apple icon and rounded separators; two rows"),
    preset!("gruvbox-rainbow", "required", "starship 1.24.2 gruvbox-rainbow", "Gruvbox rainbow with OS, user, directory, Git, languages, conda and time; two rows"),
    preset!("catppuccin-powerline", "required", "starship 1.24.2 catppuccin-powerline (mocha)", "Catppuccin Mocha dark powerline with duration and prompt on one row"),
    preset!("catppuccin-latte", "required", "starship 1.24.2 catppuccin-powerline (latte)", "Catppuccin Latte powerline for light terminals; one row"),
    preset!("catppuccin-frappe", "required", "starship 1.24.2 catppuccin-powerline (frappe)", "Catppuccin Frappe medium-dark powerline; one row"),
    preset!("catppuccin-macchiato", "required", "starship 1.24.2 catppuccin-powerline (macchiato)", "Catppuccin Macchiato dark powerline; one row"),
    preset!("jetpack", "no", "starship 1.24.2 jetpack", "Jetpack minimal left prompt, with directory, Git, languages and time on the right; no Nerd Font needed"),
    preset!("pure-preset", "no", "starship 1.24.2 pure-preset", "Pure-style directory, muted Git branch, dirty marker and duration; purple prompt on a second row"),
    preset!("bracketed-segments", "optional", "starship 1.24.2 bracketed-segments", "Starship-style modules in adjacent brackets"),
    preset!("nerd-font-symbols", "required", "starship 1.24.2 nerd-font-symbols", "Starship-style layout with Nerd Font icons for languages and tools"),
    preset!("no-empty-icons", "optional", "starship 1.24.2 no-empty-icons", "Starship-style layout; hides icons when tools are unavailable"),
    preset!("no-nerd-font", "no", "starship 1.24.2 no-nerd-font", "Starship-style layout using Unicode symbols; no Nerd Font needed"),
    preset!("no-runtime-versions", "optional", "starship 1.24.2 no-runtime-versions", "Starship-style layout showing language icons without versions"),
    preset!("plain-text-symbols", "no", "starship 1.24.2 plain-text-symbols", "Starship-style layout using ASCII text symbols"),
    preset!("powerline", "optional", "tildy (palette-based pastel-powerline)", "Palette-based powerline with user, directory, Git, languages and time; two rows"),
    preset!("lean", "optional", "tildy (Powerlevel10k lean layout)", "p10k-style lean layout without backgrounds; directory and Git left, status and time right; two rows"),
    preset!("classic", "optional", "tildy (Powerlevel10k classic layout)", "p10k-style classic powerline with neutral backgrounds, status and time on the right; two rows"),
    preset!("rainbow", "optional", "tildy (Powerlevel10k rainbow layout)", "p10k-style rainbow powerline with colored segments, status and time on the right; two rows"),
    preset!("minimal", "optional", "tildy", "Minimal single-line directory, Git branch, status and prompt"),
];

/// Names of the built-in presets (same order as `PRESET_INFO`).
pub const PRESETS: &[&str] = &[
    "plain", "pastel-powerline", "tokyo-night", "gruvbox-rainbow", "catppuccin-powerline", "catppuccin-latte", "catppuccin-frappe",
    "catppuccin-macchiato", "jetpack", "pure-preset", "bracketed-segments", "nerd-font-symbols", "no-empty-icons", "no-nerd-font",
    "no-runtime-versions", "plain-text-symbols", "powerline", "lean", "classic", "rainbow", "minimal",
];

/// The role-based presets (colored by palette roles; see `palette::ROLES`).
pub const ROLE_PRESETS: &[&str] = &["powerline", "lean", "classic", "rainbow", "minimal"];

/// Raw JSON of a built-in preset.
pub fn preset_json(name: &str) -> Option<&'static str> {
    PRESET_INFO.iter().find(|p| p.name == name).map(|p| p.json)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolTier {
    Unicode,
    Nerd,
    Ascii,
}

impl SymbolTier {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolTier::Unicode => "unicode",
            SymbolTier::Nerd => "nerd",
            SymbolTier::Ascii => "ascii",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolSetting {
    #[default]
    Auto,
    Unicode,
    Nerd,
    Ascii,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleType {
    Directory,
    Git,
    ExitStatus,
    Duration,
    Hostname,
    AgentStatus,
    Changes,
    Command,
    Character,
    Time,
    Username,
    Jobs,
    Python,
    DockerContext,
    Language,
    Starship,
    Text,
    Os,
    Fill,
    EnvVar,
    Package,
    GitCommit,
    GitMetrics,
    Shlvl,
    Localip,
    MemoryUsage,
    Sudo,
    Direnv,
    NixShell,
    Shell,
}

impl ModuleType {
    pub fn as_str(self) -> &'static str {
        match self {
            ModuleType::Directory => "directory",
            ModuleType::Git => "git",
            ModuleType::ExitStatus => "exit_status",
            ModuleType::Duration => "duration",
            ModuleType::Hostname => "hostname",
            ModuleType::AgentStatus => "agent_status",
            ModuleType::Changes => "changes",
            ModuleType::Command => "command",
            ModuleType::Character => "character",
            ModuleType::Time => "time",
            ModuleType::Username => "username",
            ModuleType::Jobs => "jobs",
            ModuleType::Python => "python",
            ModuleType::DockerContext => "docker_context",
            ModuleType::Language => "language",
            ModuleType::Starship => "starship",
            ModuleType::Text => "text",
            ModuleType::Os => "os",
            ModuleType::Fill => "fill",
            ModuleType::EnvVar => "env_var",
            ModuleType::Package => "package",
            ModuleType::GitCommit => "git_commit",
            ModuleType::GitMetrics => "git_metrics",
            ModuleType::Shlvl => "shlvl",
            ModuleType::Localip => "localip",
            ModuleType::MemoryUsage => "memory_usage",
            ModuleType::Sudo => "sudo",
            ModuleType::Direnv => "direnv",
            ModuleType::NixShell => "nix_shell",
            ModuleType::Shell => "shell",
        }
    }

    /// Types whose data comes from `PromptData.modules[id]` (config-driven providers).
    /// `git_commit` shows the hash from the git data; its provider (tags,
    /// hashes longer than 7) exists only when the module asks for those.
    /// `shlvl`, `nix_shell` and `shell` read the request env at render time.
    pub fn is_provider_backed(self) -> bool {
        matches!(
            self,
            ModuleType::Command
                | ModuleType::Python
                | ModuleType::DockerContext
                | ModuleType::Language
                | ModuleType::Starship
                | ModuleType::AgentStatus
                | ModuleType::Package
                | ModuleType::GitCommit
                | ModuleType::GitMetrics
                | ModuleType::Localip
                | ModuleType::MemoryUsage
                | ModuleType::Sudo
                | ModuleType::Direnv
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    In,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub field: String,
    pub op: Op,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Style {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<bool>,
}

impl Style {
    /// Later settings win; unset fields keep the earlier value.
    pub fn overlay(&mut self, other: &Style) {
        if other.foreground.is_some() { self.foreground = other.foreground.clone(); }
        if other.background.is_some() { self.background = other.background.clone(); }
        if other.bold.is_some() { self.bold = other.bold; }
        if other.dim.is_some() { self.dim = other.dim; }
        if other.italic.is_some() { self.italic = other.italic; }
        if other.underline.is_some() { self.underline = other.underline; }
    }
}

/// A symbol-like string: either one value for every tier, or per-tier values.
/// A missing tier falls back downwards (nerd -> unicode -> ascii); a tier set
/// to `""` means "nothing" and does not fall back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Sym {
    Plain(String),
    Tiered {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nerd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unicode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ascii: Option<String>,
    },
}

impl Sym {
    pub fn resolve(&self, tier: SymbolTier) -> &str {
        match self {
            Sym::Plain(s) => s,
            Sym::Tiered { nerd, unicode, ascii } => {
                let chain: &[&Option<String>] = match tier {
                    SymbolTier::Nerd => &[nerd, unicode, ascii],
                    SymbolTier::Unicode => &[unicode, ascii],
                    SymbolTier::Ascii => &[ascii],
                };
                chain.iter().find_map(|o| o.as_deref()).unwrap_or("")
            }
        }
    }
}

/// Git status symbols (`status_symbols`). Unset entries use the defaults in
/// `render` (Starship's: `= ⇡ ⇣ ⇕ ? $ ! + » ✘`, with ASCII fallbacks).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StatusSymbols {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicted: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diverged: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untracked: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stashed: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typechanged: Option<Sym>,
    /// `$ahead_behind` when the branch has an upstream and is neither ahead
    /// nor behind (Starship's `up_to_date`; default empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_to_date: Option<Sym>,
}

/// Values of `GitData.operation`, with Starship's `git_state` labels.
pub const GIT_OPERATIONS: &[(&str, &str)] = &[
    ("merge", "MERGING"),
    ("rebase", "REBASING"),
    ("cherry-pick", "CHERRY-PICKING"),
    ("revert", "REVERTING"),
    ("bisect", "BISECTING"),
    ("am", "AM"),
];

pub const STATUS_SYMBOL_KEYS: &[&str] =
    &["conflicted", "ahead", "behind", "diverged", "untracked", "stashed", "modified", "staged", "renamed", "deleted", "typechanged", "up_to_date"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub when: Condition,
    #[serde(default)]
    pub style: Style,
}

fn default_priority() -> i64 { 50 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleConfig {
    #[serde(rename = "type")]
    pub module_type: ModuleType,
    #[serde(default = "default_priority")]
    pub priority: i64,
    #[serde(default)]
    pub style: Style,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_when: Option<Condition>,
    /// Text template: `$var`, `${var}`, `(...)` optional groups, `\\` escapes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<Sym>,
    /// Powerline rows: pad the module with one space on each side (default
    /// true; `text` modules default to false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding: Option<bool>,
    /// Spaces appended to a nonempty symbol (0..=4). Default preserves the symbol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_padding: Option<u8>,

    // character
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_style: Option<Style>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_style: Option<Style>,
    /// Symbol after a failed command (default: `symbol`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_symbol: Option<Sym>,
    // text
    /// `type: "text"`: the literal (may contain `[...](style)` spans).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Sym>,
    // os
    /// `type: "os"`: symbol per operating system (`std::env::consts::OS`
    /// names: `macos`, `linux`, `windows`, ...); overrides the built-in table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_symbols: Option<BTreeMap<String, Sym>>,
    // env_var
    /// `type: "env_var"`: the environment variable to show (only variables the
    /// shell forwards in the request are available, e.g. CONDA_DEFAULT_ENV).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variable: Option<String>,
    // jobs
    /// `type: "jobs"`: show the count only from this many jobs (default 1;
    /// Starship's is 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number_threshold: Option<u32>,
    // username
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_always: Option<bool>,
    // time
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_format: Option<String>,
    // directory
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_symbol: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_symbol: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub substitutions: Option<BTreeMap<String, Sym>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncate_to_repo: Option<bool>,
    /// Format used inside a repository whose root is shown (Starship's
    /// `repo_root_format`): `$before_root_path`, `$repo_root`, `$path` (the
    /// part below the root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root_format: Option<String>,
    // git
    /// Branches whose name is hidden (`$branch` and `$branch_symbol` empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_branches: Option<Vec<String>>,
    /// Hide the branch name on a detached HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_attached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_symbol: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_symbols: Option<StatusSymbols>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_counts: Option<bool>,
    /// Show the in-progress operation (merge, rebase, ...) like Starship's
    /// `git_state`, e.g. `(REBASING 2/5)`. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_state: Option<bool>,
    /// Label per operation (keys: `GIT_OPERATIONS`); unset keys use Starship's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_labels: Option<BTreeMap<String, String>>,
    /// Style of the state label inside the git module. Default bold yellow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_style: Option<Style>,
    // language
    /// `type: "language"`: key into the built-in language table (`LANGUAGES`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// language/python/command: how `$version` is shown; `${raw}`, `${major}`,
    /// `${minor}`, `${patch}`. Default `v${raw}` (Starship's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_format: Option<String>,
    // starship bridge
    /// `type: "starship"`: the Starship module name (`starship module <name>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// Pinned source TOML for an imported Starship bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starship_config: Option<PathBuf>,
    // provider-backed detection (command/language/python/docker_context);
    // overrides the built-in detection table of the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_files: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_folders: Option<Vec<String>>,
    /// Command modules: the command to run (required).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    /// Command modules: regex applied to the command output; the first capture
    /// group (or the whole match) becomes `text`/`fields.version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    /// Provider-backed modules: per-run timeout and cache TTL overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl_s: Option<u64>,
    // agent_status
    /// `type: "agent_status"`: `$symbol` per state of the highlighted record
    /// (keys: `AGENT_STATES`); unset states use the built-in tiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_symbols: Option<BTreeMap<String, Sym>>,
    /// `type: "agent_status"`: `$msg` is cut to this many columns (default
    /// `AGENT_DEFAULT_MAX_WIDTH`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,

    // ---- Starship option parity (docs/starship-options.md)
    // directory
    /// Symbol for a read-only cwd, shown by `$read_only` (default 🔒).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<Sym>,
    /// Style of `$read_only` (default red).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_style: Option<Style>,
    /// Truncated components are abbreviated to this many characters instead
    /// of being dropped (0 = off; ignored when `substitutions` are set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fish_style_pwd_dir_length: Option<u32>,
    /// true (default): the shell's logical cwd; false: symlinks resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_logical_path: Option<bool>,
    /// Style of `$repo_root`; setting it (or `before_repo_root_style`) turns
    /// on the repository-root form (`repo_root_format`, or `format` with
    /// `$path` split into `$before_root_path$repo_root$path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root_style: Option<Style>,
    /// Style of `$before_root_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_repo_root_style: Option<Style>,
    // git
    /// Show `$remote_branch` even when it equals the local branch name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_show_remote: Option<bool>,
    /// git / git_metrics: `--ignore-submodules` for status and diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_submodules: Option<bool>,
    // duration
    /// Hide below this many milliseconds (default 0: any non-zero duration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_time: Option<u64>,
    /// `$human` includes milliseconds (like `$human_ms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_milliseconds: Option<bool>,
    // hostname / username
    /// hostname: cut the name at the first occurrence (default `.`; `""` =
    /// full name, from the shell's `HOST`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_at: Option<String>,
    /// hostname / username: replace a name by another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<BTreeMap<String, String>>,
    /// hostname / username / direnv: forwarded env variables that show the
    /// module (`!NAME` hides it when NAME is set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_env_vars: Option<Vec<String>>,
    // time
    /// Default `time_format` becomes `%r` (12-hour clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_12hr: Option<bool>,
    /// `local` (default) or hours east of UTC (`-5`, `5.5`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utc_time_offset: Option<String>,
    /// `HH:MM:SS-HH:MM:SS` (either side may be empty): show only inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_range: Option<String>,
    // exit_status
    /// Symbol after success; when non-empty the module also shows on 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_symbol: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_executable_symbol: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_found_symbol: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sigint_symbol: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_symbol: Option<Sym>,
    /// Use the not_executable/not_found/sigint/signal symbols (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_symbol: Option<bool>,
    /// Codes above 128 are signals (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recognize_signal_code: Option<bool>,
    // jobs
    /// Show `$symbol` from this many jobs (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_threshold: Option<u32>,
    // package
    /// Show the version of packages marked private (package.json).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_private: Option<bool>,
    // git_commit
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_hash_length: Option<u32>,
    /// Show only on a detached HEAD (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_detached: Option<bool>,
    /// Default true; false runs `git describe --tags` for `$tag`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_disabled: Option<bool>,
    /// Commits a tag may be behind HEAD (default 0: exact match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_max_candidates: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_symbol: Option<Sym>,
    // git_metrics
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_style: Option<Style>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_style: Option<Style>,
    /// Default true: `$added`/`$deleted` are empty when 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_nonzero_diffs: Option<bool>,
    // shlvl / memory_usage
    /// shlvl: show from this level (default 2). memory_usage: show from this
    /// percentage of RAM in use (default 75; -1 = always).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<i64>,
    /// shlvl: repeat `$symbol` SHLVL times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_offset: Option<u32>,
    // localip
    /// Show only in an SSH session (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_only: Option<bool>,
    // direnv
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_msg: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_allowed_msg: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_msg: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded_msg: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unloaded_msg: Option<Sym>,
    // nix_shell
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pure_msg: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impure_msg: Option<Sym>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_msg: Option<Sym>,
    /// Treat a PATH with `/nix/store` entries as a `nix shell` (state unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heuristic: Option<bool>,
    // shell
    /// Indicator per shell name (`zsh`, `bash`, `fish`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indicators: Option<BTreeMap<String, Sym>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_indicator: Option<Sym>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowStyle {
    #[default]
    Plain,
    Powerline,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Row {
    #[serde(default)]
    pub left: Vec<String>,
    #[serde(default)]
    pub right: Vec<String>,
    /// `plain`: modules joined by spaces. `powerline`: background segments
    /// joined by `separator` (left side only; the right side stays plain).
    #[serde(default)]
    pub style: RowStyle,
    /// Powerline: the glyph between segments. Plain: the text between
    /// modules (default one space).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separator: Option<Sym>,
    /// Powerline: cap drawn before a run of background segments, in the
    /// first segment's background color (e.g. ``).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Sym>,
    /// Powerline: cap drawn after a run of background segments, in the last
    /// segment's background color (default: `separator`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<Sym>,
    /// Powerline: glyph between neighbours with the same background (default:
    /// none, they abut). Drawn in the next segment's foreground unless it
    /// carries a span, e.g. `"[\u{e0b1}](fg:fg_dim)"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subseparator: Option<Sym>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Layout {
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMode {
    #[default]
    Compact,
    Full,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct History {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_separator: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<bool>,
    #[serde(default)]
    pub mode: HistoryMode,
    /// With `add_newline`, keep the blank line above the collapsed
    /// (transient) prompt in the history too (default true, like p10k).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_newline: Option<bool>,
}

/// Typed view of the effective (merged) config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigFile {
    pub schema_version: u32,
    #[serde(rename = "$comment", default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Preset the effective config was built from (`plain` unless the user chose one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default)]
    pub symbols: SymbolSetting,
    pub modules: BTreeMap<String, ModuleConfig>,
    pub layout: Layout,
    #[serde(default)]
    pub history: History,
    /// Agent status collection (zero-config wrapping of agent programs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
    /// Active palette (built-in or defined in `palettes`); see `palette.rs`.
    /// Style colors in this typed view are already resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette: Option<String>,
    /// User palettes: name -> color name -> color (`$comment` entries dropped).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub palettes: BTreeMap<String, BTreeMap<String, String>>,
    /// Starship's `add_newline`: an empty first prompt line (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_newline: Option<bool>,
    /// Starship's `command_timeout`: default `timeout_ms` of provider-backed
    /// modules (default 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_timeout_ms: Option<u64>,
}

/// Top-level `agent` section.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Run `programs` through `tildy agent wrap` from the Zsh adapter
    /// (default true; `TILDY_AGENT_WRAP=0` turns it off per shell).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap: Option<bool>,
    /// Command names to wrap (default `AGENT_DEFAULT_PROGRAMS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub programs: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Io,
    Syntax,
    Invalid,
    /// Newer schema_version than this build understands.
    Unsupported,
}

/// A config problem with a JSON path ("" = document root).
#[derive(Debug, Clone)]
pub struct ConfigError {
    pub kind: ErrorKind,
    pub path: String,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = if self.path.is_empty() { "<root>" } else { &self.path };
        write!(f, "{p}: {}", self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Fully merged, validated configuration used by the renderer.
#[derive(Debug, Clone)]
pub struct Config {
    pub schema_version: u32,
    /// User revision, or the combined user/project revision for contextual configs.
    /// CLI write transactions continue to use the raw user-file revision.
    pub revision: String,
    /// Resolved tier (config `symbols` -> $TILDY_SYMBOLS -> auto detection).
    pub symbols: SymbolTier,
    /// Effective merged raw JSON, `$comment` fields preserved.
    pub json: Value,
    /// Typed view of `json`, with palette colors resolved to their values.
    pub file: ConfigFile,
    /// The user layer exactly as parsed (None for builtin), for round-tripping.
    pub user: Option<Value>,
    pub source: Option<PathBuf>,
    /// Repository layer diagnostics; user transactions always ignore this layer.
    pub project: Option<project::Layer>,
}

/// FNV-1a 64. Chosen over std DefaultHasher because its output is specified
/// and stable across Rust releases; not cryptographic (change detection only).
pub fn content_revision(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(bv) => merge(bv, v),
                    None => { b.insert(k, v); }
                }
            }
        }
        (b, o) => *b = o,
    }
}

/// Merge a user layer: like `merge`, except that a module carrying `type`
/// replaces the base module of the same id instead of merging into it.
fn merge_user(base: &mut Value, mut over: Value) {
    if let (Some(bm), Some(Value::Object(om))) =
        (base.get_mut("modules").and_then(Value::as_object_mut), over.as_object_mut().and_then(|o| o.remove("modules")))
    {
        for (id, m) in om {
            if m.get("type").is_some() || !bm.contains_key(&id) {
                bm.insert(id, m);
            } else if let Some(b) = bm.get_mut(&id) {
                merge(b, m);
            }
        }
    }
    merge(base, over);
}

/// Apply a preset layer: its `modules` and `layout` replace the base ones;
/// other keys merge. A module the preset's layout places but does not define
/// (e.g. the built-in `agent`) is inherited from the base. The preset's own
/// top-level `$comment` is not carried over.
fn apply_preset(base: &mut Value, preset: Value) {
    let Value::Object(mut p) = preset else { return };
    p.remove("$comment");
    let placed: Vec<String> = p
        .get("layout")
        .and_then(|l| l.get("rows"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|r| ["left", "right"].into_iter().filter_map(|s| r.get(s).and_then(Value::as_array)).flatten())
        .filter_map(|id| id.as_str().map(str::to_string))
        .collect();
    if let (Some(Value::Object(pm)), Some(bm)) = (p.get_mut("modules"), base.get("modules").and_then(Value::as_object)) {
        for id in placed {
            if !pm.contains_key(&id) {
                if let Some(m) = bm.get(&id) {
                    pm.insert(id, m.clone());
                }
            }
        }
    }
    if let Some(b) = base.as_object_mut() {
        for k in ["modules", "layout"] {
            if let Some(v) = p.remove(k) {
                b.insert(k.into(), v);
            }
        }
    }
    merge(base, Value::Object(p));
}

/// Builtin layer plus the named preset (no user layer).
fn base_with_preset(name: &str) -> Result<Value, ConfigError> {
    let mut v: Value = serde_json::from_str(BUILTIN_JSON).expect("builtin.json is valid");
    let src = preset_json(name).ok_or_else(|| ConfigError {
        kind: ErrorKind::Invalid,
        path: "preset".into(),
        message: format!("unknown preset '{name}' (expected one of: {})", PRESETS.join(", ")),
    })?;
    apply_preset(&mut v, serde_json::from_str(src).expect("built-in preset is valid JSON"));
    v["preset"] = Value::String(name.into());
    Ok(v)
}

/// Effective JSON for a user layer: builtin -> preset -> user.
fn layered(user: &Value) -> Result<Value, ConfigError> {
    let name = match user.get("preset") {
        None => DEFAULT_PRESET,
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(ConfigError { kind: ErrorKind::Invalid, path: "preset".into(), message: "expected a string".into() }),
    };
    let mut effective = base_with_preset(name)?;
    merge_user(&mut effective, user.clone());
    Ok(effective)
}

/// Resolve a symbol tier. `env` is injected for tests.
pub fn resolve_symbols_with(setting: SymbolSetting, env: &dyn Fn(&str) -> Option<String>) -> SymbolTier {
    let from_setting = |s: SymbolSetting| match s {
        SymbolSetting::Unicode => Some(SymbolTier::Unicode),
        SymbolSetting::Nerd => Some(SymbolTier::Nerd),
        SymbolSetting::Ascii => Some(SymbolTier::Ascii),
        SymbolSetting::Auto => None,
    };
    if let Some(t) = from_setting(setting) { return t; }
    if let Some(v) = env("TILDY_SYMBOLS") {
        let s = match v.trim().to_ascii_lowercase().as_str() {
            "unicode" => SymbolSetting::Unicode,
            "nerd" => SymbolSetting::Nerd,
            "ascii" => SymbolSetting::Ascii,
            _ => SymbolSetting::Auto,
        };
        if let Some(t) = from_setting(s) { return t; }
    }
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    if get("TERM").as_deref() == Some("linux") { return SymbolTier::Ascii; }
    match get("LC_ALL").or_else(|| get("LC_CTYPE")).or_else(|| get("LANG")) {
        Some(loc) => {
            let l = loc.to_ascii_lowercase();
            if !(l.contains("utf-8") || l.contains("utf8")) { return SymbolTier::Ascii; }
        }
        // Unknown locale over SSH: be conservative.
        None if get("SSH_CONNECTION").is_some() || get("SSH_TTY").is_some() => return SymbolTier::Ascii,
        None => {}
    }
    match get("TERM_PROGRAM").as_deref() {
        Some("ghostty") | Some("WezTerm") => SymbolTier::Nerd,
        _ => SymbolTier::Unicode,
    }
}

pub fn resolve_symbols(setting: SymbolSetting) -> SymbolTier {
    resolve_symbols_with(setting, &|k| std::env::var(k).ok())
}

/// Parse a color name into a Zsh `%F{...}` argument. Names, `grey`,
/// `bright-<name>` (8-15), 0-255 and `#rrggbb` are accepted.
pub fn parse_color(s: &str) -> Option<String> {
    const NAMES: [&str; 8] = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    if s == "grey" || s == "gray" { return Some("8".into()); }
    if s == "default" { return Some("default".into()); }
    if NAMES.contains(&s) { return Some(s.to_string()); }
    if let Some(n) = s.strip_prefix("bright-") {
        return NAMES.iter().position(|x| *x == n).map(|i| (i + 8).to_string());
    }
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) { return Some(s.to_ascii_lowercase()); }
        return None;
    }
    if !s.is_empty() && s.len() <= 3 && s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse::<u16>().ok().filter(|n| *n <= 255).map(|n| n.to_string());
    }
    None
}

/// Default user config path: $TILDY_CONFIG, else $XDG_CONFIG_HOME/tildy/config.json,
/// else ~/.config/tildy/config.json.
pub fn default_config_path() -> Option<PathBuf> {
    config_path_with(&|k| std::env::var(k).ok())
}

pub fn config_path_with(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(p) = get("TILDY_CONFIG") { return Some(PathBuf::from(p)); }
    if let Some(x) = get("XDG_CONFIG_HOME") {
        let x = PathBuf::from(x);
        if x.is_absolute() { return Some(x.join("tildy").join("config.json")); }
    }
    get("HOME").map(|h| PathBuf::from(h).join(".config").join("tildy").join("config.json"))
}

fn build(effective: Value, revision: String, user: Option<Value>, source: Option<PathBuf>) -> Result<Config, ConfigError> {
    // Palette colors are resolved first; the validator and the typed view see
    // concrete colors, `Config::json` keeps the names.
    let resolved = palette::resolve(&effective)?;
    validate::validate(&resolved)?;
    let file: ConfigFile = serde_json::from_value(resolved)
        .map_err(|e| ConfigError { kind: ErrorKind::Invalid, path: String::new(), message: e.to_string() })?;
    Ok(Config {
        schema_version: file.schema_version,
        revision,
        symbols: resolve_symbols(file.symbols),
        json: effective,
        file,
        user,
        source,
        project: None,
    })
}

/// Parse a user config document (merged over the builtin) from text.
pub fn from_str(src: &str) -> Result<Config, ConfigError> {
    let user: Value = serde_json::from_str(src)
        .map_err(|e| ConfigError { kind: ErrorKind::Syntax, path: String::new(), message: e.to_string() })?;
    if !user.is_object() {
        return Err(ConfigError { kind: ErrorKind::Invalid, path: String::new(), message: "expected an object".into() });
    }
    let effective = layered(&user)?;
    build(effective, content_revision(src.as_bytes()), Some(user), None)
}

/// Load built-in defaults merged with the user file if present. `None` uses
/// `default_config_path()`. A missing file is not an error (returns builtin).
/// An invalid file returns Err (downcast to `ConfigError` for the JSON path)
/// and callers fall back to `builtin()`.
pub fn load(path: Option<&Path>) -> anyhow::Result<Config> {
    let owned;
    let path = match path {
        Some(p) => p,
        None => match default_config_path() {
            Some(p) => { owned = p; &owned }
            None => return Ok(builtin()),
        },
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(builtin()),
        Err(e) => {
            return Err(ConfigError { kind: ErrorKind::Io, path: String::new(), message: format!("{}: {e}", path.display()) }.into())
        }
    };
    let text = String::from_utf8(bytes.clone()).map_err(|_| ConfigError {
        kind: ErrorKind::Syntax,
        path: String::new(),
        message: format!("{}: not valid UTF-8", path.display()),
    })?;
    let mut cfg = from_str(&text)?;
    cfg.revision = content_revision(&bytes);
    cfg.source = Some(path.to_path_buf());
    Ok(cfg)
}

/// Built-in default configuration (zero-config path).
pub fn builtin() -> Config {
    let v = base_with_preset(DEFAULT_PRESET).expect("default preset exists");
    build(v, "builtin".into(), None, None).expect("builtin config validates")
}

/// Effective config of a built-in preset with no user file (for previews/tests).
pub fn preset(name: &str) -> Result<Config, ConfigError> {
    build(base_with_preset(name)?, format!("preset:{name}"), None, None)
}

/// Shared base JSON before applying a preset; use `builtin()` for effective defaults.
pub fn builtin_json() -> &'static str {
    let _ = SCHEMA_VERSION;
    BUILTIN_JSON
}

impl Config {
    pub fn with_symbols(mut self, tier: SymbolTier) -> Self {
        self.symbols = tier;
        self
    }
    pub fn history_compact(&self) -> bool {
        self.file.history.mode == HistoryMode::Compact
    }
    /// The transient prompt starts with a blank line: `add_newline` is on and
    /// `history.add_newline` has not turned it off for the collapsed form.
    pub fn transient_newline(&self) -> bool {
        self.file.add_newline == Some(true) && self.file.history.add_newline != Some(false)
    }

    /// Programs the Zsh adapter wraps with `tildy agent wrap`: `agent.programs`
    /// (default `AGENT_DEFAULT_PROGRAMS`), empty when `agent.wrap` is false.
    pub fn agent_programs(&self) -> Vec<String> {
        let a = self.file.agent.clone().unwrap_or_default();
        if a.wrap == Some(false) {
            return vec![];
        }
        a.programs.unwrap_or_else(|| AGENT_DEFAULT_PROGRAMS.iter().map(|p| p.to_string()).collect())
    }

    /// Modules referenced by the layout, in layout order, each once. The
    /// effective `modules` map may hold definitions the layout does not use
    /// (built-in leftovers, opt-in preset modules); providers should be built
    /// from this list only so unused modules cost nothing.
    pub fn layout_modules(&self) -> Vec<(&str, &ModuleConfig)> {
        let mut seen = std::collections::HashSet::new();
        let mut out = vec![];
        for row in &self.file.layout.rows {
            for id in row.left.iter().chain(row.right.iter()) {
                if seen.insert(id.as_str()) {
                    if let Some(m) = self.file.modules.get(id) {
                        out.push((id.as_str(), m));
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    fn fixtures() -> PathBuf { Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures") }

    #[test]
    fn builtin_matches_fixture() {
        let fx: Value = serde_json::from_str(&std::fs::read_to_string(fixtures().join("config.v1.valid.json")).unwrap()).unwrap();
        assert_eq!(builtin().json, fx);
        assert_eq!(builtin().revision, "builtin");
    }

    #[test]
    fn missing_file_is_builtin() {
        let c = load(Some(Path::new("/nonexistent/tildy/config.json"))).unwrap();
        assert_eq!(c.revision, "builtin");
    }

    #[test]
    fn user_override_merges_and_keeps_comments() {
        let c = load(Some(&fixtures().join("config.v1.valid-user.json"))).unwrap();
        assert_ne!(c.revision, "builtin");
        assert!(c.file.modules.contains_key("node"));
        assert!(c.file.modules.contains_key("cwd"));
        assert_eq!(c.json["$comment"], "personal config");
        assert_eq!(c.user.as_ref().unwrap()["$comment"], "personal config");
        assert_eq!(c.file.modules["git"].priority, 70);
    }

    #[test]
    fn revision_tracks_bytes() {
        assert_ne!(content_revision(b"a"), content_revision(b"b"));
        assert_eq!(content_revision(b"a"), content_revision(b"a"));
        assert_eq!(content_revision(b"").len(), 16);
    }

    fn err_of(name: &str) -> ConfigError {
        let e = load(Some(&fixtures().join(name))).unwrap_err();
        e.downcast::<ConfigError>().unwrap()
    }

    #[test]
    fn unknown_field_has_json_path() {
        let e = err_of("config.v1.invalid-unknown-field.json");
        assert_eq!(e.path, "modules.git_main.colour");
        assert_eq!(e.kind, ErrorKind::Invalid);
    }

    #[test]
    fn dangling_layout_id() {
        let e = err_of("config/invalid-dangling-ref.json");
        assert_eq!(e.path, "layout.rows[0].left[1]");
        assert!(e.message.contains("node_version"));
    }

    #[test]
    fn bad_operator_and_future_version() {
        assert_eq!(err_of("config.v1.invalid-operator.json").path, "modules.git_main.rules[0].when.op");
        let e = err_of("config.v1.invalid-future-version.json");
        assert_eq!(e.kind, ErrorKind::Unsupported);
        assert_eq!(e.path, "schema_version");
    }

    #[test]
    fn syntax_error_and_other_errors() {
        assert_eq!(from_str("{").unwrap_err().kind, ErrorKind::Syntax);
        assert_eq!(from_str(r#"{"schema_version":1,"modules":{"x":{"type":"command"}}}"#).unwrap_err().path, "modules.x.argv");
        assert_eq!(from_str(r#"{"schema_version":1,"symbols":"emoji"}"#).unwrap_err().path, "symbols");
        assert_eq!(from_str(r#"{"schema_version":1,"modules":{"cwd":{"style":{"foreground":"chartreuse"}}}}"#).unwrap_err().path, "modules.cwd.style.foreground");
    }

    #[test]
    fn symbol_tier_resolution() {
        let r = |s, e: &[(&str, &str)]| resolve_symbols_with(s, &env(e));
        let u = &[("LANG", "en_US.UTF-8")];
        assert_eq!(r(SymbolSetting::Auto, u), SymbolTier::Unicode);
        assert_eq!(r(SymbolSetting::Auto, &[("LANG", "en_US.UTF-8"), ("TERM", "linux")]), SymbolTier::Ascii);
        assert_eq!(r(SymbolSetting::Auto, &[("LANG", "C")]), SymbolTier::Ascii);
        assert_eq!(r(SymbolSetting::Auto, &[("LC_ALL", "POSIX"), ("LANG", "en_US.UTF-8")]), SymbolTier::Ascii);
        assert_eq!(r(SymbolSetting::Auto, &[("LANG", "en_US.UTF-8"), ("TERM_PROGRAM", "ghostty")]), SymbolTier::Nerd);
        assert_eq!(r(SymbolSetting::Auto, &[("LANG", "en_US.UTF-8"), ("TERM_PROGRAM", "WezTerm")]), SymbolTier::Nerd);
        assert_eq!(r(SymbolSetting::Auto, &[("LANG", "en_US.UTF-8"), ("TERM_PROGRAM", "iTerm.app")]), SymbolTier::Unicode);
        assert_eq!(r(SymbolSetting::Auto, &[]), SymbolTier::Unicode);
        assert_eq!(r(SymbolSetting::Auto, &[("SSH_CONNECTION", "1 2 3 4")]), SymbolTier::Ascii);
        // TILDY_SYMBOLS beats auto detection; config beats TILDY_SYMBOLS.
        assert_eq!(r(SymbolSetting::Auto, &[("TILDY_SYMBOLS", "ascii"), ("LANG", "en_US.UTF-8")]), SymbolTier::Ascii);
        assert_eq!(r(SymbolSetting::Unicode, &[("TILDY_SYMBOLS", "ascii")]), SymbolTier::Unicode);
        assert_eq!(r(SymbolSetting::Auto, &[("TILDY_SYMBOLS", "bogus"), ("TERM", "linux")]), SymbolTier::Ascii);
    }

    #[test]
    fn config_path_rules() {
        let p = |e: &[(&str, &str)]| config_path_with(&env(e));
        assert_eq!(p(&[("TILDY_CONFIG", "/x/c.json"), ("HOME", "/h")]), Some("/x/c.json".into()));
        assert_eq!(p(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/h")]), Some("/xdg/tildy/config.json".into()));
        assert_eq!(p(&[("XDG_CONFIG_HOME", "rel"), ("HOME", "/h")]), Some("/h/.config/tildy/config.json".into()));
        assert_eq!(p(&[("HOME", "/h")]), Some("/h/.config/tildy/config.json".into()));
        assert_eq!(p(&[]), None);
    }

    #[test]
    fn presets_validate_and_one_line_selects_the_look() {
        for name in PRESETS {
            preset(name).unwrap_or_else(|e| panic!("preset {name}: {e}"));
        }
        let c = from_str(r#"{"schema_version":1,"preset":"pastel-powerline"}"#).unwrap();
        assert_eq!(c.file.preset.as_deref(), Some("pastel-powerline"));
        assert_eq!(c.file.layout.rows.len(), 2);
        assert_eq!(c.file.layout.rows[0].style, RowStyle::Powerline);
        assert_eq!(c.file.layout.rows[0].left.first().map(String::as_str), Some("user"));
        assert_eq!(c.file.modules["time"].style.background.as_deref(), Some("#33658A"));
        // The preset replaces the built-in modules: no plain leftovers.
        assert!(!c.file.modules.contains_key("ssh_host"));
        assert_eq!(c.file.modules["git"].rules, vec![]);
        assert_eq!(builtin().file.preset.as_deref(), Some(DEFAULT_PRESET));
        let e = from_str(r#"{"schema_version":1,"preset":"neon"}"#).unwrap_err();
        assert_eq!((e.path.as_str(), e.kind), ("preset", ErrorKind::Invalid));
        assert!(e.message.contains("pastel-powerline"));
        assert_eq!(from_str(r#"{"schema_version":1,"preset":3}"#).unwrap_err().path, "preset");
    }

    #[test]
    fn preset_registry_is_consistent() {
        assert_eq!(PRESETS, PRESET_INFO.iter().map(|p| p.name).collect::<Vec<_>>().as_slice());
        for p in PRESET_INFO {
            let c = preset(p.name).unwrap_or_else(|e| panic!("preset {}: {e}", p.name));
            assert!(["required", "optional", "no"].contains(&p.nerd_font), "{}", p.name);
            // A preset that needs a Nerd Font pins the nerd tier, and only those do.
            assert_eq!(p.nerd_font == "required", p.symbols() == "nerd", "{}: symbols {}", p.name, p.symbols());
            assert_eq!(c.file.symbols != SymbolSetting::Auto, p.symbols() != "auto", "{}", p.name);
            assert!(!p.description.is_empty() && !p.source.is_empty());
            // One line with `{"preset": NAME}` selects it.
            let u = from_str(&format!(r#"{{"schema_version":1,"preset":"{}"}}"#, p.name)).unwrap();
            assert_eq!(u.file.preset.as_deref(), Some(p.name));
        }
        assert_eq!(preset_json("no-nerd-font").map(|j| j.contains("\"unicode\"")), Some(true));
        assert_eq!(PRESET_INFO.iter().find(|p| p.name == "plain-text-symbols").unwrap().symbols(), "ascii");
    }

    #[test]
    fn user_modules_merge_or_replace() {
        // Without `type`: merged into the preset module.
        let c = from_str(r##"{"schema_version":1,"preset":"pastel-powerline","modules":{"time":{"style":{"foreground":"#FFFFFF"}}}}"##).unwrap();
        let t = &c.file.modules["time"];
        assert_eq!((t.style.background.as_deref(), t.style.foreground.as_deref()), (Some("#33658A"), Some("#FFFFFF")));
        assert_eq!(t.time_format.as_deref(), Some("%R"));
        // With `type`: a full definition that replaces the module.
        let c = from_str(r#"{"schema_version":1,"modules":{"cwd":{"type":"directory"}}}"#).unwrap();
        assert_eq!(c.file.modules["cwd"].style, Style::default());
        assert_eq!(c.file.modules["cwd"].truncation_length, None);
        let c = from_str(r#"{"schema_version":1,"modules":{"node":{"type":"command","argv":["node","-v"]}}}"#).unwrap();
        assert_eq!(c.file.modules["node"].language, None);
    }

    #[test]
    fn layout_modules_lists_only_placed_modules() {
        let c = preset("pastel-powerline").unwrap();
        let ids: Vec<_> = c.layout_modules().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids.first(), Some(&"user"));
        assert_eq!(ids.last(), Some(&"char"));
        assert_eq!(ids.len(), 23);
        let c = from_str(r#"{"schema_version":1,"layout":{"rows":[{"left":["cwd","cwd"],"right":["cwd"]}]}}"#).unwrap();
        assert_eq!(c.layout_modules().len(), 1);
        assert!(c.file.modules.contains_key("node"));
    }

    #[test]
    fn new_fields_validate_with_paths() {
        let path = |src: &str| from_str(src).unwrap_err().path;
        let m = |body: &str| format!(r#"{{"schema_version":1,"modules":{{"x":{body}}},"layout":{{"rows":[{{"left":["x"]}}]}}}}"#);
        // Nested groups are valid since spans arrived; an unclosed one is not.
        assert_eq!(path(&m(r#"{"type":"time","format":"(a(b)"}"#)), "modules.x.format");
        assert_eq!(path(&m(r#"{"type":"time","format":"[a](wobbly)"}"#)), "modules.x.format");
        assert_eq!(path(&m(r#"{"type":"text"}"#)), "modules.x.text");
        assert_eq!(path(&m(r#"{"type":"env_var"}"#)), "modules.x.variable");
        assert_eq!(path(&m(r#"{"type":"env_var","variable":"a b"}"#)), "modules.x.variable");
        assert_eq!(path(&m(r#"{"type":"os","os_symbols":{"macos":3}}"#)), "modules.x.os_symbols.macos");
        assert_eq!(path(&m(r#"{"type":"text","text":"x","padding":"no"}"#)), "modules.x.padding");
        assert_eq!(path(&m(r#"{"type":"jobs","error_symbol":"x"}"#)), "modules.x.error_symbol");
        assert_eq!(path(&m(r#"{"type":"jobs","number_threshold":-2}"#)), "modules.x.number_threshold");
        assert_eq!(path(&m(r#"{"type":"time","time_format":"%Q"}"#)), "modules.x.time_format");
        assert_eq!(path(&m(r#"{"type":"language"}"#)), "modules.x.language");
        assert_eq!(path(&m(r#"{"type":"language","language":"cobol"}"#)), "modules.x.language");
        assert_eq!(path(&m(r#"{"type":"starship"}"#)), "modules.x.module");
        assert_eq!(path(&m(r#"{"type":"starship","module":"a b"}"#)), "modules.x.module");
        assert_eq!(path(&m(r#"{"type":"jobs","symbol":{"nerd":"a","emoji":"b"}}"#)), "modules.x.symbol.emoji");
        assert_eq!(path(&m(r#"{"type":"jobs","symbol":7}"#)), "modules.x.symbol");
        assert_eq!(path(&m(r#"{"type":"git","status_symbols":{"modfied":"!"}}"#)), "modules.x.status_symbols.modfied");
        assert_eq!(path(&m(r#"{"type":"git","state_labels":{"squash":"S"}}"#)), "modules.x.state_labels.squash");
        assert_eq!(path(&m(r#"{"type":"git","home_symbol":"~"}"#)), "modules.x.home_symbol");
        assert_eq!(path(&m(r#"{"type":"directory","substitutions":{"a":1}}"#)), "modules.x.substitutions.a");
        assert_eq!(path(&m(r#"{"type":"directory","truncation_length":-1}"#)), "modules.x.truncation_length");
        assert_eq!(path(&m(r#"{"type":"command","argv":["x"],"regex":"("}"#)), "modules.x.regex");
        assert_eq!(path(&m(r##"{"type":"character","success_style":{"foreground":"#12345"}}"##)), "modules.x.success_style.foreground");
        assert_eq!(path(&m(r#"{"type":"time","style":{"italic":"yes"}}"#)), "modules.x.style.italic");
        let row = |r: &str| format!(r#"{{"schema_version":1,"layout":{{"rows":[{r}]}}}}"#);
        assert_eq!(path(&row(r#"{"style":"fancy","left":["cwd"]}"#)), "layout.rows[0].style");
        assert_eq!(path(&row(r#"{"separator":["x"],"left":["cwd"]}"#)), "layout.rows[0].separator");
        assert_eq!(path(&row(r#"{"start":1,"left":["cwd"]}"#)), "layout.rows[0].start");
        assert_eq!(path(&row(r#"{"end":{"emoji":"x"},"left":["cwd"]}"#)), "layout.rows[0].end.emoji");
        // A full kitchen-sink module validates and round-trips `$comment`.
        let c = from_str(&m(r##"{"type":"directory","$comment":"c","truncation_length":0,"truncation_symbol":{"nerd":"…/","ascii":".../"},
            "home_symbol":"~","substitutions":{"Documents":{"nerd":"D"},"~/w":"W"},"truncate_to_repo":false,"format":"$path",
            "style":{"background":"#DA627D","dim":true,"italic":true}}"##))
        .unwrap();
        assert_eq!(c.json["modules"]["x"]["$comment"], "c");
        assert_eq!(c.file.modules["x"].truncation_symbol.as_ref().unwrap().resolve(SymbolTier::Unicode), ".../");
    }

    #[test]
    fn starship_parity_fields_validate() {
        let path = |src: &str| from_str(src).unwrap_err().path;
        let m = |body: &str| format!(r#"{{"schema_version":1,"modules":{{"x":{body}}},"layout":{{"rows":[{{"left":["x"]}}]}}}}"#);
        // Global options.
        assert_eq!(path(r#"{"schema_version":1,"add_newline":"yes"}"#), "add_newline");
        assert_eq!(path(r#"{"schema_version":1,"command_timeout_ms":0}"#), "command_timeout_ms");
        let c = from_str(r#"{"schema_version":1,"add_newline":true,"command_timeout_ms":800}"#).unwrap();
        assert_eq!((c.file.add_newline, c.file.command_timeout_ms), (Some(true), Some(800)));
        // Type-specific fields stay with their types.
        assert_eq!(path(&m(r#"{"type":"time","read_only":"x"}"#)), "modules.x.read_only");
        assert_eq!(path(&m(r#"{"type":"shlvl","ssh_only":true}"#)), "modules.x.ssh_only");
        assert_eq!(path(&m(r#"{"type":"jobs","threshold":3}"#)), "modules.x.threshold");
        assert_eq!(path(&m(r#"{"type":"shell","timeout_ms":5}"#)), "modules.x.timeout_ms");
        // Values.
        assert_eq!(path(&m(r#"{"type":"time","time_range":"10:00-12:00:00"}"#)), "modules.x.time_range");
        assert_eq!(path(&m(r#"{"type":"time","utc_time_offset":"+30"}"#)), "modules.x.utc_time_offset");
        assert_eq!(path(&m(r#"{"type":"hostname","detect_env_vars":["!"]}"#)), "modules.x.detect_env_vars[0]");
        assert_eq!(path(&m(r#"{"type":"hostname","aliases":{"a":1}}"#)), "modules.x.aliases.a");
        assert_eq!(path(&m(r#"{"type":"shell","indicators":{"csh":"c"}}"#)), "modules.x.indicators.csh");
        assert_eq!(path(&m(r#"{"type":"memory_usage","threshold":5000}"#)), "modules.x.threshold");
        assert!(from_str(&m(r#"{"type":"git","status_symbols":{"typechanged":"T"}}"#)).is_ok());
        assert_eq!(path(&m(r#"{"type":"directory","read_only_style":{"foreground":"nope"}}"#)), "modules.x.read_only_style.foreground");
        // A module of every new type validates with its options.
        let all = r##"{"schema_version":1,"modules":{
            "p":{"type":"package","display_private":true,"version_format":"${raw}"},
            "gc":{"type":"git_commit","commit_hash_length":10,"only_detached":false,"tag_disabled":false,"tag_max_candidates":2,"tag_symbol":" t "},
            "gm":{"type":"git_metrics","added_style":{"foreground":"green"},"deleted_style":{"foreground":"red"},"only_nonzero_diffs":false,"ignore_submodules":true},
            "sl":{"type":"shlvl","threshold":1,"repeat":true,"repeat_offset":1},
            "ip":{"type":"localip","ssh_only":false},
            "mem":{"type":"memory_usage","threshold":-1},
            "su":{"type":"sudo","timeout_ms":300},
            "de":{"type":"direnv","detect_files":[".envrc"],"detect_env_vars":["DIRENV_FILE"],"allowed_msg":"ok","denied_msg":"no"},
            "nix":{"type":"nix_shell","pure_msg":"P","impure_msg":"I","unknown_msg":"?","heuristic":true},
            "sh":{"type":"shell","indicators":{"zsh":"Z","pwsh":"PS"},"unknown_indicator":"?"},
            "t":{"type":"time","use_12hr":true,"utc_time_offset":"-5.5","time_range":"09:00:00-"},
            "st":{"type":"exit_status","success_symbol":"ok","map_symbol":true,"recognize_signal_code":false,"error_style":{"foreground":"red"}},
            "cwd":{"type":"directory","read_only":"RO","fish_style_pwd_dir_length":1,"use_logical_path":false,"repo_root_style":{"bold":true}}},
            "layout":{"rows":[{"left":["p","gc","gm","sl","ip","mem","su","de","nix","sh","t","st","cwd"]}]}}"##;
        let c = from_str(all).unwrap();
        assert_eq!(c.file.modules["gc"].module_type, ModuleType::GitCommit);
        assert_eq!(c.file.modules["sh"].indicators.as_ref().unwrap()["zsh"], Sym::Plain("Z".into()));
        for ty in ["package", "git_commit", "git_metrics", "shlvl", "localip", "memory_usage", "sudo", "direnv", "nix_shell", "shell"] {
            assert!(MODULE_TYPES.contains(&ty), "{ty}");
            let t: ModuleType = serde_json::from_value(Value::String(ty.into())).unwrap();
            assert_eq!(t.as_str(), ty);
        }
    }

    #[test]
    fn sym_tier_fallback() {
        let s: Sym = serde_json::from_value(serde_json::json!({"nerd": "N", "ascii": "A"})).unwrap();
        assert_eq!((s.resolve(SymbolTier::Nerd), s.resolve(SymbolTier::Unicode), s.resolve(SymbolTier::Ascii)), ("N", "A", "A"));
        let s: Sym = serde_json::from_value(serde_json::json!({"nerd": "N", "unicode": ""})).unwrap();
        assert_eq!((s.resolve(SymbolTier::Unicode), s.resolve(SymbolTier::Ascii)), ("", ""));
        assert_eq!(Sym::Plain("x".into()).resolve(SymbolTier::Ascii), "x");
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("grey").as_deref(), Some("8"));
        assert_eq!(parse_color("bright-red").as_deref(), Some("9"));
        assert_eq!(parse_color("208").as_deref(), Some("208"));
        assert_eq!(parse_color("#FF8800").as_deref(), Some("#ff8800"));
        assert_eq!(parse_color("white").as_deref(), Some("white"));
        assert_eq!(parse_color("0").as_deref(), Some("0"));
        assert!(parse_color("256").is_none());
        assert!(parse_color("#12").is_none());
    }
}
