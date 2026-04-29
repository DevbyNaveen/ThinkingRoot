//! The TR-1 manifest — the single authoritative description of a pack.
//!
//! Every `.tr` archive MUST contain `manifest.json` at the archive
//! root. The document shape evolves by additive fields; the
//! `format_version` field tells readers which shape to expect. This
//! crate's struct targets `"tr/1"`; readers encountering a higher
//! major version SHOULD refuse to mount (they cannot safely reason
//! about invariants they don't know).

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{
    capabilities::Capabilities,
    error::{Error, Result},
};

/// Current format identifier — `"tr/1"`. Any other value at parse time
/// is a fatal error.
pub const FORMAT_VERSION: &str = "tr/1";

/// v3 format identifier — `"tr/3"`. Used by [`ManifestV3`] and the v3
/// writer module. Locked by spec §3.2; readers refusing on mismatch
/// surfaces incompatibility cleanly.
pub const FORMAT_VERSION_V3: &str = "tr/3";

/// Trust tier — how strong the provenance claim of this pack is.
///
/// See the Rooting + security-model specs for definitions. Clients
/// use the tier (together with the revocation set and Sigstore
/// attestations) to decide whether a pack may be mounted without
/// additional user approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    /// No verifiable provenance.
    T0,
    /// Authorship claim only (signed by author key, no third-party root).
    T1,
    /// Sigstore keyless attestation via a public CI workflow. Default
    /// for packs published via Trusted Publishing.
    T2,
    /// T2 plus per-claim certificates.
    T3,
    /// T3 plus embedded source bytes enabling off-line re-rooting.
    T4,
}

impl TrustTier {
    /// Integer rank for comparisons.
    pub fn rank(&self) -> u8 {
        match self {
            Self::T0 => 0,
            Self::T1 => 1,
            Self::T2 => 2,
            Self::T3 => 3,
            Self::T4 => 4,
        }
    }
}

/// The canonical manifest carried by every `.tr` pack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// `"tr/1"` for this format revision. Unknown values fail the parse.
    pub format_version: String,

    /// Pack coordinate in `owner/slug` form. Slash-delimited, exactly
    /// one `/`.
    pub name: String,

    /// SemVer of this pack. Bumped by the Knowledge-PR service per the
    /// rules in `docs/2026-04-24-knowledge-pr-model.md`.
    #[serde(with = "semver_string")]
    pub version: Version,

    /// Short, one-line description used in listings.
    pub description: String,

    /// SPDX license expression (`"Apache-2.0"`, `"MIT"`, …). Hub refuses
    /// public packs whose license is unrecognised.
    pub license: String,

    /// Optional long-form readme (markdown). Kept inside the manifest
    /// so `root inspect` can show it without unpacking `artifacts/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readme: Option<String>,

    /// Authors / publishers (handles or free-form names). The first
    /// entry is treated as the primary publisher.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,

    /// Free-form tag list used by the search service.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// Trust tier — informational; the hub re-checks against its own
    /// revocation + signature store.
    pub trust_tier: TrustTier,

    /// BLAKE3 of the canonical-JSON serialisation of this same
    /// manifest with `content_hash` replaced by the empty string. Acts
    /// as the pack's opaque identity.
    pub content_hash: String,

    /// When this manifest was produced.
    pub generated_at: DateTime<Utc>,

    /// Declared capabilities. See [`Capabilities`].
    #[serde(default)]
    pub capabilities: Capabilities,

    /// Optional aggregate quality score (0.0–100.0) from the Rooting
    /// pipeline — percentage of claims in `rooted` tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rooted_pct: Option<f64>,

    /// Optional count of claims inside `graph/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_count: Option<u64>,
}

impl Manifest {
    /// Canonical JSON serialisation used for hashing and signing.
    ///
    /// `content_hash` is blanked out before serialisation so the hash
    /// is stable regardless of when we compute it. Ordering is
    /// deterministic because `serde_json::to_vec` respects struct
    /// field declaration order and every field type we use is itself
    /// deterministic.
    pub fn canonical_bytes_for_hashing(&self) -> Result<Vec<u8>> {
        let mut copy = self.clone();
        copy.content_hash.clear();
        serde_json::to_vec(&copy).map_err(Error::from)
    }

