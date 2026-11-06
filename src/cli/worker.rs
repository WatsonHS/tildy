//! Persistent backend. Owner: task B.
//!
//! Wires the real config, renderer and providers into
//! `tildy::session::worker::run_worker`. All protocol and scheduling logic is
//! in the library so it can be tested with fakes. The worker starts with the
//! static providers; the first request loads the config and switches to
//! `providers::from_config`, and every config change rebuilds that set.
use clap::Args;
use tildy::config::{self, Config};
use tildy::model::{PromptData, RenderedPrompt};
use tildy::providers::{self, Provider};
use tildy::session::worker::{run_worker, SnapshotTarget, WorkerOptions};
use tildy::session::PromptRenderer;
use std::path::PathBuf;
use std::sync::Arc;
use std::os::unix::fs::MetadataExt;
use std::time::SystemTime;

#[derive(Args)]
pub struct WorkerArgs {
    #[arg(long)]
    pub config: Option<std::path::PathBuf>,
    /// Parent shell pid; worker exits when it disappears (in addition to stdin EOF).
    #[arg(long)]
    pub parent_pid: Option<u32>,
    /// Snapshot file path override.
    #[arg(long)]
    pub snapshot: Option<std::path::PathBuf>,
}

pub fn run(a: WorkerArgs) -> i32 {
    let renderer = Box::new(ConfigRenderer::new(a.config, true));
    let opts = WorkerOptions {
        parent_pid: a.parent_pid.filter(|&p| p > 0),
        snapshot: a.snapshot.map(SnapshotTarget::Path).unwrap_or(SnapshotTarget::Auto),
        ..WorkerOptions::default()
    };
    // Exit reasons (EOF, BYE, parent gone, EPIPE) are all normal terminations.
    let _ = run_worker(std::io::stdin(), std::io::stdout().lock(), all_providers, renderer, opts);
    0
}

/// The static providers (session + git), used until a config is loaded.
pub(crate) fn all_providers() -> Vec<Arc<dyn Provider>> {
    providers::all().into_iter().map(Arc::from).collect()
}

/// Modification and inode identity, or `None` when the file does not exist.
type Stamp = Option<(Option<SystemTime>, u64, u64, i64, i64)>;

fn stamp(path: &std::path::Path, follow: bool) -> Stamp {
    let metadata = if follow { std::fs::metadata(path) } else { std::fs::symlink_metadata(path) };
    metadata.ok().map(|m| (m.modified().ok(), m.len(), m.ino(), m.ctime(), m.ctime_nsec()))
}

/// `config::load` + `render::render`, falling back to `config::builtin()` when
/// the user file is invalid. With `watch`, `refresh` (called once per request)
/// stats the file and reloads it when its mtime or size changed. The config
/// epoch advances when the effective config's revision changes, which makes
/// the worker rebuild its providers from it.
pub(crate) struct ConfigRenderer {
    explicit: Option<PathBuf>,
    path: Option<PathBuf>,
    resolved: bool,
    stamp: Stamp,
    project_path: Option<PathBuf>,
    project_stamp: Stamp,
    config: Option<Config>,
    degraded: Vec<String>,
    /// Modules that got no provider (unknown language, bad regex, ...).
    provider_problems: Vec<String>,
    watch: bool,
    epoch: u64,
    epoch_revision: Option<String>,
    reports: Vec<tildy::model::ModuleReport>,
    report_metadata: tildy::render::report::AnnotationMetadata,
}

impl ConfigRenderer {
    pub(crate) fn new(explicit: Option<PathBuf>, watch: bool) -> Self {
        ConfigRenderer {
            explicit,
            path: None,
            resolved: false,
            stamp: None,
            project_path: None,
            project_stamp: None,
            config: None,
            degraded: Vec::new(),
            provider_problems: Vec::new(),
            watch,
            epoch: 0,
            epoch_revision: None,
            reports: vec![],
            report_metadata: Default::default(),
        }
    }

