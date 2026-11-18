use gpui::SharedString;
use markdown::{
    ParseOptions,
    mdast::{self, Node},
};

use crate::{
    highlighter::HighlightTheme,
    text::{
        TextViewStyle,
        node::{
            self, CodeBlock, ImageNode, InlineNode, LinkMark, NodeContext, Paragraph, Span, Table,
            TableRow, TextMark,
        },
    },
};

pub(crate) fn parse(
    raw: &str,
    style: &TextViewStyle,
    cx: &mut NodeContext,
    highlight_theme: &HighlightTheme,
) -> Result<node::Node, SharedString> {
    markdown::to_mdast(raw, &ParseOptions::gfm())
        .map(|root| {
            collect_definitions(&root, cx);
            ast_to_node(root, style, cx, highlight_theme)
        })
        .map_err(|e| e.to_string().into())
}

fn collect_definitions(node: &Node, cx: &mut NodeContext) {
    if let Node::Definition(def) = node {
        cx.add_ref(
            def.identifier.clone().into(),
            LinkMark {
                url: def.url.clone().into(),
                identifier: Some(def.identifier.clone().into()),
                title: def.title.clone().map(Into::into),
            },
        );
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_definitions(child, cx);
        }
    }
}

fn parse_table_row(table: &mut Table, node: &mdast::TableRow, cx: &mut NodeContext) {
    let mut row = TableRow {
        header: table.children.is_empty(),
        ..Default::default()
    };
    node.children.iter().for_each(|c| {
        match c {
            Node::TableCell(cell) => {
                parse_table_cell(&mut row, cell, cx);
            }
            _ => {}
        };
    });
    row.children.truncate(table.column_aligns.len());
    row.children
        .resize_with(table.column_aligns.len(), Default::default);
    table.children.push(row);
}

fn parse_table_cell(row: &mut node::TableRow, node: &mdast::TableCell, cx: &mut NodeContext) {
    let mut paragraph = Paragraph::default();
    node.children.iter().for_each(|c| {
        parse_paragraph(&mut paragraph, c, cx);
    });
    let table_cell = node::TableCell {
        children: paragraph,
        ..Default::default()
    };
    row.children.push(table_cell);
}

fn parse_paragraph(paragraph: &mut Paragraph, node: &mdast::Node, cx: &mut NodeContext) {
    if let Some(position) = node.position() {
        let span = Span {
            start: position.start.offset,
            end: position.end.offset,
        };
        paragraph.set_span(match paragraph.span {
            Some(previous) => Span {
                start: previous.start.min(span.start),
                end: previous.end.max(span.end),
            },
            None => span,
        });
    }
    parse_inline(paragraph, node, cx, &TextMark::default());
}

fn push_inline(paragraph: &mut Paragraph, text: &str, mark: &TextMark) {
    paragraph.push(InlineNode::new(text.to_owned()).marks(vec![(0..text.len(), mark.clone())]));
}

