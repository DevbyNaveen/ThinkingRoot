//! v3 pack writer — produces the 3-file `package.tr` layout per the
//! Witness Mesh spec (`docs/superpowers/specs/2026-05-10-witness-mesh-design.md`,
//! tr/3.2 section, which extends the v3 base format):
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
//! The BLAKE3 pack-hash recipe is locked here per the tr/3.2 spec
//! section of `docs/superpowers/specs/2026-05-10-witness-mesh-design.md`
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
use std::time::SystemTime;

use ed25519_dalek::SigningKey;
use tar::{Builder, Header, HeaderMode};
use tr_sigstore::SigstoreBundle;
use zstd::stream::write::Encoder as ZstdEncoder;

use crate::{
    claims::ClaimRecord,
    digest::blake3_hex,
    error::{Error, Result},
    manifest::ManifestV3,
};

/// Output of [`V3PackBuilder::prepare_canonical`] — the canonical pack
/// bytes plus everything `emit_outer_tar` needs to serialize the
/// final wire output. Internal to writer_v3 (not exposed): callers
/// drive [`V3PackBuilder::build`], [`V3PackBuilder::build_signed`], or
/// [`V3PackBuilder::build_with_signer`] and let those orchestrate.
struct CanonicalPack {
    /// The BLAKE3 input for `pack_hash` per spec §3.1. For v3 / v3.1:
    /// `canonical_manifest_with_blank_pack_hash || NUL ||
    /// source.tar.zst || NUL || claims.jsonl`. For v3.2: extended
    /// with `|| NUL || witnesses.cbor || NUL || rule_catalog.toml`.
    /// Surfaced to `build_with_signer`'s closure so signers can
    /// compute their own subject digests over the same canonical
    /// bytes. The Witness Mesh members are appended in this fixed
    /// order so the recipe is invariant across runs.
    canonical_bytes: Vec<u8>,
    /// `manifest.toml` with `pack_hash` populated — what lands in the
    /// outer tar entry 1.
    manifest_toml: Vec<u8>,
    /// Inner source archive — outer tar entry 2.
    source_tar_zst: Vec<u8>,
    /// JSONL claims body — outer tar entry 3.
    claims_jsonl: Vec<u8>,
    /// `blake3:<hex>`-prefixed pack hash. Same as the corresponding
    /// field of `manifest_toml`'s `pack_hash` line, but pre-extracted
    /// to save consumers a TOML reparse.
    pack_hash: String,
    /// `witnesses.cbor` body for v3.2 packs. `None` for v3 / v3.1.
    /// CBOR-canonical encoding of the id-sorted witness vec.
    witnesses_cbor: Option<Vec<u8>>,
    /// `rule_catalog.toml` body for v3.2 packs. `None` for v3 / v3.1.
    /// The verbatim TOML the builder was staged with via
    /// `with_rule_catalog_toml`.
    rule_catalog_toml: Option<Vec<u8>>,
}

