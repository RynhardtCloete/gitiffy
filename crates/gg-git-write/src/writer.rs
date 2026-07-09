//! The `RepoWriter` trait and its system-git implementation.
//!
//! Per the architecture, *every* operation that mutates the repository or
//! touches the network goes through here, by shelling out to the user's own
//! `git` so that credential helpers, hooks, and config all behave exactly as
//! they do on the command line.

use std::ffi::OsStr;
use std::path::Path;

use gg_core::{GitError, Oid, Remote, Result, StashEntry, StatusSnapshot};

use crate::cancel::CancelToken;
use crate::git::Git;
use crate::progress::ProgressSink;
use crate::status;
use crate::types::{CommitOpts, MergeOpts, RebaseOpts, ResetMode, StashOp};

/// The mutating / network side of a git repository.
pub trait RepoWriter: Send + Sync {
    /// Stage paths (`git add -- <paths>`).
    fn stage(&self, paths: &[&Path]) -> Result<()>;
    /// Stage every change including untracked (`git add -A`).
    fn stage_all(&self) -> Result<()>;
    /// Unstage paths (`git restore --staged -- <paths>`).
    fn unstage(&self, paths: &[&Path]) -> Result<()>;
    /// Apply a unified-diff patch to the index, for hunk-level staging
    /// (`git apply --cached`). Set `reverse` to unstage a hunk.
    fn apply_to_index(&self, patch: &str, reverse: bool) -> Result<()>;
    /// Discard working-tree changes to paths (`git restore -- <paths>`).
    fn discard(&self, paths: &[&Path]) -> Result<()>;

    /// Create a commit and return its new oid.
    fn commit(&self, message: &str, opts: &CommitOpts) -> Result<Oid>;

    /// Create a branch at `start` (or HEAD if `None`).
    fn branch_create(&self, name: &str, start: Option<&str>) -> Result<()>;
    /// Delete a branch (`force` uses `-D`).
    fn branch_delete(&self, name: &str, force: bool) -> Result<()>;
    /// Rename a branch.
    fn branch_rename(&self, old: &str, new: &str) -> Result<()>;
    /// Check out an existing branch/ref/commit.
    fn checkout(&self, target: &str) -> Result<()>;
    /// Create a branch and check it out in one step (`git checkout -b`).
    fn checkout_new(&self, name: &str, start: Option<&str>) -> Result<()>;

    /// Merge `target` into the current branch.
    fn merge(&self, target: &str, opts: &MergeOpts) -> Result<()>;
    /// Rebase the current branch onto `upstream`.
    fn rebase(&self, upstream: &str, opts: &RebaseOpts) -> Result<()>;
    /// Reset HEAD (and optionally index/worktree) to `target`.
    fn reset(&self, target: &str, mode: ResetMode) -> Result<()>;
    /// Revert a commit, creating a new commit (`--no-edit`).
    fn revert(&self, oid: Oid) -> Result<()>;
    /// Cherry-pick a commit onto the current branch.
    fn cherry_pick(&self, oid: Oid) -> Result<()>;

    /// Run a stash operation.
    fn stash(&self, op: &StashOp) -> Result<()>;

    /// Create a tag (annotated when `message` is `Some`).
    fn tag_create(&self, name: &str, target: Option<&str>, message: Option<&str>) -> Result<()>;
    /// Delete a tag.
    fn tag_delete(&self, name: &str) -> Result<()>;

    /// Add a remote.
    fn remote_add(&self, name: &str, url: &str) -> Result<()>;
    /// Remove a remote.
    fn remote_remove(&self, name: &str) -> Result<()>;

