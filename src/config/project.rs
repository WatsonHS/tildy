//! Display-only repository overrides. Never feeds the user-file transaction store.
use super::{Config, ConfigError, ErrorKind};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = ".tildy.json";
pub const MAX_BYTES: usize = 64 * 1024;
pub const ROOT_FIELDS: &[&str] = &["schema_version", "$comment", "symbols", "palette", "palettes", "add_newline", "modules", "layout"];
pub const MODULE_FIELDS: &[&str] = &["$comment", "style", "symbol", "format", "padding", "icon_padding", "priority", "show_when", "success_style", "error_style", "error_symbol"];

#[derive(Debug, Clone, Serialize)]
pub struct Layer {
    pub path: PathBuf,
    pub exists: bool,
    pub valid: bool,
    pub revision: Option<String>,
    pub problem: Option<super::transaction::Problem>,
    /// Only a successfully applied layer can claim ownership of fields.
    #[serde(skip)]
    pub applied: Option<Value>,
}

/// Nearest Git worktree (including a .git file in linked worktrees/submodules).
/// Never searches above that boundary and never invokes Git or executes config.
pub fn discover(cwd: &Path) -> Option<PathBuf> {
    if std::env::var_os("TILDY_PROJECT_CONFIG").is_some_and(|v| v == "0") {
        return None;
    }
    let cwd = cwd.canonicalize().ok()?;
    if !cwd.is_dir() { return None; }
    cwd.ancestors().find(|p| p.join(".git").is_dir() || p.join(".git").is_file()).map(|p| p.join(FILE_NAME))
}

fn invalid(path: impl Into<String>, message: impl Into<String>) -> ConfigError {
    ConfigError { kind: ErrorKind::Invalid, path: path.into(), message: message.into() }
}

/// Validate the restricted input *before* merging, then validate the complete
/// config. Layout may only hide/reorder already placed modules: otherwise it
/// could activate a dormant user command, despite not changing its definition.
pub fn parse(base: &Config, text: &str) -> Result<Config, ConfigError> {
    if text.len() > MAX_BYTES { return Err(invalid("", "project config exceeds 64 KiB")); }
    let layer: Value = serde_json::from_str(text)
        .map_err(|e| ConfigError { kind: ErrorKind::Syntax, path: String::new(), message: e.to_string() })?;
    let root = layer.as_object().ok_or_else(|| invalid("", "expected an object"))?;
    if root.get("schema_version") != Some(&Value::from(1)) {
        return Err(invalid("schema_version", "project config requires schema_version 1"));
    }
    for key in root.keys() {
        if !ROOT_FIELDS.contains(&key.as_str()) {
            return Err(invalid(key, "not allowed in project config (display overrides only)"));
        }
    }
    if let Some(modules) = root.get("modules") {
        let modules = modules.as_object().ok_or_else(|| invalid("modules", "expected an object"))?;
        for (id, module) in modules {
            if !base.file.modules.contains_key(id) {
                return Err(invalid(format!("modules.{id}"), "project config cannot define new modules"));
            }
            let module = module.as_object().ok_or_else(|| invalid(format!("modules.{id}"), "expected an object"))?;
            for key in module.keys() {
                if !MODULE_FIELDS.contains(&key.as_str()) {
                    return Err(invalid(format!("modules.{id}.{key}"), "not allowed in project config (display overrides only)"));
                }
            }
        }
    }
    let mut effective = base.json.clone();
    super::merge(&mut effective, layer);
    let mut result = super::build(effective, base.revision.clone(), base.user.clone(), base.source.clone())?;
    let placed: BTreeSet<_> = base.layout_modules().into_iter().map(|(id, _)| id).collect();
    for (id, _) in result.layout_modules() {
        if !placed.contains(id) {
            return Err(invalid("layout.rows", format!("project config cannot activate unplaced module '{id}'")));
        }
    }
    // Include both layers, even if a comment-only change leaves rendering equal.
    result.revision = super::content_revision(format!("{}\0{text}", base.revision).as_bytes());
    Ok(result)
}

/// Missing/invalid repository files keep the entire lower configuration intact.
/// NOFOLLOW + NONBLOCK and the descriptor metadata check avoid symlinks/FIFOs;
/// bounded reads also handle a regular file growing after the metadata check.
pub fn apply(mut base: Config, path: Option<&Path>) -> Config {
    let Some(path) = path else { return base };
    let mut layer = Layer { path: path.to_path_buf(), exists: true, valid: true, revision: None, problem: None, applied: None };
    let read = (|| -> std::io::Result<String> {
        let file = std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES as u64 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "project config must be a regular file of at most 64 KiB"));
        }
        let mut text = String::new();
        file.take((MAX_BYTES + 1) as u64).read_to_string(&mut text)?;
        Ok(text)
    })();
    let result = match read {
        Ok(text) => {
            layer.revision = Some(super::content_revision(text.as_bytes()));
            match parse(&base, &text) {
                Ok(config) => {
                    layer.applied = serde_json::from_str(&text).ok();
                    base = config;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => { layer.exists = false; Ok(()) }
        Err(e) => Err(ConfigError { kind: ErrorKind::Io, path: String::new(), message: e.to_string() }),
    };
    if let Err(e) = result {
        layer.valid = false;
        layer.problem = Some(super::transaction::Problem::from(&e));
    }
    base.project = Some(layer);
    base
}

pub fn for_directory(base: Config, cwd: &Path) -> Config {
    apply(base, discover(cwd).as_deref())
}

/// A self-contained schema using the same definitions as the user schema.
/// Cross-layer constraints (existing/placed ids) are checked by `parse`.
pub fn schema() -> Value {
    let mut schema: Value = serde_json::from_str(super::SCHEMA_V1_JSON).expect("embedded schema");
    schema.as_object_mut().unwrap().remove("$id");
    schema["description"] = Value::String("Display-only overrides at the nearest Git worktree root. Requires schema_version 1. Modules must already exist, and layout may only place ids already placed by the lower layers. Validate with tildy config validate --project --file FILE.".into());
    schema["title"] = Value::String("Tildy repository display overrides (.tildy.json)".into());
    schema["properties"].as_object_mut().unwrap().retain(|k, _| ROOT_FIELDS.contains(&k.as_str()));
    let module = &mut schema["$defs"]["module"];
    module["properties"].as_object_mut().unwrap().retain(|k, _| MODULE_FIELDS.contains(&k.as_str()));
    module.as_object_mut().unwrap().remove("required");
    module.as_object_mut().unwrap().remove("allOf");
    schema
}
