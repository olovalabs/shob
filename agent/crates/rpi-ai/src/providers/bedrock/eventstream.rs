use bytes::{Buf, BytesMut};
use std::collections::BTreeMap;

#[derive(Debug)]
pub(super) struct Frame {
    pub headers: BTreeMap<String, String>,
    pub payload: Vec<u8>,
}

#[derive(Default)]
pub(super) struct Decoder {
    bytes: BytesMut,
}
impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }
    pub fn finish(&self) -> Result<(), String> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err("Truncated AWS eventstream frame".into())
        }
    }
    pub fn next(&mut self) -> Result<Option<Frame>, String> {
        if self.bytes.len() < 12 {
            return Ok(None);
        }
        let total = u32::from_be_bytes(self.bytes[..4].try_into().unwrap()) as usize;
        let headers = u32::from_be_bytes(self.bytes[4..8].try_into().unwrap()) as usize;
        if total < 16 || total > 16 * 1024 * 1024 || headers > total - 16 {
            return Err("Invalid AWS eventstream frame length".into());
        }
        if crc32fast::hash(&self.bytes[..8])
            != u32::from_be_bytes(self.bytes[8..12].try_into().unwrap())
        {
            return Err("AWS eventstream prelude CRC mismatch".into());
        }
        if self.bytes.len() < total {
            return Ok(None);
        }
        if crc32fast::hash(&self.bytes[..total - 4])
            != u32::from_be_bytes(self.bytes[total - 4..total].try_into().unwrap())
        {
            return Err("AWS eventstream message CRC mismatch".into());
        }
        let mut block = &self.bytes[12..12 + headers];
        let mut parsed = BTreeMap::new();
        while !block.is_empty() {
            let len = take(&mut block, 1)?[0] as usize;
            let name = std::str::from_utf8(take(&mut block, len)?)
                .map_err(|e| e.to_string())?
                .to_owned();
            let kind = take(&mut block, 1)?[0];
            let value = match kind {
                0 => "true".into(),
                1 => "false".into(),
                2 => (take(&mut block, 1)?[0] as i8).to_string(),
                3 => i16::from_be_bytes(take(&mut block, 2)?.try_into().unwrap()).to_string(),
                4 => i32::from_be_bytes(take(&mut block, 4)?.try_into().unwrap()).to_string(),
                5 | 8 => i64::from_be_bytes(take(&mut block, 8)?.try_into().unwrap()).to_string(),
                6 | 7 => {
                    let n = u16::from_be_bytes(take(&mut block, 2)?.try_into().unwrap()) as usize;
                    let value = take(&mut block, n)?;
                    if kind == 7 {
                        std::str::from_utf8(value)
                            .map_err(|e| e.to_string())?
                            .to_owned()
                    } else {
                        super::auth::hex(value)
                    }
                }
                9 => super::auth::hex(take(&mut block, 16)?),
                _ => return Err("Unknown AWS eventstream header type".into()),
            };
            parsed.insert(name, value);
        }
        let payload = self.bytes[12 + headers..total - 4].to_vec();
        self.bytes.advance(total);
        Ok(Some(Frame {
            headers: parsed,
            payload,
        }))
    }
}
fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
    if bytes.len() < n {
        return Err("Truncated AWS eventstream header".into());
    }
    let (value, rest) = bytes.split_at(n);
    *bytes = rest;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame() -> Vec<u8> {
        let header = b"\x0b:event-type\x07\x00\x08metadata";
        let body = b"{\"usage\":{}}";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((16 + header.len() + body.len()) as u32).to_be_bytes());
        bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_be_bytes());
        bytes.extend_from_slice(header);
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_be_bytes());
        bytes
    }
    #[test]
    fn decodes_every_chunk_boundary_and_multiple_frames() {
        let bytes = frame();
        for split in 0..bytes.len() {
            let mut decoder = Decoder::default();
            decoder.push(&bytes[..split]);
            assert!(decoder.next().unwrap().is_none());
            decoder.push(&bytes[split..]);
            decoder.push(&bytes);
            for _ in 0..2 {
                let f = decoder.next().unwrap().unwrap();
                assert_eq!(f.headers[":event-type"], "metadata");
                assert_eq!(f.payload, b"{\"usage\":{}}");
            }
            assert!(decoder.next().unwrap().is_none());
            decoder.finish().unwrap();
        }
    }
    #[test]
    fn rejects_corruption_and_truncation() {
        let mut bytes = frame();
        bytes[20] ^= 1;
        let mut decoder = Decoder::default();
        decoder.push(&bytes);
        assert!(decoder.next().unwrap_err().contains("CRC"));
        let mut decoder = Decoder::default();
        decoder.push(&bytes[..4]);
        assert!(decoder.finish().is_err());
    }
}
