//! Locating and invoking the system `git` binary with a hardened, predictable
//! environment.
//!
//! Every invocation forces `LC_ALL=C` and `core.quotepath=false` so output is
//! parseable across locales, and `GIT_TERMINAL_PROMPT=0` so the process fails
//! fast instead of blocking on an invisible terminal prompt. Credentials are
//! routed through the askpass helper (see `gg-credentials`).

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use gg_core::{GitError, Result};

use crate::cancel::CancelToken;
use crate::progress::{parse_progress_line, ProgressSink};

/// A handle to the system git binary, bound to a working directory.
#[derive(Clone, Debug)]
pub struct Git {
    binary: PathBuf,
    workdir: PathBuf,
    askpass: Option<PathBuf>,
    extra_env: Vec<(String, String)>,
}

/// Locate the `git` executable: PATH first, then well-known per-platform
/// install locations.
pub fn find_git() -> Result<PathBuf> {
    if let Ok(p) = which::which("git") {
        return Ok(p);
    }
    let candidates: &[&str] = if cfg!(windows) {
        &[
            r"C:\Program Files\Git\cmd\git.exe",
            r"C:\Program Files (x86)\Git\cmd\git.exe",
        ]
    } else {
        &[
            "/usr/bin/git",
            "/usr/local/bin/git",
            "/opt/homebrew/bin/git",
        ]
    };
    for c in candidates {
        let p = Path::new(c);
        if p.exists() {
            return Ok(p.to_path_buf());
        }
    }
    Err(GitError::GitBinaryMissing)
}

