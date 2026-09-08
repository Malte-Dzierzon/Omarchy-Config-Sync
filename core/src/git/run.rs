//! Child-process plumbing for git/gh/ssh: prompts off, timeouts on.
//!
//! Every spawned child runs stdin-null in its own process group with a hard
//! time bound, so no network stall or TTY prompt can ever hang a job.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{ErrorKind, GitError};

/// No network child may hang a job forever: pushes/clones get 90 s,
/// local git calls 15 s. Expiry kills the child and reports which
/// command timed out (instead of a spinner that never stops).
pub(crate) const NET_TIMEOUT: Duration = Duration::from_secs(90);
pub(crate) const LOCAL_TIMEOUT: Duration = Duration::from_secs(15);
/// Never block the GUI on a credential prompt: every child process gets
/// `/dev/null` as stdin and git is told not to ask on the terminal.
/// SSH additionally runs batch-mode (no host-key/password prompts on any
/// TTY — `GIT_TERMINAL_PROMPT` alone does not cover ssh itself), unless the
/// user already configured `GIT_SSH_COMMAND` (theirs wins).
/// Auth failures then surface as typed errors instead of
/// freezing the app on `Username for 'https://github.com':`.
pub(crate) fn no_prompt(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new",
        );
    }
    cmd
}

/// Same for `gh` one-shots (create/status/version): prompts disabled so a
/// missing decision errors out instead of reading `/dev/tty` forever.
/// NOT used for the device login flow (that one is meant to be interactive).
pub(crate) fn no_prompt_gh(cmd: &mut Command) -> &mut Command {
    no_prompt(cmd).env("GH_PROMPT_DISABLED", "1")
}

/// Kill a child AND its descendants. Git spawns pack-objects/ssh as
/// grandchildren — a plain `kill` orphans them churning CPU/RAM forever
/// (observed live: 924 MB pack-objects surviving its dead parent).
/// Children are spawned as group leaders (see `output_timeout`), so a
/// negative-pid kill takes the whole group; plain kill is the fallback.
pub(crate) fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pgid = child.id() as i32;
        if pgid > 0 {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
    let _ = child.kill();
    let _ = child.wait(); // reap, never zombie
}

/// Bounded child execution for `run_git`/`gh` one-shots: pipes are captured,
/// the whole process group is killed after `timeout`, and expiry names the
/// command. `network` picks the [`ErrorKind`] for the timeout itself.
pub(crate) fn output_timeout(
    cmd: &mut Command,
    what: &str,
    timeout: Duration,
    network: bool,
) -> Result<std::process::Output, GitError> {
    // Own process group: on timeout killpg() reaps grandchildren
    // (pack-objects/ssh) too — plain kill() would orphan them.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GitError::other(format!("cannot run {what}: {e}")))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|e| GitError::other(format!("cannot read {what} output: {e}")))
            }
            Ok(None) if Instant::now() >= deadline => {
                kill_tree(&mut child);
                let msg = if network {
                    format!(
                        "timed out after {}s: {what} — check your connection, then try again",
                        timeout.as_secs()
                    )
                } else {
                    format!(
                        "timed out after {}s: {what} (local command hung — please report this)",
                        timeout.as_secs()
                    )
                };
                return Err(if network {
                    GitError {
                        kind: ErrorKind::Network,
                        message: msg,
                    }
                } else {
                    GitError::other(msg)
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                return Err(GitError::other(format!("cannot wait for {what}: {e}")));
            }
        }
    }
}

/// Local git call (bounded by [`LOCAL_TIMEOUT`]).
pub(crate) fn run_git(repo: &Path, args: &[&str]) -> Result<String, GitError> {
    run_git_timeout(repo, args, LOCAL_TIMEOUT, false)
}

/// Network git call (bounded by [`NET_TIMEOUT`]): fetch/pull/push/clone.
pub(crate) fn run_git_net(repo: &Path, args: &[&str]) -> Result<String, GitError> {
    run_git_timeout(repo, args, NET_TIMEOUT, true)
}

pub(crate) fn run_git_timeout(
    repo: &Path,
    args: &[&str],
    timeout: Duration,
    network: bool,
) -> Result<String, GitError> {
    // Hardening: hooks from the data repo never execute
    // (`pull --ff-only` would otherwise trigger them), no auto-gc in the GUI.
    let what = format!("git {}", args.join(" "));
    let out = output_timeout(
        no_prompt(
            Command::new("git")
                .arg("-C")
                .arg(repo)
                .arg("-c")
                .arg("core.hooksPath=/dev/null")
                .arg("-c")
                .arg("gc.auto=0")
                .args(args)
                .env("GIT_OPTIONAL_LOCKS", "0"),
        ),
        &what,
        timeout,
        network,
    )?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(GitError {
            kind: GitError::classify(&stderr),
            message: format!("{what} failed: {}", stderr.trim()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn kill_tree_reaps_the_child() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        super::kill_tree(&mut child);
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    #[cfg(unix)]
    fn bounded_execution_kills_hung_children() {
        let err = output_timeout(
            std::process::Command::new("sleep").arg("5"),
            "sleep 5",
            Duration::from_millis(200),
            true,
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Network);
        assert!(err.to_string().contains("timed out after 0s: sleep 5"));
        let out = output_timeout(
            std::process::Command::new("echo").arg("hi"),
            "echo hi",
            Duration::from_secs(5),
            false,
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }
}
