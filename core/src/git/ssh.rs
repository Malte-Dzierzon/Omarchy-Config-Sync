//! Local SSH readiness probe (key + agent + network, never prompts).

use std::process::{Command, Stdio};
use std::time::Duration;

use super::run::output_timeout;

// --- Local SSH readiness (key + agent + network, never prompts) ---

/// Result of an [`ssh_status`] probe for the Details panel.
#[derive(Debug, Clone)]
pub struct SshStatus {
    pub ok: bool,
    /// Short UI line, e.g. `SSH ok · octocat` or `SSH: no key — …`.
    pub detail: String,
}

/// Test `ssh -T git@github.com` with batch mode (no prompts, ~8s connect
/// timeout, 30s overall bound). Blocking — call on a background thread.
pub fn ssh_status() -> SshStatus {
    let out = output_timeout(
        Command::new("ssh")
            .arg("-T")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=8")
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("git@github.com")
            .stdin(Stdio::null()),
        "ssh -T git@github.com",
        Duration::from_secs(30),
        true,
    );
    match out {
        Err(e) => SshStatus {
            ok: false,
            detail: format!("SSH: cannot run ssh ({e})"),
        },
        Ok(o) => {
            let mut combined = String::from_utf8_lossy(&o.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&o.stderr));
            if combined
                .to_lowercase()
                .contains("successfully authenticated")
            {
                SshStatus {
                    ok: true,
                    detail: format!("SSH ok · {}", ssh_user(&combined)),
                }
            } else {
                SshStatus {
                    ok: false,
                    detail: format!("SSH: {}", ssh_fail_short(&combined)),
                }
            }
        }
    }
}

/// `Hi octocat! You've successfully authenticated…` -> `octocat`.
fn ssh_user(combined: &str) -> String {
    for line in combined.lines() {
        if let Some(rest) = line.trim().strip_prefix("Hi ") {
            let user = rest.split('!').next().unwrap_or("").trim();
            if !user.is_empty() && !user.contains(char::is_whitespace) {
                return user.to_string();
            }
        }
    }
    "key works".to_string()
}

/// First meaningful line, mapped to an actionable short hint.
fn ssh_fail_short(combined: &str) -> String {
    let first = combined
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("unknown error");
    let lower = first.to_lowercase();
    if lower.contains("permission denied") {
        "no key for GitHub — add one (`gh ssh-key add`)".to_string()
    } else if lower.contains("could not resolve")
        || lower.contains("network is unreachable")
        || lower.contains("timed out")
    {
        "no network route to github.com".to_string()
    } else if lower.contains("host key verification failed") {
        "host key rejected — check ~/.ssh/known_hosts".to_string()
    } else {
        first.chars().take(90).collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_parse_helpers_stay_short_and_clear() {
        assert_eq!(
            ssh_user("Hi octocat! You've successfully authenticated, but GitHub does not provide shell access."),
            "octocat"
        );
        assert_eq!(ssh_user("garbage"), "key works");
        assert!(
            ssh_fail_short("git@github.com: Permission denied (publickey).").contains("no key")
        );
        assert!(ssh_fail_short("ssh: Could not resolve hostname github.com").contains("no network"));
        assert_eq!(ssh_fail_short(""), "unknown error");
    }
}
