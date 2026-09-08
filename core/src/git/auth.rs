//! GitHub auth: `gh` status probes and the in-app device login flow.
//!
//! Login runs `gh auth login --web` on a background thread; the one-time
//! code streams to the UI while `gh` waits for the browser. Cancel kills
//! the whole process group — no zombies.

use std::process::{Command, Stdio};

use super::run::{kill_tree, no_prompt_gh, output_timeout, LOCAL_TIMEOUT};
use super::GitError;

// --- GitHub browser login (minimal UX: one button, rest automatic) ---

/// `gh` CLI present on PATH?
pub fn gh_available() -> bool {
    output_timeout(
        no_prompt_gh(std::process::Command::new("gh").arg("--version")),
        "gh --version",
        LOCAL_TIMEOUT,
        false,
    )
    .map(|o| o.status.success())
    .unwrap_or(false)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
/// (missing binary, no login, broken config, timeout).
pub fn gh_auth_status() -> GhAuth {
    let out = output_timeout(
        no_prompt_gh(std::process::Command::new("gh").arg("auth").arg("status")),
        "gh auth status",
        LOCAL_TIMEOUT,
        false,
    );
    match out {
        Ok(o) => {
            let mut combined = String::from_utf8_lossy(&o.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&o.stderr));
            parse_gh_auth_status(&combined, o.status.success())
        }
        Err(_) => GhAuth::logged_out(),
    }
}
// --- GitHub browser login (in-app device flow, no terminal) ---

/// Device page the browser opens for the `gh auth login --web` flow.
pub const DEVICE_URL: &str = "https://github.com/login/device";

/// One-time code (`XXXX-XXXX`) the user types at [`DEVICE_URL`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub code: String,
}

/// Scan `gh auth login --web` output for the one-time code.
/// Matches the first `XXXX-XXXX` token (ASCII alphanumeric groups of 4).
pub fn parse_device_code(text: &str) -> Option<DeviceCode> {
    for tok in text.split_whitespace() {
        let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
        let b = t.as_bytes();
        if b.len() == 9
            && b[4] == b'-'
            && b[..4].iter().all(|c| c.is_ascii_alphanumeric())
            && b[5..].iter().all(|c| c.is_ascii_alphanumeric())
        {
            return Some(DeviceCode {
                code: t.to_string(),
            });
        }
    }
    None
}

/// Open a URL in the user's browser. Best effort, never errors.
pub fn open_browser(url: &str) {
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Events of a background [`run_device_login`] run.
#[derive(Debug)]
pub enum LoginEvent {
    /// The code appeared — show it so the user can type it in the browser.
    Code(DeviceCode),
    /// `gh` exited: `Ok` = logged in, `Err` = failed/aborted.
    Done(Result<String, GitError>),
    /// The user cancelled: `gh` was killed, nothing changed.
    Cancelled,
}

/// Drain one `gh` output pipe: forward the first device code, keep capped
/// lines for the failure tail. Generic over stdout/stderr pipe types.
fn pump_pipe<R: std::io::Read + Send + 'static>(
    pipe: R,
    sender: std::sync::mpsc::Sender<LoginEvent>,
    lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut seen = String::new();
        let mut sent = false;
        for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
            seen.push_str(&line);
            seen.push('\n');
            if !sent {
                if let Some(code) = parse_device_code(&seen) {
                    let _ = sender.send(LoginEvent::Code(code));
                    sent = true;
                }
            }
            if let Ok(mut v) = lines.lock() {
                v.push(line);
                if v.len() > 60 {
                    v.remove(0);
                }
            }
        }
    })
}

/// Run `gh auth login --web` to completion, forwarding events.
/// Blocking — call on a background thread. `gh` opens the browser itself;
/// stdin is `/dev/null` so its "Press Enter" prompts resolve immediately.
/// `cancel` is polled while waiting: when set, the `gh` child is killed and
/// the run ends with `Done(Err("login cancelled"))` — no zombie process,
/// no leaked wait. Never panics; every outcome arrives as [`LoginEvent`].
pub fn run_device_login(
    tx: std::sync::mpsc::Sender<LoginEvent>,
    cancel: &std::sync::atomic::AtomicBool,
) {
    let mut cmd = Command::new("gh");
    cmd.arg("auth")
        .arg("login")
        .arg("--web")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group: cancel kills the whole tree, never orphans.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(LoginEvent::Done(Err(GitError::other(format!(
                "cannot run gh (install it via `omarchy install gh`?): {e}"
            )))));
            return;
        }
    };
    // Stream stdout + stderr: the code prints long before `gh` exits, and
    // across gh versions it may land on either stream. First match wins.
    // Both pipes stay drained until EOF (never `break`): dropping the reader
    // early could SIGPIPE `gh` right as it reports success. Lines are also
    // kept (capped) for the failure tail below.
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut pumps = Vec::new();
    if let Some(err) = child.stderr.take() {
        pumps.push(pump_pipe(err, tx.clone(), lines.clone()));
    }
    if let Some(out) = child.stdout.take() {
        pumps.push(pump_pipe(out, tx.clone(), lines.clone()));
    }
    // Wait for exit or cancel (polled — `gh` waits on the browser indefinitely).
    loop {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            kill_tree(&mut child);
            for p in pumps {
                let _ = p.join();
            }
            let _ = tx.send(LoginEvent::Cancelled);
            return;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                for p in pumps {
                    let _ = p.join();
                }
                let combined = lines.lock().map(|v| v.join("\n")).unwrap_or_default();
                if status.success() {
                    let user = gh_auth_status().user;
                    let _ = tx.send(LoginEvent::Done(Ok(if user.is_empty() {
                        "signed in".to_string()
                    } else {
                        format!("signed in as {user}")
                    })));
                } else if parse_device_code(&combined).is_some() {
                    // Exited nonzero after printing a code (e.g. user closed the
                    // browser): the code path already delivered it; report plainly.
                    let _ = tx.send(LoginEvent::Done(Err(GitError::other(
                        "login did not complete — run it again and approve in the browser",
                    ))));
                } else {
                    let tail: String = combined
                        .trim()
                        .lines()
                        .rev()
                        .take(3)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n");
                    let _ = tx.send(LoginEvent::Done(Err(GitError::other(format!(
                        "login failed: {}",
                        tail.chars().take(400).collect::<String>()
                    )))));
                }
                return;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
            Err(e) => {
                let _ = tx.send(LoginEvent::Done(Err(GitError::other(format!(
                    "login failed: {e}"
                )))));
                return;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

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
    fn device_code_parses_gh_output() {
        let sample = "! First copy your one-time code: A1B2-C3D4\nPress Enter to open github.com in your browser...";
        assert_eq!(
            parse_device_code(sample),
            Some(DeviceCode {
                code: "A1B2-C3D4".to_string()
            })
        );
        assert_eq!(parse_device_code("nothing here"), None);
        assert_eq!(parse_device_code(""), None);
    }
}
