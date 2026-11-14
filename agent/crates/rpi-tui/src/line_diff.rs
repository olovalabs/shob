//! Column patches for stable terminal rows, preserving graphemes and styles.
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(PartialEq)]
struct Atom<'a> {
    text: &'a str,
    width: usize,
}
fn atoms(line: &str) -> Option<Vec<Atom<'_>>> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < line.len() {
        let tail = &line[offset..];
        if tail.starts_with('\x1b') {
            let bytes = tail.as_bytes();
            let end = if tail.starts_with("\x1b[") {
                let end = bytes
                    .iter()
                    .enumerate()
                    .skip(2)
                    .find(|(_, b)| (0x40..=0x7e).contains(*b))?
                    .0
                    + 1;
                if bytes[end - 1] != b'm' {
                    return None;
                }
                end
            } else if tail.starts_with("\x1b]8;") {
                let mut end = None;
                for i in 4..bytes.len() {
                    if bytes[i] == 7 {
                        end = Some(i + 1);
                        break;
                    }
                    if bytes[i] == 27 && bytes.get(i + 1) == Some(&b'\\') {
                        end = Some(i + 2);
                        break;
                    }
                }
                end?
            } else {
                return None;
            };
            result.push(Atom {
                text: &tail[..end],
                width: 0,
            });
            offset += end;
        } else {
            let end = tail.find('\x1b').unwrap_or(tail.len());
            for text in tail[..end].graphemes(true) {
                if text.chars().any(char::is_control) {
                    return None;
                }
                result.push(Atom {
                    text,
                    width: UnicodeWidthStr::width(text),
                });
            }
            offset += end;
        }
    }
    Some(result)
}
fn context(atoms: &[Atom<'_>]) -> String {
    let mut sgr = String::new();
    let mut link = String::new();
    for atom in atoms.iter().filter(|atom| atom.width == 0) {
        if atom.text.starts_with("\x1b[") {
            if matches!(atom.text, "\x1b[0m" | "\x1b[m") {
                sgr.clear();
            }
            sgr.push_str(atom.text);
        } else if atom.text.starts_with("\x1b]8;") {
            link = atom.text.into();
        }
    }
    format!("{sgr}{link}")
}

/// Track the effective SGR state across wrapped rows without replaying the
/// entire history of style changes. Replaying history is both unnecessary and
/// quadratic for text with many foreground spans that use partial resets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SgrSlot {
    Intensity,
    Italic,
    Underline,
    Blink,
    Inverse,
    Conceal,
    Strike,
    Foreground,
    Background,
    Other(u16),
}

fn set_sgr_slot(slots: &mut Vec<(SgrSlot, String)>, slot: SgrSlot, sequence: String) {
    if let Some((_, current)) = slots.iter_mut().find(|(existing, _)| *existing == slot) {
        *current = sequence;
    } else {
        slots.push((slot, sequence));
    }
}

fn remove_sgr_slot(slots: &mut Vec<(SgrSlot, String)>, slot: SgrSlot) {
    slots.retain(|(existing, _)| *existing != slot);
}

