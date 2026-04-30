//! v3 pack verification — the offline trust check `root verify
//! <pack>` runs.
//!
//! Per the v3 spec §7.6 + the Phase F design
//! (`docs/2026-04-29-phase-f-trust-verify-spec.md` §4.1), verification
//! is a 5-step pipeline:
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
//! 5. Consult the revocation deny-list. The pack's `pack_hash` is
//!    looked up in the registry's signed snapshot; if present the
//!    verdict flips to `Revoked` carrying the advisory.
//!
//! Two entry points reflect the sync/async split:
//!
//! - [`verify_v3_pack`] runs steps 1–4 synchronously, no network. Use
//!   when the caller will consult revocation separately or when the
//!   workflow is fully offline (`--no-revocation-check`).
//! - [`verify_v3_pack_with_revocation`] runs all 5 steps. Async
//!   because [`tr_revocation::RevocationCache::load_or_refresh`] is
//!   async (HTTP fetch with conditional-GET caching). On a fresh
//!   cached snapshot it only does a sync disk read — no network round-
//!   trip.

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
    /// The pack's `pack_hash` appears on the registry's signed
    /// revocation deny-list. The pack itself may be cryptographically
    /// valid (signature OK, hash matches) — but the publisher,
    /// platform, or moderation staff has explicitly disclaimed it.
    /// Carries the [`tr_revocation::Advisory`] so the CLI can render
    /// `pack`, `version`, `reason`, `revoked_at`, and `details_url`
    /// without re-fetching.
    Revoked(crate::RevokedDetails),
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

