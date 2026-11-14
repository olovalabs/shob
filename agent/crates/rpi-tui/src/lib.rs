//! Terminal User Interface for pi-rust
//!
//! This crate provides a TUI system similar to the TypeScript implementation
//! in `packages/tui/`. It supports:
//! - Main screen mode (terminal scrollback)
//! - Alternate screen mode (fullscreen with constrained layout)
//! - Component-based UI with VStack, HStack, ScrollView
//! - Mouse and keyboard input handling
//! - Image support (Kitty/iTerm2 protocols)
//! - Autocomplete, search, and more

pub mod alt_screen_search;
pub mod ansi;
pub mod assistant_message;
pub mod autocomplete;
pub mod bash_execution;
pub mod bordered_loader;
pub mod box_component;
pub mod component;
pub mod container;
pub mod countdown_timer;
pub mod diff;
pub mod dynamic_border;
pub mod editor;
pub mod footer;
pub mod fuzzy;
pub mod hstack;
pub mod image;
pub mod input;
mod jump_to_end;
pub mod keybinding_hints;
pub mod keybindings;
pub mod keys;
pub mod kill_ring;
pub mod latex;
pub mod layout;
pub mod layout_node;
mod line_diff;
pub mod loader;
pub mod markdown;
pub mod mermaid;
pub mod oklab;
pub mod overlay;
pub mod render;
pub mod scroll_view;
pub mod search_bar;
pub mod searchable_select;
pub mod select_list;
pub mod selection;
pub mod settings_list;
pub mod spacer;
pub mod status_indicator;
pub mod stdin_buffer;
pub mod syntax_highlight;
pub mod terminal;
pub mod terminal_colors;
pub mod terminal_image;
pub mod text;
pub mod theme;
pub mod theme_json;
pub mod tool_execution;
mod tool_panel;
pub mod truncated_text;
pub mod tui;
pub mod tui_alt_screen;
pub mod tui_main_screen;
pub mod undo_stack;
pub mod user_message;
pub mod utils;
pub mod visual_truncate;
pub mod vstack;
#[cfg(windows)]
pub mod windows_paste;
pub mod word_navigation;

