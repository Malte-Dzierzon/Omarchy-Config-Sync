//! Best-effort clipboard copy without extra deps: `wl-copy` (Wayland)
//! first, then `xclip` / `xsel` (X11). Used for the "Copy log" button.

use std::io::Write;
use std::process::Stdio;

/// Copy `text` to the system clipboard. True when a tool accepted it.
pub fn copy_text(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let attempts: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (bin, args) in attempts {
        if try_copy(bin, args, text) {
            return true;
        }
    }
    false
}

fn try_copy(bin: &str, args: &[&str], text: &str) -> bool {
    let mut child = match std::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let wrote = child
        .stdin
        .take()
        .map(|mut s| s.write_all(text.as_bytes()).is_ok())
        .unwrap_or(false);
    child.wait().map(|s| s.success()).unwrap_or(false) && wrote
}
