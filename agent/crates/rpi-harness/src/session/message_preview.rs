//! Shared, bounded user-message preview semantics for UI summaries.
use rpi_ai::types::{Content, UserContent};

pub(crate) fn preview(content: &UserContent) -> (String, bool) {
    let mut preview = String::new();
    let mut remaining = 2048usize;
    let mut count = 0;
    let mut append = |text: &str| {
        for ch in text.chars() {
            if remaining == 0 || count >= 140 {
                break;
            }
            remaining -= 1;
            if ch.is_whitespace() {
                if !preview.is_empty() && !preview.ends_with(' ') {
                    preview.push(' ');
                    count += 1;
                }
            } else {
                preview.push(ch);
                count += 1;
            }
        }
    };
    let has_images = match content {
        UserContent::Text(text) => {
            append(text);
            false
        }
        UserContent::Blocks(blocks) => {
            for block in blocks {
                if let Content::Text(text) = block {
                    append(&text.text);
                    append(" ");
                }
            }
            blocks
                .iter()
                .any(|block| matches!(block, Content::Image(_)))
        }
    };
    (preview.trim_end().to_owned(), has_images)
}
