use super::*;

/// Layout-only subdivisions: all syntax and inline marks come from the parser.
pub(crate) struct FlowFragment {
    node: Node,
    wrappers: Vec<FlowWrapper>,
    continuation: bool,
}

#[derive(Clone)]
enum FlowWrapper {
    Quote,
    Item {
        index: usize,
        ordered: bool,
        checked: Option<bool>,
        marker: bool,
    },
}

impl FlowFragment {
    pub(crate) fn selected_text(&self) -> String {
        self.node.selected_text()
    }

    pub(crate) fn render(
        &self,
        ix: usize,
        context: &NodeContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let mut element = self.node.render_block(
            NodeRenderOptions {
                ix,
                is_last: self.continuation,
                ..Default::default()
            },
            context,
            window,
            cx,
        );
        for (wrapper_index, wrapper) in self.wrappers.iter().enumerate().rev() {
            element = match wrapper {
                FlowWrapper::Quote => div()
                    .w_full()
                    .border_l_3()
                    .border_color(cx.theme().secondary_active)
                    .text_color(cx.theme().muted_foreground)
                    .px_4()
                    .child(element)
                    .into_any_element(),
                FlowWrapper::Item {
                    index,
                    ordered,
                    checked,
                    marker: true,
                } => Node::render_list_item_row(
                    element,
                    *index,
                    NodeRenderOptions {
                        ordered: *ordered,
                        depth: self.wrappers[..wrapper_index]
                            .iter()
                            .filter(|wrapper| matches!(wrapper, FlowWrapper::Item { .. }))
                            .count(),
                        ..Default::default()
                    },
                    *checked,
                    cx,
                )
                .into_any_element(),
                FlowWrapper::Item { marker: false, .. } => div()
                    .w_full()
                    .min_w_0()
                    .pl(rems(1.))
                    .child(element)
                    .into_any_element(),
            };
        }
        div()
            .w_full()
            .min_w_0()
            .debug_selector(move || format!("markdown-flow-block-{ix}"))
            .child(element)
            .into_any_element()
    }
}

fn text_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    if text.is_empty() {
        return vec![0..0];
    }
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + 1024).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            if let Some(at) = text[start..end].rfind('\n') {
                end = start + at + 1;
            } else if let Some(at) = text[start..end].rfind(char::is_whitespace) {
                end = start + at + text[start + at..].chars().next().unwrap().len_utf8();
            }
        }
        ranges.push(start..end);
        start = end;
    }
    ranges
}

fn split_marks<T: Clone>(
    ranges: &[Range<usize>],
    marks: &[(Range<usize>, T)],
) -> Vec<Vec<(Range<usize>, T)>> {
    let mut result = vec![Vec::new(); ranges.len()];
    for (mark, style) in marks {
        let first = ranges.partition_point(|range| range.end <= mark.start);
        for index in first..ranges.len() {
            let range = &ranges[index];
            if range.start >= mark.end {
                break;
            }
            let start = mark.start.max(range.start);
            let end = mark.end.min(range.end);
            if start < end {
                result[index].push(((start - range.start)..(end - range.start), style.clone()));
            }
        }
    }
    result
}

impl Paragraph {
    fn flow_parts(self) -> Vec<Paragraph> {
        let mut parts = Vec::new();
        let mut current = Paragraph::default();
        let mut bytes = 0;
        for inline in self.children {
            if inline.image.is_some() {
                current.children.push(inline);
                continue;
            }
            let ranges = text_ranges(&inline.text);
            let marks = split_marks(&ranges, &inline.marks);
            for (range, marks) in ranges.into_iter().zip(marks) {
                if bytes + range.len() > 1024 && !current.children.is_empty() {
                    parts.push(current);
                    current = Paragraph::default();
                    bytes = 0;
                }
                current
                    .children
                    .push(InlineNode::new(&inline.text[range.clone()]).marks(marks));
                bytes += range.len();
            }
        }
        if !current.children.is_empty() {
            parts.push(current);
        }
        parts
    }
}

