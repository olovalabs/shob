//! Probes from the #188 review, reproduced from the reviewer's comment.
//!
//! The label column is now shared across rows, so the budget that
//! `label_column_width()` computes has to cover *every* row format the
//! component emits — including the selected row, which used to append a
//! trailing space after the value and therefore overflowed a row that was
//! already padded to the requested width.
//!
//! These are offline render probes over the component's own `render()` output:
//! they pin the width invariant and the alignment of the value column, not what
//! a real terminal does with an over-wide line.

use rpi_tui::component::Component;
use rpi_tui::settings_list::{SettingItem, SettingsList};
use rpi_tui::utils::visible_width;

/// Column where the fixture value starts on a rendered row.
fn value_column(line: &str) -> usize {
    visible_width(&line[..line.find("VALUE").expect("fixture value")])
}

/// The two rows the reviewer reproduced the overflow with.
fn realistic() -> SettingsList {
    SettingsList::new(vec![
        SettingItem::new("model", "Default model", "claude-sonnet-4"),
        SettingItem::new("copy", "Fullscreen copy on select", "true"),
    ])
}

#[test]
fn review188_selected_row_fits_requested_width() {
    let list = realistic();
    let mut overflows = vec![];
    for width in [32, 37, 40, 43, 44, 80] {
        let lines = list.render(width);
        assert!(lines[0].contains("claude-sonnet-4"));
        let actual = visible_width(&lines[0]);
        eprintln!(
            "selected model requested={width}, actual={actual}, row={:?}",
            lines[0]
        );
        if actual > width {
            overflows.push((width, actual));
        }
    }
    // Same long model value while the other row is selected is a fitting control.
    list.set_selected(1);
    let unselected = list.render(40);
    eprintln!(
        "unselected model requested=40 actual={}",
        visible_width(&unselected[0])
    );
    assert!(visible_width(&unselected[0]) <= 40);
    assert!(
        overflows.is_empty(),
        "selected rows exceed the new shared-column budget: {overflows:?}"
    );
}

#[test]
fn review188_alignment_handles_ascii_cjk_and_ansi_labels() {
    let list = SettingsList::new(vec![
        SettingItem::new("theme", "Theme", "VALUE"),
        SettingItem::new("copy", "Fullscreen copy on select", "VALUE"),
        SettingItem::new("cjk", "界界界界界界界界界界界界界界界界界", "VALUE"),
        SettingItem::new("ansi", "\x1b[31mColored label\x1b[0m", "VALUE"),
        SettingItem::new(
            "long",
            "An extremely long setting label that exceeds the cap",
            "VALUE",
        ),
    ]);
    let lines = list.render(110);
    let columns: Vec<_> = lines.iter().map(|line| value_column(line)).collect();
    eprintln!("ASCII/CJK/ANSI value columns: {columns:?}");
    assert_eq!(columns, vec![35; 5]);
    assert!(lines[1].contains("Fullscreen copy on select"));
    assert!(lines[2].contains('…'));
    assert!(lines[4].contains('…'));
    assert!(lines.iter().all(|line| visible_width(line) <= 110));
}

#[test]
fn review188_scrolling_is_stable_and_filtering_recalculates_columns() {
    let items: Vec<_> = (0..13)
        .map(|i| {
            SettingItem::new(
                &format!("row{i:02}"),
                if i == 12 {
                    "An extremely long setting label that exceeds the cap"
                } else {
                    "Short"
                },
                "VALUE",
            )
        })
        .collect();
    let list = SettingsList::new(items);
    let columns = |list: &SettingsList| {
        list.render(110)
            .iter()
            .filter(|line| line.contains("VALUE"))
            .map(|line| value_column(line))
            .collect::<Vec<_>>()
    };
    assert_eq!(columns(&list), vec![35; 10]);
    list.set_selected(11);
    assert_eq!(columns(&list), vec![35; 10]);
    list.set_filter("row00");
    assert_eq!(columns(&list), vec![23]);
    list.set_filter("");
    assert_eq!(columns(&list), vec![35; 10]);
}

#[test]
fn review188_narrow_short_values_remain_visible_and_aligned() {
    let list = SettingsList::new(vec![
        SettingItem::new("theme", "Theme", "dark"),
        SettingItem::new("copy", "Fullscreen copy on select", "true"),
        SettingItem::new("autocomplete", "Autocomplete max items", "5"),
    ]);
    let lines = list.render(24);
    for (line, value) in lines.iter().zip(["dark", "true", "5"]) {
        assert!(visible_width(line) <= 24);
        let column = visible_width(&line[..line.find(value).unwrap()]);
        assert_eq!(column, 9);
    }
    assert!(lines[1].contains('…'));
    assert!(lines[2].contains('…'));
}
