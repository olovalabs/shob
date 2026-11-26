use anyhow::Result;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClipboardEntry, Context, CursorStyle, Div, Edges,
    ElementId, Entity, ExternalPaths, FollowMode, Hsla, InteractiveElement as _, IntoElement,
    KeyDownEvent, ListAlignment, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit,
    ParentElement, Pixels, Point, Rems, Render, SharedString, StatefulInteractiveElement as _,
    StyleRefinement, Styled, StyledImage as _, StyledText, Window, WindowDecorations,
    WindowOptions, actions, canvas, deferred, div, img, list, px, relative, rems, rgba, size, svg,
};
#[cfg(target_os = "linux")]
use gpui_component::TitleBar;
use gpui_component::WindowExt as _;
use gpui_component::button::Button;
use gpui_component::highlighter::HighlightTheme;
use gpui_component::input::{Input, InputEvent, InputState, RopeExt as _};
use gpui_component::notification::{Notification, NotificationType};
use gpui_component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::text::{MarkdownBlocks, TextView};
use gpui_component::{ActiveTheme as _, Root, Sizable as _};
use gpui_kit::base::{Scrollbar as KitScrollbar, VirtualListScrollHandle, v_virtual_list};
#[cfg(not(target_os = "linux"))]
use gpui_kit::component::TitleBar;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use terminal::{ColorPalette, ShellEvent, TerminalConfig, TerminalView};

mod agent_appearance;
mod agent_auth;
mod agent_bridge;
mod agent_composer;
mod agent_interaction;
mod agent_rules;
mod agent_writer;
mod ascii_glow;
mod assets;
mod attachments;
mod attention;
mod chat_batch;
mod cli;
mod completion;
mod conversation_header;
mod conversations;
mod diagnostics;
mod file_icons;
mod file_preview;
mod files_surface;
mod git_workspace;
mod integrations;
mod logos;
mod paths;
mod project_diff;
#[cfg(unix)]
mod shob_compat;
mod shob_storage;
use terminal::pty_session;
mod session_lifecycle;
mod settings;
mod shell_history;
#[path = "blocks/shell_integration.rs"]
mod shell_integration;
mod sound;
mod status;
#[cfg(unix)]
mod status_bus;
mod stream_timing;
mod subagent_chat;
mod system_detection;
mod theme;
mod token_efficiency;
mod updater;
mod window_chrome;
mod workspace_state;
mod workspace_switcher;

include!("app/01_init_shob_input_bindings.rs");
include!("app/02_chat_output_expander.rs");
include!("app/03_chat_tool_card.rs");
include!("app/04_chat_row_cache_tests.rs");
include!("app/05_panechat.rs");
include!("app/06_panenode.rs");
include!("app/07_appstate.rs");
include!("app/08_build_pane_input.rs");
include!("app/09_chat_render_isolation_tests.rs");
