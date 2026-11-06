//! Foreground-only terminal width measurement; JSON stdout may be redirected.
use clap::Args;
use serde::Serialize;
use tildy::model::{CliEnvelope, ErrorCode};
use tildy::width::{self, Entry, Identity, WidthProfile};
use std::collections::BTreeSet;

#[derive(Args)]
pub struct CalibrateArgs {
    /// Explicit label identifying this font/configuration (required).
    #[arg(long)]
    pub font_revision: String,
    /// One grapheme cluster per occurrence, at most 64. Defaults cover CJK,
    /// combining, ZWJ emoji, variation selectors and a Nerd/private-use glyph.
    #[arg(long = "cluster", action = clap::ArgAction::Append)]
    pub clusters: Vec<String>,
    #[arg(long)]
    pub json: bool,
}
#[derive(Serialize)]
struct Calibration {
    path: std::path::PathBuf,
    revision: String,
    identity: Identity,
    entries: Vec<Entry>,
}
fn fail(json: bool, code: ErrorCode, message: String) -> i32 {
    if json {
        println!(
            "{}",
            serde_json::to_string(&CliEnvelope::<()>::err(code, message, None)).unwrap()
        );
    } else {
        eprintln!("tildy calibrate: {message}");
    }
    code.exit_code()
}
pub fn run(a: CalibrateArgs) -> i32 {
    let clusters = if a.clusters.is_empty() {
        vec![
            "中".into(),
            "e\u{301}".into(),
            "👩‍💻".into(),
            "☀️".into(),
            "\u{f120}".into(),
        ]
    } else {
        a.clusters
    };
    if clusters.len() > 64 {
        return fail(
            a.json,
            ErrorCode::ValidationFailed,
            "at most 64 samples may be calibrated at once".into(),
        );
    }
    let mut seen = BTreeSet::new();
    for cluster in &clusters {
        if let Err(message) = width::validate_cluster(cluster) {
            return fail(a.json, ErrorCode::ValidationFailed, message);
        }
        if !seen.insert(cluster) {
            return fail(
                a.json,
                ErrorCode::ValidationFailed,
                "duplicate cluster sample".into(),
            );
        }
    }
    let identity = match Identity::from_process(&a.font_revision) {
        Ok(v) => v,
        Err(message) => return fail(a.json, ErrorCode::ValidationFailed, message),
    };
    let entries = match width::probe::measure(&clusters) {
        Ok(v) => v,
        Err(message) => return fail(a.json, ErrorCode::EnvironmentUnavailable, message),
    };
    let profile = WidthProfile {
        version: width::PROFILE_VERSION,
        identity: identity.clone(),
        entries: entries.clone(),
    };
    let path = match width::save_profile(&profile) {
        Ok(v) => v,
        Err(message) => return fail(a.json, ErrorCode::EnvironmentUnavailable, message),
    };
    let data = Calibration {
        path,
        revision: width::revision(&profile),
        identity,
        entries,
    };
    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&CliEnvelope::ok(data)).unwrap()
        );
    } else {
        println!(
            "Saved {} measured clusters to {} (revision {})",
            data.entries.len(),
            data.path.display(),
            data.revision
        );
    }
    0
}