    fn load(&mut self) {
        match config::load(self.path.as_deref()) {
            Ok(c) => {
                self.config = Some(c);
                self.degraded.clear();
            }
            Err(e) => {
                let shown = self.path.as_deref().map(|p| p.display().to_string()).unwrap_or_default();
                eprintln!("tildy: invalid config {shown}: {e:#}; using built-in defaults");
                self.config = Some(config::builtin());
                self.degraded = vec![format!("config_invalid: {e}")];
            }
        }
        let base = self.config.take().expect("load sets a config");
        let config = config::project::apply(base, self.project_path.as_deref());
        if let Some(layer) = config.project.as_ref().filter(|p| !p.valid) {
            let problem = layer.problem.as_ref().expect("invalid layer has problem");
            self.degraded.push(format!("project_config_invalid: {}: {}", layer.path.display(), problem.message));
        }
        self.config = Some(config);
        self.report_metadata = tildy::render::report::AnnotationMetadata::new(self.config.as_ref().expect("load sets a config"), None);
        let revision = self.config.as_ref().map(|c| c.revision.clone());
        if revision != self.epoch_revision {
            self.epoch_revision = revision;
            self.epoch += 1;
        }
    }
}

impl PromptRenderer for ConfigRenderer {
    fn refresh(&mut self) {
        self.refresh_for(&std::env::current_dir().unwrap_or_default());
    }

    fn refresh_for(&mut self, cwd: &std::path::Path) {
        if !self.resolved {
            self.path = self.explicit.clone().or_else(config::default_config_path);
            self.resolved = true;
        }
        if self.config.is_some() && !self.watch {
            return;
        }
        let now = self.path.as_deref().and_then(|p| stamp(p, true));
        let project_path = config::project::discover(cwd);
        let project_stamp = project_path.as_deref().and_then(|p| stamp(p, false));
        if self.config.is_some() && now == self.stamp && project_path == self.project_path && project_stamp == self.project_stamp {
            return;
        }
        self.stamp = now;
        self.project_path = project_path;
        self.project_stamp = project_stamp;
        self.load();
    }

    fn config_epoch(&self) -> u64 {
        self.epoch
    }

    fn providers(&mut self) -> Option<Vec<Arc<dyn Provider>>> {
        let config = self.config.as_ref()?;
        let (list, problems) = providers::from_config_checked(config);
        for p in &problems {
            eprintln!("tildy: {p}");
        }
        self.provider_problems = problems;
        Some(list.into_iter().map(Arc::from).collect())
    }

    fn render(&mut self, data: &PromptData) -> RenderedPrompt {
        if self.config.is_none() {
            self.refresh_for(&data.request.cwd);
        }
        let config = self.config.get_or_insert_with(config::builtin);
        let mut out = tildy::render::render(config, data);
        tildy::render::report::annotate_prepared(config, &self.report_metadata, data, &mut out.reports);
        self.reports = out.reports;
        out.prompt
    }

    fn reports(&self) -> Vec<tildy::model::ModuleReport> {
        self.reports.clone()
    }

    fn revision(&self) -> Option<String> {
        self.config.as_ref().map(|c| c.revision.clone())
    }

