//! Per-session progress state from `OSC 9;4` reports.
//!
//! A report that is not refreshed for [`STALE_AFTER`] is dropped, so a
//! program that crashes mid-task doesn't leave a progress bar behind
//! (same policy as Ghostty).

use std::time::{Duration, Instant};

use aiterm_vt::progress::Progress;

pub const STALE_AFTER: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Normal,
    Error,
    Paused,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shown {
    pub kind: Kind,
    /// 0.0..=1.0; `None` for indeterminate.
    pub fraction: Option<f32>,
}

#[derive(Clone, Copy, Debug)]
struct Active {
    kind: Kind,
    percent: u8,
    updated: Instant,
}

#[derive(Debug, Default)]
pub struct SessionProgress {
    active: Option<Active>,
}

impl SessionProgress {
    pub fn apply(&mut self, report: Progress, now: Instant) {
        let previous = self.active.map(|a| a.percent);
        let (kind, percent) = match report {
            Progress::Remove => {
                self.active = None;
                return;
            }
            Progress::Set(p) => (Kind::Normal, p),
            Progress::Indeterminate => (Kind::Indeterminate, previous.unwrap_or(0)),
            // Without a value, keep the last one; with none at all, fill the
            // bar so the state is still visible.
            Progress::Error(p) => (Kind::Error, p.or(previous).unwrap_or(100)),
            Progress::Paused(p) => (Kind::Paused, p.or(previous).unwrap_or(100)),
        };
        self.active = Some(Active {
            kind,
            percent,
            updated: now,
        });
    }

    pub fn shown(&self) -> Option<Shown> {
        self.active.map(|a| Shown {
            kind: a.kind,
            fraction: (a.kind != Kind::Indeterminate).then(|| a.percent as f32 / 100.0),
        })
    }

    /// When the current report goes stale.
    pub fn deadline(&self) -> Option<Instant> {
        self.active.map(|a| a.updated + STALE_AFTER)
    }

    /// Drop a stale report; returns true if the progress state changed.
    pub fn expire(&mut self, now: Instant) -> bool {
        if self.deadline().is_some_and(|d| now >= d) {
            self.active = None;
            return true;
        }
        false
    }

    pub fn is_animated(&self) -> bool {
        self.active.is_some_and(|a| a.kind == Kind::Indeterminate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(p: &SessionProgress) -> Option<(Kind, Option<f32>)> {
        p.shown().map(|s| (s.kind, s.fraction))
    }

    #[test]
    fn set_error_paused_remove() {
        let now = Instant::now();
        let mut p = SessionProgress::default();
        assert_eq!(shown(&p), None);
        p.apply(Progress::Set(40), now);
        assert_eq!(shown(&p), Some((Kind::Normal, Some(0.4))));
        // Error without a value keeps the last percentage.
        p.apply(Progress::Error(None), now);
        assert_eq!(shown(&p), Some((Kind::Error, Some(0.4))));
        p.apply(Progress::Paused(Some(70)), now);
        assert_eq!(shown(&p), Some((Kind::Paused, Some(0.7))));
        p.apply(Progress::Remove, now);
        assert_eq!(shown(&p), None);
        // Error with no history fills the bar.
        p.apply(Progress::Error(None), now);
        assert_eq!(shown(&p), Some((Kind::Error, Some(1.0))));
    }

    #[test]
    fn indeterminate_is_animated() {
        let mut p = SessionProgress::default();
        p.apply(Progress::Indeterminate, Instant::now());
        assert_eq!(shown(&p), Some((Kind::Indeterminate, None)));
        assert!(p.is_animated());
        p.apply(Progress::Set(10), Instant::now());
        assert!(!p.is_animated());
    }

    #[test]
    fn stale_reports_expire() {
        let start = Instant::now();
        let mut p = SessionProgress::default();
        p.apply(Progress::Set(50), start);
        assert_eq!(p.deadline(), Some(start + STALE_AFTER));
        assert!(!p.expire(start + STALE_AFTER - Duration::from_millis(1)));
        // A refresh moves the deadline.
        p.apply(Progress::Set(60), start + Duration::from_secs(10));
        assert!(!p.expire(start + STALE_AFTER));
        assert!(p.expire(start + Duration::from_secs(10) + STALE_AFTER));
        assert_eq!(shown(&p), None);
        assert_eq!(p.deadline(), None);
    }
}
