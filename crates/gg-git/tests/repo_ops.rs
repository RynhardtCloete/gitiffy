//! End-to-end tests for the stash-list, remote, and tag operations the new
//! toolbar/menu UI drives, against a real repo built with system git.

use std::path::{Path, PathBuf};
use std::process::Command;

use gg_core::RefKind;
use gg_git::{GitEngine, RepoReader, RepoWriter, ResetMode, StashOp};

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

fn unique_tmp(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ggrepo-{tag}-{}-{:p}",
        std::process::id(),
        &tag as *const _
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn tag_names(engine: &GitEngine) -> Vec<String> {
    engine
        .reader()
        .refs()
        .expect("refs")
        .into_iter()
        .filter(|r| r.kind == RefKind::Tag)
        .map(|r| r.name.short().to_string())
        .collect()
}

#[test]
fn stash_list_apply_drop_pop() {
    if !have_git() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = unique_tmp("stash");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    git(&dir, &["init", "-q", "-b", "main"]);
    // Engine-spawned git needs an identity; CI runners have no global one.
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "user.email", "test@example.com"]);
    // Pin newline conversion off so byte-exact content asserts hold under
    // the Windows runners' autocrlf=true default.
    git(&dir, &["config", "core.autocrlf", "false"]);
    std::fs::write(&a, "a\n").unwrap();
    std::fs::write(&b, "b\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "base"]);

    let engine = GitEngine::discover(&dir).expect("open engine");
    let push = |msg: &str| {
        engine
            .writer()
            .stash(&StashOp::Push {
                message: Some(msg.to_string()),
                include_untracked: false,
            })
            .expect("stash push");
    };

    // Two stashes from disjoint files (so later apply/pop never conflict).
    std::fs::write(&a, "a2\n").unwrap();
    push("change-a");
    std::fs::write(&b, "b2\n").unwrap();
    push("change-b");
    assert_eq!(engine.writer().stash_list().unwrap().len(), 2);

    // Apply the newest without dropping it: tree changes, list unchanged.
    engine
        .writer()
        .stash(&StashOp::Apply { index: Some(0) })
        .expect("apply");
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "b2\n");
    assert_eq!(engine.writer().stash_list().unwrap().len(), 2);

    // Reset to a clean tree, then drop the newest stash.
    engine
        .writer()
        .reset("HEAD", ResetMode::Hard)
        .expect("reset");
    engine
        .writer()
        .stash(&StashOp::Drop { index: Some(0) })
        .expect("drop");
    let list = engine.writer().stash_list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].index, 0);

    // Pop the remaining stash: it applies and is removed.
    engine
        .writer()
        .stash(&StashOp::Pop { index: Some(0) })
        .expect("pop");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "a2\n");
    assert!(engine.writer().stash_list().unwrap().is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn remotes_add_list_remove() {
    if !have_git() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = unique_tmp("remote");
    git(&dir, &["init", "-q", "-b", "main"]);
    // Engine-spawned git needs an identity; CI runners have no global one.
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "user.email", "test@example.com"]);
    // Pin newline conversion off so byte-exact content asserts hold under
    // the Windows runners' autocrlf=true default.
    git(&dir, &["config", "core.autocrlf", "false"]);
    let engine = GitEngine::discover(&dir).expect("open engine");

    assert!(engine.writer().remotes().unwrap().is_empty());
    engine
        .writer()
        .remote_add("origin", "https://example.com/repo.git")
        .expect("add origin");
    engine
        .writer()
        .remote_add("upstream", "https://example.com/up.git")
        .expect("add upstream");

    let mut remotes = engine.writer().remotes().unwrap();
    remotes.sort_by(|x, y| x.name.cmp(&y.name));
    assert_eq!(remotes.len(), 2);
    assert_eq!(remotes[0].name, "origin");
    assert_eq!(remotes[0].url, "https://example.com/repo.git");
    assert_eq!(remotes[1].name, "upstream");

    engine.writer().remote_remove("origin").expect("remove");
    let remotes = engine.writer().remotes().unwrap();
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].name, "upstream");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn tag_create_and_delete() {
    if !have_git() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = unique_tmp("tag");
    git(&dir, &["init", "-q", "-b", "main"]);
    // Engine-spawned git needs an identity; CI runners have no global one.
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "user.email", "test@example.com"]);
    // Pin newline conversion off so byte-exact content asserts hold under
    // the Windows runners' autocrlf=true default.
    git(&dir, &["config", "core.autocrlf", "false"]);
    std::fs::write(dir.join("f.txt"), "hi\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "base"]);

    let engine = GitEngine::discover(&dir).expect("open engine");
    engine
        .writer()
        .tag_create("v1", None, None)
        .expect("lightweight tag");
    engine
        .writer()
        .tag_create("v2", None, Some("release two"))
        .expect("annotated tag");
    let tags = tag_names(&engine);
    assert!(tags.contains(&"v1".to_string()));
    assert!(tags.contains(&"v2".to_string()));

    engine.writer().tag_delete("v1").expect("delete tag");
    let tags = tag_names(&engine);
    assert!(!tags.contains(&"v1".to_string()));
    assert!(tags.contains(&"v2".to_string()));

    std::fs::remove_dir_all(&dir).ok();
}
