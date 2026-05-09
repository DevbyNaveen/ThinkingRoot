//! Author-key verification.
//!
//! Distinct from the Sigstore Bundle path that `verify_v3_pack` runs.
//! Sigstore covers transparency-log presence and short-lived
//! cosign/Fulcio identity (T2 trust). This module covers T1: the
//! author of a `.tr` pack signs the canonical manifest bytes with
//! their long-lived Ed25519 key, and the manifest carries a DID
//! pointer (`manifest.author_key_id`) the verifier resolves through
//! `tr-identity` to fetch the signer's public key.
//!
//! # Why a separate verifier
//!
//! T1 and T2 stack — a pack can carry both an author signature
//! (T1) and a Sigstore Bundle (T2). They check different things:
//!
//! - **T1 / [`AuthorVerifier`]**: long-lived author identity, DID-
//!   resolved public key, signature over canonical manifest bytes.
//!   "I, alice@example.com, claim authorship."
//! - **T2 / `verify_v3_pack`**: short-lived cosign identity, Rekor
//!   transparency-log presence, signature over `pack_hash`. "An
//!   identity-proof presented to Fulcio at time T was logged."
//!
//! Both can be present, both can be absent, and they fail
//! independently. Callers compose verdicts as they see fit — the
//! `root install` flow today rejects packs whose Sigstore verdict
//! isn't `Verified`; T1 will be additive once the publish path lands
//! in cloud `services/registry`.
//!
//! # Honesty constraints
//!
//! - **No silent fallback** — if the DID resolves but no key
//!   verifies, we return [`AuthorVerdict::KeyMismatch`], not a
//!   string-typed "key error". The caller can distinguish "we tried
//!   and failed" from "we couldn't try" ([`Error::AuthorKeyResolutionFailed`])
//!   from "we didn't have to try" ([`AuthorVerdict::Unsigned`]).
//! - **No fragment hand-waving** — when the DID document
//!   advertises multiple keys we try each one. v0.1 of the
//!   resolver doesn't carry per-key fragment IDs (`tr-identity`
//!   change tracked separately); when that lands, this module
//!   tightens to single-key match.

use tr_format::ManifestV3;
use tr_identity::{Did, DidResolver};

use crate::error::{Error, Result};

/// Verifier wrapper that consumes a [`DidResolver`]. Construct once
/// per process; the resolver is stateless and clone-safe.
///
/// ```no_run
/// # async fn run() {
/// use tr_verify::AuthorVerifier;
/// use tr_identity::did::DidWebResolver;
///
/// let verifier = AuthorVerifier::new(DidWebResolver::new());
/// // verifier.verify_author(&manifest, &signature_bytes).await
/// # }
/// ```
pub struct AuthorVerifier<R: DidResolver> {
    resolver: R,
}

impl<R: DidResolver> AuthorVerifier<R> {
    /// Build a new verifier that uses the supplied resolver. The
    /// resolver is owned for the lifetime of the verifier so callers
    /// don't have to juggle borrows across `.await` boundaries.
    pub fn new(resolver: R) -> Self {
        Self { resolver }
    }

    /// Verify an author signature against the manifest's
    /// canonical-bytes-for-hashing.
    ///
    /// The canonical bytes are exactly what the writer hashed into
    /// `pack_hash`'s preimage (`manifest.canonical_bytes_for_hashing()`).
    /// Re-using that input means the signed bytes are stable across
    /// any pack-internal transformation that preserves `pack_hash`.
    ///
    /// Behaviour matrix:
    ///
    /// | `author_key_id` | `signature` empty | Verdict |
    /// |---|---|---|
    /// | None | yes | [`AuthorVerdict::Unsigned`] (legitimate) |
    /// | None | no | [`AuthorVerdict::KeyMissing`] |
    /// | Some(did) | yes | [`AuthorVerdict::KeyMissing`] |
    /// | Some(malformed) | _ | `Err(InvalidAuthorKey)` |
    /// | Some(did) | non-empty, DID resolves | [`AuthorVerdict::Verified`] / [`AuthorVerdict::KeyMismatch`] |
    /// | Some(did) | non-empty, DID fails | `Err(AuthorKeyResolutionFailed)` |
    pub async fn verify_author(
        &self,
        manifest: &ManifestV3,
        signature: &[u8],
    ) -> Result<AuthorVerdict> {
        let message = manifest.canonical_bytes_for_hashing();
        match (manifest.author_key_id.as_deref(), signature.is_empty()) {
            (None, true) => Ok(AuthorVerdict::Unsigned),
            (None, false) => Ok(AuthorVerdict::KeyMissing),
            (Some(_), true) => Ok(AuthorVerdict::KeyMissing),
            (Some(did_with_fragment), false) => {
                self.verify_with(did_with_fragment, &message, signature).await
            }
        }
    }

    /// Lower-level entry: verify an arbitrary message against a DID.
    /// Used by [`AuthorVerifier::verify_author`] and exposed for
    /// callers that sign payloads other than the manifest itself
    /// (e.g. compliance bundle signatures).
    pub async fn verify_with(
        &self,
        did_with_fragment: &str,
        message: &[u8],
        signature: &[u8],
    ) -> Result<AuthorVerdict> {
        let core = did_with_fragment
            .split('#')
            .next()
            .unwrap_or(did_with_fragment);
        let did = Did::parse(core).map_err(|e| Error::InvalidAuthorKey {
            did: did_with_fragment.to_string(),
            reason: e.to_string(),
        })?;

        let resolved = self.resolver.resolve(&did).await.map_err(|e| {
            Error::AuthorKeyResolutionFailed {
                did: did_with_fragment.to_string(),
                source: e,
            }
        })?;

        if resolved.keys.is_empty() {
            return Ok(AuthorVerdict::KeyMismatch);
        }
        // Try each advertised key; if any verifies, the signature is
        // accepted. v0.2 will tighten this to a single fragment-bound
        // key once `tr-identity::ResolvedDid` carries per-key IDs.
        for key in &resolved.keys {
            if key.verify(message, signature).is_ok() {
                return Ok(AuthorVerdict::Verified {
                    did: did.as_str().to_string(),
                });
            }
        }
        Ok(AuthorVerdict::KeyMismatch)
    }
}

/// Verdict from [`AuthorVerifier::verify_author`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorVerdict {
    /// The signature verified against an Ed25519 public key
    /// advertised by the DID document. The carried `did` string is
    /// the resolver-canonicalised core (no fragment).
    Verified {
        /// The DID that signed the manifest.
        did: String,
    },
    /// No `author_key_id` and no signature — the pack is intentionally
    /// unsigned. Not an error; callers decide policy.
    Unsigned,
    /// `author_key_id` is set but no signature was provided, OR a
    /// signature was provided but `author_key_id` is missing. Either
    /// way the pair is incomplete and verification cannot proceed.
    KeyMissing,
    /// The DID resolved but none of its advertised keys verified the
    /// signature against the canonical manifest bytes. Distinct from
    /// [`Error::AuthorKeyResolutionFailed`] — that means "we couldn't
    /// fetch the key at all".
    KeyMismatch,
}

impl AuthorVerdict {
    /// True only for [`AuthorVerdict::Verified`]. Convenience for the
    /// `root install --require-author-signature` path.
    pub fn is_verified(&self) -> bool {
        matches!(self, AuthorVerdict::Verified { .. })
    }
}
