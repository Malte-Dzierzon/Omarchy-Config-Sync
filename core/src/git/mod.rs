//! System-git sync (SSH). Runs ONLY inside the data-repo clone, never in $HOME.
//! Requires `git` on PATH and an SSH key / agent for the remote.

mod auth;
mod ops;
mod run;
mod ssh;

pub use auth::{
    gh_auth_status, gh_available, open_browser, parse_device_code, parse_gh_auth_status,
    run_device_login, DeviceCode, GhAuth, LoginEvent, DEVICE_URL,
};
pub use ops::{
    clone_repo, commit_all, fetch, gh_create_remote, gh_create_repo, init_repo,
    normalize_github_ssh, pull_ff_only, push, push_app, push_upstream, repo_dir_name, repo_status,
    set_remote, setup_state, setup_state_for, sync_repo, try_attach_existing, RepoStatus,
    SetupState,
};
pub use ssh::{ssh_status, SshStatus};

/// Machine-readable failure kinds: callers switch on these instead of
/// substring-matching message text (brittle across wordings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Login/SSH/key missing — actionable in the app (login, Use SSH).
    Auth,
    /// The remote repo does not exist on the server — actionable by
    /// creating it (Push does this automatically).
    MissingRemote,
    /// DNS/timeout/offline — transient, safe to skip quietly.
    Network,
    /// Branches diverged, fast-forward refused — user must resolve.
    Conflict,
    /// Missing `.git` — set up or clone first.
    NotRepo,
    /// Everything else (bad args, missing binary, command failed).
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError {
    kind: ErrorKind,
    message: String,
}

impl GitError {
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn other(message: impl Into<String>) -> Self {
        GitError {
            kind: ErrorKind::Other,
            message: message.into(),
        }
    }

    pub fn not_repo(message: impl Into<String>) -> Self {
        GitError {
            kind: ErrorKind::NotRepo,
            message: message.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        GitError {
            kind: ErrorKind::Conflict,
            message: message.into(),
        }
    }

    /// Rebuild with a new kind (e.g. adding call-site context to a message
    /// while keeping the machine-readable kind for the UI switch).
    pub fn with_kind(kind: ErrorKind, message: impl Into<String>) -> Self {
        GitError {
            kind,
            message: message.into(),
        }
    }

    /// Classify command stderr (case-insensitive needles). Auth first: an
    /// unreachable host behind a failed handshake still reads as auth.
    pub fn classify(stderr: &str) -> ErrorKind {
        let lower = stderr.to_lowercase();
        let has = |n: &[&str]| n.iter().any(|x| lower.contains(x));
        if has(&[
            "terminal prompts disabled",
            "could not read username",
            "authentication failed",
            "permission denied (publickey)",
            "host key verification failed",
        ]) {
            ErrorKind::Auth
        } else if has(&["repository not found", "remote repository not found"]) {
            // `ERROR: Repository not found.` / `fatal: Could not read from
            // remote repository.` — the remote repo was deleted or never
            // created (NOT an auth problem when SSH itself works).
            ErrorKind::MissingRemote
        } else if has(&[
            "divergent branches",
            "non-fast-forward",
            "failed to push some refs",
            "fetch first",
            "have diverged",
            "conflict",
        ]) {
            ErrorKind::Conflict
        } else if has(&[
            "could not resolve host",
            "could not resolve hostname",
            "network is unreachable",
            "timed out",
            "ssh: connect to host",
        ]) {
            ErrorKind::Network
        } else {
            ErrorKind::Other
        }
    }

    /// Actionable hint line for auth failures, else "".
    fn hint(&self) -> &'static str {
        if self.kind == ErrorKind::Auth {
            "\nHint: GitHub sign-in or SSH key missing — sign in again in the app or run `gh auth login --web`, then `ssh -T git@github.com`."
        } else {
            ""
        }
    }
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.message, self.hint())
    }
}

impl std::error::Error for GitError {}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_sorts_failures_for_honest_handling() {
        use ErrorKind::*;
        assert_eq!(GitError::classify("error: could not read Username for 'https://github.com': terminal prompts disabled"), Auth);
        assert_eq!(
            GitError::classify("git@github.com: Permission denied (publickey)."),
            Auth
        );
        assert_eq!(
            GitError::classify("error: failed to push some refs (fetch first)"),
            Conflict
        );
        assert_eq!(
            GitError::classify("CONFLICT (content): Merge conflict in f"),
            Conflict
        );
        assert_eq!(
            GitError::classify("ssh: Could not resolve hostname github.com"),
            Network
        );
        assert_eq!(
            GitError::classify(
                "ERROR: Repository not found.\nfatal: Could not read from remote repository."
            ),
            MissingRemote
        );
        assert_eq!(GitError::classify(""), Other);
        // Display keeps the actionable hint for auth, raw otherwise.
        assert!(GitError::other("x").to_string() == "x");
        assert!(format!(
            "{}",
            GitError {
                kind: Auth,
                message: "git push failed".to_string()
            }
        )
        .contains("Hint: GitHub sign-in"));
    }
}
