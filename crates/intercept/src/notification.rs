//! Desktop notification escape sequences:
//!
//! - `OSC 9 ; message` (iTerm2). ConEmu uses `OSC 9 ; <1-12> ; ...` for other
//!   commands (`9;4` is progress), so those are not notifications.
//! - `OSC 777 ; notify ; title ; body` (rxvt-unicode, VTE, Ghostty).
//! - `OSC 99 ; metadata ; payload` (kitty desktop notifications protocol,
//!   <https://sw.kovidgoyal.net/kitty/desktop-notifications/>): chunked
//!   title/body, base64 payloads, urgency, occasion, actions, close and
//!   capability queries.
//!
//! [`NotificationParser`] turns [`Sequence`]s into [`NotifyCommand`]s and
//! keeps the per-session state needed to assemble chunked OSC 99 messages.

use std::collections::HashMap;

use crate::Sequence;

/// Maximum assembled title + body size, and number of incomplete OSC 99
/// notifications kept per session (protects against runaway programs).
const MAX_TEXT: usize = 64 * 1024;
const MAX_PENDING: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Osc9,
    Osc777,
    Osc99,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

/// When a notification should be shown (kitty `o=` key).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occasion {
    Always,
    /// Only when the terminal window is not focused.
    Unfocused,
    /// Only when the terminal window is not visible.
    Invisible,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub source: Source,
    /// Client-chosen identifier (OSC 99 `i=`), used for close and reports.
    pub id: Option<String>,
    pub title: String,
    pub body: String,
    pub urgency: Urgency,
    pub occasion: Occasion,
    /// Focus the terminal window when the notification is activated.
    pub focus_on_activate: bool,
    /// Send `OSC 99 ; i=<id> ; ST` back when activated.
    pub report_activation: bool,
    /// Send `OSC 99 ; i=<id>:p=close ; ST` back when closed.
    pub report_close: bool,
    /// Expiry in milliseconds; `None` uses the notification server default.
    pub expire_ms: Option<i32>,
}

impl Notification {
    fn new(source: Source) -> Self {
        Self {
            source,
            id: None,
            title: String::new(),
            body: String::new(),
            urgency: Urgency::Normal,
            // OSC 9 and 777 carry no occasion: avoid notifying about a window the
            // user is looking at. Kitty's default is `always`.
            occasion: match source {
                Source::Osc99 => Occasion::Always,
                _ => Occasion::Unfocused,
            },
            focus_on_activate: true,
            report_activation: false,
            report_close: false,
            expire_ms: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotifyCommand {
    Show(Notification),
    /// Close the notification with this client id (OSC 99 `p=close`).
    Close(String),
    /// Capability query (OSC 99 `p=?`); answer with [`query_response`].
    Query(Option<String>),
    /// Which notifications are still open (OSC 99 `p=alive`); answer with [`alive_response`].
    Alive(Option<String>),
}

/// Assembles notifications from OSC 9 / 777 / 99 sequences.
#[derive(Debug, Default)]
pub struct NotificationParser {
    /// Incomplete OSC 99 notifications (`d=0`), by client id.
    pending: HashMap<String, Notification>,
}

impl NotificationParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a command if `seq` is a complete notification-related sequence.
    pub fn parse(&mut self, seq: &Sequence) -> Option<NotifyCommand> {
        match seq.osc_code()? {
            9 => parse_osc9(seq.osc_args()?),
            777 => parse_osc777(seq.osc_args()?),
            99 => self.parse_osc99(seq.osc_args()?),
            _ => None,
        }
    }

    fn parse_osc99(&mut self, args: &[u8]) -> Option<NotifyCommand> {
        let sep = args.iter().position(|&b| b == b';')?;
        let (metadata, payload) = (&args[..sep], &args[sep + 1..]);

        let mut id = None;
        let mut done = true;
        let mut kind = "title".to_string();
        let mut base64 = false;
        let mut settings: Vec<(&str, &str)> = Vec::new();
        let metadata = std::str::from_utf8(metadata).ok()?;
        for item in metadata.split(':').filter(|s| !s.is_empty()) {
            let (key, value) = item.split_once('=')?;
            match key {
                "i" => id = Some(sanitize_id(value)),
                "d" => done = value != "0",
                "p" => kind = value.to_string(),
                "e" => base64 = value == "1",
                _ => settings.push((key, value)),
            }
        }

        match kind.as_str() {
            "?" => return Some(NotifyCommand::Query(id)),
            "alive" => return Some(NotifyCommand::Alive(id)),
            "close" => return id.map(NotifyCommand::Close),
            "title" | "body" => {}
            // Icons, buttons and other payload types are not supported; ignore them.
            _ => return None,
        }

        let text = if base64 {
            String::from_utf8_lossy(&decode_base64(payload)?).into_owned()
        } else {
            String::from_utf8_lossy(payload).into_owned()
        };

        let key = id.clone().unwrap_or_default();
        if !self.pending.contains_key(&key) && self.pending.len() >= MAX_PENDING {
            return None;
        }
        let n = self.pending.entry(key.clone()).or_insert_with(|| {
            let mut n = Notification::new(Source::Osc99);
            n.id = id;
            n
        });
        for (k, v) in settings {
            apply_setting(n, k, v);
        }
        let target = if kind == "body" { &mut n.body } else { &mut n.title };
        if target.len() + text.len() <= MAX_TEXT {
            target.push_str(&text);
        }

        if !done {
            return None;
        }
        let n = self.pending.remove(&key)?;
        // A notification needs at least a title or a body.
        (!n.title.is_empty() || !n.body.is_empty()).then_some(NotifyCommand::Show(n))
    }
}

fn apply_setting(n: &mut Notification, key: &str, value: &str) {
    match key {
        "u" => {
            n.urgency = match value {
                "0" => Urgency::Low,
                "2" => Urgency::Critical,
                _ => Urgency::Normal,
            }
        }
        "o" => {
            n.occasion = match value {
                "unfocused" => Occasion::Unfocused,
                "invisible" => Occasion::Invisible,
                _ => Occasion::Always,
            }
        }
        "a" => {
            for action in value.split(',') {
                match action {
                    "focus" => n.focus_on_activate = true,
                    "-focus" => n.focus_on_activate = false,
                    "report" => n.report_activation = true,
                    "-report" => n.report_activation = false,
                    _ => {}
                }
            }
        }
        "c" => n.report_close = value == "1",
        "w" => n.expire_ms = value.parse::<i32>().ok().filter(|&ms| ms >= 0),
        _ => {}
    }
}

/// Client ids may only contain `[a-zA-Z0-9-_+.]` (kitty spec).
fn sanitize_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || "-_+.".contains(*c))
        .take(64)
        .collect()
}

