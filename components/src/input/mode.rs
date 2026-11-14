use std::rc::Rc;
use std::{cell::RefCell, ops::Range};

use gpui::{App, SharedString};
use ropey::Rope;
use tree_sitter::InputEdit;

use super::text_wrapper::TextWrapper;
use crate::highlighter::DiagnosticSet;
use crate::highlighter::SyntaxHighlighter;
use crate::input::{RopeExt as _, TabSize};

#[derive(Clone)]
pub(crate) enum InputMode {
    PlainText {
        multi_line: bool,
        tab: TabSize,
        rows: usize,
    },

    AutoGrow {
        rows: usize,
        min_rows: usize,
        max_rows: usize,
        language: Option<SharedString>,
        highlighter: Rc<RefCell<Option<SyntaxHighlighter>>>,
    },

    CodeEditor {
        multi_line: bool,
        tab: TabSize,
        rows: usize,

        line_number: bool,
        language: SharedString,
        indent_guides: bool,
        highlighter: Rc<RefCell<Option<SyntaxHighlighter>>>,
        diagnostics: DiagnosticSet,
    },
}

impl Default for InputMode {
    fn default() -> Self {
        InputMode::plain_text()
    }
}

#[allow(unused)]
impl InputMode {
    pub(super) fn plain_text() -> Self {
        InputMode::PlainText {
            multi_line: false,
            tab: TabSize::default(),
            rows: 1,
        }
    }

    pub(super) fn code_editor(language: impl Into<SharedString>) -> Self {
        InputMode::CodeEditor {
            rows: 2,
            multi_line: true,
            tab: TabSize::default(),
            language: language.into(),
            highlighter: Rc::new(RefCell::new(None)),
            line_number: true,
            indent_guides: true,
            diagnostics: DiagnosticSet::new(&Rope::new()),
        }
    }

    pub(super) fn auto_grow(min_rows: usize, max_rows: usize) -> Self {
        InputMode::AutoGrow {
            rows: min_rows,
            min_rows,
            max_rows,
            language: None,
            highlighter: Rc::new(RefCell::new(None)),
        }
    }

    pub(super) fn multi_line(mut self, multi_line: bool) -> Self {
        match &mut self {
            InputMode::PlainText { multi_line: ml, .. } => *ml = multi_line,
            InputMode::CodeEditor { multi_line: ml, .. } => *ml = multi_line,
            InputMode::AutoGrow { .. } => {}
        }
        self
    }

    #[inline]
    pub(super) fn is_single_line(&self) -> bool {
        !self.is_multi_line()
    }

    #[inline]
    pub(super) fn is_code_editor(&self) -> bool {
        matches!(self, InputMode::CodeEditor { .. })
    }

    #[inline]
    pub(super) fn is_auto_grow(&self) -> bool {
        matches!(self, InputMode::AutoGrow { .. })
    }

    #[inline]
    pub(super) fn is_multi_line(&self) -> bool {
        match self {
            InputMode::PlainText { multi_line, .. } => *multi_line,
            InputMode::CodeEditor { multi_line, .. } => *multi_line,
            InputMode::AutoGrow { max_rows, .. } => *max_rows > 1,
        }
    }

    pub(super) fn set_rows(&mut self, new_rows: usize) {
        match self {
            InputMode::PlainText { rows, .. } => {
                *rows = new_rows;
            }
            InputMode::CodeEditor { rows, .. } => {
                *rows = new_rows;
            }
            InputMode::AutoGrow {
                rows,
                min_rows,
                max_rows,
                ..
            } => {
                *rows = new_rows.clamp(*min_rows, *max_rows);
            }
        }
    }

    pub(super) fn update_auto_grow(&mut self, text_wrapper: &TextWrapper, text: &Rope) {
        if self.is_single_line() {
            return;
        }

        let line_count = (text.chars().filter(|c| *c == '\n').count() + 1).max(text_wrapper.len());
        self.set_rows(line_count);
    }

    pub(super) fn rows(&self) -> usize {
        if !self.is_multi_line() {
            return 1;
        }

        match self {
            InputMode::PlainText { rows, .. } => *rows,
            InputMode::CodeEditor { rows, .. } => *rows,
            InputMode::AutoGrow { rows, .. } => *rows,
        }
        .max(1)
    }

    #[allow(unused)]
    pub(super) fn min_rows(&self) -> usize {
        match self {
            InputMode::AutoGrow { min_rows, .. } => *min_rows,
            _ => 1,
        }
        .max(1)
    }

    #[allow(unused)]
    pub(super) fn max_rows(&self) -> usize {
        if !self.is_multi_line() {
            return 1;
        }

        match self {
            InputMode::AutoGrow { max_rows, .. } => *max_rows,
            _ => usize::MAX,
        }
    }

