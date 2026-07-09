//! End-to-end test of the branch operations the toolbar branch menu drives:
//! create, create-and-checkout, rename, checkout, and delete, against a real
//! repo built with system git. Asserts the read side (refs + status branch)
//! reflects each mutation.

use std::path::{Path, PathBuf};
use std::process::Command;

use gg_core::RefKind;
use gg_git::{GitEngine, RepoReader, RepoWriter};

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
    p.push(format!(
        "ggbranch-{}-{:p}",
        std::process::id(),
        &p as *const _
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// The set of local branch short-names currently in the repo.
fn local_branches(engine: &GitEngine) -> Vec<String> {
    engine
        .reader()
        .refs()
        .expect("refs")
        .into_iter()
        .filter(|r| r.kind == RefKind::LocalBranch)
        .map(|r| r.name.short().to_string())
        .collect()
}

#[test]
fn create_checkout_rename_delete_roundtrip() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = unique_tmp();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("f.txt"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "initial"]);

    let engine = GitEngine::discover(&dir).expect("open engine");

    // Create a branch at HEAD without checking it out.
    engine
        .writer()
        .branch_create("feature", None)
        .expect("create");
    let branches = local_branches(&engine);
    assert!(branches.contains(&"feature".to_string()));
    assert!(branches.contains(&"main".to_string()));
    assert_eq!(engine.status().unwrap().branch.as_deref(), Some("main"));

    // Create + check out in one step.
    engine
        .writer()
        .checkout_new("topic", None)
        .expect("checkout -b");
    assert_eq!(engine.status().unwrap().branch.as_deref(), Some("topic"));
    // The new HEAD branch is flagged as such in the ref list.
    assert!(engine
        .reader()
        .refs()
        .unwrap()
        .iter()
        .any(|r| r.kind == RefKind::LocalBranch && r.name.short() == "topic" && r.is_head));

    // Rename the (non-current) branch.
    engine
        .writer()
        .branch_rename("feature", "feat")
        .expect("rename");
    let branches = local_branches(&engine);
    assert!(branches.contains(&"feat".to_string()));
    assert!(!branches.contains(&"feature".to_string()));

    // Switch back to main, then delete the merged branch.
    engine.writer().checkout("main").expect("checkout main");
    assert_eq!(engine.status().unwrap().branch.as_deref(), Some("main"));
    engine
        .writer()
        .branch_delete("feat", false)
        .expect("delete");
    let branches = local_branches(&engine);
    assert!(!branches.contains(&"feat".to_string()));
    assert!(branches.contains(&"topic".to_string()));

    std::fs::remove_dir_all(&dir).ok();
}
