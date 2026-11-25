//! Progress reporting: `OSC 9 ; 4 ; state ; progress ST` (ConEmu, Windows
//! Terminal, Ghostty, iTerm2).
//!
//! | state | meaning                                 |
//! |-------|-----------------------------------------|
//! | 0     | remove the progress indicator           |
//! | 1     | normal progress, `progress` is 0..=100  |
//! | 2     | error state, optional progress          |
//! | 3     | indeterminate                           |
//! | 4     | paused / warning, optional progress     |

use crate::Sequence;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    Remove,
    /// Percentage, clamped to 0..=100.
    Set(u8),
    /// The task failed; `None` keeps the previous percentage.
    Error(Option<u8>),
    Indeterminate,
    /// The task is paused; `None` keeps the previous percentage.
    Paused(Option<u8>),
}

/// Parse an `OSC 9;4` progress report.
pub fn parse_progress(seq: &Sequence) -> Option<Progress> {
    if seq.osc_code()? != 9 {
        return None;
    }
    let args = std::str::from_utf8(seq.osc_args()?).ok()?;
    let mut parts = args.split(';');
    if parts.next()? != "4" {
        return None;
    }
    let state = match parts.next() {
        None | Some("") => 0,
        Some(s) => s.parse::<u8>().ok()?,
    };
    let value = parts
        .next()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u32>().ok())
        .map(|v| v.min(100) as u8);
    Some(match state {
        0 => Progress::Remove,
        1 => Progress::Set(value.unwrap_or(0)),
        2 => Progress::Error(value.filter(|&v| v > 0)),
        3 => Progress::Indeterminate,
        4 => Progress::Paused(value.filter(|&v| v > 0)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Option<Progress> {
        parse_progress(&Sequence::Osc(s.as_bytes().to_vec()))
    }

    #[test]
    fn states() {
        assert_eq!(parse("9;4;1;42"), Some(Progress::Set(42)));
        assert_eq!(parse("9;4;0;0"), Some(Progress::Remove));
        assert_eq!(parse("9;4;0"), Some(Progress::Remove));
        assert_eq!(parse("9;4"), Some(Progress::Remove));
        assert_eq!(parse("9;4;3"), Some(Progress::Indeterminate));
        assert_eq!(parse("9;4;2;80"), Some(Progress::Error(Some(80))));
        assert_eq!(parse("9;4;4"), Some(Progress::Paused(None)));
    }

    #[test]
    fn values_are_clamped_and_defaulted() {
        assert_eq!(parse("9;4;1;250"), Some(Progress::Set(100)));
        assert_eq!(parse("9;4;1"), Some(Progress::Set(0)));
        assert_eq!(parse("9;4;1;x"), Some(Progress::Set(0)));
        // Windows Terminal: error/paused with 0 keep the previous value.
        assert_eq!(parse("9;4;2;0"), Some(Progress::Error(None)));
    }

    #[test]
    fn not_progress() {
        assert_eq!(parse("9;4;7;1"), None);
        assert_eq!(parse("9;Build finished"), None);
        assert_eq!(parse("9;9;/home"), None);
        assert_eq!(parse("777;notify;a;b"), None);
        assert_eq!(parse_progress(&Sequence::Apc(b"9;4;1;1".to_vec())), None);
    }
}
