//! `tildy preset list|show`. Owner: M1b-presets task.
//!
//! `show` renders a preset with sample data (or, with `--here`, the current
//! directory's real git and language data), expands the Zsh prompt escapes to
//! ANSI and prints every line plus the right prompt and the transient form, so
//! the colors can be judged in the terminal. `--all` prints every preset.
//! `--palette P` recolors the role-based presets (`powerline`, `lean`,
//! `classic`, `rainbow`, `minimal`) with palette P, like
//! `{"preset": NAME, "palette": P}` in a user config.
use super::i18n::{description, tr, tr_println};
use super::preview::{compose_lines, here_data, nerd_label, parse_tier, sample_data, sample_now, terminal_cols, zsh_to_ansi};
use clap::{Args, Subcommand};
use serde::Serialize;
use tildy::config::{self, Config, PresetInfo, PRESET_INFO};
use tildy::model::{CliEnvelope, ErrorCode, PromptData};
use tildy::render::{self, LocalTime};
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

#[derive(Args)]
pub struct PresetArgs {
    #[command(subcommand)]
    pub cmd: PresetCmd,
}

#[derive(Subcommand)]
pub enum PresetCmd {
    /// List built-in presets with a one-line description (--json for structured output).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Render a preset with sample data (or the current directory) in real colors.
    Show {
        /// Preset name (see `tildy preset list`); omit with --all.
        name: Option<String>,
        /// Use the current directory's real git/language data instead of sample data.
        #[arg(long)]
        here: bool,
        /// Symbol tier override: nerd|unicode|ascii.
        #[arg(long)]
        symbols: Option<String>,
        /// Print every preset in sequence, each under its name.
        #[arg(long)]
        all: bool,
        /// Recolor the role-based presets with this palette (see `tildy palette list`).
        #[arg(long)]
        palette: Option<String>,
    },
}

#[derive(Serialize)]
struct ListEntry {
    name: &'static str,
    description: &'static str,
    nerd_font: &'static str,
    symbols: String,
    source: &'static str,
    /// Default palette of a role-based preset; null for fixed colors.
    palette: Option<String>,
}

fn entries() -> Vec<ListEntry> {
    PRESET_INFO
        .iter()
        .map(|p| ListEntry {
            name: p.name,
            description: p.description,
            nerd_font: p.nerd_font,
            symbols: p.symbols(),
            source: p.source,
            palette: p.palette(),
        })
        .collect()
}

pub fn run(a: PresetArgs) -> i32 {
    match a.cmd {
        PresetCmd::List { json } => list(json),
        PresetCmd::Show { name, here, symbols, all, palette } => show(name.as_deref(), here, symbols.as_deref(), all, palette.as_deref()),
    }
}

fn list(json: bool) -> i32 {
    let e = entries();
    if json {
        match serde_json::to_string_pretty(&CliEnvelope::ok(&e)) {
            Ok(s) => println!("{s}"),
            Err(err) => {
                eprintln!("tildy preset: {err}");
                return ErrorCode::InternalError.exit_code();
            }
        }
        return 0;
    }
    let w = e.iter().map(|x| x.name.len()).max().unwrap_or(0);
    let pad = |s: &str, n: usize| format!("{s}{}", " ".repeat(n.saturating_sub(UnicodeWidthStr::width(s))));
    for x in &e {
        let pal = x.palette.as_deref().map(|p| tr!("  (palette {p})", "  （调色板 {p}）")).unwrap_or_default();
        println!("{:<w$}  {}  {}{pal}", x.name, pad(nerd_label(x.nerd_font), 14), description(x.description));
    }
    println!();
    tr_println!("Preview: tildy preset show --all  Apply: write {{\"preset\": \"NAME\"}} in config.json", "预览：tildy preset show --all　应用：在 config.json 写 {{\"preset\": \"NAME\"}}");
    tr_println!("Role-based presets support palettes: tildy preset show NAME --palette P (tildy palette list); write {{\"preset\": \"NAME\", \"palette\": \"P\"}}", "按角色着色的预设可换调色板：tildy preset show NAME --palette P（tildy palette list），写 {{\"preset\": \"NAME\", \"palette\": \"P\"}}");
    0
}

