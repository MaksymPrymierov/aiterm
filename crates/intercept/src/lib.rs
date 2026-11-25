//! Streaming scanner for string-type escape sequences in terminal output.
//!
//! The VT parser (alacritty_terminal) silently ignores many sequences that a
//! modern terminal needs: desktop notifications (OSC 9/99/777), progress
//! (OSC 9;4), shell integration (OSC 7/133), and image protocols (kitty
//! graphics via APC, Sixel via DCS). [`Scanner`] observes the raw PTY byte
//! stream *before* it reaches the parser and reports these sequences.
//!
//! The scanner is an observer: it never modifies the stream. Results do not
//! depend on how the stream is split into chunks.
//!
//! # Agreement with the VT parser
//!
//! OSC and DCS boundaries follow the state machine of the `vte` crate used
//! by alacritty_terminal, so the scanner reports exactly the sequences the
//! parser dispatches (verified by a differential test against `vte`):
//!
//! - **OSC** (`ESC ]`) ends at BEL, ST (`ESC \`), any other ESC, CAN or SUB.
//!   C0 controls inside the string are dropped, as in `vte`. Unlike `vte`,
//!   the payload is not truncated after 16 `;`-separated parameters.
//! - **DCS** (`ESC P`) is reported only if its header reaches a final byte
//!   (when `vte` calls `hook`), and ends at ESC, CAN, SUB or C1 ST (`0x9C`),
//!   where `vte` calls `unhook`. The payload is the header (parameters,
//!   intermediates, final byte) followed by the data bytes `vte` passes on.
//! - **APC** (`ESC _`) is ignored by `vte`; following the kitty graphics
//!   protocol, it is reported only when terminated by ST (`ESC \`). CAN, SUB
//!   or ESC followed by anything else discard it.
//!
//! Only 7-bit introducers are recognized; in a UTF-8 stream, bytes `0x80` to
//! `0x9F` are not C1 controls (as in `vte`).

pub mod notification;
pub mod progress;
pub mod shell;

/// Default upper bound for a single sequence payload (kitty graphics and Sixel
/// payloads can be large).
pub const DEFAULT_MAX_LEN: usize = 32 * 1024 * 1024;

const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const ESC: u8 = 0x1b;
/// C1 String Terminator; only meaningful in DCS data (as in `vte`).
const ST_C1: u8 = 0x9c;

/// A complete string-type escape sequence. The payload excludes the
/// introducer and the terminator.
#[derive(Clone, PartialEq, Eq)]
pub enum Sequence {
    /// Operating System Command: `ESC ] payload (BEL | ESC \)`.
    Osc(Vec<u8>),
    /// Application Program Command: `ESC _ payload ESC \` (kitty graphics).
    Apc(Vec<u8>),
    /// Device Control String: `ESC P header data ESC \` (Sixel, XTGETTCAP).
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
enum State {
    /// Plain text, CSI and other sequences we don't report. From all of
    /// these, only ESC leads anywhere interesting.
    Ground,
    Escape,
    Osc,
    /// DCS header before any parameter or intermediate byte.
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    /// Malformed DCS header: skipped until ESC, CAN or SUB.
    DcsIgnore,
    DcsData,
    Apc,
}

/// Incremental scanner. Feed it every chunk read from the PTY, in order.
#[derive(Debug)]
pub struct Scanner {
    state: State,
    /// An APC saw ESC; it is complete if the next byte is `\`.
    apc_terminating: bool,
    buf: Vec<u8>,
    max_len: usize,
    overflow: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

/// C0 controls that `vte` executes or ignores without leaving the current
/// string or escape state (everything except CAN, SUB and ESC).
fn is_passive_c0(b: u8) -> bool {
    matches!(b, 0x00..=0x17 | 0x19 | 0x1c..=0x1f)
}

impl Scanner {
    pub fn new() -> Self {
        Self::with_max_len(DEFAULT_MAX_LEN)
    }

    /// Sequences whose payload exceeds `max_len` bytes are dropped.
    pub fn with_max_len(max_len: usize) -> Self {
        Self {
            state: State::Ground,
            apc_terminating: false,
            buf: Vec::new(),
            max_len,
            overflow: false,
        }
    }

