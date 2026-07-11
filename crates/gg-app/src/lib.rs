//! `gg-app` — application orchestration. Owns the message-passing architecture
//! the spec prescribes: the UI thread sends [`Command`]s to a background worker
//! that owns the [`GitEngine`]; the worker streams [`Event`]s back over a
//! channel. Heavy gix reads and git subprocess calls thus never block the UI.
//!
//! This crate is toolkit-agnostic. A GPUI or egui front-end drives it by
//! sending commands and draining events each frame (applying them with
//! `cx.notify()` / `request_repaint()`); the worker uses a plain `std::thread`
//! plus channels, which a GPUI backend can later swap for its `BackgroundExecutor`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender};

use gg_core::{FileDiff, Oid, RefRecord, Remote, Result, StashEntry, StatusSnapshot};
use gg_git::{
    CancelToken, CommitOpts, GitEngine, GitWriter, HistoryView, MergeOpts, NullSink, Progress,
    ProgressSink, RebaseOpts, RepoReader, RepoWriter, ResetMode, StashOp, WalkOpts,
};

mod state;
pub use state::{AppState, CommitDiffView, DiffView};

/// A callback the worker invokes after queueing each event, so an immediate-mode
/// UI can be woken (e.g. `egui::Context::request_repaint`) instead of polling.
pub type EventWaker = Arc<dyn Fn() + Send + Sync>;

/// The worker's event channel plus the optional UI waker, so every event both
/// queues and wakes the front-end.
#[derive(Clone)]
struct EventTx {
    tx: Sender<Event>,
    waker: Option<EventWaker>,
}

impl EventTx {
    fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        if let Some(waker) = &self.waker {
            waker();
        }
    }
}

/// A [`ProgressSink`] that forwards both parsed progress (for the bar) and every
/// raw output line (for the details transcript) to the UI as events.
struct NetSink {
    events: EventTx,
    label: String,
}

impl ProgressSink for NetSink {
    fn report(&mut self, progress: Progress) {
        self.events.send(Event::Progress {
            label: self.label.clone(),
            progress,
        });
    }

    fn line(&mut self, line: &str) {
        self.events.send(Event::OpOutput {
            label: self.label.clone(),
            line: line.to_string(),
        });
    }
}

/// Default commit cap for history walks until the UI asks for a specific limit.
const DEFAULT_HISTORY_LIMIT: usize = 2000;

/// Borrow owned paths as the `&[&Path]` the writer expects.
fn path_refs(paths: &[PathBuf]) -> Vec<&Path> {
    paths.iter().map(PathBuf::as_path).collect()
}