fn update_sgr_context(slots: &mut Vec<(SgrSlot, String)>, sequence: &str) {
    let Some(params) = sequence
        .strip_prefix("\x1b[")
        .and_then(|value| value.strip_suffix('m'))
    else {
        return;
    };
    if params.is_empty() {
        slots.clear();
        return;
    }

    let values: Vec<&str> = params.split(';').collect();
    let mut index = 0;
    while index < values.len() {
        let Ok(code) = values[index].parse::<u16>() else {
            index += 1;
            continue;
        };
        let simple = || format!("\x1b[{code}m");
        match code {
            0 => slots.clear(),
            1 | 2 => set_sgr_slot(slots, SgrSlot::Intensity, simple()),
            3 => set_sgr_slot(slots, SgrSlot::Italic, simple()),
            4 => set_sgr_slot(slots, SgrSlot::Underline, simple()),
            5 | 6 => set_sgr_slot(slots, SgrSlot::Blink, simple()),
            7 => set_sgr_slot(slots, SgrSlot::Inverse, simple()),
            8 => set_sgr_slot(slots, SgrSlot::Conceal, simple()),
            9 => set_sgr_slot(slots, SgrSlot::Strike, simple()),
            22 => remove_sgr_slot(slots, SgrSlot::Intensity),
            23 => remove_sgr_slot(slots, SgrSlot::Italic),
            24 => remove_sgr_slot(slots, SgrSlot::Underline),
            25 => remove_sgr_slot(slots, SgrSlot::Blink),
            27 => remove_sgr_slot(slots, SgrSlot::Inverse),
            28 => remove_sgr_slot(slots, SgrSlot::Conceal),
            29 => remove_sgr_slot(slots, SgrSlot::Strike),
            30..=37 | 90..=97 => {
                set_sgr_slot(slots, SgrSlot::Foreground, simple());
            }
            40..=47 | 100..=107 => {
                set_sgr_slot(slots, SgrSlot::Background, simple());
            }
            39 => remove_sgr_slot(slots, SgrSlot::Foreground),
            49 => remove_sgr_slot(slots, SgrSlot::Background),
            38 | 48 => {
                let slot = if code == 38 {
                    SgrSlot::Foreground
                } else {
                    SgrSlot::Background
                };
                let component_count = match values.get(index + 1).copied() {
                    Some("5") if values.get(index + 2).is_some() => 3,
                    Some("2") if values.get(index + 4).is_some() => 5,
                    _ => 0,
                };
                if component_count != 0 {
                    let sequence =
                        format!("\x1b[{}m", values[index..index + component_count].join(";"));
                    set_sgr_slot(slots, slot, sequence);
                    index += component_count;
                    continue;
                }
                set_sgr_slot(slots, SgrSlot::Other(code), simple());
            }
            _ => set_sgr_slot(slots, SgrSlot::Other(code), simple()),
        }
        index += 1;
    }
}

fn update_osc8_context(link: &mut Option<String>, sequence: &str) {
    let Some(content) = sequence.strip_prefix("\x1b]8;").and_then(|value| {
        value
            .strip_suffix('\x07')
            .or_else(|| value.strip_suffix("\x1b\\"))
    }) else {
        return;
    };
    let Some((_, uri)) = content.split_once(';') else {
        return;
    };
    if uri.is_empty() {
        *link = None;
    } else {
        *link = Some(sequence.to_string());
    }
}

