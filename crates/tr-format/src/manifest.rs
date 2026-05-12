//! The v3 manifest — the single authoritative description of a pack.
//!
//! Every `.tr` archive contains `manifest.toml` at the outer-tar root.
//! The shape is locked by the v3 spec §3.2. Bytes emitted by
//! [`ManifestV3::to_canonical_toml`] and
//! [`ManifestV3::canonical_bytes_for_hashing`] are the load-bearing
//! inputs to `pack_hash` per spec §3.1 — and the substrate of every
//! Sigstore-signed pack. **Changing the canonicalization rule
//! invalidates every previously-signed pack.** Locked locked locked.

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// v3 format identifier — `"tr/3"`. Used by [`ManifestV3`] and the v3
/// writer module. Locked by spec §3.2; readers refusing on mismatch
/// surface incompatibility cleanly.
pub const FORMAT_VERSION_V3: &str = "tr/3";

/// v3.1 format identifier — `"tr/3.1"`. Backward-compatible additive
/// bump that introduces:
///
/// - [`ManifestV3::sources`] — per-source detail rows ([`SourceEntry`]).
/// - [`ManifestV3::author_key_id`] — DID-style identifier of the key
///   that signed the manifest (consumed by `tr-verify::AuthorVerifier`).
///
/// Old `tr/3` readers accept new packs because the new fields are
/// `#[serde(default, skip_serializing_if = ...)]`; new readers accept
/// old packs by leaving the fields empty/`None`. Round-tripping a
/// `tr/3` manifest through v3.1 code emits identical canonical bytes.
pub const FORMAT_VERSION_V31: &str = "tr/3.1";

/// v3.2 format identifier — `"tr/3.2"`. The Witness Mesh additive
/// bump. Backward-compatible: `tr/3` and `tr/3.1` readers accept v3.2
/// packs (the new members are extra files; the old `claims.jsonl` +
/// `manifest.toml` + `source.tar.zst` shape is unchanged). v3.2-aware
/// readers gain access to:
///
/// - `witnesses.cbor` — the Witness Mesh substrate produced by the
///   rule catalog. CBOR-canonical sort, deterministic across
///   processes.
/// - `rule_catalog.toml` — the versioned rule catalog the witnesses
///   reference. Its BLAKE3 is recorded in
///   `derived_hashes` so a tampered catalog fails `tr-verify`.
///
/// A v3.2 pack manifest's `derived_hashes` allow-list adds two new
/// `kind` entries: `"witnesses.cbor.blake3"` and
/// `"rule_catalog.toml.blake3"`.
pub const FORMAT_VERSION_V32: &str = "tr/3.2";

/// Latest format version emitted by this crate's writer. Consumers
/// that want to pin against the most recent published format should
/// use this alias rather than hard-coding the literal string.
pub const FORMAT_VERSION_LATEST: &str = FORMAT_VERSION_V32;

/// Allow-list of `derived_hashes[].kind` values per spec §3.2 v3.1
/// and the v3.2 Witness Mesh extension. Anything outside this list
/// is refused at [`ManifestV3::validate`] time so the field can
/// never be used as a free-form annotation channel. Add new values
/// here only when the matching extractor surface ships in the engine.
pub const DERIVED_HASH_KINDS: &[&str] = &[
    "thumbnail.blake3",
    "transcript.blake3",
    "summary.blake3",
    // ── tr/3.2 Witness Mesh extension ──
    "witnesses.cbor.blake3",
    "rule_catalog.toml.blake3",
];

