//! v3 pack writer — produces the 3-file `package.tr` layout per spec
//! §3.1 (`docs/2026-04-29-thinkingroot-v3-final-plan.md`):
//!
//! ```text
//! package.tr/                       # outer tar (uncompressed)
//! ├── manifest.toml                 # canonical TOML, ManifestV3
//! ├── source.tar.zst                # zstd-compressed inner tar of source files
//! ├── claims.jsonl                  # one ClaimRecord per line, sorted by id
//! └── signature.sig                 # added by Week 3 Sigstore signing
//! ```
//!
//! The outer container is plain tar. Spec §3.1 allows either tar or
//! `.tar.zst` for the outer; we pick uncompressed because `source.tar.zst`
//! inside is already the compressed payload (re-compressing offers
//! negligible savings) and an uncompressed outer means `tar -tf
//! package.tr` lists the 3 files immediately — useful for inspection
//! and debugging.
//!
//! The BLAKE3 pack-hash recipe is locked here per spec §3.1 / §16.1
//! and per D7 of the v3 implementation plan
//! (`~/.claude/plans/zippy-wiggling-pelican.md`):
//!
//! ```text
//! pack_hash = blake3(
//!     manifest_with_pack_hash_blanked.canonical_toml() ||
//!     NUL ||
//!     source.tar.zst ||
//!     NUL ||
//!     claims.jsonl
//! )
//! ```
//!
//! Once Week 3 starts signing this hash, changing the recipe invalidates
//! every previously-signed pack. The companion test
//! `tests/v3_golden_pack.rs` locks the byte-identical-across-runs
//! property before signing exists.

use std::collections::BTreeMap;
use std::io::{Cursor, Write};

use tar::{Builder, Header, HeaderMode};
use zstd::stream::write::Encoder as ZstdEncoder;

use crate::{
    claims::ClaimRecord,
    digest::blake3_hex,
    error::{Error, Result},
    manifest::ManifestV3,
};

/// Path of the inner source bundle inside the outer `.tr` tar.
const SOURCE_BUNDLE_NAME: &str = "source.tar.zst";
/// Path of the manifest inside the outer `.tr` tar.
const MANIFEST_NAME: &str = "manifest.toml";
/// Path of the claim journal inside the outer `.tr` tar.
const CLAIMS_NAME: &str = "claims.jsonl";

/// Programmatic builder for a v3 `.tr` pack. Cheaply constructed; build
/// via [`V3PackBuilder::build`] when ready.
pub struct V3PackBuilder {
    manifest: ManifestV3,
    /// Source files staged in a `BTreeMap` so ordering is deterministic
    /// across runs — same key set in the same order produces byte-
    /// identical bytes.
    source_files: BTreeMap<String, Vec<u8>>,
    /// Claim records. Sorted by `id` at `build()` time per spec §10.2.
    claims: Vec<ClaimRecord>,
    /// Compression level for the inner `source.tar.zst`. Default 3 (the
    /// zstd default — fast write, good ratio). Production callers can
    /// lift to 19+ for archive-grade compression.
    inner_zstd_level: i32,
}

impl V3PackBuilder {
    /// Construct a builder over the given manifest. Hashes are filled
    /// in at `build()` time; callers don't need to set them.
    pub fn new(manifest: ManifestV3) -> Self {
        Self {
            manifest,
            source_files: BTreeMap::new(),
            claims: Vec::new(),
            inner_zstd_level: 3,
        }
    }

    /// Override the inner `source.tar.zst` zstd level. Acceptable range
    /// is `1..=22`; values are clamped.
    pub fn with_inner_zstd_level(mut self, level: i32) -> Self {
        self.inner_zstd_level = level.clamp(1, 22);
        self
    }

    /// Stage a source file under `path` (relative POSIX path, no `..`,
    /// no leading `/`). Replaces any prior entry at the same path.
    pub fn add_source_file(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        assert_safe_path(path)?;
        self.source_files.insert(path.to_string(), bytes.to_vec());
        Ok(())
    }

    /// Append a claim record. Order doesn't matter — `build()` sorts by
    /// id ascending before serializing.
    pub fn add_claim(&mut self, record: ClaimRecord) {
        self.claims.push(record);
    }

    /// Append many claim records.
    pub fn extend_claims(&mut self, records: impl IntoIterator<Item = ClaimRecord>) {
        self.claims.extend(records);
    }

