//! Streaming support for color-scheme queries and palette change reports.

use crate::colors::ColorPalette;
use crate::event::{GpuiEventProxy, TerminalNotifierHandle};
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::{Params, Parser, Perform, ansi::Processor};

#[derive(Default)]
pub(crate) struct ThemeProtocol {
    parser: Parser,
    subscribed: bool,
}

#[derive(Clone, Copy)]
enum Action {
    Set(bool),
    QueryMode,
    QueryTheme,
    Reset,
}

#[derive(Default)]
struct Detector {
    action: Option<Action>,
}

impl Perform for Detector {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let mut params = params.iter();
        match (action, intermediates) {
            ('h' | 'l', [b'?']) => {
                if params.any(|param| param.first() == Some(&2031)) {
                    self.action = Some(Action::Set(action == 'h'));
                }
            }
            ('p', [b'?', b'$']) => {
                // Alacritty's DECRQM handler only examines the first parameter.
                if params.next().and_then(|param| param.first()) == Some(&2031) {
                    self.action = Some(Action::QueryMode);
                }
            }
            ('n', [b'?']) => {
                if params.next() == Some(&[996][..]) && params.next().is_none() {
                    self.action = Some(Action::QueryTheme);
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.action = Some(Action::Reset);
        }
    }

    fn terminated(&self) -> bool {
        self.action.is_some()
    }
}

impl ThemeProtocol {
    pub(crate) fn advance(
        &mut self,
        processor: &mut Processor,
        term: &mut Term<GpuiEventProxy>,
        notifier: &TerminalNotifierHandle,
        palette: &ColorPalette,
        bytes: &[u8],
    ) {
        let mut offset = 0;
        while offset < bytes.len() {
            let mut detector = Detector::default();
            let count = self
                .parser
                .advance_until_terminated(&mut detector, &bytes[offset..]);
            let end = offset + count;
            if let Some(action) = detector.action {
                // Forward everything before the final byte first. A DECRQM for
                // 2031 is canceled instead of dispatched, preventing Alacritty's
                // unsupported-mode response; all other bytes remain untouched.
                processor.advance(term, &bytes[offset..end - 1]);
                if matches!(action, Action::QueryMode) {
                    processor.advance(term, b"\x18");
                } else {
                    processor.advance(term, &bytes[end - 1..end]);
                }
                match action {
                    Action::Set(enabled) => self.subscribed = enabled,
                    Action::QueryMode => notifier.write(if self.subscribed {
                        b"\x1b[?2031;1$y"
                    } else {
                        b"\x1b[?2031;2$y"
                    }),
                    Action::QueryTheme => notifier.write(theme_report(palette)),
                    Action::Reset => self.subscribed = false,
                }
            } else {
                processor.advance(term, &bytes[offset..end]);
            }
            offset = end;
        }
    }

    pub(crate) fn palette_changed(
        &self,
        notifier: &TerminalNotifierHandle,
        palette: &ColorPalette,
    ) {
        if self.subscribed {
            notifier.write(theme_report(palette));
        }
    }
}

fn theme_report(palette: &ColorPalette) -> &'static [u8] {
    let background = palette.rgb_at(257);
    // Use the same perceived brightness convention as common OSC 11 clients.
    let brightness = 299 * u32::from(background.r)
        + 587 * u32::from(background.g)
        + 114 * u32::from(background.b);
    if brightness < 127_500 {
        b"\x1b[?997;1n"
    } else {
        b"\x1b[?997;2n"
    }
}
