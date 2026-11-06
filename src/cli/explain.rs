//! Explain the reports captured by a worker or collected by one real render.
use clap::Args;
use serde::Serialize;
use tildy::diagnostics::{ProviderCycleTiming, Snapshot};
use tildy::model::{ErrorCode, Freshness, ModuleReport};
use std::path::PathBuf;

#[derive(Args)]
pub struct ExplainArgs {
    #[arg(long, conflicts_with = "here")]
    pub session: Option<String>,
    /// Fetch real provider data and render the current directory once.
    #[arg(long)]
    pub here: bool,
    #[arg(long)]
    pub module: Option<String>,
    #[arg(long, requires = "here")]
    pub config: Option<PathBuf>,
    #[arg(long, requires = "here")]
    pub cols: Option<u16>,
    #[arg(long)]
    pub json: bool,
}
#[derive(Serialize)]
pub struct Explanation {
    pub module: ModuleReport,
    pub reason: String,
    pub provider: Option<String>,
    pub data_source: String,
    pub timing: Option<ProviderCycleTiming>,
}
#[derive(Serialize)]
struct ExplainData {
    session_id: Option<String>,
    config_revision: Option<String>,
    modules: Vec<Explanation>,
}

pub fn reason(r: &ModuleReport) -> String {
    reason_with(r, false)
}

fn reason_with(r: &ModuleReport, chinese: bool) -> String {
    let text = |en, zh| if chinese { zh } else { en };
    if r.visible {
        return text("Visible: conditions met and space available", "显示：条件满足且有可用空间").into();
    }
    match r.hidden_reason.as_deref().unwrap_or("unspecified") {
        "no_room" => match r.additional_columns {
            Some(columns) => if chinese { format!("隐藏：被 priority 收缩隐藏，需要再宽 {columns} 列") } else { format!("Hidden by priority: needs {columns} more columns") },
            None => text("Hidden by priority: required width unavailable in this report", "隐藏：被 priority 收缩隐藏，旧报告未记录所需列数").into(),
        },
        "no_data" => text("Hidden: no data available in this environment", "隐藏：当前环境没有可显示的数据").into(),
        "not_detected" => text("Hidden: no data detected in this directory or environment", "隐藏：当前目录或环境未检测到此模块的数据").into(),
        "disabled" => text("Hidden: disabled in configuration", "隐藏：配置禁用了此模块").into(),
        "empty" => text("Hidden: empty module", "隐藏：模块内容为空").into(),
        condition if condition.starts_with("show_when:") => {
            if chinese { format!("隐藏：显示条件未满足（{}）", &condition[10..]) } else { format!("Hidden: display condition not met ({})", &condition[10..]) }
        }
        other => if chinese { format!("隐藏：{other}") } else { format!("Hidden: {other}") },
    }
}

pub fn explanations(reports: Vec<ModuleReport>, snapshot: Option<&Snapshot>) -> Vec<Explanation> {
    reports
        .into_iter()
        .map(|r| {
            let timings = snapshot
                .and_then(|s| s.timing_history.last())
                .map(|c| c.providers.as_slice())
                .unwrap_or(&[]);
            let provider = snapshot
                .and_then(|s| s.slots.iter().find(|slot| slot.provider == r.id))
                .map(|slot| slot.provider.clone())
                .or_else(|| provider_key(&r, timings));
            let timing = provider
                .as_ref()
                .and_then(|p| timings.iter().find(|t| &t.provider == p))
                .cloned();
            let slot = snapshot.and_then(|s| {
                s.slots
                    .iter()
                    .find(|slot| provider.as_deref() == Some(slot.provider.as_str()))
            });
            let freshness = r.freshness.or_else(|| slot.map(|slot| slot.freshness));
            // cache_hit describes the whole cycle. A warm cycle can first
            // show cache and then finish an execution; classify the data now
            // displayed using the live slot's settlement as well.
            let completed_fresh = freshness == Some(Freshness::Fresh)
                && slot.is_some_and(|slot| !slot.fetching)
                && timing.as_ref().is_some_and(|t| t.completed);
            let showing_cache = slot.is_some_and(|slot| slot.fetching && slot.age_ms.is_some())
                || (timing.as_ref().is_some_and(|t| t.cache_hit) && !completed_fresh);
            let data_source = match freshness {
                Some(Freshness::Timeout) => "timeout",
                Some(Freshness::Error) => "error",
                Some(Freshness::Pending) => "pending",
                Some(Freshness::Stale) => "stale",
                _ if showing_cache => "cache",
                Some(Freshness::Fresh) => "fresh",
                None => "context",
            }
            .into();
            Explanation {
                reason: reason(&r),
                module: r,
                provider,
                data_source,
                timing,
            }
        })
        .collect()
}

