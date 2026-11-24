# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Project

aiterm is a GPU-accelerated terminal emulator for Linux/Wayland, designed for working with AI coding agents (Claude Code first). Stack: Rust (edition 2024), wgpu 30, glyphon 0.12 (cosmic-text), winit 0.30, alacritty_terminal 0.26. See `README.md` for features, roadmap and architecture.

## Language rules

- **All code comments, log messages, error messages and user-facing strings in code must be in English.**
- **All public documentation must be in English**: `README.md`, `CLAUDE.md`, doc comments, commit messages, and anything else that is committed to the repository.
- `docs/` is git-ignored and holds the maintainer's private planning notes; they may be in any language. Do not link to them from public docs.

## Commands

```sh
cargo build                          # debug build
cargo test                           # unit tests
cargo fmt                            # max_width = 120 (rustfmt.toml); CI runs `cargo fmt --check`
cargo clippy                         # must stay warning-free; CI runs it with `-D warnings`
RUST_LOG=info cargo run -- -e bash   # run with logging; `-e` runs a command instead of $SHELL
```

## CI and releases

- `.github/workflows/ci.yml` runs on pushes and pull requests to `master`: fmt check, clippy (`-D warnings`), tests, release build.
- `.github/workflows/release.yml` runs on `vX.Y.Z` tags: the tag must equal the workspace `version` in the root `Cargo.toml`, otherwise the release fails. Bump the version (and `Cargo.lock`) before tagging.
- The release binary must stay free of system library link dependencies beyond libc (Wayland, xkbcommon and fonts are loaded at runtime), so the CI runner needs no extra packages.

## Code layout

Cargo workspace; dependency graph `app → render → vt → intercept` (and `app → vt`):

- `crates/app` (package `aiterm`, the binary)
  - `src/main.rs` — winit `ApplicationHandler`: window/input events, shortcuts, mouse reporting, clipboard, handling of `VtEvent`s.
  - `src/input.rs` — keyboard encoding to PTY bytes and paste handling (with unit tests).
- `crates/vt` (`aiterm-vt`) — the only crate that depends on `alacritty_terminal`.
  - `src/session.rs` — `Session` (PTY + `Term` + I/O thread), `VtEvent`, `Modes`, selection/scroll API, `Frame` snapshot; colors are resolved here.
  - `src/frame.rs` — backend-independent `Frame`, `CellView`, `CellFlags`, `Cursor`.
  - `src/pty.rs` — `TapPty`: wraps the alacritty PTY so the interceptor sees every byte read.
  - `src/theme.rs` — `Theme` and the 256-color palette.
- `crates/render` (`aiterm-render`) — wgpu renderer: instanced quads (`quad.wgsl`) for backgrounds, decorations and cursor; glyphon for text.
- `crates/intercept` (`aiterm-intercept`) — dependency-free streaming scanner for OSC/APC/DCS sequences.

## Conventions

- **Keep the VT backend swappable.** Only `aiterm-vt` may depend on `alacritty_terminal`, and its public API must not expose alacritty types. The renderer consumes `Frame` from `Session::snapshot`; the app talks to `Session` and receives `VtEvent`s (libghostty-vt is the planned fallback backend).
- **Grid alignment.** Text rows are split into segments: ASCII runs are shaped together, and each non-ASCII glyph gets its own segment at its cell's x position. Don't merge non-ASCII glyphs into runs, because fallback-font advances would shift the grid.
- **Redraws.** Request a redraw (`window.request_redraw()`); never render synchronously from PTY events. Frames coalesce on vsync.
- **Colors.** Quad colors go to the shader in linear space (`Rgb::to_linear`); glyphon takes sRGB.
- **wgpu surface.** In the `Suboptimal` case, the surface texture must be presented or dropped before calling `surface.configure`.
- New terminal protocol support (OSC/APC/DCS sequences that alacritty_terminal ignores) goes through `aiterm-intercept`: add the OSC code to `FORWARDED_OSC` in `crates/vt/src/session.rs` and handle `VtEvent::Sequence` in the app. The scanner is an observer and must never modify the byte stream.
- Prefer standard terminal protocols (kitty keyboard, OSC 9/777/99, OSC 9;4, OSC 133) over Claude Code-specific hacks.

## Testing changes visually

Run `cargo run -- -e <script>` with a script that prints the escape sequences you need. On niri, capture only the terminal window with `niri msg action screenshot-window --path <abs-path>`, never the whole screen.
