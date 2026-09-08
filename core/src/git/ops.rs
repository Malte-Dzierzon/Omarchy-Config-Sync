//! Repository operations: snapshot support, sync pipeline, GitHub repos.
//!
//! Pure git mechanics over explicit paths. The data-repo clone is the only
//! thing ever mutated; `$HOME` is read-only here.

use std::path::Path;
use std::time::Duration;

use super::auth::gh_auth_status;
use super::run::{
    no_prompt, no_prompt_gh, output_timeout, run_git, run_git_net, run_git_timeout, NET_TIMEOUT,
};
use super::{ErrorKind, GitError};

/// `git pull --ff-only` in the data repo (repairs missing tracking first).
pub fn pull_ff_only(repo: &Path) -> Result<String, GitError> {
    if !repo.join(".git").exists() {
        return Err(GitError::not_repo(
            "data repo is not a git checkout (missing .git)",
        ));
    }
    ensure_upstream(repo);
    run_git_net(repo, &["pull", "--ff-only"])
}

/// GitHub rejects blobs over 100 MB outright. A local history holding such
/// blobs (committed before the 25 MB cap existed) can NEVER push — detect it
/// in milliseconds instead of letting pack-objects churn gigabytes for minutes.
const GITHUB_MAX_BLOB: u64 = 100 * 1024 * 1024;

/// `(sha, bytes)` of blobs over `limit` in `git cat-file --batch-check` output.
fn oversized_blobs(batch_check: &str, limit: u64) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for line in batch_check.lines() {
        let mut it = line.split_whitespace();
        if let (Some(sha), Some("blob"), Some(size)) = (it.next(), it.next(), it.next()) {
            if let Ok(n) = size.parse::<u64>() {
                if n > limit {
                    out.push((sha.to_string(), n));
                }
            }
        }
    }
    out
}

/// Fail fast when the local history can never be pushed (see above).
/// Call before any push; cheap (one packed batch read).
fn check_pushable(repo: &Path) -> Result<(), GitError> {
    // Tier 1 (milliseconds): object-store size. A >100 MB blob needs room —
    // small stores cannot hold one, so the exact walk below is skipped for
    // every healthy repo. Unknown sizes fall through (safe direction).
    if let Ok(counts) = run_git(repo, &["count-objects", "-v"]) {
        let kb = |key: &str| {
            counts
                .lines()
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    if it.next() == Some(key) {
                        it.next()?.parse::<u64>().ok()
                    } else {
                        None
                    }
                })
                .next()
                .unwrap_or(u64::MAX)
        };
        if kb("size-pack") < 150 * 1024 && kb("size") < 100 * 1024 {
            return Ok(());
        }
    }
    // Tier 2 (exact, bounded): enumerate inflated blob sizes, unordered for
    // speed. Unverifiable history aborts with the recipe instead of hanging
    // pack-objects for minutes.
    let out = match run_git_timeout(
        repo,
        &[
            "cat-file",
            "--batch-check",
            "--batch-all-objects",
            "--unordered",
        ],
        Duration::from_secs(60),
        false,
    ) {
        Ok(o) => o,
        Err(_) => {
            return Err(GitError::other(format!(
                "local history is too large to verify in 60s — pushing it would take forever.\nFresh start: quit the app, delete {} (your ~/.config stays untouched), then Set up again (it re-attaches the existing remote).",
                repo.display()
            )));
        }
    };
    let big = oversized_blobs(&out, GITHUB_MAX_BLOB);
    if big.is_empty() {
        return Ok(());
    }
    let total_mb: u64 = big.iter().map(|(_, n)| n).sum::<u64>() / 1024 / 1024;
    Err(GitError::other(format!(
        "local history holds {} file(s) over GitHub's 100 MB limit ({} MB, committed before the 25 MB cap) — pushing can never succeed.\nFresh start: quit the app, delete {} (your ~/.config stays untouched), then Set up again (it re-attaches the existing remote).",
        big.len(),
        total_mb,
        repo.display()
    )))
}

