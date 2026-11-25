//! `VtEvent::Wakeup` is coalesced: after one wakeup, no further wakeups are
//! sent until the frame is captured with `Session::snapshot`, no matter how
//! much output is parsed in between.

use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use aiterm_vt::{GridSize, Session, Theme, VtEvent};

fn wakeups(events: &Receiver<VtEvent>, wait: Duration) -> usize {
    let mut n = 0;
    while let Ok(e) = events.recv_timeout(wait) {
        if matches!(e, VtEvent::Wakeup) {
            n += 1;
        }
    }
    n
}

#[test]
fn wakeups_are_coalesced_until_snapshot() {
    let (tx, events) = mpsc::channel();
    let size = GridSize {
        cols: 80,
        rows: 24,
        cell_w: 8.0,
        cell_h: 16.0,
    };
    // Prints a line, then a flood of output, then one more line, each step
    // triggered by a newline from the test.
    let script = "printf 'READY\\n'; read _; seq 1 200000; read _; printf 'MORE'; sleep 2";
    let command = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
    let session = Session::spawn(size, Some(command), move |e| {
        let _ = tx.send(e);
    })
    .expect("spawn sh");

    // First output: exactly one wakeup is delivered.
    assert_eq!(wakeups(&events, Duration::from_millis(500)), 1);

    // ~1.3 MB of output parsed in many chunks, but no snapshot taken:
    // the pending wakeup suppresses all further ones.
    session.input(&b"\n"[..]);
    assert_eq!(wakeups(&events, Duration::from_millis(500)), 0);

    // A snapshot re-arms wakeups.
    let theme = Theme::default();
    session.snapshot(&theme, &theme.palette256(), true);
    session.input(&b"\n"[..]);
    assert_eq!(wakeups(&events, Duration::from_millis(500)), 1);
}
