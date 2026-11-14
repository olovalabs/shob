//! Alternate transcript presentation driven by voice's session status mailbox.
use super::*;
use rpi_tui::ansi::visible_width;
use rpi_tui::utils::{truncate_to_width, wrap_text_with_ansi};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub(super) const PET_KEY: &str = "rpi.pet";

#[derive(serde::Deserialize)]
struct PetState {
    version: u32,
    name: String,
    animal: String,
    mood: String,
    caption: String,
    speaker: String,
    hint: String,
    auto: bool,
    #[serde(default)]
    layout: String,
    #[serde(default)]
    meter: String,
}

fn read_state(mailbox: &rpi_extensions::ExtensionStatusMailbox) -> Option<PetState> {
    let state: PetState = serde_json::from_str(&mailbox.get(PET_KEY)?).ok()?;
    (state.version == 1).then_some(state)
}

pub(super) fn active(mailbox: &rpi_extensions::ExtensionStatusMailbox) -> bool {
    read_state(mailbox).is_some()
}

pub(super) struct PetDocument {
    transcript: Arc<dyn Component>,
    mailbox: rpi_extensions::ExtensionStatusMailbox,
    started: std::time::Instant,
    height: AtomicUsize,
    last_frame: AtomicU64,
}

impl PetDocument {
    pub(super) fn new(
        transcript: Arc<dyn Component>,
        mailbox: rpi_extensions::ExtensionStatusMailbox,
    ) -> Self {
        Self {
            transcript,
            mailbox,
            started: std::time::Instant::now(),
            height: AtomicUsize::new(22),
            last_frame: AtomicU64::new(u64::MAX),
        }
    }

    pub(super) fn tick(&self, height: usize) -> bool {
        let changed = self.height.swap(height.max(1), Ordering::Relaxed) != height.max(1);
        if !active(&self.mailbox) {
            return false;
        }
        let frame = self.started.elapsed().as_millis() as u64 / 320;
        self.last_frame.swap(frame, Ordering::Relaxed) != frame || changed
    }
}

fn plain(text: &str) -> String {
    // Captions and names are data. Strip terminal control sequences before
    // adding trusted theme styles, including OSC and cursor movement.
    rpi_tui::strip_ansi(text)
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(1600)
        .collect()
}

fn fit(text: &str, width: usize) -> String {
    truncate_to_width(text, width, "")
}
fn center(text: &str, width: usize) -> String {
    format!(
        "{}{}",
        " ".repeat(width.saturating_sub(visible_width(text)) / 2),
        fit(text, width)
    )
}

fn pet_lines(state: &PetState, width: usize, height: usize, frame: u64) -> Vec<String> {
    if width == 0 || height == 0 {
        return vec![];
    }
    let colors = current_theme().colors;
    let name = plain(&state.name).chars().take(24).collect::<String>();
    let (eyes, label) = match state.mood.as_str() {
        "listen" => ("o.o", "认真听你说话"),
        "think" => ("-.-", "让我想一想"),
        "speak" => (if frame % 2 == 0 { "o.o" } else { "oOo" }, "正在回答你"),
        "happy" => ("^.^", "完成啦！"),
        "error" => (";.;", "遇到一点小问题"),
        _ => ("-.-", "安静陪着你"),
    };
    let eyes = if state.mood == "listen" && frame % 25 == 24 {
        "-.-"
    } else {
        eyes
    };
    let art = if width < 20 || height < 14 {
        vec![format!("({eyes})")]
    } else {
        vec![
            if state.animal == "bunny" {
                "     (\\_/)".into()
            } else {
                "     /\\_/\\".into()
            },
            format!("  .-( {eyes} )-."),
            " /   /   \\   \\".into(),
            "(___/     \\___)".into(),
            "     /   \\".into(),
            "    (_____)".into(),
        ]
    };
    let voice = if state.auto {
        "● 连续对话"
    } else {
        "○ 语音暂停"
    };
    let mut lines = vec![
        fit(
            &colors.accent.fg(&format!("rpi / pet · {name} · {voice}")),
            width,
        ),
        String::new(),
    ];
    for row in art {
        lines.push(center(&colors.accent.fg(&row), width));
    }
    lines.push(center(&colors.accent.fg(&name), width));
    let meter = match state.mood.as_str() {
        "listen" | "speak" => {
            if state.meter.is_empty() {
                "·"
            } else {
                &state.meter
            }
        }
        "think" => ["·", "· ·", "· · ·"][(frame % 3) as usize],
        "happy" => "♡",
        "error" => "!",
        _ => "z z Z",
    };
    lines.push(center(&format!("{label}  {meter}"), width));
    if height > 12 {
        lines.push(String::new());
    }
    let speaker = if state.speaker == "user" {
        "你"
    } else {
        &name
    };
    let caption = format!("{speaker} › {}", plain(&state.caption));
    let remaining = height.saturating_sub(lines.len() + 2);
    if remaining > 0 {
        let wrapped = wrap_text_with_ansi(&caption, width);
        let max = remaining.min(6);
        for (index, row) in wrapped.iter().take(max).enumerate() {
            let row = if index + 1 == max && wrapped.len() > max {
                format!("{}…", fit(row, width.saturating_sub(1)))
            } else {
                row.clone()
            };
            lines.push(fit(&row, width));
        }
    }
    if height > lines.len() + 1 {
        let hint = if state.hint.is_empty() {
            if state.auto {
                "说完停一下，我就会回答。"
            } else {
                "/pet auto 恢复语音"
            }
        } else {
            &state.hint
        };
        lines.push(fit(&colors.muted.fg(&plain(hint)), width));
    }
    lines.push(fit(
        &colors.muted.fg("/pet quiet 安静陪伴 · /pet off 返回聊天"),
        width,
    ));
    lines.truncate(height);
    lines
}