    /// Scan `bytes`, calling `emit` for every sequence completed in this chunk.
    pub fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(Sequence)) {
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            // Fast paths: skip or copy runs of ordinary bytes.
            let run = match self.state {
                State::Ground => match rest.iter().position(|&b| b == ESC) {
                    Some(p) => {
                        i += p + 1;
                        self.state = State::Escape;
                        continue;
                    }
                    None => return,
                },
                State::Osc => rest.iter().position(|&b| b < 0x20).unwrap_or(rest.len()),
                State::DcsData => rest
                    .iter()
                    .position(|&b| !(0x20..=0x7e).contains(&b))
                    .unwrap_or(rest.len()),
                State::Apc => rest
                    .iter()
                    .position(|&b| matches!(b, ESC | CAN | SUB))
                    .unwrap_or(rest.len()),
                _ => 0,
            };
            if run > 0 {
                self.push(&rest[..run]);
                i += run;
                continue;
            }
            self.step(rest[0], &mut emit);
            i += 1;
        }
    }

    fn step(&mut self, b: u8, emit: &mut impl FnMut(Sequence)) {
        match self.state {
            State::Ground => {
                if b == ESC {
                    self.state = State::Escape;
                }
            }
            State::Escape => self.escape(b, emit),
            State::Osc => match b {
                BEL | CAN | SUB => self.finish(Sequence::Osc, State::Ground, emit),
                ESC => self.finish(Sequence::Osc, State::Escape, emit),
                _ if is_passive_c0(b) => {}
                _ => self.push(&[b]),
            },
            State::DcsEntry | State::DcsParam | State::DcsIntermediate => self.dcs_header(b),
            State::DcsIgnore => match b {
                CAN | SUB => self.state = State::Ground,
                ESC => self.state = State::Escape,
                _ => {}
            },
            State::DcsData => match b {
                CAN | SUB | ST_C1 => self.finish(Sequence::Dcs, State::Ground, emit),
                ESC => self.finish(Sequence::Dcs, State::Escape, emit),
                0x7f..=0xff => {}
                _ => self.push(&[b]),
            },
            State::Apc => match b {
                CAN | SUB => self.state = State::Ground,
                ESC => {
                    self.apc_terminating = true;
                    self.state = State::Escape;
                }
                _ => self.push(&[b]),
            },
        }
    }

    fn escape(&mut self, b: u8, emit: &mut impl FnMut(Sequence)) {
        // A pending APC is complete only if ESC is followed by `\`. Otherwise it
        // is discarded and this byte is handled as part of the new escape sequence.
        if std::mem::take(&mut self.apc_terminating) && b == b'\\' {
            self.finish(Sequence::Apc, State::Ground, emit);
            return;
        }
        self.state = match b {
            b']' => self.begin(State::Osc),
            b'P' => self.begin(State::DcsEntry),
            b'_' => self.begin(State::Apc),
            CAN | SUB => State::Ground,
            // vte stays in the escape state for these.
            ESC | 0x7f | 0x80..=0xff => State::Escape,
            _ if is_passive_c0(b) => State::Escape,
            // CSI, SOS/PM, intermediates and final bytes: none of them can
            // start a reported sequence before the next ESC.
            _ => State::Ground,
        };
    }

    /// DCS header, mirroring vte's DcsEntry/DcsParam/DcsIntermediate states.
    fn dcs_header(&mut self, b: u8) {
        let state = self.state;
        self.state = match (state, b) {
            (_, CAN | SUB) => State::Ground,
            (_, ESC) => State::Escape,
            (_, 0x40..=0x7e) => {
                self.push(&[b]);
                State::DcsData
            }
            (State::DcsIntermediate, 0x30..=0x3f) => State::DcsIgnore,
            (State::DcsParam, 0x3c..=0x3f) => State::DcsIgnore,
            (_, 0x20..=0x2f) => {
                self.push(&[b]);
                State::DcsIntermediate
            }
            (_, 0x30..=0x3f) => {
                self.push(&[b]);
                State::DcsParam
            }
            // Passive C0, DEL and non-ASCII bytes are ignored.
            _ => state,
        };
    }

