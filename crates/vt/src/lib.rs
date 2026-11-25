//! Terminal sessions for aiterm.
//!
//! [`Session`] owns a PTY, the VT state and its I/O thread, and produces
//! [`Frame`] snapshots for the renderer. Raw PTY output is also observed by
//! [`aiterm_intercept`] to surface sequences the VT parser ignores
//! ([`VtEvent::Sequence`]).

mod frame;
mod pty;
mod session;
mod theme;

pub use aiterm_intercept::{Sequence, notification, progress};
pub use frame::{CellFlags, CellView, Cursor, CursorShape, Frame};
pub use session::{ClipboardKind, GridSize, Modes, Reply, Scroll, SelectionKind, Session, Side, TextReply, VtEvent};
pub use theme::{Rgb, Theme};
