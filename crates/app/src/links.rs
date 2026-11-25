//! Opening OSC 8 hyperlinks, and helpers for the working directory.
//!
//! Hyperlink targets come from whatever runs in the terminal, so only a small
//! set of schemes is opened, `file://` only for this host, and the URI is
//! passed to `xdg-open` as a single argument (no shell).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const ALLOWED_SCHEMES: &[&str] = &["http", "https", "mailto", "file"];

/// Whether a hyperlink target may be opened.
pub fn is_openable(uri: &str, local_host: &str) -> bool {
    let Some((scheme, rest)) = uri.split_once(':') else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    if !ALLOWED_SCHEMES.contains(&scheme.as_str()) || uri.chars().any(char::is_control) {
        return false;
    }
    if scheme == "file" {
        let Some(rest) = rest.strip_prefix("//") else {
            return false;
        };
        let host = rest.split('/').next().unwrap_or_default();
        return host.is_empty() || host == "localhost" || host.eq_ignore_ascii_case(local_host);
    }
    !rest.is_empty()
}

/// Open a URI with the desktop's default handler, without waiting for it.
pub fn open(uri: &str) {
    spawn_detached(Command::new("xdg-open").arg(uri), "xdg-open");
}

/// Start another aiterm window in `cwd`.
pub fn new_window(cwd: Option<&Path>) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return log::warn!("cannot locate the aiterm executable: {e}"),
    };
    let mut cmd = Command::new(exe);
    if let Some(dir) = cwd.filter(|d| d.is_dir()) {
        cmd.current_dir(dir);
    }
    spawn_detached(&mut cmd, "aiterm");
}

fn spawn_detached(cmd: &mut Command, name: &str) {
    match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        // Reap the child in the background so it doesn't linger as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => log::warn!("failed to run {name}: {e}"),
    }
}

/// The working directory of a process, from `/proc` (fallback when the shell
/// doesn't report OSC 7).
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// This machine's host name, used to accept local `file://` URLs.
pub fn local_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_schemes() {
        assert!(is_openable("https://example.com/a?b=c", "box"));
        assert!(is_openable("HTTP://example.com", "box"));
        assert!(is_openable("mailto:dev@example.com", "box"));
        assert!(is_openable("file:///home/user/report.pdf", "box"));
        assert!(is_openable("file://box/home/user", "box"));
        assert!(is_openable("file://localhost/tmp", "box"));
    }

    #[test]
    fn rejected() {
        for uri in [
            "javascript:alert(1)",
            "ssh://host",
            "file://otherhost/etc/passwd",
            "file:relative",
            "https:",
            "no-scheme",
            "https://example.com/\x1b[2J",
        ] {
            assert!(!is_openable(uri, "box"), "{uri}");
        }
    }

    #[test]
    fn proc_cwd_of_self() {
        assert_eq!(process_cwd(std::process::id()), std::env::current_dir().ok());
    }
}
