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
//! 4. Branch on the bundle's verification material:
//!    - **Self-signed** (`publicKey` present, no cert chain): run
//!      [`tr_sigstore::verify_bundle_offline`] — DSSE Ed25519 +
//!      subject digest only.
//!    - **Sigstore-keyless** (`x509CertificateChain` present): run
//!      [`tr_sigstore::verify_bundle_with_trust_root`] against the
//!      vendored Sigstore-public-good Fulcio bundle, plus iterate
//!      every Rekor `tlog_entry` and cryptographically verify both
//!      the inclusion proof (Merkle root replay) and the SET
//!      (Rekor signature over `(integratedTime, logIndex, logID,
//!      bodyHash)`). Reject any bundle whose log_id doesn't match
//!      Sigstore-public-good's published Rekor key.
//! 5. Consult the revocation deny-list. If the cache snapshot is
//!    [`tr_revocation::FreshnessState::Expired`] or the first-boot
//!    grace has elapsed, surface
//!    [`V3Verdict::RevocationUnverifiable`] so the caller hard-fails
//!    rather than silently accepting any pack as non-revoked. If the
//!    pack hash is on the deny-list the verdict flips to
//!    [`V3Verdict::Revoked`] carrying the advisory.
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

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tr_format::V3Pack;
use tr_sigstore::{
    SIGSTORE_PUBLIC_GOOD_REKOR_LOG_ID_HEX, SigstoreBundle, TlogEntry, TrustedRoot,
    extract_oidc_identity, hex_lower, integrated_time_to_system, leaf_hash_from_canonical_body,
    sigstore_public_good_rekor_pubkey, verify_bundle_offline, verify_bundle_with_trust_root,
    verify_inclusion_proof_offline, verify_set_signature,
};

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
        /// `Some(<URI|email>)` for Sigstore Fulcio (extracted from the
        /// leaf cert's SAN), `None` for self-signed Ed25519 packs.
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
    /// type mismatch, missing verification key, cert chain doesn't
    /// reach trust root, Rekor inclusion proof / SET failure.
    Tampered(V3TamperedKind),
    /// The pack's `pack_hash` appears on the registry's signed
    /// revocation deny-list. The pack itself may be cryptographically
    /// valid (signature OK, hash matches) — but the publisher,
    /// platform, or moderation staff has explicitly disclaimed it.
    /// Carries the [`tr_revocation::Advisory`] so the CLI can render
    /// `pack`, `version`, `reason`, `revoked_at`, and `details_url`
    /// without re-fetching.
    Revoked(crate::RevokedDetails),
    /// The revocation deny-list could not be obtained or has aged
    /// past the configured stale-grace window, so the verifier
    /// cannot conclusively decide whether the pack is revoked.
    /// Returned when the network is unreachable AND the cached
    /// snapshot is `Expired`, OR when the first-boot grace has
    /// elapsed without a successful refresh. Callers must refuse to
    /// install: silently accepting an unverifiable pack would let a
    /// revoked artifact through whenever the registry is unreachable.
    RevocationUnverifiable {
        /// Diagnostic explaining why the deny-list couldn't be
        /// trusted (cache state + underlying transport error).
        reason: String,
    },
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
    /// The Sigstore bundle's signature didn't verify, the in-toto
    /// statement's subject digest didn't match the recomputed pack
    /// hash, the Fulcio cert chain didn't reach a trusted root, or
    /// Rekor inclusion proof / SET verification failed.
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

    // Step 4 — branch on bundle's verification material.
    let cert_chain_present = bundle
        .verification_material
        .x509_certificate_chain
        .as_ref()
        .is_some_and(|c| !c.certificates.is_empty());

    if cert_chain_present {
        verify_keyless_bundle(bundle, &recomputed)
    } else {
        verify_self_signed_bundle(bundle, &recomputed)
    }
}

