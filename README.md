# aiterm

A modern GPU-accelerated terminal emulator for Linux (Wayland), built for working with AI coding agents, especially [Claude Code](https://code.claude.com).

> **Status: early development (v0.1, MVP).** aiterm runs a shell and renders TUI applications correctly, but tabs, configuration and inline images are not implemented yet. See the [roadmap](#roadmap).

## Why another terminal

Popular terminals are not well suited to agent workflows:

- **Flicker and scroll jumps** while Claude Code streams output (GNOME Terminal/VTE, Alacritty, tmux).
- **Shift+Enter is indistinguishable from Enter**, so you need `/terminal-setup` or Ctrl+J.
- **Notifications get lost** (especially through tmux), and you can't see the state of a background session.
- **Inline images** are supported inconsistently and break in split panes.
- No tabs (Alacritty, foot), or configuration is complex (Lua in WezTerm, a large `kitty.conf`).

The goal of aiterm is for all of this to work out of the box, without tmux and without manual setup.

## Features

### Working today (v0.1)

- Native Wayland, HiDPI, GPU rendering (wgpu)
- 16 / 256 / truecolor; bold, italic, underline, strikethrough, dim, inverse
- Cursor shapes: block, beam, underline, hollow block (when unfocused)
- Emoji and Nerd Font icons are pinned to the cell grid and never shift the line
- xterm-style keyboard encoding (modifiers, function keys, application cursor mode)
- Mouse reporting for applications (SGR and X10): vim, htop, mc
- Mouse selection: simple, word (double-click), line (triple-click), block (Ctrl)
- Clipboard and primary selection (copy on select, paste with middle click)
- Bracketed paste
- 50,000-line scrollback; the view stays put when new output arrives while you are scrolled up
- OSC 52 (clipboard from applications), color queries, window title

### Roadmap

| Version | Scope |
|---|---|
| 0.2 | Kitty keyboard protocol (Shift+Enter with no setup), synchronized output, desktop notifications (OSC 9/777/99), progress (OSC 9;4), OSC 7/8/133 |
| 0.3 | Tabs and splits, per-tab session status indicator |
| 0.4 | Inline images: kitty graphics protocol, Sixel, iTerm2 inline images |
| 0.5 | TOML config with hot reload, themes, GUI color editor, command palette |
| 0.6 | Claude Code hooks integration, paste screenshot from clipboard, clickable `file:line` |
| 0.7 | Custom box-drawing rendering, ligatures, fractional scaling, IME preedit |
| 0.8–1.0 | Packages (AUR, Flatpak), compatibility testing, stable release |

## Installation

Requirements: Rust 1.85+ (edition 2024), a Vulkan (or GL) driver, a Wayland compositor and `fontconfig`.

```sh
git clone <repo-url> aiterm
cd aiterm
cargo build --release
./target/release/aiterm
```

## Usage

```sh
aiterm                  # start the default shell ($SHELL)
aiterm -e claude        # run a command instead of the shell
aiterm -e htop
```

### Key bindings

| Keys | Action |
|---|---|
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>C</kbd> | Copy selection |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>V</kbd> | Paste |
| Middle click | Paste primary selection |
| <kbd>Shift</kbd>+<kbd>PgUp</kbd> / <kbd>PgDn</kbd> | Scroll history |
| <kbd>Ctrl</kbd>+<kbd>+</kbd> / <kbd>-</kbd> / <kbd>0</kbd> | Increase / decrease / reset font size |
| <kbd>Shift</kbd>+<kbd>Enter</kbd> | Newline in Claude Code (currently sends `ESC CR`) |
| <kbd>Shift</kbd> + mouse | Select text even when an application captures the mouse |

### Configuration

There is no config file yet (planned for 0.5). For now, use environment variables:

| Variable | Default | Description |
|---|---|---|
| `AITERM_FONT` | a monospace font with Nerd Font icons (via `fc-match`) | Font family name |
| `AITERM_FONT_SIZE` | `13` | Font size in points |
| `RUST_LOG` | `warn` | Log level (`info`, `debug`) |

Child processes get `TERM=xterm-256color`, `COLORTERM=truecolor` and `TERM_PROGRAM=aiterm`.

## Architecture

```
crates/aiterm/src/
├── main.rs        — winit event loop: keyboard, mouse, clipboard, terminal events
├── session.rs     — PTY + VT state (alacritty_terminal), Frame snapshot
├── input.rs       — key-to-bytes encoding for the PTY, bracketed paste
├── theme.rs       — color theme and 256-color palette
└── render/
    ├── mod.rs     — wgpu renderer: backgrounds, decorations, cursor, text (glyphon)
    └── quad.wgsl  — rectangle shader
```

Key design decisions:

- **VT core:** [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal) (parser, grid, scrollback, PTY). The renderer only sees a `Frame` snapshot, so the core can be swapped (e.g. for libghostty-vt) without touching the UI.
- **Text:** [glyphon](https://crates.io/crates/glyphon) / cosmic-text. Each row is split into segments: ASCII runs are shaped together, while every non-ASCII glyph (emoji, icons, CJK) is pinned to its own cell, so fallback fonts never shift the grid. Segments are cached per row.
- **Frames:** rendering happens only on a PTY `Wakeup` and is synced to vsync (`PresentMode::Fifo`), so bursts of updates coalesce into a single frame.

## Development

```sh
cargo build            # debug build
cargo test             # unit tests
cargo clippy           # linter (must be warning-free)
RUST_LOG=info cargo run -- -e bash
```

## Known limitations

- Box-drawing frames have gaps between rows (fixed in 0.7).
- Without CJK fonts installed, CJK characters render as boxes: install e.g. `noto-fonts-cjk`.
- No IME preedit: committed text is entered, but in-progress composition is not shown.
- One window, one session.

## License

[MIT](LICENSE)
