//! v3 pack reader — parse a `package.tr` (the 3-file outer tar layout)
//! back into typed structures.
//!
//! The reader does **not** uncompress the source bundle — it returns
//! `source.tar.zst` as opaque bytes alongside the manifest and claims.
//! Consumers that need the source files (`root verify`, the v3 reader
//! half of `root install`) then do their own zstd-decode + tar walk
//! over the inner archive. Keeping the inner source bundle as raw
//! bytes preserves the BLAKE3 hash chain — the pack-hash recipe
//! (`docs/superpowers/specs/2026-05-10-witness-mesh-design.md`,
//! tr/3.2 pack section) consumes the inner bundle byte-for-byte.

use std::io::{Cursor, Read};

use crate::{
    error::{Error, Result},
    manifest::ManifestV3,
    writer_v3::{
        CLAIMS_NAME, MANIFEST_NAME, PAPER_NAME, RULE_CATALOG_NAME, SIGNATURE_NAME,
        SOURCE_BUNDLE_NAME, WITNESSES_NAME,
    },
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
    /// Raw `witnesses.cbor` payload — canonical-CBOR-encoded
    /// `Vec<WitnessRecord>` sorted by `id` ASC. `Some` only on
    /// `tr/3.2` packs. Consumers needing typed witness rows decode
    /// via `ciborium::from_reader::<Vec<WitnessRecord>>`.
    pub witnesses_cbor: Option<Vec<u8>>,
    /// Raw `rule_catalog.toml` payload — verbatim catalog the
    /// witnesses were derived under. `Some` only on `tr/3.2` packs
    /// carrying witnesses. Pinning the catalog into the pack lets
    /// `tr-verify` reject witnesses whose rule names don't exist in
    /// the shipped catalog (I-W10 / I-W11 contract).
    pub rule_catalog_toml: Option<Vec<u8>>,
    /// Raw `paper.md` payload — Living Paper artefact, UTF-8
    /// markdown with YAML frontmatter. `Some` only on `tr/3.2` packs
    /// that shipped a per-compile paper. Renderers and the desktop
    /// install preview consume this byte-for-byte; tampering is
    /// detected by recomputing `pack_hash`.
    pub paper_md: Option<Vec<u8>>,
}

impl V3Pack {
    /// Recompute the BLAKE3 pack hash from this pack's components per
    /// spec §3.1 (v3 / v3.1) and the witness-mesh design (v3.2).
    /// Returns `blake3:<hex>` matching the format the manifest's
    /// `pack_hash` field uses. The verifier asserts this equals
    /// `manifest.pack_hash` to detect post-sign tampering.
    ///
    /// Canonical bytes recipe (matches `writer_v3::prepare_canonical`):
    /// ```text
    /// manifest_with_pack_hash_blanked || NUL || source.tar.zst
    ///   || NUL || claims.jsonl
    ///   [|| NUL || witnesses.cbor || NUL || rule_catalog.toml]
    ///   [|| NUL || paper.md]
    /// ```
    ///
    /// Each optional member is chained iff the corresponding `V3Pack`
    /// field is `Some`. v3 / v3.1 packs leave them `None` and skip
    /// the trailing legs; v3.2 packs chain the legs whose bodies
    /// were present in the outer tar.
    pub fn recompute_pack_hash(&self) -> String {
        let canonical = {
            let mut clone = self.manifest.clone();
            clone.pack_hash = String::new();
            clone.to_canonical_toml()
        };
        let mut input = Vec::with_capacity(
            canonical.len()
                + 1
                + self.source_archive.len()
                + 1
                + self.claims_jsonl.len()
                + self.witnesses_cbor.as_ref().map_or(0, |b| b.len() + 1)
                + self
                    .rule_catalog_toml
                    .as_ref()
                    .map_or(0, |b| b.len() + 1)
                + self.paper_md.as_ref().map_or(0, |b| b.len() + 1),
        );
        input.extend_from_slice(&canonical);
        input.push(0);
        input.extend_from_slice(&self.source_archive);
        input.push(0);
        input.extend_from_slice(&self.claims_jsonl);
        if let Some(w) = self.witnesses_cbor.as_ref() {
            input.push(0);
            input.extend_from_slice(w);
        }
        if let Some(c) = self.rule_catalog_toml.as_ref() {
            input.push(0);
            input.extend_from_slice(c);
        }
        if let Some(p) = self.paper_md.as_ref() {
            input.push(0);
            input.extend_from_slice(p);
        }
        format!("blake3:{}", crate::digest::blake3_hex(&input))
    }
}

