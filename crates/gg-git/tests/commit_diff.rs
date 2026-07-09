//! End-to-end test of `GitEngine::commit_diff`, which backs the history file
//! view: clicking a commit lists the files it changed and previews their diffs.

use std::path::{Path, PathBuf};
use std::process::Command;

use gg_core::FileChange;
use gg_git::GitEngine;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("spawn git")
        .success();
    assert!(ok, "git {args:?} failed");
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn unique_tmp() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ggcd-{}-{:p}", std::process::id(), &p as *const _));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn commit_diff_lists_changed_files() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = unique_tmp();
    git(&dir, &["init", "-q", "-b", "main"]);

    // Root commit: one file added.
    std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "A"]);
    let a = git_out(&dir, &["rev-parse", "HEAD"]);

    // Second commit: modify a.txt and add b.txt.
    std::fs::write(dir.join("a.txt"), "alpha edited\n").unwrap();
    std::fs::write(dir.join("b.txt"), "beta\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "B"]);
    let b = git_out(&dir, &["rev-parse", "HEAD"]);

    let engine = GitEngine::discover(&dir).expect("open engine");

    // The root commit shows its single file as an addition.
    let a_oid = gg_core::Oid::from_hex(&a).unwrap();
    let root = engine.commit_diff(a_oid).expect("commit_diff root");
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].path, PathBuf::from("a.txt"));
    assert_eq!(root[0].change, FileChange::Added);

    // The second commit lists the modified and the added file.
    let b_oid = gg_core::Oid::from_hex(&b).unwrap();
    let mut files = engine.commit_diff(b_oid).expect("commit_diff B");
    files.sort_by(|x, y| x.path.cmp(&y.path));
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].path, PathBuf::from("a.txt"));
    assert_eq!(files[0].change, FileChange::Modified);
    assert!(files[0].additions() >= 1 && files[0].deletions() >= 1);
    assert_eq!(files[1].path, PathBuf::from("b.txt"));
    assert_eq!(files[1].change, FileChange::Added);

    std::fs::remove_dir_all(&dir).ok();
}
