use std::sync::{Arc, Mutex};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId,
    InspectorElementId, LayoutId, Pixels, Size, Style, Window, point, px, size,
};

use super::{
    node::{FlowFragment, NodeContext},
    text_view::ParsedContent,
};

/// Append-only source shared by the transcript and its bounded parser worker.
/// Existing chunks are never concatenated or cloned when a paragraph settles.
#[derive(Clone, Default)]
pub struct MarkdownBlocks(Arc<Mutex<Source>>);

#[derive(Default)]
struct Source {
    chunks: Vec<gpui::SharedString>,
    bytes: usize,
    complete: Option<gpui::SharedString>,
}

impl MarkdownBlocks {
    pub fn push(&self, text: gpui::SharedString) {
        let mut source = self.0.lock().unwrap();
        assert!(source.complete.is_none(), "completed Markdown is immutable");
        source.bytes += text.len();
        source.chunks.push(text);
    }

    pub fn finish(&self, text: gpui::SharedString) {
        let mut source = self.0.lock().unwrap();
        if source.complete.is_none() {
            source.bytes = text.len();
            source.complete = Some(text);
        }
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap().bytes
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn revision(&self) -> (usize, bool) {
        let source = self.0.lock().unwrap();
        (
            source
                .complete
                .as_ref()
                .map_or(source.chunks.len(), |text| text.len()),
            source.complete.is_some(),
        )
    }
    pub fn chunk(&self, index: usize) -> Option<gpui::SharedString> {
        self.0.lock().unwrap().chunks.get(index).cloned()
    }
    pub(crate) fn complete(&self) -> Option<gpui::SharedString> {
        self.0.lock().unwrap().complete.clone()
    }
    pub(crate) fn release_chunks(&self) {
        let chunks = std::mem::take(&mut self.0.lock().unwrap().chunks);
        drop(chunks);
    }
    pub(crate) fn copy_text(&self) -> String {
        let source = self.0.lock().unwrap();
        source.complete.as_ref().map_or_else(
            || source.chunks.iter().map(|chunk| chunk.as_str()).collect(),
            |complete| complete.to_string(),
        )
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .chunks
            .iter()
            .map(|s| s.as_str())
            .collect()
    }
}

struct Block {
    node: FlowFragment,
    context: Arc<NodeContext>,
}

/// A Fenwick index supports append, height correction, and viewport lookup
/// without rebuilding a growing prefix of block descriptors or offsets.
#[derive(Default)]
pub(crate) struct Flow {
    blocks: Vec<Arc<Block>>,
    heights: Vec<f32>,
    sums: Vec<f32>,
    width: Option<Pixels>,
}

impl Flow {
    fn prefix(&self, mut count: usize) -> f32 {
        let mut sum = 0.;
        while count > 0 {
            sum += self.sums[count - 1];
            count &= count - 1;
        }
        sum
    }
    fn push(&mut self, block: Block) {
        let count = self.blocks.len() + 1;
        let start = count & (count - 1);
        let height = 32.;
        let sum = self.prefix(count - 1) - self.prefix(start) + height;
        self.blocks.push(Arc::new(block));
        self.heights.push(height);
        self.sums.push(sum);
    }
    fn update(&mut self, index: usize, height: f32) -> bool {
        let delta = height - self.heights[index];
        if delta.abs() < 0.5 {
            return false;
        }
        self.heights[index] = height;
        let mut i = index + 1;
        while i <= self.sums.len() {
            self.sums[i - 1] += delta;
            i += i & i.wrapping_neg();
        }
        true
    }
    fn at(&self, y: f32) -> usize {
        let mut index = 0;
        let mut sum = 0.;
        let mut bit = self.blocks.len().checked_next_power_of_two().unwrap_or(0);
        while bit > 0 {
            let next = index + bit;
            if next <= self.sums.len() && sum + self.sums[next - 1] <= y {
                sum += self.sums[next - 1];
                index = next;
            }
            bit >>= 1;
        }
        index
    }
    pub(crate) fn append(&mut self, content: ParsedContent) {
        let context = Arc::new(content.node_cx);
        for node in content.root_node.flow_fragments() {
            self.push(Block {
                node,
                context: context.clone(),
            });
        }
    }
    pub(crate) fn selected_text(&self) -> String {
        self.blocks
            .iter()
            .map(|block| block.node.selected_text())
            .collect()
    }
}

pub(crate) struct FlowElement {
    pub(crate) flow: Arc<Mutex<Flow>>,
    pub(crate) context: NodeContext,
}

impl Element for FlowElement {
    type RequestLayoutState = ();
    type PrepaintState = Vec<AnyElement>;
    fn id(&self) -> Option<ElementId> {
        Some("markdown-flow".into())
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, ()) {
        let flow = self.flow.clone();
        (
            window.request_measured_layout(Style::default(), move |known, available, _, _| {
                let width = known.width.unwrap_or(match available.width {
                    AvailableSpace::Definite(width) => width,
                    _ => px(640.),
                });
                let mut flow = flow.lock().unwrap();
                if flow.width.is_some_and(|old| old != width) {
                    // Resize is a layout-context change, not a streaming update.
                    // Invalidate measured heights once without copying any nodes.
                    flow.heights.fill(32.);
                    for (index, sum) in flow.sums.iter_mut().enumerate() {
                        let position = index + 1;
                        *sum = 32. * (position & position.wrapping_neg()) as f32;
                    }
                }
                flow.width = Some(width);
                size(width, px(flow.prefix(flow.blocks.len())))
            }),
            (),
        )
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<AnyElement> {
        let mask = window.content_mask().bounds;
        let top = (mask.top() - bounds.top()).as_f32().max(0.);
        let bottom = (mask.bottom() - bounds.top()).as_f32();
        if bottom <= 0. || mask.top() >= bounds.bottom() {
            return Vec::new();
        }
        let mut index = self.flow.lock().unwrap().at(top);
        let mut elements = Vec::new();
        let mut changed = false;
        loop {
            let (block, offset) = {
                let flow = self.flow.lock().unwrap();
                let Some(block) = flow.blocks.get(index) else {
                    break;
                };
                (block.clone(), flow.prefix(index))
            };
            if offset > bottom {
                break;
            }
            let mut context = (*block.context).clone();
            context.code_block_actions = self.context.code_block_actions.clone();
            context.style.heading_font_size = self.context.style.heading_font_size.clone();
            let mut element = block.node.render(index, &context, window, cx);
            let measured = element.layout_as_root(
                Size {
                    width: AvailableSpace::Definite(bounds.size.width),
                    height: AvailableSpace::MaxContent,
                },
                window,
                cx,
            );
            changed |= self
                .flow
                .lock()
                .unwrap()
                .update(index, measured.height.as_f32().max(1.));
            element.prepaint_at(bounds.origin + point(px(0.), px(offset)), window, cx);
            elements.push(element);
            index += 1;
        }
        if changed {
            cx.notify(window.current_view());
        }
        elements
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        elements: &mut Vec<AnyElement>,
        window: &mut Window,
        cx: &mut App,
    ) {
        for element in elements {
            element.paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        highlighter::HighlightTheme,
        text::{TextViewStyle, format::markdown},
    };

    fn parsed(text: &str) -> ParsedContent {
        let mut context = NodeContext::default();
        let root_node = markdown::parse(
            text,
            &TextViewStyle::default(),
            &mut context,
            &HighlightTheme::default_light(),
        )
        .unwrap();
        ParsedContent {
            root_node,
            node_cx: context,
            flow: None,
        }
    }

    #[test]
    fn appended_blocks_keep_identity_and_height_index() {
        let mut flow = Flow::default();
        flow.append(parsed("first **bold** paragraph"));
        let first = flow.blocks[0].clone();
        flow.update(0, 91.);
        for index in 0..4_000 {
            flow.append(parsed(&format!("paragraph {index}")));
        }
        assert!(Arc::ptr_eq(&first, &flow.blocks[0]));
        assert_eq!(flow.heights[0], 91.);
        assert_eq!(flow.at(90.), 0);
        assert_eq!(flow.at(91.), 1);
        assert_eq!(flow.prefix(flow.blocks.len()), 91. + 4_000. * 32.);
    }

    #[test]
    fn final_flow_shares_document_references_and_full_copy_source() {
        let document = MarkdownBlocks::default();
        document.push("[link][target] and note[^n]\n\n".into());
        document.push("[target]: https://example.com\n\n[^n]: **footnote**\n".into());
        let canonical = document.text();
        document.finish(canonical.clone().into());
        let mut flow = Flow::default();
        flow.append(parsed(&canonical));
        assert_eq!(document.copy_text(), canonical);
        assert!(flow.blocks.len() >= 3);
        for block in &flow.blocks {
            assert_eq!(
                block.context.link_refs.get("target").unwrap().url.as_str(),
                "https://example.com"
            );
        }
    }
}
