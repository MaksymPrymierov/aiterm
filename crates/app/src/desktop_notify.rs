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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopEvent {
    /// The user clicked the notification. `token` is an xdg-activation token
    /// from the server (spec 1.2 `ActivationToken`), if it provides one.
    Activated { key: Key, token: Option<String> },
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
    listen(&conn, ids.clone(), on_event)?;

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

/// Forward the server's signals for our notifications from one thread, so
/// `ActivationToken` is always seen before the `ActionInvoked` it belongs to.
fn listen(conn: &Connection, ids: IdMap, on_event: Arc<dyn Fn(DesktopEvent) + Send + Sync>) -> zbus::Result<()> {
    let proxy = Proxy::new(conn, DEST, PATH, IFACE)?;
    let signals = proxy.receive_all_signals()?;
    std::thread::Builder::new()
        .name("notify-signals".into())
        .spawn(move || {
            let mut tokens: HashMap<u32, String> = HashMap::new();
            for msg in signals {
                let header = msg.header();
                let Some(member) = header.member() else { continue };
                match member.as_str() {
                    "ActivationToken" => {
                        if let Ok((id, token)) = msg.body().deserialize::<(u32, String)>() {
                            tokens.insert(id, token);
                        }
                    }
                    "ActionInvoked" => {
                        let Ok((id, _action)) = msg.body().deserialize::<(u32, String)>() else {
                            continue;
                        };
                        let key = ids.lock().unwrap().get(&id).copied();
                        if let Some(key) = key {
                            on_event(DesktopEvent::Activated {
                                key,
                                token: tokens.remove(&id),
                            });
                        }
                    }
                    "NotificationClosed" => {
                        let Ok((id, _reason)) = msg.body().deserialize::<(u32, u32)>() else {
                            continue;
                        };
                        tokens.remove(&id);
                        if let Some(key) = ids.lock().unwrap().remove(&id) {
                            on_event(DesktopEvent::Closed(key));
                        }
                    }
                    _ => {}
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
