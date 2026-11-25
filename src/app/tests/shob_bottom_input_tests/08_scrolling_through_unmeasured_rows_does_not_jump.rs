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
            let mut text = format!("Stretch message {i} sits in the unmeasured region.");
            if i % 5 == 0 {
                for _ in 0..16 {
                    text.push_str("\n\n- A nested list\n  - A child item\n\n```rust\nlet value = 1;\n```\n\n| Name | Value |\n| --- | --- |\n| Long identifier | `example_value` |\n");
                }
            }
            pane.chat.messages.push(Arc::new(ChatMessage::new(ChatRole::Assistant, text)));
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
        !state.read_with(&cx, |s, _| s.pane_ref(pane_id).unwrap().chat.list_state.is_following_tail()),
        "the upward fling must detach from tail-following"
    );

    // Small follow-up ticks keep moving in the same direction even as uneven rows are measured.
    let mut moved = anchor;
    for _ in 0..4 {
        cx.simulate_event(gpui::ScrollEvent {
            position: bounds.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(300.0))),
            modifiers: Default::default(),
            touch_phase: gpui::TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
        cx.draw_frame().expect("layout after an upward tick");
        let current = state.read_with(&cx, |s, _| {
            s.pane_ref(pane_id).unwrap().chat.list_state.logical_scroll_top()
        });
        assert!(
            (current.item_ix, current.offset_in_item) < (moved.item_ix, moved.offset_in_item),
            "an upward tick must not bounce back down (from {moved:?} to {current:?})"
        );
        assert!(current.item_ix > 0, "small ticks must not collapse to the top");
        moved = current;
    }

    // Scroll back down a little: it must move down a bounded amount and
    // never overshoot into the tail on hinted rows.
    for delta in [-80.0, -600.0, -1400.0, 80.0, 1400.0] {
        cx.simulate_event(gpui::ScrollEvent {
            position: bounds.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(delta))),
            modifiers: Default::default(),
            touch_phase: gpui::TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
        cx.draw_frame().expect("layout after changing scroll speed or direction");
        let current = state.read_with(&cx, |s, _| {
            let list = &s.pane_ref(pane_id).unwrap().chat.list_state;
            assert!(!list.is_following_tail());
            list.logical_scroll_top()
        });
        let previous = (moved.item_ix, moved.offset_in_item);
        let next = (current.item_ix, current.offset_in_item);
        assert!(current.item_ix > 0 && current.item_ix < count);
        assert!(
            if delta < 0.0 { next > previous } else { next < previous },
            "wheel motion must not reverse when measuring Markdown (delta {delta}, from {moved:?} to {current:?})"
        );
        moved = current;
    }
}
