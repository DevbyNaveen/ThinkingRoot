//! Sigstore-compatible signing + offline DSSE verification for v3 packs.
//!
//! Implementation contract per the v3 spec §3.4 + the Phase F design
//! (`docs/2026-04-29-phase-f-trust-verify-spec.md`):
//!
//! - **Bundle wire format:** Sigstore Bundle v0.3
//!   (`application/vnd.dev.sigstore.bundle+json;version=0.3`).
//! - **DSSE envelope** wraps an in-toto v1 statement whose `subject`
//!   digest binds the bundle to a specific BLAKE3 pack hash.
//! - **DSSE statement type:** `application/vnd.thinkingroot.pack.v3+json`
//!   (locked by spec §3.4 — exposed as [`DSSE_STATEMENT_TYPE`]).
//! - **Signature algorithm:** Ed25519 today. Sigstore Fulcio-issued
//!   ECDSA-P256 keys are wire-compatible with the same envelope shape;
//!   the live OIDC + Fulcio + Rekor integration is gated behind the
//!   `sigstore-impl` feature on `tr-verify` and lands in a follow-up
//!   (Week 3.5 — see Phase F doc §7).
//!
//! What this crate does today:
//!
//! 1. [`sign_pack`] produces a [`SigstoreBundle`] over the canonical
//!    BLAKE3 pack hash using a caller-supplied Ed25519 keypair. No
//!    network. Used by the v3 pack writer when the user opts in to
//!    self-signed packs (`root pack --sign`); also drives every
//!    integration test in the v3 stack.
//! 2. [`verify_bundle_offline`] runs the verification chain in §7.6 of
//!    the v3 spec without contacting the network: signature ✓ → DSSE
//!    statement subject digest matches expected pack hash ✓ →
//!    (optional) cert chain validation against a trust root ✓ →
//!    (optional) Rekor inclusion proof.
//!
//! Live Fulcio sign + Rekor witness submission are stubbed behind the
//! `sigstore-impl` feature on `tr-verify`. The wire bytes a Sigstore-
//! keyless bundle carries are a strict superset of the bundle shape
//! emitted today — the verifier already accepts the additional fields
//! (cert chain, tlogEntries) without breaking back-compat with self-
//! signed bundles.

#![forbid(unsafe_code)]

use std::time::SystemTime;

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// DSSE statement type for v3 packs. Locked by spec §3.4 — never
/// changes for the `tr/3` format. A future `tr/4` would mint a new
/// media type; readers/verifiers refusing on mismatch surfaces
/// incompatibility cleanly.
pub const DSSE_STATEMENT_TYPE: &str = "application/vnd.thinkingroot.pack.v3+json";

/// DSSE payload type — in-toto v1 statement. Locked by the in-toto
/// spec; consumed by sigstore-rs and other DSSE-compatible verifiers.
pub const DSSE_PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";

/// In-toto v1 statement type identifier.
pub const IN_TOTO_STATEMENT_V1: &str = "https://in-toto.io/Statement/v1";

/// Sigstore Bundle media-type for v0.3 wire format. The bundle JSON
/// includes this as `mediaType`.
pub const SIGSTORE_BUNDLE_MEDIA_TYPE: &str =
    "application/vnd.dev.sigstore.bundle+json;version=0.3";

