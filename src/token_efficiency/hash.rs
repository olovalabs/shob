//! SHA-256, implemented locally.
//!
//! The cache's correctness rests on comparing file bytes; a hash is the only
//! cheap way to do that without shipping the bytes around. The root crate has
//! no hashing dependency and `sha2` appears in `Cargo.lock` at two incompatible
//! versions (0.10.9 transitively, 0.11.0 elsewhere), so adding an edge here
//! would mean resolving that ambiguity for a 200-line algorithm. FIPS 180-4 is
//! short and fully specified, and `hash.rs`'s tests pin the standard vectors.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// A SHA-256 digest. `Display` renders lowercase hex, the form used in cache
/// keys and in the anchor tags shown to the model.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(sha256(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex, all 64 characters. Used for cache keys.
    pub fn hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble is a hex digit"));
            out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("nibble is a hex digit"));
        }
        out
    }

    /// First `chars` hex characters. The anchor tag the model sees is
    /// `tag(4)`; four hex characters over a per-file digest is a display
    /// shorthand, not the comparison — every hit is still verified against the
    /// full digest and a byte length.
    pub fn tag(&self, chars: usize) -> String {
        self.hex().chars().take(chars).collect()
    }
}

impl std::fmt::Debug for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.tag(12))
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

/// SHA-256 of `input` (FIPS 180-4 §6.2).
pub fn sha256(input: &[u8]) -> [u8; 32] {
    let mut state = H0;

    let mut message = Vec::with_capacity(input.len() + 72);
    message.extend_from_slice(input);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((input.len() as u64).wrapping_mul(8)).to_be_bytes());

    for chunk in message.chunks_exact(64) {
        compress(&mut state, chunk);
    }

    let mut out = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn compress(state: &mut [u32; 8], chunk: &[u8]) {
    let mut w = [0u32; 64];
    for (index, word) in chunk.chunks_exact(4).enumerate() {
        w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
    }
    for index in 16..64 {
        let s0 = w[index - 15].rotate_right(7)
            ^ w[index - 15].rotate_right(18)
            ^ (w[index - 15] >> 3);
        let s1 = w[index - 2].rotate_right(17)
            ^ w[index - 2].rotate_right(19)
            ^ (w[index - 2] >> 10);
        w[index] = w[index - 16]
            .wrapping_add(s0)
            .wrapping_add(w[index - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;

    for index in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[index])
            .wrapping_add(w[index]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// A stable, non-cryptographic 64-bit digest of a key's parts. Used for cache
/// request keys, where a collision only costs a spurious miss-as-miss (the
/// content stamp is compared separately), never a false hit.
pub fn key_digest(parts: &[&str]) -> String {
    let mut hasher_state = H0;
    // Length-prefix each part so ("ab","c") and ("a","bc") differ.
    let mut buffer = Vec::new();
    for part in parts {
        buffer.extend_from_slice(&(part.len() as u64).to_be_bytes());
        buffer.extend_from_slice(part.as_bytes());
    }
    let mut message = buffer;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&(message.len() as u64).to_be_bytes());
    for chunk in message.chunks_exact(64) {
        compress(&mut hasher_state, chunk);
    }
    let mut out = String::with_capacity(16);
    for word in hasher_state.iter().take(2) {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_of(input: &[u8]) -> String {
        Digest::from_bytes(input).hex()
    }

    // FIPS 180-4 / NIST CAVP standard vectors.
    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            hex_of(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex_of(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_handles_block_boundaries() {
        // 55, 56 and 64 bytes are the padding edges: 56 forces a second block,
        // 64 lands exactly on one.
        let expected = "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318";
        assert_eq!(hex_of(&[b'a'; 55]), expected);
        assert_eq!(
            hex_of(&[b'a'; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            hex_of(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    #[test]
    fn million_a_vector() {
        let block = vec![b'a'; 1_000_000];
        assert_eq!(
            hex_of(&block),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn tag_is_a_prefix() {
        let digest = Digest::from_bytes(b"abc");
        assert_eq!(digest.tag(4), "ba78");
        assert_eq!(digest.tag(4).len(), 4);
        assert!(digest.hex().starts_with(&digest.tag(8)));
    }

    #[test]
    fn different_inputs_differ() {
        assert_ne!(Digest::from_bytes(b"a"), Digest::from_bytes(b"b"));
        assert_ne!(Digest::from_bytes(b""), Digest::from_bytes(b"\0"));
    }

    #[test]
    fn key_digest_is_unambiguous_across_part_boundaries() {
        assert_ne!(key_digest(&["ab", "c"]), key_digest(&["a", "bc"]));
        assert_eq!(key_digest(&["a", "b"]), key_digest(&["a", "b"]));
        assert_eq!(key_digest(&["read", "/a.rs", "1"]).len(), 16);
    }
}