fn wrapped_context(atoms: &[Atom<'_>]) -> String {
    let mut slots = Vec::new();
    let mut link = None;
    for atom in atoms.iter().filter(|atom| atom.width == 0) {
        if atom.text.starts_with("\x1b[") {
            update_sgr_context(&mut slots, atom.text);
        } else if atom.text.starts_with("\x1b]8;") {
            update_osc8_context(&mut link, atom.text);
        }
    }

    let mut output = slots
        .into_iter()
        .map(|(_, sequence)| sequence)
        .collect::<String>();
    if let Some(link) = link {
        output.push_str(&link);
    }
    output
}

/// Wrapped rows can be repainted independently. Carry their incoming style
/// explicitly and close it at the row boundary instead of depending on the
/// previous physical row having just been written by the terminal.
pub(crate) fn isolate_wrapped_lines(lines: &mut [String]) {
    let mut carry = String::new();
    for line in lines {
        let source = format!("{carry}{line}");
        let Some(parsed) = atoms(&source) else {
            carry.clear();
            continue;
        };
        carry = wrapped_context(&parsed);
        *line = format!("{source}\x1b[0m\x1b]8;;\x07");
    }
}

/// Expand a nonempty column range to whole graphemes. Selection painting and
/// clipboard extraction must agree when a pointer lands inside a wide cell.
pub(crate) fn snap_column_span(line: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let parsed = atoms(line)?;
    let mut column = 0;
    let mut bounds = None;
    for atom in parsed {
        let next = column + atom.width;
        if atom.width > 0 && column < end && next > start {
            let first = bounds.map_or(column, |(first, _)| first);
            bounds = Some((first, next));
        }
        column = next;
    }
    bounds
}

/// Paint a selection without slicing away unselected text or splitting a
/// grapheme. Original escape sequences stay in place, including OSC8 links.
pub(crate) fn highlight_span(line: &str, start: usize, end: usize) -> Option<String> {
    let parsed = atoms(line)?;
    let mut output = String::new();
    let mut column = 0;
    let mut active = start == 0 && end > 0;
    let mut sgr = Vec::new();
    if active {
        output.push_str("\x1b[7m");
    }
    let restore_inverse = |output: &mut String, sgr: &[(SgrSlot, String)]| {
        output.push_str(if sgr.iter().any(|(slot, _)| *slot == SgrSlot::Inverse) {
            "\x1b[7m"
        } else {
            "\x1b[27m"
        });
    };
    for atom in parsed {
        if atom.width == 0 {
            output.push_str(atom.text);
            if atom.text.starts_with("\x1b[") {
                update_sgr_context(&mut sgr, atom.text);
                if active {
                    output.push_str("\x1b[7m");
                }
            }
            continue;
        }
        let selected = column < end && column + atom.width > start;
        if active && !selected {
            restore_inverse(&mut output, &sgr);
            active = false;
        } else if !active && selected {
            output.push_str("\x1b[7m");
            active = true;
        }
        output.push_str(atom.text);
        column += atom.width;
    }
    if active {
        restore_inverse(&mut output, &sgr);
    }
    Some(output)
}

/// Slice terminal columns without losing SGR/OSC8 context or splitting wide graphemes.
/// Unsupported control protocols retain their existing unsliced fallback path.
pub(crate) fn slice(line: &str, start: usize, width: usize) -> Option<String> {
    let atoms = atoms(line)?;
    let end = start.saturating_add(width);
    let mut column = 0usize;
    let mut started = false;
    let mut output = String::new();
    for (index, atom) in atoms.iter().enumerate() {
        if atom.width == 0 {
            if started {
                output.push_str(atom.text);
            }
            continue;
        }
        let next = column + atom.width;
        if column >= end {
            break;
        }
        if next > start {
            if !started {
                output.push_str(&context(&atoms[..index]));
                started = true;
            }
            if column >= start && next <= end {
                output.push_str(atom.text);
            } else {
                output.push_str(&" ".repeat(next.min(end).saturating_sub(column.max(start))));
            }
        }
        column = next;
    }
    Some(output)
}

/// Return the first changed column and its replacement. Unsupported terminal
/// protocols fall back to the normal row writer rather than slicing controls.
pub(crate) fn patch(old: &str, new: &str) -> Option<(usize, String)> {
    let old = atoms(old)?;
    let new = atoms(new)?;
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let column = old[..prefix].iter().map(|a| a.width).sum();
    let old_width: usize = old.iter().map(|a| a.width).sum();
    let new_width: usize = new.iter().map(|a| a.width).sum();
    let mut suffix = 0;
    if old_width == new_width {
        suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        while suffix > 0
            && context(&old[..old.len() - suffix]) != context(&new[..new.len() - suffix])
        {
            if new[new.len() - suffix].width == 0 {
                suffix -= 1;
            } else {
                suffix = 0;
            }
        }
    }
    let mut text = context(&new[..prefix]);
    for atom in &new[prefix..new.len() - suffix] {
        text.push_str(atom.text);
    }
    text.push_str(&" ".repeat(old_width.saturating_sub(new_width)));
    text.push_str("\x1b[0m\x1b]8;;\x07");
    Some((column, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_preserves_styles_links_and_existing_inverse() {
        assert_eq!(
            highlight_span("a\x1b[31mb\x1b[0mcdef", 1, 3).unwrap(),
            "a\x1b[31m\x1b[7mb\x1b[0m\x1b[7mc\x1b[27mdef"
        );
        assert_eq!(
            highlight_span("\x1b[7mabcdef\x1b[27m", 1, 3).unwrap(),
            "\x1b[7ma\x1b[7mbc\x1b[7mdef\x1b[27m"
        );
        let linked = "a\x1b]8;;https://example.com\x07bc\x1b]8;;\x07def";
        assert_eq!(
            highlight_span(linked, 1, 3).unwrap(),
            "a\x1b]8;;https://example.com\x07\x1b[7mbc\x1b]8;;\x07\x1b[27mdef"
        );
        assert_eq!(snap_column_span("a中👩‍💻z", 2, 4), Some((1, 5)));
        assert!(highlight_span("\x1b[2Junsafe", 0, 2).is_none());
    }

    #[test]
    fn selection_restores_inverse_after_truecolor_sequences() {
        for color in ["38", "48"] {
            let line = format!("\x1b[7mab\x1b[{color};2;255;0;0mcd");
            let expected = format!("\x1b[7ma\x1b[7mb\x1b[{color};2;255;0;0m\x1b[7mc\x1b[7md");
            assert_eq!(highlight_span(&line, 1, 3).unwrap(), expected);
            let line = format!("a\x1b[{color};2;255;0;0;7mbcd");
            let expected = format!("a\x1b[{color};2;255;0;0;7m\x1b[7mbc\x1b[7md");
            assert_eq!(highlight_span(&line, 1, 3).unwrap(), expected);
        }
    }

    #[test]
    fn wrapped_style_context_keeps_only_active_sgr_state() {
        let mut lines: Vec<String> = (0..20).map(|_| "\x1b[31mA\x1b[39m".to_string()).collect();
        isolate_wrapped_lines(&mut lines);

        let lengths: Vec<usize> = lines.iter().map(String::len).collect();
        assert!(lengths.iter().all(|length| *length < 64));
        assert!(lengths.windows(2).all(|pair| pair[0] == pair[1]));

        let mut active_style = vec!["\x1b[31mred".to_string(), "tail".to_string()];
        isolate_wrapped_lines(&mut active_style);
        assert!(active_style[1].starts_with("\x1b[31mtail"));
    }

    #[test]
    fn scrolling_chat_does_not_write_unchanged_sidebar() {
        let (column, text) = patch(
            "chat old │ \x1b[2mRUN STATS\x1b[22m",
            "chat new │ \x1b[2mRUN STATS\x1b[22m",
        )
        .unwrap();
        assert_eq!(column, 5);
        assert!(text.starts_with("new"));
        assert!(!text.contains("RUN STATS"));
        let (_, text) = patch(
            "\x1b[31mold\x1b[0m │ \x1b[2mRUN STATS\x1b[22m",
            "\x1b[32mnew\x1b[0m │ \x1b[2mRUN STATS\x1b[22m",
        )
        .unwrap();
        assert!(!text.contains("RUN STATS"));
        assert!(text.contains("\x1b[32mnew"));
        let (column, text) = patch("chat same │ TPS 123", "chat same │ TPS 456").unwrap();
        assert_eq!(column, 16);
        assert!(!text.contains("chat same"));
    }
    #[test]
    fn graphemes_styles_and_shrinking_lines_are_preserved() {
        let (column, text) = patch("中文 👩‍💻 old", "中文 👩‍💻 new").unwrap();
        assert_eq!(column, 8);
        assert!(text.starts_with("new"));
        let (_, text) = patch("\x1b[31mred\x1b[0m", "\x1b[32mred\x1b[0m").unwrap();
        assert!(text.contains("\x1b[32mred"));
        let (column, text) = patch("long text", "long").unwrap();
        assert_eq!(column, 4);
        assert!(text.starts_with("     "));
        assert!(patch("plain", "\x1b[2Junsafe").is_none());
    }
}
