//! Module-report annotations shared by previews and worker snapshots. Workers
//! prepare source metadata on config reload; dynamic fields follow each render.
use super::{render_with, LocalTime, RenderOutput, RAW_SGR};
use crate::config::{self, Config, ModuleType, Style};
use crate::model::{ModuleReport, PromptData};
use std::collections::BTreeSet;

/// Only module ownership is retained, never a second resolved configuration.
/// Rebuild alongside the effective config, including invalid-config fallback.
#[derive(Debug, Default)]
pub struct AnnotationMetadata {
    preset_name: Option<String>,
    preset_modules: BTreeSet<String>,
}

impl AnnotationMetadata {
    pub fn new(config: &Config, preset: Option<&str>) -> Self {
        let preset_name = preset
            .map(String::from)
            .or_else(|| config.file.preset.clone())
            .filter(|n| n != "plain");
        // Resolve with the normal loader so modules inherited from builtin
        // retain precisely the same ownership as the compatibility wrapper.
        let preset_modules = preset_name.as_deref().and_then(|n| config::preset(n).ok())
            .map(|p| p.file.modules.into_keys().collect()).unwrap_or_default();
        Self { preset_name, preset_modules }
    }
}

/// `tildy preview` render: deterministic (`os` module renders as on macOS) and
/// every report carries `source`, `age_ms`, `style`, `row` and `side`.
/// `raw_sgr` emits dim/italic as raw SGR (terminal output only, never for the
/// shell). `preset` is the preset the config was built from, when known; it
/// defaults to the effective configuration's preset.
pub fn render_annotated(config: &Config, preset: Option<&str>, data: &PromptData, now: Option<LocalTime>, raw_sgr: bool) -> RenderOutput {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            RAW_SGR.with(|r| r.set(false));
        }
    }
    RAW_SGR.with(|r| r.set(raw_sgr));
    let _reset = Reset;
    let mut out = render_with(config, data, now, "macos");
    annotate(config, preset, data, &mut out.reports);
    out
}

/// Style words of a final style, `fg=red bold`; empty when unstyled.
pub fn style_text(s: &Style) -> String {
    let mut p = vec![];
    if let Some(c) = &s.foreground {
        p.push(format!("fg={c}"));
    }
    if let Some(c) = &s.background {
        p.push(format!("bg={c}"));
    }
    for (on, name) in [(s.bold, "bold"), (s.dim, "dim"), (s.italic, "italic"), (s.underline, "underline")] {
        if on == Some(true) {
            p.push(name.into());
        }
    }
    p.join(" ")
}

pub fn annotate(config: &Config, preset: Option<&str>, data: &PromptData, reports: &mut [ModuleReport]) {
    annotate_prepared(config, &AnnotationMetadata::new(config, preset), data, reports);
}

/// Annotate one actual render using ownership prepared for this configuration.
pub fn annotate_prepared(config: &Config, metadata: &AnnotationMetadata, data: &PromptData, reports: &mut [ModuleReport]) {
    let project_mods = config.project.as_ref().and_then(|p| p.applied.as_ref()).and_then(|p| p.get("modules")).and_then(|m| m.as_object());
    let user_mods = config.user.as_ref().and_then(|u| u.get("modules")).and_then(|m| m.as_object());
    // Reports follow layout order: per row the left ids, then the right ids,
    // skipping ids without a module definition (the renderer does the same).
    let mut places = vec![];
    for (ri, row) in config.file.layout.rows.iter().enumerate() {
        for (side, ids) in [("left", &row.left), ("right", &row.right)] {
            for id in ids {
                if config.file.modules.contains_key(id) {
                    places.push((ri, side));
                }
            }
        }
    }
    let aligned = places.len() == reports.len();
    for (i, r) in reports.iter_mut().enumerate() {
        let Some(m) = config.file.modules.get(&r.id) else { continue };
        if aligned {
            r.row = Some(places[i].0);
            r.side = Some(places[i].1.into());
        }
        r.source = Some(if project_mods.is_some_and(|p| p.contains_key(&r.id)) {
            "project".into()
        } else if user_mods.is_some_and(|u| u.contains_key(&r.id)) {
            "user".into()
        } else {
            match &metadata.preset_name {
                Some(n) if metadata.preset_modules.contains(&r.id) => format!("preset:{n}"),
                _ => "builtin".into(),
            }
        });
        r.age_ms = if m.module_type == ModuleType::Git {
            data.git.age_ms
        } else {
            data.modules.get(&r.id).and_then(|s| s.age_ms)
        };
        if r.visible {
            let mut st = m.style.clone();
            if m.module_type == ModuleType::Character {
                let s = if data.request.last_exit == 0 { &m.success_style } else { &m.error_style };
                if let Some(s) = s {
                    st.overlay(s);
                }
            }
            for rule in m.rules.iter().filter(|x| r.rules_hit.contains(&x.id)) {
                st.overlay(&rule.style);
            }
            r.style = Some(style_text(&st));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> PromptData {
        let input: serde_json::Value = serde_json::from_str(include_str!("../../fixtures/render/git_clean.input.json")).unwrap();
        serde_json::from_value(input["data"].clone()).unwrap()
    }

    #[test]
    fn prepared_annotations_match_wrapper_for_every_preset_and_repeated_layout() {
        let data = data();
        for name in config::PRESETS {
            let config = config::from_str(&serde_json::json!({
                "schema_version":1, "preset":name,
                "modules":{"char":{"type":"character","style":{"foreground":"red"}}},
                "layout":{"rows":[{"left":["cwd","char"],"right":["cwd"]},{"left":["cwd","char"]}]}
            }).to_string()).unwrap();
            let metadata = AnnotationMetadata::new(&config, None);
            let mut prepared = super::super::render(&config, &data).reports;
            let mut wrapper = prepared.clone();
            annotate(&config, None, &data, &mut wrapper);
            annotate_prepared(&config, &metadata, &data, &mut prepared);
            assert_eq!(serde_json::to_value(&prepared).unwrap(), serde_json::to_value(&wrapper).unwrap(), "{name}");
            assert_eq!(prepared.iter().filter(|r| r.id == "cwd").map(|r| (r.row.unwrap(),r.side.as_deref().unwrap())).collect::<Vec<_>>(), vec![(0,"left"),(0,"right"),(1,"left")]);
            assert!(prepared.iter().filter(|r| r.id == "char").all(|r| r.source.as_deref() == Some("user")));
            let reference = config::preset(name).unwrap();
            for r in prepared.iter().filter(|r| r.id != "char") {
                let expected = if *name != "plain" && reference.file.modules.contains_key(&r.id) { format!("preset:{name}") } else { "builtin".into() };
                assert_eq!(r.source.as_deref(), Some(expected.as_str()), "{name}/{}", r.id);
            }
        }
    }

    #[test]
    fn explicit_preset_and_unknown_preset_keep_compatibility() {
        let config = config::builtin();
        let data = data();
        for name in ["lean", "plain", "unknown"] {
            let metadata = AnnotationMetadata::new(&config, Some(name));
            let mut prepared = super::super::render(&config, &data).reports;
            let mut wrapper = prepared.clone();
            annotate(&config, Some(name), &data, &mut wrapper);
            annotate_prepared(&config, &metadata, &data, &mut prepared);
            assert_eq!(serde_json::to_value(prepared).unwrap(), serde_json::to_value(wrapper).unwrap());
        }
    }
}
