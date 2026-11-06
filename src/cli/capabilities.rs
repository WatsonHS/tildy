//! `tildy capabilities --json`. Owner: task C.
use clap::Args;
use serde::Serialize;
use tildy::config;
use tildy::model::{CliEnvelope, API_VERSION, BINARY_VERSION, PROTOCOL_VERSION, SCHEMA_VERSION};

#[derive(Args)]
pub struct CapabilitiesArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct ConfigInfo {
    path: Option<String>,
    exists: bool,
    valid: bool,
    error: Option<String>,
    revision: Option<String>,
}

/// A built-in preset (`{"preset": name}`); see `tildy preset list`.
#[derive(Serialize)]
struct PresetEntry {
    name: &'static str,
    description: &'static str,
    /// `required`, `optional` or `no`.
    nerd_font: &'static str,
    /// The symbol tier the preset pins (`auto` when none).
    symbols: String,
    source: &'static str,
    /// Default palette of a role-based preset (recolor with `palette`); null
    /// for presets with fixed colors.
    palette: Option<String>,
}

/// One top-level subcommand, read from the clap definition so the list cannot
/// drift from `tildy --help`.
#[derive(Serialize)]
struct CommandInfo {
    name: String,
    about: Option<String>,
    /// Nested subcommands (`config get`, `agent publish`, ...).
    subcommands: Vec<String>,
    /// The command (or one of its subcommands) takes `--json`.
    json: bool,
}

fn commands(cmd: &clap::Command) -> Vec<CommandInfo> {
    cmd.get_subcommands()
        .filter(|c| c.get_name() != "help")
        .map(|c| CommandInfo {
            name: c.get_name().to_string(),
            about: c.get_about().map(|a| a.to_string()),
            subcommands: c.get_subcommands().filter(|s| s.get_name() != "help").map(|s| s.get_name().to_string()).collect(),
            json: c.get_arguments().chain(c.get_subcommands().flat_map(|s| s.get_arguments())).any(|a| a.get_id() == "json"),
        })
        .collect()
}

#[derive(Serialize)]
struct Capabilities {
    api_version: u32,
    schema_version: u32,
    protocol_version: u32,
    binary_version: &'static str,
    shells: Vec<&'static str>,
    /// Every subcommand with its one-line purpose, nested subcommands and
    /// whether it speaks `--json` (command discovery for Agents).
    commands: Vec<CommandInfo>,
    module_types: Vec<&'static str>,
    rule_operators: Vec<&'static str>,
    symbol_tiers: Vec<&'static str>,
    history_modes: Vec<&'static str>,
    presets: Vec<PresetEntry>,
    /// Built-in palettes (`{"palette": name}`); see `tildy palette list`.
    palettes: Vec<config::PaletteInfo>,
    /// Role colors every built-in palette defines (role-based presets use only these).
    palette_roles: Vec<&'static str>,
    config: ConfigInfo,
    agent: AgentInfo,
    diagnostics: DiagnosticsInfo,
    terminal_width: TerminalWidthInfo,
}

#[derive(Serialize)]
struct TerminalWidthInfo {
    command: &'static str,
    explicit_foreground: bool,
    font_revision_required: bool,
    max_samples: usize,
    budget_ms: u64,
    profile_schema: &'static str,
}

#[derive(Serialize)]
struct DiagnosticsInfo {
    snapshot_schema_version: u32,
    timing_history_limit: usize,
    snapshot_schema: &'static str,
    active_session_env: &'static str,
}

/// Agent status vocabulary and the effective wrapping settings
/// (docs/agent-status.md).
#[derive(Serialize)]
struct AgentInfo {
    states: Vec<&'static str>,
    kinds: Vec<&'static str>,
    scopes: Vec<&'static str>,
    wrap: bool,
    programs: Vec<String>,
}

fn gather(cli: &clap::Command) -> Capabilities {
    let path = config::default_config_path();
    let exists = path.as_ref().is_some_and(|p| p.is_file());
    let (valid, error, revision, programs) = match config::load(path.as_deref()) {
        Ok(c) => (true, None, Some(c.revision.clone()), c.agent_programs()),
        Err(e) => (false, Some(e.to_string()), None, config::builtin().agent_programs()),
    };
    let agent = AgentInfo {
        states: config::AGENT_STATES.to_vec(),
        kinds: tildy::agent::Kind::ALL.iter().map(|k| k.as_str()).collect(),
        scopes: tildy::agent::ScopeKind::ALL.iter().map(|k| k.as_str()).collect(),
        wrap: !programs.is_empty(),
        programs,
    };
    Capabilities {
        api_version: API_VERSION,
        schema_version: SCHEMA_VERSION,
        protocol_version: PROTOCOL_VERSION,
        binary_version: BINARY_VERSION,
        shells: vec!["zsh"],
        commands: commands(cli),
        module_types: config::MODULE_TYPES.to_vec(),
        rule_operators: config::OPERATORS.to_vec(),
        symbol_tiers: config::SYMBOL_TIERS.to_vec(),
        history_modes: config::HISTORY_MODES.to_vec(),
        presets: config::PRESET_INFO
            .iter()
            .map(|p| PresetEntry {
                name: p.name,
                description: p.description,
                nerd_font: p.nerd_font,
                symbols: p.symbols(),
                source: p.source,
                palette: p.palette(),
            })
            .collect(),
        palettes: config::PALETTE_INFO.to_vec(),
        palette_roles: config::ROLES.to_vec(),
        config: ConfigInfo { path: path.map(|p| p.display().to_string()), exists, valid, error, revision },
        agent,
        terminal_width: TerminalWidthInfo { command: "calibrate", explicit_foreground: true, font_revision_required: true, max_samples: 64, budget_ms: 5000, profile_schema: "schema/terminal-width.schema.json" },
        diagnostics: DiagnosticsInfo { snapshot_schema_version: tildy::diagnostics::SNAPSHOT_SCHEMA_VERSION, timing_history_limit: tildy::diagnostics::TIMING_HISTORY_LIMIT, snapshot_schema: "schema/snapshot.schema.json", active_session_env: "TILDY_SESSION_ID" },
    }
}

pub fn run(a: CapabilitiesArgs, cli: &clap::Command) -> i32 {
    let c = gather(cli);
    if a.json {
        match serde_json::to_string_pretty(&CliEnvelope::ok(c)) {
            Ok(s) => println!("{s}"),
            Err(e) => { eprintln!("tildy: {e}"); return 1; }
        }
    } else {
        println!("tildy {} (api {}, schema {}, protocol {})", c.binary_version, c.api_version, c.schema_version, c.protocol_version);
        println!("shells: {}", c.shells.join(", "));
        println!("commands: {}", c.commands.iter().map(|x| x.name.as_str()).collect::<Vec<_>>().join(", "));
        println!("modules: {}", c.module_types.join(", "));
        println!("operators: {}", c.rule_operators.join(", "));
        println!("symbols: {}", c.symbol_tiers.join(", "));
        println!("presets: {}", c.presets.iter().map(|p| p.name).collect::<Vec<_>>().join(", "));
        println!("palettes: {}", c.palettes.iter().map(|p| p.name).collect::<Vec<_>>().join(", "));
        println!(
            "config: {} ({}{})",
            c.config.path.as_deref().unwrap_or("<none>"),
            if c.config.exists { "exists" } else { "missing, using builtin" },
            if c.config.valid { "" } else { ", INVALID" }
        );
        if let Some(e) = &c.config.error { println!("config error: {e}"); }
    }
    0
}