    #[allow(unused)]
    #[inline]
    pub(super) fn line_number(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                line_number,
                multi_line,
                ..
            } => *line_number && *multi_line,
            _ => false,
        }
    }

    pub(super) fn update_highlighter(
        &mut self,
        selected_range: &Range<usize>,
        text: &Rope,
        new_text: &str,
        force: bool,
        cx: &mut App,
    ) {
        let (language, highlighter) = match &self {
            InputMode::CodeEditor {
                language,
                highlighter,
                ..
            } => (language, highlighter),
            InputMode::AutoGrow {
                language: Some(language),
                highlighter,
                ..
            } => (language, highlighter),
            _ => return,
        };

        if !force && highlighter.borrow().is_some() {
            return;
        }

        let mut highlighter = highlighter.borrow_mut();
        if highlighter.is_none() {
            let new_highlighter = SyntaxHighlighter::new(language);
            highlighter.replace(new_highlighter);
        }

        let Some(highlighter) = highlighter.as_mut() else {
            return;
        };

        let mut selected_range = selected_range.clone();
        selected_range.end = selected_range.end.min(text.len());

        let changed_len = new_text.len() as isize - selected_range.len() as isize;
        let new_end = (selected_range.end as isize + changed_len) as usize;

        let start_pos = text.offset_to_point(selected_range.start);
        let old_end_pos = text.offset_to_point(selected_range.end);
        let new_end_pos = text.offset_to_point(new_end);

        let edit = InputEdit {
            start_byte: selected_range.start,
            old_end_byte: selected_range.end,
            new_end_byte: new_end,
            start_position: start_pos,
            old_end_position: old_end_pos,
            new_end_position: new_end_pos,
        };

        highlighter.update(Some(edit), text);
    }

    #[allow(unused)]
    pub(super) fn diagnostics(&self) -> Option<&DiagnosticSet> {
        match self {
            InputMode::CodeEditor { diagnostics, .. } => Some(diagnostics),
            _ => None,
        }
    }

    pub(super) fn diagnostics_mut(&mut self) -> Option<&mut DiagnosticSet> {
        match self {
            InputMode::CodeEditor { diagnostics, .. } => Some(diagnostics),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ropey::Rope;

    use crate::{
        highlighter::DiagnosticSet,
        input::{TabSize, mode::InputMode},
    };

    #[test]
    fn test_code_editor() {
        let mode = InputMode::code_editor("rust");
        assert_eq!(mode.is_code_editor(), true);
        assert_eq!(mode.is_multi_line(), true);
        assert_eq!(mode.is_single_line(), false);
        assert_eq!(mode.line_number(), true);
        assert_eq!(mode.has_indent_guides(), true);
        assert_eq!(mode.max_rows(), usize::MAX);
        assert_eq!(mode.min_rows(), 1);

        let mode = InputMode::CodeEditor {
            multi_line: false,
            line_number: true,
            indent_guides: true,
            rows: 0,
            tab: Default::default(),
            language: "rust".into(),
            highlighter: Default::default(),
            diagnostics: DiagnosticSet::new(&Rope::new()),
        };
        assert_eq!(mode.is_code_editor(), true);
        assert_eq!(mode.is_multi_line(), false);
        assert_eq!(mode.is_single_line(), true);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.has_indent_guides(), false);
        assert_eq!(mode.max_rows(), 1);
        assert_eq!(mode.min_rows(), 1);
    }

    #[test]
    fn test_plain() {
        let mode = InputMode::PlainText {
            multi_line: true,
            tab: TabSize::default(),
            rows: 5,
        };
        assert_eq!(mode.is_code_editor(), false);
        assert_eq!(mode.is_multi_line(), true);
        assert_eq!(mode.is_single_line(), false);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.rows(), 5);
        assert_eq!(mode.max_rows(), usize::MAX);
        assert_eq!(mode.min_rows(), 1);

        let mode = InputMode::plain_text();
        assert_eq!(mode.is_code_editor(), false);
        assert_eq!(mode.is_multi_line(), false);
        assert_eq!(mode.is_single_line(), true);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.max_rows(), 1);
        assert_eq!(mode.min_rows(), 1);
    }

    #[test]
    fn test_auto_grow() {
        let mut mode = InputMode::auto_grow(2, 5);
        assert_eq!(mode.is_code_editor(), false);
        assert_eq!(mode.is_multi_line(), true);
        assert_eq!(mode.is_single_line(), false);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.rows(), 2);
        assert_eq!(mode.max_rows(), 5);
        assert_eq!(mode.min_rows(), 2);

        mode.set_rows(4);
        assert_eq!(mode.rows(), 4);

        mode.set_rows(1);
        assert_eq!(mode.rows(), 2);

        mode.set_rows(10);
        assert_eq!(mode.rows(), 5);
    }
}
