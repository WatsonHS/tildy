//! `tildy palette list|show`. Owner: M2 task E2.
//!
//! `list` names the built-in palettes (`--json` for structured output); `show`
//! prints a palette's colors as swatches in real colors (24-bit ANSI), the
//! roles first, followed by a sample of segments and text on the scheme's own
//! background. A name that is not built in is looked up in the user config's
//! `palettes`. See docs/palettes.md.
use super::i18n::{description, text, tr_println};
use super::preview::zsh_to_ansi;
use clap::{Args, Subcommand};
use serde::Serialize;
use tildy::config::{self, palette, Palette, PALETTE_INFO, ROLES};
use tildy::model::{CliEnvelope, ErrorCode};
use unicode_width::UnicodeWidthStr;

#[derive(Args)]
pub struct PaletteArgs {
    #[command(subcommand)]
    pub cmd: PaletteCmd,
}

#[derive(Subcommand)]
pub enum PaletteCmd {
    /// List built-in palettes (--json for structured output).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print a palette's colors as swatches (built-in, or defined under `palettes` in the user config).
    Show {
        /// Palette name (see `tildy palette list`).
        name: String,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(a: PaletteArgs) -> i32 {
    match a.cmd {
        PaletteCmd::List { json } => list(json),
        PaletteCmd::Show { name, json } => show(&name, json),
    }
}

#[derive(Serialize)]
struct ListEntry {
    name: &'static str,
    description: &'static str,
    variant: &'static str,
    source: &'static str,
}

fn print_json<T: Serialize>(v: &T) -> i32 {
    match serde_json::to_string_pretty(&CliEnvelope::ok(v)) {
        Ok(s) => {
            println!("{s}");
            0
        }
        Err(e) => {
            eprintln!("tildy palette: {e}");
            ErrorCode::InternalError.exit_code()
        }
    }
}

fn pad(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(UnicodeWidthStr::width(s))))
}

fn list(json: bool) -> i32 {
    let e: Vec<ListEntry> =
        PALETTE_INFO.iter().map(|p| ListEntry { name: p.name, description: p.description, variant: p.variant, source: p.source }).collect();
    if json {
        return print_json(&e);
    }
    let w = e.iter().map(|x| x.name.len()).max().unwrap_or(0);
    for p in PALETTE_INFO {
        let pal = Palette { name: p.name.into(), colors: p.colors() };
        let strip: String = ["bg1", "bg2", "bg3", "bg4", "bg5", "bg6"].iter().map(|r| swatch(pal.get(r).unwrap_or(""), "  ")).collect();
        let variant = if p.variant == "light" { text("Light", "浅色") } else { text("Dark", "深色") };
        println!("{:<w$}  {strip}  {variant}  {}", p.name, description(p.description));
    }
    println!();
    tr_println!("Colors: tildy palette show NAME  Preview: tildy preset show powerline --palette NAME  Apply: write {{\"palette\": \"NAME\"}} in config.json", "看颜色：tildy palette show NAME　预览：tildy preset show powerline --palette NAME　应用：在 config.json 写 {{\"palette\": \"NAME\"}}");
    0
}

/// `text` on a background `color` (any tildy color), as ANSI.
fn swatch(color: &str, text: &str) -> String {
    match config::parse_color(color) {
        Some(c) => zsh_to_ansi(&format!("%K{{{c}}}{text}%k")),
        None => text.to_string(),
    }
}

/// `text` in foreground `fg` on background `bg` (either may be empty), as ANSI.
fn paint(fg: &str, bg: &str, text: &str) -> String {
    let f = config::parse_color(fg).map(|c| format!("%F{{{c}}}")).unwrap_or_default();
    let b = config::parse_color(bg).map(|c| format!("%K{{{c}}}")).unwrap_or_default();
    zsh_to_ansi(&format!("{b}{f}{}%f%k", text.replace('%', "%%")))
}

fn role_note(r: &str) -> &'static str {
    match r {
        "bg1" | "bg2" | "bg3" | "bg4" | "bg5" => text("Segment background (powerline, rainbow)", "段背景（powerline、rainbow）"),
        "bg6" => text("Final segment background (time)", "收尾段背景（时间）"),
        "fg" => text("Segment text", "段上的文字"),
        "fg_dim" => text("Secondary text (separators, time)", "次要文字（分隔、时间）"),
        "surface" => text("Neutral segment background (classic)", "中性段背景（classic）"),
        "accent" => text("Accent (directory)", "强调（目录）"),
        "ok" => text("Success (prompt, clean Git)", "成功（提示符、干净的 Git）"),
        "warn" => text("Warning (changes, duration)", "警告（改动、耗时）"),
        "err" => text("Error (failures, conflicts)", "错误（失败、冲突）"),
        "info" => text("Info (jobs, virtual environment)", "信息（任务、虚拟环境）"),
        _ => "",
    }
}

