//! Prompt marks (OSC 133) survive scrolling into history and drive prompt
//! navigation; OSC 8 hyperlinks are exposed per cell. Real PTY, public API.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use aiterm_vt::{Frame, GridSize, Session, Theme};

const SIZE: GridSize = GridSize {
    cols: 80,
    rows: 24,
    cell_w: 8.0,
    cell_h: 16.0,
};

fn snapshot(session: &Session) -> Frame {
    let theme = Theme::default();
    session.snapshot(&theme, &theme.palette256(), true)
}

fn row_text(frame: &Frame, r: usize) -> String {
    frame
        .row(r)
        .iter()
        .map(|c| c.c)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn wait_for(session: &Session, needle: &str) -> Frame {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let frame = snapshot(session);
        if (0..frame.rows).any(|r| row_text(&frame, r).contains(needle)) {
            return frame;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {needle:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn prompt_marks_and_hyperlinks() {
    let (tx, _events) = mpsc::channel();
    let script = r"printf 'PROMPT1$ '; read _; seq 1 100; printf 'PROMPT2$ '; read _; printf '\033]8;;https://example.com/docs\033\\LINK\033]8;;\033\\ plain\n'; sleep 3";
    let command = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
    let session = Session::spawn(SIZE, Some(command), move |e| {
        let _ = tx.send(e);
    })
    .expect("spawn sh");

    // The shell waits for input: mark the prompt, as the app does on OSC 133;B.
    wait_for(&session, "PROMPT1$");
    session.mark_prompt();
    assert_eq!(snapshot(&session).prompt_rows, vec![0]);

    session.input(&b"\n"[..]);
    let frame = wait_for(&session, "PROMPT2$");
    session.mark_prompt();
    let prompt2_row = (0..frame.rows)
        .find(|&r| row_text(&frame, r).starts_with("PROMPT2$"))
        .unwrap();
    // PROMPT1 scrolled into history; only PROMPT2 is visible.
    assert_eq!(snapshot(&session).prompt_rows, vec![prompt2_row]);

    // Jump up: PROMPT1 (in history) becomes the top row and keeps its mark.
    assert!(session.jump_to_prompt(true));
    let frame = snapshot(&session);
    assert!(row_text(&frame, 0).starts_with("PROMPT1$"), "{:?}", row_text(&frame, 0));
    assert_eq!(frame.prompt_rows.first(), Some(&0));
    // Nothing further up.
    assert!(!session.jump_to_prompt(true));
    // Jump down: PROMPT2 is on the last screen page, so the view returns to the bottom.
    assert!(session.jump_to_prompt(false));
    assert!(row_text(&snapshot(&session), prompt2_row).starts_with("PROMPT2$"));

    // OSC 8: the four cells of "LINK" carry the link, " plain" doesn't.
    session.input(&b"\n"[..]);
    let frame = wait_for(&session, "LINK plain");
    let row = (0..frame.rows)
        .find(|&r| row_text(&frame, r).starts_with("LINK"))
        .unwrap();
    let cells = frame.row(row);
    assert!(cells[..4].iter().all(|c| c.link == Some(0)));
    assert!(cells[4..].iter().all(|c| c.link.is_none()));
    assert_eq!(frame.links[0].uri, "https://example.com/docs");
    assert_eq!(
        session.link_at(row, 2).map(|l| l.uri).as_deref(),
        Some("https://example.com/docs")
    );
    assert_eq!(session.link_at(row, 5), None);
    assert!(session.child_pid() > 0);
}
