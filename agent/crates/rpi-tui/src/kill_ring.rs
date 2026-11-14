//! Kill ring for Emacs-style kill and yank operations.
//!
//! The kill ring stores recently deleted text, allowing users to "yank" (paste)
//! previously killed text. This supports the Emacs convention where consecutive
//! kill operations accumulate in the same entry, and yank-pop cycles through
//! previous entries.

use std::collections::VecDeque;

/// Maximum number of entries in the kill ring.
const MAX_KILL_RING_SIZE: usize = 64;

/// Options for pushing to the kill ring.
#[derive(Debug, Clone, Default)]
pub struct PushOptions {
    /// Whether to prepend to the current entry (for backward kills).
    pub prepend: bool,
    /// Whether to accumulate with the previous entry (for consecutive kills).
    pub accumulate: bool,
}

/// Kill ring for storing killed (deleted) text.
#[derive(Debug, Clone)]
pub struct KillRing {
    /// The ring of killed text entries.
    entries: VecDeque<String>,
    /// Maximum size of the ring.
    max_size: usize,
}

impl KillRing {
    /// Create a new kill ring.
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_KILL_RING_SIZE),
            max_size: MAX_KILL_RING_SIZE,
        }
    }

    /// Create a kill ring with a custom maximum size.
    pub fn with_max_size(max_size: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_size),
            max_size,
        }
    }

    /// Push text to the kill ring.
    pub fn push(&mut self, text: &str, options: PushOptions) {
        if text.is_empty() {
            return;
        }

        if options.accumulate && !self.entries.is_empty() {
            // Accumulate with the most recent entry
            if options.prepend {
                let front = self.entries.front_mut().unwrap();
                *front = format!("{}{}", text, front);
            } else {
                let front = self.entries.front_mut().unwrap();
                *front = format!("{}{}", front, text);
            }
        } else {
            // Add as a new entry
            // Check if the text is already at the front (don't duplicate)
            if self.entries.front() != Some(&text.to_string()) {
                self.entries.push_front(text.to_string());

                // Trim if over capacity
                while self.entries.len() > self.max_size {
                    self.entries.pop_back();
                }
            }
        }
    }

    /// Peek at the most recent entry without removing it.
    pub fn peek(&self) -> Option<&str> {
        self.entries.front().map(|s| s.as_str())
    }

    /// Get the number of entries in the kill ring.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the kill ring is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Rotate the kill ring forward (move front to back).
    /// Used for yank-pop to cycle through entries.
    pub fn rotate(&mut self) {
        if self.entries.len() > 1 {
            if let Some(front) = self.entries.pop_front() {
                self.entries.push_back(front);
            }
        }
    }

    /// Rotate the kill ring backward (move back to front).
    pub fn rotate_back(&mut self) {
        if self.entries.len() > 1 {
            if let Some(back) = self.entries.pop_back() {
                self.entries.push_front(back);
            }
        }
    }

    /// Clear all entries from the kill ring.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Get all entries (for debugging/testing).
    pub fn entries(&self) -> impl Iterator<Item = &String> {
        self.entries.iter()
    }

    /// Get entry at index (0 is most recent).
    pub fn get(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(|s| s.as_str())
    }
}

impl Default for KillRing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_and_peek() {
        let mut ring = KillRing::new();
        ring.push("hello", PushOptions::default());
        assert_eq!(ring.peek(), Some("hello"));
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn test_accumulate() {
        let mut ring = KillRing::new();
        ring.push("hello", PushOptions::default());
        ring.push(
            " world",
            PushOptions {
                prepend: false,
                accumulate: true,
            },
        );
        assert_eq!(ring.peek(), Some("hello world"));
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn test_accumulate_prepend() {
        let mut ring = KillRing::new();
        ring.push("world", PushOptions::default());
        ring.push(
            "hello ",
            PushOptions {
                prepend: true,
                accumulate: true,
            },
        );
        assert_eq!(ring.peek(), Some("hello world"));
    }

    #[test]
    fn test_rotate() {
        let mut ring = KillRing::new();
        ring.push("first", PushOptions::default());
        ring.push("second", PushOptions::default());
        ring.push("third", PushOptions::default());

        assert_eq!(ring.peek(), Some("third"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("second"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("first"));
    }

    #[test]
    fn test_no_duplicate() {
        let mut ring = KillRing::new();
        ring.push("hello", PushOptions::default());
        ring.push("hello", PushOptions::default());
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn test_max_size() {
        let mut ring = KillRing::with_max_size(3);
        ring.push("1", PushOptions::default());
        ring.push("2", PushOptions::default());
        ring.push("3", PushOptions::default());
        ring.push("4", PushOptions::default());

        assert_eq!(ring.len(), 3);
        assert_eq!(ring.peek(), Some("4"));
        assert_eq!(ring.get(2), Some("2")); // "1" should be evicted
    }

    #[test]
    fn test_empty_text() {
        let mut ring = KillRing::new();
        ring.push("", PushOptions::default());
        assert!(ring.is_empty());
    }
}
