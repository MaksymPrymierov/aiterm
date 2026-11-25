//! One terminal session: PTY + VT state (alacritty_terminal) + I/O thread.
//!
//! The public API uses only this crate's own types, so the VT backend can be
//! swapped (e.g. for libghostty-vt) without touching the app or the renderer.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aiterm_intercept::Sequence;
use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Notifier};
use alacritty_terminal::grid::{self, Dimensions};
use alacritty_terminal::index::{self, Column, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{self, ClipboardType, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{self, Color, NamedColor};
use bitflags::bitflags;

use crate::frame::{CellFlags, CellView, Cursor, CursorShape, Frame};
use crate::pty::TapPty;
use crate::theme::{Rgb, Theme};

/// Reply builder handed out with a request event; the result must be written back to the PTY.
pub type Reply<T> = Arc<dyn Fn(T) -> String + Send + Sync>;
/// Reply builder for clipboard loads (OSC 52 read).
pub type TextReply = Arc<dyn Fn(&str) -> String + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardKind {
    Clipboard,
    /// The X11/Wayland primary selection.
    Primary,
}

impl From<ClipboardType> for ClipboardKind {
    fn from(ty: ClipboardType) -> Self {
        match ty {
            ClipboardType::Clipboard => ClipboardKind::Clipboard,
            ClipboardType::Selection => ClipboardKind::Primary,
        }
    }
}

/// Events emitted by a session. Delivered from the PTY thread.
pub enum VtEvent {
    /// New content is available; request a redraw. Not sent again until
    /// [`Session::snapshot`] is called.
    Wakeup,
    Title(String),
    ResetTitle,
    Bell,
    /// The child process exited.
    Exit,
    /// Bytes the terminal must send back to the application.
    PtyWrite(String),
    ClipboardStore(ClipboardKind, String),
    ClipboardLoad(ClipboardKind, TextReply),
    /// Color query (OSC 4/10/11/12): index 0-255, 256 = fg, 257 = bg, 258 = cursor.
    ColorRequest(usize, Reply<Rgb>),
    TextAreaSizeRequest(Reply<GridSize>),
    /// A sequence the VT parser ignores but aiterm may act on
    /// (notifications, progress, shell integration, images).
    Sequence(Sequence),
}

impl std::fmt::Debug for VtEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VtEvent::Wakeup => write!(f, "Wakeup"),
            VtEvent::Title(t) => write!(f, "Title({t:?})"),
            VtEvent::ResetTitle => write!(f, "ResetTitle"),
            VtEvent::Bell => write!(f, "Bell"),
            VtEvent::Exit => write!(f, "Exit"),
            VtEvent::PtyWrite(s) => write!(f, "PtyWrite({s:?})"),
            VtEvent::ClipboardStore(k, _) => write!(f, "ClipboardStore({k:?})"),
            VtEvent::ClipboardLoad(k, _) => write!(f, "ClipboardLoad({k:?})"),
            VtEvent::ColorRequest(i, _) => write!(f, "ColorRequest({i})"),
            VtEvent::TextAreaSizeRequest(_) => write!(f, "TextAreaSizeRequest"),
            VtEvent::Sequence(s) => write!(f, "Sequence({s:?})"),
        }
    }
}

type Callback = Arc<dyn Fn(VtEvent) + Send + Sync>;

/// Bridges alacritty_terminal events to [`VtEvent`]s.
#[derive(Clone)]
struct Listener {
    callback: Callback,
    /// Set when a `Wakeup` was delivered and the frame has not been
    /// snapshotted yet. The PTY thread emits a wakeup after every parsed
    /// chunk (up to 64 KiB); under heavy output that is tens of thousands of
    /// events per second, of which at most one per frame is useful.
    wakeup_pending: Arc<AtomicBool>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let event = match event {
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                if self.wakeup_pending.swap(true, Ordering::AcqRel) {
                    return;
                }
                VtEvent::Wakeup
            }
            Event::Title(t) => VtEvent::Title(t),
            Event::ResetTitle => VtEvent::ResetTitle,
            Event::Bell => VtEvent::Bell,
            Event::Exit | Event::ChildExit(_) => VtEvent::Exit,
            Event::PtyWrite(s) => VtEvent::PtyWrite(s),
            Event::ClipboardStore(ty, text) => VtEvent::ClipboardStore(ty.into(), text),
            Event::ClipboardLoad(ty, f) => VtEvent::ClipboardLoad(ty.into(), f),
            Event::ColorRequest(idx, f) => {
                VtEvent::ColorRequest(idx, Arc::new(move |c: Rgb| f(ansi::Rgb { r: c.r, g: c.g, b: c.b })))
            }
            Event::TextAreaSizeRequest(f) => {
                VtEvent::TextAreaSizeRequest(Arc::new(move |size: GridSize| f(size.window_size())))
            }
        };
        (self.callback)(event);
    }
}