/// Default in-memory size cap for `read_v3_pack`.  Matches the v3 spec
/// §6.4 single-pack ceiling and the HTTP resolver's default
/// `max_pack_bytes`.  Callers that need a different limit (e.g. a
/// registry that advertises a higher cap in its discovery doc) should
/// invoke [`read_v3_pack_with_cap`] directly.
pub const DEFAULT_PACK_SIZE_CAP_BYTES: u64 = 100 * 1024 * 1024;

/// Read a v3 pack from raw outer-tar bytes, refusing archives larger
/// than [`DEFAULT_PACK_SIZE_CAP_BYTES`].
///
/// Pre-fix this function had no in-memory size cap and the local-install
/// path (`LocalFsResolver` → `read_v3_pack`) inherited the defect:
/// pointing `root install ./100gb.tr` at a hostile or accidentally-large
/// file would `Vec`-grow until the process OOM'd, before any signature
/// or hash check could run.  The HTTP resolver enforces its own ceiling
/// (`http.rs:154-186`) so that path was already safe; this guards every
/// remaining caller via the format crate itself.
///
/// Errors:
/// - `Error::TooLarge` when `bytes.len()` exceeds the default cap.
/// - `Error::Invalid` when any of the three required entries
///   (`manifest.toml`, `source.tar.zst`, `claims.jsonl`) is missing.
/// - `Error::Invalid` when `manifest.toml` fails parse (delegated to
///   [`ManifestV3::parse`]).
/// - `Error::Invalid` when `signature.sig` is present but isn't a
///   valid Sigstore Bundle JSON.
pub fn read_v3_pack(bytes: &[u8]) -> Result<V3Pack> {
    read_v3_pack_with_cap(bytes, DEFAULT_PACK_SIZE_CAP_BYTES)
}