    fn degraded(&self) -> Vec<String> {
        let modules = self.provider_problems.iter().map(|p| format!("module_invalid: {p}"));
        self.degraded.iter().cloned().chain(modules).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_symlink_target_edits_still_reload() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        let link = dir.path().join("config.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut renderer = ConfigRenderer::new(Some(link), true);
        for color in ["red", "blue"] {
            std::fs::write(&target, format!(r#"{{"schema_version":1,"modules":{{"cwd":{{"style":{{"foreground":"{color}"}}}}}}}}"#)).unwrap();
            renderer.refresh_for(dir.path());
            assert_eq!(renderer.config.as_ref().unwrap().file.modules["cwd"].style.foreground.as_deref(), Some(color));
        }
    }

    #[test]
    fn request_directory_switch_and_project_edits_refresh_sources_and_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let config = dir.path().join("user.json");
        std::fs::write(&config, r#"{"schema_version":1,"preset":"plain","layout":{"rows":[{"left":["cwd"]}]}}"#).unwrap();
        let project = repo.join(".tildy.json");
        let input: serde_json::Value = serde_json::from_str(include_str!("../../fixtures/render/git_clean.input.json")).unwrap();
        let mut data: PromptData = serde_json::from_value(input["data"].clone()).unwrap();
        let mut renderer = ConfigRenderer::new(Some(config), true);
        renderer.refresh_for(dir.path());
        let base_revision = renderer.revision();
        for (text, source, invalid) in [
            (Some(r#"{"schema_version":1,"modules":{"cwd":{"style":{"foreground":"red"}}}}"#), "project", false),
            (Some(r#"{"schema_version":1,"modules":{"cwd":{"style":{"foreground":"blue"}}}}"#), "project", false),
            (Some(r#"{"schema_version":1,"agent":{"wrap":true}}"#), "builtin", true),
            (None, "builtin", false),
        ] {
            if let Some(text) = text { std::fs::write(&project, text).unwrap(); } else { std::fs::remove_file(&project).unwrap(); }
            data.request.cwd = repo.clone();
            renderer.refresh_for(&data.request.cwd);
            renderer.render(&data);
            assert_eq!(renderer.reports()[0].source.as_deref(), Some(source));
            assert_eq!(!renderer.degraded().is_empty(), invalid);
            assert_eq!(renderer.revision() == base_revision, source != "project");
            let epoch = renderer.config_epoch();
            renderer.refresh_for(&repo);
            assert_eq!(renderer.config_epoch(), epoch, "unchanged request must reuse the config");
        }
        std::fs::write(&project, r#"{"schema_version":1,"symbols":"ascii"}"#).unwrap();
        renderer.refresh_for(&repo);
        assert_ne!(renderer.revision(), base_revision);
        renderer.refresh_for(dir.path());
        assert_eq!(renderer.revision(), base_revision);
        assert!(renderer.config.as_ref().unwrap().project.is_none());
    }

    #[test]
    fn report_metadata_reloads_with_config_and_invalid_or_missing_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let input: serde_json::Value = serde_json::from_str(include_str!("../../fixtures/render/git_clean.input.json")).unwrap();
        let data: PromptData = serde_json::from_value(input["data"].clone()).unwrap();
        let mut renderer = ConfigRenderer::new(Some(path.clone()), true);
        let cases = [
            (Some(r#"{"schema_version":1,"preset":"lean","layout":{"rows":[{"left":["cwd"]}]}}"#), "preset:lean", false),
            (Some(r#"{"schema_version":1,"preset":"pastel-powerline","modules":{"cwd":{"style":{"bold":true}}},"layout":{"rows":[{"left":["cwd"]}]}}"#), "user", false),
            (Some("invalid config"), "preset:pastel-powerline", true),
            (Some(r#"{"schema_version":1,"preset":"tokyo-night","layout":{"rows":[{"left":["cwd"]}]}}"#), "preset:tokyo-night", false),
            (None, "preset:pastel-powerline", false),
        ];
        for (contents, expected, degraded) in cases {
            if let Some(contents) = contents { std::fs::write(&path, contents).unwrap(); } else { std::fs::remove_file(&path).unwrap(); }
            renderer.refresh();
            renderer.render(&data);
            let reports = renderer.reports();
            assert_eq!(reports.iter().find(|r| r.id == "cwd").unwrap().source.as_deref(), Some(expected));
            assert_eq!(!renderer.degraded().is_empty(), degraded);
            let config = renderer.config.as_ref().unwrap();
            let mut reference = tildy::render::render(config, &data).reports;
            tildy::render::report::annotate(config, None, &data, &mut reference);
            assert_eq!(serde_json::to_value(&reports).unwrap(), serde_json::to_value(reference).unwrap());
            let epoch = renderer.config_epoch();
            renderer.refresh();
            renderer.render(&data);
            assert_eq!(renderer.config_epoch(), epoch);
            assert_eq!(serde_json::to_value(renderer.reports()).unwrap(), serde_json::to_value(&reports).unwrap());
        }
    }
}
