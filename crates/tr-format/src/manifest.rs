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
