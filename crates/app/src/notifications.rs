//! Notification policy for a session: turns OSC 9 / 777 / 99 sequences into
//! desktop notifications and replies, and handles clicks and closes.
//!
//! - OSC 9 and 777 are shown only while the window is unfocused; OSC 99
//!   follows its `o=` occasion (default: always).
//! - At most [`RATE_LIMIT`] notifications per [`RATE_WINDOW`], so a runaway
//!   program can't flood the desktop.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use aiterm_vt::Sequence;
use aiterm_vt::notification::{self, Notification, NotificationParser, NotifyCommand, Occasion, Urgency};

use crate::desktop_notify::{self, DesktopEvent, DesktopNotifier, Key, ShowRequest};

pub const RATE_LIMIT: usize = 5;
pub const RATE_WINDOW: Duration = Duration::from_secs(10);

/// Where notifications go; the D-Bus notifier in production, a recorder in tests.
pub trait Sink {
    fn show(&self, request: ShowRequest);
    fn close(&self, key: Key);
}

impl Sink for DesktopNotifier {
    fn show(&self, request: ShowRequest) {
        DesktopNotifier::show(self, request);
    }

    fn close(&self, key: Key) {
        DesktopNotifier::close(self, key);
    }
}

/// What the terminal knows about the window when a sequence arrives.
pub struct Context<'a> {
    pub focused: bool,
    /// Window title, used as the summary when a notification has no title.
    pub title: &'a str,
}

/// What the app must do in response to a click or close.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Response {
    /// Bring the terminal window to the front.
    pub focus_window: bool,
    /// Bytes to send to the application in the PTY.
    pub reply: Option<String>,
}

struct Shown {
    client_id: Option<String>,
    focus_on_activate: bool,
    report_activation: bool,
    report_close: bool,
}

pub struct Notifications<S: Sink> {
    sink: S,
    parser: NotificationParser,
    shown: HashMap<Key, Shown>,
    next_key: Key,
    recent: VecDeque<Instant>,
}

