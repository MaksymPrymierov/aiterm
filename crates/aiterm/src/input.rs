//! Keyboard -> bytes for the PTY (xterm-style encoding).
//!
//! Stage 2 adds the kitty keyboard protocol (CSI u) on top of this.

use alacritty_terminal::term::TermMode;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn mod_param(m: ModifiersState) -> u8 {
    1 + m.shift_key() as u8 + 2 * m.alt_key() as u8 + 4 * m.control_key() as u8
}

/// `CSI 1;m X` with modifiers, else SS3/CSI X depending on cursor-key mode.
fn cursor_key(letter: char, m: ModifiersState, mode: TermMode) -> Vec<u8> {
    let p = mod_param(m);
    if p > 1 {
        format!("\x1b[1;{p}{letter}").into_bytes()
    } else if mode.contains(TermMode::APP_CURSOR) {
        format!("\x1bO{letter}").into_bytes()
    } else {
        format!("\x1b[{letter}").into_bytes()
    }
}

/// `CSI n ~` / `CSI n;m ~`.
fn tilde_key(n: u8, m: ModifiersState) -> Vec<u8> {
    let p = mod_param(m);
    if p > 1 { format!("\x1b[{n};{p}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
}

/// F1-F4 use SS3 P..S.
fn ss3_fkey(letter: char, m: ModifiersState) -> Vec<u8> {
    let p = mod_param(m);
    if p > 1 { format!("\x1b[1;{p}{letter}").into_bytes() } else { format!("\x1bO{letter}").into_bytes() }
}

fn ctrl_byte(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

pub fn encode(event: &KeyEvent, m: ModifiersState, mode: TermMode) -> Option<Vec<u8>> {
    let alt_prefix = |mut v: Vec<u8>| {
        if m.alt_key() {
            v.insert(0, 0x1b);
        }
        v
    };

    let bytes = match &event.logical_key {
        Key::Named(named) => match named {
            // Shift+Enter: ESC CR, which Claude Code treats as a newline.
            // Replaced by the kitty keyboard protocol in stage 2.
            NamedKey::Enter if m.shift_key() => b"\x1b\r".to_vec(),
            NamedKey::Enter => alt_prefix(b"\r".to_vec()),
            NamedKey::Backspace if m.control_key() => alt_prefix(vec![0x08]),
            NamedKey::Backspace => alt_prefix(vec![0x7f]),
            NamedKey::Tab if m.shift_key() => b"\x1b[Z".to_vec(),
            NamedKey::Tab => alt_prefix(b"\t".to_vec()),
            NamedKey::Escape => alt_prefix(vec![0x1b]),
            NamedKey::Space if m.control_key() => alt_prefix(vec![0]),
            NamedKey::Space => alt_prefix(b" ".to_vec()),
            NamedKey::ArrowUp => cursor_key('A', m, mode),
            NamedKey::ArrowDown => cursor_key('B', m, mode),
            NamedKey::ArrowRight => cursor_key('C', m, mode),
            NamedKey::ArrowLeft => cursor_key('D', m, mode),
            NamedKey::Home => cursor_key('H', m, mode),
            NamedKey::End => cursor_key('F', m, mode),
            NamedKey::Insert => tilde_key(2, m),
            NamedKey::Delete => tilde_key(3, m),
            NamedKey::PageUp => tilde_key(5, m),
            NamedKey::PageDown => tilde_key(6, m),
            NamedKey::F1 => ss3_fkey('P', m),
            NamedKey::F2 => ss3_fkey('Q', m),
            NamedKey::F3 => ss3_fkey('R', m),
            NamedKey::F4 => ss3_fkey('S', m),
            NamedKey::F5 => tilde_key(15, m),
            NamedKey::F6 => tilde_key(17, m),
            NamedKey::F7 => tilde_key(18, m),
            NamedKey::F8 => tilde_key(19, m),
            NamedKey::F9 => tilde_key(20, m),
            NamedKey::F10 => tilde_key(21, m),
            NamedKey::F11 => tilde_key(23, m),
            NamedKey::F12 => tilde_key(24, m),
            _ => return event.text.as_ref().map(|t| alt_prefix(t.as_bytes().to_vec())),
        },
        Key::Character(s) => {
            if m.control_key() {
                let c = s.chars().next()?;
                alt_prefix(vec![ctrl_byte(c)?])
            } else {
                // Prefer the composed text (dead keys, layouts); fall back to the key.
                let text = event.text.as_deref().unwrap_or(s.as_str());
                alt_prefix(text.as_bytes().to_vec())
            }
        }
        _ => return event.text.as_ref().map(|t| t.as_bytes().to_vec()),
    };
    Some(bytes)
}

/// Prepare clipboard text for the PTY, honoring bracketed-paste mode.
pub fn paste_bytes(text: &str, mode: TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        // Strip ESC so pasted text can't terminate the bracket early.
        let clean: String = text.chars().filter(|&c| c != '\x1b').collect();
        format!("\x1b[200~{clean}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_letters() {
        assert_eq!(ctrl_byte('c'), Some(3));
        assert_eq!(ctrl_byte('C'), Some(3));
        assert_eq!(ctrl_byte('['), Some(0x1b));
        assert_eq!(ctrl_byte('é'), None);
    }

    #[test]
    fn modifier_params() {
        let shift_ctrl = ModifiersState::SHIFT | ModifiersState::CONTROL;
        assert_eq!(cursor_key('A', shift_ctrl, TermMode::empty()), b"\x1b[1;6A");
        assert_eq!(cursor_key('A', ModifiersState::empty(), TermMode::APP_CURSOR), b"\x1bOA");
        assert_eq!(tilde_key(3, ModifiersState::ALT), b"\x1b[3;3~");
    }

    #[test]
    fn paste_is_bracketed_and_sanitized() {
        assert_eq!(paste_bytes("a\x1b[201~b", TermMode::BRACKETED_PASTE), b"\x1b[200~a[201~b\x1b[201~");
        assert_eq!(paste_bytes("a\nb\r\nc", TermMode::empty()), b"a\rb\rc");
    }
}