#[derive(Serialize)]
struct ShowColor<'a> {
    name: &'a str,
    value: &'a str,
    role: bool,
}

#[derive(Serialize)]
struct Show<'a> {
    name: &'a str,
    builtin: bool,
    description: Option<&'static str>,
    variant: Option<&'static str>,
    source: Option<&'static str>,
    missing_roles: Vec<&'static str>,
    colors: Vec<ShowColor<'a>>,
}

/// A palette by name: built-in (overlaid with the user's `palettes.<name>` if
/// any), else defined in the user config.
fn find(name: &str) -> Result<Palette, String> {
    let user = config::load(None).ok().map(|c| c.json);
    let doc = match user {
        Some(serde_json::Value::Object(mut m)) => {
            m.insert("palette".into(), serde_json::Value::String(name.into()));
            serde_json::Value::Object(m)
        }
        _ => serde_json::json!({ "palette": name }),
    };
    match palette::effective(&doc) {
        Ok(Some(p)) => Ok(p),
        Ok(None) => Err(format!("unknown palette '{name}'")),
        Err(e) => Err(e.message),
    }
}

fn show(name: &str, json: bool) -> i32 {
    let pal = match find(name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("tildy palette: {e}");
            eprintln!("hint: tildy palette list");
            return ErrorCode::ValidationFailed.exit_code();
        }
    };
    let info = palette::builtin(name);
    if json {
        let colors = pal.colors.iter().map(|(k, v)| ShowColor { name: k, value: v, role: ROLES.contains(&k.as_str()) }).collect();
        return print_json(&Show {
            name,
            builtin: info.is_some(),
            description: info.map(|i| i.description),
            variant: info.map(|i| i.variant),
            source: info.map(|i| i.source),
            missing_roles: pal.missing_roles(),
            colors,
        });
    }
    let get = |k: &str| pal.get(k).unwrap_or("").to_string();
    let term_bg = get("background");
    match info {
        Some(i) => println!("\x1b[1m── {}\x1b[0m  {}  \x1b[2m[{}, {}]\x1b[0m", i.name, description(i.description), i.variant, i.source),
        None => tr_println!("\x1b[1m── {}\x1b[0m  \x1b[2m[user config palettes.{}]\x1b[0m", "\x1b[1m── {}\x1b[0m  \x1b[2m[用户配置 palettes.{}]\x1b[0m", pal.name, pal.name),
    }
    let w = pal.colors.iter().map(|(k, _)| k.len()).max().unwrap_or(0).max(8);
    tr_println!("\x1b[2mRoles\x1b[0m", "\x1b[2m角色\x1b[0m");
    for r in ROLES {
        match pal.get(r) {
            Some(v) => println!("  {} {}  {}  \x1b[2m{}\x1b[0m", swatch(v, "    "), pad(r, w), pad(v, 8), role_note(r)),
            None => tr_println!("  {}  \x1b[33mUndefined (required by role-based presets)\x1b[0m", "  {}  \x1b[33m未定义（按角色着色的预设需要它）\x1b[0m", pad(r, w + 5)),
        }
    }
    tr_println!("\x1b[2mPalette colors\x1b[0m", "\x1b[2m配色自带的颜色\x1b[0m");
    for (k, v) in pal.colors.iter().filter(|(k, _)| !ROLES.contains(&k.as_str())) {
        println!("  {} {}  {}", swatch(v, "    "), pad(k, w), v);
    }
    // Segments and text as the role-based presets draw them.
    let fg = get("fg");
    let segs: String = ["bg1", "bg2", "bg3", "bg4", "bg5", "bg6"].iter().map(|r| paint(&fg, &get(r), &format!(" {r} "))).collect();
    let texts: String = ["accent", "ok", "warn", "err", "info", "fg_dim"].iter().map(|r| paint(&get(r), &term_bg, &format!(" {r}"))).collect();
    tr_println!("\x1b[2mExample\x1b[0m", "\x1b[2m示例\x1b[0m");
    println!("  {segs}");
    println!("  {}{}", texts, paint("", &term_bg, " "));
    println!("  {}", paint(&get("accent"), &get("surface"), " surface: accent ") + &paint(&get("ok"), &get("surface"), "ok ") + &paint(&get("err"), &get("surface"), "err "));
    0
}