impl<S: Sink> Notifications<S> {
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            parser: NotificationParser::new(),
            shown: HashMap::new(),
            next_key: 1,
            recent: VecDeque::new(),
        }
    }

    /// Handle a sequence from the PTY. Returns a reply for the application, if any.
    pub fn handle_sequence(&mut self, seq: &Sequence, ctx: &Context) -> Option<String> {
        match self.parser.parse(seq)? {
            NotifyCommand::Show(n) => {
                self.show(n, ctx, Instant::now());
                None
            }
            NotifyCommand::Close(client_id) => {
                let key = self
                    .shown
                    .iter()
                    .find(|(_, s)| s.client_id.as_deref() == Some(client_id.as_str()))
                    .map(|(k, _)| *k);
                if let Some(key) = key {
                    self.sink.close(key);
                }
                None
            }
            NotifyCommand::Query(id) => Some(notification::query_response(id.as_deref())),
            NotifyCommand::Alive(id) => {
                let alive: Vec<&str> = self.shown.values().filter_map(|s| s.client_id.as_deref()).collect();
                Some(notification::alive_response(id.as_deref(), &alive))
            }
        }
    }

    fn show(&mut self, n: Notification, ctx: &Context, now: Instant) {
        let wanted = match n.occasion {
            Occasion::Always => true,
            // Wayland doesn't tell clients whether they are visible; treat
            // "invisible" like "unfocused".
            Occasion::Unfocused | Occasion::Invisible => !ctx.focused,
        };
        if !wanted {
            log::debug!("notification suppressed (window focused): {:?}", n.title);
            return;
        }
        while self
            .recent
            .front()
            .is_some_and(|t| now.duration_since(*t) > RATE_WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= RATE_LIMIT {
            log::warn!("notification rate limit reached, dropping {:?}", n.title);
            return;
        }
        self.recent.push_back(now);

        let (summary, body) = if n.title.is_empty() {
            let summary = if ctx.title.is_empty() { "aiterm" } else { ctx.title };
            (summary.to_string(), n.body)
        } else {
            (n.title, n.body)
        };
        let key = self.next_key;
        self.next_key += 1;
        self.shown.insert(
            key,
            Shown {
                client_id: n.id,
                focus_on_activate: n.focus_on_activate,
                report_activation: n.report_activation,
                report_close: n.report_close,
            },
        );
        self.sink.show(ShowRequest {
            key,
            summary,
            body,
            urgency: match n.urgency {
                Urgency::Low => desktop_notify::Urgency::Low,
                Urgency::Normal => desktop_notify::Urgency::Normal,
                Urgency::Critical => desktop_notify::Urgency::Critical,
            },
            expire_ms: n.expire_ms,
            clickable: n.focus_on_activate || n.report_activation,
        });
    }

    pub fn handle_desktop_event(&mut self, event: DesktopEvent) -> Response {
        match event {
            DesktopEvent::Activated(key) => {
                let Some(s) = self.shown.get(&key) else {
                    return Response::default();
                };
                Response {
                    focus_window: s.focus_on_activate,
                    reply: s
                        .client_id
                        .as_deref()
                        .filter(|_| s.report_activation)
                        .map(notification::activation_report),
                }
            }
            DesktopEvent::Closed(key) => {
                let Some(s) = self.shown.remove(&key) else {
                    return Response::default();
                };
                Response {
                    focus_window: false,
                    reply: s
                        .client_id
                        .as_deref()
                        .filter(|_| s.report_close)
                        .map(notification::close_report),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    #[derive(Clone, Default)]
    struct Recorder {
        shown: Rc<RefCell<Vec<ShowRequest>>>,
        closed: Rc<RefCell<Vec<Key>>>,
    }

    impl Sink for Recorder {
        fn show(&self, request: ShowRequest) {
            self.shown.borrow_mut().push(request);
        }

        fn close(&self, key: Key) {
            self.closed.borrow_mut().push(key);
        }
    }

    fn osc(p: &str) -> Sequence {
        Sequence::Osc(p.as_bytes().to_vec())
    }

    const FOCUSED: Context = Context {
        focused: true,
        title: "✳ Claude Code",
    };
    const UNFOCUSED: Context = Context {
        focused: false,
        title: "✳ Claude Code",
    };

    #[test]
    fn osc9_only_when_unfocused_and_uses_window_title() {
        let rec = Recorder::default();
        let mut n = Notifications::new(rec.clone());
        n.handle_sequence(&osc("9;Claude needs your permission"), &FOCUSED);
        assert!(rec.shown.borrow().is_empty());
        n.handle_sequence(&osc("9;Claude needs your permission"), &UNFOCUSED);
        let shown = rec.shown.borrow();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].summary, "✳ Claude Code");
        assert_eq!(shown[0].body, "Claude needs your permission");
        assert!(shown[0].clickable);
    }

    #[test]
    fn osc99_occasion_and_urgency() {
        let rec = Recorder::default();
        let mut n = Notifications::new(rec.clone());
        n.handle_sequence(&osc("99;u=2;Always shown"), &FOCUSED);
        n.handle_sequence(&osc("99;o=unfocused;Hidden"), &FOCUSED);
        let shown = rec.shown.borrow();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].summary, "Always shown");
        assert_eq!(shown[0].urgency, desktop_notify::Urgency::Critical);
    }

    #[test]
    fn rate_limit() {
        let rec = Recorder::default();
        let mut n = Notifications::new(rec.clone());
        for i in 0..20 {
            n.handle_sequence(&osc(&format!("777;notify;T{i};B")), &UNFOCUSED);
        }
        assert_eq!(rec.shown.borrow().len(), RATE_LIMIT);
        // The window slides.
        let later = Instant::now() + RATE_WINDOW + Duration::from_secs(1);
        let mut extra = Notification::clone(&match n.parser.parse(&osc("777;notify;late;B")) {
            Some(NotifyCommand::Show(x)) => x,
            _ => unreachable!(),
        });
        extra.title = "late".into();
        n.show(extra, &UNFOCUSED, later);
        assert_eq!(rec.shown.borrow().len(), RATE_LIMIT + 1);
    }

    #[test]
    fn kitty_reports_close_and_queries() {
        let rec = Recorder::default();
        let mut n = Notifications::new(rec.clone());
        n.handle_sequence(&osc("99;i=job:a=report,-focus:c=1;Done"), &FOCUSED);
        let key = rec.shown.borrow()[0].key;

        assert_eq!(
            n.handle_desktop_event(DesktopEvent::Activated(key)),
            Response {
                focus_window: false,
                reply: Some("\x1b]99;i=job;\x1b\\".into())
            }
        );
        let alive = n.handle_sequence(&osc("99;i=q:p=alive;"), &FOCUSED);
        assert_eq!(alive.as_deref(), Some("\x1b]99;i=q:p=alive;job\x1b\\"));

        n.handle_sequence(&osc("99;i=job:p=close;"), &FOCUSED);
        assert_eq!(*rec.closed.borrow(), vec![key]);
        assert_eq!(
            n.handle_desktop_event(DesktopEvent::Closed(key)),
            Response {
                focus_window: false,
                reply: Some("\x1b]99;i=job:p=close;\x1b\\".into())
            }
        );
        // Unknown or already closed notifications are ignored.
        assert_eq!(n.handle_desktop_event(DesktopEvent::Closed(key)), Response::default());

        let q = n.handle_sequence(&osc("99;i=q:p=?;"), &FOCUSED).unwrap();
        assert!(q.starts_with("\x1b]99;i=q:p=?;"));
    }

    #[test]
    fn default_activation_focuses_window() {
        let rec = Recorder::default();
        let mut n = Notifications::new(rec.clone());
        n.handle_sequence(&osc("9;Waiting"), &UNFOCUSED);
        let key = rec.shown.borrow()[0].key;
        let r = n.handle_desktop_event(DesktopEvent::Activated(key));
        assert!(r.focus_window);
        assert_eq!(r.reply, None);
    }
}