fn parse_inline(
    paragraph: &mut Paragraph,
    node: &mdast::Node,
    cx: &mut NodeContext,
    inherited: &TextMark,
) {
    match node {
        Node::Paragraph(val) => {
            for child in &val.children {
                parse_inline(paragraph, child, cx, inherited);
            }
        }
        Node::Text(val) => {
            if val.value.contains('\n') {
                push_inline(paragraph, &val.value.replace('\n', " "), inherited);
            } else {
                push_inline(paragraph, &val.value, inherited);
            }
        }
        Node::Emphasis(_) | Node::Strong(_) | Node::Delete(_) => {
            let mut mark = inherited.clone();
            match node {
                Node::Emphasis(_) => mark.italic = true,
                Node::Strong(_) => mark.bold = true,
                Node::Delete(_) => mark.strikethrough = true,
                _ => unreachable!(),
            }
            for child in node.children().unwrap() {
                parse_inline(paragraph, child, cx, &mark);
            }
        }
        Node::InlineCode(val) => push_inline(paragraph, &val.value, &inherited.clone().code()),
        Node::InlineMath(val) => push_inline(paragraph, &val.value, &inherited.clone().code()),
        Node::Break(_) => push_inline(paragraph, "\n", inherited),
        Node::Link(val) => {
            let mark = inherited.clone().link(LinkMark {
                url: val.url.clone().into(),
                title: val.title.clone().map(Into::into),
                ..Default::default()
            });
            for child in &val.children {
                parse_inline(paragraph, child, cx, &mark);
            }
        }
        Node::LinkReference(val) => {
            let mark = inherited.clone().link(LinkMark {
                identifier: Some(val.identifier.clone().into()),
                title: val.label.clone().map(Into::into),
                ..Default::default()
            });
            for child in &val.children {
                parse_inline(paragraph, child, cx, &mark);
            }
        }
        Node::Image(val) => paragraph.push_image(ImageNode {
            url: val.url.clone().into(),
            title: val.title.clone().map(Into::into),
            alt: Some(val.alt.clone().into()),
            link: inherited.link.clone(),
            ..Default::default()
        }),
        Node::ImageReference(val) => {
            if let Some(definition) = cx.link_refs.get(val.identifier.as_str()) {
                paragraph.push_image(ImageNode {
                    url: definition.url.clone().into(),
                    title: definition.title.clone(),
                    alt: Some(val.alt.clone().into()),
                    link: inherited.link.clone(),
                    ..Default::default()
                });
            }
        }
        Node::MdxTextExpression(val) => push_inline(paragraph, &val.value, inherited),
        Node::Html(val) => match super::html::parse(&val.value, cx) {
            Ok(el) if el.is_break() => push_inline(paragraph, "\n", inherited),
            Ok(el) => {
                if cfg!(debug_assertions) {
                    tracing::warn!("unsupported inline html tag: {:#?}", el);
                }
            }
            Err(err) => {
                if cfg!(debug_assertions) {
                    tracing::warn!("failed parsing html: {:#?}", err);
                }
                push_inline(paragraph, &val.value, inherited);
            }
        },
        Node::FootnoteReference(val) => {
            push_inline(
                paragraph,
                &format!("[{}]", val.identifier),
                &inherited.clone().italic(),
            );
        }
        _ => {
            if cfg!(debug_assertions) {
                tracing::warn!("unsupported inline node: {:#?}", node);
            }
        }
    }
}

