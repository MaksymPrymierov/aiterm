//! Frame pacing statistics, logged once per second at debug level:
//! `RUST_LOG=aiterm::perf=debug`.

use std::time::{Duration, Instant};

pub struct FrameStats {
    window_start: Instant,
    frames: u32,
    wakeups: u32,
    snapshot: Duration,
    render: Duration,
    max_frame: Duration,
}

impl Default for FrameStats {
    fn default() -> Self {
        Self {
            window_start: Instant::now(),
            frames: 0,
            wakeups: 0,
            snapshot: Duration::ZERO,
            render: Duration::ZERO,
            max_frame: Duration::ZERO,
        }
    }
}

impl FrameStats {
    pub fn wakeup(&mut self) {
        self.wakeups += 1;
    }

    pub fn frame(&mut self, snapshot: Duration, render: Duration) {
        self.frames += 1;
        self.snapshot += snapshot;
        self.render += render;
        self.max_frame = self.max_frame.max(snapshot + render);
        self.maybe_report();
    }

    fn maybe_report(&mut self) {
        let elapsed = self.window_start.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        if log::log_enabled!(target: "aiterm::perf", log::Level::Debug) {
            let n = self.frames.max(1);
            log::debug!(
                target: "aiterm::perf",
                "{:.1} fps, {} wakeups/s, avg snapshot {:.2} ms, avg render {:.2} ms, max frame {:.2} ms",
                self.frames as f64 / elapsed.as_secs_f64(),
                (self.wakeups as f64 / elapsed.as_secs_f64()).round(),
                self.snapshot.as_secs_f64() * 1e3 / n as f64,
                self.render.as_secs_f64() * 1e3 / n as f64,
                self.max_frame.as_secs_f64() * 1e3,
            );
        }
        *self = Self::default();
    }
}
