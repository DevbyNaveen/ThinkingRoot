//! v3 pack reader — parse a `package.tr` (the 3-file outer tar layout)
//! back into typed structures.
//!
//! Unlike the v1 reader, the v3 reader does **not** uncompress the
//! source bundle — it returns `source.tar.zst` as opaque bytes alongside
//! the manifest and claims. Consumers that need the source files
//! (`root verify`, the v3 reader half of `root install`) then do their
//! own zstd-decode + tar walk over the inner archive. Keeping the inner
//! source bundle as raw bytes preserves the BLAKE3 hash chain — the
//! pack-hash recipe (`docs/2026-04-29-thinkingroot-v3-final-plan.md`
//! §3.1) consumes the inner bundle byte-for-byte.

use std::io::{Cursor, Read};

use crate::{
    error::{Error, Result},
    manifest::ManifestV3,
    writer_v3::{CLAIMS_NAME, MANIFEST_NAME, SIGNATURE_NAME, SOURCE_BUNDLE_NAME},
};

pub use tr_sigstore::SigstoreBundle;

/// A parsed v3 pack. Field ordering matches the outer-tar layout
/// (`manifest, source_archive, claims_jsonl[, signature]`); ownership of
/// each component is moved out of the tar reader so callers can hand
/// them to the v3 verifier without extra copies.
#[derive(Debug, Clone)]
pub struct V3Pack {
    /// Parsed `manifest.toml` body.
    pub manifest: ManifestV3,
    /// Raw bytes of the inner `source.tar.zst`. The pack-hash recipe
    /// consumes these byte-for-byte; consumers needing a file listing
    /// run their own zstd-decode + tar walk.
    pub source_archive: Vec<u8>,
    /// Raw `claims.jsonl` payload — UTF-8, one JSON object per line.
    pub claims_jsonl: Vec<u8>,
    /// Parsed Sigstore Bundle when `signature.sig` was present in the
    /// outer tar; `None` when the pack was emitted unsigned (test
    /// fixtures, `--no-sign` flow).
    pub signature: Option<SigstoreBundle>,
}

impl V3Pack {
    /// Recompute the BLAKE3 pack hash from this pack's components per
    /// spec §3.1. Returns `blake3:<hex>` matching the format the
    /// manifest's `pack_hash` field uses. The verifier asserts this
    /// equals `manifest.pack_hash` to detect post-sign tampering.
    pub fn recompute_pack_hash(&self) -> String {
        let canonical = {
            let mut clone = self.manifest.clone();
            clone.pack_hash = String::new();
            clone.to_canonical_toml()
        };
        let mut input = Vec::with_capacity(
            canonical.len() + 1 + self.source_archive.len() + 1 + self.claims_jsonl.len(),
        );
        input.extend_from_slice(&canonical);
        input.push(0);
        input.extend_from_slice(&self.source_archive);
        input.push(0);
        input.extend_from_slice(&self.claims_jsonl);
        format!("blake3:{}", crate::digest::blake3_hex(&input))
    }
}