fn parse_osc9(args: &[u8]) -> Option<NotifyCommand> {
    // ConEmu commands: `9;<n>` or `9;<n>;...` with n in 1..=12.
    let first = args.split(|&b| b == b';').next().unwrap_or_default();
    if let Some(n) = std::str::from_utf8(first).ok().and_then(|s| s.parse::<u8>().ok())
        && (1..=12).contains(&n)
    {
        return None;
    }
    let body = String::from_utf8_lossy(args).into_owned();
    if body.trim().is_empty() {
        return None;
    }
    let mut n = Notification::new(Source::Osc9);
    n.body = body;
    Some(NotifyCommand::Show(n))
}

fn parse_osc777(args: &[u8]) -> Option<NotifyCommand> {
    let text = String::from_utf8_lossy(args);
    let mut parts = text.splitn(3, ';');
    if parts.next()? != "notify" {
        return None;
    }
    let mut n = Notification::new(Source::Osc777);
    n.title = parts.next().unwrap_or_default().to_string();
    n.body = parts.next().unwrap_or_default().to_string();
    (!n.title.is_empty() || !n.body.is_empty()).then_some(NotifyCommand::Show(n))
}

/// Reply to an OSC 99 capability query, listing what aiterm supports.
pub fn query_response(id: Option<&str>) -> String {
    let id = id.unwrap_or("");
    format!(
        "\x1b]99;i={id}:p=?;a=focus,report:c=1:o=always,unfocused,invisible:p=title,body,?,close,alive:u=0,1,2:w=1\x1b\\"
    )
}

/// Reply to an OSC 99 `p=alive` query with the ids of open notifications.
pub fn alive_response(id: Option<&str>, alive: &[&str]) -> String {
    format!("\x1b]99;i={}:p=alive;{}\x1b\\", id.unwrap_or(""), alive.join(","))
}

/// Sent to the application when a notification with `a=report` is activated.
pub fn activation_report(id: &str) -> String {
    format!("\x1b]99;i={id};\x1b\\")
}

/// Sent to the application when a notification with `c=1` is closed.
pub fn close_report(id: &str) -> String {
    format!("\x1b]99;i={id}:p=close;\x1b\\")
}

