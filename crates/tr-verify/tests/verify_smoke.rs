//! End-to-end smoke tests for the trust-verification pipeline.
//!
//! Each test builds a real `.tr` archive via `tr_format::writer`,
//! reads it via `tr_format::reader`, and runs `Verifier::verify`.
//! Revocation snapshots are signed with a deterministic test key and
//! written directly to a temp directory the [`RevocationCache`] reads.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use semver::Version;
use tempfile::TempDir;
use tr_format::{Manifest, TrustTier, reader, writer::PackBuilder};
use tr_revocation::{
    Advisory, Authority, CacheConfig, PinnedKey, Reason, RevocationCache, Snapshot,
};
use tr_verify::{
    AuthorKeyStore, RevokedDetails, TamperedKind, TrustedAuthorKey, Verdict, VerifiedDetails,
    Verifier, VerifierConfig,
};

// -----------------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------------

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn fresh_revocation_cache(
    cache_dir: PathBuf,
    revocation_key: &SigningKey,
    revoked_hashes: &[&str],
) -> Arc<RevocationCache> {
    let mut snap = Snapshot {
        schema_version: "1.0.0".into(),
        generated_at: 1_745_100_000,
        generated_by: "hub.example".into(),
        full_list: true,
        entries: revoked_hashes
            .iter()
            .map(|h| Advisory {
                content_hash: format!("blake3:{h}"),
                pack: "alice/thesis".into(),
                version: "0.1.0".into(),
                reason: Reason::Malware,
                revoked_at: 1_745_099_000,
                authority: Authority::HubScanner,
                details_url: "https://example/advisory".into(),
            })
            .collect(),
        signature: String::new(),
        signing_key_id: "rev-test".into(),
        next_poll_hint_sec: 3_600,
    };
    let payload = snap.canonical_bytes_for_signing().unwrap();
    let sig = revocation_key.sign(&payload);
    snap.signature = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(
        cache_dir.join("snapshot.json"),
        serde_json::to_vec(&snap).unwrap(),
    )
    .unwrap();
    std::fs::write(
        cache_dir.join("snapshot.fetched_at"),
        unix_now().to_string(),
    )
    .unwrap();

    Arc::new(RevocationCache::new(CacheConfig {
        registry_url: url::Url::parse("https://hub.example/").unwrap(),
        cache_dir,
        fresh_ttl: Duration::from_secs(60 * 60),
        stale_grace: Duration::from_secs(7 * 24 * 60 * 60),
        trusted_keys: vec![PinnedKey {
            key_id: "rev-test".into(),
            ed25519_public: revocation_key.verifying_key().to_bytes(),
        }],
        max_snapshot_bytes: 50 * 1024 * 1024,
    }))
}

fn expired_revocation_cache(cache_dir: PathBuf, revocation_key: &SigningKey) -> Arc<RevocationCache> {
    let cache = fresh_revocation_cache(cache_dir.clone(), revocation_key, &[]);
    // Roll fetched_at back ten days, past the 7-day stale_grace.
    let ten_days_ago = unix_now() - 10 * 24 * 60 * 60;
    std::fs::write(
        cache_dir.join("snapshot.fetched_at"),
        ten_days_ago.to_string(),
    )
    .unwrap();
    cache
}

fn build_unsigned_pack(name: &str, tier: TrustTier) -> Vec<u8> {
    let mut manifest = Manifest::new(name, Version::parse("0.1.0").unwrap(), "Apache-2.0");
    manifest.trust_tier = tier;
    let mut pb = PackBuilder::new(manifest);
    pb.put_text("artifacts/card.md", "# Hello").unwrap();
    pb.build().unwrap()
}

