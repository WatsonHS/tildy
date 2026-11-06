//! One-shot backend. Owner: task B.
//!
//! Reads one REQ from stdin, loads the config, runs the config's providers
//! (`providers::from_config`) in parallel under
//! `TILDY_RENDER_TIMEOUT_MS` (default 2000), renders and writes one RES
//! (`fresh`, or `partial` if the deadline hit), exit 0. A protocol error exits 5
//! with a message on stderr and nothing on stdout.
use clap::Args;
use tildy::model::ErrorCode;
use tildy::session::oneshot::{parse_timeout_ms, run_oneshot, OneshotError, OneshotOptions};

#[derive(Args)]
pub struct RenderArgs {
    /// Config file override (default: see config::default_config_path).
    #[arg(long)]
    pub config: Option<std::path::PathBuf>,
}

pub fn run(a: RenderArgs) -> i32 {
    let opts = OneshotOptions {
        timeout: parse_timeout_ms(std::env::var("TILDY_RENDER_TIMEOUT_MS").ok().as_deref()),
        ..OneshotOptions::default()
    };
    let mut renderer = super::worker::ConfigRenderer::new(a.config, false);
    let stdin = std::io::stdin().lock();
    match run_oneshot(stdin, std::io::stdout().lock(), super::worker::all_providers, &mut renderer, &opts) {
        Ok(_) | Err(OneshotError::OutputClosed) => 0,
        Err(e @ OneshotError::Protocol(_)) => {
            eprintln!("tildy render: {e}");
            ErrorCode::BackendUnavailable.exit_code()
        }
    }
}