    fn begin(&mut self, state: State) -> State {
        self.buf.clear();
        self.overflow = false;
        state
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

    fn finish(&mut self, make: fn(Vec<u8>) -> Sequence, next: State, emit: &mut impl FnMut(Sequence)) {
        self.state = next;
        if std::mem::take(&mut self.overflow) {
            return;
        }
        emit(make(std::mem::take(&mut self.buf)));
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

    fn osc(p: &[u8]) -> Sequence {
        Sequence::Osc(p.to_vec())
    }

    #[test]
    fn osc_with_bel_and_st() {
        let out = scan(b"hi\x1b]9;done\x07 and \x1b]777;notify;T;B\x1b\\!");
        assert_eq!(out, vec![osc(b"9;done"), osc(b"777;notify;T;B")]);
        assert_eq!(out[0].osc_code(), Some(9));
        assert_eq!(out[1].osc_code(), Some(777));
        assert_eq!(out[1].osc_args(), Some(&b"notify;T;B"[..]));
    }

    #[test]
    fn osc_ends_like_vte() {
        // ESC without `\`, CAN and SUB all end (and dispatch) an OSC.
        assert_eq!(
            scan(b"\x1b]0;title\x1b]9;msg\x07"),
            vec![osc(b"0;title"), osc(b"9;msg")]
        );
        assert_eq!(scan(b"\x1b]9;a\x18\x1b]9;b\x1a"), vec![osc(b"9;a"), osc(b"9;b")]);
        // C0 controls inside are dropped; DEL and UTF-8 are kept.
        assert_eq!(
            scan("\x1b]9;a\x01\tb\x7fé\x07".as_bytes()),
            vec![osc("9;ab\x7fé".as_bytes())]
        );
    }

    #[test]
    fn escape_state_is_not_left_on_c0() {
        // vte executes BEL and stays in the escape state, so `]` still starts an OSC.
        assert_eq!(scan(b"\x1b\x07]9;x\x07"), vec![osc(b"9;x")]);
        // CAN cancels the escape.
        assert!(scan(b"\x1b\x18]9;x\x07").is_empty());
    }

    #[test]
    fn apc_requires_st() {
        assert_eq!(
            scan(b"\x1b_Ga=T;AA\x07AA\x1b\\"),
            vec![Sequence::Apc(b"Ga=T;AA\x07AA".to_vec())]
        );
        // ESC + anything else discards the APC; the ESC starts a new sequence.
        assert_eq!(scan(b"\x1b_Gbad\x1b]9;x\x07"), vec![osc(b"9;x")]);
        assert!(scan(b"\x1b_Gbad\x18\x1b\\").is_empty());
    }

    #[test]
    fn dcs_header_and_data() {
        assert_eq!(
            scan(b"\x1bP0;1q#0;2;0;0;0\x1b\\"),
            vec![Sequence::Dcs(b"0;1q#0;2;0;0;0".to_vec())]
        );
        // XTGETTCAP: intermediate `+`, final `q`.
        assert_eq!(scan(b"\x1bP+q544e\x1b\\"), vec![Sequence::Dcs(b"+q544e".to_vec())]);
        // C1 ST ends DCS data; DEL is dropped.
        assert_eq!(scan(b"\x1bPqab\x7fc\x9crest"), vec![Sequence::Dcs(b"qabc".to_vec())]);
    }

    #[test]
    fn dcs_without_final_byte_is_not_reported() {
        assert!(scan(b"\x1bP1;2\x1b\\").is_empty());
        // `<` after a parameter makes the header invalid (vte's DcsIgnore).
        assert!(scan(b"\x1bP1<q data\x1b\\").is_empty());
        assert!(scan(b"\x1bP!0q data\x1b\\").is_empty());
    }

    #[test]
    fn ignores_other_escapes() {
        assert!(scan(b"\x1b[1;31mred\x1b[0m\x1b(B\x1b7\x1b8\x1b[?2026h").is_empty());
        // `]` as a CSI final byte or after an escape intermediate is not an OSC.
        assert!(scan(b"\x1b[1]\x1b(]9;x\x07").is_empty());
    }

    #[test]
    fn split_at_every_position() {
        let input: &[u8] = b"a\x1b]133;A\x1b\\b\x1b_Gf=100;QUJD\x1b\\c\x1bPq#1\x1b\\\x1b]9;4;1;50\x07d";
        let expected = scan(input);
        assert_eq!(expected.len(), 4);
        for split in 0..=input.len() {
            let (a, b) = input.split_at(split);
            assert_eq!(scan_chunks(&[a, b]), expected, "split at {split}");
        }
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(scan_chunks(&bytes), expected);
    }

    #[test]
    fn debug_is_readable_and_truncated() {
        assert_eq!(format!("{:?}", osc(b"9;4;1;42")), r#"Osc("9;4;1;42")"#);
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
        scanner.feed(b"\x1b]9;too long\x07\x1b_Gtoolong\x1b\\\x1b]9;ok\x07", |s| out.push(s));
        assert_eq!(out, vec![osc(b"9;ok")]);
    }
}
