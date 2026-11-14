//! Micro-benchmark: cost of re-rendering a transcript container with N
//! assistant messages. Run with:
//!   cargo run --release -p rpi-tui --example bench_transcript -- 200 [stream]
use std::sync::Arc;
use std::time::Instant;

use rpi_tui::{
    AssistantBlock, AssistantMessageComponent, AssistantMessageOptions, Component, Container,
    Spacer, UserMessageComponent,
};

fn sample_markdown(i: usize) -> String {
    format!(
        "## Section {i}\n\n\
This is paragraph one for message {i}. It has some **bold** and `inline code` plus a \
[link](https://example.com) so the inline parser has real work to do.\n\n\
- first bullet\n- second bullet\n- third bullet\n\n\
```rust\nfn demo_{i}() -> usize {{\n    let mut total = 0;\n    for k in 0..10 {{\n        total += k;\n    }}\n    total\n}}\n```\n\n\
Another trailing paragraph with enough words to force the wrapper to actually \
measure and break lines at realistic width.\n"
    )
}

fn build(n: usize) -> (Arc<Container>, Arc<AssistantMessageComponent>) {
    let chat = Arc::new(Container::new());
    let mut last = None;
    for i in 0..n {
        chat.add_child(Arc::new(UserMessageComponent::new(format!(
            "user prompt number {i} — please do the thing"
        ))));
        chat.add_child(Arc::new(Spacer::new(1)));
        let comp = Arc::new(AssistantMessageComponent::new(
            AssistantMessageOptions::default(),
        ));
        comp.update_blocks(&[AssistantBlock::Text(sample_markdown(i))]);
        comp.set_streaming(false);
        chat.add_child(comp.clone());
        chat.add_child(Arc::new(Spacer::new(1)));
        last = Some(comp);
    }
    (chat, last.unwrap())
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let streaming = std::env::args().nth(2).as_deref() == Some("stream");
    let width = 120;
    let frames = 30;

    let (chat, last) = build(n);
    let chat: Arc<dyn Component> = chat;
    let _ = chat.render(width); // warmup

    let start = Instant::now();
    let mut total_lines = 0usize;
    for f in 0..frames {
        if streaming {
            last.update_blocks(&[AssistantBlock::Text(sample_markdown(n + f))]);
        }
        total_lines += chat.render(width).len();
    }
    let elapsed = start.elapsed();
    let per_frame = elapsed.as_secs_f64() * 1000.0 / frames as f64;
    println!(
        "messages={n}  mode={}  width={width}  frames={frames}  lines/frame={}  render/frame={:.2} ms",
        if streaming { "stream" } else { "idle" },
        total_lines / frames,
        per_frame
    );
}
