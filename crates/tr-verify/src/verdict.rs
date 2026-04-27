//! The [`Verdict`] enum — the outward-facing result of trust
//! verification.
//!
//! Wording for each variant is part of the public contract: the
//! desktop install sheet (Phase F Stream H) and the `root install`
//! exit-code mapping (Step 5) both depend on this shape. See
//! `docs/2026-04-27-phase-f-trust-verify-design.md` §4 for the badge
//! + UX table.

use serde::{Deserialize, Serialize};
use tr_format::TrustTier;

/// Outcome of [`crate::Verifier::verify`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    /// The pack passed every check the verifier ran.
    Verified(VerifiedDetails),

    /// The pack declares [`TrustTier::T0`] (or has no signature
    /// attached despite a higher declared tier) and the policy demands
    /// a signature.
    Unsigned,

    /// The pack failed an integrity or signature check. Always
    /// fail-closed.
    Tampered(TamperedKind),

    /// The pack's content hash is on the revocation deny-list.
    Revoked(RevokedDetails),

    /// The pack is signed by an author key the local
    /// [`crate::AuthorKeyStore`] does not contain.
    KeyUnknown {
        /// The id the pack claims it was signed by.
        key_id: String,
    },

    /// The local revocation cache is older than the configured stale
    /// grace window and cannot be refreshed. Caller must refuse —
    /// see Phase F design §3 (hard-refuse policy).
    StaleCache {
        /// Approximate age in days for user display.
        age_days: u64,
    },

    /// The verifier recognises the declared trust tier but does not yet
    /// implement its protocol. Phase F.1 ships T0 + T1 only; T2+
    /// Sigstore lands in Step 4b. Fail-closed: a pack we cannot verify
    /// is never marked [`Verdict::Verified`].
    Unsupported {
        /// The declared trust tier the verifier could not handle.
        tier: TrustTier,
        /// User-facing reason string (Phase F design §4 wording).
        reason: String,
    },
}

/// Details accompanying a successful verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedDetails {
    /// The trust tier the pack was verified at.
    pub tier: TrustTier,
    /// For T1, the author key id; for T2+ (Step 4b), the Sigstore
    /// cert subject identity.
    pub author_id: Option<String>,
    /// For T2+ (Step 4b), the Rekor transparency log index containing
    /// the inclusion proof.
    pub sigstore_log_index: Option<u64>,
    /// Age of the revocation snapshot consulted, in seconds. Useful
    /// for ops dashboards.
    pub revocation_freshness_secs: u64,
}

/// Details accompanying a `Verdict::Revoked` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokedDetails {
    /// The advisory describing why the pack was revoked.
    pub advisory: tr_revocation::Advisory,
}

/// What kind of tampering was detected. Each variant carries enough
/// context to produce a developer-friendly error, while the user-facing
/// surface only needs the discriminant ("Tampered").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum TamperedKind {
    /// `manifest.compute_content_hash()` does not match
    /// `manifest.content_hash`. Someone edited a manifest field after
    /// the pack was built.
    ManifestHashMismatch {
        /// The hash carried by the pack.
        expected: String,
        /// The hash the verifier computed from the manifest body.
        actual: String,
    },
    /// The archive bytes are corrupt or fail an internal consistency
    /// check (in this Phase, reserved for future use; Phase F.1 does
    /// not produce this variant directly because [`tr_format::reader`]
    /// rejects corrupt archives at parse time).
    ArchiveCorrupt(String),
    /// The signature payload could not be verified against the
    /// expected key.
    SignaturePayloadMismatch,
}
