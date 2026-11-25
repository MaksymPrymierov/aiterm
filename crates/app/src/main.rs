mod desktop_notify;
mod input;
mod links;
mod notifications;
mod perf;
mod progress;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aiterm_render::{ProgressBar, Renderer};
use aiterm_vt::shell::ShellEvent;
use aiterm_vt::{ClipboardKind, Link, Modes, Rgb, Scroll, SelectionKind, Session, Theme, VtEvent};
use arboard::{Clipboard, GetExtLinux, LinuxClipboardKind, SetExtLinux};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{CursorIcon, Window, WindowId};

/// Events delivered to the winit event loop from other threads.
#[derive(Debug)]
enum UserEvent {
    Vt(VtEvent),
    Desktop(desktop_notify::DesktopEvent),
}

/// Largest clipboard write accepted from OSC 52.
const OSC52_MAX_BYTES: usize = 1024 * 1024;

fn linux_kind(kind: ClipboardKind) -> LinuxClipboardKind {
    match kind {
        ClipboardKind::Clipboard => LinuxClipboardKind::Clipboard,
        ClipboardKind::Primary => LinuxClipboardKind::Primary,
    }
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    theme: Theme,
    palette: [Rgb; 256],
    clipboard: Option<Clipboard>,
    /// Host name, to accept local `file://` URLs (OSC 7, OSC 8).
    hostname: String,
    command: Option<Vec<String>>,
    notifications: notifications::Notifications<desktop_notify::DesktopNotifier>,
    state: Option<WindowState>,
}

struct WindowState {
    // Drop order: session first (stops the PTY), then the renderer/window.
    session: Session,
    renderer: Renderer,
    modifiers: ModifiersState,
    /// Keys whose press was consumed by an app shortcut; their repeat and
    /// release events are not sent to the PTY either.
    consumed_keys: HashSet<PhysicalKey>,
    focused: bool,
    cursor_pos: PhysicalPosition<f64>,
    mouse_down: Option<MouseButton>,
    last_click: Option<(Instant, usize, usize, u8)>,
    scroll_accum: f64,
    stats: perf::FrameStats,
    /// Current window title (set by the application with OSC 0/2).
    title: String,
    /// Task progress reported with OSC 9;4.
    progress: progress::SessionProgress,
    /// Start of the progress animation clock.
    started: Instant,
    /// Working directory reported by the shell (OSC 7).
    cwd: Option<std::path::PathBuf>,
    /// OSC 8 hyperlink under the mouse pointer.
    hover_link: Option<Link>,
}

impl App {
    fn clipboard_get(&mut self, kind: LinuxClipboardKind) -> Option<String> {
        self.clipboard.as_mut()?.get().clipboard(kind).text().ok()
    }

    fn clipboard_set(&mut self, kind: LinuxClipboardKind, text: String) {
        if let Some(cb) = self.clipboard.as_mut()
            && let Err(e) = cb.set().clipboard(kind).text(text)
        {
            log::warn!("clipboard: {e}");
        }
    }

    fn paste(&mut self, kind: LinuxClipboardKind) {
        let Some(text) = self.clipboard_get(kind) else { return };
        if let Some(st) = &self.state {
            st.session.input(input::paste_bytes(&text, st.session.modes()));
        }
    }

    fn copy_selection(&mut self, kind: LinuxClipboardKind) {
        let text = self.state.as_ref().and_then(|s| s.session.selection_text());
        if let Some(text) = text {
            self.clipboard_set(kind, text);
        }
    }

    fn resize_grid(st: &mut WindowState) {
        let size = st.renderer.grid_size();
        if size != st.session.size() {
            st.session.resize(size);
        }
        st.renderer.window().request_redraw();
    }

