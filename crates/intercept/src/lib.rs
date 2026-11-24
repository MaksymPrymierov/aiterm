//! Streaming scanner for string-type escape sequences in terminal output.
//!
//! The VT parser (alacritty_terminal) silently ignores many sequences that a
//! modern terminal needs: desktop notifications (OSC 9/99/777), progress
//! (OSC 9;4), shell integration (OSC 7/133), and image protocols (kitty
//! graphics via APC, Sixel via DCS). [`Scanner`] observes the raw PTY byte
//! stream *before* it reaches the parser and reports these sequences.
//!
//! The scanner is an observer: it never modifies the stream. It is resilient
//! to sequences split across arbitrary read boundaries.
//!
//! Only 7-bit introducers are recognized (`ESC ]`, `ESC _`, `ESC P`); 8-bit C1
//! controls are not used by UTF-8 applications.

/// Default upper bound for a single sequence payload (kitty graphics and Sixel
/// payloads can be large).
pub const DEFAULT_MAX_LEN: usize = 32 * 1024 * 1024;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// A complete string-type escape sequence. The payload excludes the
/// introducer and the terminator.
#[derive(Clone, PartialEq, Eq)]
pub enum Sequence {
    /// Operating System Command: `ESC ] payload (BEL | ESC \)`.
    Osc(Vec<u8>),
    /// Application Program Command: `ESC _ payload ESC \` (kitty graphics).
    Apc(Vec<u8>),
    /// Device Control String: `ESC P payload ESC \` (Sixel, XTGETTCAP).
    Dcs(Vec<u8>),
}

impl Sequence {
    pub fn payload(&self) -> &[u8] {
        match self {
            Sequence::Osc(p) | Sequence::Apc(p) | Sequence::Dcs(p) => p,
        }
    }

    /// Numeric OSC code (`9` for `OSC 9;msg`, `133` for `OSC 133;A`).
    pub fn osc_code(&self) -> Option<u32> {
        let Sequence::Osc(p) = self else { return None };
        let end = p.iter().position(|&b| b == b';').unwrap_or(p.len());
        std::str::from_utf8(&p[..end]).ok()?.parse().ok()
    }

    /// OSC arguments after the code, e.g. `b"4;1;50"` for `OSC 9;4;1;50`.
    pub fn osc_args(&self) -> Option<&[u8]> {
        let Sequence::Osc(p) = self else { return None };
        let start = p.iter().position(|&b| b == b';')? + 1;
        Some(&p[start..])
    }
}

/// Shows the payload as lossy UTF-8, truncated, e.g. `Osc("9;4;1;42")`.
impl std::fmt::Debug for Sequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const MAX: usize = 64;
        let name = match self {
            Sequence::Osc(_) => "Osc",
            Sequence::Apc(_) => "Apc",
            Sequence::Dcs(_) => "Dcs",
        };
        let p = self.payload();
        let text = String::from_utf8_lossy(&p[..p.len().min(MAX)]);
        if p.len() > MAX {
            write!(f, "{name}({text:?}... {} bytes)", p.len())
        } else {
            write!(f, "{name}({text:?})")
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Osc,
    Apc,
    Dcs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    String(Kind),
    /// Saw ESC inside a string: `\` completes ST, anything else ends the
    /// string and starts a new escape sequence.
    StringEscape(Kind),
}

