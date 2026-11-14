//! Undo stack for text editing.
//!
//! Provides a simple undo mechanism that stores snapshots of state.

use std::collections::VecDeque;

/// Maximum number of undo states to keep.
const MAX_UNDO_SIZE: usize = 256;

/// Undo stack for storing state snapshots.
#[derive(Debug, Clone)]
pub struct UndoStack<T> {
    /// Stack of states (most recent at the back).
    states: VecDeque<T>,
    /// Maximum size of the stack.
    max_size: usize,
}

impl<T: Clone> UndoStack<T> {
    /// Create a new undo stack.
    pub fn new() -> Self {
        Self {
            states: VecDeque::with_capacity(MAX_UNDO_SIZE),
            max_size: MAX_UNDO_SIZE,
        }
    }

    /// Create an undo stack with a custom maximum size.
    pub fn with_max_size(max_size: usize) -> Self {
        Self {
            states: VecDeque::with_capacity(max_size),
            max_size,
        }
    }

    /// Push a state onto the undo stack.
    pub fn push(&mut self, state: T) {
        self.states.push_back(state);

        // Trim if over capacity
        while self.states.len() > self.max_size {
            self.states.pop_front();
        }
    }

    /// Pop the most recent state from the undo stack.
    pub fn pop(&mut self) -> Option<T> {
        self.states.pop_back()
    }

    /// Peek at the most recent state without removing it.
    pub fn peek(&self) -> Option<&T> {
        self.states.back()
    }

    /// Get the number of states in the undo stack.
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// Check if the undo stack is empty.
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Clear all states from the undo stack.
    pub fn clear(&mut self) {
        self.states.clear();
    }

    /// Get all states (for debugging/testing).
    pub fn states(&self) -> impl Iterator<Item = &T> {
        self.states.iter()
    }

    /// Get state at index (0 is oldest, len-1 is most recent).
    pub fn get(&self, index: usize) -> Option<&T> {
        self.states.get(index)
    }

    /// Check if we can undo.
    pub fn can_undo(&self) -> bool {
        !self.states.is_empty()
    }

    /// Undo to a specific index, returning all popped states.
    pub fn undo_to(&mut self, target_len: usize) -> Vec<T> {
        let mut popped = Vec::new();
        while self.states.len() > target_len {
            if let Some(state) = self.states.pop_back() {
                popped.push(state);
            }
        }
        popped
    }
}

impl<T: Clone> Default for UndoStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Redo stack for storing undone states.
#[derive(Debug, Clone)]
pub struct RedoStack<T> {
    /// Stack of undone states.
    states: VecDeque<T>,
    max_size: usize,
}

impl<T: Clone> RedoStack<T> {
    /// Create a new redo stack.
    pub fn new() -> Self {
        Self {
            states: VecDeque::with_capacity(MAX_UNDO_SIZE),
            max_size: MAX_UNDO_SIZE,
        }
    }

    /// Push a state onto the redo stack.
    pub fn push(&mut self, state: T) {
        self.states.push_back(state);

        while self.states.len() > self.max_size {
            self.states.pop_front();
        }
    }

    /// Pop the most recent state from the redo stack.
    pub fn pop(&mut self) -> Option<T> {
        self.states.pop_back()
    }

    /// Get the number of states in the redo stack.
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// Check if the redo stack is empty.
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Clear all states from the redo stack.
    pub fn clear(&mut self) {
        self.states.clear();
    }

    /// Check if we can redo.
    pub fn can_redo(&self) -> bool {
        !self.states.is_empty()
    }
}

impl<T: Clone> Default for RedoStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Combined undo/redo manager.
#[derive(Debug, Clone)]
pub struct UndoRedoManager<T> {
    undo_stack: UndoStack<T>,
    redo_stack: RedoStack<T>,
}

impl<T: Clone> UndoRedoManager<T> {
    /// Create a new undo/redo manager.
    pub fn new() -> Self {
        Self {
            undo_stack: UndoStack::new(),
            redo_stack: RedoStack::new(),
        }
    }

    /// Push a new state (clears redo stack).
    pub fn push(&mut self, state: T) {
        self.undo_stack.push(state);
        self.redo_stack.clear();
    }

    /// Undo: move current state to redo stack and return previous state.
    pub fn undo(&mut self, current: T) -> Option<T> {
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push(current);
            Some(prev)
        } else {
            None
        }
    }

    /// Redo: move current state to undo stack and return next state.
    pub fn redo(&mut self, current: T) -> Option<T> {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(current);
            Some(next)
        } else {
            None
        }
    }

    /// Check if undo is available.
    pub fn can_undo(&self) -> bool {
        self.undo_stack.can_undo()
    }

    /// Check if redo is available.
    pub fn can_redo(&self) -> bool {
        self.redo_stack.can_redo()
    }

    /// Clear all undo/redo history.
    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
    }

    /// Get the number of undo states.
    pub fn undo_count(&self) -> usize {
        self.undo_stack.len()
    }

    /// Get the number of redo states.
    pub fn redo_count(&self) -> usize {
        self.redo_stack.len()
    }
}

impl<T: Clone> Default for UndoRedoManager<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_and_pop() {
        let mut stack: UndoStack<i32> = UndoStack::new();
        stack.push(1);
        stack.push(2);
        stack.push(3);

        assert_eq!(stack.len(), 3);
        assert_eq!(stack.pop(), Some(3));
        assert_eq!(stack.pop(), Some(2));
        assert_eq!(stack.pop(), Some(1));
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn test_max_size() {
        let mut stack: UndoStack<i32> = UndoStack::with_max_size(2);
        stack.push(1);
        stack.push(2);
        stack.push(3);

        assert_eq!(stack.len(), 2);
        assert_eq!(stack.pop(), Some(3));
        assert_eq!(stack.pop(), Some(2));
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn test_undo_redo_manager() {
        let mut manager = UndoRedoManager::new();

        manager.push("state1".to_string());
        manager.push("state2".to_string());
        manager.push("state3".to_string());

        assert!(manager.can_undo());
        assert!(!manager.can_redo());

        // Undo
        let prev = manager.undo("current".to_string());
        assert_eq!(prev, Some("state3".to_string()));
        assert!(manager.can_redo());

        // Redo
        let next = manager.redo("state3".to_string());
        assert_eq!(next, Some("current".to_string()));
    }

    #[test]
    fn test_push_clears_redo() {
        let mut manager = UndoRedoManager::new();

        manager.push("state1".to_string());
        manager.push("state2".to_string());

        let _ = manager.undo("current".to_string());
        assert!(manager.can_redo());

        // New push should clear redo
        manager.push("new_state".to_string());
        assert!(!manager.can_redo());
    }
}
