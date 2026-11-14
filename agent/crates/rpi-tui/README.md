# rpi-tui

[![crates.io](https://img.shields.io/crates/v/rpi-tui.svg)](https://crates.io/crates/rpi-tui)
[![docs.rs](https://docs.rs/rpi-tui/badge.svg)](https://docs.rs/rpi-tui)

The terminal UI primitives behind the `rpi` interactive agent: a component
model, layout containers, an editor, transcript rendering, and both fullscreen
and main-screen terminal modes.

This is the rendering layer, not an extension point. Agents extend `rpi` by
registering tools and event handlers (see
[`rpi-plugin-sdk`](https://crates.io/crates/rpi-plugin-sdk)), and the CLI decides
how to draw them.

## Install

```toml
[dependencies]
rpi-tui = "0.3"
```

Built on `crossterm`; no async runtime required.

## What is here

| Area | Types |
| ---- | ----- |
| Component model | `Component`, `Focusable`, `Container`, `VStack`, `HStack`, `Box`, `Spacer` |
| Text and layout | `Text`, `StackEntry`, `StackChild`, `FollowMode`, width-aware wrapping |
| Input | `Editor`, `EditorOptions`, `EditorStyle`, `Input`, `keybindings`, `KillRing` |
| Transcript | `AssistantMessageComponent`, `BashExecutionComponent`, `diff::render_diff` |
| Markdown | `markdown`, `syntax_highlight`, `latex`, `mermaid` |
| Selection | `SelectList`, `SearchableSelect`, `AltScreenSearch`, `fuzzy` |
| Terminal | `ProcessTerminal`, `TuiAltScreen`, `TerminalInfo`, `terminal_image` |

## Two screen modes

- **Fullscreen** (alternate screen) — the application owns the viewport and
  paints a constrained layout. The default on Windows and Linux.
- **Main screen** — the terminal keeps ownership of scrollback and text
  selection, and the app redraws only changed lines. The default on macOS, so
  native selection and scrollback keep working.

The CLI exposes the choice as `--tui-mode regular|fullscreen`.

## Rendering model

A component returns the lines it wants drawn at a given width:

```rust
use rpi_tui::Component;

pub trait Component: Send + Sync {
    fn render(&self, width: usize) -> Vec<String>;

    /// Return `true` if the key sequence was consumed.
    fn handle_input(&self, _data: &str) -> bool {
        false
    }
}
```

The renderer diffs that against the previous frame and emits only the changed
line range. Unchanged frames produce no output at all, which is what keeps a
background status tick from yanking the viewport back to the bottom of a main-screen
session.

## Running the example

```bash
cargo run -p tui
```

Interactive demo with a scrollable transcript, an editor and keyboard
navigation — the source is at
[`examples/tui`](https://github.com/bigfish1913/pi-rust/tree/main/examples/tui).

## Documentation

- Crate docs: <https://docs.rs/rpi-tui>
- [TUI gap analysis](https://github.com/bigfish1913/pi-rust/blob/main/docs/README.md)

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).

`TuiAltScreen::set_scroll_to_end_indicator` accepts an optional label callback.
It displays the label on the last visible row of a follow-end primary
`ScrollView` while the user is reading earlier content. Hosts should call
`handle_scroll_to_end_indicator_mouse_event` before text-selection handling.
A primary-button press jumps to the end and resumes following output.
The indicator stays inside the transcript clip, preserves the scrollbar,
and does not appear in main-screen mode or over modal overlays.
