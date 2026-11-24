use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, Mutex},
};

use gpui::{
    AnyElement, App, DefiniteLength, Div, Element, ElementId, FontStyle, FontWeight, Half,
    HighlightStyle, Hsla, InteractiveElement as _, IntoElement, Length, ListState, ObjectFit,
    ParentElement, SharedString, SharedUri, StatefulInteractiveElement, Styled, StyledImage as _,
    Window, div, img, prelude::FluentBuilder as _, px, relative, rems, rgb, rgba,
};
use markdown::mdast;
use ropey::Rope;

use crate::{
    ActiveTheme as _, Icon, IconName, StyledExt, h_flex,
    highlighter::{HighlightTheme, SyntaxHighlighter},
    text::{
        CodeBlockActionsFn,
        inline::{Inline, InlineState},
    },
    tooltip::Tooltip,
    v_flex,
};

use super::{TextViewStyle, utils::list_item_prefix};

#[path = "node_flow.rs"]
mod flow_fragments;
pub(crate) use flow_fragments::FlowFragment;

fn is_shell_language(lang: Option<&str>) -> bool {
    let Some(lang) = lang else {
        return false;
    };
    matches!(
        lang.to_ascii_lowercase().as_str(),
        "bash" | "sh" | "shell" | "zsh" | "fish" | "console" | "terminal"
    )
}

#[derive(Clone, Copy)]
struct MarkdownColors {
    text_base: Hsla,
    text_muted: Hsla,
    link: Hsla,
    border_base: Hsla,
    border_muted: Hsla,
    code_bg: Hsla,
    shell_code_bg: Hsla,
    table_frame_bg: Hsla,
    table_body_bg: Hsla,
    table_body_border: Hsla,
}

impl MarkdownColors {
    fn for_theme(is_dark: bool) -> Self {
        if is_dark {
            Self {
                text_base: rgb(0xf2f2f2).into(),
                text_muted: rgb(0xaeaeae).into(),
                link: rgb(0xa2bcff).into(),
                border_base: rgba(0xffffff1a).into(),
                border_muted: rgba(0xffffff14).into(),
                code_bg: rgb(0x151515).into(),
                shell_code_bg: rgb(0x3a3a3a).into(),
                table_frame_bg: rgb(0x2e2e2e).into(),
                table_body_bg: rgb(0x242424).into(),
                table_body_border: rgba(0xffffff29).into(),
            }
        } else {
            Self {
                text_base: rgb(0x161616).into(),
                text_muted: rgb(0x5c5c5c).into(),
                link: rgb(0x3b5cf6).into(),
                border_base: rgba(0x0000001a).into(),
                border_muted: rgba(0x00000014).into(),
                code_bg: rgb(0xfcfcfc).into(),
                shell_code_bg: rgb(0xf2f2f2).into(),
                table_frame_bg: rgb(0xfafafa).into(),
                table_body_bg: rgb(0xffffff).into(),
                table_body_border: rgba(0x0000001f).into(),
            }
        }
    }
}

fn table_column_fractions(lengths: &[usize]) -> Vec<f32> {
    let largest = lengths.iter().copied().max().unwrap_or(5).clamp(5, 150);
    let minimum = (largest / 3).max(5);
    let total: usize = lengths
        .iter()
        .map(|length| (*length).clamp(minimum, 150))
        .sum();
    lengths
        .iter()
        .map(|length| (*length).clamp(minimum, 150) as f32 / total as f32)
        .collect()
}

#[cfg(test)]
mod table_width_tests {
    use super::table_column_fractions;

    #[test]
    fn columns_fit_available_width_even_with_long_cells() {
        let widths = table_column_fractions(&[5, 25, 300]);
        assert!((widths.iter().sum::<f32>() - 1.0).abs() < 0.0001);
        assert!(widths.iter().all(|width| *width > 0.0 && *width < 1.0));
        assert!(widths[0] >= widths[2] / 3.0);
        assert!(table_column_fractions(&[]).is_empty());
    }
}

