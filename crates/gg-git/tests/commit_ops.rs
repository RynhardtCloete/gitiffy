//! End-to-end tests for the commit/working-tree operations the history and
//! changes context menus drive: reset (mixed/hard), revert, stash, and
//! cherry-pick, against a real repo built with system git.

use std::path::{Path, PathBuf};
use std::process::Command;

use gg_git::{GitEngine, RepoWriter, ResetMode, StashOp};

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

fn unique_tmp(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ggops-{tag}-{}-{:p}",
        std::process::id(),
        &tag as *const _
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

#[test]
fn reset_revert_and_stash() {
    if !have_git() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = unique_tmp("rrs");
    let file = dir.join("f.txt");
    git(&dir, &["init", "-q", "-b", "main"]);
    // Engine-spawned git needs an identity; CI runners have no global one.
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "user.email", "test@example.com"]);
    std::fs::write(&file, "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "A"]);
    let a = git_out(&dir, &["rev-parse", "HEAD"]);
    std::fs::write(&file, "two\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "B"]);
    let b = git_out(&dir, &["rev-parse", "HEAD"]);

    let engine = GitEngine::discover(&dir).expect("open engine");

    // Mixed reset to A: HEAD moves back, working tree keeps "two" as an unstaged
    // modification.
    engine
        .writer()
        .reset(&a, ResetMode::Mixed)
        .expect("reset --mixed");
    assert_eq!(git_out(&dir, &["rev-parse", "HEAD"]), a);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "two\n");
    let status = engine.status().expect("status");
    assert!(status.entries.iter().any(|e| e.has_unstaged()));

    // Hard reset back to B: working tree matches B and is clean.
    engine
        .writer()
        .reset(&b, ResetMode::Hard)
        .expect("reset --hard");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "two\n");
    assert!(engine.status().expect("status").is_clean());

    // Revert B: a new commit restores "one"; tree clean, history grows.
    let b_oid = gg_core::Oid::from_hex(&b).unwrap();
    engine.writer().revert(b_oid).expect("revert");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
    assert!(engine.status().expect("status").is_clean());
    assert_eq!(git_out(&dir, &["rev-list", "--count", "HEAD"]), "3");

    // Stash a dirty tree: the change is shelved and the tree goes clean.
    std::fs::write(&file, "dirty\n").unwrap();
    engine
        .writer()
        .stash(&StashOp::Push {
            message: None,
            include_untracked: false,
        })
        .expect("stash push");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
    assert!(engine.status().expect("status").is_clean());
    assert_eq!(git_out(&dir, &["stash", "list"]).lines().count(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cherry_pick_across_branches() {
    if !have_git() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = unique_tmp("cp");
    let file = dir.join("f.txt");
    git(&dir, &["init", "-q", "-b", "main"]);
    // Engine-spawned git needs an identity; CI runners have no global one.
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "user.email", "test@example.com"]);
    std::fs::write(&file, "base\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "base"]);

    // A commit on a side branch that we want to cherry-pick onto main.
    git(&dir, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(&file, "feature change\n").unwrap();
    git(&dir, &["commit", "-q", "-am", "feature work"]);
    let c = git_out(&dir, &["rev-parse", "HEAD"]);

    git(&dir, &["checkout", "-q", "main"]);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "base\n");

    let engine = GitEngine::discover(&dir).expect("open engine");
    let c_oid = gg_core::Oid::from_hex(&c).unwrap();
    engine.writer().cherry_pick(c_oid).expect("cherry-pick");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "feature change\n");
    assert!(engine.status().expect("status").is_clean());

    std::fs::remove_dir_all(&dir).ok();
}
