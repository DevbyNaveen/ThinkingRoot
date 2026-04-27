//! Integration tests for the offline behaviour of `RevocationCache`.
//!
//! Network refresh is exercised separately by an in-process axum
//! harness in a follow-up test file once the cloud team finalises the
//! registry's response shape (see `phase-f-trust-verify-design.md`
//! §12 acceptance items).

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use tempfile::TempDir;
use tr_revocation::{
    Advisory, Authority, CacheConfig, Error, FreshnessState, PinnedKey, Reason, RevocationCache,
    Snapshot,
};

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn sign(snap: &mut Snapshot, key: &SigningKey) {
    let payload = snap.canonical_bytes_for_signing().expect("canonical bytes");
    let sig = key.sign(&payload);
    snap.signature = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
}

fn fixture(key_id: &str, hash: &str) -> Snapshot {
    Snapshot {
        schema_version: "1.0.0".into(),
        generated_at: 1_745_100_000,
        generated_by: "hub.thinkingroot.dev".into(),
        full_list: true,
        entries: vec![Advisory {
            content_hash: format!("blake3:{hash}"),
            pack: "alice/thesis".into(),
            version: "1.2.0".into(),
            reason: Reason::PublisherRequest,
            revoked_at: 1_745_099_000,
            authority: Authority::Publisher,
            details_url: "https://thinkingroot.dev/advisories/TRSA-2026-0042".into(),
        }],
        signature: String::new(),
        signing_key_id: key_id.into(),
        next_poll_hint_sec: 3_600,
    }
}

fn make_cache(dir: PathBuf, trusted: PinnedKey) -> RevocationCache {
    RevocationCache::new(CacheConfig {
        registry_url: url::Url::parse("https://hub.example/").unwrap(),
        cache_dir: dir,
        fresh_ttl: Duration::from_secs(60 * 60),
        stale_grace: Duration::from_secs(7 * 24 * 60 * 60),
        trusted_keys: vec![trusted],
        max_snapshot_bytes: 50 * 1024 * 1024,
    })
}