/// A preset's effective config, recolored with `palette` when it is role-based.
fn preset_config(p: &PresetInfo, palette: Option<&str>) -> Result<Config, config::ConfigError> {
    match palette.filter(|_| p.palette().is_some()) {
        Some(pal) => config::from_str(&serde_json::json!({"schema_version": 1, "preset": p.name, "palette": pal}).to_string()),
        None => config::preset(p.name),
    }
}

fn show(name: Option<&str>, here: bool, symbols: Option<&str>, all: bool, palette: Option<&str>) -> i32 {
    if let Some(pal) = palette {
        if config::palette::builtin(pal).is_none() {
            eprintln!("tildy preset: unknown palette '{pal}'");
            eprintln!("hint: tildy palette list");
            return ErrorCode::ValidationFailed.exit_code();
        }
    }
    let tier = match symbols.map(|s| (s, parse_tier(s))) {
        None => None,
        Some((_, Some(t))) => Some(t),
        Some((s, None)) => {
            eprintln!("tildy preset: unknown symbol tier '{s}' (nerd, unicode or ascii)");
            return ErrorCode::ValidationFailed.exit_code();
        }
    };
    let list: Vec<&PresetInfo> = match (name, all) {
        (_, true) => PRESET_INFO.iter().collect(),
        (Some(n), false) => match PRESET_INFO.iter().find(|p| p.name == n) {
            Some(p) if palette.is_some() && p.palette().is_none() => {
                eprintln!("tildy preset: '{n}' has fixed colors; --palette recolors the role-based presets ({})", config::ROLE_PRESETS.join(", "));
                return ErrorCode::ValidationFailed.exit_code();
            }
            Some(p) => vec![p],
            None => {
                eprintln!("tildy preset: unknown preset '{n}'");
                eprintln!("hint: tildy preset list");
                return ErrorCode::ValidationFailed.exit_code();
            }
        },
        (None, false) => {
            eprintln!("tildy preset: name a preset or pass --all");
            eprintln!("hint: tildy preset list");
            return ErrorCode::ValidationFailed.exit_code();
        }
    };
    let cols = terminal_cols();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    for (i, p) in list.iter().enumerate() {
        let cfg = match preset_config(p, palette) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("tildy preset: {}: {e}", p.name);
                return ErrorCode::InternalError.exit_code();
            }
        };
        let cfg = match tier {
            Some(t) => cfg.with_symbols(t),
            None => cfg,
        };
        let (data, now) = if here {
            let (data, problems) = here_data(&cfg, &cwd, cols);
            for p in problems {
                eprintln!("tildy preset: {p}");
            }
            (data, None)
        } else {
            (sample_data(&cfg, cols), sample_now())
        };
        if i > 0 {
            println!();
        }
        print_preset(p, &cfg, &data, now, cols);
    }
    0
}

fn print_preset(p: &PresetInfo, cfg: &Config, data: &PromptData, now: Option<LocalTime>, cols: usize) {
    let out = render::render_preview(cfg, data, now);
    let pal = cfg.file.palette.as_deref().map(|p| tr!(", palette {p}", "，调色板 {p}")).unwrap_or_default();
    tr_println!("\x1b[1m── {}\x1b[0m  {}  \x1b[2m[{}, symbols {}{pal}]\x1b[0m", "\x1b[1m── {}\x1b[0m  {}  \x1b[2m[{}，符号层 {}{pal}]\x1b[0m",
        p.name,
        description(p.description),
        nerd_label(p.nerd_font),
        cfg.symbols.as_str()
    );
    for line in compose_lines(&out, cols) {
        println!("{line}\x1b[0m");
    }
    if let Some(t) = &out.prompt.transient {
        tr_println!("\x1b[2mAfter execution:\x1b[0m {}\x1b[0m", "\x1b[2m执行后收起为:\x1b[0m {}\x1b[0m", zsh_to_ansi(t));
    }
}