/// Standard base64 (RFC 4648), padding optional, whitespace ignored.
fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in input {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(p: &str) -> Sequence {
        Sequence::Osc(p.as_bytes().to_vec())
    }

    fn show(cmd: Option<NotifyCommand>) -> Notification {
        match cmd {
            Some(NotifyCommand::Show(n)) => n,
            other => panic!("expected Show, got {other:?}"),
        }
    }

    #[test]
    fn osc9_message_and_conemu_commands() {
        let mut p = NotificationParser::new();
        let n = show(p.parse(&osc("9;Claude is waiting for your input")));
        assert_eq!((n.source, n.title.as_str()), (Source::Osc9, ""));
        assert_eq!(n.body, "Claude is waiting for your input");
        assert_eq!(n.occasion, Occasion::Unfocused);
        // A message that starts with a number is still a message.
        assert!(p.parse(&osc("9;5 tests failed")).is_some());
        // ConEmu: progress, cwd, etc.
        for s in ["9;4;1;50", "9;9;/home", "9;1", "9;12"] {
            assert_eq!(p.parse(&osc(s)), None, "{s}");
        }
        assert_eq!(p.parse(&osc("9;")), None);
    }

    #[test]
    fn osc777_notify() {
        let mut p = NotificationParser::new();
        let n = show(p.parse(&osc("777;notify;Claude Code;Task done; all tests pass")));
        assert_eq!(n.title, "Claude Code");
        assert_eq!(n.body, "Task done; all tests pass");
        assert_eq!(p.parse(&osc("777;preexec;cmd")), None);
    }

    #[test]
    fn osc99_simple_and_chunked() {
        let mut p = NotificationParser::new();
        let n = show(p.parse(&osc("99;;Hello world")));
        assert_eq!((n.title.as_str(), n.body.as_str()), ("Hello world", ""));
        assert_eq!(n.occasion, Occasion::Always);

        // Title and body in separate chunks, tied together by id.
        assert_eq!(p.parse(&osc("99;i=a1:d=0;Build")), None);
        assert_eq!(p.parse(&osc("99;i=a1:d=0;er done")), None);
        let n = show(p.parse(&osc("99;i=a1:p=body;All 42 tests passed")));
        assert_eq!(n.id.as_deref(), Some("a1"));
        assert_eq!(n.title, "Builder done");
        assert_eq!(n.body, "All 42 tests passed");
    }

    #[test]
    fn osc99_settings_and_base64() {
        let mut p = NotificationParser::new();
        let n = show(p.parse(&osc(
            "99;i=x:e=1:u=2:o=unfocused:a=report,-focus:c=1:w=5000;SGVsbG8g8J+Riw==",
        )));
        assert_eq!(n.title, "Hello 👋");
        assert_eq!(n.urgency, Urgency::Critical);
        assert_eq!(n.occasion, Occasion::Unfocused);
        assert!(n.report_activation && !n.focus_on_activate && n.report_close);
        assert_eq!(n.expire_ms, Some(5000));
        // Invalid base64 is rejected.
        assert_eq!(p.parse(&osc("99;e=1;not*base64")), None);
    }

    #[test]
    fn osc99_queries_and_close() {
        let mut p = NotificationParser::new();
        assert_eq!(
            p.parse(&osc("99;i=q:p=?;")),
            Some(NotifyCommand::Query(Some("q".into())))
        );
        assert_eq!(
            p.parse(&osc("99;i=x:p=alive;")),
            Some(NotifyCommand::Alive(Some("x".into())))
        );
        assert_eq!(
            p.parse(&osc("99;i=a1:p=close;")),
            Some(NotifyCommand::Close("a1".into()))
        );
        assert_eq!(p.parse(&osc("99;p=icon;data")), None);
        assert!(query_response(Some("q")).starts_with("\x1b]99;i=q:p=?;a=focus,report:"));
        assert_eq!(alive_response(Some("x"), &["a", "b"]), "\x1b]99;i=x:p=alive;a,b\x1b\\");
        assert_eq!(activation_report("a1"), "\x1b]99;i=a1;\x1b\\");
        assert_eq!(close_report("a1"), "\x1b]99;i=a1:p=close;\x1b\\");
    }

    #[test]
    fn osc99_ids_are_sanitized_and_pending_is_bounded() {
        let mut p = NotificationParser::new();
        let n = show(p.parse(&osc("99;i=../evil$(id);T")));
        assert_eq!(n.id.as_deref(), Some("..evilid"));
        for i in 0..100 {
            p.parse(&osc(&format!("99;i=p{i}:d=0;chunk")));
        }
        assert_eq!(p.pending.len(), MAX_PENDING);
    }

    #[test]
    fn non_notification_sequences_are_ignored() {
        let mut p = NotificationParser::new();
        assert_eq!(p.parse(&osc("133;A")), None);
        assert_eq!(p.parse(&Sequence::Apc(b"Ga=T".to_vec())), None);
    }
}
