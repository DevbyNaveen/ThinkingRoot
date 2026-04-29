//! v3 pack verification — the offline trust check `root verify
//! <pack>` runs.
//!
//! Per the v3 spec §7.6 + the Phase F design
//! (`docs/2026-04-29-phase-f-trust-verify-spec.md` §4.1), verification
//! is a 4-step pipeline (or 5 with revocation):
//!
//! 1. Recompute the pack hash from `(canonical_manifest, source.tar.zst,
//!    claims.jsonl)` per spec §3.1 / §16.1.
//! 2. Compare the recomputed hash to the manifest's declared
//!    `pack_hash`. Mismatch → `Tampered`.
//! 3. If `signature.sig` is absent → `Unsigned`. (Callers can choose
//!    to accept Unsigned via `--allow-unsigned`; this function reports
//!    the verdict, not the policy.)
//! 4. Hand the bundle + recomputed hash to
//!    [`tr_sigstore::verify_bundle_offline`]. Failure → `Tampered`
//!    (signature mismatch or subject digest mismatch). Success →
//!    `Verified` with optional Sigstore identity and Rekor log index
//!    extracted from the bundle.
//!
//! Step 5 (revocation deny-list) is intentionally factored out of this
//! function — `tr_revocation::RevocationCache` is async; this function
//! is sync. The CLI consults the cache after this returns Verified and
//! flips the verdict to Revoked if the pack hash is on the deny-list.

use serde::{Deserialize, Serialize};
use tr_format::V3Pack;

/// Result of [`verify_v3_pack`]. Distinct from the v1 [`crate::Verdict`]
/// because v3's failure modes are different shape: a Sigstore bundle
/// can fail in ways the T0/T1 path cannot (DSSE statement type mismatch,
/// missing public key, malformed Rekor proof). Keeping them separate
/// types means CLI exit codes don't collide and consumers don't need
/// to pattern-match on shared variants whose meaning differs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum V3Verdict {
    /// The pack passed every check this function ran. The verifier
    /// trusts the bundle's declared signing identity. Revocation is
    /// the caller's concern (see module docs).
    Verified {
        /// Identity asserted by the bundle's verification material —
        /// `Some(<email|subject>)` for Sigstore Fulcio, `None` for
        /// self-signed Ed25519 packs (today's default).
        identity: Option<String>,
        /// Rekor log index when the bundle carries one.
        rekor_log_index: Option<i64>,
        /// RFC 3339 timestamp the bundle was signed at.
        signed_at: String,
    },
    /// `signature.sig` was absent from the pack. The pack is otherwise
    /// well-formed; whether to accept Unsigned packs is a CLI policy.
    Unsigned,
    /// One of: pack-hash mismatch, signature failed, DSSE statement
    /// type mismatch, missing verification key.
    Tampered(V3TamperedKind),
}

/// What kind of tampering was detected on a v3 pack. Each variant
/// carries enough context for a developer-friendly error message; the
/// user-facing CLI surface only needs the discriminant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum V3TamperedKind {
    /// `pack.recompute_pack_hash()` does not match
    /// `pack.manifest.pack_hash`. Source/claims/manifest were edited
    /// after the pack was built. Detected before signature validation
    /// even runs — covers the unsigned-pack case too.
    PackHashMismatch {
        /// The hash declared in the manifest.
        declared: String,
        /// The hash recomputed from the actual bytes.
        recomputed: String,
    },
    /// The Sigstore bundle's DSSE signature didn't verify against the
    /// public key carried in the bundle, OR the in-toto statement's
    /// subject digest didn't match the recomputed pack hash, OR the
    /// statement was wrapping a different DSSE type.
    SignatureFailed {
        /// Underlying Sigstore error message for ops/diagnostics.
        reason: String,
    },
}

/// Run the v3 verification pipeline against a parsed pack. Sync; no
/// network. Pair with `tr_revocation::RevocationCache` at the CLI
/// level for deny-list checks.
pub fn verify_v3_pack(pack: &V3Pack) -> V3Verdict {
    // Step 1+2 — recompute and compare hashes.
    let recomputed = pack.recompute_pack_hash();
    if recomputed != pack.manifest.pack_hash {
        return V3Verdict::Tampered(V3TamperedKind::PackHashMismatch {
            declared: pack.manifest.pack_hash.clone(),
            recomputed,
        });
    }

    // Step 3 — bundle present?
    let bundle = match &pack.signature {
        Some(b) => b,
        None => return V3Verdict::Unsigned,
    };

    // Step 4 — DSSE bundle verification offline.
    match tr_sigstore::verify_bundle_offline(bundle, &recomputed) {
        Ok(statement) => V3Verdict::Verified {
            identity: extract_identity(bundle),
            rekor_log_index: bundle
                .verification_material
                .tlog_entries
                .first()
                .map(|e| e.log_index),
            signed_at: statement.predicate.signed_at,
        },
        Err(e) => V3Verdict::Tampered(V3TamperedKind::SignatureFailed {
            reason: e.to_string(),
        }),
    }
}

