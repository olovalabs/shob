use gpui::{AnyElement, App, Context, IntoElement, ParentElement as _, Styled as _, Task, Window};

use crate::{
    ActiveTheme as _, Icon, IconName, IndexPath, Selectable, h_flex,
    list::{ListState, loading::Loading},
};

#[allow(unused)]
pub trait ListDelegate: Sized + 'static {
    type Item: Selectable + IntoElement;

    /// When Query Input change, this method will be called.
    /// You can perform search here.
    fn perform_search(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    /// Return the number of sections in the list, default is 1.
    fn sections_count(&self, cx: &App) -> usize {
        1
    }

    /// Return the number of items in the section at the given index.
    fn items_count(&self, section: usize, cx: &App) -> usize;

    /// Render the item at the given index.
    ///
    /// Return None will skip the item.
    ///
    /// NOTE: Every item should have same height.
    fn render_item(
        &mut self,
        ix: IndexPath,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item>;

    /// Render the section header at the given index, default is None.
    ///
    /// NOTE: Every header should have same height.
    fn render_section_header(
        &mut self,
        section: usize,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<impl IntoElement> {
        None::<AnyElement>
    }

    /// Render the section footer at the given index, default is None.
    ///
    /// NOTE: Every footer should have same height.
    fn render_section_footer(
        &mut self,
        section: usize,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<impl IntoElement> {
        None::<AnyElement>
    }

    /// Return a Element to show when list is empty.
    fn render_empty(
        &mut self,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        h_flex()
            .size_full()
            .justify_center()
            .text_color(cx.theme().muted_foreground.opacity(0.6))
            .child(Icon::new(IconName::Inbox).size_12())
            .into_any_element()
    }

    /// Returns Some(AnyElement) to render the initial state of the list.
    ///
    /// This can be used to show a view for the list before the user has
    /// interacted with it.
    ///
    /// For example: The last search results, or the last selected item.
    ///
    /// Default is None, that means no initial state.
    fn render_initial(
        &mut self,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<AnyElement> {
        None
    }

    /// Returns the loading state to show the loading view.
    fn loading(&self, cx: &App) -> bool {
        false
    }

    /// Returns a Element to show when loading, default is built-in Skeleton
    /// loading view.
    fn render_loading(
        &mut self,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        Loading
    }

    /// Set the selected index, just store the ix, don't confirm.
    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    );

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {}

    fn is_eof(&self, cx: &App) -> bool {
        true
    }

    fn load_more_threshold(&self) -> usize {
        20
    }

    fn load_more(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {}
}
