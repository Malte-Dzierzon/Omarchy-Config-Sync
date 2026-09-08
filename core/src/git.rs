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
    // Hardening: hooks from the data repo never execute
    // (`pull --ff-only` would otherwise trigger them), no auto-gc in the GUI.
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("gc.auto=0")
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
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
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("clone")
        .arg(url.trim())
        .arg(dest)
        .env("GIT_OPTIONAL_LOCKS", "0")
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

/// Create a private GitHub repo via the `gh` CLI and attach it as `origin`.
/// No-op change when `gh` is missing or not logged in (clear error instead).
pub fn gh_create_repo(path: &Path, name: &str) -> Result<String, GitError> {
    require_repo(path)?;
    let clean: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if clean.is_empty() {
        return Err(GitError("empty repository name".to_string()));
    }
    let out = std::process::Command::new("gh")
        .arg("repo")
        .arg("create")
        .arg(&clean)
        .arg("--private")
        .arg("--source")
        .arg(path)
        .arg("--remote")
        .arg("origin")
        .output()
        .map_err(|e| {
            GitError(format!(
                "cannot run gh (install it via `omarchy install gh`?): {e}"
            ))
        })?;
    if out.status.success() {
        Ok(format!("created {clean} on GitHub"))
    } else {
        Err(GitError(format!(
            "gh repo create failed (logged in via `gh auth login`?): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
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
/// Pull/push failures are reported but never abort the whole summary —
/// the GUI shows one line instead of three buttons.
pub fn sync_repo(repo: &Path) -> Result<String, GitError> {
    require_repo(repo)?;
    run_git(repo, &["fetch", "--prune"])?;
    let pull_note = match run_git(repo, &["pull", "--ff-only"]) {
        Ok(_) => "pull ok".to_string(),
        Err(e) => format!("pull skipped ({e})"),
    };
    let push_note = match run_git(repo, &["push"]) {
        Ok(_) => "push ok".to_string(),
        Err(e) => format!("push skipped ({e})"),
    };
    Ok(format!("fetched · {pull_note} · {push_note}"))
}

// --- GitHub browser login (minimal UX: one button, rest automatic) ---

/// `gh` CLI present on PATH?
pub fn gh_available() -> bool {
    std::process::Command::new("gh")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhAuth {
    pub logged_in: bool,
    pub user: String,
    pub hosts: Vec<String>,
}

impl GhAuth {
    pub fn logged_out() -> Self {
        GhAuth {
            logged_in: false,
            user: String::new(),
            hosts: Vec::new(),
        }
    }
}

/// Parse `gh auth status` output (stdout+stderr combined). Pure + testable.
/// Success samples:
/// "✓ Logged in to github.com account malte (keyring)"
/// "✓ Logged in to github.com as malte (oauth_token)"
pub fn parse_gh_auth_status(combined: &str, success: bool) -> GhAuth {
    if !success {
        return GhAuth::logged_out();
    }
    let mut user = String::new();
    let mut hosts = Vec::new();
    for line in combined.lines() {
        let line = line.trim();
        if !line.contains("Logged in to") {
            continue;
        }
        // host = token after "to"
        if let Some(pos) = line.find("Logged in to ") {
            let rest = &line[pos + "Logged in to ".len()..];
            let host = rest.split_whitespace().next().unwrap_or("").to_string();
            if !host.is_empty() && !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        // user = token after "account " or "as "
        if user.is_empty() {
            let words: Vec<&str> = line.split_whitespace().collect();
            for (i, w) in words.iter().enumerate() {
                if (*w == "account" || *w == "as") && i + 1 < words.len() {
                    let cand = words[i + 1]
                        .trim_matches(|c| c == '(' || c == ')' || c == ',')
                        .to_string();
                    if !cand.is_empty() {
                        user = cand;
                        break;
                    }
                }
            }
        }
    }
    if hosts.is_empty() {
        return GhAuth::logged_out();
    }
    GhAuth {
        logged_in: true,
        user,
        hosts,
    }
}

/// Live `gh auth status` probe. Never errors — logged-out on any failure
/// (missing binary, no login, broken config).
pub fn gh_auth_status() -> GhAuth {
    let out = std::process::Command::new("gh")
        .arg("auth")
        .arg("status")
        .output();
    match out {
        Ok(o) => {
            let mut combined = String::from_utf8_lossy(&o.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&o.stderr));
            parse_gh_auth_status(&combined, o.status.success())
        }
        Err(_) => GhAuth::logged_out(),
    }
}

/// Open a terminal running `gh auth login --web` (which itself opens the
/// browser). Detached spawn — returns immediately, login completes in the
/// terminal. Falls back to opening the device page when no terminal is found.
pub fn launch_gh_login() -> Result<String, GitError> {
    if !gh_available() {
        return Err(GitError(
            "gh CLI missing (omarchy install gh), then try again".to_string(),
        ));
    }
    let attempts: &[&[&str]] = &[
        &[
            "xdg-terminal-exec",
            "--hold",
            "gh",
            "auth",
            "login",
            "--web",
        ],
        &["foot", "--hold", "gh", "auth", "login", "--web"],
        &["alacritty", "--hold", "-e", "gh", "auth", "login", "--web"],
        &["kitty", "--hold", "gh", "auth", "login", "--web"],
        &["ghostty", "-e", "gh", "auth", "login", "--web"],
        &["x-terminal-emulator", "-e", "gh", "auth", "login", "--web"],
    ];
    for cmd in attempts {
        let mut it = cmd.iter();
        let Some(bin) = it.next() else { continue };
        let args: Vec<&str> = it.copied().collect();
        // Detached spawn: login runs in the terminal, the GUI stays open.
        // (Spawn success != login success — the auto-tick notices completion.)
        match std::process::Command::new(bin).args(&args).spawn() {
            Ok(_) => {
                return Ok(
                    "Login terminal opened — confirm in the browser, then “I'm signed in”"
                        .to_string(),
                )
            }
            Err(_) => continue,
        }
    }
    // Last resort: at least open the browser device page.
    let _ = std::process::Command::new("xdg-open")
        .arg("https://github.com/login/device")
        .spawn();
    Err(GitError(
        "no terminal found for login — run `gh auth login --web` in a terminal".to_string(),
    ))
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
        Err(GitError(
            "not a git checkout — Clone or Init a repository first".to_string(),
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
        let app = crate::apps::find_app("zed").unwrap();
        let all: std::collections::HashSet<std::path::PathBuf> =
            crate::store::list_local_rels(&cfg, &app)
                .unwrap()
                .into_iter()
                .map(|f| f.rel)
                .collect();
        let n = crate::store::snapshot_selected(&cfg, &data.join("zed"), &app, &all).unwrap();
        assert_eq!(n, 1);
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
    fn gh_auth_parses_logged_in_variants() {
        let a = parse_gh_auth_status("✓ Logged in to github.com account malte (keyring)\n", true);
        assert!(a.logged_in);
        assert_eq!(a.user, "malte");
        assert_eq!(a.hosts, vec!["github.com".to_string()]);
        let b = parse_gh_auth_status(
            "✓ Logged in to ghe.example.com as ci-bot (oauth_token)\n",
            true,
        );
        assert!(b.logged_in);
        assert_eq!(b.user, "ci-bot");
        // failure output never counts as logged in
        let c = parse_gh_auth_status(
            "You are not logged into any GitHub hosts. To log in, run: gh auth login",
            false,
        );
        assert!(!c.logged_in);
        let d = parse_gh_auth_status("", true);
        assert!(!d.logged_in);
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
}
