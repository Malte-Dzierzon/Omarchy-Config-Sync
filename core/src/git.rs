//! System-git sync (SSH). Runs ONLY inside the data-repo clone, never in $HOME.
//! Requires `git` on PATH and an SSH key / agent for the remote.

use std::path::Path;
use std::process::Command;

#[derive(Debug)]
pub struct GitError(pub String);

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for GitError {}

fn run_git(repo: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| GitError(format!("cannot run git: {e}")))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(GitError(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )))
    }
}

/// `git pull --ff-only` in the data repo.
pub fn pull_ff_only(repo: &Path) -> Result<String, GitError> {
    if !repo.join(".git").exists() {
        return Err(GitError(
            "data repo is not a git checkout (missing .git)".to_string(),
        ));
    }
    run_git(repo, &["pull", "--ff-only"])
}

/// Stage + commit + push one app dir. No-op push when nothing changed.
pub fn push_app(repo: &Path, app_id: &str, message: &str) -> Result<String, GitError> {
    if !repo.join(".git").exists() {
        return Err(GitError(
            "data repo is not a git checkout (missing .git)".to_string(),
        ));
    }
    run_git(repo, &["add", "--", app_id])?;
    let status = run_git(repo, &["status", "--porcelain", "--", app_id])?;
    if status.trim().is_empty() {
        return Ok("nothing to commit".to_string());
    }
    let id_note = ensure_identity(repo)?;
    run_git(repo, &["commit", "-m", message])?;
    run_git(repo, &["push"])?;
    Ok(format!("pushed{id_note}"))
}