/// Read a v3 pack from raw outer-tar bytes.
///
/// Errors:
/// - `Error::Invalid` when any of the three required entries
///   (`manifest.toml`, `source.tar.zst`, `claims.jsonl`) is missing.
/// - `Error::Invalid` when `manifest.toml` fails parse (delegated to
///   [`ManifestV3::parse`]).
/// - `Error::Invalid` when `signature.sig` is present but isn't a
///   valid Sigstore Bundle JSON.
pub fn read_v3_pack(bytes: &[u8]) -> Result<V3Pack> {
    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut source_bytes: Option<Vec<u8>> = None;
    let mut claims_bytes: Option<Vec<u8>> = None;
    let mut signature_bytes: Option<Vec<u8>> = None;

    let mut archive = tar::Archive::new(Cursor::new(bytes));
    let entries = archive.entries().map_err(|e| Error::Invalid {
        what: "package.tr",
        detail: format!("not a valid tar archive: {e}"),
    })?;

    for entry in entries {
        let mut entry = entry.map_err(|e| Error::Invalid {
            what: "package.tr",
            detail: format!("tar entry read: {e}"),
        })?;
        let path = entry
            .path()
            .map_err(|e| Error::Invalid {
                what: "package.tr",
                detail: format!("tar entry path: {e}"),
            })?
            .to_string_lossy()
            .into_owned();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(|e| Error::Invalid {
            what: "package.tr",
            detail: format!("tar entry body: {e}"),
        })?;

        match path.as_str() {
            MANIFEST_NAME => manifest_bytes = Some(buf),
            SOURCE_BUNDLE_NAME => source_bytes = Some(buf),
            CLAIMS_NAME => claims_bytes = Some(buf),
            SIGNATURE_NAME => signature_bytes = Some(buf),
            // Unknown entries are forward-compatible: a future
            // `tr/3.1` could ship optional sidecars without breaking
            // current readers. We log + skip rather than reject.
            other => {
                tracing::debug!(entry = %other, "skipping unknown entry in v3 pack");
            }
        }
    }

    let manifest_bytes = manifest_bytes.ok_or(Error::Invalid {
        what: "package.tr",
        detail: format!("missing required entry `{MANIFEST_NAME}`"),
    })?;
    let source_archive = source_bytes.ok_or(Error::Invalid {
        what: "package.tr",
        detail: format!("missing required entry `{SOURCE_BUNDLE_NAME}`"),
    })?;
    let claims_jsonl = claims_bytes.ok_or(Error::Invalid {
        what: "package.tr",
        detail: format!("missing required entry `{CLAIMS_NAME}`"),
    })?;

    let manifest = ManifestV3::parse(&manifest_bytes)?;

    let signature = match signature_bytes {
        Some(bytes) => Some(serde_json::from_slice::<SigstoreBundle>(&bytes).map_err(|e| {
            Error::Invalid {
                what: "signature.sig",
                detail: format!("Sigstore bundle parse: {e}"),
            }
        })?),
        None => None,
    };

    Ok(V3Pack {
        manifest,
        source_archive,
        claims_jsonl,
        signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClaimRecord, V3PackBuilder};
    use ed25519_dalek::SigningKey;
    use semver::Version;

    fn fixture_signing_key(seed: u8) -> SigningKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        SigningKey::from_bytes(&bytes)
    }

    fn fixture_pack_signed(seed: u8) -> Vec<u8> {
        let mut b = V3PackBuilder::new(ManifestV3::new(
            "alice/reader-test",
            Version::parse("1.0.0").unwrap(),
        ));
        b.add_source_file("hello.md", b"# Hello\n").unwrap();
        b.add_claim(ClaimRecord::new(
            "c-1",
            "Greeting",
            vec!["Hello".into()],
            "hello.md",
            0,
            8,
        ));
        b.build_signed(&fixture_signing_key(seed), "package.tr")
            .unwrap()
    }

    fn fixture_pack_unsigned() -> Vec<u8> {
        let mut b = V3PackBuilder::new(ManifestV3::new(
            "alice/reader-test",
            Version::parse("1.0.0").unwrap(),
        ));
        b.add_source_file("hello.md", b"# Hello\n").unwrap();
        b.add_claim(ClaimRecord::new(
            "c-1",
            "Greeting",
            vec!["Hello".into()],
            "hello.md",
            0,
            8,
        ));
        b.build().unwrap()
    }

    #[test]
    fn read_signed_pack_returns_all_four_components() {
        let bytes = fixture_pack_signed(1);
        let pack = read_v3_pack(&bytes).unwrap();
        assert_eq!(pack.manifest.name, "alice/reader-test");
        assert!(pack.manifest.pack_hash.starts_with("blake3:"));
        assert!(!pack.source_archive.is_empty());
        assert!(!pack.claims_jsonl.is_empty());
        assert!(pack.signature.is_some(), "signed pack carries signature");
    }

    #[test]
    fn read_unsigned_pack_has_no_signature() {
        let bytes = fixture_pack_unsigned();
        let pack = read_v3_pack(&bytes).unwrap();
        assert!(pack.signature.is_none());
    }

    #[test]
    fn recompute_pack_hash_matches_manifest_declaration() {
        let bytes = fixture_pack_signed(2);
        let pack = read_v3_pack(&bytes).unwrap();
        assert_eq!(pack.recompute_pack_hash(), pack.manifest.pack_hash);
    }

    #[test]
    fn missing_manifest_is_rejected() {
        // Build a tar that has source + claims but no manifest. We do
        // this by hand because V3PackBuilder always emits all three.
        use std::io::Cursor;
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut tar_b = tar::Builder::new(cursor);
            tar_b.mode(tar::HeaderMode::Deterministic);
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_cksum();
            tar_b
                .append_data(&mut h, SOURCE_BUNDLE_NAME, &b"abcd"[..])
                .unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_cksum();
            tar_b
                .append_data(&mut h, CLAIMS_NAME, &b"ef\n\n"[..])
                .unwrap();
            tar_b.finish().unwrap();
        }

        let err = read_v3_pack(&buf).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains(MANIFEST_NAME),
            "error must mention the missing entry: {msg}"
        );
    }

    #[test]
    fn pack_hash_chain_detects_tampering() {
        let bytes = fixture_pack_signed(3);
        let mut tampered = bytes.clone();
        // Find the claims.jsonl entry inside the outer tar and flip a
        // byte in its body. Naïve search — works for these small
        // synthetic packs.
        let needle = b"\"id\":\"c-1\"";
        let pos = tampered
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("test fixture must contain claim id");
        tampered[pos + needle.len() - 1] ^= 0x01;

        // The reader still parses (we corrupted bytes, not framing).
        let pack = read_v3_pack(&tampered).unwrap();
        // But the recomputed hash now diverges from the manifest's
        // declared hash — which is exactly what the v3 verifier
        // checks.
        assert_ne!(
            pack.recompute_pack_hash(),
            pack.manifest.pack_hash,
            "byte tampering must surface as a hash divergence"
        );
    }
}
