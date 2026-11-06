//! Explicit, measured terminal-cell advances. Profiles are data, never shell
//! source. Loading a profile does no terminal I/O; calibration is foreground CLI.
pub mod probe;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const PROFILE_VERSION: u32 = 1;
pub const MAX_ENTRIES: usize = 256;
pub const MAX_CLUSTER_BYTES: usize = 128;
pub const MAX_PROFILE_BYTES: usize = 64 * 1024;
pub const MAX_COLUMNS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub term: String,
    pub program: String,
    pub program_version: String,
    pub locale: String,
    /// An explicit user label, not inferred from the terminal's font settings.
    pub font_revision: String,
}

impl Identity {
    pub fn from_process(font_revision: &str) -> Result<Self, String> {
        Self::from_values(&|key| std::env::var(key).unwrap_or_default(), font_revision)
    }

    fn from_values(get: &dyn Fn(&str) -> String, font_revision: &str) -> Result<Self, String> {
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"].iter().map(|key| get(key)).find(|s| !s.is_empty()).unwrap_or_default();
        let identity = Self { term: get("TERM"), program: get("TERM_PROGRAM"), program_version: get("TERM_PROGRAM_VERSION"), locale, font_revision: font_revision.into() };
        identity.validate()?;
        Ok(identity)
    }

    fn validate(&self) -> Result<(), String> {
        if self.term.is_empty() || self.font_revision.is_empty() { return Err("TERM and an explicit font revision are required".into()); }
        for value in [&self.term, &self.program, &self.program_version, &self.locale, &self.font_revision] {
            if value.len() > 256 || value.chars().any(unsafe_char) { return Err("invalid or oversized terminal identity".into()); }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub cluster: String,
    pub columns: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WidthProfile {
    pub version: u32,
    pub identity: Identity,
    pub entries: Vec<Entry>,
}

fn unsafe_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

pub fn validate_cluster(cluster: &str) -> Result<(), String> {
    if cluster.is_empty() || cluster.len() > MAX_CLUSTER_BYTES || cluster.chars().any(unsafe_char) || cluster.graphemes(true).count() != 1 {
        return Err(format!("each sample must be one safe grapheme cluster of at most {MAX_CLUSTER_BYTES} UTF-8 bytes"));
    }
    Ok(())
}

impl WidthProfile {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != PROFILE_VERSION { return Err("unsupported width profile version".into()); }
        self.identity.validate()?;
        if self.entries.is_empty() || self.entries.len() > MAX_ENTRIES { return Err("invalid width profile entry count".into()); }
        let mut seen = BTreeSet::new();
        for entry in &self.entries {
            validate_cluster(&entry.cluster)?;
            if entry.columns > MAX_COLUMNS || (entry.cluster.len() == 1 && entry.cluster.is_ascii() && entry.columns != 1) || !seen.insert(&entry.cluster) { return Err("invalid or duplicate width profile entry".into()); }
        }
        if serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > MAX_PROFILE_BYTES { return Err("width profile is too large".into()); }
        Ok(())
    }
}

pub fn profile_path() -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute()).map(|p| p.join(".cache")))?;
    Some(cache.join("tildy/width/profile.json"))
}

pub fn revision(profile: &WidthProfile) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    // Struct serialization has a stable field order; a revision scopes one build.
    serde_json::to_vec(profile).unwrap_or_default().hash(&mut hash);
    format!("{:016x}", hash.finish())
}

pub fn load_profile(path: &Path) -> Result<WidthProfile, String> {
    let mut options = OpenOptions::new(); options.read(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK); }
    let file = options.open(path).map_err(|e| e.to_string())?;
    // Inspect the opened descriptor, not a path that could be replaced between
    // stat and open. O_NONBLOCK also prevents a FIFO from hanging a render.
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_PROFILE_BYTES as u64 { return Err("profile must be a bounded regular file".into()); }
    let mut bytes = Vec::new();
    file.take((MAX_PROFILE_BYTES + 1) as u64).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_PROFILE_BYTES { return Err("width profile is too large".into()); }
    let profile: WidthProfile = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    profile.validate()?;
    Ok(profile)
}

