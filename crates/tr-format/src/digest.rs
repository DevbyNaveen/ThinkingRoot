//! BLAKE3 helpers used for pack content hashes and revocation lookups.
//!
//! The hex form used on the wire and inside manifests is always
//! **lowercase** 64-char hex. Use [`blake3_hex`] or [`parse_hex`] to
//! move between that wire form and raw bytes; never roll your own.

use crate::error::{Error, Result};

/// BLAKE3-256 as lowercase hex, the canonical form used in manifests,
/// revocation snapshots, and the Living Credits event log.
pub fn blake3_hex(bytes: &[u8]) -> String {
    hex::encode(blake3::hash(bytes).as_bytes())
}

/// Streaming variant — hashes the output of any `Read`. Returns both
/// the lowercase-hex digest and the number of bytes consumed.
pub fn blake3_hex_reader<R: std::io::Read>(mut reader: R) -> Result<(String, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex::encode(hasher.finalize().as_bytes()), total))
}

/// Parse a canonical 64-char lowercase hex digest back into raw bytes.
pub fn parse_hex(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 {
        return Err(Error::Invalid {
            what: "content_hash",
            detail: format!("expected 64 hex chars, got {}", s.len()),
        });
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(Error::Invalid {
            what: "content_hash",
            detail: "must be lowercase hex".into(),
        });
    }
    let mut out = [0u8; 32];
    hex::decode_to_slice(s, &mut out).map_err(|e| Error::Invalid {
        what: "content_hash",
        detail: e.to_string(),
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_hex_matches_blake3_hash() {
        let input = b"the quick brown fox";
        let expected = hex::encode(blake3::hash(input).as_bytes());
        assert_eq!(blake3_hex(input), expected);
        assert_eq!(blake3_hex(input).len(), 64);
    }

    #[test]
    fn reader_matches_contiguous() {
        let input = b"aaaabbbbccccdddd".repeat(10_000);
        let a = blake3_hex(&input);
        let (b, n) = blake3_hex_reader(std::io::Cursor::new(&input)).unwrap();
        assert_eq!(a, b);
        assert_eq!(n, input.len() as u64);
    }

    #[test]
    fn parse_hex_rejects_bad_input() {
        assert!(parse_hex("short").is_err());
        assert!(parse_hex(&"Z".repeat(64)).is_err());
        assert!(parse_hex(&"A".repeat(64)).is_err()); // uppercase rejected
        assert!(parse_hex(&"0".repeat(64)).is_ok());
    }
}
