use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Element, ElementId, GlobalElementId,
    InspectorElementId, LayoutId, ListState, Pixels, Size, Style, Window, point, px, size,
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
    layout: Arc<Mutex<FlowLayoutCache>>,
}

/// Session-local geometry belongs to the document, not the mounted text view.
#[derive(Default)]
struct FlowLayoutCache {
    heights: HashMap<usize, BlockMeasurement>,
    total: f32,
}

#[derive(Clone, Copy)]
struct BlockMeasurement {
    height: f32,
    width: Pixels,
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
    pub(crate) fn cached_height(&self) -> Pixels {
        let layout = self.0.lock().unwrap().layout.clone();
        px(layout.lock().unwrap().total)
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
    measured_at: Vec<Option<Pixels>>,
    sums: Vec<f32>,
    retained: Option<Arc<Mutex<FlowLayoutCache>>>,
}

impl Flow {
    pub(crate) fn for_completed_document(document: &MarkdownBlocks) -> Self {
        let retained = document.0.lock().unwrap().layout.clone();
        Self {
            retained: Some(retained),
            ..Self::default()
        }
    }

    // Intrinsic layout can probe several widths. Keep the last heights as
    // estimates and only update them when prepaint knows the actual width.
    fn layout_height(&self) -> Pixels {
        px(self.prefix(self.blocks.len()))
    }

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
        let cached = self
            .retained
            .as_ref()
            .and_then(|retained| retained.lock().unwrap().heights.get(&(count - 1)).copied());
        let height = cached.map_or(32., |measurement| measurement.height);
        let sum = self.prefix(count - 1) - self.prefix(start) + height;
        self.blocks.push(Arc::new(block));
        self.heights.push(height);
        self.measured_at
            .push(cached.map(|measurement| measurement.width));
        self.sums.push(sum);
    }
    fn update(&mut self, index: usize, height: f32, width: Pixels) -> bool {
        let delta = height - self.heights[index];
        let changed = delta.abs() >= 0.5;
        if !changed && self.measured_at[index] == Some(width) {
            return false;
        }
        self.measured_at[index] = Some(width);
        if changed {
            self.heights[index] = height;
            let mut i = index + 1;
            while i <= self.sums.len() {
                self.sums[i - 1] += delta;
                i += i & i.wrapping_neg();
            }
        }
        if let Some(retained) = &self.retained {
            let mut cache = retained.lock().unwrap();
            cache.heights.insert(
                index,
                BlockMeasurement {
                    height: self.heights[index],
                    width,
                },
            );
            cache.total = self.layout_height().as_f32();
        }
        changed
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
    pub(crate) list_row: Option<FlowListRow>,
}

#[derive(Clone)]
pub(crate) struct FlowListRow {
    pub(crate) list: ListState,
    pub(crate) index: usize,
    pub(crate) preserve_anchor: bool,
}

impl Element for FlowElement {
    type RequestLayoutState = ();
    type PrepaintState = (Vec<AnyElement>, Option<ContentMask<Pixels>>);
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
                size(width, flow.lock().unwrap().layout_height())
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
    ) -> Self::PrepaintState {
        let mask = window.content_mask().bounds;
        let top = (mask.top() - bounds.top()).as_f32().max(0.);
        let bottom = (mask.bottom() - bounds.top()).as_f32();
        if bottom <= 0. || mask.top() >= bounds.bottom() {
            return Self::PrepaintState::default();
        }
        let (anchor_index, anchor_origin) = {
            let flow = self.flow.lock().unwrap();
            let index = flow.at(top);
            (index, flow.prefix(index))
        };
        let anchor_offset = top - anchor_origin;
        let anchored = top > 0.
            && self
                .list_row
                .as_ref()
                .is_some_and(|row| row.preserve_anchor);
        let mut index = if anchored {
            anchor_index.saturating_sub(2)
        } else {
            anchor_index
        };
        let mut measured_rows = Vec::new();
        let mut changed = false;
        let mut correction = 0.;
        loop {
            let (block, prev_is_heading, offset) = {
                let flow = self.flow.lock().unwrap();
                let Some(block) = flow.blocks.get(index) else {
                    break;
                };
                if index < anchor_index && flow.measured_at[index] == Some(bounds.size.width) {
                    index += 1;
                    continue;
                }
                let prev_is_heading = index > 0 && flow.blocks[index - 1].node.is_heading();
                (block.clone(), prev_is_heading, flow.prefix(index))
            };
            if index > anchor_index && offset > bottom + correction {
                break;
            }
            let mut context = (*block.context).clone();
            context.code_block_actions = self.context.code_block_actions.clone();
            context.style.heading_font_size = self.context.style.heading_font_size.clone();
            let mut element =
                block
                    .node
                    .render(index, index == 0, prev_is_heading, &context, window, cx);
            let measured = element.layout_as_root(
                Size {
                    width: AvailableSpace::Definite(bounds.size.width),
                    height: AvailableSpace::MaxContent,
                },
                window,
                cx,
            );
            let mut flow = self.flow.lock().unwrap();
            let height = measured.height.as_f32().max(1.);
            changed |= flow.update(index, height, bounds.size.width);
            if anchored && index == anchor_index {
                let inset = if anchor_offset < height {
                    anchor_offset
                } else {
                    height - 1.
                };
                correction = (offset - anchor_origin) + (inset - anchor_offset);
            }
            if index >= anchor_index {
                measured_rows.push((offset, height, element));
            }
            index += 1;
        }
        if changed || correction != 0. {
            let view = window.current_view();
            if let Some(row) = self.list_row.clone().filter(|row| row.preserve_anchor) {
                // The owning list is borrowed until prepaint finishes.
                window.defer(cx, move |_, cx| {
                    let mut refresh = changed;
                    if row.index < row.list.item_count() {
                        let mut anchor = row.list.logical_scroll_top();
                        if correction != 0.
                            && anchor.item_ix == row.index
                            && !row.list.is_following_tail()
                        {
                            anchor.offset_in_item =
                                (anchor.offset_in_item + px(correction)).max(px(0.));
                            row.list.scroll_to(anchor);
                            refresh = true;
                        }
                        if changed {
                            row.list.remeasure_items(row.index..row.index + 1);
                        }
                    }
                    if refresh {
                        cx.notify(view);
                    }
                });
            } else if changed {
                // Tail-follow lays out visible rows again on the next frame.
                cx.notify(view);
            }
        }
        // The list has already placed siblings using this frame's height.
        // Never paint newly measured blocks outside that reservation.
        let height = bounds.size.height.as_f32();
        let bottom = bottom.min(height);
        measured_rows.retain_mut(|(offset, block_height, _)| {
            *offset -= correction;
            *offset < bottom && *offset + *block_height > top
        });
        let spills_above = mask.top() < bounds.top()
            && measured_rows
                .first()
                .is_some_and(|(offset, _, _)| *offset < 0.);
        let spills_below = mask.bottom() > bounds.bottom()
            && measured_rows
                .last()
                .is_some_and(|(offset, block_height, _)| *offset + *block_height > height);
        let clip = (spills_above || spills_below).then_some(ContentMask { bounds });
        let elements = window.with_content_mask(clip, |window| {
            measured_rows
                .into_iter()
                .map(|(offset, _, mut element)| {
                    element.prepaint_at(bounds.origin + point(px(0.), px(offset)), window, cx);
                    element
                })
                .collect()
        });
        (elements, clip)
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        (elements, clip): &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(*clip, |window| {
            for element in elements {
                element.paint(window, cx);
            }
        });
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
        flow.update(0, 91., px(500.));
        for index in 0..4_000 {
            flow.append(parsed(&format!("paragraph {index}")));
        }
        assert!(Arc::ptr_eq(&first, &flow.blocks[0]));
        assert_eq!(flow.heights[0], 91.);
        assert_eq!(flow.at(90.), 0);
        assert_eq!(flow.at(91.), 1);
        assert_eq!(flow.prefix(flow.blocks.len()), 91. + 4_000. * 32.);

        let document = MarkdownBlocks::default();
        let mut original = Flow::for_completed_document(&document);
        original.append(parsed("first\n\nsecond\n\nthird"));
        original.update(0, 91., px(500.));
        original.update(1, 12., px(500.));
        drop(original);
        assert_eq!(document.cached_height(), px(135.));

        let mut remounted = Flow::for_completed_document(&document);
        remounted.append(parsed("first\n\nsecond\n\nthird"));
        assert_eq!(remounted.layout_height(), px(135.));
        assert_eq!(remounted.at(95.), 1);
        assert!(remounted.update(1, 24., px(350.)));
        assert_eq!(remounted.layout_height(), px(147.));
        assert_eq!(
            remounted.measured_at,
            [Some(px(500.)), Some(px(350.)), None]
        );
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
