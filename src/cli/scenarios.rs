//! Fixed preview scenarios for `tildy preview`. Pure data: no repository
//! access, no command execution, no environment reads. The names are a stable
//! contract (agents may depend on them). Wall-clock time is fixed by the
//! caller (`preview::sample_now`, 14:05).
use tildy::config::{Config, ModuleType};
use tildy::model::{Freshness, GitData, GitState, ModuleValue, PromptData, PromptRequest, Slot};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(crate) struct ScenarioInfo {
    pub name: &'static str,
    pub description: &'static str,
    /// Terminal width used unless `--cols` overrides it.
    pub cols: u16,
}

pub(crate) const SCENARIOS: &[ScenarioInfo] = &[
    ScenarioInfo { name: "plain-dir", description: "Ordinary directory outside a repository", cols: 100 },
    ScenarioInfo { name: "home", description: "Home directory ~", cols: 100 },
    ScenarioInfo { name: "git-clean", description: "Clean Git repository (main)", cols: 100 },
    ScenarioInfo { name: "git-dirty", description: "Repository with unstaged, staged and untracked changes", cols: 100 },
    ScenarioInfo { name: "git-conflict", description: "Merge in progress with conflicts", cols: 100 },
    ScenarioInfo { name: "git-ahead-behind", description: "3 commits ahead, 1 behind upstream", cols: 100 },
    ScenarioInfo { name: "command-failed", description: "Previous command exited with status 1", cols: 100 },
    ScenarioInfo { name: "long-command", description: "Previous command took 12.3 s", cols: 100 },
    ScenarioInfo { name: "ssh", description: "SSH session (host build-01)", cols: 100 },
    ScenarioInfo { name: "narrow", description: "Narrow 40-column terminal with a deep path and dirty repository", cols: 40 },
    ScenarioInfo { name: "timeout", description: "Git timed out with no value available", cols: 100 },
    ScenarioInfo { name: "stale-cache", description: "Git and module data are stale (90 s)", cols: 100 },
    ScenarioInfo { name: "jobs", description: "Two background jobs", cols: 100 },
    ScenarioInfo { name: "languages", description: "Detected Node, Rust and Python versions", cols: 100 },
    ScenarioInfo { name: "agent-blocked", description: "Agent waiting for approval (1 other record)", cols: 100 },
    ScenarioInfo { name: "agent-error", description: "Agent error", cols: 100 },
];

pub(crate) fn find(name: &str) -> Option<&'static ScenarioInfo> {
    SCENARIOS.iter().find(|s| s.name == name)
}

const HOME: &str = "/Users/you";

fn git(branch: &str, state: GitState) -> GitData {
    GitData {
        in_repo: true,
        git_dir: Some("/Users/you/code/tildy/.git".into()),
        worktree_root: Some("/Users/you/code/tildy".into()),
        worktree_id: Some("/Users/you/code/tildy/.git".into()),
        branch: Some(branch.into()),
        upstream: Some(format!("origin/{branch}")),
        head_short: Some("1a2b3c4".into()),
        state,
        ..GitData::default()
    }
}

fn module(text: &str, fields: &[(&str, &str)]) -> Slot<ModuleValue> {
    Slot::fresh(ModuleValue {
        text: text.into(),
        fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        prerendered: None,
    })
}

fn not_detected() -> Slot<ModuleValue> {
    Slot { freshness: Freshness::Fresh, age_ms: Some(0), value: None, error: None }
}

fn stale<T>(mut s: Slot<T>, age_ms: u64) -> Slot<T> {
    s.freshness = Freshness::Stale;
    s.age_ms = Some(age_ms);
    s
}

/// The `agent_status` slot of the two agent scenarios, with the fields of the
/// M2 agent provider (`docs/architecture.md` section B).
fn agent(blocked: bool) -> Slot<ModuleValue> {
    let f: &[(&str, &str)] = if blocked {
        &[
            ("state", "blocked"),
            ("kind", "permission"),
            ("app", "claude-code"),
            ("title", "tildy"),
            ("msg", "Bash: rm -rf target"),
            ("id", "claude-code/ab12cd34"),
            ("age_s", "12"),
            ("count", "2"),
            ("blocked", "1"),
            ("error", "0"),
            ("done", "0"),
            ("working", "1"),
            ("idle", "0"),
            ("more", "1"),
        ]
    } else {
        &[
            ("state", "error"),
            ("app", "claude-code"),
            ("title", "tildy"),
            ("msg", "Build failed: 3 errors"),
            ("id", "claude-code/ab12cd34"),
            ("age_s", "5"),
            ("count", "1"),
            ("blocked", "0"),
            ("error", "1"),
            ("done", "0"),
            ("working", "0"),
            ("idle", "0"),
        ]
    };
    module(f.iter().find(|(k, _)| *k == "msg").map_or("", |(_, v)| v), f)
}

