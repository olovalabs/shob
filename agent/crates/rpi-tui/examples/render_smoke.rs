//! Non-interactive render smoke test: prints Markdown + a colored edit diff to
//! stdout so the ANSI output can be eyeballed in a real terminal without
//! launching the alt-screen interactive loop.
//!
//! Run: cargo run -p rpi-tui --example render_smoke

use rpi_tui::component::Component;
use rpi_tui::{render_diff, FooterComponent, Markdown, ToolExecutionComponent};

fn main() {
    let width = 72usize;

    // ---- Markdown sample: code block, list, bold, table, wide paragraph ----
    let md_src = "\
# Heading one

A wide paragraph that should wrap at the column boundary instead of \
overflowing or panicking — 中文测你好世界 mixed content with emoji 🦀.

## Code block

```rust
fn main() {
    let very_long_variable_name = some_thing.with_a_long_call().that_exceeds_the_available_column_budget_for_sure();
}
```

## Lists

- normal bullet
- [x] done task
- [ ] pending task

**bold** and *italic* and __strong__ and `code`.

## Table

| name | count |
|------|------:|
| alpha | 12 |
| beta  | 345 |
";
    println!("╔══ markdown ═════════════════════════════════════════════════════════╗");
    for line in Markdown::new(md_src, 1, 0).render(width) {
        println!("{}", line);
    }

    // ---- Diff sample: the shape edit_diff.rs::generate_diff_string emits ----
    let diff = " 1 fn main() {
 2     let x = 1;
-3     println!(\"hello\");
-4     let y = x + old;
+3     println!(\"hello world\");
+4     let y = x + new_name;
 5 }
 6 // trailing context";
    println!();
    println!("╔══ colored diff ════════════════════════════════════════════════════╗");
    for line in render_diff(diff, width) {
        println!("{}", line);
    }

    // ---- ToolExecutionComponent with attached diff ----
    let comp = ToolExecutionComponent::new("edit", r#"{"path":"src/main.rs"}"#);
    comp.set_result("Successfully replaced 2 block(s) in src/main.rs", false);
    comp.set_diff(render_diff(diff, width));
    println!();
    println!("╔══ ToolExecutionComponent (with diff) ═════════════════════════════╗");
    for line in comp.render(width) {
        println!("{}", line);
    }

    // ---- ToolExecutionComponent in error state (red bg tint + ✗ glyph) ----
    let err = ToolExecutionComponent::new("bash", r#"{"command":"rm -rf /"}"#);
    err.set_result("command not found: rm", true);
    println!();
    println!("╔══ ToolExecutionComponent (failed) ════════════════════════════════╗");
    for line in err.render(width) {
        println!("{}", line);
    }

    // ---- Footer: separator + [model] status row with right-aligned hints ----
    let footer = FooterComponent::new();
    footer.set_model("glm-5");
    footer.set_status("Working…");
    footer.set_thinking_level(Some("medium"));
    footer.set_hints(
        "Enter: Send | Shift+Enter: New line | Ctrl+C: Clear/Exit | Ctrl+P: Cycle | Ctrl+T: Expand",
    );
    println!();
    println!("╔══ Footer (working + thinking) ════════════════════════════════════╗");
    for line in footer.render(width) {
        println!("{}", line);
    }
}
