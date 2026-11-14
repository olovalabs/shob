//! Spacer component for adding empty space.

use std::any::Any;

use super::component::Component;

/// Spacer component that adds empty vertical space.
pub struct Spacer {
    height: usize,
}

impl Spacer {
    /// Create a new spacer with the given height in rows.
    pub fn new(height: usize) -> Self {
        Self { height }
    }
}

impl Component for Spacer {
    fn render(&self, _width: usize) -> Vec<String> {
        (0..self.height).map(|_| String::new()).collect()
    }

    fn invalidate(&self) {
        // Spacer has no cached state
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