/// OSC codes alacritty_terminal ignores and aiterm handles itself.
const FORWARDED_OSC: &[u32] = &[
    7,    // current working directory
    9,    // notification (iTerm2/ConEmu), 9;4 progress
    99,   // notification (kitty)
    133,  // shell integration: prompt/command marks
    777,  // notification (rxvt/VTE)
    1337, // iTerm2 extensions (inline images)
];

fn is_forwarded(seq: &Sequence) -> bool {
    match seq {
        Sequence::Osc(_) => seq.osc_code().is_some_and(|c| FORWARDED_OSC.contains(&c)),
        Sequence::Apc(_) | Sequence::Dcs(_) => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridSize {
    pub cols: usize,
    pub rows: usize,
    /// Cell size in physical pixels.
    pub cell_w: f32,
    pub cell_h: f32,
}

impl GridSize {
    fn window_size(&self) -> WindowSize {
        WindowSize {
            num_lines: self.rows as u16,
            num_cols: self.cols as u16,
            cell_width: self.cell_w.round() as u16,
            cell_height: self.cell_h.round() as u16,
        }
    }
}

bitflags! {
    /// Terminal modes that affect input encoding.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Modes: u16 {
        /// DECCKM: cursor keys send SS3 sequences.
        const APP_CURSOR = 1;
        const BRACKETED_PASTE = 1 << 1;
        const MOUSE_CLICK = 1 << 2;
        const MOUSE_DRAG = 1 << 3;
        const MOUSE_MOTION = 1 << 4;
        const SGR_MOUSE = 1 << 5;
        const FOCUS_IN_OUT = 1 << 6;
        const ALT_SCREEN = 1 << 7;
        /// Mouse wheel sends cursor keys on the alternate screen.
        const ALTERNATE_SCROLL = 1 << 8;

        // Kitty keyboard protocol progressive enhancement flags (`CSI > flags u`).
        // https://sw.kovidgoyal.net/kitty/keyboard-protocol/
        /// Flag 0b1: report ambiguous keys (Esc, modified keys) as CSI u.
        const KITTY_DISAMBIGUATE = 1 << 9;
        /// Flag 0b10: report repeat and release events.
        const KITTY_EVENT_TYPES = 1 << 10;
        /// Flag 0b100: report shifted and base-layout keys.
        const KITTY_ALTERNATE_KEYS = 1 << 11;
        /// Flag 0b1000: report all keys, including text keys, as escape codes.
        const KITTY_ALL_KEYS = 1 << 12;
        /// Flag 0b10000: report the text a key generates.
        const KITTY_TEXT = 1 << 13;
        const KITTY_ANY = Self::KITTY_DISAMBIGUATE.bits()
            | Self::KITTY_EVENT_TYPES.bits()
            | Self::KITTY_ALTERNATE_KEYS.bits()
            | Self::KITTY_ALL_KEYS.bits()
            | Self::KITTY_TEXT.bits();
        const MOUSE_ANY = Self::MOUSE_CLICK.bits() | Self::MOUSE_DRAG.bits() | Self::MOUSE_MOTION.bits();
    }
}

impl From<TermMode> for Modes {
    fn from(m: TermMode) -> Self {
        let map = [
            (TermMode::APP_CURSOR, Modes::APP_CURSOR),
            (TermMode::BRACKETED_PASTE, Modes::BRACKETED_PASTE),
            (TermMode::MOUSE_REPORT_CLICK, Modes::MOUSE_CLICK),
            (TermMode::MOUSE_DRAG, Modes::MOUSE_DRAG),
            (TermMode::MOUSE_MOTION, Modes::MOUSE_MOTION),
            (TermMode::SGR_MOUSE, Modes::SGR_MOUSE),
            (TermMode::FOCUS_IN_OUT, Modes::FOCUS_IN_OUT),
            (TermMode::ALT_SCREEN, Modes::ALT_SCREEN),
            (TermMode::ALTERNATE_SCROLL, Modes::ALTERNATE_SCROLL),
            (TermMode::DISAMBIGUATE_ESC_CODES, Modes::KITTY_DISAMBIGUATE),
            (TermMode::REPORT_EVENT_TYPES, Modes::KITTY_EVENT_TYPES),
            (TermMode::REPORT_ALTERNATE_KEYS, Modes::KITTY_ALTERNATE_KEYS),
            (TermMode::REPORT_ALL_KEYS_AS_ESC, Modes::KITTY_ALL_KEYS),
            (TermMode::REPORT_ASSOCIATED_TEXT, Modes::KITTY_TEXT),
        ];
        map.iter()
            .filter(|(t, _)| m.contains(*t))
            .fold(Modes::empty(), |acc, (_, o)| acc | *o)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// Positive values scroll up into history.
    Delta(i32),
    PageUp,
    PageDown,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    /// Word under the cursor (double click).
    Word,
    /// Whole lines (triple click).
    Line,
    /// Rectangular block.
    Block,
}

/// Which half of a cell the pointer is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

pub struct Session {
    term: Arc<FairMutex<Term<Listener>>>,
    wakeup_pending: Arc<AtomicBool>,
    notifier: Notifier,
    size: GridSize,
}

impl Session {
    /// Spawn `command` (or the user's shell) in a new PTY. `on_event` is
    /// called from the PTY thread.
    pub fn spawn(
        size: GridSize,
        command: Option<Vec<String>>,
        on_event: impl Fn(VtEvent) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let mut env = HashMap::new();
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
        env.insert("TERM_PROGRAM".into(), "aiterm".into());
        env.insert("TERM_PROGRAM_VERSION".into(), env!("CARGO_PKG_VERSION").into());
        let shell = command.and_then(|mut c| {
            (!c.is_empty()).then(|| {
                let program = c.remove(0);
                tty::Shell::new(program, c)
            })
        });
        let options = tty::Options {
            shell,
            env,
            ..Default::default()
        };

        let wakeup_pending = Arc::new(AtomicBool::new(false));
        let listener = Listener {
            callback: Arc::new(on_event),
            wakeup_pending: wakeup_pending.clone(),
        };
        let config = term::Config {
            scrolling_history: 50_000,
            // Mode stack handling (push/pop/set/query) is done by alacritty_terminal;
            // key encoding lives in the app.
            kitty_keyboard: true,
            ..Default::default()
        };
        let term = Term::new(config, &TermSize::new(size.cols, size.rows), listener.clone());
        let term = Arc::new(FairMutex::new(term));

        let tap = listener.callback.clone();
        let sink = Box::new(move |seq: Sequence| {
            if is_forwarded(&seq) {
                tap(VtEvent::Sequence(seq));
            }
        });
        let pty = TapPty::new(tty::new(&options, size.window_size(), 0)?, sink)?;
        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let notifier = Notifier(event_loop.channel());
        event_loop.spawn();

        Ok(Self {
            term,
            wakeup_pending,
            notifier,
            size,
        })
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    /// Write raw bytes to the PTY.
    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.notifier.notify(bytes);
    }

    /// Send user input: also snaps the view back to the bottom and clears selection.
    pub fn input(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        {
            let mut term = self.term.lock();
            term.scroll_display(grid::Scroll::Bottom);
            term.selection = None;
        }
        self.write(bytes);
    }

    pub fn resize(&mut self, size: GridSize) {
        self.size = size;
        self.term.lock().resize(TermSize::new(size.cols, size.rows));
        self.notifier.on_resize(size.window_size());
    }

    pub fn modes(&self) -> Modes {
        (*self.term.lock().mode()).into()
    }

    pub fn scroll(&self, scroll: Scroll) {
        let scroll = match scroll {
            Scroll::Delta(n) => grid::Scroll::Delta(n),
            Scroll::PageUp => grid::Scroll::PageUp,
            Scroll::PageDown => grid::Scroll::PageDown,
            Scroll::Bottom => grid::Scroll::Bottom,
        };
        self.term.lock().scroll_display(scroll);
    }

    /// Viewport cell -> grid point (accounts for scrollback offset).
    fn grid_point(term: &Term<Listener>, row: usize, col: usize) -> Point {
        let offset = term.grid().display_offset() as i32;
        let row = row.min(term.screen_lines().saturating_sub(1));
        let col = col.min(term.columns().saturating_sub(1));
        Point::new(Line(row as i32 - offset), Column(col))
    }

    fn side(side: Side) -> index::Side {
        match side {
            Side::Left => index::Side::Left,
            Side::Right => index::Side::Right,
        }
    }

    pub fn start_selection(&self, kind: SelectionKind, row: usize, col: usize, side: Side) {
        let ty = match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
            SelectionKind::Block => SelectionType::Block,
        };
        let mut term = self.term.lock();
        let point = Self::grid_point(&term, row, col);
        term.selection = Some(Selection::new(ty, point, Self::side(side)));
    }

    pub fn update_selection(&self, row: usize, col: usize, side: Side) {
        let mut term = self.term.lock();
        let point = Self::grid_point(&term, row, col);
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, Self::side(side));
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string().filter(|s| !s.is_empty())
    }

    pub fn set_focused(&self, focused: bool) {
        self.term.lock().is_focused = focused;
    }

    /// Capture the visible screen. Also re-arms [`VtEvent::Wakeup`]: wakeups
    /// are coalesced until the next snapshot.
    pub fn snapshot(&self, theme: &Theme, palette: &[Rgb; 256], focused: bool) -> Frame {
        // Clear before locking: output parsed after this point sends a new wakeup,
        // output parsed before it is included in this snapshot.
        self.wakeup_pending.store(false, Ordering::Release);
        let term = self.term.lock();
        let content = term.renderable_content();
        let cols = term.columns();
        let rows = term.screen_lines();
        let offset = content.display_offset as i32;
        let resolver = ColorResolver {
            theme,
            palette,
            overrides: content.colors,
        };

        let mut cells = vec![CellView::blank(theme.foreground, theme.background); cols * rows];
        let selection = content.selection;

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 || row as usize >= rows {
                continue;
            }
            let col = indexed.point.column.0;
            let cell = indexed.cell;
            let flags = cell.flags;

            let mut fg = resolver.resolve(cell.fg, flags, true);
            let mut bg = resolver.resolve(cell.bg, flags, false);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            let selected =
                selection.is_some_and(|s| s.contains_cell(&indexed, content.cursor.point, content.cursor.shape));
            if selected {
                bg = theme.selection_bg;
            }

            let zerowidth = cell.zerowidth().map(|z| z.iter().collect::<String>());
            cells[row as usize * cols + col] = CellView {
                c: cell.c,
                zerowidth,
                fg,
                bg,
                flags: cell_flags(flags),
            };
        }

        let cursor_row = content.cursor.point.line.0 + offset;
        let shape = match content.cursor.shape {
            _ if !focused => Some(CursorShape::HollowBlock),
            ansi::CursorShape::Block => Some(CursorShape::Block),
            ansi::CursorShape::Beam => Some(CursorShape::Beam),
            ansi::CursorShape::Underline => Some(CursorShape::Underline),
            ansi::CursorShape::HollowBlock => Some(CursorShape::HollowBlock),
            ansi::CursorShape::Hidden => None,
        };
        let visible = content.cursor.shape != ansi::CursorShape::Hidden;
        let cursor = shape
            .filter(|_| visible && cursor_row >= 0 && (cursor_row as usize) < rows)
            .map(|shape| Cursor {
                row: cursor_row as usize,
                col: content.cursor.point.column.0.min(cols.saturating_sub(1)),
                shape,
                color: content.colors[NamedColor::Cursor]
                    .map(Rgb::from)
                    .unwrap_or(theme.cursor),
            });

        // Block cursor: draw the glyph under it in the background color.
        if let Some(c) = &cursor
            && c.shape == CursorShape::Block
        {
            let cell = &mut cells[c.row * cols + c.col];
            cell.fg = cell.bg;
        }

        let background = content.colors[NamedColor::Background]
            .map(Rgb::from)
            .unwrap_or(theme.background);
        Frame {
            cols,
            rows,
            cells,
            cursor,
            background,
        }
    }
}