#[allow(unused)]
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LinkMark {
    pub url: SharedString,

    pub identifier: Option<SharedString>,
    pub title: Option<SharedString>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct TextMark {
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub code: bool,
    pub link: Option<LinkMark>,
}

impl TextMark {
    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub fn strikethrough(mut self) -> Self {
        self.strikethrough = true;
        self
    }

    pub fn code(mut self) -> Self {
        self.code = true;
        self
    }

    pub fn link(mut self, link: impl Into<LinkMark>) -> Self {
        self.link = Some(link.into());
        self
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl From<Span> for ElementId {
    fn from(value: Span) -> Self {
        ElementId::Name(format!("md-{}:{}", value.start, value.end).into())
    }
}

#[allow(unused)]
#[derive(Debug, Default, Clone)]
pub struct ImageNode {
    pub url: SharedUri,
    pub link: Option<LinkMark>,
    pub title: Option<SharedString>,
    pub alt: Option<SharedString>,
    pub width: Option<DefiniteLength>,
    pub height: Option<DefiniteLength>,
}

impl ImageNode {
    pub fn title(&self) -> String {
        self.title
            .clone()
            .unwrap_or_else(|| self.alt.clone().unwrap_or_default())
            .to_string()
    }
}

impl PartialEq for ImageNode {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
            && self.link == other.link
            && self.title == other.title
            && self.alt == other.alt
            && self.width == other.width
            && self.height == other.height
    }
}

#[derive(Default, Clone, Debug)]
pub(crate) struct InlineNode {
    pub(crate) text: SharedString,
    pub(crate) image: Option<ImageNode>,

    pub(crate) marks: Vec<(Range<usize>, TextMark)>,

    state: Arc<Mutex<InlineState>>,
}

impl PartialEq for InlineNode {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.image == other.image && self.marks == other.marks
    }
}

impl InlineNode {
    pub(crate) fn new(text: impl Into<SharedString>) -> Self {
        Self {
            text: text.into(),
            image: None,
            marks: vec![],
            state: Arc::new(Mutex::new(InlineState::default())),
        }
    }

    pub(crate) fn image(image: ImageNode) -> Self {
        let mut this = Self::new("");
        this.image = Some(image);
        this
    }

    pub(crate) fn marks(mut self, marks: Vec<(Range<usize>, TextMark)>) -> Self {
        self.marks = marks;
        self
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Paragraph {
    pub(super) span: Option<Span>,
    pub(super) children: Vec<InlineNode>,

    pub(super) link_refs: HashMap<SharedString, SharedString>,

    pub(crate) state: Arc<Mutex<InlineState>>,
}

impl PartialEq for Paragraph {
    fn eq(&self, other: &Self) -> bool {
        self.span == other.span
            && self.children == other.children
            && self.link_refs == other.link_refs
    }
}

impl Paragraph {
    pub(crate) fn new(text: String) -> Self {
        Self {
            span: None,
            children: vec![InlineNode::new(&text)],
            link_refs: HashMap::new(),
            state: Arc::new(Mutex::new(InlineState::default())),
        }
    }

    pub(super) fn selected_text(&self) -> String {
        let mut text = String::new();

        for c in self.children.iter() {
            let state = c.state.lock().unwrap();
            if let Some(selection) = &state.selection {
                let part_text = state.text.clone();
                text.push_str(&part_text[selection.start..selection.end]);
            }
        }

        let state = self.state.lock().unwrap();
        if let Some(selection) = &state.selection {
            let all_text = state.text.clone();
            text.push_str(&all_text[selection.start..selection.end]);
        }

        text
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Table {
    pub children: Vec<TableRow>,
    pub column_aligns: Vec<ColumnumnAlign>,
    pub column_fractions: Option<Vec<f32>>,
    pub flow_edges: Option<(bool, bool)>,
}

impl Table {
    pub(crate) fn column_align(&self, index: usize) -> ColumnumnAlign {
        self.column_aligns.get(index).copied().unwrap_or_default()
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub(crate) enum ColumnumnAlign {
    #[default]
    Left,
    Center,
    Right,
}

impl From<mdast::AlignKind> for ColumnumnAlign {
    fn from(value: mdast::AlignKind) -> Self {
        match value {
            mdast::AlignKind::None => ColumnumnAlign::Left,
            mdast::AlignKind::Left => ColumnumnAlign::Left,
            mdast::AlignKind::Center => ColumnumnAlign::Center,
            mdast::AlignKind::Right => ColumnumnAlign::Right,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TableRow {
    pub children: Vec<TableCell>,
    pub header: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TableCell {
    pub children: Paragraph,
    pub width: Option<DefiniteLength>,
}

impl Paragraph {
    pub(crate) fn take(&mut self) -> Paragraph {
        std::mem::replace(
            self,
            Paragraph {
                span: None,
                children: vec![],
                link_refs: Default::default(),
                state: Arc::new(Mutex::new(InlineState::default())),
            },
        )
    }

    pub(crate) fn is_image(&self) -> bool {
        false
    }

    pub(crate) fn set_span(&mut self, span: Span) {
        self.span = Some(span);
    }

    pub(crate) fn push_str(&mut self, text: &str) {
        self.children.push(
            InlineNode::new(text.to_string()).marks(vec![(0..text.len(), TextMark::default())]),
        );
    }

    pub(crate) fn push(&mut self, text: InlineNode) {
        self.children.push(text);
    }

    pub(crate) fn push_image(&mut self, image: ImageNode) {
        self.children.push(InlineNode::image(image));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.children.is_empty()
            || self
                .children
                .iter()
                .all(|node| node.text.is_empty() && node.image.is_none())
    }

    pub(crate) fn text_len(&self) -> usize {
        self.children
            .iter()
            .map(|node| node.text.len())
            .sum::<usize>()
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.children.extend(other.children);
    }
}

#[derive(Debug, Clone)]
pub struct CodeBlock {
    lang: Option<SharedString>,
    styles: Vec<(Range<usize>, HighlightStyle)>,
    state: Arc<Mutex<InlineState>>,
    copy_source: Option<SharedString>,
    flow_edges: Option<(bool, bool)>,
}

impl PartialEq for CodeBlock {
    fn eq(&self, other: &Self) -> bool {
        self.lang == other.lang && self.styles == other.styles
    }
}

impl CodeBlock {
    pub fn lang(&self) -> Option<SharedString> {
        self.lang.clone()
    }

    pub fn code(&self) -> SharedString {
        self.copy_source
            .clone()
            .unwrap_or_else(|| self.state.lock().unwrap().text.clone())
    }

    pub(crate) fn new(
        code: SharedString,
        lang: Option<SharedString>,
        _: &TextViewStyle,
        highlight_theme: &HighlightTheme,
    ) -> Self {
        let mut styles = vec![];
        if let Some(lang) = &lang {
            let mut highlighter = SyntaxHighlighter::new(&lang);
            highlighter.update(None, &Rope::from_str(code.as_str()));
            styles = highlighter.styles(&(0..code.len()), highlight_theme);
        };

        let state = Arc::new(Mutex::new(InlineState::default()));
        state.lock().unwrap().set_text(code);

        Self {
            lang,
            styles,
            state,
            copy_source: None,
            flow_edges: None,
        }
    }

    pub(super) fn selected_text(&self) -> String {
        let mut text = String::new();
        let state = self.state.lock().unwrap();
        if let Some(selection) = &state.selection {
            let part_text = state.text.clone();
            text.push_str(&part_text[selection.start..selection.end]);
        }
        text
    }

    fn render(
        &self,
        options: &NodeRenderOptions,
        node_cx: &NodeContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let style = &node_cx.style;
        let colors = MarkdownColors::for_theme(cx.theme().is_dark());
        let is_shell = is_shell_language(self.lang.as_deref());
        let (code_bg, border_color, text_color, code_styles) = if is_shell {
            (
                colors.shell_code_bg,
                colors.border_muted,
                colors.text_base,
                Vec::new(),
            )
        } else {
            (
                colors.code_bg,
                colors.border_base,
                if cx.theme().is_dark() {
                    rgba(0xffffff9e).into()
                } else {
                    rgb(0x6f6f6f).into()
                },
                self.styles.clone(),
            )
        };
        let is_first_fragment = self.flow_edges.is_none_or(|(first, _)| first);
        let is_last_fragment = self.flow_edges.is_none_or(|(_, last)| last);
        let group_name: SharedString = format!("markdown-code-{}", options.ix).into();

        div()
            .w_full()
            .min_w_0()
            .when(
                is_first_fragment
                    && !options.is_first
                    && !options.after_heading
                    && !options.in_list,
                |this| this.pt(px(12.)),
            )
            .when(
                is_last_fragment && !options.is_last && !options.in_list,
                |this| this.pb(px(24.)),
            )
            .child(
                div()
                    .id(("codeblock", options.ix))
                    .group(group_name.clone())
                    .w_full()
                    .min_w_0()
                    .p(px(12.))
                    .rounded(px(6.))
                    .border(px(0.5))
                    .border_color(border_color)
                    .bg(code_bg)
                    .text_color(text_color)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(px(13.))
                    .line_height(px(20.))
                    .relative()
                    .refine_style(&style.code_block)
                    .when(
                        self.flow_edges.is_some_and(|(first, _)| !first),
                        |mut this| {
                            this.style().padding.top = Some(px(0.).into());
                            this.style().border_widths.top = Some(px(0.).into());
                            this.style().corner_radii.top_left = Some(px(0.).into());
                            this.style().corner_radii.top_right = Some(px(0.).into());
                            this
                        },
                    )
                    .when(
                        self.flow_edges.is_some_and(|(_, last)| !last),
                        |mut this| {
                            this.style().padding.bottom = Some(px(0.).into());
                            this.style().border_widths.bottom = Some(px(0.).into());
                            this.style().corner_radii.bottom_left = Some(px(0.).into());
                            this.style().corner_radii.bottom_right = Some(px(0.).into());
                            this
                        },
                    )
                    .child(Inline::new("code", self.state.clone(), vec![], code_styles))
                    .when_some(node_cx.code_block_actions.clone(), |this, actions| {
                        this.child(
                            div()
                                .absolute()
                                .top(px(4.))
                                .right(px(4.))
                                .opacity(0.)
                                .group_hover(group_name, |this| this.opacity(1.))
                                .bg(code_bg)
                                .rounded(px(4.))
                                .child(actions(&self, window, cx)),
                        )
                    }),
            )
            .into_any_element()
    }
}

#[derive(Default, Clone)]
pub(crate) struct NodeContext {
    pub(crate) link_refs: Arc<HashMap<SharedString, LinkMark>>,
    pub(crate) style: TextViewStyle,
    pub(crate) code_block_actions: Option<Arc<CodeBlockActionsFn>>,
}

impl NodeContext {
    pub(super) fn add_ref(&mut self, identifier: SharedString, link: LinkMark) {
        Arc::make_mut(&mut self.link_refs)
            .entry(identifier)
            .or_insert(link);
    }
}

impl PartialEq for NodeContext {
    fn eq(&self, other: &Self) -> bool {
        self.link_refs == other.link_refs && self.style == other.style
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Root {
        children: Vec<Node>,
    },
    Paragraph(Paragraph),
    Heading {
        level: u8,
        children: Paragraph,
    },
    Blockquote {
        children: Vec<Node>,
    },
    List {
        children: Vec<Node>,
        ordered: bool,
        start: usize,
    },
    ListItem {
        children: Vec<Node>,
        spread: bool,

        checked: Option<bool>,
    },
    CodeBlock(CodeBlock),
    Table(Table),
    Break {
        html: bool,
    },
    Divider,

    Definition {
        identifier: SharedString,
        url: SharedString,
        title: Option<SharedString>,
    },
    Unknown,
}

impl Node {
    pub(super) fn is_list_item(&self) -> bool {
        matches!(self, Self::ListItem { .. })
    }

    pub(super) fn is_break(&self) -> bool {
        matches!(self, Self::Break { .. })
    }

    pub(super) fn compact(self) -> Node {
        match self {
            Self::Root { mut children } if children.len() == 1 => children.remove(0).compact(),
            _ => self,
        }
    }

    pub(super) fn selected_text(&self) -> String {
        let mut text = String::new();
        match self {
            Node::Root { children } => {
                let mut block_text = String::new();
                for c in children.iter() {
                    block_text.push_str(&c.selected_text());
                }
                if !block_text.is_empty() {
                    text.push_str(&block_text);
                    text.push('\n');
                }
            }
            Node::Paragraph(paragraph) => {
                let mut block_text = String::new();
                block_text.push_str(&paragraph.selected_text());
                if !block_text.is_empty() {
                    text.push_str(&block_text);
                    text.push('\n');
                }
            }
            Node::Heading { children, .. } => {
                let mut block_text = String::new();
                block_text.push_str(&children.selected_text());
                if !block_text.is_empty() {
                    text.push_str(&block_text);
                    text.push('\n');
                }
            }
            Node::List { children, .. } => {
                for c in children.iter() {
                    text.push_str(&c.selected_text());
                }
            }
            Node::ListItem { children, .. } => {
                for c in children.iter() {
                    text.push_str(&c.selected_text());
                }
            }
            Node::Blockquote { children } => {
                let mut block_text = String::new();
                for c in children.iter() {
                    block_text.push_str(&c.selected_text());
                }

                if !block_text.is_empty() {
                    text.push_str(&block_text);
                    text.push('\n');
                }
            }
            Node::Table(table) => {
                let mut block_text = String::new();
                for row in table.children.iter() {
                    let mut row_texts = vec![];
                    for cell in row.children.iter() {
                        row_texts.push(cell.children.selected_text());
                    }
                    if !row_texts.is_empty() {
                        block_text.push_str(&row_texts.join(" "));
                        block_text.push('\n');
                    }
                }

                if !block_text.is_empty() {
                    text.push_str(&block_text);
                    text.push('\n');
                }
            }
            Node::CodeBlock(code_block) => {
                let block_text = code_block.selected_text();
                if !block_text.is_empty() {
                    text.push_str(&block_text);
                    text.push('\n');
                }
            }
            Node::Definition { .. } | Node::Break { .. } | Node::Divider | Node::Unknown => {}
        }

        text
    }
}

impl Paragraph {
    fn render(
        &self,
        node_cx: &NodeContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let span = self.span;
        let children = &self.children;

        let mut child_nodes: Vec<AnyElement> = vec![];

        let mut text = String::new();
        let mut highlights: Vec<(Range<usize>, HighlightStyle)> = vec![];
        let mut links: Vec<(Range<usize>, LinkMark)> = vec![];
        let mut code_ranges = Vec::new();
        let mut offset = 0;

        let mut ix = 0;
        for inline_node in children {
            let text_len = inline_node.text.len();
            text.push_str(&inline_node.text);

            if let Some(image) = &inline_node.image {
                if text.len() > 0 {
                    inline_node
                        .state
                        .lock()
                        .unwrap()
                        .set_text(std::mem::take(&mut text).into());
                    child_nodes.push(
                        Inline::new(
                            ix,
                            inline_node.state.clone(),
                            std::mem::take(&mut links),
                            gpui::combine_highlights(std::mem::take(&mut highlights), []).collect(),
                        )
                        .code_ranges(std::mem::take(&mut code_ranges))
                        .into_any_element(),
                    );
                }
                child_nodes.push(
                    img(image.url.clone())
                        .id(ix)
                        .object_fit(ObjectFit::Contain)
                        .max_w(relative(1.))
                        .when_some(image.width, |this, width| this.w(width))
                        .when_some(image.link.clone(), |this, link| {
                            let link = link
                                .identifier
                                .as_ref()
                                .and_then(|identifier| node_cx.link_refs.get(identifier))
                                .cloned()
                                .unwrap_or(link);
                            let title = image.title();
                            this.cursor_pointer()
                                .tooltip(move |window, cx| {
                                    Tooltip::new(title.clone()).build(window, cx)
                                })
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.open_url(&link.url);
                                })
                        })
                        .into_any_element(),
                );

                text.clear();
                links.clear();
                highlights.clear();
                code_ranges.clear();
                offset = 0;
            } else {
                let mut node_highlights = vec![];
                for (range, style) in &inline_node.marks {
                    let inner_range = (offset + range.start)..(offset + range.end);

                    let colors = MarkdownColors::for_theme(cx.theme().is_dark());
                    let mut highlight = HighlightStyle::default();
                    if style.bold {
                        highlight.font_weight = Some(FontWeight::BOLD);
                    }
                    if style.italic {
                        highlight.font_style = Some(FontStyle::Italic);
                    }
                    if style.strikethrough {
                        highlight.strikethrough = Some(gpui::StrikethroughStyle {
                            thickness: gpui::px(1.),
                            ..Default::default()
                        });
                    }
                    if style.code {
                        if style.link.is_none() {
                            code_ranges.push(inner_range.clone());
                            highlight.color = Some(colors.text_base);
                        }
                        if !style.bold {
                            highlight.font_weight = Some(FontWeight::MEDIUM);
                        }
                    }

                    if let Some(mut link_mark) = style.link.clone() {
                        highlight.color = Some(colors.link);
                        highlight.underline = None;

                        if let Some(identifier) = link_mark.identifier.as_ref() {
                            if let Some(mark) = node_cx.link_refs.get(identifier) {
                                link_mark = mark.clone();
                            }
                        }

                        links.push((inner_range.clone(), link_mark));
                    }

                    node_highlights.push((inner_range, highlight));
                }

                highlights.extend(node_highlights);
                offset += text_len;
            }
            ix += 1;
        }

        if text.len() > 0 {
            self.state.lock().unwrap().set_text(text.into());
            child_nodes.push(
                Inline::new(
                    ix,
                    self.state.clone(),
                    links,
                    gpui::combine_highlights(highlights, []).collect(),
                )
                .code_ranges(code_ranges)
                .into_any_element(),
            );
        }

        div().id(span.unwrap_or_default()).children(child_nodes)
    }
}

#[derive(Default, Clone, Copy)]
struct NodeRenderOptions {
    /// Index of this block among its siblings; used to keep element IDs unique.
    ix: usize,
    in_list: bool,
    todo: bool,
    ordered: bool,
    depth: usize,
    is_first: bool,
    is_last: bool,
    after_heading: bool,
}

impl NodeRenderOptions {
    fn is_last(mut self, is_last: bool) -> Self {
        self.is_last = is_last;
        self
    }
}

impl Paragraph {
    fn to_markdown(&self) -> String {
        let mut text = self
            .children
            .iter()
            .map(|text_node| {
                let mut text = text_node.text.to_string();
                for (range, style) in &text_node.marks {
                    if style.bold {
                        text = format!("**{}**", &text_node.text[range.clone()]);
                    }
                    if style.italic {
                        text = format!("*{}*", &text_node.text[range.clone()]);
                    }
                    if style.strikethrough {
                        text = format!("~~{}~~", &text_node.text[range.clone()]);
                    }
                    if style.code {
                        text = format!("`{}`", &text_node.text[range.clone()]);
                    }
                    if let Some(link) = &style.link {
                        text = format!("[{}]({})", &text_node.text[range.clone()], link.url);
                    }
                }

                if let Some(image) = &text_node.image {
                    let alt = image.alt.clone().unwrap_or_default();
                    let title = image
                        .title
                        .clone()
                        .map_or(String::new(), |t| format!(" \"{}\"", t));
                    text.push_str(&format!("![{}]({}{})", alt, image.url, title))
                }

                text
            })
            .collect::<Vec<_>>()
            .join("");

        text.push_str("\n\n");
        text
    }
}

impl Node {
    #[allow(dead_code)]
    pub(crate) fn to_markdown(&self) -> String {
        match self {
            Node::Root { children } => children
                .iter()
                .map(|child| child.to_markdown())
                .collect::<Vec<_>>()
                .join("\n\n"),
            Node::Paragraph(paragraph) => paragraph.to_markdown(),
            Node::Heading { level, children } => {
                let hashes = "#".repeat(*level as usize);
                format!("{} {}", hashes, children.to_markdown())
            }
            Node::Blockquote { children } => {
                let content = children
                    .iter()
                    .map(|child| child.to_markdown())
                    .collect::<Vec<_>>()
                    .join("\n\n");

                content
                    .lines()
                    .map(|line| format!("> {}", line))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Node::List {
                children,
                ordered,
                start,
            } => children
                .iter()
                .enumerate()
                .map(|(i, child)| {
                    let prefix = if *ordered {
                        format!("{}. ", i + start)
                    } else {
                        "- ".to_string()
                    };
                    format!("{}{}", prefix, child.to_markdown())
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Node::ListItem {
                children, checked, ..
            } => {
                let checkbox = if let Some(checked) = checked {
                    if *checked { "[x] " } else { "[ ] " }
                } else {
                    ""
                };
                format!(
                    "{}{}",
                    checkbox,
                    children
                        .iter()
                        .map(|child| child.to_markdown())
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
            Node::CodeBlock(code_block) => {
                format!(
                    "```{}\n{}\n```",
                    code_block.lang.clone().unwrap_or_default(),
                    code_block.code()
                )
            }
            Node::Table(table) => {
                let header = table
                    .children
                    .first()
                    .map(|row| {
                        row.children
                            .iter()
                            .map(|cell| cell.children.to_markdown())
                            .collect::<Vec<_>>()
                            .join(" | ")
                    })
                    .unwrap_or_default();
                let alignments = table
                    .column_aligns
                    .iter()
                    .map(|align| {
                        match align {
                            ColumnumnAlign::Left => ":--",
                            ColumnumnAlign::Center => ":-:",
                            ColumnumnAlign::Right => "--:",
                        }
                        .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(" | ");
                let rows = table
                    .children
                    .iter()
                    .skip(1)
                    .map(|row| {
                        row.children
                            .iter()
                            .map(|cell| cell.children.to_markdown())
                            .collect::<Vec<_>>()
                            .join(" | ")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{}\n{}\n{}", header, alignments, rows)
            }
            Node::Break { html } => {
                if *html {
                    "<br>".to_string()
                } else {
                    "\n".to_string()
                }
            }
            Node::Divider => "---".to_string(),
            Node::Definition {
                identifier,
                url,
                title,
            } => {
                if let Some(title) = title {
                    format!("[{}]: {} \"{}\"", identifier, url, title)
                } else {
                    format!("[{}]: {}", identifier, url)
                }
            }
            Node::Unknown => "".to_string(),
        }
        .trim()
        .to_string()
    }
}

impl Node {
    fn render_list_item_row(
        content: AnyElement,
        ix: usize,
        options: NodeRenderOptions,
        checked: Option<bool>,
        cx: &mut App,
    ) -> Div {
        let colors = MarkdownColors::for_theme(cx.theme().is_dark());
        h_flex()
            .w_full()
            .flex_1()
            .min_w_0()
            .relative()
            .items_start()
            .content_start()
            .when(!options.todo && checked.is_none(), |this| {
                this.child(
                    div()
                        .min_w(px(16.))
                        .text_color(colors.text_muted)
                        .child(list_item_prefix(ix, options.ordered, options.depth)),
                )
            })
            .when_some(checked, |this, checked| {
                this.child(
                    div()
                        .flex()
                        .mt(rems(0.4))
                        .mr_1p5()
                        .size(rems(0.875))
                        .items_center()
                        .justify_center()
                        .rounded(cx.theme().radius.half())
                        .border_1()
                        .border_color(cx.theme().primary)
                        .text_color(cx.theme().primary_foreground)
                        .when(checked, |this| {
                            this.bg(cx.theme().primary)
                                .child(Icon::new(IconName::Check).size_2().text_xs())
                        }),
                )
            })
            .child(div().flex_1().min_w_0().overflow_hidden().child(content))
    }

    fn render_list_item(
        item: &Node,
        ix: usize,
        options: NodeRenderOptions,
        node_cx: &NodeContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        match item {
            Node::ListItem {
                children,
                spread,
                checked,
            } => v_flex()
                .id(("li", options.ix))
                .w_full()
                .min_w_0()
                .when(*spread, |this| this.child(div()))
                .children({
                    let mut items: Vec<Div> = Vec::with_capacity(children.len());

                    for (child_ix, child) in children.iter().enumerate() {
                        match child {
                            Node::Paragraph(_) => {
                                let text = child.render_block(
                                    NodeRenderOptions {
                                        ix: child_ix,
                                        depth: options.depth + 1,
                                        todo: checked.is_some(),
                                        is_last: true,
                                        ..options
                                    },
                                    node_cx,
                                    window,
                                    cx,
                                );

                                // Continuation paragraph: stack below the previous row,
                                // indented to align with the text column past the marker.
                                if child_ix > 0 {
                                    if let Some(preceding_row) = items.pop() {
                                        items.push(
                                            v_flex().child(preceding_row).child(
                                                div()
                                                    .w_full()
                                                    .pt(px(8.))
                                                    .pl(rems(1.))
                                                    .overflow_hidden()
                                                    .child(text),
                                            ),
                                        );
                                        continue;
                                    }
                                }

                                items.push(Self::render_list_item_row(
                                    text, ix, options, *checked, cx,
                                ));
                            }
                            Node::List {
                                ordered: nested_ordered,
                                ..
                            } => {
                                let indent = if *nested_ordered { px(28.) } else { px(16.) };
                                items.push(div().my(px(4.)).pl(indent).child(child.render_block(
                                    NodeRenderOptions {
                                        ix: child_ix,
                                        depth: options.depth + 1,
                                        todo: checked.is_some(),
                                        is_last: true,
                                        ..options
                                    },
                                    node_cx,
                                    window,
                                    cx,
                                )));
                            }
                            Node::Root { .. }
                            | Node::Heading { .. }
                            | Node::Blockquote { .. }
                            | Node::CodeBlock(_)
                            | Node::Table(_)
                            | Node::Divider => {
                                let block = child.render_block(
                                    NodeRenderOptions {
                                        ix: child_ix,
                                        depth: options.depth + 1,
                                        todo: checked.is_some(),
                                        is_last: true,
                                        ..options
                                    },
                                    node_cx,
                                    window,
                                    cx,
                                );

                                if child_ix == 0 {
                                    items.push(Self::render_list_item_row(
                                        block, ix, options, *checked, cx,
                                    ));
                                } else {
                                    items.push(
                                        div()
                                            .w_full()
                                            .min_w_0()
                                            .pl(rems(1.))
                                            .overflow_hidden()
                                            .child(block),
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                    items
                })
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn render_table(
        item: &Node,
        options: &NodeRenderOptions,
        node_cx: &NodeContext,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        const DEFAULT_LENGTH: usize = 5;
        let col_lens = match item {
            Node::Table(table) => {
                let mut col_lens = vec![];
                for row in table.children.iter() {
                    for (ix, cell) in row.children.iter().enumerate() {
                        if col_lens.len() <= ix {
                            col_lens.push(DEFAULT_LENGTH);
                        }

                        let len = cell.children.text_len();
                        if len > col_lens[ix] {
                            col_lens[ix] = len;
                        }
                    }
                }
                col_lens
            }
            _ => vec![],
        };

        let column_fractions = match item {
            Node::Table(table) => table
                .column_fractions
                .clone()
                .unwrap_or_else(|| table_column_fractions(&col_lens)),
            _ => table_column_fractions(&col_lens),
        };

        let colors = MarkdownColors::for_theme(cx.theme().is_dark());

        match item {
            Node::Table(table) => {
                let is_first_fragment = table.flow_edges.is_none_or(|(first, _)| first);
                let is_last_fragment = table.flow_edges.is_none_or(|(_, last)| last);
                let render_row_cells = |row_ix: usize,
                                        row: &TableRow,
                                        is_body_last: bool,
                                        is_header: bool,
                                        window: &mut Window,
                                        cx: &mut App| {
                    div()
                        .id(("row", row_ix))
                        .w_full()
                        .min_w_0()
                        .when(!is_header && !is_body_last, |this| {
                            this.border_b(px(0.5)).border_color(colors.border_muted)
                        })
                        .flex()
                        .flex_row()
                        .text_size(px(13.))
                        .line_height(px(16.))
                        .when(is_header, |this| {
                            this.text_color(colors.text_muted)
                                .font_weight(FontWeight::MEDIUM)
                        })
                        .when(!is_header, |this| {
                            this.text_color(colors.text_base)
                                .font_weight(FontWeight::NORMAL)
                        })
                        .children({
                            let mut cells = Vec::with_capacity(row.children.len());
                            for (ix, cell) in row.children.iter().enumerate() {
                                let align = table.column_align(ix);
                                let width = column_fractions.get(ix).copied().unwrap_or(0.0);

                                cells.push(
                                    div()
                                        .id(("cell", row_ix * 64 + ix))
                                        .flex()
                                        .min_w_0()
                                        .whitespace_normal()
                                        .when(align == ColumnumnAlign::Center, |this| {
                                            this.justify_center().text_center()
                                        })
                                        .when(align == ColumnumnAlign::Right, |this| {
                                            this.justify_end().text_right()
                                        })
                                        .w(Length::Definite(relative(width)))
                                        .px(px(12.))
                                        .py(if is_header { px(8.) } else { px(10.) })
                                        .child(cell.children.render(node_cx, window, cx)),
                                )
                            }
                            cells
                        })
                };

                let header_rows: Vec<(usize, &TableRow)> = table
                    .children
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| row.header)
                    .collect();
                let body_rows: Vec<(usize, &TableRow)> = table
                    .children
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| !row.header)
                    .collect();

                div()
                    .when(
                        is_first_fragment
                            && !options.is_first
                            && !options.after_heading
                            && !options.in_list,
                        |this| this.pt(px(16.)),
                    )
                    .when(
                        is_last_fragment && !options.is_last && !options.in_list,
                        |this| this.pb(px(16.)),
                    )
                    .w_full()
                    .child(
                        div()
                            .id("table")
                            .w_full()
                            .min_w_0()
                            .p(px(2.))
                            .bg(colors.table_frame_bg)
                            .border(px(0.5))
                            .border_color(colors.border_muted)
                            .rounded(px(6.))
                            .when(
                                table.flow_edges.is_some_and(|(first, _)| !first),
                                |mut this| {
                                    this.style().border_widths.top = Some(px(0.).into());
                                    this.style().corner_radii.top_left = Some(px(0.).into());
                                    this.style().corner_radii.top_right = Some(px(0.).into());
                                    this
                                },
                            )
                            .when(
                                table.flow_edges.is_some_and(|(_, last)| !last),
                                |mut this| {
                                    this.style().border_widths.bottom = Some(px(0.).into());
                                    this.style().corner_radii.bottom_left = Some(px(0.).into());
                                    this.style().corner_radii.bottom_right = Some(px(0.).into());
                                    this
                                },
                            )
                            .overflow_hidden()
                            .children(header_rows.into_iter().map(|(row_ix, row)| {
                                render_row_cells(row_ix, row, false, true, window, cx)
                            }))
                            .when(!body_rows.is_empty(), |this| {
                                let body_len = body_rows.len();
                                this.child(
                                    div()
                                        .w_full()
                                        .min_w_0()
                                        .rounded(px(4.))
                                        .bg(colors.table_body_bg)
                                        .border(px(0.5))
                                        .border_color(colors.table_body_border)
                                        .shadow_xs()
                                        .overflow_hidden()
                                        .children(body_rows.into_iter().enumerate().map(
                                            |(body_ix, (row_ix, row))| {
                                                render_row_cells(
                                                    row_ix,
                                                    row,
                                                    body_ix + 1 == body_len,
                                                    false,
                                                    window,
                                                    cx,
                                                )
                                            },
                                        )),
                                )
                            }),
                    )
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }

    pub(super) fn render_root(
        &self,
        list_state: Option<ListState>,
        node_cx: &NodeContext,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let options = NodeRenderOptions {
            is_first: true,
            is_last: true,
            ..Default::default()
        };

        let Some(list_state) = list_state else {
            return self
                .render_block(options, node_cx, window, cx)
                .into_any_element();
        };

        let children = match self {
            Node::Root { children } => children,
            _ => return div().into_any_element(),
        };

        let children = children.clone();
        let node_cx = node_cx.clone();

        if list_state.item_count() != children.len() {
            list_state.reset(children.len());
        }

        gpui::list(list_state, move |ix, window, cx| {
            let is_last = ix + 1 == children.len();
            let after_heading = ix > 0 && matches!(children[ix - 1], Node::Heading { .. });
            children[ix]
                .render_block(
                    NodeRenderOptions {
                        ix,
                        is_first: ix == 0,
                        is_last,
                        after_heading,
                        ..options
                    },
                    &node_cx,
                    window,
                    cx,
                )
                .into_any_element()
        })
        .size_full()
        .into_any()
    }

    fn render_block(
        &self,
        options: NodeRenderOptions,
        node_cx: &NodeContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let colors = MarkdownColors::for_theme(cx.theme().is_dark());
        let mb = if options.in_list || options.is_last {
            px(0.)
        } else {
            px(12.)
        };

        match self {
            Node::Root { children } => div()
                .id("div")
                .children(children.iter().enumerate().map(move |(ix, node)| {
                    let after_heading = ix > 0 && matches!(children[ix - 1], Node::Heading { .. });
                    node.render_block(
                        NodeRenderOptions {
                            ix,
                            is_first: ix == 0,
                            is_last: ix + 1 == children.len(),
                            after_heading,
                            ..options
                        },
                        node_cx,
                        window,
                        cx,
                    )
                }))
                .into_any_element(),
            Node::Paragraph(paragraph) => div()
                .id(("p", options.ix))
                .w_full()
                .min_w_0()
                .line_height(relative(1.6))
                .pb(mb)
                .child(paragraph.render(node_cx, window, cx))
                .into_any_element(),
            Node::Heading { level, children } => {
                let (default_size, font_weight, top_margin, bottom_margin, is_muted) = match level {
                    1 => (px(17.), FontWeight::SEMIBOLD, px(28.), px(12.), false),
                    2 => (px(15.), FontWeight::SEMIBOLD, px(24.), px(10.), false),
                    3 => (px(14.), FontWeight::BOLD, px(20.), px(8.), false),
                    4 | 5 | 6 => (px(14.), FontWeight::MEDIUM, px(20.), px(8.), true),
                    _ => (px(14.), FontWeight::NORMAL, px(20.), px(8.), false),
                };

                let text_size = if let Some(f) = node_cx.style.heading_font_size.as_ref() {
                    (f)(*level, node_cx.style.heading_base_font_size)
                } else {
                    default_size
                };

                div()
                    .id(SharedString::from(format!("h{}-{}", level, options.ix)))
                    .w_full()
                    .min_w_0()
                    .when(!options.is_first && !options.in_list, |this| {
                        this.pt(top_margin)
                    })
                    .when(!options.is_last, |this| this.pb(bottom_margin))
                    .whitespace_normal()
                    .text_size(text_size)
                    .line_height(relative(1.5))
                    .font_weight(font_weight)
                    .text_color(if is_muted {
                        colors.text_muted
                    } else {
                        colors.text_base
                    })
                    .child(children.render(node_cx, window, cx))
                    .into_any_element()
            }
            Node::Blockquote { children } => div()
                .w_full()
                .when(
                    !options.is_first && !options.after_heading && !options.in_list,
                    |this| this.pt(px(16.)),
                )
                .when(!options.is_last && !options.in_list, |this| {
                    this.pb(px(16.))
                })
                .child(
                    div()
                        .id(("blockquote", options.ix))
                        .w_full()
                        .text_color(colors.text_muted)
                        .border_l(px(1.))
                        .border_color(colors.border_base)
                        .pl(px(8.))
                        .children({
                            let children_len = children.len();
                            children.into_iter().enumerate().map(move |(index, c)| {
                                let is_last = index == children_len - 1;
                                let after_heading = index > 0
                                    && matches!(children[index - 1], Node::Heading { .. });
                                c.render_block(
                                    NodeRenderOptions {
                                        ix: index,
                                        is_first: index == 0,
                                        after_heading,
                                        ..options.is_last(is_last)
                                    },
                                    node_cx,
                                    window,
                                    cx,
                                )
                            })
                        }),
                )
                .into_any_element(),
            Node::List {
                children,
                ordered,
                start,
            } => v_flex()
                .id((if *ordered { "ol" } else { "ul" }, options.ix))
                .w_full()
                .min_w_0()
                .gap(px(8.))
                .when(
                    options.depth == 0
                        && !options.is_first
                        && !options.after_heading
                        && !options.in_list,
                    |this| this.pt(px(8.)),
                )
                .when(
                    options.depth == 0 && !options.is_last && !options.in_list,
                    |this| this.pb(px(12.)),
                )
                .when(options.depth == 0, |this| this.pl(px(16.)))
                .children({
                    let mut items = Vec::with_capacity(children.len());
                    let mut item_index = start.saturating_sub(1);
                    for (child_ix, item) in children.into_iter().enumerate() {
                        let is_item = item.is_list_item();

                        items.push(Self::render_list_item(
                            item,
                            item_index,
                            NodeRenderOptions {
                                ix: child_ix,
                                ordered: *ordered,
                                ..options
                            },
                            node_cx,
                            window,
                            cx,
                        ));

                        if is_item {
                            item_index += 1;
                        }
                    }
                    items
                })
                .into_any_element(),
            Node::CodeBlock(code_block) => code_block.render(&options, node_cx, window, cx),
            Node::Table { .. } => {
                Self::render_table(self, &options, node_cx, window, cx).into_any_element()
            }
            Node::Divider => div()
                .id(("divider", options.ix))
                .w_full()
                .h(px(32.))
                .into_any_element(),
            Node::Break { .. } => div().id(("break", options.ix)).into_any_element(),
            Node::Unknown | Node::Definition { .. } => div().into_any_element(),
            _ => {
                if cfg!(debug_assertions) {
                    tracing::warn!("unknown implementation: {:?}", self);
                }

                div().into_any_element()
            }
        }
    }
}