// Re-export main types
pub use alt_screen_search::{AltScreenSearch, SearchMatch, SearchState};
pub use ansi::{bold, strip_ansi, visible_width, CURSOR_MARKER};
pub use assistant_message::{AssistantBlock, AssistantMessageComponent, AssistantMessageOptions};
pub use autocomplete::{
    AutocompleteItem, AutocompleteManager, AutocompleteProvider, AutocompleteSuggestions,
    CombinedAutocompleteProvider, FilePathAutocompleteProvider, SlashCommand,
    SlashCommandAutocompleteProvider,
};
pub use bash_execution::{BashExecutionComponent, BashStatus, BashTruncation};
pub use bordered_loader::{BorderedLoader, BorderedLoaderInner};
pub use box_component::Box;
pub use component::{Component, Focusable};
pub use container::Container;
pub use countdown_timer::CountdownTimer;
pub use diff::render_diff;
pub use dynamic_border::DynamicBorder;
pub use editor::{Editor, EditorOptions, EditorStyle, WorkingState};
pub use footer::FooterComponent;
pub use fuzzy::{fuzzy_filter, fuzzy_match, FuzzyMatch};
pub use hstack::HStack;
pub use image::{Image, ImageOptions, ImageTheme};
pub use input::Input;
pub use keybinding_hints::{
    format_combo, key_code_name, key_display_text, key_hint, key_text, raw_key_hint,
};
pub use keybindings::{
    get_keybindings, set_keybindings, KeyCombo, KeybindingConflict, KeybindingDefinition,
    KeybindingId, Keybindings,
};
pub use keys::{
    is_key_release, is_key_repeat, is_kitty_protocol_active, matches_key, parse_key,
    set_kitty_protocol_active, Key, KeyEventType, KeyHelper, KeyId,
};
pub use kill_ring::{KillRing, PushOptions};
pub use latex::{is_latex, render_latex, strip_latex, RenderLatexOptions};
pub use layout::{
    composite_tui_line, extract_cursor_position, hit_test, render_layout_frame, LayoutBox,
    LayoutFrame, LayoutRect,
};
pub use layout_node::{
    allocate_stack_sizes, LayoutNode, LayoutNodeProvider, LayoutViewport, StackLayoutEntry,
};
pub use loader::{
    CancellableLoader, Loader, LoaderIndicatorOptions, ProgressLoader, SPINNER_FRAMES,
    SPINNER_FRAME_MS,
};
pub use markdown::{Markdown, MarkdownOptions};
pub use mermaid::Mermaid;
pub use overlay::{Dialog, OverlayHandle, OverlayManager, Selector, SelectorItem};
pub use scroll_view::{FollowMode, ScrollView, ScrollViewOptions};
pub use search_bar::SearchBar;
pub use searchable_select::SearchableSelectList;
pub use select_list::{SelectItem, SelectList, SelectListLayoutOptions, SelectListTheme};
pub use settings_list::{SettingItem, SettingType, SettingsList, SettingsListTheme};
pub use spacer::Spacer;
pub use status_indicator::{CompactionReason, IdleStatus, StatusIndicator, StatusKind};
pub use stdin_buffer::{StdinBuffer, StdinBufferEvent, StdinBufferOptions};
pub use terminal::{InputEvent, ProcessTerminal, Terminal, TerminalError, TerminalInfo};
pub use terminal_colors::{
    parse_osc11_background_color, parse_terminal_color_scheme_report, query_background_color,
    query_color_scheme, query_cursor_color, query_foreground_color, RgbColor, TerminalColorScheme,
};
pub use terminal_image::{
    allocate_image_id, calculate_image_rows, delete_all_kitty_images, delete_kitty_image,
    detect_capabilities, encode_iterm2, encode_kitty, get_capabilities, get_cell_dimensions,
    get_gif_dimensions, get_image_dimensions, get_jpeg_dimensions, get_png_dimensions,
    get_webp_dimensions, hyperlink, image_fallback, query_cell_dimensions, refresh_cell_dimensions,
    render_image, reset_capabilities_cache, set_capabilities, set_cell_dimensions, CellDimensions,
    ImageDimensions, ImageProtocol, ImageRenderOptions, TerminalCapabilities, DEFAULT_CELL_HEIGHT,
    DEFAULT_CELL_WIDTH,
};
pub use text::Text;
pub use theme::{
    apply_theme_preset, global_theme_manager, theme, theme_revision, Color, Theme, ThemeColors,
    ThemeManager, ThemePreset, ThemeSpacing,
};
pub use tool_execution::{ToolExecutionComponent, ToolStatus};
pub use truncated_text::TruncatedText;
pub use tui::{
    OverlayAnchor, OverlayHandle as TuiOverlayHandle, OverlayMargin, OverlayOptions, TuiMode,
    TuiStopOptions, TUI,
};
pub use tui_alt_screen::{is_viewport_tui, TuiAltScreen, VIEWPORT_TUI};
pub use tui_main_screen::TuiMainScreen;
pub use undo_stack::{RedoStack, UndoRedoManager, UndoStack};
pub use user_message::UserMessageComponent;
pub use utils::{
    apply_background_to_line, get_grapheme_segmenter, get_osc8_link_at_column, is_whitespace_char,
    slice_by_column, truncate_to_width, visible_width as visible_width_util, wrap_text_with_ansi,
    Grapheme, GraphemeSegmenter,
};
pub use visual_truncate::{truncate_to_visual_lines, VisualTruncateResult};
pub use vstack::{
    layout_vstack_constrained, StackAlign, StackChild, StackEntry, StackEntryOptions, StackOptions,
    VStack,
};
pub use word_navigation::{
    find_line_end, find_line_start, find_word_backward, find_word_end, find_word_forward,
    get_current_word_range, is_at_word_end, is_at_word_start, is_word_boundary, is_word_char,
};