/// Point a tracking-less branch at `origin/<branch>` when that ref exists.
/// Fresh inits/pushes leave `@{u}` unset, and then every pull errors with
/// "no tracking information". Best effort, never errors.
fn ensure_upstream(repo: &Path) -> bool {
    if run_git(
        repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .is_ok()
    {
        return false;
    }
    let branch = run_git(repo, &["branch", "--show-current"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if branch.is_empty() {
        return false;
    }
    let remote_ref = format!("origin/{branch}");
    if run_git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{remote_ref}"),
        ],
    )
    .is_err()
    {
        return false;
    }
    run_git(repo, &["branch", "--set-upstream-to", &remote_ref, &branch]).is_ok()
}

/// True when the current branch tracks a remote branch (`@{u}` resolves).
/// Fresh inits have none until the first publish.
fn has_upstream(repo: &Path) -> bool {
    run_git(
        repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .is_ok()
}

/// Push the current branch: plain `push` when an upstream exists, otherwise
/// `push -u origin HEAD` (a fresh repo has no `@{u}` yet, so plain `push`
/// would fail with "no upstream" on exactly the first push that matters).
/// A missing `origin` remote still errors instead of pushing nowhere.
fn push_current(repo: &Path) -> Result<String, GitError> {
    require_repo(repo)?;
    if has_upstream(repo) {
        run_git_net(repo, &["push"])?;
    } else {
        run_git_net(repo, &["push", "-u", "origin", "HEAD"])?;
    }
    Ok("pushed".to_string())
}

/// True when the remote has no branches at all (fresh empty repo): pull
/// failing there is expected, not an error worth noise.
fn remote_has_no_branches(repo: &Path) -> bool {
    run_git(repo, &["branch", "-r"])
        .map(|s| s.trim().is_empty())
        .unwrap_or(false)
}

/// Stage + commit + push one app dir. No-op push when nothing changed.
pub fn push_app(repo: &Path, app_id: &str, message: &str) -> Result<String, GitError> {
    if !repo.join(".git").exists() {
        return Err(GitError::not_repo(
            "data repo is not a git checkout (missing .git)",
        ));
    }
    check_pushable(repo)?;
    run_git(repo, &["add", "--", app_id])?;
    let status = run_git(repo, &["status", "--porcelain", "--", app_id])?;
    if status.trim().is_empty() {
        return Ok("nothing to commit".to_string());
    }
    let id_note = ensure_identity(repo)?;
    run_git(repo, &["commit", "-m", message])?;
    push_current(repo)?;
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
    run_git_net(repo, &["fetch", "--prune"])?;
    Ok("fetched".to_string())
}

/// Plain `git push` for already-committed work (first push publishes
/// with `-u` automatically, see [`push_current`]).
pub fn push(repo: &Path) -> Result<String, GitError> {
    push_current(repo)
}

/// Clone a GitHub (or any git) URL into `dest`. Parent dir must exist,
/// `dest` must not exist yet. `https://github.com/…` URLs are rewritten to
/// SSH (`git@github.com:…`) so later fetch/pull/push use the SSH key and
/// never fall back to an HTTPS username prompt.
pub fn clone_repo(url: &str, dest: &Path) -> Result<String, GitError> {
    if url.trim().is_empty() {
        return Err(GitError::other("empty clone URL"));
    }
    if dest.exists() {
        return Err(GitError::other(format!(
            "destination {} already exists",
            dest.display()
        )));
    }
    let parent = dest.parent().ok_or_else(|| GitError::other("bad path"))?;
    let fixed = normalize_github_ssh(url);
    let out = output_timeout(
        no_prompt(
            std::process::Command::new("git")
                .arg("-c")
                .arg("core.hooksPath=/dev/null")
                .arg("-c")
                .arg("gc.auto=0")
                .arg("clone")
                .arg(&fixed)
                .arg(dest)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .current_dir(parent),
        ),
        "git clone",
        NET_TIMEOUT,
        true,
    )?;
    if out.status.success() {
        Ok("cloned".to_string())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        Err(GitError {
            kind: GitError::classify(&stderr),
            message: format!("git clone failed: {}", stderr.trim()),
        })
    }
}

/// `git init -b main` in an empty (or new) directory.
pub fn init_repo(path: &Path) -> Result<String, GitError> {
    std::fs::create_dir_all(path)
        .map_err(|e| GitError::other(format!("cannot create dir: {e}")))?;
    run_git(path, &["init", "-b", "main"])?;
    Ok("initialized".to_string())
}

/// Add `origin`, or repoint it when it already exists.
/// `https://github.com/…` input is stored as SSH (see [`clone_repo`]).
pub fn set_remote(repo: &Path, url: &str) -> Result<String, GitError> {
    require_repo(repo)?;
    if url.trim().is_empty() {
        return Err(GitError::other("empty remote URL"));
    }
    let fixed = normalize_github_ssh(url);
    if run_git(repo, &["remote", "get-url", "origin"]).is_ok() {
        run_git(repo, &["remote", "set-url", "origin", &fixed])?;
    } else {
        run_git(repo, &["remote", "add", "origin", &fixed])?;
    }
    Ok("remote set".to_string())
}

/// Rewrite `https://github.com/owner/repo(.git)` to
/// `git@github.com:owner/repo.git` (case of owner/repo preserved).
/// Everything else passes through trimmed but unchanged, so non-GitHub
/// remotes and existing SSH URLs keep working.
pub fn normalize_github_ssh(url: &str) -> String {
    let t = url.trim().trim_end_matches('/').trim_end();
    if t.is_empty() {
        return String::new();
    }
    let lower = t.to_lowercase();
    for prefix in [
        "https://github.com/",
        "http://github.com/",
        "https://www.github.com/",
        "http://www.github.com/",
    ] {
        if lower.strip_prefix(prefix).is_some() {
            // Slice `t` (not `lower`): prefixes are lowercase ASCII, so byte
            // offsets match and the owner/repo case is preserved.
            let mut path = t[prefix.len()..].trim_end_matches('/').to_string();
            if path.is_empty() || !path.contains('/') {
                return t.to_string();
            }
            if !path.to_lowercase().ends_with(".git") {
                path.push_str(".git");
            }
            return format!("git@github.com:{path}");
        }
    }
    t.to_string()
}

/// First push of a fresh repo: `git push -u origin HEAD`.
pub fn push_upstream(repo: &Path) -> Result<String, GitError> {
    require_repo(repo)?;
    check_pushable(repo)?;
    run_git_net(repo, &["push", "-u", "origin", "HEAD"])?;
    Ok("pushed".to_string())
}

/// Create a private GitHub repo via the `gh` CLI and attach it as `origin`.
/// No-op change when `gh` is missing or not logged in (clear error instead).
pub fn gh_create_repo(path: &Path, name: &str) -> Result<String, GitError> {
    require_repo(path)?;
    let clean = clean_repo_name(name)?;
    let out = output_timeout(
        no_prompt_gh(
            std::process::Command::new("gh")
                .arg("repo")
                .arg("create")
                .arg(&clean)
                .arg("--private")
                .arg("--source")
                .arg(path)
                .arg("--remote")
                .arg("origin"),
        ),
        "gh repo create",
        NET_TIMEOUT,
        true,
    )?;
    if out.status.success() {
        // `gh` attaches origin as HTTPS by default — rewrite to SSH so all
        // later syncs use the key and never prompt for a username.
        if let Ok(url) =
            run_git(path, &["remote", "get-url", "origin"]).map(|s| s.trim().to_string())
        {
            let fixed = normalize_github_ssh(&url);
            if fixed != url {
                let _ = run_git(path, &["remote", "set-url", "origin", &fixed]);
            }
        }
        Ok(format!("created {clean} on GitHub"))
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        Err(GitError {
            kind: GitError::classify(&stderr),
            message: format!(
                "gh repo create failed (logged in via `gh auth login`?): {}",
                stderr.trim()
            ),
        })
    }
}

/// When `gh repo create` fails because the repo already exists under our
/// account (re-setup after deleting the local clone), attach it directly
/// instead of asking the user to paste a URL: set origin to
/// `git@github.com:<user>/<dir>.git` and publish. Returns the log line on
/// success, `None` when not applicable (leaves no remote behind on failure).
pub fn try_attach_existing(repo: &Path) -> Option<String> {
    let user = gh_auth_status().user;
    if user.is_empty() {
        return None;
    }
    let had_origin = run_git(repo, &["remote", "get-url", "origin"]).is_ok();
    let ssh = format!("git@github.com:{}/{}.git", user, repo_dir_name(repo));
    if set_remote(repo, &ssh).is_err() {
        return None;
    }
    match push_upstream(repo) {
        Ok(_) => Some(format!(
            "remote repo already existed — attached {ssh} + published"
        )),
        Err(_) => {
            if !had_origin {
                let _ = run_git(repo, &["remote", "remove", "origin"]);
            }
            None
        }
    }
}

/// Directory name for repo creation, `config-data` fallback.
pub fn repo_dir_name(repo: &Path) -> String {
    repo.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config-data")
        .to_string()
}

/// GitHub-safe repo name (`config-data` stays `config-data`).
fn clean_repo_name(name: &str) -> Result<String, GitError> {
    let clean: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if clean.is_empty() {
        return Err(GitError::other("empty repository name"));
    }
    Ok(clean)
}

/// Create an empty private GitHub repo and attach it as `origin` of the
/// local clone (no `--source`, no `--remote`: `gh` never touches local git,
/// the attach below does). Afterwards a plain push publishes the
/// already-committed snapshot. For Push auto-creating a missing remote.
pub fn gh_create_remote(repo: &Path, name: &str) -> Result<String, GitError> {
    require_repo(repo)?;
    let clean = clean_repo_name(name)?;
    let out = output_timeout(
        no_prompt_gh(
            std::process::Command::new("gh")
                .arg("repo")
                .arg("create")
                .arg(&clean)
                .arg("--private"),
        ),
        "gh repo create",
        NET_TIMEOUT,
        true,
    )?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        return Err(GitError {
            kind: GitError::classify(&stderr),
            message: format!(
                "gh repo create failed (logged in via `gh auth login`?): {}",
                stderr.trim()
            ),
        });
    }
    // Without an `origin` the follow-up push has nowhere to go (it would
    // fail with "'origin' does not appear to be a git repository") — attach
    // the just-created repo as SSH so later syncs use the key, never prompts.
    let user = gh_auth_status().user;
    if user.is_empty() {
        return Err(GitError::other(
            "remote repo created, but the GitHub user is unknown — set origin manually, then push again",
        ));
    }
    let ssh = format!("git@github.com:{user}/{clean}.git");
    set_remote(repo, &ssh)?;
    Ok(format!("created {clean} on GitHub"))
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

/// One-button background sync: fetch, then fast-forward pull, then push.
/// Strict where the user must act, lenient where retrying helps:
/// - diverged branches abort with [`ErrorKind::Conflict`] (never force-push
///   or silently skip — the user resolves, then syncs again);
/// - auth failures abort with [`ErrorKind::Auth`] (actionable: login/SSH);
/// - deterministic push failures (missing remote, rejected refs, …) abort
///   with their kind instead of hiding inside an ok-looking summary;
/// - transient network failures are reported but never abort the summary.
pub fn sync_repo(repo: &Path) -> Result<String, GitError> {
    require_repo(repo)?;
    run_git_net(repo, &["fetch", "--prune"])?;
    ensure_upstream(repo);
    let pull_note = match run_git_net(repo, &["pull", "--ff-only"]) {
        Ok(_) => "pull ok".to_string(),
        Err(e) if e.kind() == ErrorKind::Conflict => {
            return Err(GitError::conflict(
                "branches diverged (both sides moved) — resolve in a terminal (`git status` in the repo), then Sync again. Nothing was overwritten.",
            ));
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("no tracking information") && remote_has_no_branches(repo) {
                "pull ok (remote empty)".to_string()
            } else {
                format!("pull skipped ({e})")
            }
        }
    };
    check_pushable(repo)?;
    let push_note = match push_current(repo) {
        Ok(_) => "push ok".to_string(),
        // Transient (offline mid-sync, timeout): the next Sync retries the
        // push — worth a note, not an abort.
        Err(e) if e.kind() == ErrorKind::Network => format!("push skipped ({e})"),
        // Deterministic (auth, missing remote, rejected refs, …): retrying
        // changes nothing, so fail loudly and keep the fetch/pull progress
        // in the message instead of an ok-looking summary.
        Err(e) => {
            return Err(GitError::with_kind(
                e.kind(),
                format!("fetched · {pull_note}, but push failed:\n{e}"),
            ));
        }
    };
    Ok(format!("fetched · {pull_note} · {push_note}"))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupState {
    NeedLogin,
    NeedRepo,
    Ready,
}

/// Testable setup classifier: login first, then repo presence.
/// An empty path is never "Ready" (`Path::new("").join(".git")` would
/// otherwise match `./.git` of whatever the CWD happens to be).
pub fn setup_state_for(logged_in: bool, repo: &Path) -> SetupState {
    if !logged_in {
        return SetupState::NeedLogin;
    }
    if repo.as_os_str().is_empty() {
        return SetupState::NeedRepo;
    }
    if repo.join(".git").exists() {
        SetupState::Ready
    } else {
        SetupState::NeedRepo
    }
}

/// Live setup state (probes `gh auth status` + `.git` presence).
pub fn setup_state(repo: &Path) -> SetupState {
    setup_state_for(gh_auth_status().logged_in, repo)
}

fn require_repo(repo: &Path) -> Result<(), GitError> {
    if repo.join(".git").exists() {
        Ok(())
    } else {
        Err(GitError::not_repo(
            "not a git checkout — Clone or Init a repository first",
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// Env vars are process-global and tests run in parallel: every test
    /// that hides the git identity restores the previous values on exit,
    /// so no test can leak `/dev/null` config into another one.
    struct HiddenIdentity {
        prev_global: Option<OsString>,
        prev_system: Option<OsString>,
    }

    impl HiddenIdentity {
        fn hide() -> Self {
            let prev = Self {
                prev_global: std::env::var_os("GIT_CONFIG_GLOBAL"),
                prev_system: std::env::var_os("GIT_CONFIG_SYSTEM"),
            };
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
            prev
        }
    }

    impl Drop for HiddenIdentity {
        fn drop(&mut self) {
            match self.prev_global.take() {
                Some(v) => std::env::set_var("GIT_CONFIG_GLOBAL", v),
                None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
            }
            match self.prev_system.take() {
                Some(v) => std::env::set_var("GIT_CONFIG_SYSTEM", v),
                None => std::env::remove_var("GIT_CONFIG_SYSTEM"),
            }
        }
    }

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
        let _hidden = HiddenIdentity::hide();
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

    #[test]
    fn full_new_repository_flow_offline() {
        // Mirrors the GUI "New repository" button, with a local bare repo
        // standing in for GitHub (same git protocol, no network).
        let _hidden = HiddenIdentity::hide();
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(cfg.join("zed")).unwrap();
        std::fs::write(cfg.join("zed/settings.json"), "{}").unwrap();
        let origin = tmp.path().join("origin.git");
        run_git(tmp.path(), &["init", "--bare", "-b", "main", "origin.git"]).unwrap();
        let data = tmp.path().join("data");
        init_repo(&data).unwrap();
        let app = crate::apps::resolve_app(std::path::Path::new(""), "zed");
        let all: std::collections::HashSet<std::path::PathBuf> =
            crate::store::list_local_rels(&cfg, &app)
                .unwrap()
                .0
                .into_iter()
                .map(|f| f.rel)
                .collect();
        let r = crate::store::snapshot_selected(&cfg, &data.join("zed"), &app, &all).unwrap();
        assert_eq!(r.copied, 1);
        commit_all(&data, "initial sync").unwrap();
        set_remote(&data, origin.to_str().unwrap()).unwrap();
        push_upstream(&data).unwrap();
        // the "GitHub" side now serves the files; a fresh clone gets them
        let clone = tmp.path().join("clone");
        clone_repo(origin.to_str().unwrap(), &clone).unwrap();
        assert!(clone.join("zed/settings.json").is_file());
        assert_eq!(
            std::fs::read_to_string(clone.join("zed/settings.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn normalize_github_ssh_rewrites_https() {
        assert_eq!(
            normalize_github_ssh("https://github.com/me/config-data.git"),
            "git@github.com:me/config-data.git"
        );
        assert_eq!(
            normalize_github_ssh("https://github.com/Me/Config-Data"),
            "git@github.com:Me/Config-Data.git"
        );
        assert_eq!(
            normalize_github_ssh("http://www.github.com/o/r/"),
            "git@github.com:o/r.git"
        );
        // non-GitHub + existing SSH pass through
        assert_eq!(
            normalize_github_ssh("git@github.com:me/data.git"),
            "git@github.com:me/data.git"
        );
        assert_eq!(
            normalize_github_ssh("git@gitlab.com:me/data.git"),
            "git@gitlab.com:me/data.git"
        );
        assert_eq!(normalize_github_ssh("  "), "");
    }

    #[test]
    fn oversized_blobs_parses_batch_check() {
        let sample = "abc123 blob 50\ndef456 blob 104857601\nghi789 commit 100\njkl012 blob 104857600\nbadline\n";
        let big = super::oversized_blobs(sample, super::GITHUB_MAX_BLOB);
        assert_eq!(big.len(), 1);
        assert_eq!(big[0].0, "def456");
        assert_eq!(big[0].1, 104857601);
        assert!(super::oversized_blobs("", super::GITHUB_MAX_BLOB).is_empty());
    }

    #[test]
    fn ensure_upstream_repairs_tracking() {
        let _hidden = HiddenIdentity::hide();
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        super::run_git(tmp.path(), &["init", "--bare", "-b", "main", "origin.git"]).unwrap();
        let data = tmp.path().join("data");
        super::init_repo(&data).unwrap();
        std::fs::write(data.join("a.txt"), "a").unwrap();
        super::commit_all(&data, "init").unwrap();
        // No remote yet: nothing to repair.
        assert!(!super::ensure_upstream(&data));
        super::set_remote(&data, origin.to_str().unwrap()).unwrap();
        // Remote branch doesn't exist yet either.
        assert!(!super::ensure_upstream(&data));
        super::push_upstream(&data).unwrap();
        // Upstream set by push; then break it and repair.
        assert!(!super::ensure_upstream(&data));
        super::run_git(&data, &["branch", "--unset-upstream"]).unwrap();
        assert!(super::ensure_upstream(&data));
        assert!(super::run_git(
            &data,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]
        )
        .is_ok());
    }

    #[test]
    fn check_pushable_passes_small_repos() {
        let _hidden = HiddenIdentity::hide();
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        super::init_repo(&data).unwrap();
        std::fs::write(data.join("a.txt"), "a").unwrap();
        super::commit_all(&data, "init").unwrap();
        assert!(super::check_pushable(&data).is_ok());
    }

    #[test]
    fn repo_dir_name_falls_back() {
        assert_eq!(
            repo_dir_name(Path::new("/home/u/config-data")),
            "config-data"
        );
        assert_eq!(repo_dir_name(Path::new("")), "config-data");
    }

    #[test]
    fn setup_classifier_prefers_login_over_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        assert_eq!(setup_state_for(false, &missing), SetupState::NeedLogin);
        assert_eq!(setup_state_for(true, &missing), SetupState::NeedRepo);
        // an empty path is never Ready (no CWD-relative `./.git` match)
        assert_eq!(
            setup_state_for(true, std::path::Path::new("")),
            SetupState::NeedRepo
        );
        let repo = tmp.path().join("data");
        init_repo(&repo).unwrap();
        assert_eq!(setup_state_for(true, &repo), SetupState::Ready);
        // login still wins even when a repo exists
        assert_eq!(setup_state_for(false, &repo), SetupState::NeedLogin);
    }

    #[test]
    fn sync_repo_reports_combined_summary_offline() {
        let _hidden = HiddenIdentity::hide();
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        run_git(tmp.path(), &["init", "--bare", "-b", "main", "origin.git"]).unwrap();
        let data = tmp.path().join("data");
        init_repo(&data).unwrap();
        std::fs::write(data.join("a.txt"), "a").unwrap();
        commit_all(&data, "init").unwrap();
        set_remote(&data, origin.to_str().unwrap()).unwrap();
        push_upstream(&data).unwrap();
        let summary = sync_repo(&data).unwrap();
        assert!(summary.contains("fetched"));
    }

    #[test]
    fn push_current_publishes_without_upstream() {
        // Regression: plain `git push` fails with "no upstream" on exactly
        // the first push that matters — push_current must publish with -u.
        let _hidden = HiddenIdentity::hide();
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        run_git(tmp.path(), &["init", "--bare", "-b", "main", "origin.git"]).unwrap();
        let data = tmp.path().join("data");
        init_repo(&data).unwrap();
        std::fs::write(data.join("a.txt"), "a").unwrap();
        commit_all(&data, "init").unwrap();
        set_remote(&data, origin.to_str().unwrap()).unwrap();
        assert!(!super::has_upstream(&data));
        assert_eq!(super::push_current(&data).unwrap(), "pushed");
        assert!(super::has_upstream(&data));
        // …and the remote side serves the commit.
        let clone = tmp.path().join("clone");
        clone_repo(origin.to_str().unwrap(), &clone).unwrap();
        assert!(clone.join("a.txt").is_file());
    }

    #[test]
    fn push_current_without_remote_errors_instead_of_pushing_nowhere() {
        let _hidden = HiddenIdentity::hide();
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        init_repo(&data).unwrap();
        std::fs::write(data.join("a.txt"), "a").unwrap();
        commit_all(&data, "init").unwrap();
        // No origin at all: must error (actionable), never pretend success.
        assert!(super::push_current(&data).is_err());
    }

    #[test]
    fn push_app_second_run_reports_nothing_to_commit() {
        // Regression: the manifest timestamp alone must not dirty the repo —
        // otherwise every Push commits a no-op "sync zed" forever.
        let _hidden = HiddenIdentity::hide();
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(cfg.join("zed")).unwrap();
        std::fs::write(cfg.join("zed/settings.json"), "{}").unwrap();
        let origin = tmp.path().join("origin.git");
        run_git(tmp.path(), &["init", "--bare", "-b", "main", "origin.git"]).unwrap();
        let data = tmp.path().join("data");
        init_repo(&data).unwrap();
        set_remote(&data, origin.to_str().unwrap()).unwrap();
        let app = crate::apps::resolve_app(std::path::Path::new(""), "zed");
        let sel: std::collections::HashSet<std::path::PathBuf> =
            [std::path::PathBuf::from("settings.json")].into();
        crate::store::snapshot_selected(&cfg, &data.join("zed"), &app, &sel).unwrap();
        assert!(push_app(&data, "zed", "sync zed")
            .unwrap()
            .contains("pushed"));
        // Snapshot + push again without changes: clean, no new commit.
        crate::store::snapshot_selected(&cfg, &data.join("zed"), &app, &sel).unwrap();
        assert_eq!(
            push_app(&data, "zed", "sync zed").unwrap(),
            "nothing to commit"
        );
        let count: usize = run_git(&data, &["rev-list", "--count", "HEAD"])
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(count, 1);
    }
}