/// Prompt data for scenario `name` under `cfg` (which decides the module ids
/// that need slots). `cols` overrides the scenario's width. None when the name
/// is unknown.
pub(crate) fn build(name: &str, cfg: &Config, cols: Option<u16>) -> Option<PromptData> {
    let info = find(name)?;
    let repo_cwd = format!("{HOME}/code/tildy");
    let mut cwd = repo_cwd.clone();
    let mut g = Some(git("main", GitState::Clean));
    let mut git_fresh = Freshness::Fresh;
    let (mut last_exit, mut duration, mut jobs) = (0, 0, 0);
    let (mut ssh, mut hostname) = (false, "mbp");
    let mut env = "USER=you\u{1f}HOST=mbp".to_string();
    let mut languages = false;
    let mut agent_slot: Option<Slot<ModuleValue>> = None;
    let mut stale_all = false;
    match name {
        "plain-dir" => {
            cwd = format!("{HOME}/projects/notes");
            g = Some(GitData { in_repo: false, ..GitData::default() });
        }
        "home" => {
            cwd = HOME.into();
            g = Some(GitData { in_repo: false, ..GitData::default() });
        }
        "git-clean" => {}
        "git-dirty" => {
            g = Some(GitData { staged: 1, unstaged: 2, untracked: 1, ..git("main", GitState::Dirty) });
        }
        "git-conflict" => {
            g = Some(GitData {
                conflicted: 2,
                unstaged: 1,
                operation: Some("merge".into()),
                ..git("feature/preview", GitState::Conflicted)
            });
        }
        "git-ahead-behind" => g = Some(GitData { ahead: 3, behind: 1, ..git("main", GitState::Clean) }),
        "command-failed" => {
            last_exit = 1;
            duration = 120;
        }
        "long-command" => duration = 12_300,
        "ssh" => {
            ssh = true;
            hostname = "build-01";
            env = "USER=you\u{1f}HOST=build-01\u{1f}SSH_CONNECTION=10.0.0.5 51234 10.0.0.9 22\u{1f}SSH_TTY=/dev/ttys001".into();
        }
        "narrow" => {
            cwd = format!("{HOME}/code/work/clients/acme/services/billing-api");
            g = Some(GitData { unstaged: 2, untracked: 1, ..git("feature/invoice-export", GitState::Dirty) });
        }
        "timeout" => {
            git_fresh = Freshness::Timeout;
            g = None;
        }
        "stale-cache" => {
            g = Some(GitData { unstaged: 2, untracked: 1, ..git("main", GitState::Dirty) });
            git_fresh = Freshness::Stale;
            stale_all = true;
            languages = true;
        }
        "jobs" => jobs = 2,
        "languages" => languages = true,
        "agent-blocked" => agent_slot = Some(agent(true)),
        "agent-error" => agent_slot = Some(agent(false)),
        _ => return None,
    }
    let git_slot = match git_fresh {
        Freshness::Fresh => Slot::fresh(g.unwrap_or_default()),
        Freshness::Stale => stale(Slot::fresh(g.unwrap_or_default()), 90_000),
        f => Slot { freshness: f, age_ms: None, value: None, error: None },
    };
    let mut modules = BTreeMap::new();
    for (id, m) in cfg.layout_modules() {
        let slot = match (m.module_type, m.language.as_deref()) {
            (ModuleType::AgentStatus, _) => agent_slot.clone().unwrap_or_else(not_detected),
            (ModuleType::Language, Some("nodejs")) if languages => module("22.3.0", &[("version", "22.3.0")]),
            (ModuleType::Language, Some("rust")) if languages => module("1.82.0", &[("version", "1.82.0")]),
            (ModuleType::Python, _) if languages => module("3.12.1", &[("version", "3.12.1"), ("env", "demo")]),
            (t, _) if t.is_provider_backed() => not_detected(),
            _ => continue,
        };
        let slot = if stale_all && slot.value.is_some() { stale(slot, 90_000) } else { slot };
        modules.insert(id.to_string(), slot);
    }
    Some(PromptData {
        request: PromptRequest {
            gen: 1,
            session_id: "preview".into(),
            cwd: PathBuf::from(cwd),
            cols: cols.unwrap_or(info.cols),
            lines: 40,
            last_exit,
            last_duration_ms: duration,
            env,
            jobs,
        },
        home: Some(HOME.into()),
        git: git_slot,
        ssh,
        hostname: Some(hostname.into()),
        username: Some("you".into()),
        modules,
        changes: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tildy::config;

    #[test]
    fn every_scenario_builds_and_names_are_unique() {
        let cfg = config::builtin();
        let mut seen = std::collections::HashSet::new();
        for s in SCENARIOS {
            assert!(seen.insert(s.name));
            let d = build(s.name, &cfg, None).unwrap();
            assert_eq!(d.request.cols, s.cols);
        }
        assert_eq!(SCENARIOS.len(), 16);
        assert!(build("nope", &cfg, None).is_none());
    }

    /// The scenario fixtures under fixtures/render are generated from these
    /// builders: `TILDY_BLESS=1 cargo test fixture_inputs` rewrites them.
    #[test]
    fn fixture_inputs_match_builders() {
        const CASES: &[(&str, &str, &str)] = &[
            ("git-dirty", "unicode", "builtin"),
            ("git-conflict", "unicode", "builtin"),
            ("stale-cache", "unicode", "builtin"),
            ("languages", "unicode", "builtin"),
            ("ssh", "ascii", "builtin"),
            ("narrow", "unicode", "pastel-powerline"),
        ];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/render");
        for (name, tier, preset) in CASES {
            let cfg = if *preset == "builtin" { config::builtin() } else { config::preset(preset).unwrap() };
            let data = build(name, &cfg, None).unwrap();
            let input = serde_json::json!({
                "symbols": tier,
                "config": if *preset == "builtin" { serde_json::Value::Null } else { format!("preset:{preset}").into() },
                "now": "2026-10-19T14:05:00",
                "data": data,
            });
            let p = dir.join(format!("scenario_{}.input.json", name.replace('-', "_")));
            if std::env::var("TILDY_BLESS").is_ok() {
                std::fs::write(&p, serde_json::to_string_pretty(&input).unwrap() + "\n").unwrap();
                continue;
            }
            let want: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            assert_eq!(input, want, "{name}: fixture input drifted from the scenario builder");
        }
    }
}
