//! End-to-end test of the working-tree pipeline through the facade:
//! status → stage → diff → commit, against a real repo built with system git.

use std::path::{Path, PathBuf};
use std::process::Command;

use gg_core::ChangeKind;
use gg_git::{GitEngine, RepoWriter};

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

fn unique_tmp() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ggwt-{}-{:p}", std::process::id(), &p as *const _));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn status_stage_diff_commit_roundtrip() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = unique_tmp();
    git(&dir, &["init", "-q", "-b", "main"]);
    // Engine-spawned git needs an identity; CI runners have no global one.
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "user.email", "test@example.com"]);
    std::fs::write(dir.join("tracked.txt"), "one\ntwo\nthree\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "initial"]);

    // Make a tracked modification and add an untracked file.
    std::fs::write(dir.join("tracked.txt"), "one\nTWO\nthree\n").unwrap();
    std::fs::write(dir.join("new.txt"), "fresh\n").unwrap();

    let engine = GitEngine::discover(&dir).expect("open engine");

    // Status sees an unstaged modification and an untracked file.
    let status = engine.status().expect("status");
    assert_eq!(status.branch.as_deref(), Some("main"));
    let tracked = status
        .entries
        .iter()
        .find(|e| e.path.as_path() == Path::new("tracked.txt"))
        .expect("tracked entry");
    assert_eq!(tracked.unstaged, ChangeKind::Modified);
    let untracked = status
        .entries
        .iter()
        .find(|e| e.path.as_path() == Path::new("new.txt"))
        .expect("untracked entry");
    assert_eq!(untracked.unstaged, ChangeKind::Untracked);

    // Unstaged diff of the tracked file shows the TWO change.
    let diff = engine
        .diff_file(Path::new("tracked.txt"), false, false)
        .expect("unstaged diff");
    assert!(!diff.hunks.is_empty());
    assert_eq!(diff.additions(), 1);
    assert_eq!(diff.deletions(), 1);

    // Untracked file diffs as all-additions.
    let new_diff = engine
        .diff_file(Path::new("new.txt"), false, true)
        .expect("untracked diff");
    assert_eq!(new_diff.additions(), 1);

    // Stage both, then status reflects the staged side.
    engine
        .writer()
        .stage(&[Path::new("tracked.txt"), Path::new("new.txt")])
        .expect("stage");
    let status = engine.status().expect("status after stage");
    let tracked = status
        .entries
        .iter()
        .find(|e| e.path.as_path() == Path::new("tracked.txt"))
        .unwrap();
    assert!(tracked.is_staged());
    assert_eq!(tracked.staged, ChangeKind::Modified);

    // Staged diff is available against HEAD.
    let staged_diff = engine
        .diff_file(Path::new("tracked.txt"), true, false)
        .expect("staged diff");
    assert_eq!(staged_diff.additions(), 1);

    // Commit, then the working tree is clean.
    engine
        .writer()
        .commit("second", &Default::default())
        .expect("commit");
    let status = engine.status().expect("status after commit");
    assert!(status.is_clean(), "entries: {:?}", status.entries);

    // Unstage flow: modify, stage, then unstage_all.
    std::fs::write(dir.join("tracked.txt"), "one\nTWO\nTHREE\n").unwrap();
    engine.writer().stage_all().expect("stage all");
    assert!(engine
        .status()
        .unwrap()
        .entries
        .iter()
        .any(|e| e.is_staged()));
    engine.writer().unstage_all().expect("unstage all");
    assert!(engine
        .status()
        .unwrap()
        .entries
        .iter()
        .all(|e| !e.is_staged()));

    std::fs::remove_dir_all(&dir).ok();
}