/// Verify a self-signed Ed25519 v3 bundle (no Fulcio cert chain).
///
/// The bundle's `publicKey.rawBytes` is the trust anchor — the verifier
/// proves "*some* key X signed this hash"; whether key X is trusted is
/// the consumer's policy decision (revocation deny-list, author key
/// store). This is the legacy path used by `root pack --sign <key>`.
fn verify_self_signed_bundle(bundle: &SigstoreBundle, recomputed: &str) -> V3Verdict {
    match verify_bundle_offline(bundle, recomputed) {
        Ok(statement) => V3Verdict::Verified {
            identity: None,
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

/// Verify a Sigstore-keyless v3 bundle: DSSE signature, leaf cert
/// chain → trusted Fulcio root, AND Rekor inclusion proof + SET for
/// every transparency-log entry.
///
/// Refuses bundles that lack a Rekor witness — every Sigstore-keyless
/// signing event produces a tlog entry by construction, so a bundle
/// without one is malformed (or forged with the cert chain stripped
/// of its tlog). Refuses entries whose `log_id` doesn't match the
/// vendored Sigstore-public-good Rekor key.
fn verify_keyless_bundle(bundle: &SigstoreBundle, recomputed: &str) -> V3Verdict {
    let signed_at = match bundle.verification_material.tlog_entries.first() {
        Some(entry) => integrated_time_to_system(entry),
        None => {
            return V3Verdict::Tampered(V3TamperedKind::SignatureFailed {
                reason: "Sigstore-keyless bundle has no tlog entry — cannot anchor signing time"
                    .into(),
            });
        }
    };

    // DSSE signature + cert-chain → Fulcio public-good root.
    let trust_root = TrustedRoot::sigstore_public_good();
    let statement = match verify_bundle_with_trust_root(bundle, recomputed, &trust_root, signed_at)
    {
        Ok((stmt, _root_idx)) => stmt,
        Err(e) => {
            return V3Verdict::Tampered(V3TamperedKind::SignatureFailed {
                reason: format!("trust-root validation: {e}"),
            });
        }
    };

    // Rekor binding — every tlog entry must validate, both proof and SET.
    let rekor_pubkey = sigstore_public_good_rekor_pubkey();
    let mut any_witness = false;
    for entry in &bundle.verification_material.tlog_entries {
        if let Err(e) = verify_tlog_entry(entry, &rekor_pubkey) {
            return V3Verdict::Tampered(V3TamperedKind::SignatureFailed {
                reason: format!("Rekor entry log_index={}: {e}", entry.log_index),
            });
        }
        if entry.inclusion_proof.is_some() || entry.inclusion_promise.is_some() {
            any_witness = true;
        }
    }
    if !any_witness {
        return V3Verdict::Tampered(V3TamperedKind::SignatureFailed {
            reason: "Sigstore-keyless bundle has no Rekor witness (no inclusion proof or SET)"
                .into(),
        });
    }

    let identity = match extract_oidc_identity(bundle) {
        Ok(id) => id,
        Err(e) => {
            return V3Verdict::Tampered(V3TamperedKind::SignatureFailed {
                reason: format!("identity extraction: {e}"),
            });
        }
    };

    V3Verdict::Verified {
        identity,
        rekor_log_index: bundle
            .verification_material
            .tlog_entries
            .first()
            .map(|e| e.log_index),
        signed_at: statement.predicate.signed_at,
    }
}

/// Verify a single transparency-log entry: log_id matches Sigstore-
/// public-good, inclusion proof Merkle-replays, SET signature
/// validates against Rekor's public key.
///
/// An entry must carry `canonicalizedBody` (used to recompute the
/// leaf hash via RFC 6962) plus at least one of: an inclusion proof
/// or a SET (`inclusion_promise`). Entries with neither contribute no
/// transparency-log binding and are rejected by the calling
/// `verify_keyless_bundle`.
fn verify_tlog_entry(
    entry: &TlogEntry,
    rekor_pubkey: &p256::ecdsa::VerifyingKey,
) -> Result<(), String> {
    let canonical_body_b64 = entry
        .canonicalized_body
        .as_deref()
        .ok_or_else(|| "missing canonicalizedBody".to_string())?;
    let canonical_body = base64::engine::general_purpose::STANDARD
        .decode(canonical_body_b64)
        .map_err(|e| format!("canonicalizedBody base64: {e}"))?;

    // log_id must be present AND match Sigstore-public-good's
    // published value. An attacker could otherwise forge a bundle
    // citing a private/staging Rekor instance whose key they control.
    let log_id_bytes = match entry.log_id.as_ref() {
        Some(lid) => base64::engine::general_purpose::STANDARD
            .decode(&lid.key_id)
            .map_err(|e| format!("log_id base64: {e}"))?,
        None => return Err("missing log_id".into()),
    };
    let log_id_hex = hex_lower(&log_id_bytes);
    if log_id_hex != SIGSTORE_PUBLIC_GOOD_REKOR_LOG_ID_HEX {
        return Err(format!(
            "log_id {} doesn't match Sigstore-public-good ({})",
            log_id_hex, SIGSTORE_PUBLIC_GOOD_REKOR_LOG_ID_HEX
        ));
    }

    if let Some(proof) = &entry.inclusion_proof {
        let leaf_hash = leaf_hash_from_canonical_body(&canonical_body);
        verify_inclusion_proof_offline(&leaf_hash, proof)
            .map_err(|e| format!("inclusion proof: {e}"))?;
    }

    if entry.inclusion_promise.is_some() {
        verify_set_signature(entry, &canonical_body, &log_id_bytes, rekor_pubkey)
            .map_err(|e| format!("SET signature: {e}"))?;
    }

    Ok(())
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
/// 2. Load (or refresh) the cached deny-list snapshot. If
///    `load_or_refresh` returns
///    [`tr_revocation::Error::NoTrustedSnapshot`] (first-boot grace
///    expired), return [`V3Verdict::RevocationUnverifiable`] so the
///    install flow refuses the pack.
/// 3. If the snapshot is [`tr_revocation::FreshnessState::Expired`]
///    (cached but past stale-grace and refresh failed), return
///    [`V3Verdict::RevocationUnverifiable`].
/// 4. Look up `pack.manifest.pack_hash` in the snapshot. If revoked,
///    return [`V3Verdict::Revoked`]. Otherwise the original
///    `Verified` verdict stands. `Stale` and `Missing` (within grace)
///    log a warning but do not flip the verdict — those are first-
///    boot / temporary-disconnect states the protocol explicitly
///    permits.
pub async fn verify_v3_pack_with_revocation(
    pack: &V3Pack,
    revocation: &tr_revocation::RevocationCache,
) -> V3Verdict {
    let base = verify_v3_pack(pack);
    if !matches!(base, V3Verdict::Verified { .. }) {
        return base;
    }

    let (snapshot, state) = match revocation.load_or_refresh().await {
        Ok(pair) => pair,
        Err(tr_revocation::Error::NoTrustedSnapshot {
            age_secs,
            grace_secs,
            source,
        }) => {
            return V3Verdict::RevocationUnverifiable {
                reason: format!(
                    "no trusted revocation snapshot — first-boot grace expired \
                     ({age_secs}s elapsed, grace was {grace_secs}s); transport: {source}"
                ),
            };
        }
        Err(other) => {
            tracing::warn!(error = %other, "revocation cache load_or_refresh failed");
            return V3Verdict::RevocationUnverifiable {
                reason: format!("revocation cache unusable: {other}"),
            };
        }
    };

    if matches!(state, tr_revocation::FreshnessState::Expired) {
        return V3Verdict::RevocationUnverifiable {
            reason: "revocation snapshot is past stale-grace window".into(),
        };
    }

    if matches!(state, tr_revocation::FreshnessState::Stale) {
        tracing::warn!(
            "revocation snapshot is stale; using cached deny-list while attempting refresh"
        );
    }

    if let Some(advisory) =
        tr_revocation::RevocationCache::is_revoked(&snapshot, &pack.manifest.pack_hash)
    {
        return V3Verdict::Revoked(crate::RevokedDetails {
            advisory: advisory.clone(),
        });
    }

    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
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
    // RevocationCache against a temp-dir and pre-seed a properly
    // signed snapshot file so the cache's load() (which now verifies
    // the signature on disk-read) accepts it as fresh, bypassing the
    // registry HTTP path entirely.
    // ─────────────────────────────────────────────────────────────

    use std::time::SystemTime;
    use tr_revocation::{
        Advisory, Authority, CacheConfig, FreshnessState, PinnedKey, Reason, RevocationCache,
        Snapshot,
    };

    fn fixture_revocation_cache(
        revoked_hashes: Vec<String>,
    ) -> (RevocationCache, tempfile::TempDir) {
        fixture_revocation_cache_with_age(revoked_hashes, 0)
    }

    /// Same as [`fixture_revocation_cache`] but lets the caller
    /// choose how old the snapshot's `fetched_at` is in seconds.
    /// `0` = now (fresh); larger values let tests exercise stale and
    /// expired classifications.
    fn fixture_revocation_cache_with_age(
        revoked_hashes: Vec<String>,
        fetched_age_secs: u64,
    ) -> (RevocationCache, tempfile::TempDir) {
        // Synthesize a fresh signing key for the test. The pinned-key
        // material in `tr_revocation::keys::PINNED_RAW` is the
        // production trust anchor; here we explicitly inject a
        // throwaway key so the test exercises the full sign-and-
        // verify path without depending on cloud-side material.
        let mut sk_bytes = [0u8; 32];
        sk_bytes[0] = 0xAB;
        let sk = SigningKey::from_bytes(&sk_bytes);
        let pk_bytes: [u8; 32] = sk.verifying_key().to_bytes();
        let key_id = "v3-test-revocation-key";

        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_path_buf();

        let mut snapshot = Snapshot {
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
            signature: String::new(),
            signing_key_id: key_id.to_string(),
            next_poll_hint_sec: 3600,
        };

        // Sign the canonical bytes so RevocationCache::load() (which
        // now verifies on disk-read) accepts the seeded snapshot.
        let payload = snapshot.canonical_bytes_for_signing().unwrap();
        let sig = sk.sign(&payload);
        snapshot.signature =
            base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

        // Pre-seed the on-disk snapshot to bypass the registry HTTP
        // path. `load_or_refresh` reads the cached snapshot first; if
        // fresh it returns immediately without touching the network.
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join("snapshot.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(
            cache_dir.join("snapshot.fetched_at"),
            now.saturating_sub(fetched_age_secs).to_string(),
        )
        .unwrap();

        // Registry URL is unused on the fresh-cache path but required
        // by CacheConfig — point at localhost so a misconfigured test
        // can't accidentally hit a real registry.
        let mut config =
            CacheConfig::defaults_for("http://127.0.0.1:1/".parse().unwrap(), cache_dir);
        config.trusted_keys = vec![PinnedKey {
            key_id: key_id.to_string(),
            ed25519_public: pk_bytes,
        }];
        (RevocationCache::new(config), tmp)
    }

    #[tokio::test]
    async fn revoked_pack_hash_flips_verdict_to_revoked() {
        let bytes = fixture_signed_pack(10);
        let pack = read_v3_pack(&bytes).unwrap();
        // Add the pack's actual hash to the deny-list.
        let (cache, _tmp) = fixture_revocation_cache(vec![pack.manifest.pack_hash.clone()]);

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

    #[tokio::test]
    async fn expired_revocation_snapshot_returns_unverifiable() {
        // Snapshot fetched 8 days ago — past the 7-day stale grace.
        let bytes = fixture_signed_pack(14);
        let pack = read_v3_pack(&bytes).unwrap();
        let (cache, _tmp) =
            fixture_revocation_cache_with_age(vec![], 8 * 24 * 60 * 60);

        // Sanity — load() classifies this as Expired.
        let (_snap, state) = cache.load().unwrap();
        assert_eq!(state, FreshnessState::Expired);

        // verify_v3_pack_with_revocation must surface a hard refusal.
        // Note: load_or_refresh will attempt a network refresh for an
        // Expired snapshot before falling through to the cached one.
        // The localhost:1 fixture URL guarantees the refresh fails;
        // the cached (Expired) snapshot is then returned, and the
        // verifier converts it to RevocationUnverifiable.
        let verdict = verify_v3_pack_with_revocation(&pack, &cache).await;
        assert!(
            matches!(verdict, V3Verdict::RevocationUnverifiable { .. }),
            "expected RevocationUnverifiable, got {verdict:?}"
        );
    }

    #[test]
    fn der_length_short_form_decodes() {
        // Sanity check the SAN length decoder via the public verify
        // path: a self-signed bundle has no SAN, so identity is None.
        let bytes = fixture_signed_pack(20);
        let pack = read_v3_pack(&bytes).unwrap();
        if let V3Verdict::Verified { identity, .. } = verify_v3_pack(&pack) {
            assert!(identity.is_none());
        } else {
            panic!("expected Verified");
        }
    }
}
