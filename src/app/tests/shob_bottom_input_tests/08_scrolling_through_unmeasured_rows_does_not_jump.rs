#[test]
fn scrolling_through_unmeasured_rows_does_not_jump_to_top_or_bottom() {
    // Rows that have never been laid out are zero-height in the list's pixel
    // map. A transcript restored from disk (or anything taller than the
    // rendering overdraw) is full of them, and one wheel tick inside that
    // stretch used to snap the view to the very top or bottom. Seed hints
    // keep the pixel map dense so scrolling moves smoothly instead.
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane_id, true, window, cx);
        let pane = s.pane_mut(pane_id).unwrap();
        for i in 0..40 {
            pane.chat.messages.push(Arc::new(ChatMessage::new(
                ChatRole::Assistant,
                format!("Warm-up message {i} fills the first measured stretch."),
            )));
        }
    });
    cx.draw_frame().expect("draw warm-up transcript");

    // Add a long stretch in one step: after the next frame only the viewport
    // plus overdraw is measured, the rest is hinted-only space.
    cx.update_window_entity(&state, |s, _, cx| {
        let pane = s.pane_mut(pane_id).unwrap();
        for i in 40..220 {
            pane.chat.messages.push(Arc::new(ChatMessage::new(
                ChatRole::Assistant,
                format!("Stretch message {i} sits in the unmeasured region."),
            )));
        }
        cx.notify();
    });
    cx.draw_frame().expect("draw tall transcript");
    let count = state.read_with(&cx, |s, _| s.pane_ref(pane_id).unwrap().chat.list_state.item_count());
    assert!(count >= 220);

    let bounds = cx.debug_bounds("agent-chat").expect("chat viewport");
    cx.simulate_event(gpui::ScrollEvent {
        position: bounds.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(1500.0))),
        modifiers: Default::default(),
        touch_phase: gpui::TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    });
    cx.draw_frame().expect("layout after the fling");

    let anchor = state.read_with(&cx, |s, _| {
        s.pane_ref(pane_id).unwrap().chat.list_state.logical_scroll_top()
    });
    assert!(
        anchor.item_ix > 0 && anchor.item_ix < count,
        "a fast scroll through unmeasured rows must land mid-transcript, not snap to row 0 or the tail (anchor {anchor:?} of {count})"
    );
    assert!(
        !state.read_with(&cx, |s, _| s.pane_ref(pane_id).unwrap().chat.autoscroll),
        "the upward fling must detach from tail-following"
    );

    // Small follow-up ticks keep moving gradually in the same direction.
    for _ in 0..4 {
        cx.simulate_event(gpui::ScrollEvent {
            position: bounds.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(300.0))),
            modifiers: Default::default(),
            touch_phase: gpui::TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
    }
    cx.draw_frame().expect("layout after follow-up ticks");
    let moved = state.read_with(&cx, |s, _| {
        s.pane_ref(pane_id).unwrap().chat.list_state.logical_scroll_top()
    });
    assert!(
        moved.item_ix <= anchor.item_ix,
        "continued upward scrolls must not bounce back down (from {anchor:?} to {moved:?})"
    );
    assert!(
        moved.item_ix > 0,
        "small ticks inside the unmeasured stretch must not collapse to the top"
    );

    // Scroll back down a little: it must move down a bounded amount and
    // never overshoot into the tail on hinted rows.
    cx.simulate_event(gpui::ScrollEvent {
        position: bounds.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-600.0))),
        modifiers: Default::default(),
        touch_phase: gpui::TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    });
    cx.draw_frame().expect("layout after scrolling back down");
    state.read_with(&cx, |s, _| {
        let chat = &s.pane_ref(pane_id).unwrap().chat;
        let anchor = chat.list_state.logical_scroll_top();
        assert!(
            anchor.item_ix > moved.item_ix && anchor.item_ix < chat.list_state.item_count(),
            "a small downward tick must move a bounded amount, not snap to the tail (from {moved:?} to {anchor:?})"
        );
    });
}