fn pinned_for(key: &SigningKey, key_id: &str) -> PinnedKey {
    PinnedKey {
        key_id: key_id.into(),
        ed25519_public: key.verifying_key().to_bytes(),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn load_returns_missing_when_no_snapshot_on_disk() {
    let dir = TempDir::new().unwrap();
    let key = signing_key(1);
    let cache = make_cache(dir.path().to_path_buf(), pinned_for(&key, "k1"));

    let (snap, state) = cache.load().unwrap();
    assert!(snap.is_none());
    assert_eq!(state, FreshnessState::Missing);
}

#[test]
fn freshness_transitions_fresh_to_stale_to_expired() {
    let dir = TempDir::new().unwrap();
    let key = signing_key(2);
    let cache = make_cache(dir.path().to_path_buf(), pinned_for(&key, "k2"));

    let mut snap = fixture("k2", &"a".repeat(64));
    sign(&mut snap, &key);
    let bytes = serde_json::to_vec(&snap).unwrap();

    std::fs::write(dir.path().join("snapshot.json"), &bytes).unwrap();
    std::fs::write(
        dir.path().join("snapshot.fetched_at"),
        unix_now().to_string(),
    )
    .unwrap();

    assert_eq!(cache.load().unwrap().1, FreshnessState::Fresh);

    // Roll into the stale window — 2h ago is past fresh_ttl (1h) but
    // well within stale_grace (7d).
    let two_hours_ago = unix_now() - 2 * 60 * 60;
    std::fs::write(
        dir.path().join("snapshot.fetched_at"),
        two_hours_ago.to_string(),
    )
    .unwrap();
    assert_eq!(cache.load().unwrap().1, FreshnessState::Stale);

    // Past stale_grace — caller must refuse.
    let ten_days_ago = unix_now() - 10 * 24 * 60 * 60;
    std::fs::write(
        dir.path().join("snapshot.fetched_at"),
        ten_days_ago.to_string(),
    )
    .unwrap();
    assert_eq!(cache.load().unwrap().1, FreshnessState::Expired);
}

#[test]
fn is_revoked_matches_with_or_without_blake3_prefix() {
    let key = signing_key(3);
    let mut snap = fixture("k3", &"a".repeat(64));
    sign(&mut snap, &key);

    assert!(RevocationCache::is_revoked(&snap, &format!("blake3:{}", "a".repeat(64))).is_some());
    assert!(RevocationCache::is_revoked(&snap, &"a".repeat(64)).is_some());
    assert!(RevocationCache::is_revoked(&snap, &"b".repeat(64)).is_none());
}

#[test]
fn signature_verification_passes_for_trusted_key() {
    let dir = TempDir::new().unwrap();
    let key = signing_key(4);
    let cache = make_cache(dir.path().to_path_buf(), pinned_for(&key, "k4"));

    let mut snap = fixture("k4", &"a".repeat(64));
    sign(&mut snap, &key);

    cache.verify_signature(&snap).unwrap();
}

#[test]
fn signature_verification_rejects_unknown_key_id() {
    let dir = TempDir::new().unwrap();
    let key = signing_key(5);
    let cache = make_cache(dir.path().to_path_buf(), pinned_for(&key, "trusted"));

    let mut snap = fixture("not-trusted", &"a".repeat(64));
    sign(&mut snap, &key);

    let err = cache.verify_signature(&snap).unwrap_err();
    assert!(matches!(err, Error::KeyUnknown(id) if id == "not-trusted"));
}

#[test]
fn signature_verification_rejects_tampered_payload() {
    let dir = TempDir::new().unwrap();
    let key = signing_key(6);
    let cache = make_cache(dir.path().to_path_buf(), pinned_for(&key, "k6"));

    let mut snap = fixture("k6", &"a".repeat(64));
    sign(&mut snap, &key);
    // Mutate after signing — signature now covers the wrong bytes.
    snap.entries[0].pack = "mallory/oops".into();

    let err = cache.verify_signature(&snap).unwrap_err();
    assert!(matches!(err, Error::BadSignature));
}

#[test]
fn signature_verification_rejects_corrupted_signature_bytes() {
    let dir = TempDir::new().unwrap();
    let key = signing_key(7);
    let cache = make_cache(dir.path().to_path_buf(), pinned_for(&key, "k7"));

    let mut snap = fixture("k7", &"a".repeat(64));
    sign(&mut snap, &key);
    snap.signature = "not-base64-at-all-!!!".into();

    let err = cache.verify_signature(&snap).unwrap_err();
    assert!(matches!(err, Error::BadSignature));
}

#[test]
fn snapshot_over_size_cap_is_rejected_on_load() {
    let dir = TempDir::new().unwrap();
    let key = signing_key(8);
    let mut cfg = CacheConfig {
        registry_url: url::Url::parse("https://hub.example/").unwrap(),
        cache_dir: dir.path().to_path_buf(),
        fresh_ttl: Duration::from_secs(60 * 60),
        stale_grace: Duration::from_secs(7 * 24 * 60 * 60),
        trusted_keys: vec![pinned_for(&key, "k8")],
        max_snapshot_bytes: 64,
    };
    cfg.max_snapshot_bytes = 64;
    let cache = RevocationCache::new(cfg);

    // Anything larger than 64 bytes triggers TooLarge.
    let bytes = vec![b'x'; 1024];
    std::fs::write(dir.path().join("snapshot.json"), &bytes).unwrap();

    let err = cache.load().unwrap_err();
    assert!(matches!(
        err,
        Error::TooLarge {
            cap: 64,
            actual: 1024
        }
    ));
}

#[test]
fn canonical_bytes_excludes_signature_and_metadata_fields() {
    // The signed payload must remain stable when forward-compatible
    // metadata fields change. Verify the canonical bytes contain
    // entries + generated_at + generated_by, and nothing else.
    let key = signing_key(9);
    let mut snap = fixture("k9", &"c".repeat(64));
    sign(&mut snap, &key);

    let canonical = snap.canonical_bytes_for_signing().unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    let obj = parsed.as_object().unwrap();
    assert_eq!(obj.len(), 3);
    assert!(obj.contains_key("entries"));
    assert!(obj.contains_key("generated_at"));
    assert!(obj.contains_key("generated_by"));
    assert!(!obj.contains_key("signature"));
    assert!(!obj.contains_key("signing_key_id"));
    assert!(!obj.contains_key("schema_version"));
}