/// Path of the inner source bundle inside the outer `.tr` tar.
pub const SOURCE_BUNDLE_NAME: &str = "source.tar.zst";
/// Path of the manifest inside the outer `.tr` tar.
pub const MANIFEST_NAME: &str = "manifest.toml";
/// Path of the claim journal inside the outer `.tr` tar.
pub const CLAIMS_NAME: &str = "claims.jsonl";
/// Path of the Sigstore bundle inside the outer `.tr` tar — present
/// when the pack was built via [`V3PackBuilder::build_signed`].
pub const SIGNATURE_NAME: &str = "signature.sig";
/// Path of the Witness Mesh CBOR member inside the outer `.tr` tar —
/// present only in v3.2 packs.
pub const WITNESSES_NAME: &str = "witnesses.cbor";
/// Path of the rule catalog inside the outer `.tr` tar — present only
/// in v3.2 packs alongside `witnesses.cbor`.
pub const RULE_CATALOG_NAME: &str = "rule_catalog.toml";

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
    /// Witness Mesh witnesses staged for the `tr/3.2` writer. Empty
    /// for v3 / v3.1 packs. Sorted by `id` at build time so the CBOR
    /// emit is byte-deterministic.
    witnesses: Vec<crate::WitnessRecord>,
    /// Optional `rule_catalog.toml` body for `tr/3.2` packs. When
    /// present, its BLAKE3 is recorded in
    /// `manifest.derived_hashes[].kind = "rule_catalog.toml.blake3"`
    /// so a tampered catalog fails `tr-verify`.
    rule_catalog_toml: Option<String>,
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
            witnesses: Vec::new(),
            rule_catalog_toml: None,
        }
    }

    /// Append a single Witness Mesh witness record. Order doesn't
    /// matter — `build()` sorts by id ASC before CBOR-encoding so
    /// the resulting `witnesses.cbor` is byte-deterministic across
    /// processes. Triggers v3.2 wire-format on `build()`.
    pub fn add_witness(&mut self, record: crate::WitnessRecord) {
        self.witnesses.push(record);
    }

    /// Append many Witness Mesh witnesses. Same ordering contract
    /// as [`Self::add_witness`].
    pub fn extend_witnesses(
        &mut self,
        records: impl IntoIterator<Item = crate::WitnessRecord>,
    ) {
        self.witnesses.extend(records);
    }

    /// Stage the rule catalog body that produced these witnesses.
    /// Must be the canonical TOML output of
    /// `thinkingroot_extract::rule_catalog::rule_catalog_toml()` so
    /// the BLAKE3 recorded in `manifest.derived_hashes` matches
    /// what a consumer recomputes at read time. Required for v3.2
    /// packs that ship witnesses.
    pub fn with_rule_catalog_toml(mut self, toml: impl Into<String>) -> Self {
        self.rule_catalog_toml = Some(toml.into());
        self
    }

    /// True iff this builder will emit a v3.2 pack rather than a
    /// v3 / v3.1 pack. v3.2 mode activates when either witnesses
    /// have been staged or a rule catalog has been attached — both
    /// are required by `build()` once either is present.
    pub fn is_v32(&self) -> bool {
        !self.witnesses.is_empty() || self.rule_catalog_toml.is_some()
    }

    /// Encode the staged witnesses as canonical CBOR. The output is
    /// byte-deterministic: witnesses are sorted by `id` ascending
    /// before encoding, and `ciborium`'s default writer emits
    /// canonical CBOR (sorted map keys, shortest int form, no
    /// indefinite-length items). Same input vec → same bytes
    /// across processes.
    ///
    /// `pub(crate)` so the v3.2 emission path can call it without
    /// exposing CBOR internals on the public surface.
    pub(crate) fn encode_witnesses_cbor(&self) -> Result<Vec<u8>> {
        let mut sorted = self.witnesses.clone();
        sorted.sort_by(|a, b| a.id.cmp(&b.id));
        let mut buf: Vec<u8> = Vec::new();
        ciborium::into_writer(&sorted, &mut buf).map_err(|e| {
            Error::Invalid {
                what: "witnesses.cbor",
                detail: format!("ciborium encode failed: {e}"),
            }
        })?;
        Ok(buf)
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
        let prepared = self.prepare_canonical()?;
        emit_outer_tar(
            &prepared.manifest_toml,
            &prepared.source_tar_zst,
            &prepared.claims_jsonl,
            None,
            prepared.witnesses_cbor.as_deref(),
            prepared.rule_catalog_toml.as_deref(),
        )
    }

    /// Run the canonical-bytes prep (steps 1–5 of the pack-build recipe
    /// per spec §3.1) and return everything a downstream signer or tar
    /// emitter needs:
    ///
    /// - `canonical_bytes` — the BLAKE3 input for `pack_hash`, namely
    ///   `canonical_manifest_with_blank_pack_hash || NUL ||
    ///   source.tar.zst || NUL || claims.jsonl`. This is what every v3
    ///   signer (Ed25519 self-signed and Sigstore-keyless DSSE both)
    ///   covers.
    /// - `manifest_toml` — same manifest but with `pack_hash` populated;
    ///   this is the `manifest.toml` that lands in the outer tar.
    /// - `source_tar_zst`, `claims_jsonl` — outer tar payload entries
    ///   2 and 3.
    /// - `pack_hash` — the `blake3:<hex>`-prefixed digest matching
    ///   `manifest.pack_hash`.
    ///
    /// Shared by [`Self::build`], [`Self::build_signed`] (Ed25519
    /// self-signed), and [`Self::build_with_signer`] (Sigstore-keyless
    /// DSSE) so the canonicalization rule has one definition. Locking
    /// this rule was D7 of the v3 implementation plan; the golden-bytes
    /// test in `tests/v3_golden.rs` is what pins it.
    fn prepare_canonical(&mut self) -> Result<CanonicalPack> {
        let source_tar_zst = self.build_inner_source_archive()?;
        let source_hash = format!("blake3:{}", blake3_hex(&source_tar_zst));

        self.claims.sort_by(|a, b| a.id.cmp(&b.id));
        let claims_jsonl = self.serialize_claims_jsonl()?;
        let claims_hash = format!("blake3:{}", blake3_hex(&claims_jsonl));

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

        // ── Witness Mesh v3.2 extension ───────────────────────────
        // Encode witnesses + record rule catalog; both bodies feed
        // into the canonical bytes recipe AND surface as new
        // `derived_hashes` entries so `tr-verify` can re-check them
        // independently of the pack hash.
        let (witnesses_cbor, rule_catalog_toml_bytes) = if self.is_v32() {
            let witnesses = self.encode_witnesses_cbor()?;
            let catalog_str = self.rule_catalog_toml.clone().ok_or_else(|| {
                Error::Invalid {
                    what: "rule_catalog.toml",
                    detail: "v3.2 pack requires a rule catalog; call \
                             `V3PackBuilder::with_rule_catalog_toml` before `build`"
                        .into(),
                }
            })?;
            let catalog = catalog_str.into_bytes();
            // Bump format version to v3.2. The witness + catalog
            // bodies' BLAKE3 are implicitly covered by `pack_hash`
            // via the canonical_bytes recipe extension below — no
            // separate `derived_hashes` entries are needed for tamper
            // detection. (The top-level `ManifestV3` does not carry
            // a pack-wide `derived_hashes` field today; the
            // per-`SourceEntry` `derived_hashes` is unrelated to
            // pack-level Witness Mesh artefacts.)
            self.manifest.format_version = crate::FORMAT_VERSION_V32.to_string();
            (Some(witnesses), Some(catalog))
        } else {
            (None, None)
        };

        let canonical_manifest = self.manifest.canonical_bytes_for_hashing();
        // Canonical bytes recipe:
        //   manifest || NUL || source.tar.zst || NUL || claims.jsonl
        // v3.2 extension (append in fixed order so the recipe is
        // invariant across runs):
        //   || NUL || witnesses.cbor || NUL || rule_catalog.toml
        let mut canonical_bytes = Vec::with_capacity(
            canonical_manifest.len()
                + 1
                + source_tar_zst.len()
                + 1
                + claims_jsonl.len()
                + witnesses_cbor.as_ref().map_or(0, |b| b.len() + 1)
                + rule_catalog_toml_bytes
                    .as_ref()
                    .map_or(0, |b| b.len() + 1),
        );
        canonical_bytes.extend_from_slice(&canonical_manifest);
        canonical_bytes.push(0);
        canonical_bytes.extend_from_slice(&source_tar_zst);
        canonical_bytes.push(0);
        canonical_bytes.extend_from_slice(&claims_jsonl);
        if let Some(ref w) = witnesses_cbor {
            canonical_bytes.push(0);
            canonical_bytes.extend_from_slice(w);
        }
        if let Some(ref c) = rule_catalog_toml_bytes {
            canonical_bytes.push(0);
            canonical_bytes.extend_from_slice(c);
        }
        let pack_hash = format!("blake3:{}", blake3_hex(&canonical_bytes));
        self.manifest.pack_hash = pack_hash.clone();

        let manifest_toml = self.manifest.to_canonical_toml();

        Ok(CanonicalPack {
            canonical_bytes,
            manifest_toml,
            source_tar_zst,
            claims_jsonl,
            pack_hash,
            witnesses_cbor,
            rule_catalog_toml: rule_catalog_toml_bytes,
        })
    }

    /// Build a signed pack. Same as [`build`] except the supplied
    /// Ed25519 signing key authenticates a Sigstore Bundle which is
    /// emitted as the 4th outer-tar entry `signature.sig`.
    ///
    /// The signing key is ephemeral from this function's perspective —
    /// it's never persisted by `tr-format`. Production callers wire
    /// `root pack --sign` to a Fulcio-issued ephemeral keypair (Week
    /// 3.5 work, behind the `sigstore-impl` feature on `tr-verify`);
    /// today's tests + power users supply their own [`SigningKey`] via
    /// `SigningKey::generate(&mut rand::rngs::OsRng)`.
    ///
    /// `pack_filename` lands in the bundle's in-toto statement
    /// (`subject[0].name`) so verifiers can sanity-check the bundle is
    /// signing the expected file.
    pub fn build_signed(
        mut self,
        signing_key: &SigningKey,
        pack_filename: &str,
    ) -> Result<Vec<u8>> {
        let prepared = self.prepare_canonical()?;
        // The bundle binds the BLAKE3 digest into a DSSE-signed in-toto
        // statement; a verifier replays the chain offline to prove
        // (a) the bundle's signature is valid for the declared key
        // and (b) the statement's subject digest matches the pack
        // hash recomputed from the outer tar's bytes.
        let bundle = tr_sigstore::sign_pack(
            &prepared.pack_hash,
            pack_filename,
            signing_key,
            SystemTime::now(),
        )
        .map_err(|e| Error::Invalid {
            what: "signature.sig",
            detail: format!("sigstore sign: {e}"),
        })?;
        let bundle_bytes = serde_json::to_vec(&bundle)?;
        emit_outer_tar(
            &prepared.manifest_toml,
            &prepared.source_tar_zst,
            &prepared.claims_jsonl,
            Some(&bundle_bytes),
            prepared.witnesses_cbor.as_deref(),
            prepared.rule_catalog_toml.as_deref(),
        )
    }

    /// Build a signed pack via a caller-supplied closure. The closure
    /// receives the canonical pack bytes (the BLAKE3-input bytes spec
    /// §3.1 specifies for `pack_hash`), the formatted `pack_hash`
    /// string (e.g. `"blake3:abc..."`), and the `pack_filename` to
    /// embed in the bundle's in-toto statement; it returns a
    /// [`SigstoreBundle`] which is appended to the outer tar as
    /// `signature.sig`.
    ///
    /// This is the integration point for Sigstore-keyless DSSE
    /// signing: callers supply a closure that drives
    /// `tr_sigstore::live::sign_canonical_bytes_keyless` (Fulcio cert
    /// request → DSSE PAE sign → Rekor witness). The closure can also
    /// route through any other signer that produces a v3-compatible
    /// bundle (Ed25519 self-signed, HSM-backed, KMS, etc.) without
    /// `tr-format` taking a dependency on each signer's transitive
    /// stack.
    ///
    /// `E` is the closure's error type — propagated as
    /// [`Error::Invalid`] with `what="signature.sig"`. Use
    /// [`std::convert::Infallible`] when the signer cannot fail
    /// (callers wrapping a sync stable signer).
    pub fn build_with_signer<F, E>(mut self, sign_fn: F, pack_filename: &str) -> Result<Vec<u8>>
    where
        F: FnOnce(&[u8], &str, &str) -> std::result::Result<SigstoreBundle, E>,
        E: std::fmt::Display,
    {
        let prepared = self.prepare_canonical()?;
        let bundle = sign_fn(
            &prepared.canonical_bytes,
            &prepared.pack_hash,
            pack_filename,
        )
        .map_err(|e| Error::Invalid {
            what: "signature.sig",
            detail: format!("external signer: {e}"),
        })?;
        let bundle_bytes = serde_json::to_vec(&bundle)?;
        emit_outer_tar(
            &prepared.manifest_toml,
            &prepared.source_tar_zst,
            &prepared.claims_jsonl,
            Some(&bundle_bytes),
            prepared.witnesses_cbor.as_deref(),
            prepared.rule_catalog_toml.as_deref(),
        )
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
            let mut encoder =
                ZstdEncoder::new(&mut compressed, self.inner_zstd_level).map_err(|e| {
                    Error::Invalid {
                        what: "source.tar.zst",
                        detail: format!("zstd encoder: {e}"),
                    }
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

/// Emit the outer tar for a v3 pack. Used by both `build` (no
/// signature) and `build_signed` (4th entry `signature.sig`).
fn emit_outer_tar(
    manifest_toml: &[u8],
    source_tar_zst: &[u8],
    claims_jsonl: &[u8],
    signature_sig: Option<&[u8]>,
    witnesses_cbor: Option<&[u8]>,
    rule_catalog_toml: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut outer = Vec::with_capacity(
        manifest_toml.len()
            + source_tar_zst.len()
            + claims_jsonl.len()
            + signature_sig.map(<[u8]>::len).unwrap_or(0)
            + witnesses_cbor.map(<[u8]>::len).unwrap_or(0)
            + rule_catalog_toml.map(<[u8]>::len).unwrap_or(0)
            + 4096,
    );
    {
        let cursor = Cursor::new(&mut outer);
        let mut tar_builder = Builder::new(cursor);
        tar_builder.mode(HeaderMode::Deterministic);
        // Tar entry order is fixed for reproducibility:
        //   manifest.toml, source.tar.zst, claims.jsonl,
        //   [witnesses.cbor], [rule_catalog.toml],
        //   [signature.sig].
        // v3 packs stop at claims.jsonl; v3.2 inserts the Witness
        // Mesh members between claims.jsonl and signature.sig.
        append_file(&mut tar_builder, MANIFEST_NAME, manifest_toml)?;
        append_file(&mut tar_builder, SOURCE_BUNDLE_NAME, source_tar_zst)?;
        append_file(&mut tar_builder, CLAIMS_NAME, claims_jsonl)?;
        if let Some(w) = witnesses_cbor {
            append_file(&mut tar_builder, WITNESSES_NAME, w)?;
        }
        if let Some(c) = rule_catalog_toml {
            append_file(&mut tar_builder, RULE_CATALOG_NAME, c)?;
        }
        if let Some(sig) = signature_sig {
            append_file(&mut tar_builder, SIGNATURE_NAME, sig)?;
        }
        tar_builder.finish()?;
    }
    Ok(outer)
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

/// Path-safety contract: relative paths only, no `..`, no leading `/`.
/// The outer tar paths are validated here so we never emit a v3 pack
/// that a stricter reader would reject.
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

    fn sample_witness_record(id_byte: u8) -> crate::WitnessRecord {
        let id_hex = format!("{:0>64}", format!("{:02x}", id_byte));
        crate::WitnessRecord::new(
            id_hex,
            "declares::function",
            "tree-sitter::function-decl@v1",
            vec![crate::WitnessRecordInput::Bytes {
                file: "f".into(),
                start: 0,
                end: 5,
            }],
            vec![crate::WitnessRecordSpan {
                file: "f".into(),
                start: 0,
                end: 5,
            }],
            "1".repeat(64),
            "Public",
            0.99,
        )
    }

    #[test]
    fn is_v32_flips_when_witnesses_added() {
        let mut b = V3PackBuilder::new(fixture_manifest());
        assert!(!b.is_v32(), "fresh builder is v3/v3.1");
        b.add_witness(sample_witness_record(1));
        assert!(b.is_v32(), "adding a witness opts into v3.2");
    }

    #[test]
    fn is_v32_flips_when_rule_catalog_attached() {
        let b = V3PackBuilder::new(fixture_manifest())
            .with_rule_catalog_toml("catalog_version = \"1.0.0\"\n");
        assert!(b.is_v32());
    }

    #[test]
    fn encode_witnesses_cbor_is_deterministic() {
        let mut b1 = V3PackBuilder::new(fixture_manifest());
        b1.add_witness(sample_witness_record(0x10));
        b1.add_witness(sample_witness_record(0x20));
        let cbor1 = b1.encode_witnesses_cbor().unwrap();

        // Same witnesses staged in opposite order should produce
        // byte-identical CBOR (id-sorted before encoding).
        let mut b2 = V3PackBuilder::new(fixture_manifest());
        b2.add_witness(sample_witness_record(0x20));
        b2.add_witness(sample_witness_record(0x10));
        let cbor2 = b2.encode_witnesses_cbor().unwrap();

        assert_eq!(cbor1, cbor2, "CBOR encoding must be order-independent");
        assert!(!cbor1.is_empty(), "non-empty witness set produces non-empty CBOR");
    }

    #[test]
    fn encode_witnesses_cbor_empty_set_succeeds() {
        let b = V3PackBuilder::new(fixture_manifest());
        let cbor = b.encode_witnesses_cbor().unwrap();
        // An empty CBOR array is 1 byte (`0x80`); we accept any
        // ≤4-byte encoding (canonical CBOR rules) — the exact value
        // is `[0x80]` for `ciborium`.
        assert!(cbor.len() <= 4);
    }

    #[test]
    fn v32_build_requires_rule_catalog() {
        let mut b = V3PackBuilder::new(fixture_manifest());
        b.add_witness(sample_witness_record(0x42));
        // Witness staged but no rule catalog → build() must reject.
        let result = b.build();
        assert!(result.is_err(), "v3.2 build without catalog must error");
    }

    #[test]
    fn v32_build_emits_witnesses_and_catalog_tar_entries() {
        let mut b = V3PackBuilder::new(fixture_manifest())
            .with_rule_catalog_toml("catalog_version = \"1.0.0\"\n");
        b.add_witness(sample_witness_record(0x01));
        b.add_witness(sample_witness_record(0x02));
        let bytes = b.build().unwrap();

        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                e.path()
                    .ok()
                    .and_then(|p| p.to_str().map(String::from))
            })
            .collect();
        // v3.2 tar layout: manifest.toml, source.tar.zst,
        // claims.jsonl, witnesses.cbor, rule_catalog.toml.
        assert!(names.contains(&"manifest.toml".to_string()));
        assert!(names.contains(&"source.tar.zst".to_string()));
        assert!(names.contains(&"claims.jsonl".to_string()));
        assert!(
            names.contains(&"witnesses.cbor".to_string()),
            "v3.2 pack must include witnesses.cbor; got {names:?}"
        );
        assert!(
            names.contains(&"rule_catalog.toml".to_string()),
            "v3.2 pack must include rule_catalog.toml; got {names:?}"
        );
    }

    #[test]
    fn v32_pack_is_byte_deterministic() {
        let pack1 = {
            let mut b = V3PackBuilder::new(fixture_manifest())
                .with_rule_catalog_toml("catalog_version = \"1.0.0\"\n");
            b.add_witness(sample_witness_record(0x11));
            b.add_witness(sample_witness_record(0x22));
            b.build().unwrap()
        };
        let pack2 = {
            let mut b = V3PackBuilder::new(fixture_manifest())
                .with_rule_catalog_toml("catalog_version = \"1.0.0\"\n");
            // Same witnesses, opposite staging order.
            b.add_witness(sample_witness_record(0x22));
            b.add_witness(sample_witness_record(0x11));
            b.build().unwrap()
        };
        assert_eq!(
            pack1, pack2,
            "v3.2 pack bytes must be staging-order-invariant (sorted at build)"
        );
    }

    #[test]
    fn v32_pack_format_version_is_v32_in_manifest() {
        let mut b = V3PackBuilder::new(fixture_manifest())
            .with_rule_catalog_toml("catalog_version = \"1.0.0\"\n");
        b.add_witness(sample_witness_record(0xFF));
        let bytes = b.build().unwrap();
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().to_str() == Some("manifest.toml") {
                let mut content = String::new();
                use std::io::Read;
                entry.read_to_string(&mut content).unwrap();
                assert!(
                    content.contains("format_version = \"tr/3.2\""),
                    "manifest must declare tr/3.2: {content}"
                );
                return;
            }
        }
        panic!("manifest.toml not found in v3.2 pack");
    }
}
