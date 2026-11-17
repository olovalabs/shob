//! Bounded JSONL framing for the native RPC transport.
use super::protocol::MAX_FRAME_BYTES;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Read one frame, counting every byte including newline/CRLF. A final
/// unterminated EOF frame is accepted. The dispatcher validates UTF-8.
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let length = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1);
        if line.len() + length > MAX_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "RPC frame exceeds maximum size",
            ));
        }
        let complete = bytes[length - 1] == b'\n';
        line.extend_from_slice(&bytes[..length]);
        reader.consume(length);
        if complete {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn reads_one_bounded_frame() {
        let mut reader = BufReader::new(&b"{\"type\":\"ping\"}\n"[..]);
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap(),
            b"{\"type\":\"ping\"}\n"
        );
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn exact_boundaries_include_lf_crlf_and_final_eof() {
        for ending in [b"\n".as_slice(), b"\r\n".as_slice(), b"".as_slice()] {
            let mut bytes = vec![b' '; MAX_FRAME_BYTES - ending.len()];
            bytes.extend_from_slice(ending);
            let mut reader = BufReader::with_capacity(127, bytes.as_slice());
            assert_eq!(
                read_frame(&mut reader).await.unwrap().unwrap().len(),
                MAX_FRAME_BYTES
            );
            assert!(read_frame(&mut reader).await.unwrap().is_none());
            bytes.insert(0, b' ');
            let mut reader = BufReader::with_capacity(127, bytes.as_slice());
            assert_eq!(
                read_frame(&mut reader).await.unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
        }
    }
}
