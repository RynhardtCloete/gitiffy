//! Verify that streaming network ops forward git's full output (not just parsed
//! progress) through the `ProgressSink::line` hook, so the UI can show a real
//! transcript. Uses a local repo-to-repo fetch (no network).

use std::path::{Path, PathBuf};
use std::process::Command;

use gg_git_write::{CancelToken, GitWriter, Progress, ProgressSink, RepoWriter};

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
        "ggstream-{}-{:p}",
        std::process::id(),
        &p as *const _
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[derive(Default)]
struct Capture {
    progress: usize,
    lines: Vec<String>,
}

impl ProgressSink for Capture {
    fn report(&mut self, _p: Progress) {
        self.progress += 1;
    }
    fn line(&mut self, line: &str) {
        self.lines.push(line.to_string());
    }
}

#[test]
fn fetch_streams_full_output() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git not available");
        return;
    }
    let base = unique_tmp();
    let remote = base.join("remote");
    std::fs::create_dir_all(&remote).unwrap();

    // A remote repo with one commit, cloned locally.
    git(&remote, &["init", "-q", "-b", "main"]);
    std::fs::write(remote.join("f.txt"), "one\n").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-q", "-m", "A"]);
    git(&base, &["clone", "-q", remote.to_str().unwrap(), "clone"]);
    let clone = base.join("clone");

    // Advance the remote so the fetch actually transfers a ref update.
    std::fs::write(remote.join("f.txt"), "two\n").unwrap();
    git(&remote, &["commit", "-q", "-am", "B"]);

    let writer = GitWriter::discover(&clone).expect("open clone");
    let mut cap = Capture::default();
    writer
        .fetch("origin", &mut cap, &CancelToken::new())
        .expect("fetch");

    // git prints the ref update ("<old>..<new>  main -> origin/main") to stderr;
    // it isn't a progress meter, so it must reach the transcript via `line`.
    assert!(
        cap.lines.iter().any(|l| l.contains("->")),
        "expected a ref-update line in the streamed transcript, got: {:?}",
        cap.lines
    );

    std::fs::remove_dir_all(&base).ok();
}
