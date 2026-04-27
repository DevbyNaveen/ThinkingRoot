//! The verification pipeline.
//!
//! Five-step pipeline (first failure short-circuits) per
//! `docs/2026-04-27-phase-f-trust-verify-design.md` §2.2:
//!
//! 1. Manifest self-hash recompute.
//! 2. Archive content hash check (already done by `tr_format::reader`).
//! 3. Revocation cache consult.
//! 4. Trust-tier verification.
//! 5. Return [`Verdict::Verified`] with the gathered details.

use std::sync::Arc;

use ed25519_dalek::{Signature, Verifier as Ed25519Verifier, VerifyingKey};
use tr_format::{TrustTier, reader::Pack};
use tr_revocation::{FreshnessState, RevocationCache};

use crate::error::Result;
use crate::keys::AuthorKeyStore;
use crate::verdict::{RevokedDetails, TamperedKind, Verdict, VerifiedDetails};

/// Per-call configuration passed to [`Verifier::new`]. The CLI
/// constructs one of these per `root install` invocation; the desktop
/// app reuses one across the lifetime of the install sheet.
#[derive(Clone)]
pub struct VerifierConfig {
    /// Shared revocation cache.
    pub revocation: Arc<RevocationCache>,
    /// Local trust store of author public keys.
    pub author_keys: Arc<AuthorKeyStore>,
    /// Minimum trust tier required for a [`Verdict::Verified`] result.
    /// Default: [`TrustTier::T1`] for `https://` installs and
    /// [`TrustTier::T0`] for `--local-only` installs (the CLI sets the
    /// default per pack-ref kind).
    pub require_min_tier: TrustTier,
    /// If `true`, [`TrustTier::T0`] packs verify successfully even when
    /// `require_min_tier` is higher. The CLI flips this on with the
    /// `--allow-unsigned` flag.
    pub allow_unsigned: bool,
}

/// The verification entry point. Cheap to construct; calls
/// [`Verifier::verify`] for each pack.
pub struct Verifier {
    config: VerifierConfig,
}

impl Verifier {
    /// Construct a new verifier with the given configuration.
    pub fn new(config: VerifierConfig) -> Self {
        Self { config }
    }

    /// Run the full pipeline against an opened pack.
    ///
    /// Returns a [`Verdict`] for every successful pipeline run; the
    /// only `Err` return is for failures that prevent the verifier from
    /// running at all (revocation cache I/O failure with no fallback).
    pub async fn verify(&self, pack: &Pack) -> Result<Verdict> {
        // Step 1 — manifest self-hash.
        if let Some(verdict) = check_manifest_hash(pack) {
            return Ok(verdict);
        }

        // Step 2 — archive bytes hash. tr_format::reader already
        // computed `pack.content_bytes_hash` from the raw archive
        // bytes; the registry-side BLAKE3 cross-check happened in the
        // CLI before we got here. There is no second cross-check at
        // this layer.

        // Step 3 — revocation. Fail-closed if the cache is unavailable
        // and we have no fallback snapshot at all.
        let (snapshot, freshness) = self.config.revocation.load_or_refresh().await?;
        if matches!(freshness, FreshnessState::Expired) {
            return Ok(Verdict::StaleCache {
                age_days: stale_grace_days(self.config.revocation.config()),
            });
        }
        if let Some(advisory) = RevocationCache::is_revoked(&snapshot, &pack.content_bytes_hash) {
            return Ok(Verdict::Revoked(RevokedDetails {
                advisory: advisory.clone(),
            }));
        }
        let revocation_freshness_secs = freshness_secs(freshness, self.config.revocation.config());

        // Step 4 — trust-tier verification.
        let tier = pack.manifest.trust_tier;
        match tier {
            TrustTier::T0 => self.verify_t0(tier, revocation_freshness_secs),
            TrustTier::T1 => self.verify_t1(pack, tier, revocation_freshness_secs),
            TrustTier::T2 | TrustTier::T3 | TrustTier::T4 => {
                tracing::warn!(
                    ?tier,
                    "T2+ Sigstore verification is not yet wired (Phase F.1b / Step 4b)"
                );
                Ok(Verdict::Unsupported {
                    tier,
                    reason: "Sigstore trust root not yet bundled — see Phase F design §11"
                        .to_string(),
                })
            }
        }
    }

