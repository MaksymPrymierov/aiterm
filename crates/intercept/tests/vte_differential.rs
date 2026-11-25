//! Differential test: on random byte streams, the scanner must report exactly
//! the OSC and DCS sequences that the `vte` parser (used by
//! alacritty_terminal) dispatches, in the same order, regardless of how the
//! stream is chunked.

use aiterm_intercept::{Scanner, Sequence};
use vte::{Params, Parser, Perform};

/// `vte` keeps at most this many OSC parameters; longer OSCs are compared by
/// their first parameters only.
const VTE_MAX_OSC_PARAMS: usize = 16;

#[derive(Debug, PartialEq, Eq, Clone)]
enum Seen {
    /// Joined OSC parameters; `truncated` if vte dropped parameters.
    Osc { payload: Vec<u8>, truncated: bool },
    /// DCS final byte followed by data bytes.
    Dcs { action: u8, data: Vec<u8> },
}

#[derive(Default)]
struct Recorder {
    seen: Vec<Seen>,
    dcs: Option<(u8, Vec<u8>)>,
}

impl Perform for Recorder {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.seen.push(Seen::Osc {
            payload: params.join(&b';'),
            truncated: params.len() >= VTE_MAX_OSC_PARAMS,
        });
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        self.dcs = Some((action as u8, Vec::new()));
    }

    fn put(&mut self, byte: u8) {
        if let Some((_, data)) = self.dcs.as_mut() {
            data.push(byte);
        }
    }

    fn unhook(&mut self) {
        if let Some((action, data)) = self.dcs.take() {
            self.seen.push(Seen::Dcs { action, data });
        }
    }
}

/// Deterministic xorshift PRNG (no extra dependencies).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Random streams biased towards bytes that matter to escape parsing.
fn random_stream(rng: &mut Rng) -> Vec<u8> {
    const INTERESTING: &[u8] = &[
        0x1b, 0x1b, 0x1b, b']', b']', b'P', b'P', b'_', b'\\', b'\\', 0x07, 0x18, 0x1a, b';', b';', b'0', b'1', b'9',
        b'q', b'#', b'+', b'!', b'<', b'[', b'X', b'^', b'a', b' ', 0x01, 0x0a, 0x7f, 0x9c, 0xc3, 0xa9,
    ];
    let len = rng.below(48) + 1;
    (0..len)
        .map(|_| {
            if rng.below(8) == 0 {
                rng.next() as u8
            } else {
                INTERESTING[rng.below(INTERESTING.len())]
            }
        })
        .collect()
}

fn vte_sequences(input: &[u8]) -> Vec<Seen> {
    let mut parser: Parser = Parser::new();
    let mut rec = Recorder::default();
    parser.advance(&mut rec, input);
    rec.seen
}

fn scanner_sequences(input: &[u8], rng: &mut Rng) -> Vec<Sequence> {
    let mut scanner = Scanner::new();
    let mut out = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        let n = rng.below(rest.len()) + 1;
        let (chunk, tail) = rest.split_at(n);
        scanner.feed(chunk, |s| out.push(s));
        rest = tail;
    }
    out
}

/// Split a scanner DCS payload into (header, final byte, data).
fn split_dcs(payload: &[u8]) -> (&[u8], u8, &[u8]) {
    let fin = payload
        .iter()
        .position(|b| (0x40..=0x7e).contains(b))
        .expect("DCS payload has a final byte");
    (&payload[..fin], payload[fin], &payload[fin + 1..])
}

fn check(input: &[u8], rng: &mut Rng) {
    let expected = vte_sequences(input);
    let actual: Vec<Sequence> = scanner_sequences(input, rng)
        .into_iter()
        .filter(|s| !matches!(s, Sequence::Apc(_)))
        .collect();
    assert_eq!(
        actual.len(),
        expected.len(),
        "sequence count differs for {input:?}\n  vte:     {expected:?}\n  scanner: {actual:?}"
    );
    for (a, e) in actual.iter().zip(&expected) {
        match (a, e) {
            (Sequence::Osc(p), Seen::Osc { payload, truncated }) => {
                if *truncated {
                    assert!(p.starts_with(payload), "OSC prefix differs for {input:?}");
                } else {
                    assert_eq!(p, payload, "OSC payload differs for {input:?}");
                }
            }
            (Sequence::Dcs(p), Seen::Dcs { action, data }) => {
                let (header, fin, rest) = split_dcs(p);
                assert!(
                    header.iter().all(|b| (0x20..=0x3f).contains(b)),
                    "bad DCS header for {input:?}"
                );
                assert_eq!((fin, rest), (*action, data.as_slice()), "DCS differs for {input:?}");
            }
            _ => panic!("sequence kind differs for {input:?}\n  vte: {e:?}\n  scanner: {a:?}"),
        }
    }
}

#[test]
fn agrees_with_vte_on_random_streams() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..300_000 {
        let input = random_stream(&mut rng);
        check(&input, &mut rng);
    }
}

#[test]
fn agrees_with_vte_on_realistic_sequences() {
    let mut rng = Rng(42);
    let cases: &[&[u8]] = &[
        b"\x1b]0;title\x07\x1b]9;Claude is waiting\x1b\\\x1b]9;4;1;50\x07",
        b"\x1b]133;A\x1b\\$ ls\r\n\x1b]133;C\x1b\\out\r\n\x1b]133;D;0\x1b\\",
        b"\x1b]7;file://host/home/user\x07",
        b"\x1b]777;notify;Title;Body\x1b\\",
        b"\x1bP0;1;0q\"1;1;8;8#0;2;100;0;0#0~~~~~~~~-\x1b\\",
        b"\x1bP+q544e;636f6c6f7273\x1b\\",
        b"\x1b]1;a;b;c;d;e;f;g;h;i;j;k;l;m;n;o;p;q;r;s\x07",
        b"\x1b[?2026h\x1b[2J\x1b]2;x\x07\x1b[?2026l",
    ];
    for c in cases {
        check(c, &mut rng);
    }
}
