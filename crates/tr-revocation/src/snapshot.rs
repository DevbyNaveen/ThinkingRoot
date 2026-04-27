//! Wire schema for the signed revocation snapshot.
//!
//! Mirrors the response shape documented in
//! `docs/2026-04-24-revocation-protocol-spec.md` §4.1 verbatim. New
//! fields are additive — older clients ignore unknown fields via serde
//! defaults; the cloud guarantees field-only growth.

use serde::{Deserialize, Serialize};

/// One revocation list as served by the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Schema version of this document. Currently `"1.0.0"`.
    pub schema_version: String,
    /// Unix epoch seconds at which the snapshot was generated.
    pub generated_at: i64,
    /// Hostname of the issuing registry, e.g. `"hub.thinkingroot.dev"`.
    pub generated_by: String,
    /// `true` if `entries` is the full list; `false` for incremental
    /// (`since=…`) responses.
    #[serde(default)]
    pub full_list: bool,
    /// Revoked artifacts. Iterated linearly by [`crate::RevocationCache::is_revoked`];
    /// linear scan is fine for the protocol's expected cardinality
    /// (low thousands at launch).
    pub entries: Vec<Advisory>,
    /// Base64-encoded Ed25519 signature over the canonical bytes
    /// returned by [`Snapshot::canonical_bytes_for_signing`].
    pub signature: String,
    /// Identifier of the key that produced `signature`. Looked up
    /// against the client's pinned key set.
    pub signing_key_id: String,
    /// Hint to the caller — how many seconds until the next poll is
    /// worth attempting.
    pub next_poll_hint_sec: u64,
}

/// One advisory entry inside a [`Snapshot`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Advisory {
    /// Canonical content hash of the revoked `.tr`. Format is
    /// `"blake3:<64-lower-hex>"`. Comparisons normalize the prefix.
    pub content_hash: String,
    /// The pack coordinate `owner/slug`.
    pub pack: String,
    /// SemVer string of the revoked version.
    pub version: String,
    /// Why the artifact was revoked.
    pub reason: Reason,
    /// Unix epoch seconds at which the revocation was issued.
    pub revoked_at: i64,
    /// Who issued the revocation.
    pub authority: Authority,
    /// URL of the human-readable advisory.
    pub details_url: String,
}

/// Coarse-grained revocation reason. The full free-form rationale is
/// at `Advisory::details_url`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Publisher self-revoked.
    PublisherRequest,
    /// Malicious code detected.
    Malware,
    /// API key or other secret leaked inside the pack.
    SecretLeak,
    /// CSAM detected — auto-revoked under 18 USC §2258A.
    Csam,
    /// DMCA takedown.
    Dmca,
    /// Org admin revoked on behalf of a publisher in their org.
    OrgAdminRequest,
    /// Court order or other legal directive.
    Legal,
    /// None of the above. Avoid in new code.
    Unspecified,
}

/// Who issued a revocation. Used by the desktop UI to render a clear
/// attribution line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// The pack's own publisher.
    Publisher,
    /// An admin of the publisher's org.
    OrgAdmin,
    /// Hub moderation staff.
    HubModeration,
    /// Automated abuse / malware scanner.
    HubScanner,
    /// Hub legal team.
    Legal,
    /// Two-key emergency quorum (CEO + CTO).
    Emergency,
}

impl Snapshot {
    /// Return the bytes that the registry signed.
    ///
    /// Per `revocation-protocol-spec.md` §7.2 the signed payload is
    /// canonical JSON of `{ entries, generated_at, generated_by }`
    /// **without** the signature, key id, or schema metadata. Computing
    /// this independently from `serde_json::to_vec(self)` keeps the
    /// signed shape stable when we add forward-compatible fields.
    pub fn canonical_bytes_for_signing(&self) -> std::result::Result<Vec<u8>, serde_json::Error> {
        #[derive(Serialize)]
        struct Canonical<'a> {
            entries: &'a [Advisory],
            generated_at: i64,
            generated_by: &'a str,
        }
        serde_json::to_vec(&Canonical {
            entries: &self.entries,
            generated_at: self.generated_at,
            generated_by: &self.generated_by,
        })
    }
}
