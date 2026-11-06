//! `package` module: the version of the package whose manifest is in the cwd.
//! Owner: M2 task E1.
//!
//! Manifests are read in Starship's order (cargo, npm, jsr, pyproject,
//! setup.cfg, composer, gradle, julia, mix, helm, maven, meson, shards, v,
//! vpkg, sbt, daml, dart, R, galaxy); the first one that yields a version
//! wins. Nimble needs `nimble dump` and is not supported. Nothing runs: only
//! files of at most 1 MiB in the cwd (and, for a Cargo workspace member, the
//! workspace root) are read. The rules follow Starship's `package` module
//! (ISC license, Copyright (c) Starship Contributors); no code is copied.
//! YAML and XML are scanned for the one key needed instead of being parsed.

use regex::Regex;
use serde_json::Value as Json;
use std::path::Path;

const MAX_MANIFEST: u64 = 1 << 20;

fn read(dir: &Path, name: &str) -> Option<String> {
    let p = dir.join(name);
    let meta = std::fs::metadata(&p).ok()?;
    if !meta.is_file() || meta.len() > MAX_MANIFEST {
        return None;
    }
    std::fs::read_to_string(p).ok()
}

fn json_version(src: &str) -> Option<String> {
    let v: Json = serde_json::from_str(src).ok()?;
    v.get("version")?.as_str().map(str::to_string)
}

fn toml_table(src: &str) -> Option<toml::Table> {
    src.parse::<toml::Table>().ok()
}

fn toml_path<'a>(t: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (first, rest) = path.split_first()?;
    let mut v = t.get(*first)?;
    for k in rest {
        v = v.get(*k)?;
    }
    Some(v)
}

/// A top-level `version:` key of a YAML document (no nesting, no anchors).
fn yaml_version(src: &str) -> Option<String> {
    for line in src.lines() {
        let Some(rest) = line.strip_prefix("version:") else { continue };
        let mut v = rest.trim();
        if let Some(i) = v.find(" #") {
            v = v[..i].trim_end();
        }
        let v = v.trim_matches(|c| c == '"' || c == '\'');
        return (!v.is_empty()).then(|| v.to_string());
    }
    None
}

fn regex_version(src: &str, re: &str) -> Option<String> {
    let re = Regex::new(re).expect("package regex compiles");
    re.captures(src).and_then(|c| c.name("version")).map(|m| m.as_str().trim().to_string())
}

fn cargo(dir: &Path) -> Option<String> {
    let t = toml_table(&read(dir, "Cargo.toml")?)?;
    let v = toml_path(&t, &["package", "version"]);
    if let Some(s) = v.and_then(toml::Value::as_str) {
        return Some(s.to_string());
    }
    let ws_ver = |t: &toml::Table| toml_path(t, &["workspace", "package", "version"]).and_then(toml::Value::as_str).map(str::to_string);
    if v.and_then(|v| v.get("workspace")).and_then(toml::Value::as_bool) == Some(true) {
        if t.contains_key("workspace") {
            return ws_ver(&t);
        }
        // The workspace root is the nearest ancestor with a Cargo.toml.
        for anc in dir.ancestors().skip(1) {
            if anc.join("Cargo.toml").is_file() {
                return ws_ver(&toml_table(&read(anc, "Cargo.toml")?)?);
            }
        }
        return None;
    }
    ws_ver(&t)
}

fn npm(dir: &Path, display_private: bool) -> Option<String> {
    let v: Json = serde_json::from_str(&read(dir, "package.json")?).ok()?;
    if !display_private && v.get("private").and_then(Json::as_bool) == Some(true) {
        return None;
    }
    let raw = v.get("version")?.as_str()?;
    if raw == "null" {
        return None;
    }
    let cleaned = clean(raw);
    if cleaned == "0.0.0-development" || cleaned.starts_with("0.0.0-semantic") {
        return Some(SEMANTIC.to_string());
    }
    Some(raw.to_string())
}

fn jsr(dir: &Path) -> Option<String> {
    ["deno.json", "jsr.json"].iter().find_map(|n| read(dir, n)).and_then(|s| json_version(&s))
}

fn pyproject(dir: &Path) -> Option<String> {
    let t = toml_table(&read(dir, "pyproject.toml")?)?;
    toml_path(&t, &["project", "version"])
        .or_else(|| toml_path(&t, &["tool", "poetry", "version"]))
        .and_then(toml::Value::as_str)
        .map(str::to_string)
}