/// Errors produced by sign + verify. Distinct types per failure mode
/// so callers (tr-verify) can map straight to a `Verdict` variant.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The bundle JSON could not be parsed.
    #[error("bundle parse: {0}")]
    BundleParse(#[from] serde_json::Error),
    /// The DSSE payload could not be base64-decoded.
    #[error("bundle base64: {0}")]
    Base64(#[from] base64::DecodeError),
    /// The bundle's signature failed Ed25519 verification.
    #[error("signature verification failed")]
    SignatureMismatch,
    /// The DSSE payload's subject digest does not match the pack hash
    /// we recomputed locally — someone tampered with the pack after
    /// signing.
    #[error("subject digest mismatch: expected {expected}, payload has {payload}")]
    SubjectMismatch {
        /// Hash recomputed by the verifier from the pack bytes.
        expected: String,
        /// Hash the bundle's DSSE statement claims.
        payload: String,
    },
    /// The DSSE statement type didn't match `DSSE_STATEMENT_TYPE`. The
    /// bundle is signing something other than a v3 pack.
    #[error("DSSE statement type mismatch: expected {expected}, got {got}")]
    StatementTypeMismatch {
        /// `application/vnd.thinkingroot.pack.v3+json`.
        expected: &'static str,
        /// What the bundle's predicateType field actually contains.
        got: String,
    },
    /// The bundle's verification material doesn't carry an Ed25519
    /// public key in a shape we recognise. Self-signed bundles use
    /// `verificationMaterial.publicKey.rawBytes`; Fulcio-signed bundles
    /// use `verificationMaterial.x509CertificateChain` (handled by the
    /// follow-up `sigstore-impl` feature).
    #[error("no Ed25519 verification key in bundle")]
    MissingVerificationKey,
    /// The Ed25519 key bytes in the bundle were the wrong length.
    #[error("Ed25519 key length: expected 32 bytes, got {0}")]
    InvalidKeyLength(usize),
    /// The Ed25519 signature bytes were the wrong length.
    #[error("Ed25519 signature length: expected 64 bytes, got {0}")]
    InvalidSignatureLength(usize),
}

/// Result alias for sigstore operations.
pub type Result<T> = std::result::Result<T, Error>;

// ─────────────────────────────────────────────────────────────────
// Bundle wire format — Sigstore Bundle v0.3 + the bits in-toto needs.
// ─────────────────────────────────────────────────────────────────

/// Top-level Sigstore Bundle. Serialized as `signature.sig` inside the
/// outer v3 tar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SigstoreBundle {
    /// `application/vnd.dev.sigstore.bundle+json;version=0.3`.
    #[serde(rename = "mediaType")]
    pub media_type: String,

    /// What identity / public key signed the envelope.
    #[serde(rename = "verificationMaterial")]
    pub verification_material: VerificationMaterial,

    /// The signed in-toto statement, wrapped in a DSSE envelope.
    #[serde(rename = "dsseEnvelope")]
    pub dsse_envelope: DsseEnvelope,
}

/// Verification material — either a self-signed Ed25519 public key
/// (today) or a Fulcio cert chain (Week 3.5 follow-up). Both shapes
/// coexist: self-signed bundles set `public_key`; Fulcio bundles set
/// `x509_certificate_chain`. A bundle can carry both for transition
/// scenarios.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerificationMaterial {
    /// Self-signed Ed25519 public key. None when the signer is
    /// Sigstore-keyless via Fulcio.
    #[serde(rename = "publicKey", default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<PublicKeyMaterial>,

    /// Fulcio-issued ephemeral X.509 cert chain. None for self-signed.
    /// Populated by the live `sigstore-impl` flow (Week 3.5).
    #[serde(
        rename = "x509CertificateChain",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub x509_certificate_chain: Option<X509CertificateChain>,

    /// Rekor inclusion proofs witnessing the signing event. Empty for
    /// self-signed bundles. The `sigstore-impl` flow populates one
    /// entry per Rekor witness.
    #[serde(rename = "tlogEntries", default, skip_serializing_if = "Vec::is_empty")]
    pub tlog_entries: Vec<TlogEntry>,
}

/// Raw public-key material. `raw_bytes` is base64-encoded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublicKeyMaterial {
    /// Base64-encoded key bytes. For Ed25519 these are exactly 32
    /// bytes pre-encoding.
    #[serde(rename = "rawBytes")]
    pub raw_bytes: String,
}

