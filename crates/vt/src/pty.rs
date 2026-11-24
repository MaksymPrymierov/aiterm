//! PTY wrapper that lets the [`aiterm_intercept::Scanner`] observe every byte
//! read from the child before alacritty_terminal's parser sees it.

use std::fs::File;
use std::io::{self, Read};
use std::sync::Arc;

use aiterm_intercept::{Scanner, Sequence};
use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{ChildEvent, EventedPty, EventedReadWrite, Pty};
use polling::{Event, PollMode, Poller};

pub(crate) type SequenceSink = Box<dyn FnMut(Sequence) + Send>;

pub(crate) struct TapReader {
    /// A `dup` of the PTY master fd; shares the non-blocking file status.
    file: File,
    scanner: Scanner,
    sink: SequenceSink,
}

impl Read for TapReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        self.scanner.feed(&buf[..n], &mut self.sink);
        Ok(n)
    }
}

pub(crate) struct TapPty {
    inner: Pty,
    reader: TapReader,
}

impl TapPty {
    pub(crate) fn new(inner: Pty, sink: SequenceSink) -> io::Result<Self> {
        let file = inner.file().try_clone()?;
        Ok(Self {
            inner,
            reader: TapReader {
                file,
                scanner: Scanner::new(),
                sink,
            },
        })
    }
}

impl EventedReadWrite for TapPty {
    type Reader = TapReader;
    type Writer = File;

    unsafe fn register(&mut self, poll: &Arc<Poller>, interest: Event, mode: PollMode) -> io::Result<()> {
        // SAFETY: the inner PTY owns the registered fds and lives as long as `self`.
        unsafe { self.inner.register(poll, interest, mode) }
    }

    fn reregister(&mut self, poll: &Arc<Poller>, interest: Event, mode: PollMode) -> io::Result<()> {
        self.inner.reregister(poll, interest, mode)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.inner.deregister(poll)
    }

    fn reader(&mut self) -> &mut TapReader {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut File {
        self.inner.writer()
    }
}

impl EventedPty for TapPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.inner.next_child_event()
    }
}

impl OnResize for TapPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        self.inner.on_resize(window_size);
    }
}