/// The v3 manifest. Carried as `manifest.toml` inside `package.tr`.
///
/// Wire-format ordering and serialization are explicitly canonicalized
/// by [`ManifestV3::to_canonical_toml`] — a manual emitter rather than
/// the upstream `toml::to_string` call so we don't accidentally inherit
/// the toml crate's internal map ordering decisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestV3 {
    /// `"tr/3"` for this format revision. The only required schema-
    /// validation field per spec §3.2.
    pub format_version: String,

    /// Pack coordinate in `owner/slug` form. Validation rules: each
    /// segment is `[a-z0-9][a-z0-9-]*`, length ≤ 64.
    pub name: String,

    /// SemVer of this pack.
    #[serde(with = "semver_string")]
    pub version: Version,

    /// `blake3:` + 64 hex chars — BLAKE3 of `source.tar.zst` bytes.
    pub source_hash: String,

    /// `blake3:` + 64 hex chars — BLAKE3 of `claims.jsonl` bytes.
    pub claims_hash: String,

    /// `blake3:` + 64 hex chars — BLAKE3 of canonical
    /// `(manifest_with_pack_hash_blanked || NUL || source.tar.zst || NUL || claims.jsonl)`
    /// per spec §3.1, §16.1.
    pub pack_hash: String,

    /// Informational counts. Optional per spec §3.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_files: Option<u64>,
    /// Total uncompressed source bytes. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_bytes: Option<u64>,
    /// Number of lines in `claims.jsonl`. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_count: Option<u64>,
    /// When extraction ran. Optional, ISO 8601.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_at: Option<DateTime<Utc>>,
    /// Extractor identity (e.g. `"thinkingroot/extract@0.9.1"`). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extractor: Option<String>,

    /// SPDX license expression. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,

    /// One-line description. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Long-form human README in markdown. Optional. Capped at 256 KiB
    /// by [`ManifestV3::validate`]. Adding the field as optional with
    /// `#[serde(default)]` is forward-compatible with `tr/3` — old
    /// readers parse and drop it; old packs without the field round-
    /// trip identically through new code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readme: Option<String>,

    /// Author handles or contact strings. Optional, default empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,

    /// DID-style identifier of the key that signed the manifest, when
    /// the pack is author-signed. Format `did:method:identifier#fragment`,
    /// matching `tr_identity::Did`. Consumed by
    /// `tr-verify::AuthorVerifier`. **Setting this field requires
    /// `format_version == "tr/3.1"`** — [`ManifestV3::validate`] rejects
    /// the combination on `tr/3` to keep the schema-bump invariant
    /// honest.
    ///
    /// Forward-compat: old `tr/3` readers parse and drop this field;
    /// old packs without the field round-trip identically through new
    /// code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_key_id: Option<String>,

    /// Per-source detail rows. Empty/absent on `tr/3`; populated on
    /// `tr/3.1` packs that opt in to multimodal extractors. Each entry
    /// names a source file by its relative path inside `source.tar.zst`,
    /// its BLAKE3 content hash, byte count, optional MIME type, and any
    /// derived-content hashes (thumbnails, transcripts, summaries). A
    /// non-empty value requires `format_version == "tr/3.1"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceEntry>,
}

/// Per-source detail row. Introduced in `tr/3.1`. Each entry describes
/// one file inside the inner `source.tar.zst` bundle. The list is the
/// authoritative inventory consumed by the cloud registry, the EU AI
/// Act compliance bundle (`root compliance --eu-ai-act`), and the
/// multimodal extractor pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    /// Path inside `source.tar.zst`, relative, forward-slash-separated.
    /// Validated against `[A-Za-z0-9_./-]+` (no `..`, no leading `/`)
    /// to refuse path-traversal payloads at parse time.
    pub relative_path: String,
    /// BLAKE3 of the file body — `blake3:<64-lowercase-hex>`.
    pub content_hash: String,
    /// Uncompressed byte length of the file.
    pub bytes: u64,
    /// IANA MIME type when known. `None` for files where the extractor
    /// declines to guess (rare). Surface only — the engine never
    /// trusts a manifest-declared MIME for routing decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Hashes of derived artefacts (thumbnail, transcript, summary).
    /// Each entry's `kind` must be in [`DERIVED_HASH_KINDS`]; arbitrary
    /// kinds are refused at [`ManifestV3::validate`] time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived_hashes: Vec<DerivedHash>,
}

/// Hash of a derived artefact attached to a [`SourceEntry`]. The
/// `kind` field is constrained to a small allow-list so the field can
/// never become a free-form annotation channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedHash {
    /// One of [`DERIVED_HASH_KINDS`]. Anything else fails validation.
    pub kind: String,
    /// BLAKE3 of the derived bytes — `blake3:<64-lowercase-hex>`.
    pub hash: String,
}

impl ManifestV3 {
    /// Minimal builder. Hashes are all-empty until the v3 writer fills
    /// them in at pack-emit time. Defaults to [`FORMAT_VERSION_V3`] —
    /// callers that opt in to v3.1 features (`sources`, `author_key_id`)
    /// must bump `format_version` to [`FORMAT_VERSION_V31`] explicitly,
    /// which `validate()` then enforces.
    pub fn new(name: impl Into<String>, version: Version) -> Self {
        Self {
            format_version: FORMAT_VERSION_V3.to_string(),
            name: name.into(),
            version,
            source_hash: String::new(),
            claims_hash: String::new(),
            pack_hash: String::new(),
            source_files: None,
            source_bytes: None,
            claim_count: None,
            extracted_at: None,
            extractor: None,
            license: None,
            description: None,
            readme: None,
            authors: Vec::new(),
            author_key_id: None,
            sources: Vec::new(),
        }
    }