/// X.509 cert chain placeholder — populated by the `sigstore-impl`
/// flow when Fulcio issues the cert. Shape matches Sigstore's
/// `X509CertificateChain` proto so swap-in is byte-identical.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct X509CertificateChain {
    /// PEM-encoded leaf-first cert chain.
    #[serde(default)]
    pub certificates: Vec<X509Certificate>,
}

/// One PEM-encoded X.509 certificate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct X509Certificate {
    /// Base64-encoded DER bytes (matches Sigstore's `RawCertificate`).
    #[serde(rename = "rawBytes")]
    pub raw_bytes: String,
}

/// Rekor inclusion proof witnessing the signing event. Populated by
/// the live `sigstore-impl` flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TlogEntry {
    /// The Rekor log index this entry occupies.
    #[serde(rename = "logIndex")]
    pub log_index: i64,
    /// The integrated time (Unix seconds) Rekor recorded the entry at.
    #[serde(rename = "integratedTime")]
    pub integrated_time: i64,
    /// Inclusion-proof Merkle audit path + tree size + checkpoint.
    /// Verifiers replay this against the Rekor public key without
    /// network access.
    #[serde(rename = "inclusionProof", default)]
    pub inclusion_proof: serde_json::Value,
}

/// DSSE envelope — the signed payload + signature(s).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DsseEnvelope {
    /// Base64-encoded statement bytes.
    pub payload: String,
    /// `application/vnd.in-toto+json`.
    #[serde(rename = "payloadType")]
    pub payload_type: String,
    /// One or more signatures over the DSSE PAE of (payloadType ||
    /// payload). Multiple entries support quorum signing; today we
    /// always emit exactly one.
    pub signatures: Vec<DsseSignature>,
}

/// One signature over the DSSE PAE.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DsseSignature {
    /// Base64-encoded signature bytes.
    pub sig: String,
}

/// In-toto v1 statement — what the DSSE envelope's `payload` decodes
/// to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InTotoStatement {
    /// Always `https://in-toto.io/Statement/v1`.
    #[serde(rename = "_type")]
    pub statement_type: String,
    /// What this statement is about. v3 always emits exactly one entry
    /// pointing at the pack file with the BLAKE3 digest.
    pub subject: Vec<Subject>,
    /// Always `application/vnd.thinkingroot.pack.v3+json` for v3.
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    /// Domain-specific predicate body. v3 uses
    /// [`PackPredicate`] with the format version + signing time.
    pub predicate: PackPredicate,
}

/// One subject of an in-toto statement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Subject {
    /// Pack filename or coordinate.
    pub name: String,
    /// Algorithm-keyed digest map. v3 always uses `blake3`.
    pub digest: serde_json::Map<String, serde_json::Value>,
}

/// Predicate body for the v3 DSSE statement type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackPredicate {
    /// `tr/3`.
    #[serde(rename = "format_version")]
    pub format_version: String,
    /// RFC 3339 timestamp of when the bundle was assembled.
    #[serde(rename = "signed_at")]
    pub signed_at: String,
}

// ─────────────────────────────────────────────────────────────────
// Sign + verify
// ─────────────────────────────────────────────────────────────────

