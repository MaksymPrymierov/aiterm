//! Shell integration sequences:
//!
//! - `OSC 7 ; file://host/path` — current working directory (also kitty's
//!   `kitty-shell-cwd://` scheme). Paths are percent-decoded; reports for
//!   other hosts (e.g. from an SSH session) are ignored.
//! - `OSC 133 ; A|B|C|D[;exit]` — semantic prompt marks (FinalTerm):
//!   prompt start, prompt end / input start, command output start, command
//!   finished.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use crate::Sequence;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellEvent {
    /// OSC 7: the shell changed directory.
    Cwd(PathBuf),
    /// OSC 133;A
    PromptStart,
    /// OSC 133;B: the prompt is drawn, the user types from here.
    PromptEnd,
    /// OSC 133;C: the command started; its output follows.
    CommandStart,
    /// OSC 133;D: the command finished, with its exit status if reported.
    CommandEnd(Option<i32>),
}

/// Parse OSC 7 and OSC 133. `local_host` is this machine's host name.
pub fn parse_shell_event(seq: &Sequence, local_host: &str) -> Option<ShellEvent> {
    match seq.osc_code()? {
        7 => parse_cwd(seq.osc_args()?, local_host).map(ShellEvent::Cwd),
        133 => parse_mark(seq.osc_args()?),
        _ => None,
    }
}

fn parse_cwd(url: &[u8], local_host: &str) -> Option<PathBuf> {
    let rest = [&b"file://"[..], b"kitty-shell-cwd://"]
        .iter()
        .find_map(|scheme| url.strip_prefix(*scheme))?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let host = std::str::from_utf8(&rest[..slash]).ok()?;
    if !(host.is_empty() || host == "localhost" || host.eq_ignore_ascii_case(local_host)) {
        return None;
    }
    let path = percent_decode(&rest[slash..])?;
    if path.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(path)))
}

fn parse_mark(args: &[u8]) -> Option<ShellEvent> {
    let text = std::str::from_utf8(args).ok()?;
    let mut parts = text.split(';');
    Some(match parts.next()? {
        "A" => ShellEvent::PromptStart,
        "B" => ShellEvent::PromptEnd,
        "C" => ShellEvent::CommandStart,
        "D" => ShellEvent::CommandEnd(parts.next().and_then(|s| s.parse().ok())),
        _ => return None,
    })
}

fn percent_decode(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            let hex = std::str::from_utf8(s.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Option<ShellEvent> {
        parse_shell_event(&Sequence::Osc(s.as_bytes().to_vec()), "myhost")
    }

    fn cwd(p: &str) -> Option<ShellEvent> {
        Some(ShellEvent::Cwd(PathBuf::from(p)))
    }

    #[test]
    fn osc7_cwd() {
        assert_eq!(parse("7;file://myhost/home/user/src"), cwd("/home/user/src"));
        assert_eq!(parse("7;file:///tmp"), cwd("/tmp"));
        assert_eq!(parse("7;file://localhost/tmp"), cwd("/tmp"));
        assert_eq!(parse("7;kitty-shell-cwd://MyHost/tmp"), cwd("/tmp"));
        assert_eq!(
            parse("7;file://myhost/home/user/My%20Docs/%D0%BF%D1%80%D0%BE"),
            cwd("/home/user/My Docs/про")
        );
    }

    #[test]
    fn osc7_rejects_remote_and_malformed() {
        assert_eq!(parse("7;file://otherhost/home"), None);
        assert_eq!(parse("7;https://myhost/home"), None);
        assert_eq!(parse("7;file://myhost"), None);
        assert_eq!(parse("7;file://myhost/bad%zz"), None);
        assert_eq!(parse("7;file://myhost/nul%00byte"), None);
    }

    #[test]
    fn osc133_marks() {
        assert_eq!(parse("133;A"), Some(ShellEvent::PromptStart));
        assert_eq!(parse("133;A;cl=m;aid=123"), Some(ShellEvent::PromptStart));
        assert_eq!(parse("133;B"), Some(ShellEvent::PromptEnd));
        assert_eq!(parse("133;C"), Some(ShellEvent::CommandStart));
        assert_eq!(parse("133;D;0"), Some(ShellEvent::CommandEnd(Some(0))));
        assert_eq!(parse("133;D;130"), Some(ShellEvent::CommandEnd(Some(130))));
        assert_eq!(parse("133;D"), Some(ShellEvent::CommandEnd(None)));
        assert_eq!(parse("133;Z"), None);
        assert_eq!(parse("9;hello"), None);
    }
}
