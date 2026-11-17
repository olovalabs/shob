//! Bulk reads for an already confirmed Windows console text paste.
//!
//! Crossterm reads one console record per call. For large pastes, the console
//! round trips dominate the time before the editor can fold the text.

use std::io;

use winapi::um::{
    consoleapi::ReadConsoleInputW,
    processenv::GetStdHandle,
    winbase::STD_INPUT_HANDLE,
    wincon::{
        PeekConsoleInputW, INPUT_RECORD, KEY_EVENT, LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED,
        RIGHT_ALT_PRESSED, RIGHT_CTRL_PRESSED, SHIFT_PRESSED,
    },
    winuser::{
        VK_BACK, VK_CONTROL, VK_DELETE, VK_ESCAPE, VK_F1, VK_F24, VK_MENU, VK_PRIOR, VK_SHIFT,
        VK_TAB,
    },
};

/// Read up to 1024 queued console records belonging to a confirmed text paste.
///
/// The caller must be the console's sole input reader, with no event cached by
/// its normal reader. Non-text events and Ctrl/Alt shortcuts remain queued for
/// that reader. A trailing Enter also stays queued so it can still submit.
/// Complete UTF-16 pairs are consumed together, including their key releases.
pub fn read_text_batch() -> io::Result<String> {
    // SAFETY: the console APIs initialize only the reported number of records.
    // INPUT_RECORD contains only integers and unions of POD console records.
    let mut records: [INPUT_RECORD; 1024] = unsafe { std::mem::zeroed() };
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut count = 0;
    if unsafe {
        PeekConsoleInputW(
            handle,
            records.as_mut_ptr(),
            records.len() as u32,
            &mut count,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let (take, _) = text_prefix(&records[..count as usize]);
    if take == 0 {
        return Ok(String::new());
    }
    // Peeked records cannot disappear while the sole reader owns the queue.
    // Read only the safe prefix; never steal a later mouse or shortcut event.
    if unsafe { ReadConsoleInputW(handle, records.as_mut_ptr(), take as u32, &mut count) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(text_prefix(&records[..count as usize]).1)
}

fn text_prefix(records: &[INPUT_RECORD]) -> (usize, String) {
    let mut units = Vec::new();
    let mut take = 0;
    let mut committed_units = 0;
    let mut high_surrogate = false;
    let mut previous_cr = false;
    for (index, record) in records.iter().enumerate() {
        if record.EventType != KEY_EVENT {
            break;
        }
        // SAFETY: EventType selects the KeyEvent member of the union.
        let key = unsafe { record.Event.KeyEvent() };
        let vk = key.wVirtualKeyCode as i32;
        if matches!(vk, VK_BACK | VK_ESCAPE | VK_SHIFT | VK_CONTROL | VK_MENU)
            || (VK_PRIOR..=VK_DELETE).contains(&vk)
            || (VK_F1..=VK_F24).contains(&vk)
            || (vk == VK_TAB && key.dwControlKeyState & SHIFT_PRESSED != 0)
        {
            break;
        }
        if key.dwControlKeyState
            & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED | LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED)
            != 0
        {
            break;
        }
        let unit = unsafe { *key.uChar.UnicodeChar() };
        if unit < 32 && !matches!(unit, 9 | 10 | 13) || unit == 127 {
            break;
        }
        if key.bKeyDown != 0 {
            if (0xd800..=0xdbff).contains(&unit) {
                if high_surrogate {
                    break;
                }
                high_surrogate = true;
            } else if (0xdc00..=0xdfff).contains(&unit) {
                if !high_surrogate {
                    break;
                }
                high_surrogate = false;
            } else if high_surrogate {
                break;
            }
            // Some console clients inject CRLF, others just CR. Treat either
            // as one newline, even when release records separate the pair.
            if unit != 10 || !previous_cr {
                units.push(if unit == 13 { 10 } else { unit });
            }
            previous_cr = unit == 13;
        }
        // Do not consume an incomplete scalar or final pasted/typed Enter.
        // The normal reader can inspect what arrives behind it and decide.
        if !high_surrogate && units.last() != Some(&10) {
            take = index + 1;
            committed_units = units.len();
        }
    }
    units.truncate(committed_units);
    (
        take,
        String::from_utf16(&units).expect("complete UTF-16 paste prefix"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(unit: u16, down: bool, modifiers: u32) -> INPUT_RECORD {
        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        record.EventType = KEY_EVENT;
        unsafe {
            let key = record.Event.KeyEvent_mut();
            key.bKeyDown = i32::from(down);
            key.wRepeatCount = 1;
            key.dwControlKeyState = modifiers;
            *key.uChar.UnicodeChar_mut() = unit;
        }
        record
    }

    fn records(text: &str) -> Vec<INPUT_RECORD> {
        text.encode_utf16()
            .flat_map(|unit| [key(unit, true, 0), key(unit, false, 0)])
            .collect()
    }

    #[test]
    fn large_unicode_paste_preserves_text_across_batches() {
        let text = "中文🙂\t pasted text\n".repeat(6000);
        let queue = records(&text);
        let mut offset = 0;
        let mut actual = String::new();
        while offset < queue.len() {
            let end = (offset + 1024).min(queue.len());
            let (take, chunk) = text_prefix(&queue[offset..end]);
            if take == 0 {
                // The final newline stays for the normal Enter handler.
                assert_eq!(text_prefix(&queue[offset..]).0, 0);
                break;
            }
            offset += take;
            actual.push_str(&chunk);
        }
        assert_eq!(actual, text.trim_end_matches('\n'));
        assert_eq!(queue.len() - offset, 2);
        let editor = crate::Editor::simple();
        editor.handle_paste(&actual);
        assert!(editor.get_text().starts_with("[paste #1"));
        assert_eq!(
            editor.get_expanded_text(),
            crate::editor::expand_tabs(&actual, crate::editor::PASTE_TAB_WIDTH)
        );
    }

    #[test]
    fn shortcuts_and_mouse_events_are_left_for_normal_dispatch() {
        let mut alt_code = key('中' as u16, false, 0);
        unsafe {
            alt_code.Event.KeyEvent_mut().wVirtualKeyCode = VK_MENU as u16;
        }
        let mut back_tab = key(9, true, SHIFT_PRESSED);
        unsafe {
            back_tab.Event.KeyEvent_mut().wVirtualKeyCode = VK_TAB as u16;
        }
        for event in [
            key('c' as u16, true, LEFT_CTRL_PRESSED),
            key('x' as u16, true, LEFT_ALT_PRESSED),
            alt_code,
            back_tab,
            unsafe { std::mem::zeroed() },
        ] {
            let mut queue = records("paste");
            let expected = queue.len();
            queue.push(event);
            queue.extend(records("later"));
            assert_eq!(text_prefix(&queue), (expected, "paste".into()));
        }
    }

    #[test]
    fn incomplete_surrogate_pair_remains_queued() {
        let mut queue = records("中");
        queue.push(key(0xd83d, true, 0));
        queue.push(key(0xd83d, false, 0));
        assert_eq!(text_prefix(&queue), (2, "中".into()));
        queue.push(key(0xde42, true, 0));
        queue.push(key(0xde42, false, 0));
        assert_eq!(text_prefix(&queue), (6, "中🙂".into()));
    }

    #[test]
    fn interior_newlines_and_tabs_are_text_but_final_enter_is_not_consumed() {
        let queue = records("a\r\nb\tc\r");
        assert_eq!(text_prefix(&queue), (queue.len() - 2, "a\nb\tc".into()));
        assert_eq!(text_prefix(&records("\r")), (0, String::new()));
    }
}