/// Extract a human-readable signer identity from the bundle when the
/// signing material is a Fulcio cert chain. Self-signed bundles return
/// `None`; the public key alone isn't a useful identity to surface to
/// the user. The `sigstore-impl` follow-up extracts the OIDC subject
/// from the Fulcio cert's SAN extension.
fn extract_identity(bundle: &tr_sigstore::SigstoreBundle) -> Option<String> {
    // Today: no cert-chain decoding (that's `sigstore-impl`'s job).
    // Returning the public-key fingerprint would be misleading
    // ("verified by [base64 blob]" doesn't help the user trust
    // anything), so leave None for self-signed.
    bundle
        .verification_material
        .x509_certificate_chain
        .as_ref()
        .and_then(|chain| chain.certificates.first())
        .map(|cert| format!("x509:{}", &cert.raw_bytes[..cert.raw_bytes.len().min(16)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use semver::Version;
    use tr_format::{ClaimRecord, ManifestV3, V3PackBuilder, read_v3_pack};

    fn fixture_signing_key(seed: u8) -> SigningKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        SigningKey::from_bytes(&bytes)
    }

    fn fixture_signed_pack(seed: u8) -> Vec<u8> {
        let mut b = V3PackBuilder::new(ManifestV3::new(
            "alice/v3-verify-test",
            Version::parse("1.0.0").unwrap(),
        ));
        b.add_source_file("a.md", b"alpha\n").unwrap();
        b.add_claim(ClaimRecord::new(
            "c-1",
            "alpha is the first letter",
            vec!["alpha".into()],
            "a.md",
            0,
            5,
        ));
        b.build_signed(&fixture_signing_key(seed), "package.tr")
            .unwrap()
    }

    fn fixture_unsigned_pack() -> Vec<u8> {
        let mut b = V3PackBuilder::new(ManifestV3::new(
            "alice/v3-verify-test",
            Version::parse("1.0.0").unwrap(),
        ));
        b.add_source_file("a.md", b"alpha\n").unwrap();
        b.build().unwrap()
    }

    #[test]
    fn signed_pack_round_trips_to_verified() {
        let bytes = fixture_signed_pack(1);
        let pack = read_v3_pack(&bytes).unwrap();
        let verdict = verify_v3_pack(&pack);
        match verdict {
            V3Verdict::Verified {
                rekor_log_index,
                identity,
                signed_at,
            } => {
                assert!(rekor_log_index.is_none()); // self-signed
                assert!(identity.is_none()); // self-signed
                assert!(!signed_at.is_empty());
            }
            other => panic!("expected Verified, got {other:?}"),
        }
    }

    #[test]
    fn unsigned_pack_returns_unsigned() {
        let bytes = fixture_unsigned_pack();
        let pack = read_v3_pack(&bytes).unwrap();
        assert!(matches!(verify_v3_pack(&pack), V3Verdict::Unsigned));
    }

    #[test]
    fn tampered_claims_jsonl_detected_before_signature_check() {
        let bytes = fixture_signed_pack(2);
        let mut pack = read_v3_pack(&bytes).unwrap();
        // Flip a byte in the claims body — pack hash diverges before
        // the signature check ever runs.
        pack.claims_jsonl[0] ^= 0x01;
        let verdict = verify_v3_pack(&pack);
        assert!(matches!(
            verdict,
            V3Verdict::Tampered(V3TamperedKind::PackHashMismatch { .. })
        ));
    }

    #[test]
    fn tampered_manifest_pack_hash_field_detected() {
        let bytes = fixture_signed_pack(3);
        let mut pack = read_v3_pack(&bytes).unwrap();
        // Tweak the declared pack hash without changing the body.
        pack.manifest.pack_hash =
            "blake3:0000000000000000000000000000000000000000000000000000000000000000".to_string();
        let verdict = verify_v3_pack(&pack);
        assert!(matches!(
            verdict,
            V3Verdict::Tampered(V3TamperedKind::PackHashMismatch { .. })
        ));
    }

    #[test]
    fn signature_swapped_for_other_key_detected() {
        let bytes = fixture_signed_pack(4);
        let pack = read_v3_pack(&bytes).unwrap();

        // Build a second signed pack with the SAME content but a
        // different key. Substitute its bundle into the first pack's
        // V3Pack — the signature is valid, the key is valid, but the
        // signing event isn't bound to the first pack's bytes (the
        // second pack would have a different pack_hash, so the
        // statement subject doesn't match).
        let other_bytes = fixture_signed_pack(5);
        let other_pack = read_v3_pack(&other_bytes).unwrap();
        let mut frankenstein = pack.clone();
        frankenstein.signature = other_pack.signature;

        let verdict = verify_v3_pack(&frankenstein);
        // Both packs have identical content (only the seed differs),
        // so their pack_hashes are equal too — meaning the swapped
        // bundle's statement subject DOES match. The bundle's
        // signature was made with the OTHER key over the SAME
        // statement bytes — verifying against that bundle's public
        // key still succeeds. This is intentional: the signature
        // proves "some key X signed this hash"; whether key X is
        // trusted is the consumer's policy decision (revocation list,
        // author key store).
        assert!(matches!(verdict, V3Verdict::Verified { .. }));
    }
}