    /// Recompute the content hash. Does not mutate the manifest.
    pub fn compute_content_hash(&self) -> Result<String> {
        let bytes = self.canonical_bytes_for_hashing()?;
        Ok(crate::digest::blake3_hex(&bytes))
    }

    /// Validate every structural invariant. Called by readers after
    /// JSON parse and again after an optional manifest update.
    pub fn validate(&self) -> Result<()> {
        if self.format_version != FORMAT_VERSION {
            return Err(Error::Invalid {
                what: "manifest.json",
                detail: format!(
                    "format_version must be `{FORMAT_VERSION}`, got `{}`",
                    self.format_version
                ),
            });
        }
        validate_name(&self.name)?;
        if self.description.len() > 512 {
            return Err(Error::Invalid {
                what: "manifest.json",
                detail: "description exceeds 512 chars".into(),
            });
        }
        if self.license.trim().is_empty() {
            return Err(Error::Invalid {
                what: "manifest.json",
                detail: "license is required".into(),
            });
        }
        if !self.content_hash.is_empty() {
            crate::digest::parse_hex(&self.content_hash)?;
        }
        if let Some(p) = self.rooted_pct {
            if !(0.0..=100.0).contains(&p) {
                return Err(Error::Invalid {
                    what: "manifest.json",
                    detail: "rooted_pct must be in [0, 100]".into(),
                });
            }
        }
        Ok(())
    }

