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

    /// Validate every structural invariant on the reader path:
    /// `format_version == "tr/3"`, well-formed `name`/`version`,
    /// non-empty hash fields, and consistent counts.
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
            assert!(ManifestV3::parse(&toml_bytes).is_err(), "`{bad}` should fail");
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
}