fn ast_to_node(
    value: mdast::Node,
    style: &TextViewStyle,
    cx: &mut NodeContext,
    highlight_theme: &HighlightTheme,
) -> node::Node {
    match value {
        Node::Root(val) => {
            let children = val
                .children
                .into_iter()
                .map(|c| ast_to_node(c, style, cx, highlight_theme))
                .collect();
            node::Node::Root { children }
        }
        Node::Paragraph(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, cx);
            });

            node::Node::Paragraph(paragraph)
        }
        Node::Blockquote(val) => {
            let children = val
                .children
                .into_iter()
                .map(|c| ast_to_node(c, style, cx, highlight_theme))
                .collect();
            node::Node::Blockquote { children }
        }
        Node::List(list) => {
            let children = list
                .children
                .into_iter()
                .map(|c| ast_to_node(c, style, cx, highlight_theme))
                .collect();
            node::Node::List {
                ordered: list.ordered,
                start: list.start.unwrap_or(1) as usize,
                children,
            }
        }
        Node::ListItem(val) => {
            let children = val
                .children
                .into_iter()
                .map(|c| ast_to_node(c, style, cx, highlight_theme))
                .collect();
            node::Node::ListItem {
                children,
                spread: val.spread,
                checked: val.checked,
            }
        }
        Node::Break(_) => node::Node::Break { html: false },
        Node::Code(raw) => node::Node::CodeBlock(CodeBlock::new(
            raw.value.into(),
            raw.lang.map(|s| s.into()),
            style,
            highlight_theme,
        )),
        Node::Heading(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, cx);
            });

            node::Node::Heading {
                level: val.depth,
                children: paragraph,
            }
        }
        Node::Math(val) => node::Node::CodeBlock(CodeBlock::new(
            val.value.into(),
            None,
            style,
            highlight_theme,
        )),
        Node::Html(val) => match super::html::parse(&val.value, cx) {
            Ok(el) => el,
            Err(err) => {
                if cfg!(debug_assertions) {
                    tracing::warn!("error parsing html: {:#?}", err);
                }

                node::Node::Paragraph(Paragraph::new(val.value))
            }
        },
        Node::MdxFlowExpression(val) => node::Node::CodeBlock(CodeBlock::new(
            val.value.into(),
            Some("mdx".into()),
            style,
            highlight_theme,
        )),
        Node::Yaml(val) => node::Node::CodeBlock(CodeBlock::new(
            val.value.into(),
            Some("yml".into()),
            style,
            highlight_theme,
        )),
        Node::Toml(val) => node::Node::CodeBlock(CodeBlock::new(
            val.value.into(),
            Some("toml".into()),
            style,
            highlight_theme,
        )),
        Node::MdxJsxTextElement(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, cx);
            });
            node::Node::Paragraph(paragraph)
        }
        Node::MdxJsxFlowElement(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, cx);
            });
            node::Node::Paragraph(paragraph)
        }
        Node::ThematicBreak(_) => node::Node::Divider,
        Node::Table(val) => {
            let mut table = Table::default();
            table.column_aligns = val
                .align
                .clone()
                .into_iter()
                .map(|align| align.into())
                .collect();
            val.children.iter().for_each(|c| {
                if let Node::TableRow(row) = c {
                    parse_table_row(&mut table, row, cx);
                }
            });

            node::Node::Table(table)
        }
        Node::FootnoteDefinition(def) => {
            let mut paragraph = Paragraph::default();
            let prefix = format!("[{}]: ", def.identifier);
            paragraph.push(InlineNode::new(&prefix).marks(vec![(
                0..prefix.len(),
                TextMark {
                    italic: true,
                    ..Default::default()
                },
            )]));

            def.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, cx);
            });
            node::Node::Paragraph(paragraph)
        }
        Node::Definition(def) => node::Node::Definition {
            identifier: def.identifier.clone().into(),
            url: def.url.clone().into(),
            title: def.title.clone().map(|s| s.into()),
        },
        _ => {
            if cfg!(debug_assertions) {
                tracing::warn!("unsupported node: {:#?}", value);
            }
            node::Node::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(source: &str) -> (node::Node, NodeContext) {
        let style = TextViewStyle::default();
        let mut context = NodeContext::default();
        let root = parse(source, &style, &mut context, &style.highlight_theme).unwrap();
        (root, context)
    }

    fn paragraph(source: &str) -> Paragraph {
        let (root, _) = document(source);
        let node::Node::Root { mut children } = root else {
            panic!("expected document")
        };
        let node::Node::Paragraph(paragraph) = children.remove(0) else {
            panic!("expected paragraph")
        };
        paragraph
    }

    fn inline<'a>(paragraph: &'a Paragraph, text: &str) -> &'a InlineNode {
        paragraph
            .children
            .iter()
            .find(|inline| inline.text.as_str() == text)
            .unwrap_or_else(|| panic!("missing inline {text:?}: {paragraph:?}"))
    }

    fn mark(paragraph: &Paragraph, text: &str) -> TextMark {
        let inline = inline(paragraph, text);
        assert_eq!(inline.marks.len(), 1);
        assert_eq!(inline.marks[0].0, 0..text.len());
        inline.marks[0].1.clone()
    }

    #[test]
    fn nested_emphasis_preserves_code_links_images_and_unicode_ranges() {
        let paragraph = paragraph(
            "**bold *世界* ~~gone `code`~~ [*label*](https://example.com \"title\") ![alt](image.png)**",
        );
        assert!(mark(&paragraph, "bold ").bold);
        assert_eq!(
            mark(&paragraph, "世界"),
            TextMark::default().bold().italic()
        );
        assert_eq!(
            mark(&paragraph, "code"),
            TextMark::default().bold().strikethrough().code()
        );
        let link = mark(&paragraph, "label");
        assert!(link.bold && link.italic);
        assert_eq!(
            link.link.as_ref().unwrap().url.as_str(),
            "https://example.com"
        );
        assert_eq!(link.link.unwrap().title.unwrap().as_str(), "title");
        assert!(
            paragraph
                .children
                .iter()
                .any(|inline| inline.image.is_some())
        );
    }

    #[test]
    fn references_preserve_label_marks_and_linked_images() {
        let (root, context) = document(
            "[**bold** and `code`][target] [![alt][picture]][target]\n\n[target]: https://example.com \"Target\"\n[picture]: image.png \"Picture\"\n",
        );
        let node::Node::Root { children } = root else {
            panic!("expected document")
        };
        let node::Node::Paragraph(paragraph) = &children[0] else {
            panic!("expected paragraph")
        };
        for (text, code) in [("bold", false), ("code", true)] {
            let mark = mark(paragraph, text);
            assert_eq!(mark.code, code);
            assert_eq!(mark.bold, !code);
            assert_eq!(mark.link.unwrap().identifier.unwrap().as_str(), "target");
        }
        let image = paragraph
            .children
            .iter()
            .find_map(|inline| inline.image.as_ref())
            .unwrap();
        assert_eq!(image.url, gpui::SharedUri::from("image.png"));
        assert_eq!(image.title.as_ref().unwrap().as_str(), "Picture");
        assert_eq!(
            image
                .link
                .as_ref()
                .unwrap()
                .identifier
                .as_ref()
                .unwrap()
                .as_str(),
            "target"
        );
        assert_eq!(
            context.link_refs["target"].url.as_str(),
            "https://example.com"
        );
    }

    #[test]
    fn hard_breaks_survive_nested_marks_without_turning_soft_breaks_into_hard_breaks() {
        for source in [
            "**first  \nsecond**",
            "**first\\\nsecond**",
            "**first<br>second**",
        ] {
            let paragraph = paragraph(source);
            let text: String = paragraph
                .children
                .iter()
                .map(|inline| inline.text.as_str())
                .collect();
            assert_eq!(text, "first\nsecond");
            assert!(mark(&paragraph, "\n").bold);
        }
        let paragraph = paragraph("first\nsecond");
        let text: String = paragraph
            .children
            .iter()
            .map(|inline| inline.text.as_str())
            .collect();
        assert_eq!(text, "first second");
    }

    #[test]
    fn tables_normalize_ragged_rows_and_preserve_nested_cell_marks() {
        let (root, _) = document(
            "| left | middle | right |\n| :--- | :---: | ---: |\n| **[cell](https://example.com)** | `value` |\n| a | b | c | ignored |\n| | | |\n",
        );
        let node::Node::Root { children } = root else {
            panic!("expected document")
        };
        let node::Node::Table(table) = &children[0] else {
            panic!("expected table")
        };
        assert_eq!(table.children.len(), 4);
        assert!(table.children[0].header);
        assert!(table.children[1..].iter().all(|row| !row.header));
        assert!(table.children.iter().all(|row| row.children.len() == 3));
        assert_eq!(table.column_align(0), node::ColumnumnAlign::Left);
        assert_eq!(table.column_align(1), node::ColumnumnAlign::Center);
        assert_eq!(table.column_align(2), node::ColumnumnAlign::Right);
        let mark = mark(&table.children[1].children[0].children, "cell");
        assert!(mark.bold);
        assert!(mark.link.is_some());
    }

    #[test]
    fn nested_ordered_tasks_and_code_keep_structure_and_literal_content() {
        let (root, _) = document(
            "7. [x] **done `code`**\n\n   3. nested\n8. next\n\n```text\n  **literal**\n\n    indented\n```\n",
        );
        let node::Node::Root { children } = root else {
            panic!("expected document")
        };
        let node::Node::List {
            start,
            children: items,
            ordered,
        } = &children[0]
        else {
            panic!("expected list")
        };
        assert!(*ordered);
        assert_eq!(*start, 7);
        let node::Node::ListItem {
            checked,
            children: item,
            ..
        } = &items[0]
        else {
            panic!("expected task")
        };
        assert_eq!(*checked, Some(true));
        let node::Node::Paragraph(paragraph) = &item[0] else {
            panic!("expected task label")
        };
        assert_eq!(mark(paragraph, "code"), TextMark::default().bold().code());
        assert!(matches!(
            item[1],
            node::Node::List {
                ordered: true,
                start: 3,
                ..
            }
        ));
        let node::Node::CodeBlock(code) = &children[1] else {
            panic!("expected code")
        };
        assert_eq!(code.lang().unwrap().as_str(), "text");
        assert_eq!(code.code().as_str(), "  **literal**\n\n    indented");
    }
}