/// Repo-local fallback identity so the first snapshot just works even when
/// the user never configured git globally. Global config is never touched.
/// Returns a log suffix ("", or " (set local git identity)").
fn ensure_identity(repo: &Path) -> Result<&'static str, GitError> {
    let email = run_git(repo, &["config", "user.email"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !email.is_empty() {
        return Ok("");
    }
    run_git(repo, &["config", "user.email", "config-sync@local"])?;
    run_git(repo, &["config", "user.name", "Config Sync"])?;
    Ok(" (set local git identity)")
}

#[derive(Debug, Clone)]
pub struct RepoStatus {
    pub is_repo: bool,
    pub branch: String,
    pub clean: bool,
    pub ahead: usize,
    pub behind: usize,
    pub remote_url: String,
}

/// Read-only repo health (no network). Ahead/behind are vs. the last fetch.
pub fn repo_status(repo: &Path) -> Result<RepoStatus, GitError> {
    if !repo.join(".git").exists() {
        return Ok(RepoStatus {
            is_repo: false,
            branch: String::new(),
            clean: true,
            ahead: 0,
            behind: 0,
            remote_url: String::new(),
        });
    }
    // Unborn branch (fresh `init`, no commits yet) has no HEAD to parse.
    let branch = run_git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "main".to_string());
    let clean = run_git(repo, &["status", "--porcelain"])?.trim().is_empty();
    let remote_url = run_git(repo, &["remote", "get-url", "origin"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let (ahead, behind) = match run_git(
        repo,
        &["rev-list", "--left-right", "--count", "HEAD...@{u}"],
    ) {
        Ok(out) => {
            let mut it = out.split_whitespace().filter_map(|n| n.parse().ok());
            (it.next().unwrap_or(0), it.next().unwrap_or(0))
        }
        Err(_) => (0, 0), // no upstream yet
    };
    Ok(RepoStatus {
        is_repo: true,
        branch,
        clean,
        ahead,
        behind,
        remote_url,
    })
}

/// Update remote-tracking refs (network via SSH). Read-only for local files.
pub fn fetch(repo: &Path) -> Result<String, GitError> {
    require_repo(repo)?;
    run_git(repo, &["fetch", "--prune"])?;
    Ok("fetched".to_string())
}

/// Plain `git push` for already-committed work.
pub fn push(repo: &Path) -> Result<String, GitError> {
    require_repo(repo)?;
    run_git(repo, &["push"])?;
    Ok("pushed".to_string())
}

/// Clone a GitHub (or any git) URL into `dest`. Parent dir must exist,
/// `dest` must not exist yet.
pub fn clone_repo(url: &str, dest: &Path) -> Result<String, GitError> {
    if url.trim().is_empty() {
        return Err(GitError("empty clone URL".to_string()));
    }
    if dest.exists() {
        return Err(GitError(format!(
            "destination {} already exists",
            dest.display()
        )));
    }
    let parent = dest
        .parent()
        .ok_or_else(|| GitError("bad path".to_string()))?;
    let out = std::process::Command::new("git")
        .arg("clone")
        .arg(url.trim())
        .arg(dest)
        .current_dir(parent)
        .output()
        .map_err(|e| GitError(format!("cannot run git: {e}")))?;
    if out.status.success() {
        Ok("cloned".to_string())
    } else {
        Err(GitError(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// `git init -b main` in an empty (or new) directory.
pub fn init_repo(path: &Path) -> Result<String, GitError> {
    std::fs::create_dir_all(path).map_err(|e| GitError(format!("cannot create dir: {e}")))?;
    run_git(path, &["init", "-b", "main"])?;
    Ok("initialized".to_string())
}

/// Add `origin`, or repoint it when it already exists.
pub fn set_remote(repo: &Path, url: &str) -> Result<String, GitError> {
    require_repo(repo)?;
    if url.trim().is_empty() {
        return Err(GitError("empty remote URL".to_string()));
    }
    if run_git(repo, &["remote", "get-url", "origin"]).is_ok() {
        run_git(repo, &["remote", "set-url", "origin", url.trim()])?;
    } else {
        run_git(repo, &["remote", "add", "origin", url.trim()])?;
    }
    Ok("remote set".to_string())
}

/// First push of a fresh repo: `git push -u origin HEAD`.
pub fn push_upstream(repo: &Path) -> Result<String, GitError> {
    require_repo(repo)?;
    run_git(repo, &["push", "-u", "origin", "HEAD"])?;
    Ok("pushed".to_string())
}

/// Stage everything and commit once. No-op message when nothing changed.
/// For the autonomous first snapshot (many apps, one commit).
pub fn commit_all(repo: &Path, message: &str) -> Result<String, GitError> {
    require_repo(repo)?;
    let status = run_git(repo, &["status", "--porcelain"])?;
    if status.trim().is_empty() {
        return Ok("nothing to commit".to_string());
    }
    run_git(repo, &["add", "-A"])?;
    let id_note = ensure_identity(repo)?;
    run_git(repo, &["commit", "-m", message])?;
    Ok(format!("committed{id_note}"))
}

fn require_repo(repo: &Path) -> Result<(), GitError> {
    if repo.join(".git").exists() {
        Ok(())
    } else {
        Err(GitError(
            "not a git checkout — Clone or Init a repository first".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_status_and_local_clone_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("data");
        init_repo(&repo).unwrap();
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        run_git(&repo, &["add", "--", "a.txt"]).unwrap();
        run_git(
            &repo,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "x",
            ],
        )
        .unwrap();
        let st = repo_status(&repo).unwrap();
        assert!(st.is_repo && st.clean);
        set_remote(&repo, "git@github.com:me/data.git").unwrap();
        let st = repo_status(&repo).unwrap();
        assert_eq!(st.remote_url, "git@github.com:me/data.git");
        // offline clone via local path
        let clone = tmp.path().join("clone");
        clone_repo(repo.to_str().unwrap(), &clone).unwrap();
        assert!(repo_status(&clone).unwrap().is_repo);
        let st = repo_status(&tmp.path().join("nope")).unwrap();
        assert!(!st.is_repo);
    }

    #[test]
    fn commit_works_without_any_git_identity() {
        // Hide every identity source from git child processes only.
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("fresh");
        init_repo(&repo).unwrap();
        // Make really sure no local identity snuck in.
        let _ = run_git(&repo, &["config", "--unset", "user.email"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        let msg = commit_all(&repo, "initial sync").unwrap();
        assert!(msg.contains("committed"));
        assert!(repo_status(&repo).unwrap().clean);
    }
}
