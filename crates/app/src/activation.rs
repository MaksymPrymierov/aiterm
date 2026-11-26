//! Window activation with an xdg-activation token (Wayland).
//!
//! winit 0.30 can only use an activation token when creating a window. To
//! focus our existing window after a notification click, take the token the
//! notification server provides (`ActivationToken` signal) and call
//! `xdg_activation_v1.activate` on winit's own `wl_display` connection, using
//! a private event queue.

use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::window::Window;

struct State;

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgActivationV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &XdgActivationV1,
        _: <XdgActivationV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Ask the compositor to focus `window`, proving user intent with `token`.
pub fn activate(window: &Window, token: &str) -> Result<(), String> {
    let display = match window.display_handle().map_err(|e| e.to_string())?.as_raw() {
        RawDisplayHandle::Wayland(h) => h.display.as_ptr(),
        _ => return Err("not a Wayland window".into()),
    };
    let surface = match window.window_handle().map_err(|e| e.to_string())?.as_raw() {
        RawWindowHandle::Wayland(h) => h.surface.as_ptr(),
        _ => return Err("not a Wayland window".into()),
    };

    // SAFETY: `display` is winit's live wl_display, which outlives this call;
    // `from_foreign_display` doesn't take ownership of it.
    let backend = unsafe { Backend::from_foreign_display(display.cast()) };
    let conn = Connection::from_backend(backend);
    let (globals, queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
    let activation: XdgActivationV1 = globals
        .bind(&queue.handle(), 1..=1, ())
        .map_err(|e| format!("xdg_activation_v1 unavailable: {e}"))?;

    // SAFETY: `surface` is the wl_surface of `window`, alive while `window` is.
    let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.cast()) }.map_err(|e| e.to_string())?;
    let surface = WlSurface::from_id(&conn, id).map_err(|e| e.to_string())?;

    activation.activate(token.to_string(), &surface);
    activation.destroy();
    conn.flush().map_err(|e| e.to_string())
}
