//! Bounded per-cycle provider timings, oldest first within the selected tail.
use clap::Args;
use tildy::model::ErrorCode;
#[derive(Args)]
pub struct TimingsArgs {
    #[arg(long)]
    pub session: Option<String>,
    /// Number of cycles, 1..=32 (default 10).
    #[arg(long, default_value = "10", allow_hyphen_values = true)]
    pub last: String,
    #[arg(long)]
    pub json: bool,
}
pub fn run(a: TimingsArgs) -> i32 {
    let last = a
        .last
        .parse::<usize>()
        .ok()
        .filter(|last| (1..=tildy::diagnostics::TIMING_HISTORY_LIMIT).contains(last));
    let Some(last) = last else {
        return super::inspect::fail(
            a.json,
            ErrorCode::ValidationFailed,
            "--last must be between 1 and 32".into(),
            None,
        );
    };
    let Some(id) = a.session.or_else(|| std::env::var("TILDY_SESSION_ID").ok()) else {
        return super::inspect::fail(
            a.json,
            ErrorCode::ValidationFailed,
            "--session is required outside a tildy shell".into(),
            None,
        );
    };
    let (_, snapshot) = match super::inspect::load_session(&id) {
        Ok(s) => s,
        Err((code, message)) => return super::inspect::fail(a.json, code, message, None),
    };
    let cycles = &snapshot.timing_history[snapshot.timing_history.len().saturating_sub(last)..];
    if a.json {
        super::inspect::emit(&cycles);
    } else {
        println!("gen  at_ms          provider              ms  cache executed completed timeout coalesced");
        for c in cycles {
            for p in &c.providers {
                println!(
                    "{}  {}  {:20} {:5}  {} {} {} {} {}",
                    c.gen,
                    c.at_ms,
                    p.provider,
                    p.duration_ms,
                    p.cache_hit,
                    p.executed,
                    p.completed,
                    p.timed_out,
                    p.coalesced_count
                );
            }
        }
    }
    0
}
