use serde_json::{json, Value};
use tildy::model::{Freshness, GitData, GitState};
use tildy::providers::git_status;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const T: Duration = Duration::from_secs(10);

fn build() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/git/make.sh");
    let st = Command::new("sh").arg(&script).arg(td.path()).output().unwrap();
    assert!(st.status.success(), "make.sh failed: {}", String::from_utf8_lossy(&st.stderr));
    td
}

fn stable(d: &GitData) -> Value {
    json!({
        "in_repo": d.in_repo,
        "branch": d.branch,
        "detached": d.detached,
        "unborn": d.unborn,
        "ahead": d.ahead,
        "behind": d.behind,
        "staged": d.staged,
        "unstaged": d.unstaged,
        "untracked": d.untracked,
        "conflicted": d.conflicted,
        "state": d.state,
        "renamed": d.renamed,
        "deleted": d.deleted,
        "stashed": d.stashed,
        "operation": d.operation,
        "operation_progress": d.operation_progress,
    })
}

#[test]
fn fixtures_match_expected() {
    let td = build();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/git/expected");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let name = p.file_stem().unwrap().to_str().unwrap().to_string();
        let expected: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let slot = git_status(&td.path().join(&name), T);
        assert_eq!(slot.freshness, Freshness::Fresh, "{name}: {:?}", slot.error);
        let d = slot.value.expect("value");
        assert_eq!(stable(&d), expected, "scenario {name}");
        assert!(!d.incomplete, "{name}");
        n += 1;
    }
    assert!(n >= 17, "only {n} scenarios");
}

#[test]
fn identity_fields() {
    let td = build();
    let root = td.path().canonicalize().unwrap();
    let main = git_status(&root.join("linked_worktree_main"), T).value.unwrap();
    let wt = git_status(&root.join("linked_worktree"), T).value.unwrap();
    assert_eq!(wt.worktree_root.as_deref(), Some(root.join("linked_worktree").as_path()));
    assert_ne!(main.worktree_id, wt.worktree_id);
    assert_ne!(main.git_dir, wt.git_dir);
    assert_eq!(main.head_short.as_ref().map(|s| s.len()), Some(7));

    // subdirectory resolves to the same worktree root
    let sub = root.join("clean/deep/er");
    std::fs::create_dir_all(&sub).unwrap();
    let d = git_status(&sub, T).value.unwrap();
    assert_eq!(d.worktree_root.as_deref(), Some(root.join("clean").as_path()));

    let ab = git_status(&root.join("ahead_behind"), T).value.unwrap();
    assert_eq!(ab.upstream.as_deref(), Some("origin/main"));
    let un = git_status(&root.join("unborn"), T).value.unwrap();
    assert!(un.head_short.is_none());
    let nr = git_status(&root.join("not_a_repo"), T).value.unwrap();
    assert!(!nr.in_repo && nr.state == GitState::Unknown && nr.worktree_root.is_none());
}

/// The stash count read from the reflog file equals what git itself reports,
/// and the operation markers are read from the worktree's own git dir.
#[test]
fn stash_count_matches_git_and_operations_are_per_worktree() {
    let td = build();
    let root = td.path().canonicalize().unwrap();
    let git = |dir: &Path, args: &[&str]| -> String {
        let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    let stash = root.join("stash");
    let d = git_status(&stash, T).value.unwrap();
    assert_eq!(d.stashed.to_string(), git(&stash, &["rev-list", "--walk-reflogs", "--count", "refs/stash"]));
    // Dropping an entry rewrites the reflog.
    git(&stash, &["stash", "drop", "-q"]);
    assert_eq!(git_status(&stash, T).value.unwrap().stashed, 1);
    // A linked worktree shares refs/stash with its main worktree.
    let main = root.join("linked_worktree_main");
    git(&main, &["-c", "user.name=a", "-c", "user.email=a@b", "stash", "-q", "--include-untracked"]);
    std::fs::write(main.join("x.txt"), "x").unwrap();
    git(&main, &["-c", "user.name=a", "-c", "user.email=a@b", "stash", "-q", "--include-untracked"]);
    assert_eq!(git_status(&main, T).value.unwrap().stashed, 1);
    assert_eq!(git_status(&root.join("linked_worktree"), T).value.unwrap().stashed, 1);

    let linked = root.join("linked_worktree");
    let gdir = PathBuf::from(git(&linked, &["rev-parse", "--absolute-git-dir"]));
    std::fs::write(gdir.join("BISECT_LOG"), "").unwrap();
    assert_eq!(git_status(&linked, T).value.unwrap().operation.as_deref(), Some("bisect"));
    assert_eq!(git_status(&main, T).value.unwrap().operation, None, "main worktree is idle");
}

#[test]
fn missing_cwd_is_error_not_panic() {
    let s = git_status(Path::new("/definitely/not/here/tildy"), T);
    assert_eq!(s.freshness, Freshness::Error);
    assert!(s.error.is_some());
}

#[test]
fn non_utf8_and_odd_paths() {
    use std::os::unix::ffi::OsStrExt;
    let td = build();
    let base = td.path().join("clean");
    // macOS filesystems reject non-UTF-8 names; use them only where allowed.
    let mut odd = base.join(PathBuf::from(std::ffi::OsStr::from_bytes(b"d\xff dir")));
    if std::fs::create_dir_all(&odd).is_err() {
        odd = base.join("d \u{e9}\u{4e2d} 'q' $x");
        std::fs::create_dir_all(&odd).unwrap();
    }
    std::fs::write(odd.join("f ile 'q' $x"), "x").unwrap();
    std::fs::write(base.join("sp ace\ttab.txt"), "x").unwrap();
    let s = git_status(&base, T);
    assert_eq!(s.freshness, Freshness::Fresh);
    let d = s.value.unwrap();
    assert_eq!(d.untracked, 2);
    assert_eq!(d.state, GitState::Dirty);
    let s = git_status(&odd, T);
    assert_eq!(s.freshness, Freshness::Fresh);
    assert!(s.value.unwrap().in_repo);
}

/// `cargo test --release --test providers_git -- --ignored --nocapture bench`
#[test]
#[ignore]
fn bench_timing() {
    fn median(p: &Path) -> f64 {
        let mut v: Vec<f64> = (0..20)
            .map(|_| {
                let t = std::time::Instant::now();
                let s = git_status(p, T);
                assert_eq!(s.freshness, Freshness::Fresh);
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[9] + v[10]) / 2.0
    }
    let td = tempfile::tempdir().unwrap();
    let r = td.path();
    let g = |a: &[&str]| assert!(Command::new("git").arg("-C").arg(r).args(a).output().unwrap().status.success());
    g(&["init", "-q"]);
    for i in 0..50 {
        std::fs::write(r.join(format!("f{i}.txt")), format!("{i}")).unwrap();
    }
    g(&["add", "."]);
    g(&["-c", "user.name=a", "-c", "user.email=a@b", "commit", "-qm", "x"]);
    println!("temp 50-file repo median: {:.1} ms", median(r));
    println!("tildy repo median: {:.1} ms", median(Path::new(env!("CARGO_MANIFEST_DIR"))));
}