/// A command sent from the UI to the background worker.
pub enum Command {
    /// Load (and lay out) history from the given tips/limit.
    LoadHistory(WalkOpts),
    /// Re-read the reference list.
    LoadRefs,
    /// Re-read the working-tree status.
    LoadStatus,
    /// Compute the diff for one path (staged or unstaged; untracked => all-add).
    LoadDiff {
        /// Repo-relative path.
        path: PathBuf,
        /// Diff the index vs HEAD (true) or working tree vs index (false).
        staged: bool,
        /// The path is untracked.
        untracked: bool,
    },
    /// Compute the per-file diffs introduced by a commit, for previewing the
    /// files changed in a historical commit.
    LoadCommitDiff(Oid),
    /// Stage paths.
    Stage(Vec<PathBuf>),
    /// Unstage paths.
    Unstage(Vec<PathBuf>),
    /// Stage every change.
    StageAll,
    /// Unstage everything.
    UnstageAll,
    /// Apply a single-hunk patch to the index (`git apply --cached`). Forward
    /// (`reverse = false`) stages the hunk; `reverse = true` unstages it. The
    /// UI builds `patch` from the previewed diff via
    /// [`gg_git`]'s raw diff + `gg_diff::single_hunk_patch`.
    ApplyHunk {
        /// The one-hunk unified-diff patch text.
        patch: String,
        /// Reverse-apply (unstage) instead of forward-apply (stage).
        reverse: bool,
    },
    /// Discard working-tree changes to paths (deletes untracked files).
    Discard {
        /// Paths to discard.
        paths: Vec<PathBuf>,
        /// The paths are untracked (delete instead of restore).
        untracked: bool,
    },
    /// Check out a branch/ref/commit.
    Checkout(String),
    /// Create a commit from the staged index.
    Commit {
        /// Commit message.
        message: String,
        /// Commit options.
        opts: CommitOpts,
    },
    /// Commit the staged index, then push the current branch.
    CommitAndPush {
        /// Commit message (title and optional body).
        message: String,
    },
    /// Create a branch.
    CreateBranch {
        /// New branch name.
        name: String,
        /// Start point (or HEAD if `None`).
        start: Option<String>,
        /// Check the new branch out immediately (`git checkout -b`).
        checkout: bool,
    },
    /// Delete a branch (`force` uses `-D` to drop unmerged work).
    DeleteBranch {
        /// Branch to delete.
        name: String,
        /// Force-delete even if not merged.
        force: bool,
    },
    /// Rename a branch.
    RenameBranch {
        /// Existing branch name.
        old: String,
        /// New branch name.
        new: String,
    },
    /// Cherry-pick a commit onto the current branch.
    CherryPick(Oid),
    /// Revert a commit, creating a new commit that undoes it.
    Revert(Oid),
    /// Reset the current branch to a target commit/ref.
    Reset {
        /// Target commit/ref.
        target: String,
        /// Soft (HEAD only), mixed (HEAD + index), or hard (HEAD + index + tree).
        mode: ResetMode,
    },
    /// Stash the working tree (and optionally untracked files).
    Stash {
        /// Also stash untracked files (`-u`).
        include_untracked: bool,
    },
    /// Fetch from a remote.
    Fetch(String),
    /// Pull from a remote.
    Pull(String),
    /// Push a refspec to a remote.
    Push {
        /// Remote name.
        remote: String,
        /// Refspec.
        refspec: String,
        /// Force (with lease).
        force: bool,
    },
    /// Push the current branch using git's configured upstream/push rules.
    PushCurrent {
        /// Force (with lease).
        force: bool,
    },
    /// Re-read the stash list.
    LoadStashes,
    /// Apply a stash entry without dropping it.
    StashApply(usize),
    /// Apply a stash entry and drop it.
    StashPop(usize),
    /// Drop a stash entry.
    StashDrop(usize),
    /// Re-read the configured remotes.
    LoadRemotes,
    /// Add a remote.
    AddRemote {
        /// Remote name.
        name: String,
        /// Remote URL.
        url: String,
    },
    /// Remove a remote.
    RemoveRemote(String),
    /// Create a tag (annotated when `message` is `Some`).
    CreateTag {
        /// Tag name.
        name: String,
        /// Target commit/ref, or HEAD when `None`.
        target: Option<String>,
        /// Annotation message (lightweight tag when `None`).
        message: Option<String>,
    },
    /// Delete a tag.
    DeleteTag(String),
    /// Merge a branch/commit into the current branch.
    Merge {
        /// Target branch/commit to merge in.
        target: String,
    },
    /// Rebase the current branch onto a branch/commit.
    Rebase {
        /// Upstream branch/commit to rebase onto.
        upstream: String,
    },
    /// Route this repository's credential prompts through an askpass helper,
    /// answering with the given credentials (session-scoped: they apply to
    /// every subsequent network operation on this worker).
    SetCredentials {
        /// Path to the askpass helper binary (e.g. `gg-askpass`).
        askpass: PathBuf,
        /// The credentials the helper may answer with.
        creds: gg_git::Credentials,
    },
    /// Stop the worker thread.
    Shutdown,
}

/// An event sent from the worker back to the UI.
pub enum Event {
    /// History finished loading.
    History(Arc<HistoryView>),
    /// Reference list refreshed.
    Refs(Vec<RefRecord>),
    /// Working-tree status refreshed.
    Status(StatusSnapshot),
    /// A file diff finished computing.
    Diff {
        /// The path diffed.
        path: PathBuf,
        /// Whether this was the staged diff.
        staged: bool,
        /// The computed diff.
        diff: FileDiff,
        /// Raw unified diff text git produced (empty for untracked), used to
        /// build single-hunk patches for hunk-level staging.
        raw: String,
    },
    /// A commit's per-file diffs finished computing.
    CommitDiff {
        /// The commit inspected.
        oid: Oid,
        /// One entry per file the commit changed.
        files: Vec<FileDiff>,
    },
    /// The stash list refreshed.
    Stashes(Vec<StashEntry>),
    /// The configured remotes refreshed.
    Remotes(Vec<Remote>),
    /// Progress update for a long/network operation.
    Progress {
        /// Operation label (e.g. "push").
        label: String,
        /// The progress payload.
        progress: Progress,
    },
    /// One raw output line from a streaming operation (git's stdout/stderr), for
    /// the operation-details transcript.
    OpOutput {
        /// Operation label (e.g. "pull").
        label: String,
        /// A single line of git output.
        line: String,
    },
    /// An operation completed successfully.
    Completed(String),
    /// An operation failed.
    Failed {
        /// Operation label.
        label: String,
        /// Human-readable error.
        error: String,
    },
}

