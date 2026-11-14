use anyhow::Result;
use gpui::{
    AppContext, Context, Edges, Entity, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, Styled, Window, div, px,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use terminal::pty_session::{PendingPty, PreparedPty, PtyEvent};
use terminal::{ColorPalette, TerminalConfig, TerminalView};

struct TerminalApp {
    terminal: Entity<TerminalView>,
}

impl TerminalApp {
    fn new(terminal: Entity<TerminalView>) -> Self {
        Self { terminal }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;

        if keystroke.modifiers.control && (keystroke.key == "+" || keystroke.key == "=") {
            self.terminal.update(cx, |terminal, cx| {
                let mut config = terminal.config().clone();
                config.font_size += px(1.0);
                terminal.update_config(config, cx);
            });
            cx.stop_propagation();
        } else if keystroke.modifiers.control && keystroke.key == "-" {
            self.terminal.update(cx, |terminal, cx| {
                let mut config = terminal.config().clone();

                if config.font_size > px(6.0) {
                    config.font_size -= px(1.0);
                    terminal.update_config(config, cx);
                }
            });
            cx.stop_propagation();
        }
    }
}

impl Render for TerminalApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .on_key_down(cx.listener(Self::on_key_down))
            .child(self.terminal.clone())
    }
}

fn main() -> Result<()> {
    let app = gpui_platform::application();

    app.run(move |cx| {
        let PendingPty {
            reader,
            writer,
            control,
            events,
        } = PendingPty::spawn(|| {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            let pair = native_pty_system().openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })?;
            let mut cmd = CommandBuilder::new(&shell);
            cmd.env("TERM", "xterm-256color");
            cmd.env("COLORTERM", "truecolor");
            PreparedPty::spawn(pair, cmd)
        });
        cx.spawn(async move |cx| {
            let colors = ColorPalette::builder()
                .background(0x16, 0x16, 0x17)
                .foreground(0xC9, 0xC7, 0xCD)
                .cursor(0xC9, 0xC7, 0xCD)
                .black(0x10, 0x10, 0x10)
                .red(0xEF, 0xA6, 0xA2)
                .green(0x80, 0xC9, 0x90)
                .yellow(0xA6, 0x94, 0x60)
                .blue(0xA3, 0xB8, 0xEF)
                .magenta(0xE6, 0xA3, 0xDC)
                .cyan(0x50, 0xCA, 0xCD)
                .white(0x80, 0x80, 0x80)
                .bright_black(0x39, 0x41, 0x4E)
                .bright_red(0xE0, 0xAF, 0x85)
                .bright_green(0x5A, 0xCC, 0xAF)
                .bright_yellow(0xC8, 0xC8, 0x74)
                .bright_blue(0xCC, 0xAC, 0xED)
                .bright_magenta(0xF2, 0xA1, 0xC2)
                .bright_cyan(0x74, 0xC3, 0xE4)
                .bright_white(0xC0, 0xC0, 0xC0)
                .build();

            let config = TerminalConfig {
                font_family: "Mononoki Nerd Font".into(),
                font_size: px(14.0),
                cols: 80,
                rows: 24,
                scrollback: 10000,
                line_height_multiplier: 1.05,
                padding: Edges::all(px(8.0)),
                colors,
                copy_on_select: true,
                ..TerminalConfig::default()
            };

            let resize_callback = move |cols: usize, rows: usize| {
                control.resize(PtySize {
                    cols: cols as u16,
                    rows: rows as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            };

            cx.open_window(
                gpui::WindowOptions {
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Terminal".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let terminal = cx.new(|cx| {
                        let mut view = TerminalView::new(writer, reader, config, cx)
                            .with_resize_callback(resize_callback)
                            .with_exit_callback(|_window, cx| {
                                cx.quit();
                            });
                        view.set_capture_output(false);
                        view
                    });
                    let weak = terminal.downgrade();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        while let Ok(event) = events.recv().await {
                            let result = weak.update(cx, |terminal, cx| match event {
                                PtyEvent::Ready { .. } => {}
                                PtyEvent::Exited(status) => {
                                    terminal.record_child_exit(status.exit_code() as i32, cx)
                                }
                                PtyEvent::Error(error) => terminal.report_io_error(error, cx),
                            });
                            if result.is_err() {
                                break;
                            }
                        }
                    })
                    .detach();

                    let focus = terminal.read(cx).focus_handle().clone();
                    focus.focus(window, cx);

                    cx.new(|_cx| TerminalApp::new(terminal))
                },
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });

    Ok(())
}
