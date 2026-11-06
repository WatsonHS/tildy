//! Git status provider (porcelain v2 via the git CLI). Owner: task D (M1),
//! task B (M1b: stash/renamed/deleted).
//!
//! Strategy: two git processes run concurrently under one shared deadline:
//! 1. `git -C cwd rev-parse --path-format=absolute --git-common-dir --show-toplevel --git-dir`
//!    for worktree identity (and the "not a repository" decision);
//! 2. `git --no-optional-locks -C cwd status --porcelain=v2 -z --branch --untracked-files=normal`.
//!
//! The stash count comes from the stash reflog file in the common git dir (see
//! [`stash_count`]); renamed/deleted counts come from the porcelain records; an
//! in-progress operation (merge, rebase, ...) comes from marker files in the
//! worktree's own git dir (see [`detect_operation`]). Neither starts a process.
//!
//! Untracked policy (M1): `--untracked-files=normal`, no cap, so `incomplete`
//! is always false; untracked directories count as one entry each (git's
//! "normal" behaviour). Submodules use git's default (`ignore=none` unless
//! configured), also not capped. On timeout the slot is `Timeout` with no
//! value, so an incomplete scan is never reported as clean.
//!
//! Known limitation: rev-parse paths are newline-delimited (no -z), so a
//! worktree path containing a newline yields a wrong identity (no panic).

use super::exec;
use crate::model::{Freshness, GitData, GitState, Slot};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

enum GitErr {
    Timeout,
    Other(String),
}

struct Out {
    ok: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run git with a deadline (process-group kill on timeout, see `exec::run`).
fn run_git(args: &[&str], cwd: &Path, deadline: Instant) -> Result<Out, GitErr> {
    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0");
    match exec::run(cmd, deadline, usize::MAX) {
        Ok(o) => Ok(Out { ok: o.success, stdout: o.stdout, stderr: o.stderr }),
        Err(exec::RunErr::Timeout) => Err(GitErr::Timeout),
        Err(exec::RunErr::NotFound) => Err(GitErr::Other("git not found in PATH".into())),
        Err(exec::RunErr::Spawn(m)) => Err(GitErr::Other(format!("cannot run git: {m}"))),
    }
}

fn slot_err(freshness: Freshness, msg: String) -> Slot<GitData> {
    Slot { freshness, age_ms: None, value: None, error: Some(msg) }
}

fn first_line(b: &[u8]) -> String {
    String::from_utf8_lossy(b).lines().next().unwrap_or("").trim().to_string()
}

/// Synchronous git status with a total timeout (covers every git invocation).
pub fn git_status(cwd: &Path, timeout: Duration) -> Slot<GitData> {
    git_status_with(cwd, timeout, false)
}

/// [`git_status`]; `ignore_submodules` adds `--ignore-submodules=dirty`
/// (Starship's `git_status.ignore_submodules`: changes inside submodules do
/// not count, a moved submodule commit still does).
pub fn git_status_with(cwd: &Path, timeout: Duration, ignore_submodules: bool) -> Slot<GitData> {
    let deadline = Instant::now() + timeout;
    let mut status_args = vec!["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=normal"];
    if ignore_submodules {
        status_args.push("--ignore-submodules=dirty");
    }
    let (ident, status) = std::thread::scope(|s| {
        let h = s.spawn(|| run_git(IDENT_ARGS, cwd, deadline));
        let st = run_git(&status_args, cwd, deadline);
        (h.join().unwrap_or(Err(GitErr::Other("internal thread panic".into()))), st)
    });

    let ident = match ident {
        Ok(o) => o,
        Err(GitErr::Timeout) => return slot_err(Freshness::Timeout, "git timed out".into()),
        Err(GitErr::Other(m)) => return slot_err(Freshness::Error, m),
    };
    if !ident.ok {
        let err = String::from_utf8_lossy(&ident.stderr).to_string();
        // Bare repos and "inside .git" fail --show-toplevel: no worktree, so not a prompt repo.
        if err.contains("not a git repository")
            || err.contains("this operation must be run in a work tree")
        {
            return Slot::fresh(GitData { in_repo: false, state: GitState::Unknown, ..Default::default() });
        }
        return slot_err(Freshness::Error, format!("git rev-parse failed: {}", first_line(&ident.stderr)));
    }
    let status = match status {
        Ok(o) => o,
        Err(GitErr::Timeout) => return slot_err(Freshness::Timeout, "git status timed out".into()),
        Err(GitErr::Other(m)) => return slot_err(Freshness::Error, m),
    };
    if !status.ok {
        return slot_err(Freshness::Error, format!("git status failed: {}", first_line(&status.stderr)));
    }

    let Some((common, top, gdir)) = parse_ident(&ident.stdout) else {
        return slot_err(Freshness::Error, "unexpected rev-parse output".into());
    };

    let mut data = parse_status(&status.stdout);
    data.stashed = match stash_count(&common, cwd, deadline) {
        Ok(n) => n,
        Err(GitErr::Timeout) => return slot_err(Freshness::Timeout, "git stash count timed out".into()),
        Err(GitErr::Other(m)) => return slot_err(Freshness::Error, m),
    };
    (data.operation, data.operation_progress) = detect_operation(&gdir);
    data.in_repo = true;
    data.worktree_id = Some(worktree_id_of(&common, &top));
    data.git_dir = Some(gdir);
    data.worktree_root = Some(top);
    Slot::fresh(data)
}

/// `rev-parse` arguments for worktree identity: common dir, toplevel, git dir.
const IDENT_ARGS: &[&str] = &["rev-parse", "--path-format=absolute", "--git-common-dir", "--show-toplevel", "--git-dir"];

/// Canonical (common dir, toplevel, git dir) from the [`IDENT_ARGS`] output.
fn parse_ident(stdout: &[u8]) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let mut lines = stdout.split(|&b| b == b'\n').filter(|l| !l.is_empty());
    let (common, top, gdir) = (lines.next()?, lines.next()?, lines.next()?);
    let canon = |b: &[u8]| -> PathBuf {
        let p = PathBuf::from(OsStr::from_bytes(b));
        std::fs::canonicalize(&p).unwrap_or(p)
    };
    Some((canon(common), canon(top), canon(gdir)))
}

