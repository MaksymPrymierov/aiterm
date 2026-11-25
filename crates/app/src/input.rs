//! Keyboard -> bytes for the PTY.
//!
//! Two encoders:
//! - legacy xterm-style encoding, used by default;
//! - the kitty keyboard protocol (<https://sw.kovidgoyal.net/kitty/keyboard-protocol/>),
//!   used when an application enables any progressive enhancement flag with
//!   `CSI > flags u`. The flag stack (push/pop/set/query) is tracked by the VT
//!   backend; this module only encodes keys according to the active flags.
//!
//! Key events are first converted from winit into a platform-independent
//! [`KeyInput`], which keeps the encoders unit-testable.

use aiterm_vt::Modes;
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key as WKey, KeyCode, KeyLocation, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

/// Modifier bits as defined by the kitty protocol (the encoded value is `1 + bits`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods(u8);

impl Mods {
    pub const NONE: Mods = Mods(0);
    pub const SHIFT: Mods = Mods(1);
    pub const ALT: Mods = Mods(2);
    pub const CTRL: Mods = Mods(4);
    pub const SUPER: Mods = Mods(8);
    pub const HYPER: Mods = Mods(16);
    pub const META: Mods = Mods(32);

    pub fn contains(self, other: Mods) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn without(self, other: Mods) -> Mods {
        Mods(self.0 & !other.0)
    }

    fn shift(self) -> bool {
        self.contains(Mods::SHIFT)
    }

    fn alt(self) -> bool {
        self.contains(Mods::ALT)
    }

    fn ctrl(self) -> bool {
        self.contains(Mods::CTRL)
    }

    /// Value of the modifier parameter (`1 + bits`), shared by xterm and kitty.
    fn param(self) -> u8 {
        1 + self.0
    }
}

impl std::ops::BitOr for Mods {
    type Output = Mods;
    fn bitor(self, rhs: Mods) -> Mods {
        Mods(self.0 | rhs.0)
    }
}

impl From<ModifiersState> for Mods {
    fn from(m: ModifiersState) -> Self {
        let mut out = Mods::NONE;
        for (has, bit) in [
            (m.shift_key(), Mods::SHIFT),
            (m.alt_key(), Mods::ALT),
            (m.control_key(), Mods::CTRL),
            (m.super_key(), Mods::SUPER),
        ] {
            if has {
                out = out | bit;
            }
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Press,
    Repeat,
    Release,
}

impl Action {
    /// Kitty event type number.
    fn code(self) -> u8 {
        match self {
            Action::Press => 1,
            Action::Repeat => 2,
            Action::Release => 3,
        }
    }
}

/// Keys with a legacy (xterm) encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    Escape,
    Enter,
    Tab,
    Backspace,
    Insert,
    Delete,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// F1-F12.
    F(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A text key, identified by its unshifted, lower-case codepoint in the
    /// active layout (`'a'`, `';'`, `'ф'`).
    Char(char),
    Named(Named),
    /// A key with no legacy encoding (F13+, lock keys, media keys, menu);
    /// only reported in kitty mode, as `CSI code u`.
    Functional(u32),
    /// A modifier key itself; only reported with the "report all keys" flag.
    Modifier {
        code: u32,
        bit: Mods,
    },
    /// A numpad key: its kitty code, plus the equivalent navigation key used
    /// in legacy mode when Num Lock is off.
    Keypad {
        code: u32,
        legacy: Option<Named>,
    },
}

/// A platform-independent key event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub key: Key,
    /// Modifier state *after* this event (pressing Shift includes Shift).
    pub mods: Mods,
    pub action: Action,
    /// Text produced by the key, if any.
    pub text: Option<String>,
    /// The character produced with Shift held (kitty "shifted key").
    pub shifted: Option<char>,
    /// The key in the standard US PC-101 layout (kitty "base layout key").
    /// Also used for Ctrl shortcuts in non-Latin layouts.
    pub base: Option<char>,
}

impl KeyInput {
    #[cfg(test)]
    pub fn new(key: Key, mods: Mods, action: Action) -> Self {
        Self {
            key,
            mods,
            action,
            text: None,
            shifted: None,
            base: None,
        }
    }