fn cell_flags(f: Flags) -> CellFlags {
    let mut out = CellFlags::empty();
    out.set(CellFlags::BOLD, f.contains(Flags::BOLD));
    out.set(CellFlags::ITALIC, f.contains(Flags::ITALIC));
    out.set(CellFlags::UNDERLINE, f.intersects(Flags::ALL_UNDERLINES));
    out.set(CellFlags::STRIKEOUT, f.contains(Flags::STRIKEOUT));
    out.set(CellFlags::WIDE_CHAR, f.contains(Flags::WIDE_CHAR));
    out.set(
        CellFlags::WIDE_SPACER,
        f.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
    );
    out
}

struct ColorResolver<'a> {
    theme: &'a Theme,
    palette: &'a [Rgb; 256],
    overrides: &'a term::color::Colors,
}

impl ColorResolver<'_> {
    fn indexed(&self, i: usize) -> Rgb {
        self.overrides[i].map(Rgb::from).unwrap_or(self.palette[i])
    }

    fn resolve(&self, color: Color, flags: Flags, is_fg: bool) -> Rgb {
        let dim = is_fg && flags.contains(Flags::DIM);
        let rgb = match color {
            Color::Spec(c) => c.into(),
            Color::Indexed(i) => {
                // Bold-as-bright for the 8 basic colors.
                let i = if is_fg && i < 8 && flags.contains(Flags::BOLD) {
                    i + 8
                } else {
                    i
                };
                self.indexed(i as usize)
            }
            Color::Named(n) => {
                let idx = n as usize;
                if let Some(c) = self.overrides[idx] {
                    c.into()
                } else {
                    match n {
                        NamedColor::Foreground | NamedColor::BrightForeground => self.theme.foreground,
                        NamedColor::Background => self.theme.background,
                        NamedColor::Cursor => self.theme.cursor,
                        NamedColor::DimForeground => self.theme.foreground.scale(0.66),
                        n if idx < 8 && is_fg && flags.contains(Flags::BOLD) => self.indexed(n as usize + 8),
                        n if idx < 16 => self.indexed(n as usize),
                        // Dim* variants follow Foreground..; map back onto the normal colors.
                        n => self.indexed(n as usize - NamedColor::DimBlack as usize).scale(0.66),
                    }
                }
            }
        };
        if dim { rgb.scale(0.66) } else { rgb }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_only_unhandled_osc_codes() {
        assert!(is_forwarded(&Sequence::Osc(b"9;4;1;50".to_vec())));
        assert!(is_forwarded(&Sequence::Osc(b"133;A".to_vec())));
        assert!(!is_forwarded(&Sequence::Osc(b"0;title".to_vec())));
        assert!(!is_forwarded(&Sequence::Osc(b"52;c;aGk=".to_vec())));
        assert!(is_forwarded(&Sequence::Apc(b"Ga=T".to_vec())));
    }

    #[test]
    fn modes_conversion() {
        let m: Modes = (TermMode::APP_CURSOR | TermMode::SGR_MOUSE | TermMode::MOUSE_DRAG).into();
        assert_eq!(m, Modes::APP_CURSOR | Modes::SGR_MOUSE | Modes::MOUSE_DRAG);
        assert!(m.intersects(Modes::MOUSE_ANY));

        let k: Modes = (TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC).into();
        assert_eq!(k, Modes::KITTY_DISAMBIGUATE | Modes::KITTY_ALL_KEYS);
    }
}
