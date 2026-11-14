//! Input buffering for batch splitting.
//!
//! Buffers stdin input to handle bracketed paste mode and other
//! batch input scenarios.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default batch timeout in milliseconds.
const DEFAULT_BATCH_TIMEOUT_MS: u64 = 16; // ~60fps

/// Maximum buffer size.
const MAX_BUFFER_SIZE: usize = 64 * 1024; // 64KB

/// Options for stdin buffer.
#[derive(Debug, Clone)]
pub struct StdinBufferOptions {
    /// Maximum time to wait before flushing a batch.
    pub batch_timeout_ms: u64,
    /// Maximum buffer size before forced flush.
    pub max_buffer_size: usize,
    /// Enable bracketed paste mode handling.
    pub handle_bracketed_paste: bool,
}

impl Default for StdinBufferOptions {
    fn default() -> Self {
        Self {
            batch_timeout_ms: DEFAULT_BATCH_TIMEOUT_MS,
            max_buffer_size: MAX_BUFFER_SIZE,
            handle_bracketed_paste: true,
        }
    }
}

/// Event types emitted by the stdin buffer.
#[derive(Debug, Clone)]
pub enum StdinBufferEvent {
    /// Normal input data.
    Data(String),
    /// Start of bracketed paste.
    PasteStart,
    /// End of bracketed paste with the pasted content.
    PasteEnd(String),
    /// Batch of input data.
    Batch(String),
}

/// Stdin buffer for handling batch input and bracketed paste.
pub struct StdinBuffer {
    options: StdinBufferOptions,
    buffer: Mutex<String>,
    events: Mutex<VecDeque<StdinBufferEvent>>,
    in_paste: Mutex<bool>,
    paste_buffer: Mutex<String>,
    last_input: Mutex<Option<Instant>>,
    callbacks: Mutex<Vec<Arc<dyn Fn(&StdinBufferEvent) + Send + Sync>>>,
}

impl StdinBuffer {
    /// Create a new stdin buffer with default options.
    pub fn new() -> Self {
        Self::with_options(StdinBufferOptions::default())
    }

    /// Create a new stdin buffer with options.
    pub fn with_options(options: StdinBufferOptions) -> Self {
        Self {
            options,
            buffer: Mutex::new(String::new()),
            events: Mutex::new(VecDeque::new()),
            in_paste: Mutex::new(false),
            paste_buffer: Mutex::new(String::new()),
            last_input: Mutex::new(None),
            callbacks: Mutex::new(Vec::new()),
        }
    }

    /// Add a callback for events.
    pub fn on_event(&self, callback: Arc<dyn Fn(&StdinBufferEvent) + Send + Sync>) {
        if let Ok(mut callbacks) = self.callbacks.lock() {
            callbacks.push(callback);
        }
    }

    /// Process input data.
    pub fn process(&self, data: &str) {
        if data.is_empty() {
            return;
        }

        // Update last input time
        if let Ok(mut last) = self.last_input.lock() {
            *last = Some(Instant::now());
        }

        // Handle bracketed paste mode
        if self.options.handle_bracketed_paste {
            self.process_with_bracketed_paste(data);
        } else {
            self.process_simple(data);
        }
    }

    /// Process data with bracketed paste handling.
    fn process_with_bracketed_paste(&self, data: &str) {
        let mut remaining = data;

        while !remaining.is_empty() {
            let in_paste = *self.in_paste.lock().unwrap();

            if in_paste {
                // Look for paste end marker
                if let Some(end_pos) = remaining.find("\x1b[201~") {
                    // Extract paste content
                    let paste_content = &remaining[..end_pos];
                    if let Ok(mut paste_buffer) = self.paste_buffer.lock() {
                        paste_buffer.push_str(paste_content);
                        let complete_paste = paste_buffer.clone();
                        paste_buffer.clear();

                        // Emit paste end event
                        self.emit_event(StdinBufferEvent::PasteEnd(complete_paste));
                    }
                    if let Ok(mut in_paste) = self.in_paste.lock() {
                        *in_paste = false;
                    }
                    remaining = &remaining[end_pos + 6..]; // Skip paste end marker
                } else {
                    // Still in paste, buffer the data
                    if let Ok(mut paste_buffer) = self.paste_buffer.lock() {
                        paste_buffer.push_str(remaining);
                    }
                    break;
                }
            } else {
                // Look for paste start marker
                if let Some(start_pos) = remaining.find("\x1b[200~") {
                    // Process any data before paste start
                    if start_pos > 0 {
                        self.process_simple(&remaining[..start_pos]);
                    }

                    // Enter paste mode
                    if let Ok(mut in_paste) = self.in_paste.lock() {
                        *in_paste = true;
                    }
                    self.emit_event(StdinBufferEvent::PasteStart);

                    remaining = &remaining[start_pos + 6..]; // Skip paste start marker
                } else {
                    // No paste markers, process as normal
                    self.process_simple(remaining);
                    break;
                }
            }
        }
    }