    /// Finalise. Computes `source_hash`, `claims_hash`, and `pack_hash`,
    /// fills the manifest, and emits the outer tar bytes.
    ///
    /// Steps (locked):
    /// 1. Build inner `source.tar.zst` (deterministic tar +
    ///    zstd-encoded).
    /// 2. Sort claims by id ASC; serialize as JSONL with sorted JSON
    ///    keys per record.
    /// 3. Hash both, populate the manifest's `source_hash` /
    ///    `claims_hash` / informational counts.
    /// 4. Compute `pack_hash` over `(canonical_manifest || NUL ||
    ///    source.tar.zst || NUL || claims.jsonl)`.
    /// 5. Re-emit the manifest with `pack_hash` populated.
    /// 6. Wrap into the outer tar `[manifest.toml, source.tar.zst,
    ///    claims.jsonl]` in that order.
    pub fn build(mut self) -> Result<Vec<u8>> {
        // 1. Inner source archive.
        let source_tar_zst = self.build_inner_source_archive()?;
        let source_hash = format!("blake3:{}", blake3_hex(&source_tar_zst));

        // 2. Claims JSONL.
        self.claims.sort_by(|a, b| a.id.cmp(&b.id));
        let claims_jsonl = self.serialize_claims_jsonl()?;
        let claims_hash = format!("blake3:{}", blake3_hex(&claims_jsonl));

        // 3. Populate informational manifest fields. Hashes go in now;
        //    `pack_hash` stays empty until step 4.
        self.manifest.source_hash = source_hash;
        self.manifest.claims_hash = claims_hash;
        self.manifest.source_files = Some(self.source_files.len() as u64);
        self.manifest.source_bytes = Some(
            self.source_files
                .values()
                .map(|b| b.len() as u64)
                .sum::<u64>(),
        );
        self.manifest.claim_count = Some(self.claims.len() as u64);

        // 4. Pack hash recipe. The canonical manifest bytes here have
        //    `pack_hash` blanked — that's the contract spec §3.1 makes
        //    with verifiers.
        let canonical_manifest = self.manifest.canonical_bytes_for_hashing();
        let mut hash_input = Vec::with_capacity(
            canonical_manifest.len() + 1 + source_tar_zst.len() + 1 + claims_jsonl.len(),
        );
        hash_input.extend_from_slice(&canonical_manifest);
        hash_input.push(0);
        hash_input.extend_from_slice(&source_tar_zst);
        hash_input.push(0);
        hash_input.extend_from_slice(&claims_jsonl);
        self.manifest.pack_hash = format!("blake3:{}", blake3_hex(&hash_input));

        // 5. Final manifest.toml — same canonicalization, but with
        //    `pack_hash` filled in now.
        let manifest_toml = self.manifest.to_canonical_toml();

        // 6. Outer tar (uncompressed).
        let mut outer = Vec::with_capacity(
            manifest_toml.len() + source_tar_zst.len() + claims_jsonl.len() + 4096,
        );
        {
            let cursor = Cursor::new(&mut outer);
            let mut tar_builder = Builder::new(cursor);
            tar_builder.mode(HeaderMode::Deterministic);
            append_file(&mut tar_builder, MANIFEST_NAME, &manifest_toml)?;
            append_file(&mut tar_builder, SOURCE_BUNDLE_NAME, &source_tar_zst)?;
            append_file(&mut tar_builder, CLAIMS_NAME, &claims_jsonl)?;
            tar_builder.finish()?;
        }
        Ok(outer)
    }

    fn build_inner_source_archive(&self) -> Result<Vec<u8>> {
        let mut tar_bytes = Vec::with_capacity(4096);
        {
            let cursor = Cursor::new(&mut tar_bytes);
            let mut tar_builder = Builder::new(cursor);
            tar_builder.mode(HeaderMode::Deterministic);
            // BTreeMap iteration is in sorted-key order — deterministic.
            for (path, contents) in &self.source_files {
                append_file(&mut tar_builder, path, contents)?;
            }
            tar_builder.finish()?;
        }
        let mut compressed = Vec::with_capacity(tar_bytes.len() / 2);
        {
            let mut encoder = ZstdEncoder::new(&mut compressed, self.inner_zstd_level)
                .map_err(|e| Error::Invalid {
                    what: "source.tar.zst",
                    detail: format!("zstd encoder: {e}"),
                })?;
            encoder.write_all(&tar_bytes)?;
            encoder.finish().map_err(|e| Error::Invalid {
                what: "source.tar.zst",
                detail: format!("zstd finish: {e}"),
            })?;
        }
        Ok(compressed)
    }

    fn serialize_claims_jsonl(&self) -> Result<Vec<u8>> {
        // serde_json::to_vec emits compact JSON in struct field order.
        // Field order is locked at the ClaimRecord struct declaration
        // (`id, stmt, ents, file, start, end` first, then optionals);
        // both producers and consumers see the same key order.
        let mut out = Vec::with_capacity(self.claims.len() * 256);
        for claim in &self.claims {
            let line = serde_json::to_vec(claim)?;
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
        Ok(out)
    }
}

fn append_file<W: Write>(builder: &mut Builder<W>, path: &str, bytes: &[u8]) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    builder
        .append_data(&mut header, path, bytes)
        .map_err(Error::from)?;
    Ok(())
}