    fn verify_t0(&self, tier: TrustTier, revocation_freshness_secs: u64) -> Result<Verdict> {
        if self.config.require_min_tier > TrustTier::T0 && !self.config.allow_unsigned {
            return Ok(Verdict::Unsigned);
        }
        Ok(Verdict::Verified(VerifiedDetails {
            tier,
            author_id: None,
            sigstore_log_index: None,
            revocation_freshness_secs,
        }))
    }

    fn verify_t1(
        &self,
        pack: &Pack,
        tier: TrustTier,
        revocation_freshness_secs: u64,
    ) -> Result<Verdict> {
        let sig_bytes = match pack.entry("signatures/author.sig") {
            Some(b) => b,
            None => return Ok(Verdict::Unsigned),
        };
        let signature = match Signature::from_slice(sig_bytes) {
            Ok(s) => s,
            Err(_) => {
                return Ok(Verdict::Tampered(TamperedKind::SignaturePayloadMismatch));
            }
        };

        let key_id = match pack.manifest.authors.first() {
            Some(id) if !id.is_empty() => id.clone(),
            _ => return Ok(Verdict::Unsigned),
        };

        let trusted = match self.config.author_keys.get(&key_id) {
            Some(k) => k,
            None => return Ok(Verdict::KeyUnknown { key_id }),
        };

        let verifying = match VerifyingKey::from_bytes(&trusted.ed25519_public) {
            Ok(v) => v,
            Err(_) => {
                tracing::error!(key_id, "trusted key has invalid Ed25519 bytes");
                return Ok(Verdict::Tampered(TamperedKind::SignaturePayloadMismatch));
            }
        };

        let canonical = match pack.manifest.canonical_bytes_for_hashing() {
            Ok(b) => b,
            Err(_) => {
                return Ok(Verdict::Tampered(TamperedKind::ManifestHashMismatch {
                    expected: pack.manifest.content_hash.clone(),
                    actual: String::new(),
                }));
            }
        };

        match verifying.verify(&canonical, &signature) {
            Ok(()) => Ok(Verdict::Verified(VerifiedDetails {
                tier,
                author_id: Some(key_id),
                sigstore_log_index: None,
                revocation_freshness_secs,
            })),
            Err(_) => Ok(Verdict::Tampered(TamperedKind::SignaturePayloadMismatch)),
        }
    }
}

fn check_manifest_hash(pack: &Pack) -> Option<Verdict> {
    // If `content_hash` is empty, treat as not yet finalised — refuse.
    if pack.manifest.content_hash.is_empty() {
        return Some(Verdict::Tampered(TamperedKind::ManifestHashMismatch {
            expected: String::new(),
            actual: "<recomputed-blank>".to_string(),
        }));
    }
    let actual = match pack.manifest.compute_content_hash() {
        Ok(h) => h,
        Err(_) => {
            return Some(Verdict::Tampered(TamperedKind::ManifestHashMismatch {
                expected: pack.manifest.content_hash.clone(),
                actual: String::new(),
            }));
        }
    };
    if actual != pack.manifest.content_hash {
        return Some(Verdict::Tampered(TamperedKind::ManifestHashMismatch {
            expected: pack.manifest.content_hash.clone(),
            actual,
        }));
    }
    None
}

fn freshness_secs(state: FreshnessState, config: &tr_revocation::CacheConfig) -> u64 {
    match state {
        FreshnessState::Fresh => 0,
        FreshnessState::Stale => config.fresh_ttl.as_secs(),
        FreshnessState::Expired | FreshnessState::Missing => config.stale_grace.as_secs(),
    }
}

fn stale_grace_days(config: &tr_revocation::CacheConfig) -> u64 {
    config.stale_grace.as_secs() / 86_400
}
