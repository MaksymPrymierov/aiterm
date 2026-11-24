//! Backend-independent, render-ready snapshot of the visible terminal.
//!
//! These types intentionally do not expose alacritty_terminal types, so the
//! renderer stays decoupled from the VT backend.

use bitflags::bitflags;

use crate::theme::Rgb;

bitflags! {
    /// Visual attributes the renderer needs. Inverse, hidden and dim are
    /// already resolved into `fg`/`bg` colors.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct CellFlags: u8 {
        const BOLD = 1;
        const ITALIC = 1 << 1;
        /// Any underline style (single, double, curly, dotted, dashed).
        const UNDERLINE = 1 << 2;
        const STRIKEOUT = 1 << 3;
        /// First cell of a double-width character.
        const WIDE_CHAR = 1 << 4;
        /// Placeholder cell after (or before, at line wrap) a wide character.
        const WIDE_SPACER = 1 << 5;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CellView {
    pub c: char,
    /// Combining characters attached to `c`.
    pub zerowidth: Option<String>,
    pub fg: Rgb,
    pub bg: Rgb,
    pub flags: CellFlags,
}

impl CellView {
    pub(crate) fn blank(fg: Rgb, bg: Rgb) -> Self {
        Self {
            c: ' ',
            zerowidth: None,
            fg,
            bg,
            flags: CellFlags::empty(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Beam,
    Underline,
    /// Outline, used when the window is unfocused.
    HollowBlock,
}

#[derive(Clone, Copy, Debug)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub shape: CursorShape,
    pub color: Rgb,
}

/// The visible grid, row-major, `cols * rows` cells.
pub struct Frame {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<CellView>,
    /// `None` when the cursor is hidden or scrolled out of view.
    pub cursor: Option<Cursor>,
    pub background: Rgb,
}

impl Frame {
    pub fn row(&self, r: usize) -> &[CellView] {
        &self.cells[r * self.cols..(r + 1) * self.cols]
    }
}