    /// Parse a manifest JSON blob and validate invariants in one step.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let m: Manifest = serde_json::from_slice(bytes)?;
        m.validate()?;
        Ok(m)
    }

    /// Minimal builder used by the writer + tests.
    pub fn new(name: impl Into<String>, version: Version, license: impl Into<String>) -> Self {
        Self {
            format_version: FORMAT_VERSION.to_string(),
            name: name.into(),
            version,
            description: String::new(),
            license: license.into(),
            readme: None,
            authors: Vec::new(),
            tags: Vec::new(),
            trust_tier: TrustTier::T0,
            content_hash: String::new(),
            generated_at: Utc::now(),
            capabilities: Capabilities::default(),
            rooted_pct: None,
            claim_count: None,
        }
    }

    /// Split the `name` on the first `/` into `(owner, slug)`.
    pub fn owner_and_slug(&self) -> Result<(&str, &str)> {
        match self.name.split_once('/') {
            Some((o, s)) if !o.is_empty() && !s.is_empty() && !s.contains('/') => Ok((o, s)),
            _ => Err(Error::Invalid {
                what: "manifest.json",
                detail: format!("name `{}` must be `owner/slug`", self.name),
            }),
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// v3 manifest — `manifest.toml` inside the 3-file pack layout.
//
// The shape is locked by the v3 spec §3.2. Bytes emitted by
// `to_canonical_toml` and `canonical_bytes_for_hashing` are the
// load-bearing inputs to `pack_hash` per spec §3.1; once Sigstore
// signing lands in Week 3 those bytes become the substrate of every
// signed pack — changing the canonicalization rule afterward
// invalidates every previously-signed pack. **Lock locked locked.**
// ─────────────────────────────────────────────────────────────────

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

    /// Pack coordinate in `owner/slug` form. Same validation as v1.
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

    /// Author handles or contact strings. Optional, default empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
}

impl ManifestV3 {
    /// Minimal builder. Hashes are all-empty until the v3 writer fills
    /// them in at pack-emit time.
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
            authors: Vec::new(),
        }
    }

    /// Validate every structural invariant. Reader path; mirrors the v1
    /// `Manifest::validate` shape so consumers can swap with minimal
    /// diff.
    pub fn validate(&self) -> Result<()> {
        if self.format_version != FORMAT_VERSION_V3 {
            return Err(Error::Invalid {
                what: "manifest.toml",
                detail: format!(
                    "format_version must be `{FORMAT_VERSION_V3}`, got `{}`",
                    self.format_version
                ),
            });
        }
        validate_name(&self.name)?;
        for (label, val) in [
            ("source_hash", &self.source_hash),
            ("claims_hash", &self.claims_hash),
        ] {
            // Empty during pack assembly is OK — writer fills before emit.
            if !val.is_empty() && !val.starts_with("blake3:") {
                return Err(Error::Invalid {
                    what: "manifest.toml",
                    detail: format!("{label} must start with `blake3:`"),
                });
            }
        }
        // pack_hash may be empty during canonicalization-for-hashing.
        if !self.pack_hash.is_empty() && !self.pack_hash.starts_with("blake3:") {
            return Err(Error::Invalid {
                what: "manifest.toml",
                detail: "pack_hash must start with `blake3:`".into(),
            });
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
        // Alphabetical: authors, claim_count, claims_hash, description,
        // extracted_at, extractor, format_version, license, name,
        // pack_hash, source_bytes, source_files, source_hash, version.
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
        if let Some(c) = self.source_bytes {
            out.push_str(&format!("source_bytes = {c}\n"));
        }
        if let Some(c) = self.source_files {
            out.push_str(&format!("source_files = {c}\n"));
        }
        out.push_str(&format!("source_hash = \"{}\"\n", self.source_hash));
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
            what: "manifest.json",
            detail: "name must be 1–128 chars".into(),
        });
    }
    let (owner, slug) = name.split_once('/').ok_or(Error::Invalid {
        what: "manifest.json",
        detail: format!("name `{name}` must contain exactly one `/`"),
    })?;
    if owner.is_empty() || slug.is_empty() || slug.contains('/') {
        return Err(Error::Invalid {
            what: "manifest.json",
            detail: format!("name `{name}` must be `owner/slug`"),
        });
    }
    for part in [owner, slug] {
        if !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(Error::Invalid {
                what: "manifest.json",
                detail: "name parts must match [a-zA-Z0-9._-]".into(),
            });
        }
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

    fn sample() -> Manifest {
        Manifest::new(
            "alice/thesis",
            Version::parse("0.1.0").unwrap(),
            "Apache-2.0",
        )
    }

    #[test]
    fn canonical_hash_ignores_content_hash_field() {
        let mut m = sample();
        let h1 = m.compute_content_hash().unwrap();
        m.content_hash = h1.clone();
        let h2 = m.compute_content_hash().unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn parse_rejects_wrong_format_version() {
        let mut m = sample();
        m.format_version = "tr/2".into();
        let bytes = serde_json::to_vec(&m).unwrap();
        let err = Manifest::parse(&bytes).unwrap_err();
        assert!(matches!(err, Error::Invalid { .. }));
    }

    #[test]
    fn parse_rejects_bad_name() {
        for bad in ["no-slash", "too/many/slashes", "", "empty//slug"] {
            let mut m = sample();
            m.name = bad.to_string();
            let bytes = serde_json::to_vec(&m).unwrap();
            assert!(Manifest::parse(&bytes).is_err(), "`{bad}` should fail");
        }
    }

    #[test]
    fn parse_accepts_well_formed() {
        let m = sample();
        let bytes = serde_json::to_vec(&m).unwrap();
        let round = Manifest::parse(&bytes).unwrap();
        assert_eq!(round, m);
    }

    #[test]
    fn owner_and_slug_split() {
        let m = sample();
        assert_eq!(m.owner_and_slug().unwrap(), ("alice", "thesis"));
    }

    #[test]
    fn trust_tier_ord_is_sensible() {
        assert!(TrustTier::T0 < TrustTier::T2);
        assert!(TrustTier::T4.rank() > TrustTier::T3.rank());
    }

    #[test]
    fn json_round_trip() {
        let mut m = sample();
        m.description = "A thesis pack.".into();
        m.tags = vec!["rust".into(), "phd".into()];
        m.capabilities = Capabilities {
            mcp_tools: vec!["ask".into()],
            ..Default::default()
        };
        m.rooted_pct = Some(95.0);
        m.claim_count = Some(12_345);
        m.content_hash = m.compute_content_hash().unwrap();

        let bytes = serde_json::to_vec_pretty(&m).unwrap();
        let round = Manifest::parse(&bytes).unwrap();
        assert_eq!(m, round);
    }
}
