//! `tildy` CLI. Subcommand bodies live in src/cli/*. Owners: see docs/protocol.md.
use clap::{CommandFactory, Parser, Subcommand};

mod cli;

#[derive(Parser)]
#[command(name = "tildy", version = tildy::model::BINARY_VERSION, about)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Set up .zshrc, or print a shell loader with `init zsh`.
    Init(cli::init::InitArgs),
    /// One-shot backend: read one REQ frame on stdin, write one RES frame, exit.
    Render(cli::render::RenderArgs),
    /// Persistent per-shell backend speaking the frame protocol on stdin/stdout.
    Worker(cli::worker::WorkerArgs),
    /// Report supported shells, modules, versions (--json).
    Capabilities(cli::capabilities::CapabilitiesArgs),
    /// Read a stable session snapshot or list active sessions.
    Inspect(cli::inspect::InspectArgs),
    /// Explain why modules are visible, hidden, stale or slow.
    Explain(cli::explain::ExplainArgs),
    /// Show bounded per-cycle provider timings.
    Timings(cli::timings::TimingsArgs),
    /// Measure terminal cell advances in an explicit foreground calibration.
    Calibrate(cli::calibrate::CalibrateArgs),
    /// Import a Starship config as a candidate tildy config.
    Import(cli::import::ImportArgs),
    /// List or preview built-in presets.
    Preset(cli::preset::PresetArgs),
    /// List built-in palettes or show a palette's colors.
    Palette(cli::palette::PaletteArgs),
    /// Choose a theme, colors, layout and blank lines with live preview.
    #[command(alias = "configure")]
    Theme(cli::configure::ConfigureArgs),
    /// Read, validate, apply or roll back the user config with revision checks.
    Config(cli::config::ConfigArgs),
    /// Print the config JSON Schema.
    Schema(cli::schema::SchemaArgs),
    /// Publish, list, clear or collect agent task status (OSC 7501 vocabulary).
    Agent(cli::agent::AgentArgs),
    /// Render a fixed scenario with a config and print a module report.
    Preview(cli::preview::PreviewArgs),
}

fn main() {
    let args = Args::parse();
    let code = match args.cmd {
        Cmd::Init(a) => cli::init::run(a),
        Cmd::Render(a) => cli::render::run(a),
        Cmd::Worker(a) => cli::worker::run(a),
        Cmd::Capabilities(a) => cli::capabilities::run(a, &Args::command()),
        Cmd::Inspect(a) => cli::inspect::run(a),
        Cmd::Explain(a) => cli::explain::run(a),
        Cmd::Timings(a) => cli::timings::run(a),
        Cmd::Calibrate(a) => cli::calibrate::run(a),
        Cmd::Import(a) => cli::import::run(a),
        Cmd::Preset(a) => cli::preset::run(a),
        Cmd::Palette(a) => cli::palette::run(a),
        Cmd::Theme(a) => cli::configure::run(a),
        Cmd::Config(a) => cli::config::run(a),
        Cmd::Schema(a) => cli::schema::run(a),
        Cmd::Agent(a) => cli::agent::run(a),
        Cmd::Preview(a) => cli::preview::run(a),
    };
    std::process::exit(code);
}
