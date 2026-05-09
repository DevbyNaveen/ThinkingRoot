//! Slice 7 — author-key verification via tr-identity DID resolver.
//!
//! Each test wires a deterministic mock resolver so the matrix
//! (Verified / Unsigned / KeyMissing / KeyMismatch / ResolutionFailed
//! / InvalidAuthorKey) is exercised without network calls.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use ed25519_dalek::{Signer, SigningKey};
use semver::Version;
use tr_format::{FORMAT_VERSION_V31, ManifestV3};
use tr_identity::{Did, DidResolver, Keypair, PublicKeyRef, ResolvedDid};
use tr_verify::{AuthorVerdict, AuthorVerifier, Error};

// ── Mock resolver ────────────────────────────────────────────────

struct MockResolver {
    /// did string -> public keys to return
    by_did: HashMap<String, Vec<PublicKeyRef>>,
    /// did string -> if present, return this error instead of the
    /// public-key set
    failures: HashMap<String, tr_identity::Error>,
    calls: Mutex<Vec<String>>,
}

impl MockResolver {
    fn new() -> Self {
        Self {
            by_did: HashMap::new(),
            failures: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        }
    }
    fn with_keys(mut self, did: &str, keys: Vec<PublicKeyRef>) -> Self {
        self.by_did.insert(did.to_string(), keys);
        self
    }
    fn with_failure(mut self, did: &str, err: tr_identity::Error) -> Self {
        self.failures.insert(did.to_string(), err);
        self
    }
}

#[async_trait]
impl DidResolver for MockResolver {
    async fn resolve(&self, did: &Did) -> tr_identity::Result<ResolvedDid> {
        self.calls.lock().unwrap().push(did.as_str().to_string());
        if let Some(err) = self.failures.get(did.as_str()) {
            // We can't clone tr_identity::Error so re-construct an
            // equivalent variant from the message.
            return Err(tr_identity::Error::DidWebFetch(err.to_string()));
        }
        let keys = self.by_did.get(did.as_str()).cloned().unwrap_or_default();
        Ok(ResolvedDid {
            did: did.clone(),
            keys,
        })
    }
}

// ── Helpers ──────────────────────────────────────────────────────

fn fixture_keypair(seed: u8) -> SigningKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    SigningKey::from_bytes(&bytes)
}

fn pubkey_of(sk: &SigningKey) -> PublicKeyRef {
    PublicKeyRef::from_slice(sk.verifying_key().as_bytes()).unwrap()
}

fn signed_v31_manifest(did: &str, sk: &SigningKey) -> (ManifestV3, Vec<u8>) {
    let mut m = ManifestV3::new("alice/auth-fixture", Version::parse("0.1.0").unwrap());
    m.format_version = FORMAT_VERSION_V31.into();
    m.author_key_id = Some(did.into());
    let bytes = m.canonical_bytes_for_hashing();
    let sig = sk.sign(&bytes).to_bytes().to_vec();
    (m, sig)
}

// ── Tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn unsigned_pack_returns_unsigned() {
    let mut m = ManifestV3::new("alice/no-sig", Version::parse("0.1.0").unwrap());
    m.format_version = FORMAT_VERSION_V31.into();
    // author_key_id is None and signature is empty → Unsigned.
    let verifier = AuthorVerifier::new(MockResolver::new());
    let verdict = verifier.verify_author(&m, &[]).await.unwrap();
    assert_eq!(verdict, AuthorVerdict::Unsigned);
}

#[tokio::test]
async fn signed_pack_with_resolvable_did_verifies() {
    let sk = fixture_keypair(1);
    let did = "did:web:alice.example#k1";
    let (m, sig) = signed_v31_manifest(did, &sk);
    let resolver = MockResolver::new()
        .with_keys("did:web:alice.example", vec![pubkey_of(&sk)]);
    let verifier = AuthorVerifier::new(resolver);
    let verdict = verifier.verify_author(&m, &sig).await.unwrap();
    match verdict {
        AuthorVerdict::Verified { did } => assert_eq!(did, "did:web:alice.example"),
        other => panic!("expected Verified, got {other:?}"),
    }
}

#[tokio::test]
async fn signed_pack_with_unresolvable_did_returns_resolution_failed() {
    let sk = fixture_keypair(2);
    let did = "did:web:gone.example";
    let (m, sig) = signed_v31_manifest(did, &sk);
    let resolver = MockResolver::new().with_failure(
        did,
        tr_identity::Error::DidWebFetch("dns: NXDOMAIN".into()),
    );
    let verifier = AuthorVerifier::new(resolver);
    let err = verifier.verify_author(&m, &sig).await.unwrap_err();
    match err {
        Error::AuthorKeyResolutionFailed { did: d, .. } => assert_eq!(d, did),
        other => panic!("expected AuthorKeyResolutionFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn signed_pack_with_wrong_key_returns_key_mismatch() {
    let signer = fixture_keypair(3);
    let other = fixture_keypair(99);
    let did = "did:web:wrong-key.example";
    let (m, sig) = signed_v31_manifest(did, &signer);
    // DID document advertises a DIFFERENT public key.
    let resolver = MockResolver::new().with_keys(did, vec![pubkey_of(&other)]);
    let verifier = AuthorVerifier::new(resolver);
    let verdict = verifier.verify_author(&m, &sig).await.unwrap();
    assert_eq!(verdict, AuthorVerdict::KeyMismatch);
}

#[tokio::test]
async fn signed_pack_without_key_id_returns_key_missing() {
    let sk = fixture_keypair(4);
    let mut m = ManifestV3::new("alice/missing-keyid", Version::parse("0.1.0").unwrap());
    m.format_version = FORMAT_VERSION_V31.into();
    // author_key_id intentionally left None.
    let bytes = m.canonical_bytes_for_hashing();
    let sig = sk.sign(&bytes).to_bytes().to_vec();
    let verifier = AuthorVerifier::new(MockResolver::new());
    let verdict = verifier.verify_author(&m, &sig).await.unwrap();
    assert_eq!(verdict, AuthorVerdict::KeyMissing);
}

#[tokio::test]
async fn malformed_did_returns_invalid_author_key() {
    let mut m = ManifestV3::new("alice/bad-did", Version::parse("0.1.0").unwrap());
    m.format_version = FORMAT_VERSION_V31.into();
    // tr-format's validate() catches a malformed DID up front, but
    // the verifier must also refuse loudly when handed a manifest
    // that has somehow bypassed validation (e.g. constructed
    // programmatically and not parsed through ManifestV3::parse).
    m.author_key_id = Some("not-a-did".into());
    let verifier = AuthorVerifier::new(MockResolver::new());
    let err = verifier.verify_author(&m, &[1u8; 64]).await.unwrap_err();
    match err {
        Error::InvalidAuthorKey { did, .. } => assert_eq!(did, "not-a-did"),
        other => panic!("expected InvalidAuthorKey, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_did_document_returns_key_mismatch() {
    let sk = fixture_keypair(5);
    let did = "did:web:empty-doc.example";
    let (m, sig) = signed_v31_manifest(did, &sk);
    // Resolver returns a doc with NO keys — no chance of verification,
    // not an error from our perspective.
    let resolver = MockResolver::new().with_keys(did, vec![]);
    let verifier = AuthorVerifier::new(resolver);
    let verdict = verifier.verify_author(&m, &sig).await.unwrap();
    assert_eq!(verdict, AuthorVerdict::KeyMismatch);
}

// Sanity check that the `Keypair` module's helpers integrate.
#[allow(dead_code)]
fn _keypair_compiles() {
    let _kp = Keypair::generate();
}
