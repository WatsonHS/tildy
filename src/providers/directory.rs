//! Directory helpers (home contraction, truncation). Owner: task D.
//!
//! The truncation behaviour (count path components after the home prefix,
//! keep the repository root and everything below it, otherwise keep the last
//! N components) follows the semantics of Starship's `directory` module
//! (`truncation_length` / `truncate_to_repo`). Starship is ISC licensed,
//! Copyright (c) Starship Contributors. No Starship code is copied; the
//! behaviour is re-implemented, but NOTICE should still credit it.

use std::path::{Component, Path};

/// Replace a leading `home` with `~`. Matching is component-wise, so
/// `/home/ab` is not contracted by home `/home/a`. Non-UTF-8 is lossy.
pub fn contract_home(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if !home.as_os_str().is_empty() && home != Path::new("/") {
            if let Ok(rest) = path.strip_prefix(home) {
                let rest = rest.to_string_lossy();
                return if rest.is_empty() { "~".to_string() } else { format!("~/{rest}") };
            }
        }
    }
    path.to_string_lossy().into_owned()
}

/// Shorten a display path.
///
/// `path_str` and `repo_root` must be in the same form (both absolute, or both
/// home-contracted with [`contract_home`]). A leading `~` or `/` is not counted
/// as a segment.
/// - Inside a repo (`repo_root` is a component-wise prefix of `path_str`): keep
///   the repo root directory name and everything below it, no prefix, however
///   many segments that is.
/// - Otherwise: if there are more than `keep` segments, return `…/` plus the
///   last `keep` segments. `keep == 0` disables truncation.
pub fn truncate_segments(path_str: &str, keep: usize, repo_root: Option<&Path>) -> String {
    let (prefix, rest) = if let Some(r) = path_str.strip_prefix("~/") {
        ("~/", r)
    } else if path_str == "~" {
        return path_str.to_string();
    } else if let Some(r) = path_str.strip_prefix('/') {
        ("/", r)
    } else {
        ("", path_str)
    };
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();

    if let Some(root) = repo_root {
        let root_str = root.to_string_lossy();
        let (rprefix, rrest) = if let Some(r) = root_str.strip_prefix("~/") {
            ("~/", r.to_string())
        } else if let Some(r) = root_str.strip_prefix('/') {
            ("/", r.to_string())
        } else {
            ("", root_str.to_string())
        };
        let rsegs: Vec<&str> = rrest.split('/').filter(|s| !s.is_empty()).collect();
        if rprefix == prefix && !rsegs.is_empty() && segs.len() >= rsegs.len() && segs[..rsegs.len()] == rsegs[..] {
            return segs[rsegs.len() - 1..].join("/");
        }
    }

    if keep == 0 || segs.len() <= keep {
        return path_str.to_string();
    }
    format!("…/{}", segs[segs.len() - keep..].join("/"))
}

/// Number of normal components in a path (diagnostic helper).
pub fn depth(path: &Path) -> usize {
    path.components().filter(|c| matches!(c, Component::Normal(_))).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home() {
        let h = Path::new("/Users/me");
        assert_eq!(contract_home(Path::new("/Users/me"), Some(h)), "~");
        assert_eq!(contract_home(Path::new("/Users/me/a/b"), Some(h)), "~/a/b");
        assert_eq!(contract_home(Path::new("/Users/meme/a"), Some(h)), "/Users/meme/a");
        assert_eq!(contract_home(Path::new("/x"), None), "/x");
        assert_eq!(contract_home(Path::new("/x"), Some(Path::new("/"))), "/x");
    }

    #[test]
    fn trunc_plain() {
        assert_eq!(truncate_segments("~/a/b/c/d", 4, None), "~/a/b/c/d");
        assert_eq!(truncate_segments("~/a/b/c/d/e", 4, None), "…/b/c/d/e");
        assert_eq!(truncate_segments("/a/b/c", 2, None), "…/b/c");
        assert_eq!(truncate_segments("/", 2, None), "/");
        assert_eq!(truncate_segments("~", 2, None), "~");
        assert_eq!(truncate_segments("/a/b/c", 0, None), "/a/b/c");
    }

    #[test]
    fn trunc_repo() {
        let r = Path::new("~/w/p/proj");
        assert_eq!(truncate_segments("~/w/p/proj/src/x/y/z", 2, Some(r)), "proj/src/x/y/z");
        assert_eq!(truncate_segments("~/w/p/proj", 2, Some(r)), "proj");
        // not inside: falls back
        assert_eq!(truncate_segments("~/w/p/projx/a/b", 2, Some(r)), "…/a/b");
        let r = Path::new("/srv/a/b/repo");
        assert_eq!(truncate_segments("/srv/a/b/repo/s", 1, Some(r)), "repo/s");
    }
}
