//! gittify entry point.
//!
//! The eventual product selects a GPUI (default) or egui UI backend via feature
//! flag and runs the windowed app. Those backends live in excluded crates that
//! pull heavy graphics stacks; until they are wired in CI, the binary ships a
//! fully-working CLI that renders the commit graph through the exact same
//! layout engine the GUI will use — an end-to-end check of the core.

mod ascii;

use std::path::PathBuf;
use std::process::ExitCode;

use gg_git::{GitEngine, RepoReader, WalkOpts};

struct Args {
    path: PathBuf,
    limit: Option<usize>,
    first_parent: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut path = PathBuf::from(".");
    let mut limit = None;
    let mut first_parent = false;
    let mut saw_path = false;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--limit" | "-n" => {
                let v = it.next().ok_or_else(|| format!("{arg} requires a value"))?;
                limit = Some(
                    v.parse::<usize>()
                        .map_err(|_| format!("invalid count: {v}"))?,
                );
            }
            "--first-parent" => first_parent = true,
            "-h" | "--help" => return Err("help".into()),
            other if other.starts_with('-') => {
                return Err(format!("unknown flag: {other}"));
            }
            other => {
                path = PathBuf::from(other);
                saw_path = true;
            }
        }
    }
    let _ = saw_path;
    Ok(Args {
        path,
        limit,
        first_parent,
    })
}

fn usage() {
    eprintln!(
        "gittify — high-performance git GUI (CLI graph preview)\n\n\
         USAGE:\n    gittify [PATH] [--limit N] [--first-parent]\n\n\
         ARGS:\n    PATH                 repository path (default: current dir)\n\n\
         OPTIONS:\n    -n, --limit N        show at most N commits\n    \
         --first-parent       follow only first parents\n    -h, --help           show this help"
    );
}

fn run() -> Result<(), String> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) if e == "help" => {
            usage();
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    let engine = GitEngine::discover(&args.path).map_err(|e| e.to_string())?;

    let refs = engine.reader().refs().map_err(|e| e.to_string())?;
    let labels = ascii::ref_labels(&refs);

    let opts = WalkOpts {
        tips: Vec::new(),
        limit: args.limit,
        first_parent: args.first_parent,
    };
    let view = engine.history_graph(&opts).map_err(|e| e.to_string())?;

    if view.commits.is_empty() {
        println!("(no commits)");
        return Ok(());
    }

    print!("{}", ascii::render(&view, &labels));
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gittify: {e}");
            ExitCode::FAILURE
        }
    }
}
