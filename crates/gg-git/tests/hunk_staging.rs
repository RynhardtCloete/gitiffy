//! End-to-end test of hunk-level staging through the facade: take a file with
//! two well-separated changes (two hunks), stage only the first hunk via the
//! single-hunk patch slicer + `git apply --cached`, then unstage it again by
//! reverse-applying the staged hunk. Runs against a real repo built with system
//! git, exercising exactly the path the GUI's stage/unstage-hunk buttons use.

use std::path::{Path, PathBuf};
use std::process::Command;

use gg_core::LineKind;
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
    p.push(format!(
        "gghunk-{}-{:p}",
        std::process::id(),
        &p as *const _
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// True if the diff has an added line whose text equals `needle`.
fn has_addition(diff: &gg_core::FileDiff, needle: &str) -> bool {
    diff.hunks
        .iter()
        .flat_map(|h| &h.lines)
        .any(|l| l.kind == LineKind::Addition && l.text == needle)
}

#[test]
fn stage_then_unstage_a_single_hunk() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = unique_tmp();
    git(&dir, &["init", "-q", "-b", "main"]);

    let file = Path::new("f.txt");
    let original: String = (1..=20).map(|n| format!("line{n}\n")).collect();
    std::fs::write(dir.join(file), &original).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "initial"]);

    // Two changes far enough apart that git keeps them as separate hunks.
    let modified: String = (1..=20)
        .map(|n| match n {
            2 => "LINE_TWO\n".to_string(),
            19 => "LINE_NINETEEN\n".to_string(),
            _ => format!("line{n}\n"),
        })
        .collect();
    std::fs::write(dir.join(file), &modified).unwrap();

    let engine = GitEngine::discover(&dir).expect("open engine");

    // The unstaged diff has two hunks; its raw text drives single-hunk staging.
    let (diff, raw) = engine
        .diff_file_with_raw(file, false, false)
        .expect("unstaged diff");
    assert_eq!(diff.hunks.len(), 2, "expected two separate hunks");
    assert!(!raw.is_empty());

    // Stage ONLY the first hunk (the line-2 change).
    let patch0 = gg_diff::single_hunk_patch(&raw, 0).expect("hunk-0 patch");
    engine
        .writer()
        .apply_to_index(&patch0, false)
        .expect("stage hunk 0");

    // The file is now both staged (hunk 0) and unstaged (hunk 1).
    let status = engine.status().expect("status");
    let e = status
        .entries
        .iter()
        .find(|e| e.path.as_path() == file)
        .expect("entry");
    assert!(e.is_staged(), "first hunk should be staged");
    assert!(e.has_unstaged(), "second hunk should remain unstaged");

    let staged = engine.diff_file(file, true, false).expect("staged diff");
    assert_eq!(staged.hunks.len(), 1);
    assert!(has_addition(&staged, "LINE_TWO"), "staged the wrong hunk");
    assert!(!has_addition(&staged, "LINE_NINETEEN"));

    let unstaged = engine.diff_file(file, false, false).expect("unstaged diff");
    assert_eq!(unstaged.hunks.len(), 1);
    assert!(has_addition(&unstaged, "LINE_NINETEEN"));
    assert!(!has_addition(&unstaged, "LINE_TWO"));

    // Now unstage that hunk by reverse-applying the staged diff's only hunk.
    let (_s, staged_raw) = engine
        .diff_file_with_raw(file, true, false)
        .expect("staged raw");
    let unstage_patch = gg_diff::single_hunk_patch(&staged_raw, 0).expect("staged hunk patch");
    engine
        .writer()
        .apply_to_index(&unstage_patch, true)
        .expect("unstage hunk 0");

    // Back to fully unstaged: nothing staged, both changes in the worktree.
    let status = engine.status().expect("status after unstage");
    let e = status
        .entries
        .iter()
        .find(|e| e.path.as_path() == file)
        .expect("entry");
    assert!(!e.is_staged(), "should be fully unstaged again");
    assert!(e.has_unstaged());
    let unstaged2 = engine.diff_file(file, false, false).expect("unstaged diff");
    assert_eq!(unstaged2.hunks.len(), 2, "both hunks unstaged again");

    std::fs::remove_dir_all(&dir).ok();
}