/// `GitData.worktree_id`: `<canonical common dir>@<canonical toplevel>`.
fn worktree_id_of(common: &Path, top: &Path) -> String {
    format!("{}@{}", common.to_string_lossy(), top.to_string_lossy())
}

/// Deadline of [`worktree_identity`].
pub const IDENTITY_TIMEOUT: Duration = Duration::from_millis(1000);

/// The worktree identity of `cwd`, identical to the `GitData.worktree_id`
/// that [`git_status`] reports for it (same `rev-parse`, same
/// canonicalization), without the status scan. `None` outside a worktree
/// (including bare repositories and the inside of `.git`), when git is
/// missing, or after [`IDENTITY_TIMEOUT`]. One `git rev-parse` process, so
/// callers on a hot path should cache it per cwd.
pub fn worktree_identity(cwd: &Path) -> Option<String> {
    let out = run_git(IDENT_ARGS, cwd, Instant::now() + IDENTITY_TIMEOUT).ok()?;
    if !out.ok {
        return None;
    }
    let (common, top, _) = parse_ident(&out.stdout)?;
    Some(worktree_id_of(&common, &top))
}

/// In-progress operation recorded in a worktree's git dir (`--git-dir`, which
/// is per worktree for linked worktrees). Precedence when several markers
/// exist follows Starship: rebase > merge > cherry-pick > revert > bisect.
/// `rebase-apply/` is `am` when `rebase-apply/applying` exists. Progress is
/// (current, total) from `rebase-merge/{msgnum,end}` or `rebase-apply/{next,last}`.
pub fn detect_operation(git_dir: &Path) -> (Option<String>, Option<(u32, u32)>) {
    let rm = git_dir.join("rebase-merge");
    if rm.is_dir() {
        return (Some("rebase".into()), progress(&rm.join("msgnum"), &rm.join("end")));
    }
    let ra = git_dir.join("rebase-apply");
    if ra.is_dir() {
        let op = if ra.join("applying").exists() { "am" } else { "rebase" };
        return (Some(op.into()), progress(&ra.join("next"), &ra.join("last")));
    }
    for (marker, op) in [("MERGE_HEAD", "merge"), ("CHERRY_PICK_HEAD", "cherry-pick"), ("REVERT_HEAD", "revert"), ("BISECT_LOG", "bisect")] {
        if git_dir.join(marker).exists() {
            return (Some(op.into()), None);
        }
    }
    (None, None)
}

fn progress(current: &Path, total: &Path) -> Option<(u32, u32)> {
    let read = |p: &Path| std::fs::read_to_string(p).ok()?.trim().parse::<u32>().ok();
    Some((read(current)?, read(total)?))
}