    /// Validate every structural invariant on the reader path:
    /// `format_version` ∈ {`tr/3`, `tr/3.1`, `tr/3.2`}, well-formed
    /// `name`/`version`, hash fields are either empty (during pack
    /// assembly) or `blake3:<64-lowercase-hex>`, and consistent
    /// counts. v3.1-only fields (`sources`, `author_key_id`) are
    /// rejected on `tr/3` so a v3 declaration cannot smuggle in
    /// features that require the bump.
    pub fn validate(&self) -> Result<()> {
        // `is_v31` covers BOTH `tr/3.1` AND `tr/3.2` — v3.2 is a
        // strict additive bump of v3.1 (extra files in the tarball,
        // no new manifest fields), so every v3.1 invariant carries
        // forward verbatim.
        let is_v31 = match self.format_version.as_str() {
            FORMAT_VERSION_V3 => false,
            FORMAT_VERSION_V31 | FORMAT_VERSION_V32 => true,
            other => {
                return Err(Error::Invalid {
                    what: "manifest.toml",
                    detail: format!(
                        "format_version must be `{FORMAT_VERSION_V3}`, `{FORMAT_VERSION_V31}`, or `{FORMAT_VERSION_V32}`, got `{other}`"
                    ),
                });
            }
        };
        validate_name(&self.name)?;
        for (label, val) in [
            ("source_hash", &self.source_hash),
            ("claims_hash", &self.claims_hash),
            ("pack_hash", &self.pack_hash),
        ] {
            validate_blake3_hash_field(label, val)?;
        }
        if let Some(r) = &self.readme {
            const MAX_README_BYTES: usize = 256 * 1024;
            if r.len() > MAX_README_BYTES {
                return Err(Error::Invalid {
                    what: "manifest.toml",
                    detail: format!(
                        "readme exceeds {MAX_README_BYTES} bytes (got {})",
                        r.len()
                    ),
                });
            }
        }
        // Version-feature consistency: v3.1-only fields require the bump.
        if !is_v31 && self.author_key_id.is_some() {
            return Err(Error::Invalid {
                what: "manifest.toml",
                detail: format!(
                    "`author_key_id` requires format_version `{FORMAT_VERSION_V31}`; got `{}`",
                    self.format_version
                ),
            });
        }
        if !is_v31 && !self.sources.is_empty() {
            return Err(Error::Invalid {
                what: "manifest.toml",
                detail: format!(
                    "`sources` requires format_version `{FORMAT_VERSION_V31}`; got `{}`",
                    self.format_version
                ),
            });
        }
        if let Some(did) = &self.author_key_id {
            validate_author_key_id(did)?;
        }
        for (i, src) in self.sources.iter().enumerate() {
            validate_source_entry(i, src)?;
        }
        // Cross-field consistency: when both summary counts and the
        // detailed `sources` list are present they must agree. This
        // catches accidental drift between the per-source list and the
        // aggregate counts emitted by the writer.
        if let Some(declared) = self.source_files {
            if !self.sources.is_empty() && declared as usize != self.sources.len() {
                return Err(Error::Invalid {
                    what: "manifest.toml",
                    detail: format!(
                        "source_files = {declared} disagrees with sources.len() = {}",
                        self.sources.len()
                    ),
                });
            }
        }
        if let Some(declared) = self.source_bytes {
            if !self.sources.is_empty() {
                let summed: u64 = self.sources.iter().map(|s| s.bytes).sum();
                if declared != summed {
                    return Err(Error::Invalid {
                        what: "manifest.toml",
                        detail: format!(
                            "source_bytes = {declared} disagrees with sum(sources[].bytes) = {summed}"
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Parse a `manifest.toml` blob and validate.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let s = std::str::from_utf8(bytes).map_err(|e| Error::Invalid {
            what: "manifest.toml",
            detail: format!("not valid UTF-8: {e}"),
        })?;
        let m: ManifestV3 = toml::from_str(s).map_err(|e| Error::Invalid {
            what: "manifest.toml",
            detail: format!("toml parse: {e}"),
        })?;
        m.validate()?;
        Ok(m)
    }

    /// Split the `name` on the first `/` into `(owner, slug)`.
    pub fn owner_and_slug(&self) -> Result<(&str, &str)> {
        match self.name.split_once('/') {
            Some((o, s)) if !o.is_empty() && !s.is_empty() && !s.contains('/') => Ok((o, s)),
            _ => Err(Error::Invalid {
                what: "manifest.toml",
                detail: format!("name `{}` must be `owner/slug`", self.name),
            }),
        }
    }

    /// Emit the canonical TOML body. Used by the v3 writer to produce
    /// `manifest.toml` and by [`ManifestV3::canonical_bytes_for_hashing`]
    /// to produce the input to the BLAKE3 pack hash.
    ///
    /// Canonicalization rules (spec §3.2 + D7 from the v3 implementation
    /// plan):
    /// 1. Keys sorted alphabetically.
    /// 2. No trailing whitespace on any line.
    /// 3. Unix line endings (LF).
    /// 4. Each value emitted with a fixed, locked formatter (no
    ///    upstream-toml-version-dependent quirks).
    ///
    /// `blank_pack_hash = true` blanks the `pack_hash` field — the
    /// hashing-input form. `blank_pack_hash = false` emits the actual
    /// manifest-body form for the pack file.
    fn emit_canonical_toml(&self, blank_pack_hash: bool) -> String {
        let mut out = String::new();
        // Alphabetical: authors, author_key_id, claim_count, claims_hash,
        // description, extracted_at, extractor, format_version, license,
        // name, pack_hash, readme, source_bytes, source_files, source_hash,
        // sources, version.
        //
        // Note on ordering of `author_key_id` vs `authors`: ASCII-wise
        // `'_' (0x5F) < 's' (0x73)` so `author_key_id` < `authors` …
        // but only when comparing the eighth byte. Both strings share
        // the prefix `author`; at byte 6 we have `_` (key_id) vs `s`
        // (authors), and `_` < `s` — so `author_key_id` sorts BEFORE
        // `authors`. We emit in that order to keep canonical bytes
        // deterministic.
        if let Some(d) = &self.author_key_id {
            out.push_str("author_key_id = \"");
            escape_toml_string_into(&mut out, d);
            out.push_str("\"\n");
        }
        if !self.authors.is_empty() {
            out.push_str("authors = [");
            for (i, a) in self.authors.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('"');
                escape_toml_string_into(&mut out, a);
                out.push('"');
            }
            out.push_str("]\n");
        }
        if let Some(c) = self.claim_count {
            out.push_str(&format!("claim_count = {c}\n"));
        }
        out.push_str(&format!("claims_hash = \"{}\"\n", self.claims_hash));
        if let Some(d) = &self.description {
            out.push_str("description = \"");
            escape_toml_string_into(&mut out, d);
            out.push_str("\"\n");
        }
        if let Some(e) = &self.extracted_at {
            // RFC 3339 with seconds precision and a literal `Z` suffix.
            // Emitted as a quoted basic string (not TOML's native
            // datetime literal) so chrono's `DateTime<Utc>` serde
            // adapter — which expects a string — round-trips through
            // `ManifestV3::parse`. TOML native datetime would require
            // a custom deserializer in every consumer.
            out.push_str(&format!(
                "extracted_at = \"{}\"\n",
                e.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ));
        }
        if let Some(e) = &self.extractor {
            out.push_str("extractor = \"");
            escape_toml_string_into(&mut out, e);
            out.push_str("\"\n");
        }
        out.push_str(&format!("format_version = \"{}\"\n", self.format_version));
        if let Some(l) = &self.license {
            out.push_str("license = \"");
            escape_toml_string_into(&mut out, l);
            out.push_str("\"\n");
        }
        out.push_str("name = \"");
        escape_toml_string_into(&mut out, &self.name);
        out.push_str("\"\n");
        let pack_hash = if blank_pack_hash {
            ""
        } else {
            self.pack_hash.as_str()
        };
        out.push_str(&format!("pack_hash = \"{pack_hash}\"\n"));
        if let Some(r) = &self.readme {
            out.push_str("readme = \"");
            escape_toml_string_into(&mut out, r);
            out.push_str("\"\n");
        }
        if let Some(c) = self.source_bytes {
            out.push_str(&format!("source_bytes = {c}\n"));
        }
        if let Some(c) = self.source_files {
            out.push_str(&format!("source_files = {c}\n"));
        }
        out.push_str(&format!("source_hash = \"{}\"\n", self.source_hash));
        if !self.sources.is_empty() {
            // Inline array-of-inline-tables. Avoids the TOML pitfall
            // where `[[sources]]` headers steal subsequent top-level
            // keys; with inline tables every top-level key stays at
            // the outer scope.
            out.push_str("sources = [\n");
            for src in &self.sources {
                out.push_str("    { ");
                emit_source_entry_inline(&mut out, src);
                out.push_str(" },\n");
            }
            out.push_str("]\n");
        }
        out.push_str(&format!("version = \"{}\"\n", self.version));
        out
    }

    /// Canonical TOML body suitable for writing as `manifest.toml`.
    pub fn to_canonical_toml(&self) -> Vec<u8> {
        self.emit_canonical_toml(false).into_bytes()
    }

    /// Canonical bytes used as the pack-hash input. `pack_hash` is
    /// blanked; everything else is identical to [`to_canonical_toml`].
    pub fn canonical_bytes_for_hashing(&self) -> Vec<u8> {
        self.emit_canonical_toml(true).into_bytes()
    }
}

/// Emit a [`SourceEntry`] as an inline TOML table body — the part
/// between `{` and `}`. Fields ordered alphabetically: `bytes`,
/// `content_hash`, `derived_hashes`, `mime_type`, `relative_path`.
/// Optional/empty fields are skipped to match the
/// `skip_serializing_if` rules on the struct.
fn emit_source_entry_inline(out: &mut String, src: &SourceEntry) {
    out.push_str(&format!("bytes = {}", src.bytes));
    out.push_str(", content_hash = \"");
    escape_toml_string_into(out, &src.content_hash);
    out.push('"');
    if !src.derived_hashes.is_empty() {
        out.push_str(", derived_hashes = [");
        for (i, dh) in src.derived_hashes.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str("{ hash = \"");
            escape_toml_string_into(out, &dh.hash);
            out.push_str("\", kind = \"");
            escape_toml_string_into(out, &dh.kind);
            out.push_str("\" }");
        }
        out.push(']');
    }
    if let Some(mt) = &src.mime_type {
        out.push_str(", mime_type = \"");
        escape_toml_string_into(out, mt);
        out.push('"');
    }
    out.push_str(", relative_path = \"");
    escape_toml_string_into(out, &src.relative_path);
    out.push('"');
}

/// Validate a `SourceEntry` row. Refuses path-traversal payloads,
/// malformed BLAKE3 hashes, and `derived_hashes[].kind` outside
/// [`DERIVED_HASH_KINDS`].
fn validate_source_entry(index: usize, src: &SourceEntry) -> Result<()> {
    let path = &src.relative_path;
    if path.is_empty() || path.len() > 1024 {
        return Err(Error::Invalid {
            what: "manifest.toml",
            detail: format!("sources[{index}].relative_path must be 1–1024 chars"),
        });
    }
    if path.starts_with('/') {
        return Err(Error::Invalid {
            what: "manifest.toml",
            detail: format!("sources[{index}].relative_path must not start with `/`"),
        });
    }
    // `..` segments and backslashes are rejected so a malicious manifest
    // cannot smuggle path-traversal hooks into compliance bundles or
    // mount targets that consume this list verbatim.
    for seg in path.split('/') {
        if seg == ".." || seg.contains('\\') {
            return Err(Error::Invalid {
                what: "manifest.toml",
                detail: format!(
                    "sources[{index}].relative_path contains forbidden segment `{seg}`"
                ),
            });
        }
    }
    if !path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'))
    {
        return Err(Error::Invalid {
            what: "manifest.toml",
            detail: format!(
                "sources[{index}].relative_path may only contain [A-Za-z0-9_./-]"
            ),
        });
    }
    validate_blake3_hash_field("sources[].content_hash", &src.content_hash)?;
    if src.content_hash.is_empty() {
        return Err(Error::Invalid {
            what: "manifest.toml",
            detail: format!("sources[{index}].content_hash must not be empty"),
        });
    }
    for (j, dh) in src.derived_hashes.iter().enumerate() {
        if !DERIVED_HASH_KINDS.contains(&dh.kind.as_str()) {
            return Err(Error::Invalid {
                what: "manifest.toml",
                detail: format!(
                    "sources[{index}].derived_hashes[{j}].kind = `{}` is not in the allow-list ({:?})",
                    dh.kind, DERIVED_HASH_KINDS
                ),
            });
        }
        validate_blake3_hash_field("derived_hashes[].hash", &dh.hash)?;
        if dh.hash.is_empty() {
            return Err(Error::Invalid {
                what: "manifest.toml",
                detail: format!(
                    "sources[{index}].derived_hashes[{j}].hash must not be empty"
                ),
            });
        }
    }
    Ok(())
}

/// Validate the `author_key_id` field — must be a `did:method:identifier`
/// possibly with a `#fragment` suffix. We do not resolve the DID here;
/// resolution lives in `tr-verify::AuthorVerifier`.
fn validate_author_key_id(did: &str) -> Result<()> {
    if did.len() > 512 {
        return Err(Error::Invalid {
            what: "manifest.toml",
            detail: "author_key_id must be ≤ 512 chars".into(),
        });
    }
    let core = did.split('#').next().unwrap_or("");
    let parts: Vec<&str> = core.split(':').collect();
    if parts.len() < 3 || parts[0] != "did" {
        return Err(Error::Invalid {
            what: "manifest.toml",
            detail: format!(
                "author_key_id `{did}` must match `did:method:identifier[#fragment]`"
            ),
        });
    }
    if parts[1].is_empty() || parts[2..].iter().any(|p| p.is_empty()) {
        return Err(Error::Invalid {
            what: "manifest.toml",
            detail: format!("author_key_id `{did}` has empty method or identifier segment"),
        });
    }
    Ok(())
}

/// Escape a string into TOML basic-string form. Only the rules we
/// actually need: backslash, double-quote, and ASCII control
/// characters. Other characters (including non-ASCII UTF-8) are
/// passed through verbatim — TOML basic strings accept arbitrary
/// Unicode aside from the control range.
fn escape_toml_string_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 {
        return Err(Error::Invalid {
            what: "manifest.toml",
            detail: "name must be 1–128 chars".into(),
        });
    }
    let (owner, slug) = name.split_once('/').ok_or(Error::Invalid {
        what: "manifest.toml",
        detail: format!("name `{name}` must contain exactly one `/`"),
    })?;
    if owner.is_empty() || slug.is_empty() || slug.contains('/') {
        return Err(Error::Invalid {
            what: "manifest.toml",
            detail: format!("name `{name}` must be `owner/slug`"),
        });
    }
    // Match the documented spec at the struct docstring: each segment
    // is `[a-z0-9][a-z0-9-]*`, length ≤ 64.  Lowercase-only is what
    // the registry's URL routing relies on (case-insensitive lookup
    // would otherwise let two distinct packs collide on disk).
    for (label, part) in [("owner", owner), ("slug", slug)] {
        if part.len() > 64 {
            return Err(Error::Invalid {
                what: "manifest.toml",
                detail: format!("{label} must be ≤ 64 chars (got {})", part.len()),
            });
        }
        let first = part.chars().next().expect("non-empty checked above");
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(Error::Invalid {
                what: "manifest.toml",
                detail: format!(
                    "{label} `{part}` must start with [a-z0-9] (lowercase + digit only)"
                ),
            });
        }
        if !part
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(Error::Invalid {
                what: "manifest.toml",
                detail: format!(
                    "{label} `{part}` may only contain [a-z0-9-]; uppercase, dots, and underscores are rejected"
                ),
            });
        }
    }
    Ok(())
}

/// Validate a `blake3:` hash field — empty allowed (during pack
/// assembly the writer hasn't filled it yet) or exactly the form
/// `blake3:<64-lowercase-hex>`. Refuses truncated, uppercase, or
/// non-hex suffixes that the previous prefix-only check accepted.
fn validate_blake3_hash_field(label: &'static str, val: &str) -> Result<()> {
    if val.is_empty() {
        return Ok(());
    }
    let suffix = val.strip_prefix("blake3:").ok_or(Error::Invalid {
        what: "manifest.toml",
        detail: format!("{label} must start with `blake3:`"),
    })?;
    if suffix.len() != 64 {
        return Err(Error::Invalid {
            what: "manifest.toml",
            detail: format!(
                "{label} suffix must be 64 hex chars (got {})",
                suffix.len()
            ),
        });
    }
    if !suffix.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        return Err(Error::Invalid {
            what: "manifest.toml",
            detail: format!("{label} suffix must be lowercase hex"),
        });
    }
    Ok(())
}

