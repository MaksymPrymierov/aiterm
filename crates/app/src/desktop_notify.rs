//! Desktop notifications through the freedesktop `org.freedesktop.Notifications`
//! D-Bus service (<https://specifications.freedesktop.org/notification-spec/>).
//!
//! All D-Bus work happens on background threads; the UI thread only sends
//! requests and receives [`DesktopEvent`]s. Without a session bus or
//! notification server, requests are dropped with a single warning.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::Value;

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const IFACE: &str = "org.freedesktop.Notifications";

/// Opaque handle chosen by the caller to identify a notification.
pub type Key = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Urgency {
    Low = 0,
    Normal = 1,
    Critical = 2,
}

#[derive(Debug)]
pub struct ShowRequest {
    pub key: Key,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    /// Milliseconds; `None` uses the server default.
    pub expire_ms: Option<i32>,
    /// Make the notification clickable (the "default" action).
    pub clickable: bool,
}

#[derive(Debug)]
enum Request {
    Show(ShowRequest),
    Close(Key),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopEvent {
    /// The user clicked the notification.
    Activated(Key),
    /// The notification was dismissed, expired or closed.
    Closed(Key),
}

pub struct DesktopNotifier {
    tx: Sender<Request>,
}

type IdMap = Arc<Mutex<HashMap<u32, Key>>>;

impl DesktopNotifier {
    /// Starts the notification thread. `on_event` is called from a background thread.
    pub fn spawn(on_event: impl Fn(DesktopEvent) + Send + Sync + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let on_event = Arc::new(on_event);
        let spawned = std::thread::Builder::new().name("notifications".into()).spawn(move || {
            if let Err(e) = run(rx, on_event) {
                log::warn!("desktop notifications unavailable: {e}");
            }
        });
        if let Err(e) = spawned {
            log::warn!("failed to start notification thread: {e}");
        }
        Self { tx }
    }

    pub fn show(&self, request: ShowRequest) {
        let _ = self.tx.send(Request::Show(request));
    }

    pub fn close(&self, key: Key) {
        let _ = self.tx.send(Request::Close(key));
    }
}

fn run(rx: Receiver<Request>, on_event: Arc<dyn Fn(DesktopEvent) + Send + Sync>) -> zbus::Result<()> {
    let conn = Connection::session()?;
    let proxy = Proxy::new(&conn, DEST, PATH, IFACE)?;
    let caps: Vec<String> = proxy.call("GetCapabilities", &()).unwrap_or_default();
    let markup = caps.iter().any(|c| c == "body-markup");
    let actions = caps.iter().any(|c| c == "actions");
    log::debug!("notification server capabilities: {caps:?}");

    let ids: IdMap = Arc::default();
    listen(&conn, "ActionInvoked", ids.clone(), on_event.clone(), |msg| {
        let (id, _action): (u32, String) = msg.body().deserialize().ok()?;
        Some((id, false))
    })?;
    listen(&conn, "NotificationClosed", ids.clone(), on_event, |msg| {
        let (id, _reason): (u32, u32) = msg.body().deserialize().ok()?;
        Some((id, true))
    })?;

    for request in rx {
        match request {
            Request::Show(r) => {
                let body = if markup { escape_markup(&r.body) } else { r.body };
                let action_list: Vec<&str> = if r.clickable && actions {
                    vec!["default", "Open"]
                } else {
                    vec![]
                };
                let mut hints: HashMap<&str, Value> = HashMap::new();
                hints.insert("urgency", Value::U8(r.urgency as u8));
                let args = (
                    "aiterm",
                    0u32,
                    "utilities-terminal",
                    r.summary.as_str(),
                    body.as_str(),
                    action_list,
                    hints,
                    r.expire_ms.unwrap_or(-1),
                );
                match proxy.call::<_, _, u32>("Notify", &args) {
                    Ok(id) => {
                        ids.lock().unwrap().insert(id, r.key);
                    }
                    Err(e) => log::warn!("Notify failed: {e}"),
                }
            }
            Request::Close(key) => {
                let id = ids.lock().unwrap().iter().find(|(_, k)| **k == key).map(|(id, _)| *id);
                if let Some(id) = id
                    && let Err(e) = proxy.call::<_, _, ()>("CloseNotification", &(id,))
                {
                    log::warn!("CloseNotification failed: {e}");
                }
            }
        }
    }
    Ok(())
}

/// Spawn a thread forwarding `signal` for our notifications. `parse` returns
/// the server id and whether the notification is gone.
fn listen(
    conn: &Connection,
    signal: &'static str,
    ids: IdMap,
    on_event: Arc<dyn Fn(DesktopEvent) + Send + Sync>,
    parse: fn(&zbus::Message) -> Option<(u32, bool)>,
) -> zbus::Result<()> {
    let proxy = Proxy::new(conn, DEST, PATH, IFACE)?;
    let signals = proxy.receive_signal(signal)?;
    std::thread::Builder::new()
        .name(format!("notify-{signal}"))
        .spawn(move || {
            for msg in signals {
                let Some((id, closed)) = parse(&msg) else { continue };
                let key = {
                    let mut ids = ids.lock().unwrap();
                    if closed { ids.remove(&id) } else { ids.get(&id).copied() }
                };
                if let Some(key) = key {
                    on_event(if closed {
                        DesktopEvent::Closed(key)
                    } else {
                        DesktopEvent::Activated(key)
                    });
                }
            }
        })
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    Ok(())
}

/// The body may be interpreted as a markup subset; escape it so program
/// output is always shown literally.
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_escaped() {
        assert_eq!(
            escape_markup("a < b && <b>x</b>"),
            "a &lt; b &amp;&amp; &lt;b&gt;x&lt;/b&gt;"
        );
    }
}