impl Git {
    /// Bind to an explicit working directory (must already be a working tree).
    pub fn open(workdir: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            binary: find_git()?,
            workdir: workdir.into(),
            askpass: None,
            extra_env: Vec::new(),
        })
    }

    /// Discover the repository containing `start` (walks up to the work-tree
    /// root via `git rev-parse --show-toplevel`).
    pub fn discover(start: impl AsRef<Path>) -> Result<Self> {
        let binary = find_git()?;
        let out = Command::new(&binary)
            .arg("rev-parse")
            .arg("--show-toplevel")
            .current_dir(start.as_ref())
            .env("LC_ALL", "C")
            .output()
            .map_err(|e| GitError::Other(format!("failed to spawn git: {e}")))?;
        if !out.status.success() {
            return Err(GitError::NotARepository(
                start.as_ref().display().to_string(),
            ));
        }
        let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(Self {
            binary,
            workdir: PathBuf::from(top),
            askpass: None,
            extra_env: Vec::new(),
        })
    }

    /// The working-tree root this handle operates on.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Route credential prompts through the given askpass helper binary.
    pub fn with_askpass(mut self, helper: impl Into<PathBuf>) -> Self {
        self.askpass = Some(helper.into());
        self
    }

    /// Add an environment variable to every invocation (e.g. a credential
    /// channel for the askpass helper).
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    fn base_command(&self) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.workdir)
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            // Disable any pager and color so captured output is clean.
            .env("GIT_PAGER", "cat")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(["-c", "core.quotepath=false"])
            .args(["-c", "color.ui=false"]);
        if let Some(askpass) = &self.askpass {
            cmd.env("GIT_ASKPASS", askpass);
            cmd.env("SSH_ASKPASS", askpass);
            cmd.env("SSH_ASKPASS_REQUIRE", "force");
        }
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        cmd
    }

    /// Run git to completion, capturing stdout. Errors carry the exit code and
    /// (locale-normalized) stderr.
    pub fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self
            .base_command()
            .args(args)
            .output()
            .map_err(|e| GitError::Other(format!("failed to spawn git: {e}")))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(GitError::CommandFailed {
                code: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }

    /// Run git, feeding `stdin_data` to its standard input (used for
    /// `git apply --cached -` hunk staging). Returns captured stdout.
    pub fn run_with_stdin<I, S>(&self, args: I, stdin_data: &[u8]) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        use std::io::Write;

        let mut child = self
            .base_command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| GitError::Other(format!("failed to spawn git: {e}")))?;
        child
            .stdin
            .take()
            .expect("stdin piped")
            .write_all(stdin_data)
            .map_err(|e| GitError::Other(format!("failed writing git stdin: {e}")))?;
        let out = child
            .wait_with_output()
            .map_err(|e| GitError::Other(format!("git wait failed: {e}")))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(GitError::CommandFailed {
                code: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }

    /// Run a network operation with `--progress`, streaming parsed updates to
    /// `sink` and honoring `cancel` (which kills the child process).
    pub fn run_streaming<I, S>(
        &self,
        args: I,
        sink: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut child = self
            .base_command()
            .args(args)
            .arg("--progress")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| GitError::Other(format!("failed to spawn git: {e}")))?;

        // git writes progress + most messages to stderr (carriage-return chunks)
        // and the final summary (merge result, diffstat, "Already up to date") to
        // stdout. Read stdout on a side thread so it can't fill its pipe and
        // block, and forward its lines through a channel.
        let stdout = child.stdout.take();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let stdout_thread = std::thread::spawn(move || {
            if let Some(out) = stdout {
                for line in BufReader::new(out)
                    .lines()
                    .map_while(std::result::Result::ok)
                {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        });

        let stderr = child.stderr.take().expect("stderr piped");
        let mut reader = BufReader::new(stderr);
        let mut chunk: Vec<u8> = Vec::with_capacity(256);
        // Keep a rolling tail of recent text for error reporting.
        let mut tail = String::new();
        let mut byte = [0u8; 1];

        loop {
            if cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitError::Cancelled);
            }
            match reader.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    let b = byte[0];
                    if b == b'\r' || b == b'\n' {
                        if !chunk.is_empty() {
                            let line = String::from_utf8_lossy(&chunk).into_owned();
                            // Recognized progress meters drive the bar; every
                            // other line is forwarded to the details transcript
                            // (so it doesn't fill with per-percent ticks).
                            if let Some(p) = parse_progress_line(&line) {
                                sink.report(p);
                            } else {
                                sink.line(&line);
                            }
                            push_tail(&mut tail, &line);
                            chunk.clear();
                        }
                        // Opportunistically flush any stdout lines that arrived.
                        while let Ok(line) = rx.try_recv() {
                            sink.line(&line);
                            push_tail(&mut tail, &line);
                        }
                    } else {
                        chunk.push(b);
                    }
                }
                Err(e) => return Err(GitError::Other(format!("reading git stderr: {e}"))),
            }
        }
        if !chunk.is_empty() {
            let line = String::from_utf8_lossy(&chunk).into_owned();
            sink.line(&line);
            push_tail(&mut tail, &line);
        }
        // Drain remaining stdout (blocks until the stdout thread closes its end).
        while let Ok(line) = rx.recv() {
            sink.line(&line);
            push_tail(&mut tail, &line);
        }
        let _ = stdout_thread.join();

        let status = child
            .wait()
            .map_err(|e| GitError::Other(format!("git wait failed: {e}")))?;
        if status.success() {
            Ok(())
        } else if tail.contains("could not read Username")
            || tail.contains("Authentication failed")
            || tail.contains("terminal prompts disabled")
        {
            Err(GitError::AuthRequired(tail.trim().to_string()))
        } else {
            Err(GitError::CommandFailed {
                code: status.code().unwrap_or(-1),
                stderr: tail.trim().to_string(),
            })
        }
    }
}

fn push_tail(tail: &mut String, line: &str) {
    tail.push_str(line);
    tail.push('\n');
    // Bound memory: keep only the last ~4 KiB.
    if tail.len() > 4096 {
        let cut = tail.len() - 4096;
        *tail = tail[cut..].to_string();
    }
}