fn setup_cfg(dir: &Path) -> Option<String> {
    let src = read(dir, "setup.cfg")?;
    let mut in_meta = false;
    for line in src.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_meta = l == "[metadata]";
            continue;
        }
        if !in_meta {
            continue;
        }
        if let Some((k, v)) = l.split_once(['=', ':']) {
            if k.trim() == "version" {
                let v = v.trim();
                return (!v.is_empty() && !v.starts_with("attr:") && !v.starts_with("file:")).then(|| v.to_string());
            }
        }
    }
    None
}

fn gradle(dir: &Path) -> Option<String> {
    read(dir, "gradle.properties")
        .and_then(|s| regex_version(&s, r"(?m)^\s*version\s*=\s*(?P<version>.*)"))
        .or_else(|| read(dir, "build.gradle").and_then(|s| regex_version(&s, r#"(?m)^version( |\s*=\s*)['"](?P<version>[^'"]+)['"]$"#)))
}

/// The `<version>` element directly under the root element of a pom.xml.
fn maven(dir: &Path) -> Option<String> {
    let src = read(dir, "pom.xml")?;
    let tag = Regex::new(r"<(/?)([A-Za-z_][\w.:-]*)[^>]*?(/?)>").expect("tag regex");
    // Drop comments, CDATA and processing instructions first.
    let strip = Regex::new(r"(?s)<!--.*?-->|<!\[CDATA\[.*?\]\]>|<\?.*?\?>|<!DOCTYPE[^>]*>").expect("strip regex");
    let src = strip.replace_all(&src, "");
    let mut depth = 0usize;
    for c in tag.captures_iter(&src) {
        let whole = c.get(0)?;
        let (close, name, empty) = (&c[1] == "/", &c[2], &c[3] == "/");
        if close {
            depth = depth.saturating_sub(1);
        } else if empty {
        } else {
            if depth == 1 && name == "version" {
                let rest = &src[whole.end()..];
                let text = rest[..rest.find('<').unwrap_or(rest.len())].trim();
                return (!text.is_empty() && !text.starts_with('$')).then(|| text.to_string());
            }
            depth += 1;
        }
    }
    None
}

fn meson(dir: &Path) -> Option<String> {
    let src: String = read(dir, "meson.build")?.split_ascii_whitespace().collect();
    regex_version(&src, r"project\([^())]*,version:'(?P<version>[^']+)'[^())]*\)")
}

/// Text placed in `fields.version` for npm's semantic-release placeholder;
/// the renderer shows it as is (no `version_format`).
pub const SEMANTIC: &str = "semantic";

/// Starship's cleanup before `version_format`: quotes dropped, trimmed,
/// leading `v`s removed.
fn clean(raw: &str) -> String {
    raw.replace('"', "").trim().trim_start_matches('v').to_string()
}

/// The package version of `dir` (cleaned, without `version_format`), or the
/// [`SEMANTIC`] marker.
pub fn package_version(dir: &Path, display_private: bool) -> Option<String> {
    let finders: [&dyn Fn() -> Option<String>; 20] = [
        &|| cargo(dir),
        &|| npm(dir, display_private),
        &|| jsr(dir),
        &|| pyproject(dir),
        &|| setup_cfg(dir),
        &|| read(dir, "composer.json").and_then(|s| json_version(&s)),
        &|| gradle(dir),
        &|| toml_table(&read(dir, "Project.toml")?).and_then(|t| t.get("version")?.as_str().map(str::to_string)),
        &|| read(dir, "mix.exs").and_then(|s| regex_version(&s, r#"(?m)version: "(?P<version>[^"]+)""#)),
        &|| read(dir, "Chart.yaml").and_then(|s| yaml_version(&s)),
        &|| maven(dir),
        &|| meson(dir),
        &|| read(dir, "shard.yml").and_then(|s| yaml_version(&s)),
        &|| read(dir, "v.mod").and_then(|s| regex_version(&s, r"(?m)^\s*version\s*:\s*'(?P<version>[^']+)'")),
        &|| read(dir, "vpkg.json").and_then(|s| json_version(&s)),
        &|| read(dir, "build.sbt").and_then(|s| regex_version(&s, r"(?m)^(.*/)*\s*version\s*:=\s*.(?P<version>[\d\.]+)")),
        &|| read(dir, "daml.yaml").and_then(|s| yaml_version(&s)),
        &|| read(dir, "pubspec.yaml").and_then(|s| yaml_version(&s)),
        &|| read(dir, "DESCRIPTION").and_then(|s| regex_version(&s, r"(?m)^Version:\s*(?P<version>.*$)")),
        &|| read(dir, "galaxy.yml").and_then(|s| yaml_version(&s)),
    ];
    finders.iter().find_map(|f| f()).map(|v| if v == SEMANTIC { v } else { clean(&v) }).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        for (n, c) in files {
            std::fs::write(td.path().join(n), c).unwrap();
        }
        td
    }

    #[test]
    fn manifests() {
        let v = |files: &[(&str, &str)]| package_version(dir_with(files).path(), false);
        assert_eq!(v(&[("package.json", r#"{"version":"1.2.3"}"#)]).as_deref(), Some("1.2.3"));
        assert_eq!(v(&[("package.json", r#"{"version":"1.2.3","private":true}"#)]), None);
        assert_eq!(package_version(dir_with(&[("package.json", r#"{"version":"1.2.3","private":true}"#)]).path(), true).as_deref(), Some("1.2.3"));
        assert_eq!(v(&[("package.json", r#"{"version":"0.0.0-development"}"#)]).as_deref(), Some(SEMANTIC));
        assert_eq!(v(&[("package.json", r#"{"name":"x"}"#)]), None);
        assert_eq!(v(&[("Cargo.toml", "[package]\nname='a'\nversion = \"0.1.0-m1\"\n")]).as_deref(), Some("0.1.0-m1"));
        assert_eq!(v(&[("Cargo.toml", "[workspace.package]\nversion = \"2.0.0\"\n")]).as_deref(), Some("2.0.0"));
        assert_eq!(v(&[("pyproject.toml", "[project]\nversion = \"3.1\"\n")]).as_deref(), Some("3.1"));
        assert_eq!(v(&[("pyproject.toml", "[tool.poetry]\nversion = \"v0.9.0\"\n")]).as_deref(), Some("0.9.0"));
        assert_eq!(v(&[("pyproject.toml", "[project]\ndynamic = [\"version\"]\n")]), None);
        assert_eq!(v(&[("setup.cfg", "[metadata]\nname = x\nversion = 1.4.0\n")]).as_deref(), Some("1.4.0"));
        assert_eq!(v(&[("setup.cfg", "[metadata]\nversion = attr: x.__version__\n")]), None);
        assert_eq!(v(&[("composer.json", r#"{"version":"5.0.1"}"#)]).as_deref(), Some("5.0.1"));
        assert_eq!(v(&[("Chart.yaml", "apiVersion: v2\nversion: 0.3.2 # chart\nappVersion: \"1.0\"\n")]).as_deref(), Some("0.3.2"));
        assert_eq!(v(&[("pubspec.yaml", "name: a\nversion: '1.0.0+1'\n")]).as_deref(), Some("1.0.0+1"));
        assert_eq!(v(&[("mix.exs", "def project do\n [app: :a, version: \"0.2.0\"]\n")]).as_deref(), Some("0.2.0"));
        assert_eq!(v(&[("build.gradle", "plugins {}\nversion = '1.1'\n")]).as_deref(), Some("1.1"));
        assert_eq!(v(&[("Project.toml", "name = \"A\"\nversion = \"0.5.0\"\n")]).as_deref(), Some("0.5.0"));
        assert_eq!(
            v(&[("pom.xml", "<?xml version=\"1.0\"?>\n<project><!-- <version>9</version> --><parent><version>1</version></parent><version>2.5.0</version></project>")]).as_deref(),
            Some("2.5.0")
        );
        assert_eq!(v(&[("meson.build", "project('a', 'c',\n  version : '0.7.1')\n")]).as_deref(), Some("0.7.1"));
        // go.mod has no version.
        assert_eq!(v(&[("go.mod", "module x\n\ngo 1.22\n")]), None);
        // Order: Cargo.toml before package.json.
        assert_eq!(v(&[("Cargo.toml", "[package]\nversion = \"1.0.0\"\n"), ("package.json", r#"{"version":"2.0.0"}"#)]).as_deref(), Some("1.0.0"));
        assert_eq!(v(&[]), None);
    }

    #[test]
    fn cargo_workspace_member() {
        let td = dir_with(&[("Cargo.toml", "[workspace]\nmembers=['m']\n[workspace.package]\nversion = \"4.2.0\"\n")]);
        let m = td.path().join("m");
        std::fs::create_dir(&m).unwrap();
        std::fs::write(m.join("Cargo.toml"), "[package]\nname='m'\nversion.workspace = true\n").unwrap();
        assert_eq!(package_version(&m, false).as_deref(), Some("4.2.0"));
    }
}