fn build_t1_signed_pack(name: &str, key: &SigningKey, key_id: &str) -> Vec<u8> {
    let mut manifest = Manifest::new(name, Version::parse("0.1.0").unwrap(), "Apache-2.0");
    manifest.trust_tier = TrustTier::T1;
    manifest.authors = vec![key_id.to_string()];

    // Freeze generated_at *before* signing — the signature payload is
    // canonical_bytes_for_hashing(), which includes generated_at.
    // PackBuilder::keep_generated_at() preserves the value through
    // build() so verify-side recomputation reproduces the same bytes.
    let canonical = manifest.canonical_bytes_for_hashing().unwrap();
    let signature = key.sign(&canonical);

    let mut pb = PackBuilder::new(manifest).keep_generated_at();
    pb.put_file("signatures/author.sig", &signature.to_bytes())
        .unwrap();
    pb.put_text("artifacts/card.md", "# T1 demo").unwrap();
    pb.build().unwrap()
}

fn cli_verifier_config(
    revocation: Arc<RevocationCache>,
    author_keys: Arc<AuthorKeyStore>,
    require_min_tier: TrustTier,
) -> VerifierConfig {
    VerifierConfig {
        revocation,
        author_keys,
        require_min_tier,
        allow_unsigned: false,
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[tokio::test]
async fn t0_unsigned_is_refused_when_min_tier_is_t1() {
    let tmp = TempDir::new().unwrap();
    let rev_key = signing_key(1);
    let revocation = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

    let bytes = build_unsigned_pack("alice/demo", TrustTier::T0);
    let pack = reader::read_bytes(&bytes).unwrap();

    let verifier = Verifier::new(cli_verifier_config(
        revocation,
        Arc::new(AuthorKeyStore::empty()),
        TrustTier::T1,
    ));
    let verdict = verifier.verify(&pack).await.unwrap();
    assert!(matches!(verdict, Verdict::Unsigned), "got {verdict:?}");
}

#[tokio::test]
async fn t0_unsigned_passes_when_allow_unsigned_is_true() {
    let tmp = TempDir::new().unwrap();
    let rev_key = signing_key(2);
    let revocation = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

    let bytes = build_unsigned_pack("alice/demo", TrustTier::T0);
    let pack = reader::read_bytes(&bytes).unwrap();

    let mut config = cli_verifier_config(
        revocation,
        Arc::new(AuthorKeyStore::empty()),
        TrustTier::T1,
    );
    config.allow_unsigned = true;
    let verifier = Verifier::new(config);
    let verdict = verifier.verify(&pack).await.unwrap();
    assert!(matches!(
        verdict,
        Verdict::Verified(VerifiedDetails {
            tier: TrustTier::T0,
            ..
        })
    ));
}

#[tokio::test]
async fn t1_round_trip_verifies_when_author_key_is_trusted() {
    let tmp = TempDir::new().unwrap();
    let rev_key = signing_key(3);
    let revocation = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

    let author = signing_key(4);
    let trusted = TrustedAuthorKey {
        key_id: "alice".into(),
        ed25519_public: author.verifying_key().to_bytes(),
    };
    let store = Arc::new(AuthorKeyStore::with_keys([trusted]));

    let bytes = build_t1_signed_pack("alice/thesis", &author, "alice");
    let pack = reader::read_bytes(&bytes).unwrap();

    let verifier = Verifier::new(cli_verifier_config(revocation, store, TrustTier::T1));
    let verdict = verifier.verify(&pack).await.unwrap();
    match verdict {
        Verdict::Verified(d) => {
            assert_eq!(d.tier, TrustTier::T1);
            assert_eq!(d.author_id.as_deref(), Some("alice"));
        }
        other => panic!("expected Verified(T1), got {other:?}"),
    }
}

#[tokio::test]
async fn t1_unknown_author_returns_key_unknown() {
    let tmp = TempDir::new().unwrap();
    let rev_key = signing_key(5);
    let revocation = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

    let author = signing_key(6);
    let bytes = build_t1_signed_pack("alice/thesis", &author, "alice-unknown");
    let pack = reader::read_bytes(&bytes).unwrap();

    let verifier = Verifier::new(cli_verifier_config(
        revocation,
        Arc::new(AuthorKeyStore::empty()),
        TrustTier::T1,
    ));
    let verdict = verifier.verify(&pack).await.unwrap();
    match verdict {
        Verdict::KeyUnknown { key_id } => assert_eq!(key_id, "alice-unknown"),
        other => panic!("expected KeyUnknown, got {other:?}"),
    }
}

#[tokio::test]
async fn t1_signature_with_wrong_key_returns_tampered() {
    let tmp = TempDir::new().unwrap();
    let rev_key = signing_key(7);
    let revocation = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

    let real_author = signing_key(8);
    let impostor = signing_key(9);

    // Pack signed by real_author, but trust store advertises a key
    // claiming to be the same id but with impostor's bytes.
    let trusted = TrustedAuthorKey {
        key_id: "alice".into(),
        ed25519_public: impostor.verifying_key().to_bytes(),
    };
    let store = Arc::new(AuthorKeyStore::with_keys([trusted]));

    let bytes = build_t1_signed_pack("alice/thesis", &real_author, "alice");
    let pack = reader::read_bytes(&bytes).unwrap();

    let verifier = Verifier::new(cli_verifier_config(revocation, store, TrustTier::T1));
    let verdict = verifier.verify(&pack).await.unwrap();
    assert!(matches!(
        verdict,
        Verdict::Tampered(TamperedKind::SignaturePayloadMismatch)
    ));
}

#[tokio::test]
async fn revoked_pack_returns_revoked_with_advisory() {
    let tmp = TempDir::new().unwrap();
    let rev_key = signing_key(10);

    let bytes = build_unsigned_pack("alice/bad", TrustTier::T0);
    let pack = reader::read_bytes(&bytes).unwrap();

    let revocation = fresh_revocation_cache(
        tmp.path().join("rev"),
        &rev_key,
        &[&pack.content_bytes_hash],
    );

    let mut config = cli_verifier_config(
        revocation,
        Arc::new(AuthorKeyStore::empty()),
        TrustTier::T0,
    );
    config.allow_unsigned = true;
    let verifier = Verifier::new(config);
    let verdict = verifier.verify(&pack).await.unwrap();
    match verdict {
        Verdict::Revoked(RevokedDetails { advisory }) => {
            assert_eq!(advisory.pack, "alice/thesis");
            assert!(matches!(advisory.reason, Reason::Malware));
        }
        other => panic!("expected Revoked, got {other:?}"),
    }
}

#[tokio::test]
async fn expired_revocation_cache_returns_stale_cache() {
    let tmp = TempDir::new().unwrap();
    let rev_key = signing_key(11);
    let revocation = expired_revocation_cache(tmp.path().join("rev"), &rev_key);

    let bytes = build_unsigned_pack("alice/demo", TrustTier::T0);
    let pack = reader::read_bytes(&bytes).unwrap();

    let mut config = cli_verifier_config(
        revocation,
        Arc::new(AuthorKeyStore::empty()),
        TrustTier::T0,
    );
    config.allow_unsigned = true;
    let verifier = Verifier::new(config);
    let verdict = verifier.verify(&pack).await.unwrap();
    assert!(matches!(verdict, Verdict::StaleCache { age_days } if age_days >= 7));
}

#[tokio::test]
async fn t2_pack_returns_unsupported_until_step_4b() {
    let tmp = TempDir::new().unwrap();
    let rev_key = signing_key(12);
    let revocation = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

    let bytes = build_unsigned_pack("alice/sigstore-demo", TrustTier::T2);
    let pack = reader::read_bytes(&bytes).unwrap();

    let verifier = Verifier::new(cli_verifier_config(
        revocation,
        Arc::new(AuthorKeyStore::empty()),
        TrustTier::T1,
    ));
    let verdict = verifier.verify(&pack).await.unwrap();
    match verdict {
        Verdict::Unsupported { tier, .. } => assert_eq!(tier, TrustTier::T2),
        other => panic!("expected Unsupported(T2), got {other:?}"),
    }
}