/// Sign a v3 pack. Constructs the in-toto statement over the pack hash,
/// wraps it in a DSSE envelope signed with the supplied Ed25519 key,
/// and returns the assembled bundle. No network.
///
/// Inputs:
/// - `pack_hash`: BLAKE3 hex of the pack (matches the manifest's
///   `pack_hash` field, with or without the `blake3:` prefix — both are
///   accepted; the wire format always carries the bare hex inside the
///   in-toto statement's digest map).
/// - `pack_filename`: human-readable name (e.g. `"package.tr"` or
///   `"alice/golden-1.0.0.tr"`). Lands in the statement's
///   `subject[0].name`.
/// - `signing_key`: Ed25519 private key. Cryptographic library leaves
///   ownership to the caller; never persisted by this function.
/// - `signed_at`: timestamp the bundle records (`SystemTime::now()` is
///   the typical choice).
pub fn sign_pack(
    pack_hash: &str,
    pack_filename: &str,
    signing_key: &SigningKey,
    signed_at: SystemTime,
) -> Result<SigstoreBundle> {
    let bare_hash = strip_blake3_prefix(pack_hash);

    let statement = InTotoStatement {
        statement_type: IN_TOTO_STATEMENT_V1.to_string(),
        subject: vec![Subject {
            name: pack_filename.to_string(),
            digest: {
                let mut m = serde_json::Map::new();
                m.insert(
                    "blake3".to_string(),
                    serde_json::Value::String(bare_hash.to_string()),
                );
                m
            },
        }],
        predicate_type: DSSE_STATEMENT_TYPE.to_string(),
        predicate: PackPredicate {
            format_version: "tr/3".to_string(),
            signed_at: format_rfc3339(signed_at),
        },
    };

    let payload_bytes = serde_json::to_vec(&statement)?;
    let pae = dsse_pae(DSSE_PAYLOAD_TYPE, &payload_bytes);
    let signature = signing_key.sign(&pae);

    let b64 = base64::engine::general_purpose::STANDARD;
    let payload_b64 = b64.encode(&payload_bytes);
    let sig_b64 = b64.encode(signature.to_bytes());
    let pubkey_b64 = b64.encode(signing_key.verifying_key().to_bytes());

    Ok(SigstoreBundle {
        media_type: SIGSTORE_BUNDLE_MEDIA_TYPE.to_string(),
        verification_material: VerificationMaterial {
            public_key: Some(PublicKeyMaterial {
                raw_bytes: pubkey_b64,
            }),
            x509_certificate_chain: None,
            tlog_entries: Vec::new(),
        },
        dsse_envelope: DsseEnvelope {
            payload: payload_b64,
            payload_type: DSSE_PAYLOAD_TYPE.to_string(),
            signatures: vec![DsseSignature { sig: sig_b64 }],
        },
    })
}

