//! Synchronized output (DEC private mode 2026) end to end: a real PTY, the
//! interceptor tap, the VT parser and `Session::snapshot`.
//!
//! Between `CSI ? 2026 h` (BSU) and `CSI ? 2026 l` (ESU) the parser buffers
//! output, so a snapshot must never show a half-drawn ("torn") frame.
//! Applications such as Claude Code rely on this to avoid flicker.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use aiterm_vt::{GridSize, Session, Theme, VtEvent};

const SIZE: GridSize = GridSize {
    cols: 80,
    rows: 24,
    cell_w: 8.0,
    cell_h: 16.0,
};

fn spawn(script: &str) -> (Session, mpsc::Receiver<VtEvent>) {
    let (tx, rx) = mpsc::channel();
    let command = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
    let session = Session::spawn(SIZE, Some(command), move |e| {
        let _ = tx.send(e);
    })
    .expect("spawn sh");
    (session, rx)
}

fn screen_text(session: &Session) -> String {
    let theme = Theme::default();
    let frame = session.snapshot(&theme, &theme.palette256(), true);
    (0..frame.rows)
        .map(|r| frame.row(r).iter().map(|c| c.c).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_for(session: &Session, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if screen_text(session).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

struct Observation {
    /// A snapshot showed the first half of the frame without the second.
    torn: bool,
    /// The complete frame was eventually shown.
    completed: bool,
}

/// Draws a frame in two halves with a pause in between (shorter than the
/// 150 ms sync timeout), optionally wrapped in BSU/ESU, while sampling
/// snapshots every millisecond.
fn draw_split_frame(synchronized: bool) -> Observation {
    let (bsu, esu) = if synchronized {
        (r"\033[?2026h", r"\033[?2026l")
    } else {
        ("", "")
    };
    let script = format!(
        r"printf 'READY\n'; read _; printf '{bsu}\033[2J\033[HFIRST-HALF'; sleep 0.08; printf 'SECOND-HALF{esu}'; sleep 2"
    );
    let (session, _events) = spawn(&script);
    assert!(
        wait_for(&session, "READY", Duration::from_secs(5)),
        "shell did not start"
    );

    session.input(&b"\n"[..]);
    let mut obs = Observation {
        torn: false,
        completed: false,
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let text = screen_text(&session);
        let first = text.contains("FIRST-HALF");
        let second = text.contains("SECOND-HALF");
        obs.torn |= first && !second;
        if first && second {
            obs.completed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    obs
}

#[test]
fn synchronized_frame_is_never_torn() {
    let obs = draw_split_frame(true);
    assert!(obs.completed, "the synchronized frame was never shown");
    assert!(!obs.torn, "a snapshot showed a half-drawn synchronized frame");
}

/// Control experiment: proves the sampling above is able to observe tearing,
/// so the previous test is meaningful.
#[test]
fn unsynchronized_frame_is_observably_torn() {
    let obs = draw_split_frame(false);
    assert!(obs.completed, "the frame was never shown");
    assert!(obs.torn, "sampling failed to observe the intermediate state");
}

/// An application that starts a synchronized update and never ends it must
/// not freeze the screen: the parser gives up after its timeout (150 ms).
#[test]
fn unterminated_sync_times_out() {
    let (session, _events) = spawn(r"printf '\033[?2026hSTUCK'; sleep 3");
    let start = Instant::now();
    assert!(
        wait_for(&session, "STUCK", Duration::from_secs(2)),
        "unterminated synchronized update froze the screen"
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}

/// Applications detect support with DECRQM (`CSI ? 2026 $ p`); the reply
/// `CSI ? 2026 ; 2 $ y` means "supported, currently reset".
#[test]
fn decrqm_reports_mode_2026_as_supported() {
    let (_session, events) = spawn(r"printf '\033[?2026$p'; sleep 2");
    let deadline = Instant::now() + Duration::from_secs(3);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(VtEvent::PtyWrite(reply)) if reply.contains("?2026;") => {
                assert_eq!(reply, "\x1b[?2026;2$y");
                return;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    panic!("no DECRQM reply for mode 2026");
}