/// Handle the UI keeps: send commands, drain events, request cancellation.
pub struct AppHandle {
    commands: Sender<Command>,
    events: Receiver<Event>,
    cancel: CancelToken,
    worker: Option<JoinHandle<()>>,
}

impl AppHandle {
    /// Spawn the background worker for an already-opened engine.
    pub fn spawn(engine: GitEngine) -> Self {
        Self::spawn_with_waker(engine, None)
    }

    /// Spawn the background worker, waking the UI via `waker` after each event.
    pub fn spawn_with_waker(engine: GitEngine, waker: Option<EventWaker>) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<Command>();
        let (evt_tx, evt_rx) = crossbeam_channel::unbounded::<Event>();
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let events = EventTx { tx: evt_tx, waker };

        let worker = std::thread::Builder::new()
            .name("gg-git-worker".into())
            .spawn(move || run_worker(engine, cmd_rx, events, worker_cancel))
            .expect("spawn worker thread");

        Self {
            commands: cmd_tx,
            events: evt_rx,
            cancel,
            worker: Some(worker),
        }
    }

    /// Discover a repository and spawn the worker for it.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let engine = GitEngine::discover(path.into())?;
        Ok(Self::spawn(engine))
    }

    /// Discover a repository and spawn the worker with a UI waker.
    pub fn open_with_waker(path: impl Into<PathBuf>, waker: EventWaker) -> Result<Self> {
        let engine = GitEngine::discover(path.into())?;
        Ok(Self::spawn_with_waker(engine, Some(waker)))
    }

    /// Queue a command for the worker.
    pub fn send(&self, command: Command) {
        // If the worker is gone the UI is shutting down; dropping is fine.
        let _ = self.commands.send(command);
    }

    /// Non-blocking drain of all currently-available events.
    pub fn poll_events(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    /// Blocking receive of the next event (for non-UI / test drivers).
    pub fn recv_event(&self) -> Option<Event> {
        self.events.recv().ok()
    }

    /// Signal cancellation of the in-flight network operation.
    pub fn request_cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for AppHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn run_worker(
    mut engine: GitEngine,
    commands: Receiver<Command>,
    events: EventTx,
    cancel: CancelToken,
) {
    // The most recent history request; auto-refreshes after mutations reuse it
    // so a paged-in view doesn't snap back to the default window.
    let mut hist_opts = WalkOpts {
        tips: Vec::new(),
        limit: Some(DEFAULT_HISTORY_LIMIT),
        first_parent: false,
    };
    while let Ok(cmd) = commands.recv() {
        match cmd {
            Command::Shutdown => break,
            Command::SetCredentials { askpass, creds } => {
                engine = engine.with_credentials(&askpass, &creds);
            }
            other => handle(&engine, other, &events, &cancel, &mut hist_opts),
        }
    }
}

fn handle(
    engine: &GitEngine,
    cmd: Command,
    events: &EventTx,
    cancel: &CancelToken,
    hist_opts: &mut WalkOpts,
) {
    let send = |e: Event| {
        events.send(e);
    };
    match cmd {
        // Both intercepted in `run_worker` (they mutate the loop, not the repo).
        Command::Shutdown | Command::SetCredentials { .. } => {}
        Command::LoadHistory(opts) => {
            *hist_opts = opts;
            match engine.history_graph(hist_opts) {
                Ok(view) => send(Event::History(Arc::new(view))),
                Err(e) => send(Event::Failed {
                    label: "load-history".into(),
                    error: e.to_string(),
                }),
            }
        }
        Command::LoadRefs => match engine.reader().refs() {
            Ok(refs) => send(Event::Refs(refs)),
            Err(e) => send(Event::Failed {
                label: "load-refs".into(),
                error: e.to_string(),
            }),
        },
        Command::LoadStatus => refresh_status(engine, &send),
        Command::LoadDiff {
            path,
            staged,
            untracked,
        } => match engine.diff_file_with_raw(&path, staged, untracked) {
            Ok((diff, raw)) => send(Event::Diff {
                path,
                staged,
                diff,
                raw,
            }),
            Err(e) => send(Event::Failed {
                label: "load-diff".into(),
                error: e.to_string(),
            }),
        },
        Command::ApplyHunk { patch, reverse } => {
            after_mutation(
                engine,
                &send,
                "apply-hunk",
                engine.writer().apply_to_index(&patch, reverse),
            );
        }
        Command::LoadCommitDiff(oid) => match engine.commit_diff(oid) {
            Ok(files) => send(Event::CommitDiff { oid, files }),
            Err(e) => send(Event::Failed {
                label: "load-commit-diff".into(),
                error: e.to_string(),
            }),
        },
        Command::Stage(paths) => {
            after_mutation(
                engine,
                &send,
                "stage",
                engine.writer().stage(&path_refs(&paths)),
            );
        }
        Command::Unstage(paths) => {
            after_mutation(
                engine,
                &send,
                "unstage",
                engine.writer().unstage(&path_refs(&paths)),
            );
        }
        Command::StageAll => {
            after_mutation(engine, &send, "stage-all", engine.writer().stage_all());
        }
        Command::UnstageAll => {
            after_mutation(engine, &send, "unstage-all", engine.writer().unstage_all());
        }
        Command::Discard { paths, untracked } => {
            let refs = path_refs(&paths);
            let result = if untracked {
                engine.writer().clean(&refs)
            } else {
                engine.writer().discard(&refs)
            };
            after_mutation(engine, &send, "discard", result);
        }
        Command::Checkout(target) => match engine.writer().checkout(&target) {
            Ok(()) => {
                send(Event::Completed("checkout".into()));
                refresh_refs(engine, &send);
                refresh_status(engine, &send);
                refresh_history(engine, &send, hist_opts);
            }
            Err(e) => send(Event::Failed {
                label: "checkout".into(),
                error: e.to_string(),
            }),
        },
        Command::Commit { message, opts } => match engine.writer().commit(&message, &opts) {
            Ok(_) => {
                send(Event::Completed("commit".into()));
                refresh_status(engine, &send);
                refresh_history(engine, &send, hist_opts);
            }
            Err(e) => send(Event::Failed {
                label: "commit".into(),
                error: e.to_string(),
            }),
        },
        Command::CommitAndPush { message } => {
            match engine.writer().commit(&message, &CommitOpts::default()) {
                Ok(_) => {
                    send(Event::Completed("commit".into()));
                    run_network(engine, events, cancel, "push", |w, sink, c| {
                        w.push_current(false, sink, c)
                    });
                    refresh_status(engine, &send);
                    refresh_history(engine, &send, hist_opts);
                }
                Err(e) => send(Event::Failed {
                    label: "commit".into(),
                    error: e.to_string(),
                }),
            }
        }
        Command::CreateBranch {
            name,
            start,
            checkout,
        } => {
            let result = if checkout {
                engine.writer().checkout_new(&name, start.as_deref())
            } else {
                engine.writer().branch_create(&name, start.as_deref())
            };
            match result {
                Ok(()) => {
                    send(Event::Completed("create-branch".into()));
                    refresh_refs(engine, &send);
                    // A checkout moves HEAD, so the status and graph change too.
                    if checkout {
                        refresh_status(engine, &send);
                        refresh_history(engine, &send, hist_opts);
                    }
                }
                Err(e) => send(Event::Failed {
                    label: "create-branch".into(),
                    error: e.to_string(),
                }),
            }
        }
        Command::DeleteBranch { name, force } => {
            match engine.writer().branch_delete(&name, force) {
                Ok(()) => {
                    send(Event::Completed("delete-branch".into()));
                    refresh_refs(engine, &send);
                }
                Err(e) => send(Event::Failed {
                    label: "delete-branch".into(),
                    error: e.to_string(),
                }),
            }
        }
        Command::RenameBranch { old, new } => match engine.writer().branch_rename(&old, &new) {
            Ok(()) => {
                send(Event::Completed("rename-branch".into()));
                refresh_refs(engine, &send);
                // Renaming the current branch changes the status branch name.
                refresh_status(engine, &send);
            }
            Err(e) => send(Event::Failed {
                label: "rename-branch".into(),
                error: e.to_string(),
            }),
        },
        Command::CherryPick(oid) => match engine.writer().cherry_pick(oid) {
            Ok(()) => {
                send(Event::Completed("cherry-pick".into()));
                refresh_all(engine, &send, hist_opts);
            }
            Err(e) => send(Event::Failed {
                label: "cherry-pick".into(),
                error: e.to_string(),
            }),
        },
        Command::Revert(oid) => match engine.writer().revert(oid) {
            Ok(()) => {
                send(Event::Completed("revert".into()));
                refresh_all(engine, &send, hist_opts);
            }
            Err(e) => send(Event::Failed {
                label: "revert".into(),
                error: e.to_string(),
            }),
        },
        Command::Reset { target, mode } => match engine.writer().reset(&target, mode) {
            Ok(()) => {
                send(Event::Completed("reset".into()));
                refresh_all(engine, &send, hist_opts);
            }
            Err(e) => send(Event::Failed {
                label: "reset".into(),
                error: e.to_string(),
            }),
        },
        Command::Stash { include_untracked } => {
            let op = StashOp::Push {
                message: None,
                include_untracked,
            };
            match engine.writer().stash(&op) {
                Ok(()) => {
                    send(Event::Completed("stash".into()));
                    refresh_all(engine, &send, hist_opts);
                    refresh_stashes(engine, &send);
                }
                Err(e) => send(Event::Failed {
                    label: "stash".into(),
                    error: e.to_string(),
                }),
            }
        }
        Command::Fetch(remote) => {
            run_network(engine, events, cancel, "fetch", |w, sink, c| {
                w.fetch(&remote, sink, c)
            });
        }
        Command::Pull(remote) => {
            run_network(engine, events, cancel, "pull", |w, sink, c| {
                w.pull(&remote, sink, c)
            });
        }
        Command::Push {
            remote,
            refspec,
            force,
        } => {
            run_network(engine, events, cancel, "push", |w, sink, c| {
                w.push(&remote, &refspec, force, sink, c)
            });
        }
        Command::PushCurrent { force } => {
            run_network(engine, events, cancel, "push", |w, sink, c| {
                w.push_current(force, sink, c)
            });
        }
        Command::LoadStashes => refresh_stashes(engine, &send),
        Command::StashApply(index) => stash_op(
            engine,
            &send,
            "stash-apply",
            StashOp::Apply { index: Some(index) },
            hist_opts,
        ),
        Command::StashPop(index) => stash_op(
            engine,
            &send,
            "stash-pop",
            StashOp::Pop { index: Some(index) },
            hist_opts,
        ),
        Command::StashDrop(index) => stash_op(
            engine,
            &send,
            "stash-drop",
            StashOp::Drop { index: Some(index) },
            hist_opts,
        ),
        Command::LoadRemotes => refresh_remotes(engine, &send),
        Command::AddRemote { name, url } => match engine.writer().remote_add(&name, &url) {
            Ok(()) => {
                send(Event::Completed("add-remote".into()));
                refresh_remotes(engine, &send);
            }
            Err(e) => send(Event::Failed {
                label: "add-remote".into(),
                error: e.to_string(),
            }),
        },
        Command::RemoveRemote(name) => match engine.writer().remote_remove(&name) {
            Ok(()) => {
                send(Event::Completed("remove-remote".into()));
                refresh_remotes(engine, &send);
            }
            Err(e) => send(Event::Failed {
                label: "remove-remote".into(),
                error: e.to_string(),
            }),
        },
        Command::CreateTag {
            name,
            target,
            message,
        } => match engine
            .writer()
            .tag_create(&name, target.as_deref(), message.as_deref())
        {
            Ok(()) => {
                send(Event::Completed("create-tag".into()));
                refresh_refs(engine, &send);
            }
            Err(e) => send(Event::Failed {
                label: "create-tag".into(),
                error: e.to_string(),
            }),
        },
        Command::DeleteTag(name) => match engine.writer().tag_delete(&name) {
            Ok(()) => {
                send(Event::Completed("delete-tag".into()));
                refresh_refs(engine, &send);
            }
            Err(e) => send(Event::Failed {
                label: "delete-tag".into(),
                error: e.to_string(),
            }),
        },
        Command::Merge { target } => match engine.writer().merge(&target, &MergeOpts::default()) {
            Ok(()) => {
                send(Event::Completed("merge".into()));
                refresh_all(engine, &send, hist_opts);
            }
            Err(e) => send(Event::Failed {
                label: "merge".into(),
                error: e.to_string(),
            }),
        },
        Command::Rebase { upstream } => {
            match engine.writer().rebase(&upstream, &RebaseOpts::default()) {
                Ok(()) => {
                    send(Event::Completed("rebase".into()));
                    refresh_all(engine, &send, hist_opts);
                }
                Err(e) => send(Event::Failed {
                    label: "rebase".into(),
                    error: e.to_string(),
                }),
            }
        }
    }
}

fn report_unit(send: &impl Fn(Event), label: &str, result: Result<()>) {
    match result {
        Ok(()) => send(Event::Completed(label.to_string())),
        Err(e) => send(Event::Failed {
            label: label.to_string(),
            error: e.to_string(),
        }),
    }
}

/// After a working-tree mutation, re-read status (so the UI lists update) or
/// surface the error.
fn after_mutation(engine: &GitEngine, send: &impl Fn(Event), label: &str, result: Result<()>) {
    match result {
        Ok(()) => refresh_status(engine, send),
        Err(e) => send(Event::Failed {
            label: label.to_string(),
            error: e.to_string(),
        }),
    }
}

fn refresh_status(engine: &GitEngine, send: &impl Fn(Event)) {
    match engine.status() {
        Ok(status) => send(Event::Status(status)),
        Err(e) => send(Event::Failed {
            label: "status".into(),
            error: e.to_string(),
        }),
    }
}

/// Re-read the reference list (after an op that changes branches or HEAD) so
/// the branch menu and ref pills reflect the new state.
fn refresh_refs(engine: &GitEngine, send: &impl Fn(Event)) {
    match engine.reader().refs() {
        Ok(refs) => send(Event::Refs(refs)),
        Err(e) => send(Event::Failed {
            label: "load-refs".into(),
            error: e.to_string(),
        }),
    }
}

/// Re-read refs, status, and history together (after an op that can change all
/// three, e.g. checkout, reset, revert, cherry-pick, stash).
fn refresh_all(engine: &GitEngine, send: &impl Fn(Event), hist_opts: &WalkOpts) {
    refresh_refs(engine, send);
    refresh_status(engine, send);
    refresh_history(engine, send, hist_opts);
}

/// Re-read the stash list.
fn refresh_stashes(engine: &GitEngine, send: &impl Fn(Event)) {
    match engine.writer().stash_list() {
        Ok(stashes) => send(Event::Stashes(stashes)),
        Err(e) => send(Event::Failed {
            label: "stash-list".into(),
            error: e.to_string(),
        }),
    }
}

/// Re-read the configured remotes.
fn refresh_remotes(engine: &GitEngine, send: &impl Fn(Event)) {
    match engine.writer().remotes() {
        Ok(remotes) => send(Event::Remotes(remotes)),
        Err(e) => send(Event::Failed {
            label: "remotes".into(),
            error: e.to_string(),
        }),
    }
}

/// Run a stash apply/pop/drop, then refresh the working tree and stash list.
fn stash_op(
    engine: &GitEngine,
    send: &impl Fn(Event),
    label: &str,
    op: StashOp,
    hist_opts: &WalkOpts,
) {
    match engine.writer().stash(&op) {
        Ok(()) => {
            send(Event::Completed(label.to_string()));
            refresh_all(engine, send, hist_opts);
            refresh_stashes(engine, send);
        }
        Err(e) => send(Event::Failed {
            label: label.to_string(),
            error: e.to_string(),
        }),
    }
}

fn refresh_history(engine: &GitEngine, send: &impl Fn(Event), opts: &WalkOpts) {
    match engine.history_graph(opts) {
        Ok(view) => send(Event::History(Arc::new(view))),
        Err(e) => send(Event::Failed {
            label: "load-history".into(),
            error: e.to_string(),
        }),
    }
}

/// Run a network op, forwarding progress as events and honoring cancellation.
fn run_network(
    engine: &GitEngine,
    events: &EventTx,
    cancel: &CancelToken,
    label: &str,
    op: impl FnOnce(&GitWriter, &mut dyn ProgressSink, &CancelToken) -> Result<()>,
) {
    cancel.reset();
    let mut sink = NetSink {
        events: events.clone(),
        label: label.to_string(),
    };
    let result = op(engine.writer(), &mut sink, cancel);
    // Drop the sink before reporting completion so no progress trails the result.
    drop(sink);
    let send = |e: Event| {
        events.send(e);
    };
    report_unit(&send, label, result);
}

// Keep `NullSink` referenced so it remains part of the public surface for
// callers that want a non-forwarding sink.
#[allow(dead_code)]
fn _null_sink() -> NullSink {
    NullSink
}