    pub fn from_winit(event: &KeyEvent, modifiers: ModifiersState) -> Option<Self> {
        let action = match (event.state, event.repeat) {
            (ElementState::Released, _) => Action::Release,
            (ElementState::Pressed, true) => Action::Repeat,
            (ElementState::Pressed, false) => Action::Press,
        };
        let mut mods = Mods::from(modifiers);
        let numpad = event.location == KeyLocation::Numpad;
        let right = event.location == KeyLocation::Right;

        let key = match &event.logical_key {
            WKey::Named(named) => {
                if let Some((left_code, bit)) = modifier_key(*named) {
                    // Report the modifier state after this event.
                    mods = match action {
                        Action::Release => mods.without(bit),
                        _ => mods | bit,
                    };
                    let code = if right && left_code != ISO_LEVEL3_SHIFT {
                        left_code + 6
                    } else {
                        left_code
                    };
                    Key::Modifier { code, bit }
                } else if numpad && let Some(k) = keypad_named(*named) {
                    k
                } else if let Some(n) = legacy_named(*named) {
                    Key::Named(n)
                } else if let Some(code) = functional_code(*named) {
                    Key::Functional(code)
                } else if *named == NamedKey::Space {
                    Key::Char(' ')
                } else {
                    let c = event.text.as_ref()?.chars().next()?;
                    Key::Char(c)
                }
            }
            WKey::Character(s) => {
                let c = s.chars().next()?;
                match numpad.then(|| keypad_char(c)).flatten() {
                    Some(code) => Key::Keypad { code, legacy: None },
                    None => {
                        let unshifted = match event.key_without_modifiers() {
                            WKey::Character(u) => u.chars().next().unwrap_or(c),
                            _ => c,
                        };
                        Key::Char(lowercase(unshifted))
                    }
                }
            }
            _ => return None,
        };

        let shifted = match (&event.logical_key, key) {
            (WKey::Character(s), Key::Char(code)) if mods.shift() => s.chars().next().filter(|&c| c != code),
            _ => None,
        };
        let base = match event.physical_key {
            PhysicalKey::Code(code) => us_layout_char(code),
            PhysicalKey::Unidentified(_) => None,
        };

        Some(Self {
            key,
            mods,
            action,
            // With Ctrl applied (e.g. "\x01" for Ctrl+A), so keys that insert no text
            // never report associated text.
            text: event.text_with_all_modifiers().map(str::to_string),
            shifted,
            base,
        })
    }
}

fn lowercase(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

const ISO_LEVEL3_SHIFT: u32 = 57453;

/// Left-side kitty code and modifier bit for modifier keys.
fn modifier_key(k: NamedKey) -> Option<(u32, Mods)> {
    Some(match k {
        NamedKey::Shift => (57441, Mods::SHIFT),
        NamedKey::Control => (57442, Mods::CTRL),
        NamedKey::Alt => (57443, Mods::ALT),
        NamedKey::Super => (57444, Mods::SUPER),
        NamedKey::Hyper => (57445, Mods::HYPER),
        NamedKey::Meta => (57446, Mods::META),
        NamedKey::AltGraph => (ISO_LEVEL3_SHIFT, Mods::NONE),
        _ => return None,
    })
}

fn legacy_named(k: NamedKey) -> Option<Named> {
    Some(match k {
        NamedKey::Escape => Named::Escape,
        NamedKey::Enter => Named::Enter,
        NamedKey::Tab => Named::Tab,
        NamedKey::Backspace => Named::Backspace,
        NamedKey::Insert => Named::Insert,
        NamedKey::Delete => Named::Delete,
        NamedKey::ArrowLeft => Named::Left,
        NamedKey::ArrowRight => Named::Right,
        NamedKey::ArrowUp => Named::Up,
        NamedKey::ArrowDown => Named::Down,
        NamedKey::PageUp => Named::PageUp,
        NamedKey::PageDown => Named::PageDown,
        NamedKey::Home => Named::Home,
        NamedKey::End => Named::End,
        NamedKey::F1 => Named::F(1),
        NamedKey::F2 => Named::F(2),
        NamedKey::F3 => Named::F(3),
        NamedKey::F4 => Named::F(4),
        NamedKey::F5 => Named::F(5),
        NamedKey::F6 => Named::F(6),
        NamedKey::F7 => Named::F(7),
        NamedKey::F8 => Named::F(8),
        NamedKey::F9 => Named::F(9),
        NamedKey::F10 => Named::F(10),
        NamedKey::F11 => Named::F(11),
        NamedKey::F12 => Named::F(12),
        _ => return None,
    })
}

/// Kitty codes for keys without a legacy encoding.
fn functional_code(k: NamedKey) -> Option<u32> {
    let f = |n: u32| 57376 + n - 13;
    Some(match k {
        NamedKey::CapsLock => 57358,
        NamedKey::ScrollLock => 57359,
        NamedKey::NumLock => 57360,
        NamedKey::PrintScreen => 57361,
        NamedKey::Pause => 57362,
        NamedKey::ContextMenu => 57363,
        NamedKey::F13 => f(13),
        NamedKey::F14 => f(14),
        NamedKey::F15 => f(15),
        NamedKey::F16 => f(16),
        NamedKey::F17 => f(17),
        NamedKey::F18 => f(18),
        NamedKey::F19 => f(19),
        NamedKey::F20 => f(20),
        NamedKey::F21 => f(21),
        NamedKey::F22 => f(22),
        NamedKey::F23 => f(23),
        NamedKey::F24 => f(24),
        NamedKey::F25 => f(25),
        NamedKey::F26 => f(26),
        NamedKey::F27 => f(27),
        NamedKey::F28 => f(28),
        NamedKey::F29 => f(29),
        NamedKey::F30 => f(30),
        NamedKey::F31 => f(31),
        NamedKey::F32 => f(32),
        NamedKey::F33 => f(33),
        NamedKey::F34 => f(34),
        NamedKey::F35 => f(35),
        NamedKey::MediaPlay => 57428,
        NamedKey::MediaPause => 57429,
        NamedKey::MediaPlayPause => 57430,
        NamedKey::MediaStop => 57432,
        NamedKey::MediaFastForward => 57433,
        NamedKey::MediaRewind => 57434,
        NamedKey::MediaTrackNext => 57435,
        NamedKey::MediaTrackPrevious => 57436,
        NamedKey::MediaRecord => 57437,
        NamedKey::AudioVolumeDown => 57438,
        NamedKey::AudioVolumeUp => 57439,
        NamedKey::AudioVolumeMute => 57440,
        _ => return None,
    })
}

/// Numpad navigation keys (Num Lock off) and Enter.
fn keypad_named(k: NamedKey) -> Option<Key> {
    let (code, legacy) = match k {
        NamedKey::Enter => (57414, Some(Named::Enter)),
        NamedKey::ArrowLeft => (57417, Some(Named::Left)),
        NamedKey::ArrowRight => (57418, Some(Named::Right)),
        NamedKey::ArrowUp => (57419, Some(Named::Up)),
        NamedKey::ArrowDown => (57420, Some(Named::Down)),
        NamedKey::PageUp => (57421, Some(Named::PageUp)),
        NamedKey::PageDown => (57422, Some(Named::PageDown)),
        NamedKey::Home => (57423, Some(Named::Home)),
        NamedKey::End => (57424, Some(Named::End)),
        NamedKey::Insert => (57425, Some(Named::Insert)),
        NamedKey::Delete => (57426, Some(Named::Delete)),
        // Numpad 5 with Num Lock off.
        NamedKey::Clear => (57427, None),
        _ => return None,
    };
    Some(Key::Keypad { code, legacy })
}

/// Numpad text keys (Num Lock on).
fn keypad_char(c: char) -> Option<u32> {
    Some(match c {
        '0'..='9' => 57399 + (c as u32 - '0' as u32),
        '.' => 57409,
        '/' => 57410,
        '*' => 57411,
        '-' => 57412,
        '+' => 57413,
        '=' => 57415,
        ',' => 57416,
        _ => return None,
    })
}

/// The character a physical key produces in the US PC-101 layout.
pub fn us_layout_char(code: KeyCode) -> Option<char> {
    use KeyCode::*;
    let letters = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR,
        KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    if let Some(i) = letters.iter().position(|&k| k == code) {
        return Some((b'a' + i as u8) as char);
    }
    let digits = [
        Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
    ];
    if let Some(i) = digits.iter().position(|&k| k == code) {
        return Some((b'0' + i as u8) as char);
    }
    Some(match code {
        Minus => '-',
        Equal => '=',
        BracketLeft => '[',
        BracketRight => ']',
        Backslash => '\\',
        Semicolon => ';',
        Quote => '\'',
        Backquote => '`',
        Comma => ',',
        Period => '.',
        Slash => '/',
        Space => ' ',
        _ => return None,
    })
}

/// Encode a key event for the PTY according to the active terminal modes.
pub fn encode(k: &KeyInput, modes: Modes) -> Option<Vec<u8>> {
    if modes.intersects(Modes::KITTY_ANY) {
        kitty::encode(k, modes)
    } else {
        legacy::encode(k, modes)
    }
}

/// Prepare clipboard text for the PTY, honoring bracketed-paste mode.
pub fn paste_bytes(text: &str, mode: Modes) -> Vec<u8> {
    if mode.contains(Modes::BRACKETED_PASTE) {
        // Strip ESC so pasted text can't terminate the bracket early.
        let clean: String = text.chars().filter(|&c| c != '\x1b').collect();
        format!("\x1b[200~{clean}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Final byte for keys encoded as `CSI 1;mods X`.
fn letter_final(n: Named) -> Option<char> {
    Some(match n {
        Named::Up => 'A',
        Named::Down => 'B',
        Named::Right => 'C',
        Named::Left => 'D',
        Named::End => 'F',
        Named::Home => 'H',
        Named::F(1) => 'P',
        Named::F(2) => 'Q',
        Named::F(4) => 'S',
        _ => return None,
    })
}

/// Number for keys encoded as `CSI number;mods ~`.
fn tilde_number(n: Named) -> Option<u8> {
    Some(match n {
        Named::Insert => 2,
        Named::Delete => 3,
        Named::PageUp => 5,
        Named::PageDown => 6,
        Named::F(3) => 13,
        Named::F(5) => 15,
        Named::F(6) => 17,
        Named::F(7) => 18,
        Named::F(8) => 19,
        Named::F(9) => 20,
        Named::F(10) => 21,
        Named::F(11) => 23,
        Named::F(12) => 24,
        _ => return None,
    })
}

mod legacy {
    //! xterm-style encoding.

    use super::*;

    pub fn encode(k: &KeyInput, modes: Modes) -> Option<Vec<u8>> {
        if k.action == Action::Release {
            return None;
        }
        let m = k.mods;
        match k.key {
            Key::Char(c) => {
                if m.ctrl() {
                    // Fall back to the US layout so Ctrl+C works in e.g. Ukrainian layouts.
                    let b = ctrl_byte(c).or_else(|| k.base.and_then(ctrl_byte))?;
                    return Some(alt_prefix(m, vec![b]));
                }
                let text = k.text.clone().unwrap_or_else(|| c.to_string());
                Some(alt_prefix(m, text.into_bytes()))
            }
            Key::Named(n) => Some(named(n, m, modes)),
            Key::Keypad { legacy: Some(n), .. } if k.text.is_none() => Some(named(n, m, modes)),
            Key::Keypad { .. } => k.text.as_ref().map(|t| alt_prefix(m, t.clone().into_bytes())),
            Key::Functional(_) | Key::Modifier { .. } => None,
        }
    }

    pub fn named(n: Named, m: Mods, modes: Modes) -> Vec<u8> {
        match n {
            // Shift+Enter: ESC CR, which Claude Code treats as a newline when the
            // kitty protocol is not enabled.
            Named::Enter if m.shift() => b"\x1b\r".to_vec(),
            Named::Enter => alt_prefix(m, b"\r".to_vec()),
            Named::Backspace if m.ctrl() => alt_prefix(m, vec![0x08]),
            Named::Backspace => alt_prefix(m, vec![0x7f]),
            Named::Tab if m.shift() => b"\x1b[Z".to_vec(),
            Named::Tab => alt_prefix(m, b"\t".to_vec()),
            Named::Escape => alt_prefix(m, vec![0x1b]),
            Named::F(1..=4) => {
                let letter = match n {
                    Named::F(1) => 'P',
                    Named::F(2) => 'Q',
                    Named::F(3) => 'R',
                    _ => 'S',
                };
                if m.is_empty() {
                    format!("\x1bO{letter}").into_bytes()
                } else {
                    format!("\x1b[1;{}{letter}", m.param()).into_bytes()
                }
            }
            _ => {
                if let Some(letter) = letter_final(n) {
                    if !m.is_empty() {
                        format!("\x1b[1;{}{letter}", m.param()).into_bytes()
                    } else if modes.contains(Modes::APP_CURSOR) {
                        format!("\x1bO{letter}").into_bytes()
                    } else {
                        format!("\x1b[{letter}").into_bytes()
                    }
                } else {
                    let num = tilde_number(n).unwrap_or(0);
                    if m.is_empty() {
                        format!("\x1b[{num}~").into_bytes()
                    } else {
                        format!("\x1b[{num};{}~", m.param()).into_bytes()
                    }
                }
            }
        }
    }

    fn alt_prefix(m: Mods, mut v: Vec<u8>) -> Vec<u8> {
        if m.alt() {
            v.insert(0, 0x1b);
        }
        v
    }

    pub fn ctrl_byte(c: char) -> Option<u8> {
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
}

mod kitty {
    //! Kitty keyboard protocol encoding.

    use super::*;

    struct Flags {
        event_types: bool,
        alternate_keys: bool,
        all_keys: bool,
        text: bool,
    }

    pub fn encode(k: &KeyInput, modes: Modes) -> Option<Vec<u8>> {
        let flags = Flags {
            event_types: modes.contains(Modes::KITTY_EVENT_TYPES),
            alternate_keys: modes.contains(Modes::KITTY_ALTERNATE_KEYS),
            all_keys: modes.contains(Modes::KITTY_ALL_KEYS),
            text: modes.contains(Modes::KITTY_TEXT),
        };
        if k.action == Action::Release && !flags.event_types {
            return None;
        }
        let m = k.mods;
        // Keys that would be sent as plain text / legacy bytes (no release events for these).
        let as_legacy = |bytes: Vec<u8>| (k.action != Action::Release).then_some(bytes);

        match k.key {
            Key::Char(c) => {
                // Text keys stay text unless modified by more than Shift.
                if !flags.all_keys && m.without(Mods::SHIFT).is_empty() {
                    let text = k.text.clone().unwrap_or_else(|| c.to_string());
                    return as_legacy(text.into_bytes());
                }
                Some(csi_u(c as u32, k, &flags))
            }
            Key::Named(n @ (Named::Enter | Named::Tab | Named::Backspace)) => {
                // Kept legacy when unmodified, so a shell stays usable if a program
                // exits without resetting the protocol.
                if !flags.all_keys && m.is_empty() {
                    return as_legacy(legacy::named(n, m, modes));
                }
                let code = match n {
                    Named::Enter => 13,
                    Named::Tab => 9,
                    _ => 127,
                };
                Some(csi_u(code, k, &flags))
            }
            Key::Named(Named::Escape) => Some(csi_u(27, k, &flags)),
            Key::Named(n) => {
                let plain_press = m.is_empty() && (k.action == Action::Press || !flags.event_types);
                if !flags.all_keys && plain_press {
                    return as_legacy(legacy::named(n, m, modes));
                }
                Some(functional_legacy_form(n, k, &flags))
            }
            Key::Functional(code) => Some(csi_u(code, k, &flags)),
            Key::Modifier { code, .. } => flags.all_keys.then(|| csi_u(code, k, &flags)),
            Key::Keypad { code, .. } => {
                let has_text = k.text.as_ref().is_some_and(|t| !t.is_empty());
                if !flags.all_keys && m.without(Mods::SHIFT).is_empty() && has_text {
                    return as_legacy(k.text.clone().unwrap_or_default().into_bytes());
                }
                Some(csi_u(code, k, &flags))
            }
        }
    }

    /// `;mods[:event]` suffix, or empty when both are default.
    fn mods_field(k: &KeyInput, flags: &Flags, force: bool) -> String {
        let event = flags.event_types && k.action != Action::Press;
        let param = k.mods.param();
        if param == 1 && !event && !force {
            return String::new();
        }
        if event {
            format!(";{param}:{}", k.action.code())
        } else {
            format!(";{param}")
        }
    }

    /// `CSI 1;mods X` or `CSI number;mods ~` for keys that keep their legacy shape.
    fn functional_legacy_form(n: Named, k: &KeyInput, flags: &Flags) -> Vec<u8> {
        let mods = mods_field(k, flags, false);
        if let Some(letter) = letter_final(n) {
            if mods.is_empty() {
                format!("\x1b[{letter}").into_bytes()
            } else {
                format!("\x1b[1{mods}{letter}").into_bytes()
            }
        } else {
            let num = tilde_number(n).unwrap_or(0);
            format!("\x1b[{num}{mods}~").into_bytes()
        }
    }

    /// `CSI code[:shifted[:base]] [; mods[:event] [; text]] u`.
    fn csi_u(code: u32, k: &KeyInput, flags: &Flags) -> Vec<u8> {
        let mut out = format!("\x1b[{code}");

        if flags.alternate_keys && matches!(k.key, Key::Char(_)) {
            let shifted = k.shifted.filter(|&s| k.mods.shift() && s as u32 != code);
            let base = k.base.filter(|&b| b as u32 != code);
            match (shifted, base) {
                (Some(s), Some(b)) => out += &format!(":{}:{}", s as u32, b as u32),
                (Some(s), None) => out += &format!(":{}", s as u32),
                (None, Some(b)) => out += &format!("::{}", b as u32),
                (None, None) => {}
            }
        }

        let text = k
            .text
            .as_deref()
            .filter(|_| flags.text && flags.all_keys && k.action != Action::Release)
            .filter(|t| !t.is_empty() && !t.chars().any(char::is_control));

        out += &mods_field(k, flags, text.is_some());
        if let Some(t) = text {
            let cps: Vec<String> = t.chars().map(|c| (c as u32).to_string()).collect();
            out += ";";
            out += &cps.join(":");
        }
        out.push('u');
        out.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIS: Modes = Modes::KITTY_DISAMBIGUATE;

    fn key(key: Key, mods: Mods) -> KeyInput {
        KeyInput::new(key, mods, Action::Press)
    }

    fn ch(c: char, mods: Mods) -> KeyInput {
        let mut k = key(Key::Char(c), mods);
        k.text = (!mods.contains(Mods::CTRL)).then(|| c.to_string());
        k.base = c.is_ascii().then_some(c);
        k
    }

    fn shifted(c: char, s: char, mods: Mods) -> KeyInput {
        let mut k = ch(c, mods | Mods::SHIFT);
        k.text = (!mods.contains(Mods::CTRL)).then(|| s.to_string());
        k.shifted = Some(s);
        k
    }

    fn with(mut k: KeyInput, action: Action) -> KeyInput {
        k.action = action;
        if action == Action::Release {
            k.text = None;
        }
        k
    }

    fn enc(k: &KeyInput, modes: Modes) -> Option<String> {
        encode(k, modes).map(|b| String::from_utf8(b).unwrap())
    }

    fn e(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    // --- legacy ---

    #[test]
    fn legacy_ctrl_letters_and_non_latin_layout() {
        assert_eq!(encode(&ch('c', Mods::CTRL), Modes::empty()), Some(vec![3]));
        assert_eq!(legacy::ctrl_byte('['), Some(0x1b));
        // Ctrl+С on a Ukrainian layout: the physical key is US 'c'.
        let mut k = key(Key::Char('с'), Mods::CTRL);
        k.base = Some('c');
        assert_eq!(encode(&k, Modes::empty()), Some(vec![3]));
        assert_eq!(enc(&ch('x', Mods::ALT), Modes::empty()), e("\x1bx"));
    }

    #[test]
    fn legacy_named_keys() {
        let up = key(Key::Named(Named::Up), Mods::NONE);
        assert_eq!(enc(&up, Modes::empty()), e("\x1b[A"));
        assert_eq!(enc(&up, Modes::APP_CURSOR), e("\x1bOA"));
        assert_eq!(
            enc(&key(Key::Named(Named::Up), Mods::SHIFT | Mods::CTRL), Modes::empty()),
            e("\x1b[1;6A")
        );
        assert_eq!(
            enc(&key(Key::Named(Named::Delete), Mods::ALT), Modes::empty()),
            e("\x1b[3;3~")
        );
        assert_eq!(
            enc(&key(Key::Named(Named::F(3)), Mods::NONE), Modes::empty()),
            e("\x1bOR")
        );
        assert_eq!(
            enc(&key(Key::Named(Named::Enter), Mods::SHIFT), Modes::empty()),
            e("\x1b\r")
        );
    }

    #[test]
    fn legacy_ignores_releases_and_unencodable_keys() {
        assert_eq!(enc(&with(ch('a', Mods::NONE), Action::Release), Modes::empty()), None);
        assert_eq!(enc(&key(Key::Functional(57358), Mods::NONE), Modes::empty()), None);
        let shift = key(
            Key::Modifier {
                code: 57441,
                bit: Mods::SHIFT,
            },
            Mods::SHIFT,
        );
        assert_eq!(enc(&shift, Modes::empty()), None);
    }

    // --- kitty: disambiguate (flag 1) ---

    #[test]
    fn kitty_disambiguate() {
        assert_eq!(enc(&key(Key::Named(Named::Escape), Mods::NONE), DIS), e("\x1b[27u"));
        assert_eq!(enc(&ch('a', Mods::CTRL), DIS), e("\x1b[97;5u"));
        assert_eq!(enc(&ch('a', Mods::ALT), DIS), e("\x1b[97;3u"));
        assert_eq!(enc(&shifted('a', 'A', Mods::ALT), DIS), e("\x1b[97;4u"));
        // Plain and shifted text stays text.
        assert_eq!(enc(&ch('a', Mods::NONE), DIS), e("a"));
        assert_eq!(enc(&shifted('a', 'A', Mods::NONE), DIS), e("A"));
        // Unmodified Enter/Tab/Backspace stay legacy; modified ones are CSI u.
        assert_eq!(enc(&key(Key::Named(Named::Enter), Mods::NONE), DIS), e("\r"));
        assert_eq!(enc(&key(Key::Named(Named::Enter), Mods::SHIFT), DIS), e("\x1b[13;2u"));
        assert_eq!(enc(&key(Key::Named(Named::Enter), Mods::CTRL), DIS), e("\x1b[13;5u"));
        assert_eq!(enc(&key(Key::Named(Named::Tab), Mods::SHIFT), DIS), e("\x1b[9;2u"));
        assert_eq!(
            enc(&key(Key::Named(Named::Backspace), Mods::ALT), DIS),
            e("\x1b[127;3u")
        );
    }

    #[test]
    fn kitty_functional_keys() {
        // Unmodified: legacy shape (honoring DECCKM).
        assert_eq!(enc(&key(Key::Named(Named::Up), Mods::NONE), DIS), e("\x1b[A"));
        assert_eq!(
            enc(&key(Key::Named(Named::Up), Mods::NONE), DIS | Modes::APP_CURSOR),
            e("\x1bOA")
        );
        assert_eq!(enc(&key(Key::Named(Named::Up), Mods::CTRL), DIS), e("\x1b[1;5A"));
        assert_eq!(enc(&key(Key::Named(Named::F(1)), Mods::CTRL), DIS), e("\x1b[1;5P"));
        // F3 uses `13~` (CSI R would clash with cursor position reports).
        assert_eq!(enc(&key(Key::Named(Named::F(3)), Mods::CTRL), DIS), e("\x1b[13;5~"));
        assert_eq!(enc(&key(Key::Named(Named::PageDown), Mods::SHIFT), DIS), e("\x1b[6;2~"));
        // Keys with no legacy encoding.
        assert_eq!(enc(&key(Key::Functional(57376), Mods::NONE), DIS), e("\x1b[57376u"));
        assert_eq!(enc(&key(Key::Functional(57358), Mods::CTRL), DIS), e("\x1b[57358;5u"));
    }

    #[test]
    fn kitty_keypad() {
        let mut kp1 = key(
            Key::Keypad {
                code: 57400,
                legacy: None,
            },
            Mods::NONE,
        );
        kp1.text = Some("1".into());
        assert_eq!(enc(&kp1, DIS), e("1"));
        kp1.mods = Mods::CTRL;
        kp1.text = None;
        assert_eq!(enc(&kp1, DIS), e("\x1b[57400;5u"));
        let kp_up = key(
            Key::Keypad {
                code: 57419,
                legacy: Some(Named::Up),
            },
            Mods::NONE,
        );
        assert_eq!(enc(&kp_up, DIS), e("\x1b[57419u"));
        assert_eq!(enc(&kp_up, Modes::empty()), e("\x1b[A"));
    }

    // --- kitty: event types (flag 2) ---

    #[test]
    fn kitty_event_types() {
        let m = DIS | Modes::KITTY_EVENT_TYPES;
        assert_eq!(enc(&with(ch('a', Mods::CTRL), Action::Release), m), e("\x1b[97;5:3u"));
        assert_eq!(enc(&with(ch('a', Mods::CTRL), Action::Repeat), m), e("\x1b[97;5:2u"));
        // Text keys and unmodified Enter have no release events.
        assert_eq!(enc(&with(ch('a', Mods::NONE), Action::Release), m), None);
        assert_eq!(enc(&with(ch('a', Mods::NONE), Action::Repeat), m), e("a"));
        assert_eq!(
            enc(&with(key(Key::Named(Named::Enter), Mods::NONE), Action::Release), m),
            None
        );
        assert_eq!(
            enc(&with(key(Key::Named(Named::Escape), Mods::NONE), Action::Release), m),
            e("\x1b[27;1:3u")
        );
        assert_eq!(
            enc(&with(key(Key::Named(Named::Up), Mods::NONE), Action::Repeat), m),
            e("\x1b[1;1:2A")
        );
        assert_eq!(
            enc(&with(key(Key::Named(Named::Delete), Mods::NONE), Action::Release), m),
            e("\x1b[3;1:3~")
        );
        // Without the flag, releases are dropped.
        assert_eq!(enc(&with(ch('a', Mods::CTRL), Action::Release), DIS), None);
    }

    // --- kitty: alternate keys (flag 4) ---

    #[test]
    fn kitty_alternate_keys() {
        let m = DIS | Modes::KITTY_ALTERNATE_KEYS;
        assert_eq!(enc(&shifted('a', 'A', Mods::CTRL), m), e("\x1b[97:65;6u"));
        assert_eq!(enc(&shifted(';', ':', Mods::ALT), m), e("\x1b[59:58;4u"));
        // Ukrainian layout: 'ф' on the US 'a' key.
        let mut uk = key(Key::Char('ф'), Mods::CTRL);
        uk.base = Some('a');
        assert_eq!(enc(&uk, m), e("\x1b[1092::97;5u"));
        uk.mods = Mods::CTRL | Mods::SHIFT;
        uk.shifted = Some('Ф');
        assert_eq!(enc(&uk, m), e("\x1b[1092:1060:97;6u"));
        // The base key is omitted when equal to the key itself.
        assert_eq!(enc(&ch('a', Mods::CTRL), m), e("\x1b[97;5u"));
    }

    // --- kitty: report all keys (flag 8) and associated text (flag 16) ---

    #[test]
    fn kitty_report_all_keys() {
        let m = DIS | Modes::KITTY_ALL_KEYS;
        assert_eq!(enc(&ch('a', Mods::NONE), m), e("\x1b[97u"));
        assert_eq!(enc(&shifted('a', 'A', Mods::NONE), m), e("\x1b[97;2u"));
        assert_eq!(enc(&key(Key::Named(Named::Enter), Mods::NONE), m), e("\x1b[13u"));
        assert_eq!(enc(&key(Key::Named(Named::Up), Mods::NONE), m), e("\x1b[A"));
        // Modifier keys are reported, with the state after the event.
        let lshift = key(
            Key::Modifier {
                code: 57441,
                bit: Mods::SHIFT,
            },
            Mods::SHIFT,
        );
        assert_eq!(enc(&lshift, m), e("\x1b[57441;2u"));
        assert_eq!(enc(&lshift, DIS), None);
        let release = with(
            KeyInput {
                mods: Mods::NONE,
                ..lshift
            },
            Action::Release,
        );
        assert_eq!(enc(&release, m | Modes::KITTY_EVENT_TYPES), e("\x1b[57441;1:3u"));
    }

    #[test]
    fn kitty_associated_text() {
        let m = DIS | Modes::KITTY_ALL_KEYS | Modes::KITTY_TEXT;
        assert_eq!(enc(&ch('a', Mods::NONE), m), e("\x1b[97;1;97u"));
        assert_eq!(enc(&shifted('a', 'A', Mods::NONE), m), e("\x1b[97;2;65u"));
        // Control characters are never reported as text.
        assert_eq!(enc(&ch('a', Mods::CTRL), m), e("\x1b[97;5u"));
        // Text requires flag 8.
        assert_eq!(enc(&ch('a', Mods::CTRL), DIS | Modes::KITTY_TEXT), e("\x1b[97;5u"));
    }

    #[test]
    fn paste_is_bracketed_and_sanitized() {
        assert_eq!(
            paste_bytes("a\x1b[201~b", Modes::BRACKETED_PASTE),
            b"\x1b[200~a[201~b\x1b[201~"
        );
        assert_eq!(paste_bytes("a\nb\r\nc", Modes::empty()), b"a\rb\rc");
    }
}
