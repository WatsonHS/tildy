//! Importer goldens for Starship 1.24.2's built-in presets
//! (`starship preset NAME > fixtures/import/presets/NAME.toml`).
//!
//! Each TOML is imported in commands mode (what the built-in tildy presets
//! start from) and compared with `fixtures/import/presets/NAME.expected.json`
//! (`candidate` + `unsupported` + `skipped`). The catppuccin flavors import the
//! same TOML with another palette. Run with TILDY_BLESS=1 to rewrite.
use serde_json::{json, Value};
use tildy::import::{import_starship_with, ImportOptions};
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/import/presets")
}

/// (golden name, toml name, palette override)
const CASES: &[(&str, &str, Option<&str>)] = &[
    ("bracketed-segments", "bracketed-segments", None),
    ("catppuccin-powerline", "catppuccin-powerline", None),
    ("catppuccin-latte", "catppuccin-powerline", Some("catppuccin_latte")),
    ("catppuccin-frappe", "catppuccin-powerline", Some("catppuccin_frappe")),
    ("catppuccin-macchiato", "catppuccin-powerline", Some("catppuccin_macchiato")),
    ("gruvbox-rainbow", "gruvbox-rainbow", None),
    ("jetpack", "jetpack", None),
    ("nerd-font-symbols", "nerd-font-symbols", None),
    ("no-empty-icons", "no-empty-icons", None),
    ("no-nerd-font", "no-nerd-font", None),
    ("no-runtime-versions", "no-runtime-versions", None),
    ("pastel-powerline", "pastel-powerline", None),
    ("plain-text-symbols", "plain-text-symbols", None),
    ("pure-preset", "pure-preset", None),
    ("tokyo-night", "tokyo-night", None),
];

#[test]
fn starship_presets_import_to_golden_candidates() {
    for (name, toml, palette) in CASES {
        let src = std::fs::read_to_string(dir().join(format!("{toml}.toml"))).unwrap();
        let opts = ImportOptions { palette: palette.map(str::to_string), commands: true };
        let r = import_starship_with(&src, &opts).unwrap_or_else(|e| panic!("{name}: {e}"));
        // Every candidate loads as a tildy config.
        tildy::config::from_str(&r.candidate.to_string()).unwrap_or_else(|e| panic!("{name}: candidate invalid: {e}"));
        let got = json!({ "candidate": r.candidate, "unsupported": r.unsupported, "skipped": r.skipped });
        let p = dir().join(format!("{name}.expected.json"));
        if std::env::var("TILDY_BLESS").is_ok() {
            std::fs::write(&p, serde_json::to_string_pretty(&got).unwrap() + "\n").unwrap();
            continue;
        }
        let want: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|_| panic!("missing {name}.expected.json")))
            .unwrap();
        assert_eq!(got, want, "importer golden mismatch: {name} (TILDY_BLESS=1 to rewrite)");
    }
}

#[test]
fn catppuccin_flavors_differ_only_in_the_palette() {
    let src = std::fs::read_to_string(dir().join("catppuccin-powerline.toml")).unwrap();
    let get = |p: &str| import_starship_with(&src, &ImportOptions { palette: Some(p.into()), commands: true }).unwrap().candidate;
    let (mocha, latte) = (get("catppuccin_mocha"), get("catppuccin_latte"));
    // The starship.toml palettes are kept; colors stay by name.
    assert_eq!(mocha["modules"], latte["modules"]);
    assert_eq!(mocha["modules"]["cwd"]["style"]["background"], "peach");
    assert_eq!((mocha["palette"].as_str(), latte["palette"].as_str()), (Some("catppuccin_mocha"), Some("catppuccin_latte")));
    assert_eq!(mocha["palettes"].as_object().unwrap().len(), 4);
    // The loader resolves them per palette.
    let bg = |c: &Value| tildy::config::from_str(&c.to_string()).unwrap().file.modules["cwd"].style.background.clone().unwrap();
    assert_eq!(bg(&mocha), "#FAB387");
    assert_eq!(bg(&latte), "#FE640B");
}
