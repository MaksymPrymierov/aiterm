//! One terminal session: PTY + VT state (alacritty_terminal) + I/O thread.
//!
//! The renderer never touches `Term` directly; it consumes a [`Frame`] snapshot,
//! so the VT backend can later be swapped (e.g. for libghostty-vt).

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{self, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use winit::event_loop::EventLoopProxy;

use crate::theme::{Rgb, Theme};

#[derive(Debug)]
pub enum UserEvent {
    Term(Event),
}

#[derive(Clone)]
pub struct EventProxy(EventLoopProxy<UserEvent>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send_event(UserEvent::Term(event));
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GridSize {
    pub cols: usize,
    pub rows: usize,
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

pub struct Session {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    notifier: Notifier,
    pub size: GridSize,
}

impl Session {
    pub fn spawn(proxy: EventLoopProxy<UserEvent>, size: GridSize, command: Option<Vec<String>>) -> std::io::Result<Self> {
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
        let options = tty::Options { shell, env, ..Default::default() };

        let proxy = EventProxy(proxy);
        let config = term::Config { scrolling_history: 50_000, ..Default::default() };
        let term = Term::new(config, &TermSize::new(size.cols, size.rows), proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let pty = tty::new(&options, size.window_size(), 0)?;
        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false)?;
        let notifier = Notifier(event_loop.channel());
        event_loop.spawn();

        Ok(Self { term, notifier, size })
    }

    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.notifier.notify(bytes);
    }

    /// Send user input: also snaps the view back to the bottom and clears selection.
    pub fn input(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        {
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
        }
        self.write(bytes);
    }

    pub fn resize(&mut self, size: GridSize) {
        self.size = size;
        self.term.lock().resize(TermSize::new(size.cols, size.rows));
        self.notifier.on_resize(size.window_size());
    }

    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    pub fn scroll(&self, scroll: Scroll) {
        self.term.lock().scroll_display(scroll);
    }

    pub fn window_size(&self) -> WindowSize {
        self.size.window_size()
    }

    /// Viewport cell -> grid point (accounts for scrollback offset).
    fn grid_point(term: &Term<EventProxy>, row: usize, col: usize) -> Point {
        let offset = term.grid().display_offset() as i32;
        let row = row.min(term.screen_lines().saturating_sub(1));
        let col = col.min(term.columns().saturating_sub(1));
        Point::new(Line(row as i32 - offset), Column(col))
    }

    pub fn start_selection(&self, ty: SelectionType, row: usize, col: usize, side: Side) {
        let mut term = self.term.lock();
        let point = Self::grid_point(&term, row, col);
        term.selection = Some(Selection::new(ty, point, side));
    }

    pub fn update_selection(&self, row: usize, col: usize, side: Side) {
        let mut term = self.term.lock();
        let point = Self::grid_point(&term, row, col);
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string().filter(|s| !s.is_empty())
    }

    pub fn set_focused(&self, focused: bool) {
        self.term.lock().is_focused = focused;
    }

    pub fn snapshot(&self, theme: &Theme, palette: &[Rgb; 256], focused: bool) -> Frame {
        let term = self.term.lock();
        let content = term.renderable_content();
        let cols = term.columns();
        let rows = term.screen_lines();
        let offset = content.display_offset as i32;
        let resolver = ColorResolver { theme, palette, overrides: content.colors };

        let mut cells = vec![CellView::blank(theme); cols * rows];
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
            let selected = selection.is_some_and(|s| s.contains_cell(&indexed, content.cursor.point, content.cursor.shape));
            if selected {
                bg = theme.selection_bg;
            }

            let zerowidth = cell.zerowidth().map(|z| z.iter().collect::<String>());
            cells[row as usize * cols + col] = CellView { c: cell.c, zerowidth, fg, bg, flags };
        }

        let cursor_row = content.cursor.point.line.0 + offset;
        let cursor = (content.cursor.shape != CursorShape::Hidden
            && cursor_row >= 0
            && (cursor_row as usize) < rows)
            .then(|| Cursor {
                row: cursor_row as usize,
                col: content.cursor.point.column.0.min(cols.saturating_sub(1)),
                shape: if focused { content.cursor.shape } else { CursorShape::HollowBlock },
                color: content.colors[NamedColor::Cursor].map(Rgb::from).unwrap_or(theme.cursor),
            });

        // Block cursor: draw the glyph under it in the background color.
        if let Some(c) = &cursor
            && c.shape == CursorShape::Block {
                let cell = &mut cells[c.row * cols + c.col];
                cell.fg = cell.bg;
            }

        let background = content.colors[NamedColor::Background].map(Rgb::from).unwrap_or(theme.background);
        Frame { cols, rows, cells, cursor, background }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CellView {
    pub c: char,
    pub zerowidth: Option<String>,
    pub fg: Rgb,
    pub bg: Rgb,
    pub flags: Flags,
}

impl CellView {
    fn blank(theme: &Theme) -> Self {
        Self { c: ' ', zerowidth: None, fg: theme.foreground, bg: theme.background, flags: Flags::empty() }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub shape: CursorShape,
    pub color: Rgb,
}

pub struct Frame {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<CellView>,
    pub cursor: Option<Cursor>,
    pub background: Rgb,
}

impl Frame {
    pub fn row(&self, r: usize) -> &[CellView] {
        &self.cells[r * self.cols..(r + 1) * self.cols]
    }
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
                let i = if is_fg && i < 8 && flags.contains(Flags::BOLD) { i + 8 } else { i };
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