    /// Handle app-level shortcuts. Returns true if consumed.
    fn shortcut(&mut self, event: &KeyEvent) -> bool {
        let Some(st) = self.state.as_mut() else { return false };
        let m = st.modifiers;
        let ctrl_shift = m.control_key() && m.shift_key();
        // Match letters by physical key, so shortcuts work in any keyboard layout.
        let physical = match event.physical_key {
            PhysicalKey::Code(code) => input::us_layout_char(code),
            PhysicalKey::Unidentified(_) => None,
        };
        let key = match &event.logical_key {
            Key::Character(s) => physical.map_or_else(|| s.to_lowercase(), String::from),
            Key::Named(NamedKey::PageUp) if m.shift_key() => {
                st.session.scroll(Scroll::PageUp);
                st.renderer.window().request_redraw();
                return true;
            }
            Key::Named(NamedKey::PageDown) if m.shift_key() => {
                st.session.scroll(Scroll::PageDown);
                st.renderer.window().request_redraw();
                return true;
            }
            _ => return false,
        };
        match key.as_str() {
            "c" if ctrl_shift => self.copy_selection(LinuxClipboardKind::Clipboard),
            // Previous / next shell prompt (OSC 133), as in kitty.
            "z" | "x" if ctrl_shift => {
                if st.session.jump_to_prompt(key == "z") {
                    st.renderer.window().request_redraw();
                }
            }
            "n" if ctrl_shift => {
                let cwd = st.cwd.clone().or_else(|| links::process_cwd(st.session.child_pid()));
                links::new_window(cwd.as_deref());
            }
            "v" if ctrl_shift => self.paste(LinuxClipboardKind::Clipboard),
            "+" | "=" if m.control_key() => {
                st.renderer.adjust_font_size(Some(1.0));
                Self::resize_grid(st);
            }
            "-" | "_" if m.control_key() => {
                st.renderer.adjust_font_size(Some(-1.0));
                Self::resize_grid(st);
            }
            "0" if m.control_key() => {
                st.renderer.adjust_font_size(None);
                Self::resize_grid(st);
            }
            _ => return false,
        }
        true
    }

    fn mouse_reporting(st: &WindowState) -> Option<Modes> {
        let mode = st.session.modes();
        (mode.intersects(Modes::MOUSE_ANY) && !st.modifiers.shift_key()).then_some(mode)
    }

    /// Send a mouse event to the application (SGR or legacy X10 encoding).
    fn report_mouse(st: &WindowState, mode: Modes, button: u8, pressed: bool, motion: bool) {
        let (row, col, _) = st.renderer.cell_at(st.cursor_pos.x, st.cursor_pos.y);
        let size = st.session.size();
        let (row, col) = (row.min(size.rows - 1), col.min(size.cols - 1));
        let m = st.modifiers;
        let mut b = button as u32 + 4 * m.shift_key() as u32 + 8 * m.alt_key() as u32 + 16 * m.control_key() as u32;
        if motion {
            b += 32;
        }
        let bytes = if mode.contains(Modes::SGR_MOUSE) {
            format!("\x1b[<{b};{};{}{}", col + 1, row + 1, if pressed { 'M' } else { 'm' }).into_bytes()
        } else {
            let b = if pressed { b } else { 3 + (b & !3) };
            if col >= 223 || row >= 223 {
                return;
            }
            vec![0x1b, b'[', b'M', 32 + b as u8, 33 + col as u8, 33 + row as u8]
        };
        st.session.write(bytes);
    }