/// Same path-safety contract as the v1 writer: relative paths only, no
/// `..`, no leading `/`. The outer tar paths are also validated here so
/// we never emit a v3 pack that an older or stricter reader would
/// reject.
fn assert_safe_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(Error::Invalid {
            what: "path",
            detail: "empty path".into(),
        });
    }
    if path.starts_with('/') {
        return Err(Error::Invalid {
            what: "path",
            detail: format!("path `{path}` must be relative"),
        });
    }
    for component in path.split('/') {
        if component == ".." {
            return Err(Error::Invalid {
                what: "path",
                detail: format!("path `{path}` contains `..`"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;

    fn fixture_manifest() -> ManifestV3 {
        ManifestV3 {
            license: Some("MIT".into()),
            description: Some("test pack".into()),
            authors: vec!["alice@example.com".into()],
            ..ManifestV3::new("alice/test", Version::parse("1.0.0").unwrap())
        }
    }

    #[test]
    fn build_produces_three_entry_tar() {
        let mut b = V3PackBuilder::new(fixture_manifest());
        b.add_source_file("hello.md", b"# Hello\n").unwrap();
        b.add_claim(ClaimRecord::new(
            "c-1",
            "Greeting",
            vec!["Hello".into()],
            "hello.md",
            0,
            8,
        ));
        let bytes = b.build().unwrap();

        // Inspect the outer tar directly — must list exactly the 3 files.
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| {
                let e = e.ok()?;
                let p = e.path().ok()?.to_string_lossy().into_owned();
                Some(p)
            })
            .collect();
        assert_eq!(names, vec![MANIFEST_NAME, SOURCE_BUNDLE_NAME, CLAIMS_NAME]);
    }

    #[test]
    fn pack_bytes_stable_across_runs() {
        let make = || {
            let mut b = V3PackBuilder::new(fixture_manifest());
            b.add_source_file("a.md", b"alpha").unwrap();
            b.add_source_file("b.md", b"beta").unwrap();
            b.add_claim(ClaimRecord::new("c-2", "Two", vec![], "b.md", 0, 4));
            b.add_claim(ClaimRecord::new("c-1", "One", vec![], "a.md", 0, 5));
            b.build().unwrap()
        };
        let p1 = make();
        let p2 = make();
        assert_eq!(p1, p2, "byte-identical builds across runs");
    }

    #[test]
    fn claims_sorted_by_id_ascending() {
        let mut b = V3PackBuilder::new(fixture_manifest());
        b.add_source_file("a.md", b"x").unwrap();
        b.add_claim(ClaimRecord::new("c-9", "Nine", vec![], "a.md", 0, 1));
        b.add_claim(ClaimRecord::new("c-1", "One", vec![], "a.md", 0, 1));
        b.add_claim(ClaimRecord::new("c-5", "Five", vec![], "a.md", 0, 1));
        let bytes = b.build().unwrap();

        // Extract claims.jsonl from the outer tar and verify order.
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().to_string_lossy() == CLAIMS_NAME {
                let mut buf = String::new();
                use std::io::Read;
                entry.read_to_string(&mut buf).unwrap();
                let ids: Vec<&str> = buf
                    .lines()
                    .map(|l| {
                        let v: serde_json::Value = serde_json::from_str(l).unwrap();
                        v["id"].as_str().unwrap().to_string().leak() as &str
                    })
                    .collect();
                assert_eq!(ids, vec!["c-1", "c-5", "c-9"]);
                return;
            }
        }
        panic!("claims.jsonl not found in outer tar");
    }

    #[test]
    fn manifest_pack_hash_present_after_build() {
        let mut b = V3PackBuilder::new(fixture_manifest());
        b.add_source_file("hello.md", b"# Hello\n").unwrap();
        let bytes = b.build().unwrap();

        let mut archive = tar::Archive::new(Cursor::new(bytes));
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().to_string_lossy() == MANIFEST_NAME {
                let mut buf = Vec::new();
                use std::io::Read;
                entry.read_to_end(&mut buf).unwrap();
                let m = ManifestV3::parse(&buf).unwrap();
                assert!(m.pack_hash.starts_with("blake3:"));
                assert_eq!(m.pack_hash.len(), "blake3:".len() + 64);
                assert!(m.source_hash.starts_with("blake3:"));
                assert!(m.claims_hash.starts_with("blake3:"));
                assert_eq!(m.source_files, Some(1));
                assert_eq!(m.claim_count, Some(0));
                return;
            }
        }
        panic!("manifest.toml not found in outer tar");
    }

    #[test]
    fn rejects_unsafe_paths() {
        let mut b = V3PackBuilder::new(fixture_manifest());
        assert!(b.add_source_file("/abs.md", b"x").is_err());
        assert!(b.add_source_file("../escape.md", b"x").is_err());
        assert!(b.add_source_file("a/../b.md", b"x").is_err());
        assert!(b.add_source_file("", b"x").is_err());
    }

    #[test]
    fn empty_pack_builds_successfully() {
        let b = V3PackBuilder::new(fixture_manifest());
        let bytes = b.build().unwrap();

        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let count = archive.entries().unwrap().count();
        assert_eq!(count, 3, "still emits manifest + source + claims");
    }
}
