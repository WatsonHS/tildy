//! Separate test binary: mutates PATH for the whole process.
use tildy::model::Freshness;
use tildy::providers::git_status;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{Duration, Instant};

fn fake_git(dir: &std::path::Path, body: &str) {
    let p = dir.join("git");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn timeout_kills_child_and_missing_git_errors() {
    let td = tempfile::tempdir().unwrap();
    let bin = td.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    // `sleep` runs as a grandchild of the wrapper script.
    fake_git(&bin, "sleep 7.123\nexit 0");
    let old = std::env::var_os("PATH").unwrap();
    let mut paths = vec![bin.clone()];
    paths.extend(std::env::split_paths(&old));
    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());

    let t0 = Instant::now();
    let s = git_status(td.path(), Duration::from_millis(1500));
    let el = t0.elapsed();
    assert_eq!(s.freshness, Freshness::Timeout, "{:?}", s);
    assert!(s.value.is_none());
    assert!(el >= Duration::from_millis(1400) && el < Duration::from_millis(1800), "{el:?}");

    std::thread::sleep(Duration::from_millis(200));
    let left = Command::new("pgrep").args(["-f", "sleep 7.123"]).output().unwrap();
    assert!(left.stdout.is_empty(), "leftover: {}", String::from_utf8_lossy(&left.stdout));

    // git missing entirely
    let empty = td.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    std::env::set_var("PATH", &empty);
    let s = git_status(td.path(), Duration::from_millis(500));
    assert_eq!(s.freshness, Freshness::Error);
    assert!(s.error.unwrap().contains("not found"));
}