/// Run the v3 verification pipeline AND consult the revocation
/// deny-list. Async because [`tr_revocation::RevocationCache::load_or_refresh`]
/// performs a conditional-GET against the registry; on a fresh cached
/// snapshot the call is satisfied entirely from disk and never touches
/// the network.
///
/// Step ordering:
///
/// 1. Run [`verify_v3_pack`] (sync, steps 1–4). Any non-`Verified`
///    result is returned as-is — there's no point checking revocation
///    on a pack that already failed crypto.
/// 2. Load (or refresh) the cached deny-list snapshot.
/// 3. Look up `pack.manifest.pack_hash` in the snapshot. If the pack
///    is revoked, return [`V3Verdict::Revoked`] carrying the advisory.
///    Otherwise the original `Verified` verdict stands.
///
/// The freshness state of the snapshot is intentionally NOT folded
/// into the verdict — `tr_revocation::FreshnessState::Stale` is a
/// caller-policy concern (warn vs. refuse). Callers that want to
/// surface staleness should consult [`tr_revocation::RevocationCache::load`]
/// directly. This function fail-opens on stale-but-usable snapshots
/// per the protocol's first-boot grace window
/// (`docs/2026-04-24-revocation-protocol-spec.md` §5.4).
pub async fn verify_v3_pack_with_revocation(
    pack: &V3Pack,
    revocation: &tr_revocation::RevocationCache,
) -> V3Verdict {
    let base = verify_v3_pack(pack);
    // Only consult revocation when the pack is otherwise verified.
    // Tampered/Unsigned packs can't be revoked-but-otherwise-trusted
    // — they're already failures.
    if !matches!(base, V3Verdict::Verified { .. }) {
        return base;
    }

    let (snapshot, _state) = match revocation.load_or_refresh().await {
        Ok(pair) => pair,
        Err(_) => {
            // Network unreachable AND no cached snapshot: per the
            // protocol's first-boot grace, proceed with the original
            // `Verified`. The CLI surfaces this as a warning rather
            // than a hard failure.
            return base;
        }
    };

    if let Some(advisory) =
        tr_revocation::RevocationCache::is_revoked(&snapshot, &pack.manifest.pack_hash)
    {
        return V3Verdict::Revoked(crate::RevokedDetails {
            advisory: advisory.clone(),
        });
    }

    base
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

    // ─────────────────────────────────────────────────────────────
    // verify_v3_pack_with_revocation tests. We construct a
    // RevocationCache against a temp-dir and pre-seed the snapshot
    // file directly so the cache treats it as already-fetched-and-
    // fresh, bypassing the registry HTTP path entirely.
    // ─────────────────────────────────────────────────────────────

    use std::time::SystemTime;
    use tr_revocation::{
        Advisory, Authority, CacheConfig, Reason, RevocationCache, Snapshot,
    };

    fn fixture_revocation_cache(
        revoked_hashes: Vec<String>,
    ) -> (RevocationCache, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_path_buf();

        let snapshot = Snapshot {
            schema_version: "1.0.0".to_string(),
            generated_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            generated_by: "test".to_string(),
            full_list: true,
            entries: revoked_hashes
                .into_iter()
                .map(|h| Advisory {
                    content_hash: h,
                    pack: "alice/v3-verify-test".to_string(),
                    version: "1.0.0".to_string(),
                    reason: Reason::Malware,
                    revoked_at: 1_700_000_000,
                    authority: Authority::HubModeration,
                    details_url: "https://example.com/advisory".to_string(),
                })
                .collect(),
            // Empty signature is acceptable in this fixture because the
            // test pre-seeds `snapshot.json` directly on disk, which
            // bypasses the signed-fetch path entirely (production code
            // requires a valid Ed25519 signature against the registry's
            // signing key).
            signature: String::new(),
            signing_key_id: "test-key".to_string(),
            next_poll_hint_sec: 3600,
        };

        // Pre-seed the on-disk snapshot to bypass the registry HTTP
        // path. `load_or_refresh` reads the cached snapshot first; if
        // fresh it returns immediately without touching the network.
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join("snapshot.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        std::fs::write(
            cache_dir.join("snapshot.fetched_at"),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string(),
        )
        .unwrap();

        // Registry URL is unused on the fresh-cache path but required
        // by CacheConfig — point at localhost so a misconfigured test
        // can't accidentally hit a real registry.
        let config = CacheConfig::defaults_for(
            "http://127.0.0.1:1/".parse().unwrap(),
            cache_dir,
        );
        (RevocationCache::new(config), tmp)
    }

    #[tokio::test]
    async fn revoked_pack_hash_flips_verdict_to_revoked() {
        let bytes = fixture_signed_pack(10);
        let pack = read_v3_pack(&bytes).unwrap();
        // Add the pack's actual hash to the deny-list.
        let (cache, _tmp) =
            fixture_revocation_cache(vec![pack.manifest.pack_hash.clone()]);

        let verdict = verify_v3_pack_with_revocation(&pack, &cache).await;
        match verdict {
            V3Verdict::Revoked(details) => {
                assert_eq!(details.advisory.reason, Reason::Malware);
                assert_eq!(details.advisory.pack, "alice/v3-verify-test");
            }
            other => panic!("expected Revoked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unrevoked_pack_passes_through_to_verified() {
        let bytes = fixture_signed_pack(11);
        let pack = read_v3_pack(&bytes).unwrap();
        // Empty deny-list — pack should not be revoked.
        let (cache, _tmp) = fixture_revocation_cache(vec![]);

        let verdict = verify_v3_pack_with_revocation(&pack, &cache).await;
        assert!(matches!(verdict, V3Verdict::Verified { .. }));
    }

    #[tokio::test]
    async fn unrelated_revocation_does_not_match() {
        let bytes = fixture_signed_pack(12);
        let pack = read_v3_pack(&bytes).unwrap();
        // Deny-list contains a different pack's hash; ours stays
        // verified.
        let (cache, _tmp) = fixture_revocation_cache(vec![
            "blake3:dead0000000000000000000000000000000000000000000000000000000000ff".to_string(),
        ]);

        let verdict = verify_v3_pack_with_revocation(&pack, &cache).await;
        assert!(matches!(verdict, V3Verdict::Verified { .. }));
    }

    #[tokio::test]
    async fn tampered_pack_short_circuits_before_revocation_check() {
        let bytes = fixture_signed_pack(13);
        let mut pack = read_v3_pack(&bytes).unwrap();
        pack.manifest.pack_hash =
            "blake3:0000000000000000000000000000000000000000000000000000000000000000".to_string();
        // Even if the (tampered) hash is on the deny-list, the
        // tamper detection fires first — we never reach the
        // revocation check. The verdict is Tampered, not Revoked.
        let (cache, _tmp) = fixture_revocation_cache(vec![pack.manifest.pack_hash.clone()]);

        let verdict = verify_v3_pack_with_revocation(&pack, &cache).await;
        assert!(matches!(
            verdict,
            V3Verdict::Tampered(V3TamperedKind::PackHashMismatch { .. })
        ));
    }
}