    fn handle_vt_event(&mut self, event: VtEvent, event_loop: &ActiveEventLoop) {
        let Some(st) = self.state.as_mut() else { return };
        match event {
            VtEvent::Wakeup => {
                st.stats.wakeup();
                st.renderer.window().request_redraw();
            }
            VtEvent::Title(t) => {
                st.renderer.window().set_title(&t);
                st.title = t;
            }
            VtEvent::ResetTitle => {
                st.renderer.window().set_title("aiterm");
                st.title.clear();
            }
            VtEvent::PtyWrite(s) => st.session.write(s.into_bytes()),
            VtEvent::TextAreaSizeRequest(reply) => st.session.write(reply(st.session.size()).into_bytes()),
            VtEvent::ColorRequest(idx, reply) => {
                let rgb = match idx {
                    256 => self.theme.foreground,
                    257 => self.theme.background,
                    258 => self.theme.cursor,
                    i if i < 256 => self.palette[i],
                    _ => self.theme.foreground,
                };
                st.session.write(reply(rgb).into_bytes());
            }
            VtEvent::ClipboardStore(kind, text) => {
                // OSC 52: only while the user is looking at this terminal, and
                // bounded, so background output can't silently replace the clipboard.
                if !st.focused {
                    log::warn!("OSC 52 clipboard write ignored: window is not focused");
                } else if text.len() > OSC52_MAX_BYTES {
                    log::warn!("OSC 52 clipboard write ignored: {} bytes exceeds the limit", text.len());
                } else {
                    self.clipboard_set(linux_kind(kind), text);
                }
            }
            VtEvent::ClipboardLoad(kind, reply) => {
                let text = self.clipboard_get(linux_kind(kind)).unwrap_or_default();
                if let Some(st) = &self.state {
                    st.session.write(reply(&text).into_bytes());
                }
            }
            VtEvent::Bell => {
                if !st.focused {
                    st.renderer
                        .window()
                        .request_user_attention(Some(winit::window::UserAttentionType::Informational));
                }
            }
            // Notifications, progress, shell integration and images are handled in later milestones.
            VtEvent::Sequence(seq) => {
                log::debug!("intercepted: {seq:?}");
                if let Some(event) = aiterm_vt::shell::parse_shell_event(&seq, &self.hostname) {
                    match event {
                        ShellEvent::Cwd(dir) => st.cwd = Some(dir),
                        // The shell now waits for input: the cursor is on the prompt line.
                        ShellEvent::PromptEnd => {
                            st.session.mark_prompt();
                            st.renderer.window().request_redraw();
                        }
                        ShellEvent::PromptStart | ShellEvent::CommandStart | ShellEvent::CommandEnd(_) => {}
                    }
                    return;
                }
                if let Some(report) = aiterm_vt::progress::parse_progress(&seq) {
                    st.progress.apply(report, Instant::now());
                    st.renderer.window().request_redraw();
                    return;
                }
                let ctx = notifications::Context {
                    focused: st.focused,
                    title: &st.title,
                };
                if let Some(reply) = self.notifications.handle_sequence(&seq, &ctx) {
                    st.session.write(reply.into_bytes());
                }
            }
            VtEvent::Exit => event_loop.exit(),
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(st) = self.state.as_mut() else { return };
        if st.progress.expire(Instant::now()) {
            st.renderer.window().request_redraw();
        }
        // Wake up to drop a stale progress report.
        event_loop.set_control_flow(match st.progress.deadline() {
            Some(deadline) => ControlFlow::WaitUntil(deadline),
            None => ControlFlow::Wait,
        });
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("aiterm")
            .with_inner_size(LogicalSize::new(960.0, 600.0))
            .with_name("aiterm", "aiterm");
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        window.set_ime_allowed(true);
        window.set_cursor(CursorIcon::Text);

        let renderer = pollster::block_on(Renderer::new(window.clone(), event_loop));
        let proxy = self.proxy.clone();
        let on_event = move |e| {
            let _ = proxy.send_event(UserEvent::Vt(e));
        };
        let session = match Session::spawn(renderer.grid_size(), self.command.take(), on_event) {
            Ok(s) => s,
            Err(e) => {
                log::error!("failed to spawn shell: {e}");
                event_loop.exit();
                return;
            }
        };
        self.state = Some(WindowState {
            session,
            renderer,
            modifiers: ModifiersState::empty(),
            consumed_keys: HashSet::new(),
            focused: true,
            cursor_pos: PhysicalPosition::new(0.0, 0.0),
            mouse_down: None,
            last_click: None,
            scroll_accum: 0.0,
            stats: perf::FrameStats::default(),
            title: String::new(),
            progress: progress::SessionProgress::default(),
            started: Instant::now(),
            cwd: None,
            hover_link: None,
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Vt(e) => self.handle_vt_event(e, event_loop),
            UserEvent::Desktop(e) => {
                let response = self.notifications.handle_desktop_event(e);
                let Some(st) = &self.state else { return };
                if response.focus_window {
                    // winit 0.30 cannot activate an existing window on Wayland; an
                    // attention request makes the compositor focus or highlight it.
                    st.renderer
                        .window()
                        .request_user_attention(Some(winit::window::UserAttentionType::Critical));
                }
                if let Some(reply) = response.reply {
                    st.session.write(reply.into_bytes());
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let WindowEvent::KeyboardInput { event: key, .. } = &event
            && key.state == ElementState::Pressed
            && !key.repeat
            && self.shortcut(key)
        {
            if let Some(st) = self.state.as_mut() {
                st.consumed_keys.insert(key.physical_key);
            }
            return;
        }
        if let WindowEvent::MouseInput {
            state: ElementState::Released,
            button: MouseButton::Left,
            ..
        } = &event
        {
            // Select-to-copy (Linux primary selection) when releasing after a drag.
            if self
                .state
                .as_ref()
                .is_some_and(|s| s.mouse_down == Some(MouseButton::Left) && Self::mouse_reporting(s).is_none())
            {
                self.copy_selection(LinuxClipboardKind::Primary);
            }
        }
        if let WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Middle,
            ..
        } = &event
            && self.state.as_ref().is_some_and(|s| Self::mouse_reporting(s).is_none())
        {
            self.paste(LinuxClipboardKind::Primary);
            return;
        }

        let Some(st) = self.state.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                st.renderer.resize(size.width, size.height);
                Self::resize_grid(st);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                st.renderer.set_scale(scale_factor);
                Self::resize_grid(st);
            }
            WindowEvent::Focused(f) => {
                st.focused = f;
                st.session.set_focused(f);
                if st.session.modes().contains(Modes::FOCUS_IN_OUT) {
                    st.session.write(if f { &b"\x1b[I"[..] } else { &b"\x1b[O"[..] });
                }
                st.renderer.window().request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => st.modifiers = m.state(),
            WindowEvent::KeyboardInput { event: key, .. } => {
                if st.consumed_keys.contains(&key.physical_key) {
                    if key.state == ElementState::Released {
                        st.consumed_keys.remove(&key.physical_key);
                    }
                    return;
                }
                let Some(k) = input::KeyInput::from_winit(&key, st.modifiers) else {
                    return;
                };
                log::trace!("key {:?} {:?} -> {k:?}", key.logical_key, key.physical_key);
                if let Some(bytes) = input::encode(&k, st.session.modes()) {
                    match k.action {
                        // Releases (kitty event types) must not scroll or clear the selection.
                        input::Action::Release => st.session.write(bytes),
                        _ => st.session.input(bytes),
                    }
                }
            }
            WindowEvent::Ime(Ime::Commit(text)) => st.session.input(text.into_bytes()),
            WindowEvent::CursorMoved { position, .. } => {
                let old = st.renderer.cell_at(st.cursor_pos.x, st.cursor_pos.y);
                st.cursor_pos = position;
                let (row, col, side) = st.renderer.cell_at(position.x, position.y);
                let link = st.session.link_at(row, col);
                if link != st.hover_link {
                    let icon = if link.is_some() {
                        CursorIcon::Pointer
                    } else {
                        CursorIcon::Text
                    };
                    st.renderer.window().set_cursor(icon);
                    st.hover_link = link;
                    st.renderer.window().request_redraw();
                }
                if let Some(mode) = Self::mouse_reporting(st) {
                    let moved = (old.0, old.1) != (row, col);
                    let report = match st.mouse_down {
                        Some(_) => mode.intersects(Modes::MOUSE_DRAG | Modes::MOUSE_MOTION),
                        None => mode.contains(Modes::MOUSE_MOTION),
                    };
                    if moved && report {
                        let b = match st.mouse_down {
                            Some(MouseButton::Left) => 0,
                            Some(MouseButton::Middle) => 1,
                            Some(MouseButton::Right) => 2,
                            _ => 3,
                        };
                        Self::report_mouse(st, mode, b, true, true);
                    }
                } else if st.mouse_down == Some(MouseButton::Left) {
                    st.session.update_selection(row, col, side);
                    st.renderer.window().request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if st.modifiers.control_key() && st.hover_link.is_some() => {
                // Ctrl+click opens the hyperlink under the pointer.
                let uri = &st.hover_link.as_ref().unwrap().uri;
                if links::is_openable(uri, &self.hostname) {
                    links::open(uri);
                } else {
                    log::warn!("refusing to open hyperlink {uri:?}");
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                st.mouse_down = if pressed { Some(button) } else { None };
                if let Some(mode) = Self::mouse_reporting(st) {
                    let b = match button {
                        MouseButton::Left => 0,
                        MouseButton::Middle => 1,
                        MouseButton::Right => 2,
                        _ => return,
                    };
                    Self::report_mouse(st, mode, b, pressed, false);
                } else if pressed && button == MouseButton::Left {
                    let (row, col, side) = st.renderer.cell_at(st.cursor_pos.x, st.cursor_pos.y);
                    let now = Instant::now();
                    let clicks = match st.last_click {
                        Some((t, r, c, n)) if now - t < Duration::from_millis(400) && (r, c) == (row, col) => n % 3 + 1,
                        _ => 1,
                    };
                    st.last_click = Some((now, row, col, clicks));
                    let ty = match clicks {
                        2 => SelectionKind::Word,
                        3 => SelectionKind::Line,
                        _ if st.modifiers.control_key() => SelectionKind::Block,
                        _ => SelectionKind::Simple,
                    };
                    st.session.start_selection(ty, row, col, side);
                    st.renderer.window().request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64 * 3.0,
                    MouseScrollDelta::PixelDelta(p) => {
                        st.scroll_accum += p.y / st.renderer.metrics.h as f64;
                        let whole = st.scroll_accum.trunc();
                        st.scroll_accum -= whole;
                        whole
                    }
                };
                let n = lines as i32;
                if n == 0 {
                    return;
                }
                let mode = st.session.modes();
                if let Some(mode) = Self::mouse_reporting(st) {
                    let b = if n > 0 { 64 } else { 65 };
                    for _ in 0..n.unsigned_abs().min(10) {
                        Self::report_mouse(st, mode, b, true, false);
                    }
                } else if mode.contains(Modes::ALT_SCREEN | Modes::ALTERNATE_SCROLL) {
                    let seq: &[u8] = match (n > 0, mode.contains(Modes::APP_CURSOR)) {
                        (true, true) => b"\x1bOA",
                        (true, false) => b"\x1b[A",
                        (false, true) => b"\x1bOB",
                        (false, false) => b"\x1b[B",
                    };
                    st.session.write(seq.repeat(n.unsigned_abs() as usize));
                } else {
                    st.session.scroll(Scroll::Delta(n));
                    st.renderer.window().request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                let start = Instant::now();
                let mut frame = st.session.snapshot(&self.theme, &self.palette, st.focused);
                frame.hovered_link = st
                    .hover_link
                    .as_ref()
                    .and_then(|l| frame.links.iter().position(|x| x == l))
                    .map(|i| i as u16);
                let snapshot = start.elapsed();
                let bar = st.progress.shown().map(|shown| ProgressBar {
                    fraction: shown.fraction,
                    color: match shown.kind {
                        progress::Kind::Normal | progress::Kind::Indeterminate => self.theme.ansi[4],
                        progress::Kind::Error => self.theme.ansi[1],
                        progress::Kind::Paused => self.theme.ansi[3],
                    },
                    phase: st.started.elapsed().as_secs_f32(),
                });
                let status = st.hover_link.as_ref().map(|l| l.uri.as_str());
                st.renderer.render(&frame, bar.as_ref(), status);
                st.stats.frame(snapshot, start.elapsed() - snapshot);
                if st.progress.is_animated() {
                    // Paced by frame callbacks: one frame per display refresh while visible.
                    st.renderer.window().request_redraw();
                }
            }
            _ => {}
        }
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    // `aiterm -e cmd args...` runs a command instead of the login shell.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args.first().map(String::as_str) {
        Some("-e") if args.len() > 1 => Some(args[1..].to_vec()),
        _ => None,
    };

    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("event loop");
    let theme = Theme::default();
    let notify_proxy = event_loop.create_proxy();
    let notifier = desktop_notify::DesktopNotifier::spawn(move |e| {
        let _ = notify_proxy.send_event(UserEvent::Desktop(e));
    });
    let mut app = App {
        proxy: event_loop.create_proxy(),
        notifications: notifications::Notifications::new(notifier),
        palette: theme.palette256(),
        theme,
        clipboard: Clipboard::new().map_err(|e| log::warn!("no clipboard: {e}")).ok(),
        hostname: links::local_hostname(),
        command,
        state: None,
    };
    event_loop.run_app(&mut app).expect("run app");
}