    /// Fetch from a remote with progress.
    fn fetch(&self, remote: &str, sink: &mut dyn ProgressSink, cancel: &CancelToken) -> Result<()>;
    /// Pull (fetch + integrate) from a remote with progress.
    fn pull(&self, remote: &str, sink: &mut dyn ProgressSink, cancel: &CancelToken) -> Result<()>;
    /// Push a refspec to a remote with progress.
    fn push(
        &self,
        remote: &str,
        refspec: &str,
        force: bool,
        sink: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<()>;
}

/// `RepoWriter` backed by the system `git` binary.
#[derive(Clone, Debug)]
pub struct GitWriter {
    git: Git,
}

impl GitWriter {
    /// Wrap an existing [`Git`] handle.
    pub fn new(git: Git) -> Self {
        Self { git }
    }

    /// Discover the repository containing `start`.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            git: Git::discover(start)?,
        })
    }

    /// The underlying git handle (to share askpass/env configuration).
    pub fn git(&self) -> &Git {
        &self.git
    }

    fn head_oid(&self) -> Result<Oid> {
        let out = self.git.run(["rev-parse", "HEAD"])?;
        Oid::from_hex(out.trim()).map_err(Into::into)
    }

    /// Working-tree status via porcelain v2 (the CLI reserve read path).
    pub fn status(&self) -> Result<StatusSnapshot> {
        let out = self.git.run([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ])?;
        Ok(status::parse(&out))
    }

    /// Raw unified diff for one path (`git diff [--cached] -- <path>`).
    pub fn raw_diff(&self, path: &Path, staged: bool) -> Result<String> {
        let mut args: Vec<&OsStr> = vec!["diff".as_ref(), "--no-ext-diff".as_ref()];
        if staged {
            args.push("--cached".as_ref());
        }
        args.push("--".as_ref());
        args.push(path.as_os_str());
        self.git.run(args)
    }

    /// Raw unified diff introduced by a commit, against its first parent (or the
    /// empty tree for the root commit), with rename detection. Used to inspect a
    /// historical commit's changed files (`git show --first-parent ...`).
    pub fn commit_raw_diff(&self, oid: Oid) -> Result<String> {
        let hex = oid.to_hex();
        self.git.run([
            "show",
            "--first-parent",
            "--no-ext-diff",
            "-M",
            "--format=",
            hex.as_str(),
        ])
    }

    /// List stash entries (`git stash list`), most-recent first.
    pub fn stash_list(&self) -> Result<Vec<StashEntry>> {
        // `%gd` is the stash selector ("stash@{N}"), `%s` the subject; NUL-join
        // the two so subjects containing tabs/spaces parse cleanly.
        let out = self.git.run(["stash", "list", "--format=%gd%x00%s"])?;
        let mut entries = Vec::new();
        for line in out.lines() {
            let mut parts = line.splitn(2, '\0');
            let selector = parts.next().unwrap_or("");
            let message = parts.next().unwrap_or("").to_string();
            if let Some(index) = selector
                .strip_prefix("stash@{")
                .and_then(|s| s.strip_suffix('}'))
                .and_then(|s| s.parse::<usize>().ok())
            {
                entries.push(StashEntry { index, message });
            }
        }
        Ok(entries)
    }

    /// List configured remotes with their fetch URLs (`git remote -v`).
    pub fn remotes(&self) -> Result<Vec<Remote>> {
        let out = self.git.run(["remote", "-v"])?;
        let mut remotes: Vec<Remote> = Vec::new();
        for line in out.lines() {
            // Each line is "<name>\t<url> (fetch|push)"; keep the fetch URL once.
            let mut it = line.split_whitespace();
            let (Some(name), Some(url), Some(kind)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            if kind == "(fetch)" && !remotes.iter().any(|r| r.name == name) {
                remotes.push(Remote {
                    name: name.to_string(),
                    url: url.to_string(),
                });
            }
        }
        Ok(remotes)
    }

    /// Unstage everything (`git reset -q HEAD`), leaving the working tree alone.
    pub fn unstage_all(&self) -> Result<()> {
        self.git.run(["reset", "-q", "HEAD"]).map(|_| ())
    }

    /// Delete untracked files (`git clean -f -- <paths>`).
    pub fn clean(&self, paths: &[&Path]) -> Result<()> {
        let mut args: Vec<&OsStr> = vec!["clean".as_ref(), "-f".as_ref(), "--".as_ref()];
        args.extend(paths.iter().map(|p| p.as_os_str()));
        self.git.run(args).map(|_| ())
    }

    /// Push the current branch using the repo's configured upstream/push rules
    /// (`git push`), streaming progress.
    pub fn push_current(
        &self,
        force: bool,
        sink: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<()> {
        let mut args: Vec<&str> = vec!["push"];
        if force {
            args.push("--force-with-lease");
        }
        self.git.run_streaming(args, sink, cancel)
    }
}

/// Borrow a list of paths as the `OsStr` args git expects.
fn path_args<'a>(paths: &'a [&'a Path]) -> Vec<&'a std::ffi::OsStr> {
    paths.iter().map(|p| p.as_os_str()).collect()
}

impl RepoWriter for GitWriter {
    fn stage(&self, paths: &[&Path]) -> Result<()> {
        let mut args: Vec<&std::ffi::OsStr> = vec!["add".as_ref(), "--".as_ref()];
        args.extend(path_args(paths));
        self.git.run(args).map(|_| ())
    }

    fn stage_all(&self) -> Result<()> {
        self.git.run(["add", "-A"]).map(|_| ())
    }

    fn unstage(&self, paths: &[&Path]) -> Result<()> {
        let mut args: Vec<&std::ffi::OsStr> =
            vec!["restore".as_ref(), "--staged".as_ref(), "--".as_ref()];
        args.extend(path_args(paths));
        self.git.run(args).map(|_| ())
    }

    fn apply_to_index(&self, patch: &str, reverse: bool) -> Result<()> {
        let mut args: Vec<&str> = vec!["apply", "--cached", "--whitespace=nowarn"];
        if reverse {
            args.push("--reverse");
        }
        args.push("-");
        self.git.run_with_stdin(args, patch.as_bytes()).map(|_| ())
    }

    fn discard(&self, paths: &[&Path]) -> Result<()> {
        let mut args: Vec<&std::ffi::OsStr> = vec!["restore".as_ref(), "--".as_ref()];
        args.extend(path_args(paths));
        self.git.run(args).map(|_| ())
    }

    fn commit(&self, message: &str, opts: &CommitOpts) -> Result<Oid> {
        let mut args: Vec<String> = vec!["commit".into(), "-m".into(), message.into()];
        if opts.amend {
            args.push("--amend".into());
        }
        if opts.allow_empty {
            args.push("--allow-empty".into());
        }
        if opts.all {
            args.push("-a".into());
        }
        if opts.sign {
            args.push("-S".into());
        }
        if let Some(author) = &opts.author {
            args.push("--author".into());
            args.push(author.clone());
        }
        self.git.run(&args)?;
        self.head_oid()
    }

    fn branch_create(&self, name: &str, start: Option<&str>) -> Result<()> {
        let mut args = vec!["branch", name];
        if let Some(s) = start {
            args.push(s);
        }
        self.git.run(args).map(|_| ())
    }

    fn branch_delete(&self, name: &str, force: bool) -> Result<()> {
        let flag = if force { "-D" } else { "-d" };
        self.git.run(["branch", flag, name]).map(|_| ())
    }

    fn branch_rename(&self, old: &str, new: &str) -> Result<()> {
        self.git.run(["branch", "-m", old, new]).map(|_| ())
    }

    fn checkout(&self, target: &str) -> Result<()> {
        self.git.run(["checkout", target]).map(|_| ())
    }

    fn checkout_new(&self, name: &str, start: Option<&str>) -> Result<()> {
        let mut args = vec!["checkout", "-b", name];
        if let Some(s) = start {
            args.push(s);
        }
        self.git.run(args).map(|_| ())
    }

    fn merge(&self, target: &str, opts: &MergeOpts) -> Result<()> {
        let mut args: Vec<String> = vec!["merge".into()];
        if opts.no_ff {
            args.push("--no-ff".into());
        }
        if opts.squash {
            args.push("--squash".into());
        }
        if let Some(m) = &opts.message {
            args.push("-m".into());
            args.push(m.clone());
        }
        args.push(target.into());
        self.git.run(&args).map(|_| ())
    }

    fn rebase(&self, upstream: &str, opts: &RebaseOpts) -> Result<()> {
        if opts.interactive {
            return Err(GitError::Other(
                "interactive rebase is not supported by the non-interactive writer".into(),
            ));
        }
        let mut args: Vec<String> = vec!["rebase".into()];
        if let Some(onto) = &opts.onto {
            args.push("--onto".into());
            args.push(onto.clone());
        }
        args.push(upstream.into());
        self.git.run(&args).map(|_| ())
    }

    fn reset(&self, target: &str, mode: ResetMode) -> Result<()> {
        let flag = match mode {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        };
        self.git.run(["reset", flag, target]).map(|_| ())
    }

    fn revert(&self, oid: Oid) -> Result<()> {
        self.git
            .run(["revert", "--no-edit", &oid.to_hex()])
            .map(|_| ())
    }

    fn cherry_pick(&self, oid: Oid) -> Result<()> {
        self.git.run(["cherry-pick", &oid.to_hex()]).map(|_| ())
    }

    fn stash(&self, op: &StashOp) -> Result<()> {
        let stash_ref = |index: Option<usize>| match index {
            Some(i) => format!("stash@{{{i}}}"),
            None => "stash@{0}".to_string(),
        };
        match op {
            StashOp::Push {
                message,
                include_untracked,
            } => {
                let mut args: Vec<String> = vec!["stash".into(), "push".into()];
                if *include_untracked {
                    args.push("-u".into());
                }
                if let Some(m) = message {
                    args.push("-m".into());
                    args.push(m.clone());
                }
                self.git.run(&args).map(|_| ())
            }
            StashOp::Pop { index } => self
                .git
                .run(["stash", "pop", &stash_ref(*index)])
                .map(|_| ()),
            StashOp::Apply { index } => self
                .git
                .run(["stash", "apply", &stash_ref(*index)])
                .map(|_| ()),
            StashOp::Drop { index } => self
                .git
                .run(["stash", "drop", &stash_ref(*index)])
                .map(|_| ()),
        }
    }

    fn tag_create(&self, name: &str, target: Option<&str>, message: Option<&str>) -> Result<()> {
        let mut args: Vec<String> = vec!["tag".into()];
        if let Some(m) = message {
            args.push("-a".into());
            args.push("-m".into());
            args.push(m.into());
        }
        args.push(name.into());
        if let Some(t) = target {
            args.push(t.into());
        }
        self.git.run(&args).map(|_| ())
    }

    fn tag_delete(&self, name: &str) -> Result<()> {
        self.git.run(["tag", "-d", name]).map(|_| ())
    }

    fn remote_add(&self, name: &str, url: &str) -> Result<()> {
        self.git.run(["remote", "add", name, url]).map(|_| ())
    }

    fn remote_remove(&self, name: &str) -> Result<()> {
        self.git.run(["remote", "remove", name]).map(|_| ())
    }

    fn fetch(&self, remote: &str, sink: &mut dyn ProgressSink, cancel: &CancelToken) -> Result<()> {
        self.git.run_streaming(["fetch", remote], sink, cancel)
    }

    fn pull(&self, remote: &str, sink: &mut dyn ProgressSink, cancel: &CancelToken) -> Result<()> {
        self.git.run_streaming(["pull", remote], sink, cancel)
    }

    fn push(
        &self,
        remote: &str,
        refspec: &str,
        force: bool,
        sink: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<()> {
        let mut args: Vec<String> = vec!["push".into()];
        if force {
            // Safer than --force: refuses to clobber others' work.
            args.push("--force-with-lease".into());
        }
        args.push(remote.into());
        args.push(refspec.into());
        self.git.run_streaming(&args, sink, cancel)
    }
}
