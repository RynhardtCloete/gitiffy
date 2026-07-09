//! `gg-credentials` — the plumbing that lets a GUI answer git's credential
//! prompts. When git (or ssh) needs a username/password/passphrase and there is
//! no terminal, it invokes the program named by `GIT_ASKPASS` / `SSH_ASKPASS`
//! with the human-readable prompt as its first argument and reads the answer
//! from stdout.
//!
//! This crate holds the logic shared by the app (which configures the helper)
//! and the `gg-askpass` helper binary (which answers): classifying the prompt
//! text and resolving the value from the environment channel the app sets up.
//!
//! The baseline channel passes the username and a token/password through
//! environment variables on the spawned git process. A richer GUI flow can
//! layer an IPC prompt on top, but the classification logic is identical.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Env var the helper reads to answer a "Username" prompt.
pub const ENV_USERNAME: &str = "GG_ASKPASS_USERNAME";
/// Env var the helper reads to answer a "Password" prompt.
pub const ENV_PASSWORD: &str = "GG_ASKPASS_PASSWORD";
/// Env var the helper reads to answer an SSH key passphrase prompt.
pub const ENV_PASSPHRASE: &str = "GG_ASKPASS_PASSPHRASE";

/// The category of credential git/ssh is asking for, inferred from the prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// A username (e.g. "Username for 'https://github.com':").
    Username,
    /// A password or token (e.g. "Password for 'https://x@github.com':").
    Password,
    /// An SSH key passphrase (e.g. "Enter passphrase for key '/.../id_ed25519':").
    Passphrase,
    /// An unrecognized prompt.
    Unknown,
}

/// Classify a git/ssh askpass prompt string.
pub fn classify_prompt(prompt: &str) -> PromptKind {
    let p = prompt.to_ascii_lowercase();
    if p.contains("passphrase") {
        PromptKind::Passphrase
    } else if p.contains("username") || p.contains("login") {
        PromptKind::Username
    } else if p.contains("password") || p.contains("token") {
        PromptKind::Password
    } else {
        PromptKind::Unknown
    }
}

/// The credentials the app makes available to the helper for one git operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    /// Username, if known.
    pub username: Option<String>,
    /// Password or personal-access token, if known.
    pub password: Option<String>,
    /// SSH key passphrase, if known.
    pub passphrase: Option<String>,
}

impl Credentials {
    /// The environment pairs to set on the spawned git process so the helper
    /// can answer prompts. Only set keys for values we actually have.
    pub fn to_env(&self) -> Vec<(&'static str, String)> {
        let mut env = Vec::new();
        if let Some(u) = &self.username {
            env.push((ENV_USERNAME, u.clone()));
        }
        if let Some(p) = &self.password {
            env.push((ENV_PASSWORD, p.clone()));
        }
        if let Some(p) = &self.passphrase {
            env.push((ENV_PASSPHRASE, p.clone()));
        }
        env
    }

    /// Resolve the answer to a prompt from these credentials.
    pub fn answer(&self, prompt: &str) -> Option<String> {
        match classify_prompt(prompt) {
            PromptKind::Username => self.username.clone(),
            PromptKind::Password => self.password.clone(),
            PromptKind::Passphrase => self.passphrase.clone(),
            PromptKind::Unknown => None,
        }
    }
}

/// The helper's resolution path: classify the prompt and read the matching env
/// var the app set on this process. Returns `None` when unset (the helper then
/// exits non-zero so git fails fast instead of hanging).
pub fn answer_from_env(prompt: &str) -> Option<String> {
    let key = match classify_prompt(prompt) {
        PromptKind::Username => ENV_USERNAME,
        PromptKind::Password => ENV_PASSWORD,
        PromptKind::Passphrase => ENV_PASSPHRASE,
        PromptKind::Unknown => return None,
    };
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_prompts() {
        assert_eq!(
            classify_prompt("Username for 'https://github.com':"),
            PromptKind::Username
        );
        assert_eq!(
            classify_prompt("Password for 'https://x@github.com':"),
            PromptKind::Password
        );
        assert_eq!(
            classify_prompt("Enter passphrase for key '/home/u/.ssh/id_ed25519':"),
            PromptKind::Passphrase
        );
        assert_eq!(classify_prompt("Something odd:"), PromptKind::Unknown);
    }

    #[test]
    fn answers_from_credentials() {
        let creds = Credentials {
            username: Some("octocat".into()),
            password: Some("ghp_token".into()),
            passphrase: None,
        };
        assert_eq!(creds.answer("Username for X"), Some("octocat".into()));
        assert_eq!(creds.answer("Password for X"), Some("ghp_token".into()));
        assert_eq!(creds.answer("Enter passphrase for key"), None);
        assert_eq!(creds.to_env().len(), 2);
    }
}