/// Verify a [`SigstoreBundle`] against an expected pack hash. Offline-
/// only — does not contact Rekor or Fulcio. Returns the in-toto
/// statement on success so callers can pluck out the predicate
/// (`signed_at`, `format_version`).
///
/// Verification chain (v3 spec §7.6, locked):
///
/// 1. Pull the public key out of `verification_material` (self-signed
///    today; `sigstore-impl` adds Fulcio cert chain validation).
/// 2. Re-derive the DSSE PAE from `(payloadType, payload)` and verify
///    the signature against the public key.
/// 3. Decode the in-toto statement; assert `predicateType` matches the
///    locked v3 statement type.
/// 4. Assert the statement's first subject digest matches
///    `expected_pack_hash`.
///
/// Step 5 (Rekor inclusion proof) is no-op for self-signed bundles
/// (`tlog_entries` is empty); the `sigstore-impl` follow-up adds the
/// Merkle audit-path replay against the bundled Rekor public key.
pub fn verify_bundle_offline(
    bundle: &SigstoreBundle,
    expected_pack_hash: &str,
) -> Result<InTotoStatement> {
    let pubkey_bytes = bundle
        .verification_material
        .public_key
        .as_ref()
        .ok_or(Error::MissingVerificationKey)?;
    let b64 = base64::engine::general_purpose::STANDARD;

    let raw = b64.decode(&pubkey_bytes.raw_bytes)?;
    if raw.len() != 32 {
        return Err(Error::InvalidKeyLength(raw.len()));
    }
    let mut key_array = [0u8; 32];
    key_array.copy_from_slice(&raw);
    let verifying = VerifyingKey::from_bytes(&key_array)
        .map_err(|_| Error::InvalidKeyLength(32))?;

    let payload = b64.decode(&bundle.dsse_envelope.payload)?;
    let pae = dsse_pae(&bundle.dsse_envelope.payload_type, &payload);

    let sig_b64 = bundle
        .dsse_envelope
        .signatures
        .first()
        .ok_or(Error::SignatureMismatch)?;
    let sig_bytes = b64.decode(&sig_b64.sig)?;
    if sig_bytes.len() != 64 {
        return Err(Error::InvalidSignatureLength(sig_bytes.len()));
    }
    let mut sig_array = [0u8; 64];
    sig_array.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(&sig_array);

    verifying
        .verify(&pae, &signature)
        .map_err(|_| Error::SignatureMismatch)?;

    // Statement now has cryptographic provenance — verify the
    // semantic claims it carries.
    let statement: InTotoStatement = serde_json::from_slice(&payload)?;
    if statement.predicate_type != DSSE_STATEMENT_TYPE {
        return Err(Error::StatementTypeMismatch {
            expected: DSSE_STATEMENT_TYPE,
            got: statement.predicate_type.clone(),
        });
    }

    let bare_expected = strip_blake3_prefix(expected_pack_hash);
    let payload_digest = statement
        .subject
        .first()
        .and_then(|s| s.digest.get("blake3"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();

    if payload_digest != bare_expected {
        return Err(Error::SubjectMismatch {
            expected: bare_expected.to_string(),
            payload: payload_digest,
        });
    }

    Ok(statement)
}

/// DSSE Pre-Authentication Encoding (PAE) per the DSSE spec.
///
/// `PAE("DSSEv1", payloadType, payload) = "DSSEv1 " || len(payloadType)
///  || " " || payloadType || " " || len(payload) || " " || payload`
///
/// Lengths are decimal ASCII. Same encoding sigstore-rs and
/// every conformant DSSE library produces.
fn dsse_pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut pae = Vec::with_capacity(64 + payload_type.len() + payload.len());
    pae.extend_from_slice(b"DSSEv1 ");
    pae.extend_from_slice(payload_type.len().to_string().as_bytes());
    pae.push(b' ');
    pae.extend_from_slice(payload_type.as_bytes());
    pae.push(b' ');
    pae.extend_from_slice(payload.len().to_string().as_bytes());
    pae.push(b' ');
    pae.extend_from_slice(payload);
    pae
}

/// Strip a `blake3:` prefix from a pack hash for in-toto statement
/// digest emission. The wire format inside the in-toto statement's
/// digest map omits the prefix (the algorithm name is the map key);
/// our manifest stores it as `blake3:<hex>` for readability.
fn strip_blake3_prefix(hash: &str) -> &str {
    hash.strip_prefix("blake3:").unwrap_or(hash)
}

/// Format a SystemTime as RFC 3339 with seconds precision and a
/// trailing `Z`. Matches the format the v3 manifest's `extracted_at`
/// emits, so consumers see consistent timestamp syntax.
fn format_rfc3339(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand_dummy::generate_signing_key;

    /// Tiny deterministic key generator for tests. Real callers use
    /// `SigningKey::generate(&mut rand::rngs::OsRng)` or load a key from
    /// disk; tests use a fixed seed so failures are reproducible.
    mod rand_dummy {
        use super::SigningKey;
        pub fn generate_signing_key(seed: u8) -> SigningKey {
            let mut bytes = [0u8; 32];
            bytes[0] = seed;
            // Ed25519 accepts any 32 bytes as a seed; secret derivation
            // hashes them through SHA-512 internally.
            SigningKey::from_bytes(&bytes)
        }
    }

    fn fixture_hash() -> &'static str {
        // 64 hex chars = 32 bytes BLAKE3 output.
        "blake3:abcd1234ef5678901234567890abcdef1234567890abcdef1234567890abcd"
    }

    #[test]
    fn sign_round_trips_through_verify() {
        let key = generate_signing_key(1);
        let bundle = sign_pack(
            fixture_hash(),
            "package.tr",
            &key,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        let stmt = verify_bundle_offline(&bundle, fixture_hash()).unwrap();
        assert_eq!(stmt.predicate_type, DSSE_STATEMENT_TYPE);
        assert_eq!(stmt.predicate.format_version, "tr/3");
        assert_eq!(stmt.subject.len(), 1);
        assert_eq!(stmt.subject[0].name, "package.tr");
    }

    #[test]
    fn verify_rejects_wrong_hash() {
        let key = generate_signing_key(2);
        let bundle = sign_pack(fixture_hash(), "p.tr", &key, SystemTime::UNIX_EPOCH).unwrap();
        let other_hash =
            "blake3:0000000000000000000000000000000000000000000000000000000000000000";
        let err = verify_bundle_offline(&bundle, other_hash).unwrap_err();
        assert!(matches!(err, Error::SubjectMismatch { .. }));
    }

    #[test]
    fn verify_rejects_tampered_signature() {
        let key = generate_signing_key(3);
        let mut bundle =
            sign_pack(fixture_hash(), "p.tr", &key, SystemTime::UNIX_EPOCH).unwrap();
        // Flip a bit in the base64 signature → still valid base64, but
        // the underlying bytes no longer verify.
        let sig = &bundle.dsse_envelope.signatures[0].sig;
        let mut sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig)
            .unwrap();
        sig_bytes[0] ^= 0x01;
        bundle.dsse_envelope.signatures[0].sig =
            base64::engine::general_purpose::STANDARD.encode(&sig_bytes);
        let err = verify_bundle_offline(&bundle, fixture_hash()).unwrap_err();
        assert!(matches!(err, Error::SignatureMismatch));
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let key = generate_signing_key(4);
        let mut bundle =
            sign_pack(fixture_hash(), "p.tr", &key, SystemTime::UNIX_EPOCH).unwrap();
        // Decode payload, change the subject, re-encode. Signature
        // covers the original payload — the swap breaks it.
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut stmt: InTotoStatement =
            serde_json::from_slice(&b64.decode(&bundle.dsse_envelope.payload).unwrap()).unwrap();
        stmt.subject[0].name = "evil.tr".to_string();
        let new_payload = serde_json::to_vec(&stmt).unwrap();
        bundle.dsse_envelope.payload = b64.encode(&new_payload);
        let err = verify_bundle_offline(&bundle, fixture_hash()).unwrap_err();
        assert!(matches!(err, Error::SignatureMismatch));
    }

    #[test]
    fn bundle_round_trips_through_serde() {
        let key = generate_signing_key(5);
        let bundle = sign_pack(fixture_hash(), "p.tr", &key, SystemTime::UNIX_EPOCH).unwrap();
        let json = serde_json::to_string(&bundle).unwrap();
        let parsed: SigstoreBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(bundle, parsed);
    }

    #[test]
    fn bundle_media_type_is_locked() {
        let key = generate_signing_key(6);
        let bundle = sign_pack(fixture_hash(), "p.tr", &key, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(bundle.media_type, SIGSTORE_BUNDLE_MEDIA_TYPE);
        assert_eq!(bundle.dsse_envelope.payload_type, DSSE_PAYLOAD_TYPE);
    }

    #[test]
    fn dsse_pae_format_matches_spec() {
        // Spec example: PAE("DSSEv1", "type", "payload") =
        //   "DSSEv1 4 type 7 payload"
        let pae = dsse_pae("type", b"payload");
        assert_eq!(&pae, b"DSSEv1 4 type 7 payload");
    }

    #[test]
    fn strip_blake3_prefix_idempotent() {
        assert_eq!(strip_blake3_prefix("blake3:abcd"), "abcd");
        assert_eq!(strip_blake3_prefix("abcd"), "abcd");
    }

    #[test]
    fn missing_public_key_in_bundle_errors_cleanly() {
        let key = generate_signing_key(7);
        let mut bundle =
            sign_pack(fixture_hash(), "p.tr", &key, SystemTime::UNIX_EPOCH).unwrap();
        bundle.verification_material.public_key = None;
        let err = verify_bundle_offline(&bundle, fixture_hash()).unwrap_err();
        assert!(matches!(err, Error::MissingVerificationKey));
    }
}
