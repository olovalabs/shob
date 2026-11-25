use gpui::SharedString;
use markdown::ParseOptions;

use crate::{
    highlighter::HighlightTheme,
    text::{
        AgentMarkdown, TextViewStyle,
        node::{Node, NodeContext},
    },
};

pub(crate) fn parse(
    raw: &str,
    style: &TextViewStyle,
    cx: &mut NodeContext,
    highlight_theme: &HighlightTheme,
) -> Result<Node, SharedString> {
    let mut options = ParseOptions::gfm();
    if style.agent_markdown == AgentMarkdown::User {
        options.gfm_strikethrough_single_tilde = false;
        options.constructs.html_flow = false;
        options.constructs.html_text = false;
    }
    super::markdown::parse_with_options(raw, style, cx, highlight_theme, &options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_messages_preserve_lines_paths_and_literal_html() {
        let style = TextViewStyle {
            agent_markdown: AgentMarkdown::User,
            ..Default::default()
        };
        let mut cx = NodeContext {
            style: style.clone(),
            ..Default::default()
        };
        let root = parse(
            "~/one ~two\n<em>text</em> ~~gone~~",
            &style,
            &mut cx,
            &style.highlight_theme,
        )
        .unwrap();
        let Node::Root { children } = root else {
            panic!("expected root")
        };
        let Node::Paragraph(paragraph) = &children[0] else {
            panic!("expected paragraph")
        };
        let text: String = paragraph
            .children
            .iter()
            .map(|node| node.text.as_str())
            .collect();
        assert_eq!(text, "~/one ~two\n<em>text</em> gone");
        assert!(
            paragraph
                .children
                .iter()
                .any(|node| node.text.as_str() == "gone" && node.marks[0].1.strikethrough)
        );
    }
}