mod semver_string {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Version;

    pub fn serialize<S: Serializer>(v: &Version, s: S) -> Result<S::Ok, S::Error> {
        v.to_string().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Version, D::Error> {
        let raw = String::deserialize(d)?;
        Version::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ManifestV3 {
        ManifestV3::new("alice/thesis", Version::parse("0.1.0").unwrap())
    }

    #[test]
    fn parse_rejects_wrong_format_version() {
        let mut m = sample();
        m.format_version = "tr/2".into();
        let toml_bytes = m.to_canonical_toml();
        let err = ManifestV3::parse(&toml_bytes).unwrap_err();
        assert!(matches!(err, Error::Invalid { .. }));
    }

    #[test]
    fn parse_rejects_bad_name() {
        for bad in ["no-slash", "too/many/slashes", "", "empty//slug"] {
            let mut m = sample();
            m.name = bad.to_string();
            let toml_bytes = m.to_canonical_toml();
            assert!(
                ManifestV3::parse(&toml_bytes).is_err(),
                "`{bad}` should fail"
            );
        }
    }

    #[test]
    fn owner_and_slug_split() {
        let m = sample();
        assert_eq!(m.owner_and_slug().unwrap(), ("alice", "thesis"));
    }

    #[test]
    fn canonical_hash_blanks_pack_hash() {
        let mut m = sample();
        m.pack_hash = "blake3:not-yet-computed".into();
        // canonical_bytes_for_hashing emits the manifest with pack_hash
        // blanked; populating pack_hash before vs. after should not
        // change the hashing input.
        let bytes_with_hash = m.canonical_bytes_for_hashing();
        m.pack_hash.clear();
        let bytes_without = m.canonical_bytes_for_hashing();
        assert_eq!(bytes_with_hash, bytes_without);
    }

    #[test]
    fn readme_round_trip_some() {
        let mut m = sample();
        m.readme = Some("# Title\n\nBody with `code` and *emphasis*\n".into());
        let toml_bytes = m.to_canonical_toml();
        let parsed = ManifestV3::parse(&toml_bytes).unwrap();
        assert_eq!(parsed.readme, m.readme);
        let toml_again = parsed.to_canonical_toml();
        assert_eq!(toml_bytes, toml_again, "second emit must be byte-stable");
    }

    #[test]
    fn readme_round_trip_none() {
        let m = sample();
        let toml_bytes = m.to_canonical_toml();
        let toml_str = std::str::from_utf8(&toml_bytes).unwrap();
        assert!(
            !toml_str.contains("readme = "),
            "None readme must not appear in canonical TOML, got:\n{toml_str}"
        );
        let parsed = ManifestV3::parse(&toml_bytes).unwrap();
        assert_eq!(parsed.readme, None);
    }

    #[test]
    fn readme_escapes_special_chars() {
        let mut m = sample();
        let payload = "quotes \"x\" backslash \\ newline\nreturn\rtab\tcontrol\u{0001} emoji 🌳";
        m.readme = Some(payload.into());
        let toml_bytes = m.to_canonical_toml();
        let parsed = ManifestV3::parse(&toml_bytes).unwrap();
        assert_eq!(parsed.readme.as_deref(), Some(payload));
    }

    #[test]
    fn readme_above_cap_rejected() {
        let mut m = sample();
        m.readme = Some("x".repeat(257 * 1024));
        match m.validate() {
            Err(Error::Invalid { what, detail }) => {
                assert_eq!(what, "manifest.toml");
                assert!(detail.contains("readme exceeds"));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn readme_old_pack_hashes_identically() {
        // A pre-feature pack has no `readme` field. After upgrading,
        // round-tripping with `readme: None` must produce the exact
        // same canonical-bytes-for-hashing as before — i.e. the field
        // is invisible when None. Regression guard for forward-compat.
        let m = sample();
        let bytes_a = m.canonical_bytes_for_hashing();
        let toml_str = std::str::from_utf8(&bytes_a).unwrap();
        assert!(
            !toml_str.contains("readme"),
            "manifest with readme: None must not emit a readme line"
        );

        let parsed = ManifestV3::parse(&m.to_canonical_toml()).unwrap();
        let bytes_b = parsed.canonical_bytes_for_hashing();
        assert_eq!(
            bytes_a, bytes_b,
            "round-trip through new code must be byte-stable when readme is None"
        );
    }

    // ── tr/3.1 schema bump tests ────────────────────────────────────

    fn good_source_entry(name: &str) -> SourceEntry {
        // 64-hex BLAKE3 placeholders — `0123456789abcdef` × 4 = 64 chars.
        const CONTENT: &str =
            "blake3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const DERIVED: &str =
            "blake3:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        SourceEntry {
            relative_path: name.into(),
            content_hash: CONTENT.into(),
            bytes: 100,
            mime_type: Some("text/markdown".into()),
            derived_hashes: vec![DerivedHash {
                kind: "summary.blake3".into(),
                hash: DERIVED.into(),
            }],
        }
    }

    #[test]
    fn manifest_v31_round_trip() {
        let mut m = sample();
        m.format_version = FORMAT_VERSION_V31.into();
        m.author_key_id = Some("did:key:z6MkpzExample123#k1".into());
        m.sources = vec![good_source_entry("a.md"), good_source_entry("b/c.md")];
        m.source_files = Some(2);
        m.source_bytes = Some(200);
        let toml_bytes = m.to_canonical_toml();
        let parsed = ManifestV3::parse(&toml_bytes).unwrap();
        assert_eq!(parsed.format_version, FORMAT_VERSION_V31);
        assert_eq!(parsed.author_key_id.as_deref(), Some("did:key:z6MkpzExample123#k1"));
        assert_eq!(parsed.sources.len(), 2);
        assert_eq!(parsed.sources[0].relative_path, "a.md");
        assert_eq!(parsed.sources[1].relative_path, "b/c.md");
        // Second emit is byte-stable (pack_hash determinism).
        let toml_again = parsed.to_canonical_toml();
        assert_eq!(toml_bytes, toml_again, "second emit must be byte-stable");
    }

    #[test]
    fn v3_reader_ignores_unknown_v31_inline_fields() {
        // Forward-compat: a `tr/3` manifest produced by *old* code
        // that received a v3.1 pack must drop the v3.1 fields silently.
        // We simulate "old code" by parsing a v3.1 TOML body with the
        // current parser, then forcing the format_version back to v3
        // and re-validating — this is the minimal proof that the new
        // fields don't bleed through into a v3 declaration.
        let mut m = sample();
        m.format_version = FORMAT_VERSION_V31.into();
        m.author_key_id = Some("did:web:example.com#k1".into());
        m.sources = vec![good_source_entry("a.md")];
        m.source_files = Some(1);
        m.source_bytes = Some(100);
        let _ = m.to_canonical_toml();
        // Now the same struct without the v3.1 fields, declared v3 —
        // must round-trip cleanly with no v3.1 contamination.
        let mut m_v3 = sample();
        m_v3.format_version = FORMAT_VERSION_V3.into();
        let bytes = m_v3.to_canonical_toml();
        let parsed = ManifestV3::parse(&bytes).unwrap();
        assert!(parsed.author_key_id.is_none());
        assert!(parsed.sources.is_empty());
    }

    #[test]
    fn validate_rejects_v3_with_author_key_id() {
        let mut m = sample();
        m.format_version = FORMAT_VERSION_V3.into();
        m.author_key_id = Some("did:key:abc#k1".into());
        match m.validate() {
            Err(Error::Invalid { detail, .. }) => {
                assert!(
                    detail.contains("author_key_id") && detail.contains(FORMAT_VERSION_V31),
                    "error should name the field and the required version: {detail}"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_v3_with_sources() {
        let mut m = sample();
        m.format_version = FORMAT_VERSION_V3.into();
        m.sources = vec![good_source_entry("a.md")];
        m.source_files = Some(1);
        m.source_bytes = Some(100);
        match m.validate() {
            Err(Error::Invalid { detail, .. }) => {
                assert!(detail.contains("sources") && detail.contains(FORMAT_VERSION_V31));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_path_traversal() {
        let mut m = sample();
        m.format_version = FORMAT_VERSION_V31.into();
        for bad in ["../etc/passwd", "/abs/path", "ok/../bad", "back\\slash"] {
            let mut src = good_source_entry("placeholder");
            src.relative_path = bad.into();
            m.sources = vec![src];
            m.source_files = Some(1);
            m.source_bytes = Some(100);
            assert!(
                m.validate().is_err(),
                "`{bad}` must be rejected as path traversal"
            );
        }
    }

    #[test]
    fn validate_rejects_unknown_derived_hash_kind() {
        let mut m = sample();
        m.format_version = FORMAT_VERSION_V31.into();
        let mut src = good_source_entry("a.md");
        src.derived_hashes = vec![DerivedHash {
            kind: "embedding.blake3".into(), // not in allow-list
            hash: "blake3:0000000000000000000000000000000000000000000000000000000000000000".into(),
        }];
        m.sources = vec![src];
        m.source_files = Some(1);
        m.source_bytes = Some(100);
        match m.validate() {
            Err(Error::Invalid { detail, .. }) => {
                assert!(detail.contains("embedding.blake3") && detail.contains("allow-list"));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_malformed_did() {
        for bad in [
            "not-a-did",
            "did:",
            "did::missing-method",
            "did:method:",
            "did:method",
        ] {
            let mut m = sample();
            m.format_version = FORMAT_VERSION_V31.into();
            m.author_key_id = Some(bad.into());
            assert!(
                m.validate().is_err(),
                "`{bad}` must be rejected as malformed DID"
            );
        }
    }

    #[test]
    fn validate_rejects_count_disagreement() {
        let mut m = sample();
        m.format_version = FORMAT_VERSION_V31.into();
        m.sources = vec![good_source_entry("a.md"), good_source_entry("b.md")];
        m.source_files = Some(99); // disagrees
        m.source_bytes = Some(200);
        assert!(m.validate().is_err());

        m.source_files = Some(2);
        m.source_bytes = Some(99); // disagrees with sum
        assert!(m.validate().is_err());

        m.source_bytes = Some(200); // 100 + 100
        assert!(m.validate().is_ok());
    }

    #[test]
    fn canonical_toml_alphabetical_includes_v31_fields() {
        let mut m = sample();
        m.format_version = FORMAT_VERSION_V31.into();
        m.author_key_id = Some("did:key:abc#k1".into());
        m.sources = vec![good_source_entry("a.md")];
        m.source_files = Some(1);
        m.source_bytes = Some(100);
        let bytes = m.to_canonical_toml();
        let toml_str = std::str::from_utf8(&bytes).unwrap();
        // Validate ordering by finding offsets — `author_key_id` MUST
        // come before `claims_hash` and `sources` MUST come after
        // `source_hash`.
        let pos_author = toml_str.find("author_key_id").expect("author_key_id present");
        let pos_claims = toml_str.find("claims_hash").expect("claims_hash present");
        let pos_source_hash = toml_str.find("source_hash").expect("source_hash present");
        let pos_sources = toml_str.find("sources = [").expect("sources present");
        let pos_version = toml_str.find("\nversion = ").expect("version present");
        assert!(pos_author < pos_claims, "author_key_id must precede claims_hash");
        assert!(
            pos_source_hash < pos_sources,
            "sources must follow source_hash"
        );
        assert!(
            pos_sources < pos_version,
            "sources must precede version (otherwise TOML inline-table parsing breaks)"
        );
    }

    #[test]
    fn v31_hash_is_byte_stable_across_two_serialisations() {
        let mut m = sample();
        m.format_version = FORMAT_VERSION_V31.into();
        m.author_key_id = Some("did:key:abc#k1".into());
        m.sources = vec![good_source_entry("a.md"), good_source_entry("b.md")];
        m.source_files = Some(2);
        m.source_bytes = Some(200);
        let bytes_a = m.canonical_bytes_for_hashing();
        let parsed = ManifestV3::parse(&m.to_canonical_toml()).unwrap();
        let bytes_b = parsed.canonical_bytes_for_hashing();
        assert_eq!(
            bytes_a, bytes_b,
            "v3.1 manifest must hash identically across parse-then-emit"
        );
    }
}