impl Node {
    pub(crate) fn flow_fragments(self) -> Vec<FlowFragment> {
        let nodes: Vec<Node> = match self {
            Node::Root { children } => {
                return children
                    .into_iter()
                    .flat_map(Node::flow_fragments)
                    .collect();
            }
            Node::Blockquote { children } => {
                let mut parts: Vec<_> = children
                    .into_iter()
                    .flat_map(Node::flow_fragments)
                    .collect();
                for part in &mut parts {
                    part.wrappers.insert(0, FlowWrapper::Quote);
                }
                return parts;
            }
            Node::List {
                children,
                ordered,
                start,
            } => {
                let mut result = Vec::new();
                for (index, item) in children.into_iter().enumerate() {
                    if let Node::ListItem {
                        children, checked, ..
                    } = item
                    {
                        let parts = children.into_iter().flat_map(Node::flow_fragments);
                        for (part_index, mut part) in parts.enumerate() {
                            part.wrappers.insert(
                                0,
                                FlowWrapper::Item {
                                    index: start.saturating_sub(1) + index,
                                    ordered,
                                    checked,
                                    marker: part_index == 0,
                                },
                            );
                            result.push(part);
                        }
                    }
                }
                return result;
            }
            Node::Paragraph(paragraph) => paragraph
                .flow_parts()
                .into_iter()
                .map(Node::Paragraph)
                .collect(),
            Node::Heading { level, children } => children
                .flow_parts()
                .into_iter()
                .map(|children| Node::Heading { level, children })
                .collect(),
            Node::CodeBlock(code) => {
                let full = code.code();
                let ranges = text_ranges(&full);
                let count = ranges.len();
                let styles = split_marks(&ranges, &code.styles);
                ranges
                    .into_iter()
                    .zip(styles)
                    .enumerate()
                    .map(|(index, (range, styles))| {
                        let state = Arc::new(Mutex::new(InlineState::default()));
                        state
                            .lock()
                            .unwrap()
                            .set_text(SharedString::from(&full[range]));
                        Node::CodeBlock(CodeBlock {
                            lang: code.lang.clone(),
                            styles,
                            state,
                            copy_source: Some(full.clone()),
                            flow_edges: Some((index == 0, index + 1 == count)),
                        })
                    })
                    .collect()
            }
            Node::Table(table) => {
                let mut lengths = Vec::new();
                for row in &table.children {
                    for (index, cell) in row.children.iter().enumerate() {
                        if index == lengths.len() {
                            lengths.push(5);
                        }
                        lengths[index] = lengths[index].max(cell.children.text_len());
                    }
                }
                let fractions = table_column_fractions(&lengths);
                let mut rows = Vec::new();
                for row in table.children {
                    let mut cells: Vec<_> = row
                        .children
                        .into_iter()
                        .map(|cell| {
                            let mut parts = cell.children.flow_parts();
                            if parts.is_empty() {
                                parts.push(Paragraph::default());
                            }
                            (cell.width, parts.into_iter())
                        })
                        .collect();
                    loop {
                        let mut any = false;
                        let children = cells
                            .iter_mut()
                            .map(|(width, parts)| {
                                let part = parts.next();
                                any |= part.is_some();
                                TableCell {
                                    width: *width,
                                    children: part.unwrap_or_default(),
                                }
                            })
                            .collect();
                        if !any {
                            break;
                        }
                        rows.push(TableRow {
                            children,
                            header: row.header,
                        });
                    }
                }
                let count = rows.len();
                rows.into_iter()
                    .enumerate()
                    .map(|(index, row)| {
                        Node::Table(Table {
                            children: vec![row],
                            column_aligns: table.column_aligns.clone(),
                            column_fractions: Some(fractions.clone()),
                            flow_edges: Some((index == 0, index + 1 == count)),
                        })
                    })
                    .collect()
            }
            node => vec![node],
        };
        let count = nodes.len();
        nodes
            .into_iter()
            .enumerate()
            .map(|(index, node)| FlowFragment {
                node,
                wrappers: Vec::new(),
                continuation: index + 1 < count,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_inline_subdivision_preserves_unicode_and_marks() {
        let text = "世界🦀 bold ".repeat(600);
        let paragraph = Paragraph {
            children: vec![
                InlineNode::new(text.clone())
                    .marks(vec![(0..text.len(), TextMark::default().bold())]),
            ],
            ..Default::default()
        };
        let parts = paragraph.flow_parts();
        assert!(parts.len() > 1);
        let mut reconstructed = String::new();
        for part in parts {
            assert!(part.text_len() <= 1024);
            for inline in part.children {
                assert_eq!(inline.marks[0].0, 0..inline.text.len());
                assert!(inline.marks[0].1.bold);
                reconstructed.push_str(&inline.text);
            }
        }
        assert_eq!(reconstructed, text);
    }

    #[test]
    fn oversized_code_fragments_keep_full_copy_and_highlights() {
        let source: SharedString = "let value = \"世界🦀\";\n".repeat(2_000).into();
        let code = CodeBlock::new(
            source.clone(),
            Some("rust".into()),
            &TextViewStyle::default(),
            &HighlightTheme::default_light(),
        );
        let fragments = Node::CodeBlock(code).flow_fragments();
        assert!(fragments.len() > 20);
        let mut shown = String::new();
        for fragment in fragments {
            let Node::CodeBlock(code) = fragment.node else {
                panic!("code fragment")
            };
            assert_eq!(code.code(), source);
            let text = code.state.lock().unwrap().text.clone();
            assert!(text.len() <= 1024);
            assert!(code.styles.iter().all(|(range, _)| range.end <= text.len()));
            shown.push_str(&text);
        }
        assert_eq!(shown, source.as_str());
    }

    #[test]
    fn final_parser_fragments_preserve_setext_nested_lists_tables_and_references() {
        let source = "Title\n=====\n\n7. first\n   - **nested**\n\n     continuation\n8. second\n\n| a | b |\n| - | - |\n| long | x |\n\n[visible][target] and note[^n]\n\n[target]: https://example.com\n\n[^n]: final **note**\n";
        let mut context = NodeContext::default();
        let root = crate::text::format::markdown::parse(
            source,
            &TextViewStyle::default(),
            &mut context,
            &HighlightTheme::default_light(),
        )
        .unwrap();
        assert_eq!(
            context.link_refs.get("target").unwrap().url.as_str(),
            "https://example.com"
        );
        let fragments = root.flow_fragments();
        assert!(
            fragments
                .iter()
                .any(|fragment| matches!(fragment.node, Node::Heading { level: 1, .. }))
        );
        assert!(fragments.iter().any(|fragment| {
            fragment
                .wrappers
                .iter()
                .filter(|wrapper| matches!(wrapper, FlowWrapper::Item { .. }))
                .count()
                == 2
        }));
        assert!(
            fragments
                .iter()
                .any(|fragment| fragment.wrappers.iter().any(|wrapper| matches!(
                    wrapper,
                    FlowWrapper::Item {
                        index: 6,
                        ordered: true,
                        marker: true,
                        ..
                    }
                )))
        );
        assert!(
            fragments
                .iter()
                .any(|fragment| fragment.wrappers.iter().any(|wrapper| matches!(
                    wrapper,
                    FlowWrapper::Item {
                        index: 7,
                        ordered: true,
                        marker: true,
                        ..
                    }
                )))
        );
        let tables: Vec<_> = fragments
            .iter()
            .filter_map(|fragment| match &fragment.node {
                Node::Table(table) => Some(table),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0].column_fractions, tables[1].column_fractions);
    }

    #[test]
    fn oversized_table_cells_are_bounded_without_dropping_marked_text() {
        let text = "long 世界🦀 cell ".repeat(1_000);
        let table = Table {
            children: vec![TableRow {
                children: vec![TableCell {
                    children: Paragraph {
                        children: vec![
                            InlineNode::new(text.clone())
                                .marks(vec![(0..text.len(), TextMark::default().italic())]),
                        ],
                        ..Default::default()
                    },
                    width: None,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let fragments = Node::Table(table).flow_fragments();
        assert!(fragments.len() > 10);
        let mut actual = String::new();
        for fragment in fragments {
            let Node::Table(table) = fragment.node else {
                panic!("table fragment")
            };
            let paragraph = &table.children[0].children[0].children;
            assert!(paragraph.text_len() <= 1024);
            for inline in &paragraph.children {
                assert!(inline.marks[0].1.italic);
                actual.push_str(&inline.text);
            }
        }
        assert_eq!(actual, text);
    }

    #[test]
    fn table_fragments_keep_empty_rows_and_header_continuations() {
        let source = format!(
            "| {} | short |\n| --- | --- |\n| | |\n",
            "header ".repeat(400)
        );
        let mut context = NodeContext::default();
        let root = crate::text::format::markdown::parse(
            &source,
            &TextViewStyle::default(),
            &mut context,
            &HighlightTheme::default_light(),
        )
        .unwrap();
        let tables: Vec<_> = root
            .flow_fragments()
            .into_iter()
            .map(|fragment| {
                let Node::Table(table) = fragment.node else {
                    panic!("expected table fragment")
                };
                assert!(
                    table.children[0]
                        .children
                        .iter()
                        .all(|cell| cell.children.text_len() <= 1024)
                );
                table
            })
            .collect();
        assert!(tables.len() > 2);
        assert!(
            tables[..tables.len() - 1]
                .iter()
                .all(|table| table.children[0].header)
        );
        let empty_row = &tables.last().unwrap().children[0];
        assert!(!empty_row.header);
        assert_eq!(empty_row.children.len(), 2);
        assert!(
            empty_row
                .children
                .iter()
                .all(|cell| cell.children.is_empty())
        );
    }
}