pub fn save_profile(profile: &WidthProfile) -> Result<PathBuf, String> {
    let path = profile_path().ok_or("no absolute cache directory available")?;
    save_profile_at(&path, profile)?;
    Ok(path)
}

pub fn save_profile_at(path: &Path, profile: &WidthProfile) -> Result<(), String> {
    profile.validate()?;
    let bytes = serde_json::to_vec_pretty(profile).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_PROFILE_BYTES { return Err("width profile is too large".into()); }
    let parent = path.parent().ok_or("profile has no parent directory")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temporary = parent.join(format!(".profile-{}-{nonce}-{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut options = OpenOptions::new(); options.write(true).create_new(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
        file.write_all(&bytes).and_then(|_| file.sync_all()).map_err(|e| e.to_string())?;
        fs::rename(&temporary, path).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temporary); }
    result
}

#[derive(Debug, Clone, Default)]
pub struct WidthPolicy {
    profile: Option<Arc<WidthProfile>>,
}

impl WidthPolicy {
    pub fn from_profile(profile: WidthProfile) -> Result<Self, String> {
        profile.validate()?;
        Ok(Self { profile: Some(Arc::new(profile)) })
    }
    pub fn revision(&self) -> Option<String> { self.profile.as_deref().map(revision) }
    pub fn measured_columns(&self, cluster: &str) -> Option<usize> {
        self.profile.as_ref()?.entries.iter().find(|entry| entry.cluster == cluster).map(|entry| entry.columns)
    }
    pub fn width(&self, text: &str) -> usize {
        if self.profile.is_none() { return UnicodeWidthStr::width(text); }
        let mut total = 0;
        let mut unmeasured_start = 0;
        for (offset, cluster) in text.grapheme_indices(true) {
            if let Some(columns) = self.measured_columns(cluster) {
                total += UnicodeWidthStr::width(&text[unmeasured_start..offset]) + columns;
                unmeasured_start = offset + cluster.len();
            }
        }
        total + UnicodeWidthStr::width(&text[unmeasured_start..])
    }
    fn escape(&self, text: &str) -> String {
        if self.profile.is_none() { return text.replace('%', "%%"); }
        let mut output = String::new();
        for cluster in text.graphemes(true) {
            let escaped = cluster.replace('%', "%%");
            match self.measured_columns(cluster) {
                Some(0) => output.push_str(&format!("%{{{escaped}%}}")),
                Some(columns) => output.push_str(&format!("%{{{escaped}%{columns}G%}}")),
                None => output.push_str(&escaped),
            }
        }
        output
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp { path: PathBuf, modified: Option<SystemTime>, len: u64, file_key: (u64, u64) }
thread_local! {
    static CURRENT: RefCell<WidthPolicy> = RefCell::new(WidthPolicy::default());
    static CACHED: RefCell<Option<(Stamp, Option<Arc<WidthProfile>>)>> = const { RefCell::new(None) };
}

/// The guard restores the previous policy even through nested renders or panic.
pub fn with_policy<T>(policy: WidthPolicy, render: impl FnOnce() -> T) -> T {
    struct Restore(Option<WidthPolicy>);
    impl Drop for Restore { fn drop(&mut self) { CURRENT.with(|current| { *current.borrow_mut() = self.0.take().unwrap_or_default(); }); } }
    let previous = CURRENT.with(|current| std::mem::replace(&mut *current.borrow_mut(), policy));
    let _restore = Restore(Some(previous));
    render()
}

pub fn is_active() -> bool { CURRENT.with(|current| current.borrow().profile.is_some()) }

pub fn width(text: &str) -> usize { CURRENT.with(|current| current.borrow().width(text)) }
pub fn escape(text: &str) -> String { CURRENT.with(|current| current.borrow().escape(text)) }

/// A disabled identity returns before any profile stat/open. Only a render
/// entry calls this; module-level width operations never touch the filesystem.
pub fn policy_for_request_env(env: &str) -> WidthPolicy {
    let forwarded = env.split('\u{1f}').find_map(|pair| pair.strip_prefix("TILDY_FONT_REVISION="));
    let identity = if let Some(revision) = forwarded {
        if revision.is_empty() { return WidthPolicy::default(); }
        let pairs: BTreeMap<_, _> = env.split('\u{1f}').filter_map(|pair| pair.split_once('=')).collect();
        Identity::from_values(&|key| pairs.get(key).copied().unwrap_or_default().into(), revision)
    } else {
        let revision = std::env::var("TILDY_FONT_REVISION").unwrap_or_default();
        if revision.is_empty() { return WidthPolicy::default(); }
        Identity::from_process(&revision)
    };
    let Ok(identity) = identity else { return WidthPolicy::default(); };
    let Some(path) = profile_path() else { return WidthPolicy::default(); };
    let Ok(metadata) = fs::symlink_metadata(&path) else { return WidthPolicy::default(); };
    #[cfg(unix)] let file_key = { use std::os::unix::fs::MetadataExt; (metadata.dev(), metadata.ino()) };
    #[cfg(not(unix))] let file_key = (0, 0);
    let stamp = Stamp { path, modified: metadata.modified().ok(), len: metadata.len(), file_key };
    let profile = CACHED.with(|cached| {
        let mut cached = cached.borrow_mut();
        if cached.as_ref().is_none_or(|(previous, _)| previous != &stamp) {
            let profile = load_profile(&stamp.path).ok().map(Arc::new);
            *cached = Some((stamp, profile));
        }
        cached.as_ref().and_then(|(_, profile)| profile.clone())
    });
    WidthPolicy { profile: profile.filter(|profile| profile.identity == identity) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> WidthProfile {
        WidthProfile { version: 1, identity: Identity { term:"xterm".into(),program:"test".into(),program_version:"1".into(),locale:"en_US.UTF-8".into(),font_revision:"test-font".into() }, entries:vec![Entry { cluster:"👩‍💻".into(),columns:3 },Entry { cluster:"%".into(),columns:1 }] }
    }
    #[test]
    fn exact_clusters_validate_and_scoped_policy_restores_after_panic() {
        for invalid in ["", "ab", "\u{1b}", "x\n", "\u{202e}"] { assert!(validate_cluster(invalid).is_err()); }
        for valid in ["中", "👩‍💻", "e\u{301}", "%", "☀️"] { validate_cluster(valid).unwrap(); }
        let p = WidthPolicy::from_profile(profile()).unwrap();
        assert_eq!(p.width("x👩‍💻%"),5);
        with_policy(p, || {
            assert_eq!(width("👩‍💻"),3);
            assert_eq!(escape("%👩‍💻"),"%{%%%1G%}%{👩‍💻%3G%}");
            let _ = std::panic::catch_unwind(|| with_policy(WidthPolicy::default(), || panic!("test restoration")));
            assert_eq!(width("👩‍💻"),3);
        });
        assert_eq!(width("👩‍💻"),2);
    }
    #[test]
    fn atomic_bounded_profiles_preserve_previous_file_on_invalid_write() {
        let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("profile.json"); let p=profile();
        save_profile_at(&path,&p).unwrap(); assert_eq!(load_profile(&path).unwrap(),p);
        let previous=fs::read(&path).unwrap(); let mut bad=p.clone(); bad.entries.push(bad.entries[0].clone());
        assert!(save_profile_at(&path,&bad).is_err()); assert_eq!(fs::read(&path).unwrap(),previous);
        bad=p.clone(); bad.entries[0].columns=9; assert!(bad.validate().is_err());
        bad=p; bad.entries[1].columns=2; assert!(bad.validate().is_err());
        fs::write(&path,vec![b'x';MAX_PROFILE_BYTES+1]).unwrap(); assert!(load_profile(&path).is_err());
    }
}
