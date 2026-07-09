//! End-to-end smoke test for the gix read engine against a real repository
//! built with the system `git`. Skips gracefully if `git` is unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;

use gg_git_read::{GixRepo, RepoReader, WalkOpts};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn unique_tmp() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ggread-{}-{:p}",
        std::process::id(),
        &p as *const _
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

#[test]
fn walk_refs_and_head_on_a_real_repo() {
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = unique_tmp();

    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "first"]);

    std::fs::write(dir.join("a.txt"), "hello\nworld\n").unwrap();
    git(&dir, &["commit", "-q", "-am", "second"]);

    // A feature branch with one commit, merged back.
    git(&dir, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("b.txt"), "feature\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "feature work"]);
    git(&dir, &["checkout", "-q", "main"]);
    git(
        &dir,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
    );

    let repo = GixRepo::discover(&dir).expect("open repo");

    // HEAD resolves to main.
    let head = repo.head().expect("head").expect("born head");
    assert!(head.is_head);

    // Refs include main and feature.
    let refs = repo.refs().expect("refs");
    let names: Vec<String> = refs.iter().map(|r| r.name.short().to_string()).collect();
    assert!(names.iter().any(|n| n == "main"), "names: {names:?}");
    assert!(names.iter().any(|n| n == "feature"), "names: {names:?}");

    // Walk should see all 4 commits (first, second, feature work, merge).
    let commits = repo.walk(&WalkOpts::default()).expect("walk");
    assert_eq!(
        commits.len(),
        4,
        "commits: {:?}",
        commits.iter().map(|c| &c.summary).collect::<Vec<_>>()
    );

    // The merge commit has two parents.
    let merge = commits
        .iter()
        .find(|c| c.summary == "merge feature")
        .unwrap();
    assert_eq!(merge.parents.len(), 2);
    assert!(merge.is_merge());

    // commit() round-trips a single decode and matches the walk.
    let again = repo.commit(merge.oid).unwrap();
    assert_eq!(again.oid, merge.oid);
    assert_eq!(again.parents, merge.parents);

    // Blob content at a commit.
    let blob = repo
        .read_blob_at(merge.oid, Path::new("b.txt"))
        .unwrap()
        .expect("b.txt present at merge");
    assert_eq!(blob, b"feature\n");

    std::fs::remove_dir_all(&dir).ok();
}