/// Number of stash entries, i.e. `git rev-list --walk-reflogs --count refs/stash`
/// (0 when `refs/stash` does not exist).
///
/// With the default `files` ref backend the stash reflog is the shared file
/// `<common-dir>/logs/refs/stash`, one line per entry (`git stash drop` rewrites
/// it), so counting its lines gives the same number without a third process.
/// Only a `reftable` repository (no reflog files) runs the `rev-list` command,
/// under the same deadline as the status call.
fn stash_count(common_dir: &Path, cwd: &Path, deadline: Instant) -> Result<u32, GitErr> {
    match std::fs::read(common_dir.join("logs/refs/stash")) {
        Ok(bytes) => return Ok(bytes.split(|&b| b == b'\n').filter(|l| !l.is_empty()).count() as u32),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(GitErr::Other(format!("cannot read stash reflog: {e}"))),
    }
    if !common_dir.join("reftable").is_dir() {
        return Ok(0);
    }
    let out = run_git(&["rev-list", "--walk-reflogs", "--count", "refs/stash", "--"], cwd, deadline)?;
    if !out.ok {
        // "ambiguous argument 'refs/stash'": no stash.
        return Ok(0);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0))
}

/// Parse `status --porcelain=v2 -z --branch` output. Never panics; unknown
/// records are skipped. Identity fields are left for the caller; `in_repo` is
/// not set here.
pub fn parse_status(raw: &[u8]) -> GitData {
    let mut d = GitData::default();
    let mut oid: Option<String> = None;
    let mut head: Option<String> = None;
    let mut fields = raw.split(|&b| b == 0);
    while let Some(f) = fields.next() {
        if f.is_empty() {
            continue;
        }
        let s = String::from_utf8_lossy(f);
        if let Some(h) = s.strip_prefix("# ") {
            if let Some(v) = h.strip_prefix("branch.oid ") {
                oid = Some(v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.head ") {
                head = Some(v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.upstream ") {
                d.upstream = Some(v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.ab ") {
                for tok in v.split_whitespace() {
                    if let Some(n) = tok.strip_prefix('+') {
                        d.ahead = n.parse().unwrap_or(0);
                    } else if let Some(n) = tok.strip_prefix('-') {
                        d.behind = n.parse().unwrap_or(0);
                    }
                }
            }
            continue;
        }
        let mut chars = s.chars();
        match chars.next() {
            Some('1') => count_xy(&s, &mut d),
            Some('2') => {
                count_xy(&s, &mut d);
                d.renamed += 1;
                // With -z the original path is a separate NUL-terminated field.
                fields.next();
            }
            Some('u') => d.conflicted += 1,
            Some('?') => d.untracked += 1,
            _ => {} // '!' ignored, unknown skipped
        }
    }

    match head.as_deref() {
        Some("(detached)") => d.detached = true,
        Some(h) => d.branch = Some(h.to_string()),
        None => {}
    }
    match oid.as_deref() {
        Some("(initial)") => d.unborn = true,
        Some(o) if !o.is_empty() => d.head_short = Some(o.chars().take(7).collect()),
        _ => {}
    }
    d.incomplete = false;
    d.state = if d.conflicted > 0 {
        GitState::Conflicted
    } else if d.staged + d.unstaged + d.untracked > 0 {
        GitState::Dirty
    } else {
        GitState::Clean
    };
    d
}

/// Record format: `<1|2> XY ...`; X = index, Y = worktree, '.' = unchanged.
/// An entry deleted in the index or the worktree counts once in `deleted`.
fn count_xy(s: &str, d: &mut GitData) {
    let xy = s.split(' ').nth(1).unwrap_or("");
    let mut c = xy.chars();
    if let (Some(x), Some(y)) = (c.next(), c.next()) {
        if x != '.' {
            d.staged += 1;
        }
        if y != '.' {
            d.unstaged += 1;
        }
        if x == 'T' || y == 'T' {
            d.typechanged += 1;
        }
        if x == 'D' || y == 'D' {
            d.deleted += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let raw = b"# branch.oid 0123456789abcdef\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 a b f.txt\0\
1 MM N... 100644 100644 100644 a b g.txt\0\
2 R. N... 100644 100644 100644 a b R100 new.txt\0old.txt\0\
u UU N... 1 2 3 4 a b c conflict.txt\0\
? new dir/\0! ign\0";
        let d = parse_status(raw);
        assert_eq!(d.branch.as_deref(), Some("main"));
        assert_eq!(d.head_short.as_deref(), Some("0123456"));
        assert_eq!((d.ahead, d.behind), (2, 1));
        assert_eq!((d.staged, d.unstaged, d.untracked, d.conflicted), (2, 2, 1, 1));
        assert_eq!((d.renamed, d.deleted), (1, 0));
        assert_eq!(d.state, GitState::Conflicted);
        assert_eq!(d.upstream.as_deref(), Some("origin/main"));
    }

    #[test]
    fn parse_renamed_and_deleted() {
        let raw = b"# branch.oid 0123456789abcdef\0# branch.head main\0\
1 D. N... 100644 000000 000000 a b gone.txt\0\
1 .D N... 100644 100644 000000 a b rm.txt\0\
1 MD N... 100644 100644 000000 a b both.txt\0\
2 RD N... 100644 100644 000000 a b R100 moved.txt\0orig.txt\0\
2 C. N... 100644 100644 100644 a b C75 copy.txt\0src.txt\0\
u DU N... 1 2 3 4 a b c conflict.txt\0";
        let d = parse_status(raw);
        assert_eq!(d.renamed, 2, "renamed and copied records");
        assert_eq!(d.deleted, 4, "D. .D MD RD; the DU conflict is not a deletion");
        assert_eq!((d.staged, d.unstaged, d.conflicted), (4, 3, 1));
        assert_eq!(d.stashed, 0, "stash is not part of porcelain output");
    }

    #[test]
    fn operation_markers_and_precedence() {
        let td = tempfile::tempdir().unwrap();
        let g = td.path();
        assert_eq!(detect_operation(g), (None, None));
        std::fs::write(g.join("BISECT_LOG"), "").unwrap();
        assert_eq!(detect_operation(g).0.as_deref(), Some("bisect"));
        std::fs::write(g.join("REVERT_HEAD"), "x").unwrap();
        assert_eq!(detect_operation(g).0.as_deref(), Some("revert"));
        std::fs::write(g.join("CHERRY_PICK_HEAD"), "x").unwrap();
        assert_eq!(detect_operation(g).0.as_deref(), Some("cherry-pick"));
        std::fs::write(g.join("MERGE_HEAD"), "x").unwrap();
        assert_eq!(detect_operation(g), (Some("merge".into()), None));
        std::fs::create_dir(g.join("rebase-apply")).unwrap();
        assert_eq!(detect_operation(g), (Some("rebase".into()), None), "progress unknown without next/last");
        std::fs::write(g.join("rebase-apply/next"), "2\n").unwrap();
        std::fs::write(g.join("rebase-apply/last"), "5\n").unwrap();
        assert_eq!(detect_operation(g), (Some("rebase".into()), Some((2, 5))));
        std::fs::write(g.join("rebase-apply/applying"), "").unwrap();
        assert_eq!(detect_operation(g), (Some("am".into()), Some((2, 5))));
        std::fs::create_dir(g.join("rebase-merge")).unwrap();
        std::fs::write(g.join("rebase-merge/msgnum"), "1").unwrap();
        std::fs::write(g.join("rebase-merge/end"), "garbage").unwrap();
        assert_eq!(detect_operation(g), (Some("rebase".into()), None));
        std::fs::write(g.join("rebase-merge/end"), "3").unwrap();
        assert_eq!(detect_operation(g), (Some("rebase".into()), Some((1, 3))));
    }

    #[test]
    fn worktree_identity_matches_git_status() {
        let git = |dir: &Path, args: &[&str]| {
            let ok = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_CONFIG_NOSYSTEM", "1").output().is_ok_and(|o| o.status.success());
            assert!(ok, "git {args:?}");
        };
        let td = tempfile::tempdir().unwrap();
        let repo = td.path().join("repo");
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "x"]);
        let t = Duration::from_secs(5);
        let id = worktree_identity(&repo.join("sub")).expect("inside a worktree");
        assert_eq!(Some(id.clone()), git_status(&repo, t).value.unwrap().worktree_id);
        assert_eq!(worktree_identity(&repo).as_deref(), Some(id.as_str()));
        // A linked worktree has its own identity.
        let wt = td.path().join("wt");
        git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let wid = worktree_identity(&wt).unwrap();
        assert_ne!(wid, id);
        assert_eq!(Some(wid), git_status(&wt, t).value.unwrap().worktree_id);
        assert_eq!(worktree_identity(td.path()), None, "outside a repository");
        assert_eq!(worktree_identity(&repo.join(".git")), None, "inside .git");
    }

    #[test]
    fn parse_unborn_detached_garbage() {
        let d = parse_status(b"# branch.oid (initial)\0# branch.head main\0");
        assert!(d.unborn && d.head_short.is_none() && d.branch.as_deref() == Some("main"));
        assert_eq!(d.state, GitState::Clean);
        let d = parse_status(b"# branch.oid abcdef0123\0# branch.head (detached)\0\xff\xfe\0? \xff\0");
        assert!(d.detached && d.branch.is_none());
        assert_eq!(d.untracked, 1);
        assert_eq!(parse_status(b"").state, GitState::Clean);
    }
    #[test]
    fn typechanged_records_count_once_per_file() {
        let mut data = GitData::default();
        count_xy("1 TT N... 100644 100644 120000 a b file", &mut data);
        assert_eq!((data.typechanged, data.staged, data.unstaged), (1, 1, 1));
        count_xy("1 .T N... 100644 100644 120000 a b other", &mut data);
        assert_eq!((data.typechanged, data.staged, data.unstaged), (2, 1, 2));
    }
}