    /// Process data without bracketed paste handling.
    fn process_simple(&self, data: &str) {
        // Check if we should batch
        let should_batch = self.should_batch();

        if should_batch {
            // Add to buffer
            if let Ok(mut buffer) = self.buffer.lock() {
                if buffer.len() + data.len() <= self.options.max_buffer_size {
                    buffer.push_str(data);
                } else {
                    // Buffer full, flush first
                    let flushed = std::mem::take(&mut *buffer);
                    self.emit_event(StdinBufferEvent::Batch(flushed));
                    buffer.push_str(data);
                }
            }
        } else {
            // Flush any existing buffer first
            self.flush();

            // Emit directly
            self.emit_event(StdinBufferEvent::Data(data.to_string()));
        }
    }

    /// Check if we should batch input.
    fn should_batch(&self) -> bool {
        if let Ok(last) = self.last_input.lock() {
            if let Some(last_time) = *last {
                let elapsed = last_time.elapsed();
                // Don't batch if this is the first input (elapsed < 1ms)
                if elapsed < Duration::from_millis(1) {
                    return false;
                }
                return elapsed < Duration::from_millis(self.options.batch_timeout_ms);
            }
        }
        false
    }

    /// Flush the buffer.
    pub fn flush(&self) {
        if let Ok(mut buffer) = self.buffer.lock() {
            if !buffer.is_empty() {
                let data = std::mem::take(&mut *buffer);
                self.emit_event(StdinBufferEvent::Batch(data));
            }
        }
    }

    /// Check if we're in paste mode.
    pub fn is_in_paste(&self) -> bool {
        *self.in_paste.lock().unwrap()
    }

    /// Emit an event to all callbacks.
    fn emit_event(&self, event: StdinBufferEvent) {
        if let Ok(callbacks) = self.callbacks.lock() {
            for callback in callbacks.iter() {
                callback(&event);
            }
        }

        // Also add to event queue
        if let Ok(mut events) = self.events.lock() {
            events.push_back(event);
        }
    }

    /// Get pending events.
    pub fn pop_event(&self) -> Option<StdinBufferEvent> {
        self.events.lock().ok()?.pop_front()
    }

    /// Clear all pending events.
    pub fn clear_events(&self) {
        if let Ok(mut events) = self.events.lock() {
            events.clear();
        }
    }

    /// Reset the buffer state.
    pub fn reset(&self) {
        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.clear();
        }
        if let Ok(mut in_paste) = self.in_paste.lock() {
            *in_paste = false;
        }
        if let Ok(mut paste_buffer) = self.paste_buffer.lock() {
            paste_buffer.clear();
        }
        if let Ok(mut last_input) = self.last_input.lock() {
            *last_input = None;
        }
        self.clear_events();
    }
}

impl Default for StdinBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_input() {
        let buffer = StdinBuffer::new();
        let received = Arc::new(Mutex::new(String::new()));
        let received_clone = received.clone();

        buffer.on_event(Arc::new(move |event| {
            if let StdinBufferEvent::Data(data) = event {
                *received_clone.lock().unwrap() = data.clone();
            }
        }));

        buffer.process("hello");
        assert_eq!(*received.lock().unwrap(), "hello");
    }

    #[test]
    fn test_bracketed_paste() {
        let buffer = StdinBuffer::new();
        let received = Arc::new(Mutex::new(String::new()));
        let received_clone = received.clone();

        buffer.on_event(Arc::new(move |event| {
            if let StdinBufferEvent::PasteEnd(data) = event {
                *received_clone.lock().unwrap() = data.clone();
            }
        }));

        buffer.process("\x1b[200~pasted content\x1b[201~");
        assert_eq!(*received.lock().unwrap(), "pasted content");
    }

    #[test]
    fn test_reset() {
        let buffer = StdinBuffer::new();
        buffer.process("data");
        buffer.reset();

        assert!(!buffer.is_in_paste());
    }
}