impl Component for PetDocument {
    fn render(&self, width: usize) -> Vec<String> {
        let Some(state) = read_state(&self.mailbox) else {
            return self.transcript.render(width);
        };
        let height = self.height.load(Ordering::Relaxed);
        let frame = self.started.elapsed().as_millis() as u64 / 320;
        if state.layout == "work" && width >= 70 {
            let pet_width = 28;
            let chat_width = width.saturating_sub(pet_width + 3);
            let document = self.transcript.render_document(chat_width);
            let start = document.len().saturating_sub(height);
            let chat = document.lines(start, height);
            let pet = pet_lines(&state, pet_width, height, frame);
            return (0..height)
                .map(|i| {
                    let left = chat.get(i).map(String::as_str).unwrap_or("");
                    let right = pet.get(i).map(String::as_str).unwrap_or("");
                    format!(
                        "{}{} │ {}",
                        fit(left, chat_width),
                        " ".repeat(chat_width.saturating_sub(visible_width(left))),
                        right
                    )
                })
                .collect();
        }
        let mut lines = pet_lines(&state, width, height, frame);
        // Center the pet stage in the available transcript viewport without
        // growing past it or disturbing the editor dock.
        if lines.len() < height {
            let padding = (height - lines.len()) / 2;
            lines.splice(1..1, std::iter::repeat(String::new()).take(padding));
        }
        lines
    }
    fn render_document(&self, width: usize) -> rpi_tui::component::RenderedDocument {
        if !active(&self.mailbox) {
            // Normal chat must retain shared historical blocks all the way to
            // ScrollView; flattening this wrapper defeats incremental reuse.
            self.transcript.render_document(width)
        } else {
            rpi_tui::component::RenderedDocument::from_lines(self.render(width))
        }
    }

    fn invalidate(&self) {
        self.transcript.invalidate();
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(mood: &str) -> String {
        serde_json::json!({"version":1,"name":"小鱼","animal":"cat","mood":mood,"caption":"这是一个很长的中文回复。".repeat(30),"speaker":"pet","hint":"voice: listening","auto":true}).to_string()
    }
    struct SharedOnlyTranscript;

    impl Component for SharedOnlyTranscript {
        fn render(&self, _: usize) -> Vec<String> {
            panic!("pet wrapper must not flatten the transcript");
        }
        fn render_document(&self, _: usize) -> rpi_tui::component::RenderedDocument {
            rpi_tui::component::RenderedDocument::from_lines(vec!["shared chat".into()])
        }
        fn invalidate(&self) {}
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn default_and_work_layouts_preserve_shared_transcript_rendering() {
        let mailbox = rpi_extensions::ExtensionStatusMailbox::new();
        let doc = PetDocument::new(Arc::new(SharedOnlyTranscript), mailbox.clone());
        assert_eq!(doc.render_document(80).lines(0, 1), ["shared chat"]);
        let mut state: serde_json::Value = serde_json::from_str(&snapshot("think")).unwrap();
        state["layout"] = "work".into();
        mailbox.set(PET_KEY, &state.to_string());
        assert!(doc
            .render_document(100)
            .lines(0, 22)
            .join("\n")
            .contains("shared chat"));
    }

    #[test]
    fn pet_switch_preserves_live_transcript_and_falls_back_on_bad_metadata() {
        let transcript = Arc::new(Text::new("original chat", 0, 0));
        let mailbox = rpi_extensions::ExtensionStatusMailbox::new();
        let doc = PetDocument::new(transcript, mailbox.clone());
        assert!(doc.render(80).join("\n").contains("original chat"));
        mailbox.set(PET_KEY, &snapshot("listen"));
        assert!(doc.render(80).join("\n").contains("认真听你说话"));
        assert!(!doc.render(80).join("\n").contains("original chat"));
        for bad in ["broken", r#"{"version":2}"#, ""] {
            mailbox.set(PET_KEY, bad);
            assert!(doc.render(80).join("\n").contains("original chat"));
        }
    }
    #[test]
    fn all_expressions_fit_narrow_and_short_viewports() {
        for mood in ["listen", "think", "speak", "happy", "sleep", "error"] {
            let state: PetState = serde_json::from_str(&snapshot(mood)).unwrap();
            for width in [1, 12, 20, 40, 80] {
                for height in [1, 8, 16, 24] {
                    let rows = pet_lines(&state, width, height, 1);
                    assert!(rows.len() <= height);
                    assert!(
                        rows.iter().all(|row| visible_width(row) <= width),
                        "{width}x{height}: {rows:?}"
                    );
                }
            }
        }
    }
    #[test]
    fn work_layout_keeps_chat_visible_and_status_data_out_of_footer() {
        let mailbox = rpi_extensions::ExtensionStatusMailbox::new();
        let mut state: serde_json::Value = serde_json::from_str(&snapshot("think")).unwrap();
        state["layout"] = "work".into();
        mailbox.set(PET_KEY, &state.to_string());
        mailbox.set("voice", "voice: listening");
        let doc = PetDocument::new(Arc::new(Text::new("original chat", 0, 0)), mailbox.clone());
        assert!(doc.render(100).join("\n").contains("original chat"));
        assert_eq!(mailbox.text_except(&[PET_KEY]), "voice: listening");
    }
}
