use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::types::AgentHarnessOptions;
use rpi_tui::terminal::{InputEvent, Terminal, TerminalInfo};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

struct TestTerminal(Arc<AtomicUsize>);
impl Terminal for TestTerminal {
    fn info(&self) -> TerminalInfo {
        TerminalInfo {
            columns: self.0.load(Ordering::Relaxed),
            rows: 24,
            ..Default::default()
        }
    }
    fn write(&self, _: &str) {}
    fn hide_cursor(&self) {}
    fn show_cursor(&self) {}
    fn move_cursor(&self, _: usize, _: usize) {}
    fn clear_screen(&self) {}
    fn set_title(&self, _: &str) {}
    fn enable_mouse(&self) {}
    fn disable_mouse(&self) {}
    fn enter_raw_mode(&self) {}
    fn exit_raw_mode(&self) {}
    fn refresh_size(&self) {}
    fn start(&self, _: Box<dyn Fn(InputEvent) + Send + Sync>, _: Box<dyn Fn() + Send + Sync>) {}
    fn stop(&self) {}
    fn is_tty(&self) -> bool {
        false
    }
    fn set_progress(&self, _: bool) {}
    fn flush(&self) {}
}
struct Fixture {
    ui: SideQuestionUi,
    tui: Arc<TuiAltScreen>,
    host: Arc<dyn RuntimeActionHost>,
    parent: AgentHarness,
    width: Arc<AtomicUsize>,
    keys: Arc<rpi_tui::keybindings::Keybindings>,
    main_scroll: Arc<ScrollView>,
}
impl Fixture {
    async fn new(slow: bool) -> Self {
        let script = if slow {
            FauxScript::new()
                .with_text("slow answer")
                .with_tokens_per_second(0.1)
        } else {
            FauxScript::new().with_text("answer")
        };
        let provider = FauxProvider::new(script);
        let parent = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider],
            ..Default::default()
        })
        .await
        .unwrap();
        let (host, cell) = crate::extensions_actions::HarnessActionHost::new_empty(
            vec![],
            std::env::temp_dir(),
            tokio::runtime::Handle::current(),
            Default::default(),
            rpi_extensions::ToolCallContext::new("."),
        );
        cell.set(parent.downgrade()).ok().unwrap();
        let host: Arc<dyn RuntimeActionHost> = Arc::new(host);
        host.side_question(
            7,
            json!({"op":"show"}),
            Arc::new(|_| Ok(json!({"ok":true}))),
        )
        .await
        .unwrap();
        let width = Arc::new(AtomicUsize::new(120));
        let tui = Arc::new(TuiAltScreen::new(
            Box::new(TestTerminal(width.clone())),
            false,
            None,
        ));
        let keys = Arc::new(rpi_tui::keybindings::Keybindings::new());
        let editor = Arc::new(Editor::simple());
        editor.set_text("main draft must survive");
        let main_scroll = Arc::new(ScrollView::new(
            Arc::new(Text::new("MAIN\n".repeat(100), 0, 0)),
            ScrollViewOptions::default(),
        ));
        let main = Arc::new(VStack::from_entries(vec![
            StackEntry::new(main_scroll.clone()).basis(0).grow(1),
            StackEntry::new(editor.clone()).shrink(0),
        ]));
        let ui = SideQuestionUi::new(
            main,
            editor,
            false,
            true,
            Arc::new(FooterComponent::new()),
            Arc::new(Text::new("", 0, 0)),
        );
        let mut fixture = Self {
            ui,
            tui,
            host,
            parent,
            width,
            keys,
            main_scroll,
        };
        fixture.sync(true);
        fixture
    }
    fn sync(&mut self, allowed: bool) -> bool {
        self.ui.sync_host(
            &self.tui,
            Some(self.host.clone()),
            tokio::runtime::Handle::current(),
            self.parent.session().storage().metadata().id.clone(),
            self.keys.clone(),
            allowed,
        )
    }
    fn frame(&self) -> String {
        let frame = rpi_tui::layout::render_layout_frame(
            self.ui.root(),
            self.width.load(Ordering::Relaxed),
            24,
        );
        for line in &frame.lines {
            assert!(rpi_tui::ansi::visible_width(line) <= self.width.load(Ordering::Relaxed));
        }
        strip_ansi(&frame.lines.join("\n"))
    }
}
fn snapshot(text: &str) -> Value {
    json!({"status":"running","turns":[{"requestId":"test","question":"问题 **question**","text":text,"status":"running"}],"queued":[]})
}
#[tokio::test]
async fn side_view_includes_the_shared_footer() {
    let fixture = Fixture::new(false).await;
    let view = fixture.ui.view().unwrap();
    let rendered = strip_ansi(&view.root.render(60).join("\n"));
    assert!(
        rendered.contains("claude-sonnet-5"),
        "side footer: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submitted_question_and_answer_reach_the_side_view_after_sync() {
    let mut fixture = Fixture::new(false).await;
    let view = fixture.ui.view().unwrap();
    view.editor.set_text("visible side question");
    view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    tokio::time::timeout(Duration::from_secs(3), async {
        while view.submitting.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let views = fixture.host.side_question_views();
            if views
                .first()
                .is_some_and(|snapshot| snapshot["status"] == "done")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    assert!(fixture.sync(true));
    let rendered = fixture.frame();
    assert!(
        rendered.contains("visible side question"),
        "side question: {rendered}"
    );
    assert!(rendered.contains("answer"), "side answer: {rendered}");
}

#[tokio::test]
async fn native_markdown_full_answer_is_scrollable_without_jumping_on_stream_updates() {
    let fixture = Fixture::new(false).await;
    let view = fixture.ui.view().unwrap();
    let text = format!(
        "# START\n\n**bold text**\n\n```rust\nlet 中文 = 42;\n```\n\n{}\nLAST-LINE",
        "中文内容 long line to wrap naturally\n".repeat(120)
    );
    view.update(&snapshot(&text));
    fixture.frame();
    assert!(view.scroll.content_height() > 120);
    view.scroll.scroll_to_start();
    let top = fixture.frame();
    assert!(top.contains("START"));
    assert!(top.contains("bold text"));
    assert!(!top.contains("**bold text**"));
    view.key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    assert!(fixture.frame().contains("LAST-LINE"));
    view.scroll.scroll_by(-12);
    fixture.frame();
    let position = view.scroll.scroll_top();
    assert!(!view.scroll.is_following_end());
    view.update(&snapshot(&format!(
        "{text}\nMORE-OUTPUT\n{}",
        "more\n".repeat(40)
    )));
    fixture.frame();
    assert_eq!(view.scroll.scroll_top(), position);
    assert!(!view.scroll.is_following_end());
}
#[tokio::test]
async fn side_editor_matches_main_working_spinner_and_repaints_while_running() {
    let mut fixture = Fixture::new(false).await;
    let view = fixture.ui.view().unwrap();

    view.update(&snapshot("partial answer"));
    let first = strip_ansi(&view.editor.render(80)[0]);
    assert!(first.contains("Working…"), "side editor border: {first}");
    // Frames come from elapsed wall clock (shared with the main editor's
    // indicator), so repaints within one 80ms window show the same glyph;
    // animation across windows is covered by the rpi-tui editor tests.
    let second = strip_ansi(&view.editor.render(80)[0]);
    assert_eq!(
        first, second,
        "same-window repaints must not move the shared spinner"
    );

    assert!(fixture.sync(true), "running side chat must request repaint");

    view.update(&json!({
        "status": "done",
        "turns": [{
            "requestId": "test",
            "question": "question",
            "text": "complete",
            "status": "done"
        }],
        "queued": []
    }));
    assert!(
        view.editor.working().is_none(),
        "completed side chat stops spinner"
    );
    assert!(
        !fixture.sync(true),
        "idle side chat must not request animation repaint"
    );
}

#[tokio::test]
async fn resize_focus_and_paste_preserve_independent_drafts_and_restore_main() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut f = Fixture::new(false).await;
    assert!(f.ui.focused());
    assert_eq!(f.ui.root().get_children().len(), 3);
    f.ui.paste("side draft", &f.tui);
    let view = f.ui.view().unwrap();
    assert_eq!(view.editor.get_expanded_text(), "side draft");
    assert_eq!(
        f.ui.main_editor.get_expanded_text(),
        "main draft must survive"
    );
    f.width.store(80, Ordering::Relaxed);
    f.sync(true);
    assert_eq!(f.ui.root().get_children().len(), 1);
    assert!(!f.frame().contains("main draft must survive"));
    f.width.store(120, Ordering::Relaxed);
    f.sync(true);
    assert_eq!(f.ui.root().get_children().len(), 3);
    assert!(Arc::ptr_eq(&view, &f.ui.view().unwrap()));
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 2,
        row: 22,
        modifiers: KeyModifiers::NONE,
    };
    assert!(f.ui.mouse(&click, &f.tui, &f.main_scroll));
    assert!(!f.ui.focused());
    assert!(!f.ui.paste("not a side paste", &f.tui));
    assert_eq!(view.editor.get_expanded_text(), "side draft");
    f.host
        .side_question_ui(7, view.id.clone(), json!({"op":"hide"}))
        .await
        .unwrap();
    f.sync(true);
    assert_eq!(f.ui.root().get_children().len(), 1);
    assert!(f.frame().contains("main draft must survive"));
    assert!(f.ui.main_editor.is_focused());
}
#[tokio::test]
async fn modal_dialogs_suspend_side_input_without_overwriting_their_focus() {
    let mut f = Fixture::new(false).await;
    let modal: Arc<dyn Component> = Arc::new(Text::new("permission dialog", 0, 0));
    f.tui.set_focus(Some(modal.clone()));
    f.sync(false);
    assert!(!f.ui.focused());
    assert_eq!(f.ui.root().get_children().len(), 1);
    assert!(Arc::ptr_eq(&modal, &f.tui.get_focus().unwrap()));
    assert!(!f.ui.key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &f.tui
    ));
    f.sync(true);
    assert!(f.ui.focused());
    assert_eq!(f.ui.root().get_children().len(), 3);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follow_up_editor_submits_and_queue_rejection_preserves_draft() {
    let f = Fixture::new(true).await;
    let view = f.ui.view().unwrap();
    view.editor.set_text("first question");
    view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    tokio::time::timeout(Duration::from_secs(3), async {
        while view.submitting.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(view.editor.get_expanded_text(), "");
    tokio::time::timeout(Duration::from_secs(3), async {
        while f.host.side_question_views()[0]["status"] != "running" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    for n in 0..8 {
        f.host
            .side_question_ui(
                7,
                view.id.clone(),
                json!({"op":"start","question":format!("queued {n}")}),
            )
            .await
            .unwrap();
    }
    view.editor.set_text("do not lose this rejected draft");
    view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    tokio::time::timeout(Duration::from_secs(3), async {
        while view.submitting.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        view.editor.get_expanded_text(),
        "do not lose this rejected draft"
    );
    assert!(strip_ansi(&view.notice.render(80).join("\n")).contains("queue is full"));
    f.host
        .side_question_ui(7, view.id.clone(), json!({"op":"cancel"}))
        .await
        .unwrap();
    assert!(f.parent.get_tip_id().await.unwrap().is_none());
}
#[tokio::test]
async fn mouse_wheel_targets_pointer_pane_without_stealing_keyboard_focus() {
    use crossterm::event::{MouseEvent, MouseEventKind};
    let f = Fixture::new(false).await;
    let mut ui = f.ui;
    let view = ui.view().unwrap();
    view.update(&snapshot(&"content\n".repeat(100)));
    rpi_tui::layout::render_layout_frame(ui.root(), 120, 24);
    let main_position = f.main_scroll.scroll_top();
    let side_position = view.scroll.scroll_top();
    let event = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 100,
        row: 3,
        modifiers: KeyModifiers::NONE,
    };
    assert!(ui.mouse(&event, &f.tui, &f.main_scroll));
    assert!(view.scroll.scroll_top() < side_position);
    assert_eq!(f.main_scroll.scroll_top(), main_position);
    assert!(ui.focused());
    let event = MouseEvent { column: 2, ..event };
    ui.mouse(&event, &f.tui, &f.main_scroll);
    assert!(f.main_scroll.scroll_top() < main_position);
    assert!(ui.focused());
}

#[tokio::test]
async fn regular_mode_uses_a_height_bounded_full_screen_and_restores_main_draft() {
    let mut f = Fixture::new(false).await;
    let main = f.ui.main.clone();
    let editor = f.ui.main_editor.clone();
    f.ui = SideQuestionUi::new(
        main,
        editor,
        true,
        true,
        Arc::new(FooterComponent::new()),
        Arc::new(Text::new("", 0, 0)),
    );
    f.sync(true);
    let view = f.ui.view().unwrap();
    view.update(&snapshot(&format!(
        "{}\nLAST-REGULAR-LINE",
        "regular content\n".repeat(120)
    )));
    let rows = f.ui.root().render(120);
    assert!(rows.len() <= 24);
    assert!(strip_ansi(&rows.join("\n")).contains("LAST-REGULAR-LINE"));
    assert!(!strip_ansi(&rows.join("\n")).contains("main draft must survive"));
    view.scroll.scroll_to_start();
    assert!(!strip_ansi(&f.ui.root().render(120).join("\n")).contains("LAST-REGULAR-LINE"));
    f.host
        .side_question_ui(7, view.id.clone(), json!({"op":"hide"}))
        .await
        .unwrap();
    f.sync(true);
    assert!(strip_ansi(&f.ui.root().render(120).join("\n")).contains("main draft must survive"));
    assert!(f.ui.empty_draft() == view.editor.get_expanded_text().is_empty());
}
