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
- `.github/workflows/release.yml` runs on `vX.Y.Z` tags: the tag must equal the workspace `version` in the root `Cargo.toml`, otherwise the release fails. Bump the version (and `Cargo.lock`) and add a `## [X.Y.Z] - date` section to `CHANGELOG.md` before tagging; that section becomes the release notes (followed by GitHub's generated change list).
- The release binary must stay free of system library link dependencies beyond libc (Wayland, xkbcommon and fonts are loaded at runtime), so the CI runner needs no extra packages.

## Code layout

Cargo workspace; dependency graph `app → render → vt → intercept` (and `app → vt`):

- `crates/app` (package `aiterm`, the binary)
  - `src/main.rs` — winit `ApplicationHandler`: window/input events, shortcuts, mouse reporting, clipboard, handling of `VtEvent`s.
  - `src/perf.rs` — frame statistics (fps, wakeups, snapshot/render time), logged once per second with `RUST_LOG=aiterm::perf=debug`.
  - `src/notifications.rs` — notification policy (focus/occasion rules, rate limit, OSC 99 replies and reports) behind a `Sink` trait so it is unit-tested without D-Bus.
  - `src/desktop_notify.rs` — freedesktop notifications over D-Bus (zbus 5 blocking API, background threads); listens for `ActionInvoked`/`NotificationClosed`.
  - `src/progress.rs` — per-session `OSC 9;4` progress state (keeps the last value for error/paused, expires after 15 s); the app arms `ControlFlow::WaitUntil` for the expiry and redraws every frame only while progress is indeterminate.
  - `src/links.rs` — OSC 8 link opening (scheme allow-list, `xdg-open` without a shell), new window in a directory, `/proc` cwd fallback.
  - `src/input.rs` — keyboard encoding: winit `KeyEvent` → platform-independent `KeyInput` → bytes, via the legacy xterm encoder or the kitty keyboard protocol encoder (selected by `Modes::KITTY_*`); paste handling. Heavily unit-tested.
- `crates/vt` (`aiterm-vt`) — the only crate that depends on `alacritty_terminal`.
  - `src/session.rs` — `Session` (PTY + `Term` + I/O thread), `VtEvent`, `Modes`, selection/scroll API, `Frame` snapshot; colors are resolved here.
  - `src/frame.rs` — backend-independent `Frame`, `CellView`, `CellFlags`, `Cursor`.
  - `src/pty.rs` — `TapPty`: wraps the alacritty PTY so the interceptor sees every byte read.
  - `src/theme.rs` — `Theme` and the 256-color palette.
- `crates/render` (`aiterm-render`) — wgpu renderer: instanced quads (`quad.wgsl`) for backgrounds, decorations, cursor and the `ProgressBar` overlay; glyphon for text.
- `crates/intercept` (`aiterm-intercept`) — dependency-free streaming scanner for OSC/APC/DCS sequences, plus `notification.rs` (OSC 9/777/99 parsers, including chunked kitty notifications), `progress.rs` (`OSC 9;4`) and `shell.rs` (OSC 7 cwd, OSC 133 prompt marks). OSC/DCS boundaries mirror the `vte` state machine; `tests/vte_differential.rs` checks this on 300k random streams with random chunking (vte is a dev-dependency only). `tests/throughput.rs` is an ignored benchmark (`cargo test --release -p aiterm-intercept --test throughput -- --ignored --nocapture`).

## Conventions

- **Keep the VT backend swappable.** Only `aiterm-vt` may depend on `alacritty_terminal`, and its public API must not expose alacritty types. The renderer consumes `Frame` from `Session::snapshot`; the app talks to `Session` and receives `VtEvent`s (libghostty-vt is the planned fallback backend).
- **Grid alignment.** Text rows are split into segments: ASCII runs are shaped together, and each non-ASCII glyph gets its own segment at its cell's x position. Don't merge non-ASCII glyphs into runs, because fallback-font advances would shift the grid.
- **Frame pacing.** Request a redraw (`window.request_redraw()`); never render synchronously from PTY events. The renderer calls `window.pre_present_notify()` before presenting, so winit delivers at most one `RedrawRequested` per compositor frame callback (one render per vblank, none while hidden), and prefers `PresentMode::Mailbox` so presenting never blocks the event loop. `VtEvent::Wakeup` is coalesced: after one wakeup, the next is sent only after `Session::snapshot` (`crates/vt/tests/wakeup.rs`).
- **PTY read budget.** alacritty_terminal's event loop parses at most 64 KiB per terminal lock, then releases the `FairMutex` and services input; measured under a flood this keeps 100 fps and ~4 ms key-to-app latency without throttling throughput, so there is no separate per-frame read budget. Re-measure with `RUST_LOG=aiterm::perf=debug` before adding one.
- **Synchronized output (DEC 2026)** is implemented by the vte parser: bytes between BSU and ESU are buffered and never reach `Term` until the update ends (or after a 150 ms timeout), and no `Wakeup` is sent meanwhile. Always render from `Session::snapshot` so this guarantee holds; `crates/vt/tests/sync_output.rs` guards it with a real PTY (including a control test that proves tearing is observable).
- **Colors.** Quad colors go to the shader in linear space (`Rgb::to_linear`); glyphon takes sRGB.
- **wgpu surface.** In the `Suboptimal` case, the surface texture must be presented or dropped before calling `surface.configure`.
- New terminal protocol support (OSC/APC/DCS sequences that alacritty_terminal ignores) goes through `aiterm-intercept`: add the OSC code to `FORWARDED_OSC` in `crates/vt/src/session.rs` and handle `VtEvent::Sequence` in the app. The scanner is an observer and must never modify the byte stream, and it must keep agreeing with vte about where OSC/DCS sequences start and end (any change to its state machine has to pass the differential test).
- **Keyboard.** The kitty flag stack (`CSI > u` push, `CSI < u` pop, `CSI = u`, `CSI ? u` query) is handled by alacritty_terminal (`Config::kitty_keyboard`); `input.rs` only encodes keys. Encoders take `KeyInput`, never winit types, so every encoding rule has a unit test. Follow the spec at https://sw.kovidgoyal.net/kitty/keyboard-protocol/ exactly; key events can be traced with `RUST_LOG=aiterm=trace`.
- **Notifications** must never block the UI thread: all D-Bus calls run on the notification threads, and a missing session bus only logs a warning. To test end to end, watch `dbus-monitor --session "interface='org.freedesktop.Notifications'"` while a probe script emits OSC 99, and close notifications with `busctl --user call ... CloseNotification`.
- **Prompt marks (OSC 133).** alacritty_terminal has no shell-integration support and no scroll counter, so marks are stored in the grid itself: on `133;B` (shell idle, cursor on the input line) `Session::mark_prompt` sets the spare cell-flag bit `0x8000` on the line's first cell. The mark moves with scrolling and reflow and vanishes when the line is cleared; `prompt_mark_bit_is_unused` fails if an alacritty_terminal upgrade claims that bit.
- **Untrusted input from the PTY.** Hyperlink targets, OSC 7 paths and OSC 52 data come from programs: open only allow-listed schemes, show the real URI on hover, accept OSC 7 only for this host, and accept OSC 52 writes only while focused and up to 1 MiB (reads are disabled).
- Prefer standard terminal protocols (kitty keyboard, OSC 9/777/99, OSC 9;4, OSC 133) over Claude Code-specific hacks.

## Testing changes visually

Run `cargo run -- -e <script>` with a script that prints the escape sequences you need. On niri, capture only the terminal window with `niri msg action screenshot-window --path <abs-path>`, never the whole screen.

To test keyboard input end to end, run a probe script in aiterm (raw mode, write `CSI > flags u`, record received bytes) and inject keys with `ydotool key <code>:1 <code>:0` (goes through the real keymap) or `wtype` (uses its own keymap, so Shift combinations are not realistic). Always check that the focused window is aiterm (`niri msg --json focused-window`) before injecting keys. Test windows take focus, so the user's own keystrokes can land in them; don't mistake that for terminal output. Don't `pkill -f <script name>` from the same shell command that mentions the script, it kills that shell too.