fn provider_key(r: &ModuleReport, timings: &[ProviderCycleTiming]) -> Option<String> {
    // Provider-backed modules use their config id. Several render-only Git
    // modules share the git slot; hostname/username use static session keys.
    if timings.iter().any(|t| t.provider == r.id) {
        return Some(r.id.clone());
    }
    match r.module_type.as_str() {
        "git" | "git_branch" | "git_status" | "git_commit" => Some("git".into()),
        "hostname" => Some("session/hostname".into()),
        "username" => Some("session/username".into()),
        _ => None,
    }
}

pub fn run(a: ExplainArgs) -> i32 {
    let session = a.session.or_else(|| {
        (!a.here)
            .then(|| std::env::var("TILDY_SESSION_ID").ok())
            .flatten()
    });
    let (reports, snapshot, revision, here_timings) = if let Some(id) = session.as_deref() {
        match super::inspect::load_session(id) {
            Ok((_, s)) => (s.module_reports.clone(), Some(s), None, vec![]),
            Err((code, message)) => return super::inspect::fail(a.json, code, message, None),
        }
    } else {
        let cfg = match tildy::config::load(a.config.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                return super::inspect::fail(
                    a.json,
                    ErrorCode::ValidationFailed,
                    e.to_string(),
                    None,
                )
            }
        };
        let cwd = match std::env::current_dir() {
            Ok(p) => p,
            Err(e) => {
                return super::inspect::fail(
                    a.json,
                    ErrorCode::EnvironmentUnavailable,
                    e.to_string(),
                    None,
                )
            }
        };
        let cfg = if a.config.is_none() { tildy::config::project::for_directory(cfg, &cwd) } else { cfg };
        let (data, _, timings) = super::preview::here_data_with_timings(
            &cfg,
            &cwd,
            a.cols
                .map(usize::from)
                .unwrap_or_else(super::preview::terminal_cols),
        );
        let mut output = tildy::render::render(&cfg, &data);
        tildy::render::report::annotate(&cfg, None, &data, &mut output.reports);
        (output.reports, None, Some(cfg.revision), timings)
    };
    if reports.is_empty() {
        return super::inspect::fail(
            a.json,
            ErrorCode::BackendUnavailable,
            "the snapshot has no module reports; render a prompt with tildy 0.3.0-m3 or use --here"
                .into(),
            None,
        );
    }
    let selected: Vec<_> = reports
        .into_iter()
        .filter(|r| a.module.as_ref().is_none_or(|id| &r.id == id))
        .collect();
    if selected.is_empty() {
        return super::inspect::fail(
            a.json,
            ErrorCode::ValidationFailed,
            format!("unknown module {}", a.module.unwrap_or_default()),
            None,
        );
    }
    let mut modules = explanations(selected, snapshot.as_ref());
    if snapshot.is_none() {
        for e in &mut modules {
            e.provider = provider_key(&e.module, &here_timings);
            e.timing = e
                .provider
                .as_ref()
                .and_then(|p| here_timings.iter().find(|t| &t.provider == p))
                .cloned();
            if e.data_source == "context" && e.timing.is_some() {
                e.data_source = "fresh".into();
            }
        }
    }
    let data = ExplainData {
        session_id: session,
        config_revision: snapshot
            .as_ref()
            .and_then(|s| s.config_revision.clone())
            .or(revision),
        modules,
    };
    if a.json {
        super::inspect::emit(&data);
    } else {
        for e in data.modules {
            println!("{}  {}", e.module.id, reason_with(&e.module, super::i18n::chinese()));
            println!(
                "  source={} data={} age={} ms rules={} provider={} duration={} ms",
                e.module.source.as_deref().unwrap_or("unknown"),
                e.data_source,
                e.module
                    .age_ms
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into()),
                e.module.rules_hit.join(","),
                e.provider.as_deref().unwrap_or("-"),
                e.timing
                    .map(|t| t.duration_ms.to_string())
                    .unwrap_or_else(|| "-".into())
            );
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_tracks_warm_revalidation_settlement_and_pure_cache_hits() {
        let mut snapshot: Snapshot = serde_json::from_str(include_str!(
            "../../fixtures/protocol/snapshot.valid-m3.json"
        ))
        .unwrap();
        let report = snapshot.module_reports.first_mut().unwrap();
        report.visible = true;
        report.hidden_reason = None;
        report.freshness = Some(Freshness::Fresh);
        let reports = snapshot.module_reports.clone();
        let slot = snapshot
            .slots
            .iter_mut()
            .find(|slot| slot.provider == "git")
            .unwrap();
        slot.freshness = Freshness::Fresh;
        slot.age_ms = Some(100);
        slot.fetching = true;
        let timing = snapshot
            .timing_history
            .last_mut()
            .unwrap()
            .providers
            .first_mut()
            .unwrap();
        timing.cache_hit = true;
        timing.executed = true;
        timing.completed = false;
        assert_eq!(
            explanations(reports.clone(), Some(&snapshot))[0].data_source,
            "cache",
            "warm partial still displays cached data"
        );
        snapshot
            .slots
            .iter_mut()
            .find(|slot| slot.provider == "git")
            .unwrap()
            .fetching = false;
        snapshot
            .timing_history
            .last_mut()
            .unwrap()
            .providers
            .first_mut()
            .unwrap()
            .completed = true;
        assert_eq!(
            explanations(reports.clone(), Some(&snapshot))[0].data_source,
            "fresh",
            "settled fresh result replaces the cache despite the earlier hit"
        );
        snapshot
            .timing_history
            .last_mut()
            .unwrap()
            .providers
            .first_mut()
            .unwrap()
            .executed = false;
        snapshot
            .timing_history
            .last_mut()
            .unwrap()
            .providers
            .first_mut()
            .unwrap()
            .completed = false;
        assert_eq!(
            explanations(reports.clone(), Some(&snapshot))[0].data_source,
            "cache",
            "pure cache hit performed no execution"
        );
        snapshot
            .timing_history
            .last_mut()
            .unwrap()
            .providers
            .first_mut()
            .unwrap()
            .executed = true;
        snapshot
            .timing_history
            .last_mut()
            .unwrap()
            .providers
            .first_mut()
            .unwrap()
            .completed = true;
        snapshot
            .timing_history
            .last_mut()
            .unwrap()
            .providers
            .first_mut()
            .unwrap()
            .timed_out = true;
        assert_eq!(
            explanations(reports.clone(), Some(&snapshot))[0].data_source,
            "fresh",
            "a late fresh result does not inherit its earlier timeout as current freshness"
        );
        let mut stale = reports;
        stale[0].freshness = Some(Freshness::Stale);
        assert_eq!(
            explanations(stale, Some(&snapshot))[0].data_source,
            "stale",
            "cache and timeout history cannot mask currently stale data"
        );
    }

    #[test]
    fn every_scenario_module_has_a_nonempty_reason() {
        for preset in ["plain", "pastel-powerline"] {
            let cfg = tildy::config::preset(preset).unwrap();
            for scene in crate::cli::scenarios::SCENARIOS {
                let data =
                    crate::cli::scenarios::build(scene.name, &cfg, Some(scene.cols)).unwrap();
                let output =
                    tildy::render::report::render_annotated(&cfg, Some(preset), &data, None, false);
                for report in output.reports {
                    assert!(!reason(&report).is_empty(), "{preset}/{}", scene.name);
                }
            }
        }
    }
}
