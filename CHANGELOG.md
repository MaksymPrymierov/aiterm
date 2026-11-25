# Changelog

All notable changes to aiterm are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/). The release workflow
publishes the section of the tagged version as the GitHub release notes.

## [0.2.0] - 2026-11-26

Terminal protocols for working with Claude Code and other modern TUI applications.

### Added

- Kitty keyboard protocol with all progressive enhancement flags (disambiguate,
  event types, alternate keys, all keys as escape codes, associated text).
  Shift+Enter, Ctrl+Enter and key releases reach applications unambiguously.
- Desktop notifications from OSC 9 (iTerm2), OSC 777 (rxvt/VTE/Ghostty) and
  OSC 99 (kitty: chunked and base64 payloads, urgency, occasion, close,
  capability and alive queries, activation and close reports) through the
  freedesktop notification service over D-Bus. OSC 9/777 notify only while the
  window is unfocused; at most 5 notifications per 10 seconds.
- Progress reporting (`OSC 9 ; 4`) as a bar along the top edge: normal, error,
  paused and animated indeterminate states; stale reports clear after 15 seconds.
- Hyperlinks (OSC 8): underlined, highlighted on hover with the real target
  shown in the bottom-left corner; Ctrl+click opens `http(s)`, `mailto` and
  local `file://` links.
- Shell integration: OSC 7 working directory and OSC 133 prompt marks, shown in
  the left margin.
- Shortcuts: Ctrl+Shift+Z / Ctrl+Shift+X jump to the previous / next prompt,
  Ctrl+Shift+N opens a new window in the current directory.
- Frame statistics with `RUST_LOG=aiterm::perf=debug`.

### Changed

- The codebase is a Cargo workspace: `app`, `vt`, `render` and `intercept`.
  Only `aiterm-vt` depends on alacritty_terminal.
- Rendering is paced by Wayland frame callbacks (one frame per display refresh,
  none while hidden) and prefers Mailbox presentation; PTY wakeups are
  coalesced to one per frame. Under heavy output the render thread is idle
  over 90% of the time and key-to-application latency dropped from 8–15 ms
  to about 4 ms.
- App shortcuts match physical keys, so they work in non-Latin keyboard
  layouts; Ctrl+letter also falls back to the US layout key in legacy mode.

### Security

- OSC 52 clipboard writes are accepted only while the window is focused and up
  to 1 MiB; applications can never read the clipboard.
- Hyperlinks open only allow-listed schemes, `file://` only for this host, via
  `xdg-open` without a shell. OSC 7 is accepted only for this host.
- Notification text is escaped for servers that interpret markup.

### Verified

- Synchronized output (DEC 2026) never shows a torn frame; PTY-based tests
  include a control experiment that proves tearing is observable.
- The OSC/APC/DCS scanner agrees with the vte parser on 300,000 random,
  randomly chunked streams.

## [0.1.0] - 2026-11-25

First release: a working single-window terminal.

### Added

- Native Wayland window with GPU rendering (wgpu, glyphon), HiDPI.
- PTY and VT emulation through alacritty_terminal; 16/256/truecolor, text
  styles, cursor shapes; emoji and Nerd Font icons pinned to the cell grid.
- xterm keyboard and mouse reporting, mouse selection, clipboard and primary
  selection, bracketed paste, 50,000-line scrollback.
- `aiterm -e <command>`, `AITERM_FONT` and `AITERM_FONT_SIZE`.
- CI on pushes and pull requests; releases built from `vX.Y.Z` tags.

[0.2.0]: https://github.com/MaksymPrymierov/aiterm/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/MaksymPrymierov/aiterm/releases/tag/v0.1.0