/// Read a v3 pack from raw outer-tar bytes with an explicit size cap.
///
/// Use when a caller has its own policy for the maximum pack size
/// (e.g. a registry advertising a higher `max_pack_bytes` in its
/// discovery document).  Returns [`Error::TooLarge`] when `bytes.len()`
/// exceeds `max_bytes`.
pub fn read_v3_pack_with_cap(bytes: &[u8], max_bytes: u64) -> Result<V3Pack> {
    let actual = bytes.len() as u64;
    if actual > max_bytes {
        return Err(Error::TooLarge {
            cap: max_bytes,
            actual,
        });
    }
    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut source_bytes: Option<Vec<u8>> = None;
    let mut claims_bytes: Option<Vec<u8>> = None;
    let mut signature_bytes: Option<Vec<u8>> = None;
    let mut witnesses_cbor: Option<Vec<u8>> = None;
    let mut rule_catalog_toml: Option<Vec<u8>> = None;
    let mut paper_md: Option<Vec<u8>> = None;

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
            WITNESSES_NAME => witnesses_cbor = Some(buf),
            RULE_CATALOG_NAME => rule_catalog_toml = Some(buf),
            PAPER_NAME => paper_md = Some(buf),
            // Unknown entries are forward-compatible: a future
            // wire-format bump could ship optional sidecars without
            // breaking current readers. We log + skip rather than reject.
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

    // v3.2 packs that carry witnesses MUST also carry the rule catalog
    // (and vice versa) — `tr-verify` cannot validate Witness Mesh
    // rule references without the catalog, and a catalog without
    // witnesses serves no purpose. Refuse half-pairs at read time
    // rather than letting them slip through to a confused verifier.
    match (witnesses_cbor.as_ref(), rule_catalog_toml.as_ref()) {
        (Some(_), None) => {
            return Err(Error::Invalid {
                what: "package.tr",
                detail: format!(
                    "v3.2 pack has `{WITNESSES_NAME}` but is missing `{RULE_CATALOG_NAME}`"
                ),
            });
        }
        (None, Some(_)) => {
            return Err(Error::Invalid {
                what: "package.tr",
                detail: format!(
                    "v3.2 pack has `{RULE_CATALOG_NAME}` but is missing `{WITNESSES_NAME}`"
                ),
            });
        }
        _ => {}
    }

    let signature = match signature_bytes {
        Some(bytes) => Some(
            serde_json::from_slice::<SigstoreBundle>(&bytes).map_err(|e| Error::Invalid {
                what: "signature.sig",
                detail: format!("Sigstore bundle parse: {e}"),
            })?,
        ),
        None => None,
    };

    Ok(V3Pack {
        manifest,
        source_archive,
        claims_jsonl,
        signature,
        witnesses_cbor,
        rule_catalog_toml,
        paper_md,
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
    fn read_v3_pack_with_cap_rejects_oversized_input() {
        // Regression: pre-fix `read_v3_pack` had no size cap and a
        // local-install of a 100GB file (hostile or accidental) would
        // OOM the process before any verification could run.  We cap
        // the input bytes themselves up-front; downstream parsing
        // never sees a Vec larger than the configured ceiling.
        let bytes = fixture_pack_signed(7);
        let actual = bytes.len() as u64;
        // Pick a cap one byte below `actual` so the fixture trips the guard.
        let tight_cap = actual - 1;
        let err = read_v3_pack_with_cap(&bytes, tight_cap)
            .expect_err("read_v3_pack_with_cap must refuse input > cap");
        match err {
            Error::TooLarge { cap, actual: a } => {
                assert_eq!(cap, tight_cap);
                assert_eq!(a, actual);
            }
            other => panic!("expected Error::TooLarge, got {other:?}"),
        }

        // And the same input under a generous cap parses fine.
        let pack = read_v3_pack_with_cap(&bytes, actual + 1)
            .expect("input ≤ cap parses normally");
        assert!(pack.signature.is_some());
    }

    #[test]
    fn read_v3_pack_default_cap_accepts_realistic_pack() {
        // The default cap (100 MiB) must accept any realistic synthetic
        // fixture this test crate produces.  Pin the relationship so a
        // future bump that raises the cap still works.
        let bytes = fixture_pack_signed(8);
        assert!((bytes.len() as u64) < DEFAULT_PACK_SIZE_CAP_BYTES);
        let pack = read_v3_pack(&bytes).expect("default cap must accept fixture");
        assert!(pack.signature.is_some());
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

    // ── v3.2 reader expansion (witnesses + rule catalog + paper) ────

    fn fixture_witness_record(id_seed: u8) -> crate::WitnessRecord {
        crate::WitnessRecord {
            id: format!("{:0>64}", format!("a{id_seed:x}")),
            witness_type: "declares::function".into(),
            rule: "tree-sitter::function-decl@v1".into(),
            inputs: vec![crate::WitnessRecordInput::Bytes {
                file: "hello.md".into(),
                start: 0,
                end: 8,
            }],
            spans: vec![crate::WitnessRecordSpan {
                file: "hello.md".into(),
                start: 0,
                end: 8,
            }],
            content_blake3: format!("{:0>64}", "deadbeef"),
            symbol: Some("hello".into()),
            sensitivity: "Public".into(),
            confidence: 0.99,
        }
    }

    fn fixture_v32_pack(with_witnesses: bool, with_paper: bool) -> Vec<u8> {
        let mut b = V3PackBuilder::new(ManifestV3::new(
            "alice/v32-reader-test",
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
        if with_witnesses {
            b.add_witness(fixture_witness_record(1));
            b.add_witness(fixture_witness_record(2));
            b = b.with_rule_catalog_toml("catalog_version = \"1.0.0\"\n[rules]\n");
        }
        if with_paper {
            b = b.with_paper(
                "---\npaper_version: 1\nworkspace: v32-reader-test\n---\n\n# Living Paper\n\nHello.\n",
            );
        }
        b.build().unwrap()
    }

    #[test]
    fn v32_read_round_trip_returns_witnesses_catalog_and_paper() {
        let bytes = fixture_v32_pack(true, true);
        let pack = read_v3_pack(&bytes).unwrap();

        assert_eq!(pack.manifest.format_version, crate::FORMAT_VERSION_V32);
        assert!(
            pack.witnesses_cbor.is_some(),
            "v3.2 pack must surface witnesses.cbor through the reader"
        );
        assert!(
            pack.rule_catalog_toml.is_some(),
            "v3.2 pack must surface rule_catalog.toml through the reader"
        );
        assert!(
            pack.paper_md.is_some(),
            "v3.2 pack with paper.md must surface paper through the reader"
        );

        let paper = std::str::from_utf8(pack.paper_md.as_ref().unwrap()).unwrap();
        assert!(paper.starts_with("---\npaper_version: 1"));
        assert!(paper.contains("# Living Paper"));
    }

    #[test]
    fn v32_recompute_pack_hash_chains_v32_members() {
        // The v3.2 canonical chain must include witnesses + catalog +
        // paper — otherwise a reader's recompute and the writer's
        // pack_hash diverge by construction. Pin the symmetry here.
        let bytes = fixture_v32_pack(true, true);
        let pack = read_v3_pack(&bytes).unwrap();
        assert_eq!(
            pack.recompute_pack_hash(),
            pack.manifest.pack_hash,
            "v3.2 reader's recomputed pack_hash must match the writer's declared pack_hash"
        );
    }

    #[test]
    fn v32_paper_only_round_trip_excludes_witnesses() {
        // A pack may ship a Living Paper without witnesses (e.g. a
        // prose-only workspace where no extractor fired). The reader
        // must surface the paper and leave witness fields None.
        let bytes = fixture_v32_pack(false, true);
        let pack = read_v3_pack(&bytes).unwrap();

        assert!(pack.witnesses_cbor.is_none());
        assert!(pack.rule_catalog_toml.is_none());
        assert!(pack.paper_md.is_some());
        assert_eq!(pack.manifest.format_version, crate::FORMAT_VERSION_V32);
        assert_eq!(pack.recompute_pack_hash(), pack.manifest.pack_hash);
    }

    #[test]
    fn v32_paper_tampering_is_detected_by_pack_hash() {
        let bytes = fixture_v32_pack(false, true);
        let mut tampered = bytes.clone();
        // Flip one byte inside the paper body.
        let needle = b"Hello.";
        let pos = tampered
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("fixture paper must contain the marker");
        tampered[pos] ^= 0x01;

        let pack = read_v3_pack(&tampered).unwrap();
        assert_ne!(
            pack.recompute_pack_hash(),
            pack.manifest.pack_hash,
            "paper.md tampering must surface as a hash divergence"
        );
    }

    #[test]
    fn v32_reader_refuses_witnesses_without_catalog() {
        // Build a tar manually that has witnesses but no rule_catalog.
        // This is the half-pair forbidden state — the reader's job is
        // to refuse it before tr-verify gets a confusing input.
        use crate::ManifestV3;
        let mut b = V3PackBuilder::new(ManifestV3::new(
            "alice/half-pair",
            Version::parse("1.0.0").unwrap(),
        ));
        b.add_source_file("hello.md", b"# Hello\n").unwrap();
        b.add_witness(fixture_witness_record(5));
        b = b.with_rule_catalog_toml("catalog_version = \"1.0.0\"\n[rules]\n");
        let valid = b.build().unwrap();

        // Strip the rule_catalog.toml entry from the outer tar.
        let stripped = {
            let mut out = Vec::new();
            {
                let cursor = std::io::Cursor::new(&mut out);
                let mut tar_b = tar::Builder::new(cursor);
                tar_b.mode(tar::HeaderMode::Deterministic);
                let mut archive = tar::Archive::new(std::io::Cursor::new(&valid));
                for entry in archive.entries().unwrap() {
                    let mut entry = entry.unwrap();
                    let path = entry.path().unwrap().to_string_lossy().into_owned();
                    if path == RULE_CATALOG_NAME {
                        continue;
                    }
                    let mut body = Vec::new();
                    std::io::Read::read_to_end(&mut entry, &mut body).unwrap();
                    let mut header = tar::Header::new_gnu();
                    header.set_size(body.len() as u64);
                    header.set_mode(0o644);
                    header.set_cksum();
                    tar_b.append_data(&mut header, &path, &body[..]).unwrap();
                }
                tar_b.finish().unwrap();
            }
            out
        };

        let err = read_v3_pack(&stripped).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains(WITNESSES_NAME) && msg.contains(RULE_CATALOG_NAME),
            "error must point at the missing catalog: {msg}"
        );
    }
}
