//! `gg-askpass` — the helper binary git/ssh invoke for credentials.
//!
//! git calls this as `gg-askpass "<prompt>"` and reads the answer from stdout.
//! We classify the prompt and echo the matching value the app placed in the
//! environment. If we have no answer we exit non-zero so git fails fast (it is
//! run with `GIT_TERMINAL_PROMPT=0`, so it will not fall back to a tty prompt).

use std::process::ExitCode;

fn main() -> ExitCode {
    let prompt = std::env::args().nth(1).unwrap_or_default();
    match gg_credentials::answer_from_env(&prompt) {
        Some(answer) => {
            println!("{answer}");
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("gg-askpass: no credential available for prompt: {prompt}");
            ExitCode::FAILURE
        }
    }
}
