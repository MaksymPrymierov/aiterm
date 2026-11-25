# aiterm

[![CI](https://github.com/MaksymPrymierov/aiterm/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/MaksymPrymierov/aiterm/actions/workflows/ci.yml)

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
- [Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/) with all progressive enhancement flags (disambiguate, event types, alternate keys, all keys as escape codes, associated text): Shift+Enter, Ctrl+Enter, Alt combinations and key releases reach applications unambiguously
- xterm-style keyboard encoding for applications that don't enable the kitty protocol; Ctrl shortcuts also work in non-Latin layouts (Ctrl+С on a Ukrainian layout sends Ctrl+C)
- Mouse reporting for applications (SGR and X10): vim, htop, mc
- Mouse selection: simple, word (double-click), line (triple-click), block (Ctrl)
- Clipboard and primary selection (copy on select, paste with middle click)
- Bracketed paste
- Desktop notifications from applications via OSC 9 (iTerm2), OSC 777 (rxvt/VTE/Ghostty) and OSC 99 (kitty protocol: chunked/base64 payloads, urgency, occasion, close, capability queries, activation and close reports), delivered through the freedesktop notification service over D-Bus; OSC 9/777 only notify while the window is unfocused, with a rate limit of 5 per 10 s
- Synchronized output (DEC mode 2026): applications that wrap redraws in `CSI ? 2026 h` … `CSI ? 2026 l` (like Claude Code) never show a half-drawn frame; support is reported via DECRQM, and an unterminated update times out after 150 ms
- Frame pacing tied to the compositor: at most one render per display refresh and none while the window is hidden; heavy output (e.g. `seq 1 10000000`) keeps full frame rate with ~4 ms key-to-application latency
- 50,000-line scrollback; the view stays put when new output arrives while you are scrolled up
- OSC 52 (clipboard from applications), color queries, window title

### Roadmap

| Version | Scope |
|---|---|
| 0.2 | ~~Kitty keyboard protocol~~ (done), ~~synchronized output~~ (done), ~~desktop notifications (OSC 9/777/99)~~ (done), progress (OSC 9;4), OSC 7/8/133 |
| 0.3 | Tabs and splits, per-tab session status indicator |
| 0.4 | Inline images: kitty graphics protocol, Sixel, iTerm2 inline images |
| 0.5 | TOML config with hot reload, themes, GUI color editor, command palette |
| 0.6 | Claude Code hooks integration, paste screenshot from clipboard, clickable `file:line` |
| 0.7 | Custom box-drawing rendering, ligatures, fractional scaling, IME preedit |
| 0.8–1.0 | Packages (AUR, Flatpak), compatibility testing, stable release |

## Installation

Prebuilt x86_64 Linux binaries are attached to every [GitHub release](https://github.com/MaksymPrymierov/aiterm/releases):

```sh
tar -xzf aiterm-v*-x86_64-linux.tar.gz
./aiterm-v*-x86_64-linux/aiterm
```

To build from source you need Rust 1.85+ (edition 2024). Running requires a Vulkan (or GL) driver, a Wayland compositor and `fontconfig`.

```sh
git clone https://github.com/MaksymPrymierov/aiterm.git
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
| <kbd>Shift</kbd>+<kbd>Enter</kbd> | Newline in Claude Code: `CSI 13;2u` with the kitty protocol, `ESC CR` otherwise |
| <kbd>Shift</kbd> + mouse | Select text even when an application captures the mouse |

### Configuration

There is no config file yet (planned for 0.5). For now, use environment variables:

| Variable | Default | Description |
|---|---|---|
| `AITERM_FONT` | a monospace font with Nerd Font icons (via `fc-match`) | Font family name |
| `AITERM_FONT_SIZE` | `13` | Font size in points |
| `RUST_LOG` | `warn` | Log level (`info`, `debug`) |

Child processes get `TERM=xterm-256color`, `COLORTERM=truecolor` and `TERM_PROGRAM=aiterm`.

### Claude Code notifications

Claude Code picks its notification protocol from the terminal it detects, and doesn't know aiterm yet. Set the channel once, in `/config` → Notifications or in `~/.claude/settings.json`:

```json
{ "preferredNotifChannel": "kitty" }
```

`kitty` (OSC 99), `iterm2` (OSC 9) and `ghostty` (OSC 777) all work. Clicking a notification asks the compositor to focus aiterm; with winit 0.30 on Wayland this is an attention request, so some compositors highlight the window instead of focusing it.

## Architecture

The project is a Cargo workspace:

```
crates/
├── app/        aiterm           — binary: winit event loop, keyboard/mouse input, clipboard
├── vt/         aiterm-vt        — Session (PTY + VT state), Frame snapshots, theme, events
├── render/     aiterm-render    — wgpu renderer: backgrounds, decorations, cursor, text
└── intercept/  aiterm-intercept — streaming scanner for OSC/APC/DCS sequences (no dependencies)
```

Dependency graph: `app → render → vt → intercept` (and `app → vt`). Only `vt` depends on `alacritty_terminal`.

Key design decisions:

- **VT core:** [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal) (parser, grid, scrollback, PTY), wrapped by `aiterm-vt`. Its public API (`Session`, `Frame`, `VtEvent`, `Modes`) uses only its own types, so the core can be swapped (e.g. for libghostty-vt) without touching the app or the renderer.
- **Sequence interception:** the PTY reader is wrapped so that `aiterm-intercept` sees every byte before the VT parser. Sequences the parser ignores (OSC 7/9/99/133/777/1337, kitty graphics APC, Sixel DCS) are delivered as `VtEvent::Sequence`; this is the foundation for notifications, progress, shell integration and inline images.
- **Text:** [glyphon](https://crates.io/crates/glyphon) / cosmic-text. Each row is split into segments: ASCII runs are shaped together, while every non-ASCII glyph (emoji, icons, CJK) is pinned to its own cell, so fallback fonts never shift the grid. Segments are cached per row.
- **Frames:** rendering happens only on a PTY `Wakeup` and is synced to vsync (`PresentMode::Fifo`), so bursts of updates coalesce into a single frame.

## Development

```sh
cargo build            # debug build
cargo test             # unit tests
cargo fmt              # format (max width 120, see rustfmt.toml)
cargo clippy           # linter (must be warning-free)
RUST_LOG=info cargo run -- -e bash
RUST_LOG=aiterm::perf=debug cargo run --release   # fps, wakeups, snapshot/render time per second
```

### CI and releases

- **CI** (`.github/workflows/ci.yml`) runs on pushes to `master` and on pull requests into `master`: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` and a release build. The binary is uploaded as a workflow artifact.
- **Releases** (`.github/workflows/release.yml`) run when a `vX.Y.Z` tag is pushed. The workflow checks that the tag matches the crate version, then builds, packages `aiterm` with `README.md` and `LICENSE`, and publishes a GitHub release with a SHA-256 checksum. Tags with a suffix (e.g. `v0.2.0-beta.1`) become pre-releases.

To cut a release:

```sh
# bump `version` in the root Cargo.toml, then:
cargo build && git commit -am "Release v0.2.0"
git tag v0.2.0
git push origin master v0.2.0
```

## Known limitations

- Box-drawing frames have gaps between rows (fixed in 0.7).
- Without CJK fonts installed, CJK characters render as boxes: install e.g. `noto-fonts-cjk`.
- No IME preedit: committed text is entered, but in-progress composition is not shown.
- One window, one session.

## License

[MIT](LICENSE)
