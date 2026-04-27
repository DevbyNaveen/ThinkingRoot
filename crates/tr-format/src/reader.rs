//! Open a `.tr` pack: parse the manifest, enumerate entries, verify
//! invariants.
//!
//! Readers load the archive entries lazily via the tar iterator. The
//! top-level [`Pack`] value holds the parsed manifest and a map of
//! entry paths → byte ranges (or bytes, in the in-memory case) so
//! callers can fetch individual entries cheaply.

use std::{collections::BTreeMap, io::Cursor, path::Path};

use crate::{
    digest::blake3_hex,
    error::{Error, Result},
    manifest::Manifest,
    writer,
};

/// Default byte cap applied by [`read_bytes`] (128 MiB).
pub const DEFAULT_SIZE_CAP: u64 = 128 * 1024 * 1024;

/// An opened TR-1 pack.
#[derive(Debug)]
pub struct Pack {
    /// Parsed manifest.
    pub manifest: Manifest,
    /// BLAKE3 (hex) of the raw `.tr` bytes as delivered.
    pub content_bytes_hash: String,
    /// Entries indexed by in-pack path.
    entries: BTreeMap<String, Vec<u8>>,
}

impl Pack {
    /// All entry paths, sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Read a specific entry. Returns `None` if the path is absent.
    pub fn entry(&self, path: &str) -> Option<&[u8]> {
        self.entries.get(path).map(Vec::as_slice)
    }

    /// Number of entries (including `manifest.json`).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if the pack is empty (should never happen for a valid
    /// pack, but kept for symmetry with `len`).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total size across all payload entries (excluding manifest).
    pub fn payload_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|(p, _)| p.as_str() != "manifest.json")
            .map(|(_, b)| b.len() as u64)
            .sum()
    }
}

/// Read and parse a `.tr` from a byte slice, enforcing the default
/// size cap. For uploads exceeding the cap, use [`read_bytes_capped`].
pub fn read_bytes(bytes: &[u8]) -> Result<Pack> {
    read_bytes_capped(bytes, DEFAULT_SIZE_CAP)
}

/// Same as [`read_bytes`] with an explicit cap.
pub fn read_bytes_capped(bytes: &[u8], cap: u64) -> Result<Pack> {
    if bytes.len() as u64 > cap {
        return Err(Error::TooLarge {
            cap,
            actual: bytes.len() as u64,
        });
    }
    let entries = writer::read_entries(Cursor::new(bytes))?;
    finalise(entries, bytes)
}

/// Read and parse a `.tr` file from disk, enforcing the default size
/// cap against the file's on-disk size.
pub fn read_file(path: impl AsRef<Path>) -> Result<Pack> {
    let bytes = std::fs::read(path)?;
    read_bytes(&bytes)
}

fn finalise(entries: BTreeMap<String, Vec<u8>>, raw: &[u8]) -> Result<Pack> {
    let manifest_bytes = entries
        .get("manifest.json")
        .ok_or(Error::Missing("manifest.json"))?;
    let manifest = Manifest::parse(manifest_bytes)?;

    Ok(Pack {
        manifest,
        content_bytes_hash: blake3_hex(raw),
        entries,
    })
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::*;
    use crate::{manifest::Manifest, writer::PackBuilder};

    fn make_pack() -> Vec<u8> {
        let mut pb = PackBuilder::new(Manifest::new(
            "alice/demo",
            Version::parse("0.1.0").unwrap(),
            "Apache-2.0",
        ));
        pb.put_text("artifacts/card.md", "# Hello").unwrap();
        pb.put_text("graph/t.jsonl", "{}").unwrap();
        pb.build().unwrap()
    }

    #[test]
    fn read_returns_manifest_and_entries() {
        let bytes = make_pack();
        let p = read_bytes(&bytes).unwrap();
        assert_eq!(p.manifest.name, "alice/demo");
        assert!(p.paths().any(|p| p == "manifest.json"));
        assert!(p.entry("artifacts/card.md").is_some());
        assert!(p.entry("does-not-exist").is_none());
        assert_eq!(p.content_bytes_hash.len(), 64);
        assert!(p.payload_bytes() > 0);
        assert!(!p.is_empty());
    }

    #[test]
    fn cap_is_enforced() {
        let bytes = make_pack();
        let err = read_bytes_capped(&bytes, 10).unwrap_err();
        assert!(matches!(err, Error::TooLarge { .. }));
    }

    #[test]
    fn missing_manifest_is_detected() {
        // Build a valid archive then strip manifest.json in-place by
        // re-tarring without it.
        use std::io::{Cursor, Write};
        use tar::{Builder, Header};

        let mut tar_bytes = Vec::new();
        {
            let mut b = Builder::new(Cursor::new(&mut tar_bytes));
            let mut header = Header::new_gnu();
            header.set_path("random.txt").unwrap();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            b.append(&header, &mut Cursor::new(b"hey")).unwrap();
            b.finish().unwrap();
        }
        let mut zstd_bytes = Vec::new();
        {
            let mut e = zstd::stream::write::Encoder::new(&mut zstd_bytes, 3).unwrap();
            e.write_all(&tar_bytes).unwrap();
            e.finish().unwrap();
        }
        let err = read_bytes(&zstd_bytes).unwrap_err();
        assert!(matches!(err, Error::Missing("manifest.json")));
    }

    #[test]
    fn tampered_manifest_is_rejected() {
        let mut bytes = make_pack();
        // Flip a byte within the compressed stream to make it unparseable.
        bytes[20] ^= 0xff;
        assert!(read_bytes(&bytes).is_err());
    }

    #[test]
    fn round_trip_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.tr");
        std::fs::write(&path, make_pack()).unwrap();
        let p = read_file(&path).unwrap();
        assert_eq!(p.manifest.name, "alice/demo");
    }
}