/// Incremental scanner. Feed it every chunk read from the PTY, in order.
#[derive(Debug)]
pub struct Scanner {
    state: State,
    buf: Vec<u8>,
    max_len: usize,
    overflow: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self::with_max_len(DEFAULT_MAX_LEN)
    }

    /// Sequences whose payload exceeds `max_len` bytes are dropped.
    pub fn with_max_len(max_len: usize) -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            max_len,
            overflow: false,
        }
    }

    /// Scan `bytes`, calling `emit` for every sequence completed in this chunk.
    pub fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(Sequence)) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => {
                    // Fast path: skip to the next ESC.
                    match bytes[i..].iter().position(|&b| b == ESC) {
                        Some(p) => {
                            i += p + 1;
                            self.state = State::Escape;
                        }
                        None => return,
                    }
                    continue;
                }
                State::Escape => {
                    self.state = match bytes[i] {
                        b']' => self.begin(Kind::Osc),
                        b'_' => self.begin(Kind::Apc),
                        b'P' => self.begin(Kind::Dcs),
                        ESC => State::Escape,
                        _ => State::Ground,
                    };
                }
                State::String(kind) => {
                    // Fast path: copy payload up to the next control byte of interest.
                    let rest = &bytes[i..];
                    let stop = rest
                        .iter()
                        .position(|&b| b == ESC || b == BEL || b == CAN || b == SUB)
                        .unwrap_or(rest.len());
                    self.push(&rest[..stop]);
                    i += stop;
                    if i == bytes.len() {
                        return;
                    }
                    match bytes[i] {
                        ESC => self.state = State::StringEscape(kind),
                        BEL if kind == Kind::Osc => {
                            self.finish(kind, &mut emit);
                            self.state = State::Ground;
                        }
                        // BEL is payload data in APC/DCS.
                        BEL => self.push(&[BEL]),
                        // CAN / SUB abort the sequence.
                        _ => self.state = State::Ground,
                    }
                }
                State::StringEscape(kind) => {
                    if bytes[i] == b'\\' {
                        self.finish(kind, &mut emit);
                        self.state = State::Ground;
                    } else {
                        // ESC without `\`: the string ends here (as in vte) and
                        // this byte belongs to a new escape sequence.
                        self.finish(kind, &mut emit);
                        self.state = State::Escape;
                        continue;
                    }
                }
            }
            i += 1;
        }
    }

    fn begin(&mut self, kind: Kind) -> State {
        self.buf.clear();
        self.overflow = false;
        State::String(kind)
    }

    fn push(&mut self, data: &[u8]) {
        if self.overflow {
            return;
        }
        if self.buf.len() + data.len() > self.max_len {
            self.overflow = true;
            self.buf = Vec::new();
            return;
        }
        self.buf.extend_from_slice(data);
    }

    fn finish(&mut self, kind: Kind, emit: &mut impl FnMut(Sequence)) {
        if self.overflow {
            self.overflow = false;
            return;
        }
        let payload = std::mem::take(&mut self.buf);
        emit(match kind {
            Kind::Osc => Sequence::Osc(payload),
            Kind::Apc => Sequence::Apc(payload),
            Kind::Dcs => Sequence::Dcs(payload),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_chunks(chunks: &[&[u8]]) -> Vec<Sequence> {
        let mut scanner = Scanner::new();
        let mut out = Vec::new();
        for c in chunks {
            scanner.feed(c, |s| out.push(s));
        }
        out
    }

    fn scan(bytes: &[u8]) -> Vec<Sequence> {
        scan_chunks(&[bytes])
    }

    #[test]
    fn osc_with_bel_and_st() {
        let out = scan(b"hi\x1b]9;done\x07 and \x1b]777;notify;T;B\x1b\\!");
        assert_eq!(
            out,
            vec![
                Sequence::Osc(b"9;done".to_vec()),
                Sequence::Osc(b"777;notify;T;B".to_vec())
            ]
        );
        assert_eq!(out[0].osc_code(), Some(9));
        assert_eq!(out[1].osc_code(), Some(777));
        assert_eq!(out[1].osc_args(), Some(&b"notify;T;B"[..]));
    }

    #[test]
    fn apc_and_dcs_keep_bel_as_data() {
        let out = scan(b"\x1b_Ga=T;AA\x07AA\x1b\\\x1bPq#0;2;0;0;0\x1b\\");
        assert_eq!(
            out,
            vec![
                Sequence::Apc(b"Ga=T;AA\x07AA".to_vec()),
                Sequence::Dcs(b"q#0;2;0;0;0".to_vec())
            ]
        );
        assert_eq!(out[0].osc_code(), None);
    }

    #[test]
    fn ignores_other_escapes() {
        assert!(scan(b"\x1b[1;31mred\x1b[0m\x1b(B\x1b7\x1b8").is_empty());
    }

    #[test]
    fn can_aborts() {
        assert!(scan(b"\x1b]9;nope\x18\x1b]9;").is_empty());
    }

    #[test]
    fn esc_without_backslash_ends_string_and_starts_new_escape() {
        let out = scan(b"\x1b]0;title\x1b]9;msg\x07");
        assert_eq!(
            out,
            vec![Sequence::Osc(b"0;title".to_vec()), Sequence::Osc(b"9;msg".to_vec())]
        );
    }

    #[test]
    fn split_at_every_position() {
        let input: &[u8] = b"a\x1b]133;A\x1b\\b\x1b_Gf=100;QUJD\x1b\\c\x1b]9;4;1;50\x07d";
        let expected = scan(input);
        assert_eq!(expected.len(), 3);
        for split in 0..=input.len() {
            let (a, b) = input.split_at(split);
            assert_eq!(scan_chunks(&[a, b]), expected, "split at {split}");
        }
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(scan_chunks(&bytes), expected);
    }

    #[test]
    fn debug_is_readable_and_truncated() {
        assert_eq!(
            format!("{:?}", Sequence::Osc(b"9;4;1;42".to_vec())),
            r#"Osc("9;4;1;42")"#
        );
        let long = format!("{:?}", Sequence::Apc(vec![b'A'; 100]));
        assert!(
            long.starts_with("Apc(\"AAAA") && long.ends_with("... 100 bytes)"),
            "{long}"
        );
    }

    #[test]
    fn oversized_sequence_is_dropped_and_scanner_recovers() {
        let mut scanner = Scanner::with_max_len(4);
        let mut out = Vec::new();
        scanner.feed(b"\x1b]9;too long\x07\x1b]9;ok\x07", |s| out.push(s));
        assert_eq!(out, vec![Sequence::Osc(b"9;ok".to_vec())]);
    }
}
