//! Length-prefixed framing for the binary (CBOR) protocol.
//!
//! Port of the upstream `packages/protocol/` module.
//! 4-byte big-endian unsigned payload length followed by the payload bytes.
//! [`FrameDecoder`] incrementally splits arbitrary byte chunks into complete
//! payloads and enforces a maximum frame length so a corrupt/hostile stream
//! cannot make the peer allocate unbounded memory.

use std::fmt;

/// Native `FRAME_HEADER_LENGTH`.
pub const FRAME_HEADER_LENGTH: usize = 4;
/// Native `DEFAULT_MAX_FRAME_LENGTH` (16 MiB).
pub const DEFAULT_MAX_FRAME_LENGTH: usize = 16 * 1024 * 1024;
/// Native `PAYLOAD_BLOCK_SIZE`.
pub const PAYLOAD_BLOCK_SIZE: usize = 64 * 1024;

/// Framing error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError(pub String);

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FrameError {}

/// Prefix `payload` with its 4-byte big-endian length.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > u32::MAX as usize {
        return Err(FrameError(
            "Frame payload exceeds the unsigned 32-bit length limit".into(),
        ));
    }
    let mut frame = Vec::with_capacity(FRAME_HEADER_LENGTH + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Validate that `frame` contains exactly one complete frame within `max`.
pub fn assert_complete_frame(frame: &[u8], max_frame_length: usize) -> Result<(), FrameError> {
    if frame.len() < FRAME_HEADER_LENGTH {
        return Err(FrameError(
            "Frame does not contain a complete length prefix".into(),
        ));
    }
    let length = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    if length > max_frame_length {
        return Err(FrameError(format!(
            "Frame length {length} exceeds configured limit of {max_frame_length}"
        )));
    }
    if frame.len() != FRAME_HEADER_LENGTH + length {
        return Err(FrameError(
            "Frame must contain exactly one complete payload".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    Ended,
    Failed,
}

/// Incremental decoder: feed byte chunks, get whole payloads back.
pub struct FrameDecoder {
    header: [u8; FRAME_HEADER_LENGTH],
    header_len: usize,
    max_frame_length: usize,
    payload: Vec<u8>,
    expected_payload_len: Option<usize>,
    state: State,
}

impl FrameDecoder {
    pub fn new(max_frame_length: usize) -> Self {
        Self {
            header: [0; FRAME_HEADER_LENGTH],
            header_len: 0,
            max_frame_length,
            payload: Vec::new(),
            expected_payload_len: None,
            state: State::Open,
        }
    }

    pub fn with_default_max() -> Self {
        Self::new(DEFAULT_MAX_FRAME_LENGTH)
    }

    /// Feed `chunk`, returning every complete payload it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        match self.state {
            State::Ended => return Err(FrameError("Frame decoder has ended".into())),
            State::Failed => return Err(FrameError("Frame decoder has failed".into())),
            State::Open => {}
        }

        let mut frames = Vec::new();
        let mut offset = 0usize;
        while offset < chunk.len() {
            if self.expected_payload_len.is_none() {
                let take = (FRAME_HEADER_LENGTH - self.header_len).min(chunk.len() - offset);
                self.header[self.header_len..self.header_len + take]
                    .copy_from_slice(&chunk[offset..offset + take]);
                self.header_len += take;
                offset += take;
                if self.header_len < FRAME_HEADER_LENGTH {
                    continue;
                }
                let frame_length = u32::from_be_bytes(self.header) as usize;
                self.header_len = 0;
                if frame_length > self.max_frame_length {
                    self.state = State::Failed;
                    return Err(FrameError(format!(
                        "Frame length {frame_length} exceeds configured limit of {}",
                        self.max_frame_length
                    )));
                }
                if frame_length == 0 {
                    frames.push(Vec::new());
                    continue;
                }
                self.expected_payload_len = Some(frame_length);
                self.payload = Vec::with_capacity(frame_length.min(PAYLOAD_BLOCK_SIZE));
            }

            let expected = self.expected_payload_len.unwrap();
            let want = expected - self.payload.len();
            let take = want.min(chunk.len() - offset);
            self.payload
                .extend_from_slice(&chunk[offset..offset + take]);
            offset += take;

            if self.payload.len() == expected {
                frames.push(std::mem::take(&mut self.payload));
                self.expected_payload_len = None;
            }
        }
        Ok(frames)
    }

    /// Mark the stream ended. Any buffered partial frame is an error.
    pub fn end(&mut self) -> Result<(), FrameError> {
        if self.header_len != 0 || self.expected_payload_len.is_some() {
            self.state = State::Failed;
            return Err(FrameError("Frame decoder ended mid-frame".into()));
        }
        self.state = State::Ended;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_round_trip() {
        let frame = encode_frame(b"hello").unwrap();
        assert_eq!(&frame[..4], &[0, 0, 0, 5]);
        assert_complete_frame(&frame, DEFAULT_MAX_FRAME_LENGTH).unwrap();
    }

    #[test]
    fn decoder_splits_multiple_and_partial_chunks() {
        let mut decoder = FrameDecoder::with_default_max();
        let a = encode_frame(b"one").unwrap();
        let b = encode_frame(b"two").unwrap();
        let mut stream = Vec::new();
        stream.extend_from_slice(&a);
        stream.extend_from_slice(&b);

        // Deliberately split mid-header and mid-payload.
        let out1 = decoder.push(&stream[..2]).unwrap();
        assert!(out1.is_empty());
        let out2 = decoder.push(&stream[2..5]).unwrap();
        assert!(out2.is_empty());
        let out3 = decoder.push(&stream[5..7]).unwrap();
        assert_eq!(out3, vec![b"one".to_vec()]);
        let out4 = decoder.push(&stream[7..]).unwrap();
        assert_eq!(out4, vec![b"two".to_vec()]);
        decoder.end().unwrap();
    }

    #[test]
    fn empty_frame() {
        let frame = encode_frame(b"").unwrap();
        let mut decoder = FrameDecoder::with_default_max();
        assert_eq!(decoder.push(&frame).unwrap(), vec![Vec::<u8>::new()]);
    }

    #[test]
    fn rejects_oversized() {
        let frame = encode_frame(&vec![0u8; 32]).unwrap();
        assert!(assert_complete_frame(&frame, 16).is_err());
        let mut decoder = FrameDecoder::new(16);
        assert!(decoder.push(&frame).is_err());
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut frame = encode_frame(b"x").unwrap();
        frame.push(0);
        assert!(assert_complete_frame(&frame, DEFAULT_MAX_FRAME_LENGTH).is_err());
    }
}
