//! Word navigation utilities for text editing.
//!
//! Provides functions for finding word boundaries in text.

/// Characters that are considered word boundaries.
const WORD_BOUNDARY_CHARS: &[char] = &[
    ' ', '\t', '\n', '\r', '-', '_', '.', '/', ':', '\\', '(', ')', '[', ']', '{', '}', '<', '>',
    '=', '+', '*', '&', '|', '!', '@', '#', '$', '%', '^', '~', '`', ',', ';', '"', '\'',
];

/// Check if a character is a word boundary.
pub fn is_word_boundary(c: char) -> bool {
    WORD_BOUNDARY_CHARS.contains(&c)
}

/// Check if a character is a word character.
pub fn is_word_char(c: char) -> bool {
    !is_word_boundary(c)
}

/// Find the start of the previous word from the given position.
/// Returns the index of the start of the previous word.
pub fn find_word_backward(text: &str, position: usize) -> usize {
    if position == 0 {
        return 0;
    }

    let chars: Vec<char> = text.chars().collect();
    let mut index = position.saturating_sub(1);

    // Skip any trailing whitespace or boundary characters
    while index > 0 && (chars[index].is_whitespace() || is_word_boundary(chars[index])) {
        index -= 1;
    }

    // Now find the start of this word
    while index > 0 && is_word_char(chars[index.saturating_sub(1)]) {
        index -= 1;
    }

    index
}

/// Find the start of the next word from the given position.
/// Returns the index of the start of the next word.
pub fn find_word_forward(text: &str, position: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut index = position;

    // Skip current word characters
    while index < chars.len() && is_word_char(chars[index]) {
        index += 1;
    }

    // Skip any whitespace or boundary characters
    while index < chars.len() && (chars[index].is_whitespace() || is_word_boundary(chars[index])) {
        index += 1;
    }

    index
}

/// Find the end of the current word from the given position.
/// Returns the index just after the last character of the word.
pub fn find_word_end(text: &str, position: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut index = position;

    // Skip any whitespace or boundary characters
    while index < chars.len() && (chars[index].is_whitespace() || is_word_boundary(chars[index])) {
        index += 1;
    }

    // Find the end of this word
    while index < chars.len() && is_word_char(chars[index]) {
        index += 1;
    }

    index
}

/// Find the end of the previous word.
/// Returns the index just after the last character of the previous word.
pub fn find_prev_word_end(text: &str, position: usize) -> usize {
    if position == 0 {
        return 0;
    }

    let chars: Vec<char> = text.chars().collect();
    let mut index = position.saturating_sub(1);

    // Skip any whitespace or boundary characters going backward
    while index > 0 && (chars[index].is_whitespace() || is_word_boundary(chars[index])) {
        index -= 1;
    }

    // Now we're at the end of a word, find its start
    let end = index + 1;
    while index > 0 && is_word_char(chars[index.saturating_sub(1)]) {
        index -= 1;
    }

    end
}

/// Find the start of the current line (for multi-line text).
pub fn find_line_start(text: &str, position: usize) -> usize {
    if position == 0 {
        return 0;
    }

    // Work with byte positions, converting as needed
    let byte_pos = text
        .char_indices()
        .nth(position)
        .map(|(i, _)| i)
        .unwrap_or(text.len());

    // Find the previous newline
    text[..byte_pos]
        .rfind('\n')
        .map(|i| {
            // Convert byte position to char position
            text[..i].chars().count() + 1
        })
        .unwrap_or(0)
}

/// Find the end of the current line (for multi-line text).
/// Returns the position of the newline or end of text.
pub fn find_line_end(text: &str, position: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut index = position;

    while index < chars.len() && chars[index] != '\n' {
        index += 1;
    }

    index
}

/// Get the current word at the given position.
/// Returns (start, end) indices of the word.
pub fn get_current_word_range(text: &str, position: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();

    if position >= chars.len() {
        return (position, position);
    }

    // If we're on a boundary character, return just that character
    if is_word_boundary(chars[position]) {
        return (position, position + 1);
    }

    // Find word start
    let mut start = position;
    while start > 0 && is_word_char(chars[start.saturating_sub(1)]) {
        start -= 1;
    }

    // Find word end
    let mut end = position;
    while end < chars.len() && is_word_char(chars[end]) {
        end += 1;
    }

    (start, end)
}

/// Check if we're at the start of a word.
pub fn is_at_word_start(text: &str, position: usize) -> bool {
    if position == 0 {
        return true;
    }

    let chars: Vec<char> = text.chars().collect();
    if position >= chars.len() {
        return false;
    }

    // Either previous char is a boundary or we're at position 0
    is_word_char(chars[position]) && is_word_boundary(chars[position - 1])
}

/// Check if we're at the end of a word.
pub fn is_at_word_end(text: &str, position: usize) -> bool {
    if position == 0 {
        return false;
    }

    let chars: Vec<char> = text.chars().collect();
    if position > chars.len() {
        return false;
    }

    // Current position is after a word char and next is a boundary or end
    let prev_is_word = is_word_char(chars[position - 1]);
    let next_is_boundary = position >= chars.len() || is_word_boundary(chars[position]);

    prev_is_word && next_is_boundary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_word_backward() {
        let text = "hello world";
        assert_eq!(find_word_backward(text, 11), 6); // Before "world"
        assert_eq!(find_word_backward(text, 6), 0); // Before "hello"
        assert_eq!(find_word_backward(text, 0), 0); // At start
    }

    #[test]
    fn test_find_word_forward() {
        let text = "hello world";
        assert_eq!(find_word_forward(text, 0), 6); // After "hello"
        assert_eq!(find_word_forward(text, 6), 11); // After "world" (skip to end)
        assert_eq!(find_word_forward(text, 11), 11); // At end
    }

    #[test]
    fn test_find_word_end() {
        let text = "hello world";
        assert_eq!(find_word_end(text, 0), 5); // End of "hello"
        assert_eq!(find_word_end(text, 6), 11); // End of "world"
    }

    #[test]
    fn test_word_boundaries() {
        assert!(is_word_boundary(' '));
        assert!(is_word_boundary('.'));
        assert!(is_word_boundary('/'));
        assert!(!is_word_boundary('a'));
        assert!(!is_word_boundary('1'));
    }

    #[test]
    fn test_get_current_word_range() {
        let text = "hello world";
        assert_eq!(get_current_word_range(text, 0), (0, 5));
        assert_eq!(get_current_word_range(text, 2), (0, 5));
        assert_eq!(get_current_word_range(text, 6), (6, 11));
        assert_eq!(get_current_word_range(text, 8), (6, 11));
    }

    #[test]
    fn test_special_characters() {
        let text = "hello.world";
        assert_eq!(find_word_forward(text, 0), 6); // After "hello", stop at '.' skip to "world"
        assert_eq!(find_word_forward(text, 5), 6); // Skip '.', start at "world"
    }

    #[test]
    fn test_find_line_start() {
        let text = "hello\nworld";
        assert_eq!(find_line_start(text, 0), 0);
        assert_eq!(find_line_start(text, 6), 6); // Start of "world"
        assert_eq!(find_line_start(text, 8), 6); // Still start of "world"
    }

    #[test]
    fn test_find_line_end() {
        let text = "hello\nworld";
        assert_eq!(find_line_end(text, 0), 5); // End of "hello" (before \n)
        assert_eq!(find_line_end(text, 6), 11); // End of "world"
    }
}
