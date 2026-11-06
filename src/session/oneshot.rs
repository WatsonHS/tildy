//! One-shot backend core (`tildy render`): one REQ in, one RES out.
//!
//! Providers run in parallel threads under one overall deadline. The RES is
//! `fresh` when every provider answered in time, otherwise `partial` with the
//! missing slots marked `timeout`. `worker_epoch` is 0. No HELLO is sent.
//!
//! The config is loaded first (after the REQ is known to be valid) because it
//! decides the provider set (`PromptRenderer::providers`). There is no
//! cross-process cache: config module version commands run here under their
//! own timeouts, in parallel with git, bounded by the overall deadline. This
//! is the fallback path; the worker keeps them off the critical path.

use super::{assemble, read_line_bounded, InputLine, PromptRenderer, ProviderSlot};
use crate::model::{Freshness, RenderStatus};
use crate::protocol::{self, ProtocolError, ShellFrame, MAX_REQUEST_BYTES};
use crate::providers::{Provider, ProviderValue};
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub const DEFAULT_RENDER_TIMEOUT: Duration = Duration::from_millis(2000);

#[derive(Debug, Clone)]
pub struct OneshotOptions {
    /// Overall deadline for all providers (`TILDY_RENDER_TIMEOUT_MS`).
    pub timeout: Duration,
    pub home: Option<PathBuf>,
}

impl Default for OneshotOptions {
    fn default() -> Self {
        OneshotOptions { timeout: DEFAULT_RENDER_TIMEOUT, home: std::env::var_os("HOME").map(PathBuf::from) }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OneshotError {
    /// Bad or missing REQ: exit 5, message on stderr, nothing on stdout.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// stdout closed (EPIPE): exit quietly.
    #[error("output closed")]
    OutputClosed,
}

/// Parse `TILDY_RENDER_TIMEOUT_MS`-style values: a positive integer of
/// milliseconds, anything else means the default.
pub fn parse_timeout_ms(raw: Option<&str>) -> Duration {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_RENDER_TIMEOUT)
}

/// Returns the status of the RES written, or `None` if the input was `BYE`
/// (nothing is written). After a valid REQ the renderer is refreshed and its
/// providers are used; `make_providers` runs only when it has none.
pub fn run_oneshot<R, W, F>(
    input: R,
    mut output: W,
    make_providers: F,
    renderer: &mut dyn PromptRenderer,
    opts: &OneshotOptions,
) -> Result<Option<RenderStatus>, OneshotError>
where
    R: Read,
    W: Write,
    F: FnOnce() -> Vec<Arc<dyn Provider>>,
{
    let start = Instant::now();
    let mut reader = BufReader::new(input);
    let line = match read_line_bounded(&mut reader, MAX_REQUEST_BYTES) {
        Ok(Some(InputLine::Line(l))) => l,
        Ok(Some(InputLine::TooLong)) => return Err(OneshotError::Protocol(ProtocolError::TooLong.to_string())),
        Ok(None) => return Err(OneshotError::Protocol("no request on stdin".into())),
        Err(e) => return Err(OneshotError::Protocol(format!("cannot read stdin: {e}"))),
    };
    let mut req = match protocol::parse_line(&line) {
        Ok(Some(ShellFrame::Request(r))) => r,
        Ok(Some(ShellFrame::Bye)) => return Ok(None),
        Ok(Some(ShellFrame::Ack { .. })) => {
            return Err(OneshotError::Protocol("expected REQ, got ACK".into()))
        }
        Ok(None) => return Err(OneshotError::Protocol("empty request line".into())),
        Err(e) => return Err(OneshotError::Protocol(e.to_string())),
    };
    // The instant-prompt key is for the worker's cache; render never writes it.
    let _ = super::instant::take_key(&mut req);
    let _ = super::instant::take_shown(&mut req);
    let presentation = super::instant::PresentationEnv::take(&mut req);
    let deadline = start + opts.timeout;
    renderer.refresh_for(&req.cwd);
    let providers = renderer.providers().unwrap_or_else(make_providers);
    let providers = providers.as_slice();

    let (tx, rx) = mpsc::channel::<(usize, Option<ProviderValue>)>();
    for (i, p) in providers.iter().enumerate() {
        let provider = Arc::clone(p);
        let req = req.clone();
        let guard = Guard { tx: tx.clone(), index: i, sent: false };
        let spawned = thread::Builder::new()
            .name(format!("tildy-fetch-{}", p.spec().key))
            .spawn(move || {
                let value = provider.fetch(&req);
                guard.finish(value);
            });
        if spawned.is_err() {
            let _ = tx.send((i, None));
        }
    }
    drop(tx);

    let mut results: Vec<Option<Result<ProviderValue, String>>> = vec![None; providers.len()];
    let mut remaining = providers.len();
    while remaining > 0 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((i, value)) => {
                if results[i].is_none() {
                    remaining -= 1;
                }
                results[i] = Some(value.ok_or_else(|| format!("provider {} failed", providers[i].spec().key)));
            }
            Err(_) => break,
        }
    }
    let timeout_ms = opts.timeout.as_millis();
    let slots: Vec<ProviderSlot> = results
        .into_iter()
        .zip(providers)
        .map(|(r, p)| {
            let key = p.spec().key;
            match r {
                Some(Ok(v)) => ProviderSlot { key, freshness: Freshness::Fresh, age_ms: Some(0), value: Some(v), error: None },
                Some(Err(e)) => ProviderSlot { key, freshness: Freshness::Error, age_ms: None, value: None, error: Some(e) },
                None => ProviderSlot {
                    key,
                    freshness: Freshness::Timeout,
                    age_ms: None,
                    value: None,
                    error: Some(format!("no result within {timeout_ms} ms")),
                },
            }
        })
        .collect();
    let mut data = assemble(&req, opts.home.clone(), &slots);
    presentation.restore(&mut data.request);
    let mut prompt = renderer.render(&data);
    prompt.status = if remaining == 0 { RenderStatus::Fresh } else { RenderStatus::Partial };
    let frame = protocol::encode_result(req.gen, 0, &prompt);
    output
        .write_all(frame.as_bytes())
        .and_then(|_| output.flush())
        .map_err(|_| OneshotError::OutputClosed)?;
    Ok(Some(prompt.status))
}

struct Guard {
    tx: mpsc::Sender<(usize, Option<ProviderValue>)>,
    index: usize,
    sent: bool,
}

impl Guard {
    fn finish(mut self, value: ProviderValue) {
        self.sent = true;
        let _ = self.tx.send((self.index, Some(value)));
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self.tx.send((self.index, None));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_env_parsing() {
        assert_eq!(parse_timeout_ms(None), DEFAULT_RENDER_TIMEOUT);
        assert_eq!(parse_timeout_ms(Some("150")), Duration::from_millis(150));
        assert_eq!(parse_timeout_ms(Some(" 7 ")), Duration::from_millis(7));
        for bad in ["0", "-5", "abc", "", "1.5"] {
            assert_eq!(parse_timeout_ms(Some(bad)), DEFAULT_RENDER_TIMEOUT, "{bad}");
        }
    }
}
