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

pub mod rekor;
pub mod trust_root;
pub use rekor::{
    RFC6962_LEAF_PREFIX, leaf_hash_from_canonical_body, verify_inclusion_proof_offline,
    verify_set_signature,
};
pub use trust_root::{TrustedCertificate, TrustedRoot, verify_cert_chain};

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
    /// The bundle's signature failed verification (algorithm-agnostic
    /// — both Ed25519 and ECDSA P-256 paths funnel here on a bad sig).
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
    /// The bundle's verification material is empty — neither a
    /// self-signed `publicKey` nor a Fulcio `x509CertificateChain`
    /// is set.
    #[error("no verification key or cert chain in bundle")]
    MissingVerificationKey,
    /// The Ed25519 key bytes in the bundle were the wrong length.
    #[error("Ed25519 key length: expected 32 bytes, got {0}")]
    InvalidKeyLength(usize),
    /// The Ed25519 signature bytes were the wrong length.
    #[error("Ed25519 signature length: expected 64 bytes, got {0}")]
    InvalidSignatureLength(usize),
    /// The leaf cert in the bundle's `x509CertificateChain` could not
    /// be parsed as DER X.509.
    #[error("leaf certificate parse: {0}")]
    CertParse(String),
    /// The leaf cert's `x509CertificateChain.certificates` array was
    /// empty.
    #[error("certificate chain has no leaf cert")]
    EmptyCertChain,
    /// The leaf cert's SubjectPublicKeyInfo declared a key algorithm we
    /// don't support. Today the verifier handles ECDSA P-256 (Fulcio's
    /// default) and Ed25519 (self-signed); other curves error out so
    /// callers can surface a clean "unsupported" verdict instead of
    /// silently failing the signature check.
    #[error("unsupported public key algorithm: {0}")]
    UnsupportedKeyAlgorithm(String),
    /// The DSSE signature bytes could not be parsed as either DER-
    /// encoded ASN.1 ECDSA (sigstore-rs / cosign default) or raw
    /// 64-byte IEEE P1363 r||s (some non-Sigstore signers). The bytes
    /// are corrupt or use a third encoding we don't handle.
    #[error("ECDSA signature format unrecognised (expected DER or 64-byte raw)")]
    EcdsaSignatureFormat,
    /// A cert in the chain (or in the trust root) has a validity
    /// window that does not include the `signed_at` timestamp the
    /// caller supplied. Sigstore-public-good's leaf certs are valid
    /// for ~10 minutes; a 5-minute clock skew tolerance is applied
    /// by the chain validator before this error fires.
    #[error("certificate not valid at signing time: {0}")]
    CertValidity(String),
    /// The topmost cert in the chain was not signed by any of the
    /// trusted root CAs. The chain is internally consistent (cert i
    /// signed by cert i+1) but the chain doesn't terminate at a
    /// recognised root.
    #[error("chain does not reach trust root: {0}")]
    ChainDoesNotReachTrustRoot(String),
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

/// One transparency-log witness for a signed bundle. Each entry binds
/// the bundle to a specific append-only log (Rekor for Sigstore
/// public-good) at a specific log index, witnessed at a specific time.
///
/// Wire shape matches Sigstore Bundle v0.3's `TransparencyLogEntry`
/// protobuf message — see <https://github.com/sigstore/protobuf-specs>.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TlogEntry {
    /// The Rekor log index this entry occupies.
    #[serde(rename = "logIndex")]
    pub log_index: i64,
    /// Identifies which transparency log witnessed this entry.
    /// `keyId` is base64 of SHA-256 of the log's public key.
    #[serde(rename = "logId", default, skip_serializing_if = "Option::is_none")]
    pub log_id: Option<LogId>,
    /// E.g. `"hashedrekord"` + `"0.0.1"`. Identifies which Rekor entry
    /// kind generated `canonicalized_body`.
    #[serde(rename = "kindVersion", default, skip_serializing_if = "Option::is_none")]
    pub kind_version: Option<KindVersion>,
    /// Unix-seconds timestamp Rekor recorded for this entry.
    #[serde(rename = "integratedTime")]
    pub integrated_time: i64,
    /// SignedEntryTimestamp — Rekor's signature over the canonical
    /// (logIndex, logId, integratedTime, body-hash) tuple. Optional
    /// because some Rekor versions emit only the inclusion proof; if
    /// present, it's the load-bearing piece that ties this entry to
    /// the Rekor key without recomputing the Merkle root.
    #[serde(rename = "inclusionPromise", default, skip_serializing_if = "Option::is_none")]
    pub inclusion_promise: Option<InclusionPromise>,
    /// Inclusion-proof Merkle audit path + tree size + signed
    /// checkpoint. Verifiers replay this against the Rekor public key
    /// without network access.
    #[serde(rename = "inclusionProof", default, skip_serializing_if = "Option::is_none")]
    pub inclusion_proof: Option<RekorInclusionProof>,
    /// Canonicalized JSON of the original Rekor entry body. The leaf
    /// hash is `SHA-256(0x00 || canonical_body)`. Stored base64 on
    /// the wire; consumers that just want to verify the inclusion
    /// proof against a leaf hash they computed externally can ignore
    /// this field.
    #[serde(rename = "canonicalizedBody", default, skip_serializing_if = "Option::is_none")]
    pub canonicalized_body: Option<String>,
}

/// Rekor's identifier — `keyId` is base64 of SHA-256 of the Rekor
/// public key, useful for distinguishing Sigstore-public-good Rekor
/// from a private deployment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogId {
    #[serde(rename = "keyId")]
    pub key_id: String,
}

/// Identifies which Rekor entry kind generated `canonicalized_body`.
/// Sigstore-public-good emits `kind="hashedrekord", version="0.0.1"`
/// for code-signing entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KindVersion {
    pub kind: String,
    pub version: String,
}

/// Rekor SignedEntryTimestamp — base64-encoded ECDSA-with-SHA256
/// signature by Rekor's public key over the canonical
/// `(integratedTime, logIndex, logID, body-hash)` tuple.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InclusionPromise {
    #[serde(rename = "signedEntryTimestamp")]
    pub signed_entry_timestamp: String,
}

/// Merkle inclusion proof — the audit path from `leaf_hash` to the
/// log's root at `tree_size`, plus an optional signed checkpoint that
/// binds the root to Rekor's signing key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RekorInclusionProof {
    /// Position of the witnessed entry in the log.
    #[serde(rename = "logIndex")]
    pub log_index: i64,
    /// Total leaves in the log at the time the proof was generated.
    #[serde(rename = "treeSize")]
    pub tree_size: i64,
    /// The log's root hash at `tree_size` — base64 of 32 bytes.
    #[serde(rename = "rootHash")]
    pub root_hash: String,
    /// Audit-path siblings, leaf-up. Each entry is base64 of 32
    /// bytes. Length is `ceil(log2(tree_size))` for a complete tree.
    #[serde(rename = "hashes", default)]
    pub hashes: Vec<String>,
    /// Signed checkpoint envelope — Rekor signs the root hash and
    /// log size as a textual "checkpoint" envelope per the C2SP
    /// signed-checkpoint format. Optional; when absent, callers
    /// must rely on `inclusion_promise.signed_entry_timestamp`
    /// instead.
    #[serde(rename = "checkpoint", default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<RekorCheckpoint>,
}

/// Signed checkpoint envelope, per the C2SP spec
/// (<https://github.com/C2SP/C2SP/blob/main/signed-note.md>). The
/// `envelope` field is the textual envelope; verifiers split it on
/// the `\n\n` separator to recover the body and the signatures.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RekorCheckpoint {
    pub envelope: String,
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
/// 1. Pick the verification material: prefer
///    `verification_material.x509_certificate_chain` (Fulcio-issued
///    bundles) over `verification_material.public_key` (self-signed
///    bundles). Fail clean if neither is set.
/// 2. Re-derive the DSSE PAE from `(payloadType, payload)` and verify
///    the signature against the public key recovered in step 1
///    (Ed25519 raw or ECDSA P-256 from leaf cert SPKI).
/// 3. Decode the in-toto statement; assert `predicateType` matches the
///    locked v3 statement type.
/// 4. Assert the statement's first subject digest matches
///    `expected_pack_hash`.
///
/// Steps 5+ — cert chain validation against the Sigstore trust root
/// and Rekor inclusion-proof replay — are layered on top by
/// [`tr_verify::verify_v3_pack`] (chain validation lands in the next
/// commit; Rekor replay is a Phase F §4.1 step 5 follow-up).
///
/// Self-signed bundles (`tlog_entries` empty, `public_key` set) and
/// Fulcio-signed bundles (`tlog_entries` populated, `cert_chain` set)
/// share the same return type: a successful in-toto statement carrying
/// the predicate the caller should trust ONLY after layering identity
/// verification on top.
pub fn verify_bundle_offline(
    bundle: &SigstoreBundle,
    expected_pack_hash: &str,
) -> Result<InTotoStatement> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let payload = b64.decode(&bundle.dsse_envelope.payload)?;
    let pae = dsse_pae(&bundle.dsse_envelope.payload_type, &payload);

    let sig_b64 = bundle
        .dsse_envelope
        .signatures
        .first()
        .ok_or(Error::SignatureMismatch)?;
    let sig_bytes = b64.decode(&sig_b64.sig)?;

    // Dispatch based on verification material. Prefer cert chain
    // (Fulcio bundles always carry one — empty cert chain is a malformed
    // Fulcio bundle, not a fallback to self-signed).
    let cert_chain_set = bundle
        .verification_material
        .x509_certificate_chain
        .as_ref()
        .map(|c| !c.certificates.is_empty())
        .unwrap_or(false);

    if cert_chain_set {
        verify_dsse_with_ecdsa_cert(
            bundle
                .verification_material
                .x509_certificate_chain
                .as_ref()
                .unwrap(),
            &pae,
            &sig_bytes,
        )?;
    } else if let Some(pk) = bundle.verification_material.public_key.as_ref() {
        verify_dsse_with_ed25519(pk, &pae, &sig_bytes)?;
    } else {
        return Err(Error::MissingVerificationKey);
    }

    // Statement now has cryptographic provenance — verify the
    // semantic claims it carries.
    verify_statement_semantics(&payload, expected_pack_hash)
}

/// Decode the in-toto statement, check its predicate type matches the
/// locked v3 statement type, and check its subject digest matches the
/// expected pack hash. Shared between the Ed25519 and ECDSA paths
/// because the wire-level statement layout is the same regardless of
/// who signed it.
fn verify_statement_semantics(
    payload: &[u8],
    expected_pack_hash: &str,
) -> Result<InTotoStatement> {
    let statement: InTotoStatement = serde_json::from_slice(payload)?;
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

/// Verify the DSSE signature with a raw Ed25519 public key from the
/// bundle's `verification_material.public_key.raw_bytes`. Used for
/// self-signed bundles (`root pack --sign <key-file>`).
fn verify_dsse_with_ed25519(
    pk: &PublicKeyMaterial,
    pae: &[u8],
    sig_bytes: &[u8],
) -> Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD;

    let raw = b64.decode(&pk.raw_bytes)?;
    if raw.len() != 32 {
        return Err(Error::InvalidKeyLength(raw.len()));
    }
    let mut key_array = [0u8; 32];
    key_array.copy_from_slice(&raw);
    let verifying = VerifyingKey::from_bytes(&key_array)
        .map_err(|_| Error::InvalidKeyLength(32))?;

    if sig_bytes.len() != 64 {
        return Err(Error::InvalidSignatureLength(sig_bytes.len()));
    }
    let mut sig_array = [0u8; 64];
    sig_array.copy_from_slice(sig_bytes);
    let signature = Signature::from_bytes(&sig_array);

    verifying
        .verify(pae, &signature)
        .map_err(|_| Error::SignatureMismatch)
}

/// Verify the DSSE signature against the leaf cert's ECDSA P-256
/// public key, recovered from the cert's SubjectPublicKeyInfo. Does
/// **not** validate the cert chain itself — that is the
/// trust-root-aware step layered on top by `tr-verify` (next commit).
///
/// Both common DSSE-ECDSA wire formats are accepted on input:
/// 1. **DER-encoded ASN.1 SEQUENCE { r INTEGER, s INTEGER }** — what
///    sigstore-rs and cosign emit. This is the de-facto standard.
/// 2. **Raw 64-byte fixed-length r ‖ s (IEEE P1363)** — emitted by some
///    Sigstore-adjacent signers and by direct uses of `p256::ecdsa`
///    without explicit DER-encode.
///
/// We try DER first (matches Sigstore's de-facto), fall through to
/// raw on parse failure. Anything else surfaces as
/// [`Error::EcdsaSignatureFormat`].
fn verify_dsse_with_ecdsa_cert(
    chain: &X509CertificateChain,
    pae: &[u8],
    sig_bytes: &[u8],
) -> Result<()> {
    use signature::Verifier as _;
    use x509_cert::Certificate;
    use x509_cert::der::{Decode, Encode};

    let leaf = chain.certificates.first().ok_or(Error::EmptyCertChain)?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let cert_der = b64.decode(&leaf.raw_bytes)?;

    let cert = Certificate::from_der(&cert_der)
        .map_err(|e| Error::CertParse(format!("DER decode: {e}")))?;

    let spki_der = cert
        .tbs_certificate
        .subject_public_key_info
        .to_der()
        .map_err(|e| Error::CertParse(format!("SPKI re-encode: {e}")))?;

    // p256's `from_public_key_der` validates that the SPKI carries the
    // ecPublicKey OID with secp256r1 / P-256 parameters. Mismatched
    // curves (P-384, P-521, RSA, Ed25519-in-cert) surface here as a
    // clean unsupported-algorithm error.
    use p256::pkcs8::DecodePublicKey as _;
    let verifying = p256::ecdsa::VerifyingKey::from_public_key_der(&spki_der)
        .map_err(|e| Error::UnsupportedKeyAlgorithm(format!("P-256 SPKI: {e}")))?;

    let signature = parse_ecdsa_p256_signature(sig_bytes)?;

    verifying
        .verify(pae, &signature)
        .map_err(|_| Error::SignatureMismatch)
}

/// Bundle verification with full trust-root chain validation.
///
/// This is the function callers should use when they want both
/// cryptographic provenance AND identity binding (i.e., "this pack was
/// signed by someone whose ephemeral cert was issued by the Sigstore
/// public-good Fulcio CA at signing time").
///
/// Steps:
///
/// 1. [`verify_bundle_offline`] — re-derives the DSSE PAE, validates the
///    signature, and asserts the in-toto statement's subject digest
///    matches `expected_pack_hash`.
/// 2. [`verify_cert_chain`] — walks the bundle's
///    `x509CertificateChain` toward a root CA in `trust_root`. Each
///    cert's validity window must include `signed_at` (5-minute clock
///    skew tolerance applied).
///
/// On success, returns the in-toto statement (so callers can pluck out
/// the `signed_at` predicate) along with the index of the trust-root
/// CA that terminated the chain (useful for logging "signed under
/// Sigstore-public-good v1").
///
/// Self-signed bundles (no cert chain) are rejected here with
/// [`Error::MissingVerificationKey`] — callers that want to permit
/// self-signed bundles should use [`verify_bundle_offline`] directly.
pub fn verify_bundle_with_trust_root(
    bundle: &SigstoreBundle,
    expected_pack_hash: &str,
    trust_root: &TrustedRoot,
    signed_at: SystemTime,
) -> Result<(InTotoStatement, usize)> {
    // Cryptographic verification first — short-circuits on signature
    // mismatch, subject digest mismatch, etc.
    let statement = verify_bundle_offline(bundle, expected_pack_hash)?;

    let chain = bundle
        .verification_material
        .x509_certificate_chain
        .as_ref()
        .ok_or(Error::MissingVerificationKey)?;
    let root_idx = verify_cert_chain(chain, trust_root, signed_at)?;
    Ok((statement, root_idx))
}

/// Try DER-encoded ECDSA signature first, fall back to raw 64-byte
/// IEEE P1363 r ‖ s. Documented input contract on
/// [`verify_dsse_with_ecdsa_cert`].
fn parse_ecdsa_p256_signature(bytes: &[u8]) -> Result<p256::ecdsa::Signature> {
    if let Ok(s) = p256::ecdsa::Signature::from_der(bytes) {
        return Ok(s);
    }
    if bytes.len() == 64
        && let Ok(s) = p256::ecdsa::Signature::from_slice(bytes)
    {
        return Ok(s);
    }
    Err(Error::EcdsaSignatureFormat)
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

    // ─────────────────────────────────────────────────────────────
    // ECDSA P-256 cert-bundle path — Fulcio-style bundles. These
    // tests exercise the verifier's cryptographic-validation path
    // without contacting Fulcio: we build a syntactically valid X.509
    // leaf cert wrapping a freshly-generated ECDSA P-256 keypair, sign
    // a DSSE envelope with that key, and assemble a bundle with the
    // `x509CertificateChain` field set instead of `publicKey`. Cert
    // chain validation against a trust root is the next commit's job;
    // these tests prove the signature-verification path itself works.
    // ─────────────────────────────────────────────────────────────

    use ::der::Encode as _;
    use ::der::asn1::BitString;
    // `Signer::sign` is already in scope via the top-of-file
    // `ed25519_dalek::Signer` import (which is a re-export of
    // `signature::Signer`); no need to bring it in again.
    use p256::ecdsa::{Signature as P256Signature, SigningKey as P256SigningKey};
    use spki::{EncodePublicKey as _, SubjectPublicKeyInfoOwned};
    use std::str::FromStr;
    use x509_cert::Certificate as X509Cert;
    use x509_cert::TbsCertificate;
    use x509_cert::Version;
    use x509_cert::name::Name;
    use x509_cert::serial_number::SerialNumber;
    use x509_cert::spki::AlgorithmIdentifierOwned;
    use x509_cert::time::Validity;

    /// Build a syntactically valid X.509 leaf cert wrapping the given
    /// ECDSA P-256 verifying key. The cert's issuer signature field is
    /// dummy bytes — fine here because Task #52 only verifies the DSSE
    /// signature (chain-of-trust validation is the next commit's job).
    fn build_minimal_p256_leaf_cert(verifying: &p256::ecdsa::VerifyingKey) -> Vec<u8> {
        // SubjectPublicKeyInfo: convert through `p256::PublicKey`
        // (whose `EncodePublicKey` impl is what the elliptic-curve crate
        // ships). `ecdsa::VerifyingKey<NistP256>` doesn't carry that
        // trait directly in this version, so we go via `From`.
        let pk = p256::PublicKey::from(verifying);
        let spki_der = pk.to_public_key_der().expect("encode SPKI");
        let spki = SubjectPublicKeyInfoOwned::try_from(spki_der.as_bytes())
            .expect("parse SPKI");

        // ecdsa-with-SHA256 — the algorithm OID Sigstore-issued certs
        // use for the issuer signature.
        let ecdsa_with_sha256 =
            ::der::asn1::ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2");
        let sig_alg = AlgorithmIdentifierOwned {
            oid: ecdsa_with_sha256,
            parameters: None,
        };

        let tbs = TbsCertificate {
            version: Version::V3,
            serial_number: SerialNumber::from(1u32),
            signature: sig_alg.clone(),
            issuer: Name::from_str("CN=test-issuer").expect("parse issuer"),
            validity: Validity::from_now(std::time::Duration::from_secs(86400))
                .expect("validity window"),
            subject: Name::from_str("CN=test-subject").expect("parse subject"),
            subject_public_key_info: spki,
            issuer_unique_id: None,
            subject_unique_id: None,
            extensions: None,
        };

        let cert = X509Cert {
            tbs_certificate: tbs,
            signature_algorithm: sig_alg,
            // A real Fulcio-issued cert carries the issuer's ECDSA
            // signature here; our test cert carries dummy zero bytes
            // because Task #52's verifier doesn't validate the cert
            // chain. The next commit's `verify_v3_pack_with_trust_root`
            // is what notices a bogus issuer signature.
            signature: BitString::from_bytes(&[0u8; 64]).expect("dummy bitstring"),
        };

        cert.to_der().expect("encode cert")
    }

    fn deterministic_p256_key(seed: u8) -> P256SigningKey {
        let mut bytes = [0u8; 32];
        bytes[31] = seed.max(1); // avoid 0
        P256SigningKey::from_slice(&bytes).expect("p256 from slice")
    }

    /// Sign a v3 bundle with an ECDSA P-256 key (as Fulcio would) and
    /// return the assembled bundle ready for `verify_bundle_offline`.
    fn build_ecdsa_signed_bundle(
        signing: &P256SigningKey,
        pack_hash: &str,
        pack_filename: &str,
    ) -> SigstoreBundle {
        let bare = strip_blake3_prefix(pack_hash);
        let statement = InTotoStatement {
            statement_type: IN_TOTO_STATEMENT_V1.to_string(),
            subject: vec![Subject {
                name: pack_filename.to_string(),
                digest: {
                    let mut m = serde_json::Map::new();
                    m.insert(
                        "blake3".to_string(),
                        serde_json::Value::String(bare.to_string()),
                    );
                    m
                },
            }],
            predicate_type: DSSE_STATEMENT_TYPE.to_string(),
            predicate: PackPredicate {
                format_version: "tr/3".to_string(),
                signed_at: format_rfc3339(SystemTime::UNIX_EPOCH),
            },
        };

        let payload_bytes = serde_json::to_vec(&statement).expect("encode statement");
        let pae = dsse_pae(DSSE_PAYLOAD_TYPE, &payload_bytes);

        // Sigstore-rs / cosign convention: DER-encoded ECDSA signature.
        let sig: P256Signature = signing.sign(&pae);
        let sig_der = sig.to_der();
        let sig_bytes = sig_der.as_bytes();

        let cert_der = build_minimal_p256_leaf_cert(signing.verifying_key());
        let b64 = base64::engine::general_purpose::STANDARD;

        SigstoreBundle {
            media_type: SIGSTORE_BUNDLE_MEDIA_TYPE.to_string(),
            verification_material: VerificationMaterial {
                public_key: None,
                x509_certificate_chain: Some(X509CertificateChain {
                    certificates: vec![X509Certificate {
                        raw_bytes: b64.encode(&cert_der),
                    }],
                }),
                tlog_entries: Vec::new(),
            },
            dsse_envelope: DsseEnvelope {
                payload: b64.encode(&payload_bytes),
                payload_type: DSSE_PAYLOAD_TYPE.to_string(),
                signatures: vec![DsseSignature {
                    sig: b64.encode(sig_bytes),
                }],
            },
        }
    }

    #[test]
    fn ecdsa_p256_cert_bundle_round_trips_through_verify() {
        let signing = deterministic_p256_key(0x42);
        let bundle = build_ecdsa_signed_bundle(&signing, fixture_hash(), "fulcio-style.tr");
        let stmt = verify_bundle_offline(&bundle, fixture_hash()).unwrap();
        assert_eq!(stmt.predicate_type, DSSE_STATEMENT_TYPE);
        assert_eq!(stmt.predicate.format_version, "tr/3");
        assert_eq!(stmt.subject[0].name, "fulcio-style.tr");
    }

    #[test]
    fn ecdsa_p256_round_trip_with_raw_64_byte_signature() {
        // Some non-Sigstore signers emit raw IEEE P1363 (r ‖ s) instead
        // of DER-encoded signatures. The verifier must accept both;
        // build a bundle whose `sig` field is the 64-byte raw form.
        let signing = deterministic_p256_key(0x55);
        let mut bundle = build_ecdsa_signed_bundle(&signing, fixture_hash(), "raw-sig.tr");

        // Re-sign the same PAE with the raw fixed-length encoding.
        let b64 = base64::engine::general_purpose::STANDARD;
        let payload = b64.decode(&bundle.dsse_envelope.payload).unwrap();
        let pae = dsse_pae(&bundle.dsse_envelope.payload_type, &payload);
        let sig: P256Signature = signing.sign(&pae);
        let raw_bytes = sig.to_bytes(); // 64 bytes
        assert_eq!(raw_bytes.len(), 64);
        bundle.dsse_envelope.signatures[0].sig = b64.encode(raw_bytes);

        verify_bundle_offline(&bundle, fixture_hash()).unwrap();
    }

    #[test]
    fn ecdsa_p256_rejects_tampered_signature() {
        let signing = deterministic_p256_key(0x77);
        let mut bundle = build_ecdsa_signed_bundle(&signing, fixture_hash(), "tampered.tr");
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut sig_bytes = b64.decode(&bundle.dsse_envelope.signatures[0].sig).unwrap();
        // Flip a bit deep in the DER body — past the SEQUENCE header,
        // landing inside one of the INTEGERs. Result is still parseable
        // DER but the underlying r/s no longer satisfies the signature
        // equation.
        let target = sig_bytes.len() - 1;
        sig_bytes[target] ^= 0x01;
        bundle.dsse_envelope.signatures[0].sig = b64.encode(&sig_bytes);
        let err = verify_bundle_offline(&bundle, fixture_hash()).unwrap_err();
        assert!(
            matches!(err, Error::SignatureMismatch | Error::EcdsaSignatureFormat),
            "expected SignatureMismatch or EcdsaSignatureFormat, got {err:?}"
        );
    }

    #[test]
    fn ecdsa_p256_rejects_wrong_subject_digest() {
        let signing = deterministic_p256_key(0x99);
        let bundle = build_ecdsa_signed_bundle(&signing, fixture_hash(), "p.tr");
        let other =
            "blake3:0000000000000000000000000000000000000000000000000000000000000000";
        let err = verify_bundle_offline(&bundle, other).unwrap_err();
        assert!(matches!(err, Error::SubjectMismatch { .. }));
    }

    #[test]
    fn empty_cert_chain_errors_cleanly() {
        let signing = deterministic_p256_key(0xAA);
        let mut bundle = build_ecdsa_signed_bundle(&signing, fixture_hash(), "p.tr");
        bundle
            .verification_material
            .x509_certificate_chain
            .as_mut()
            .unwrap()
            .certificates
            .clear();
        bundle.verification_material.public_key = None;
        // Empty chain + no public key → MissingVerificationKey (the
        // dispatcher considers an empty chain "not set" and falls
        // through to checking public_key).
        let err = verify_bundle_offline(&bundle, fixture_hash()).unwrap_err();
        assert!(matches!(err, Error::MissingVerificationKey));
    }

    /// Build a 3-cert synthetic chain (root CA → intermediate → leaf)
    /// using the same hand-rolled approach as `trust_root::tests`. The
    /// leaf signs the DSSE PAE; the bundle ships [leaf, intermediate].
    /// Returns the assembled bundle, the root cert (trust-root input),
    /// and the leaf signing key (so callers can produce signatures
    /// against the same key in additional tests).
    fn build_full_synthetic_bundle(
        pack_hash: &str,
        pack_filename: &str,
    ) -> (SigstoreBundle, Vec<u8>) {
        use crate::trust_root::test_helpers as tr_tests;

        let root_signer = tr_tests::p256_key(0xC1);
        let int_signer = tr_tests::p256_key(0xC2);
        let leaf_signer = tr_tests::p256_key(0xC3);

        let (root_cert, root_name) =
            tr_tests::build_self_signed_root(&root_signer, "Synthetic Root");
        let (int_cert, int_name) = tr_tests::build_intermediate(
            &root_signer,
            &root_name,
            &int_signer,
            "Synthetic Intermediate",
        );
        let leaf_cert =
            tr_tests::build_leaf(&int_signer, &int_name, &leaf_signer, "leaf");

        // Sign the DSSE PAE with the leaf.
        let bare = strip_blake3_prefix(pack_hash);
        let statement = InTotoStatement {
            statement_type: IN_TOTO_STATEMENT_V1.to_string(),
            subject: vec![Subject {
                name: pack_filename.to_string(),
                digest: {
                    let mut m = serde_json::Map::new();
                    m.insert(
                        "blake3".to_string(),
                        serde_json::Value::String(bare.to_string()),
                    );
                    m
                },
            }],
            predicate_type: DSSE_STATEMENT_TYPE.to_string(),
            predicate: PackPredicate {
                format_version: "tr/3".to_string(),
                signed_at: format_rfc3339(SystemTime::now()),
            },
        };
        let payload_bytes = serde_json::to_vec(&statement).unwrap();
        let pae = dsse_pae(DSSE_PAYLOAD_TYPE, &payload_bytes);
        let sig: P256Signature = leaf_signer.sign(&pae);
        let sig_der = sig.to_der();

        let b64 = base64::engine::general_purpose::STANDARD;
        let leaf_der = ::der::Encode::to_der(&leaf_cert).unwrap();
        let int_der = ::der::Encode::to_der(&int_cert).unwrap();
        let root_der = ::der::Encode::to_der(&root_cert).unwrap();

        let bundle = SigstoreBundle {
            media_type: SIGSTORE_BUNDLE_MEDIA_TYPE.to_string(),
            verification_material: VerificationMaterial {
                public_key: None,
                x509_certificate_chain: Some(X509CertificateChain {
                    certificates: vec![
                        X509Certificate {
                            raw_bytes: b64.encode(&leaf_der),
                        },
                        X509Certificate {
                            raw_bytes: b64.encode(&int_der),
                        },
                    ],
                }),
                tlog_entries: Vec::new(),
            },
            dsse_envelope: DsseEnvelope {
                payload: b64.encode(&payload_bytes),
                payload_type: DSSE_PAYLOAD_TYPE.to_string(),
                signatures: vec![DsseSignature {
                    sig: b64.encode(sig_der.as_bytes()),
                }],
            },
        };
        (bundle, root_der)
    }

    #[test]
    fn full_chain_bundle_verifies_against_synthetic_trust_root() {
        let (bundle, root_der) =
            build_full_synthetic_bundle(fixture_hash(), "full-chain.tr");
        let trust_root = TrustedRoot::from_root_ders(&[&root_der]).unwrap();
        let (stmt, root_idx) = verify_bundle_with_trust_root(
            &bundle,
            fixture_hash(),
            &trust_root,
            SystemTime::now(),
        )
        .unwrap();
        assert_eq!(root_idx, 0);
        assert_eq!(stmt.predicate.format_version, "tr/3");
        assert_eq!(stmt.subject[0].name, "full-chain.tr");
    }

    #[test]
    fn full_chain_bundle_with_no_chain_errors_missing_key() {
        let (mut bundle, root_der) =
            build_full_synthetic_bundle(fixture_hash(), "p.tr");
        bundle.verification_material.x509_certificate_chain = None;
        bundle.verification_material.public_key = None;
        let trust_root = TrustedRoot::from_root_ders(&[&root_der]).unwrap();
        let err = verify_bundle_with_trust_root(
            &bundle,
            fixture_hash(),
            &trust_root,
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::MissingVerificationKey));
    }

    #[test]
    fn full_chain_bundle_with_self_signed_only_rejected_by_trust_root_path() {
        // verify_bundle_with_trust_root does not accept self-signed
        // bundles. The user-facing pattern is: callers that want to
        // permit both self-signed AND Sigstore-signed bundles run
        // verify_bundle_offline first, then conditionally run trust-
        // root validation only if a chain is present.
        let key = generate_signing_key(0xD0);
        let bundle = sign_pack(fixture_hash(), "p.tr", &key, SystemTime::now()).unwrap();
        // Build a trust root with one bogus root CA — the check should
        // fail with MissingVerificationKey before chain validation runs.
        let bogus_root = deterministic_p256_key(0xD1);
        let (root_cert, _) = crate::trust_root::test_helpers::build_self_signed_root(
            &bogus_root,
            "Bogus",
        );
        let root_der = ::der::Encode::to_der(&root_cert).unwrap();
        let trust_root = TrustedRoot::from_root_ders(&[&root_der]).unwrap();
        let err = verify_bundle_with_trust_root(
            &bundle,
            fixture_hash(),
            &trust_root,
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::MissingVerificationKey));
    }

    #[test]
    fn corrupt_leaf_cert_der_errors_cleanly() {
        let signing = deterministic_p256_key(0xBB);
        let mut bundle = build_ecdsa_signed_bundle(&signing, fixture_hash(), "p.tr");
        let b64 = base64::engine::general_purpose::STANDARD;
        bundle
            .verification_material
            .x509_certificate_chain
            .as_mut()
            .unwrap()
            .certificates[0]
            .raw_bytes = b64.encode([0xDE, 0xAD, 0xBE, 0xEF]);
        let err = verify_bundle_offline(&bundle, fixture_hash()).unwrap_err();
        assert!(
            matches!(err, Error::CertParse(_)),
            "expected CertParse, got {err:?}"
        );
    }
}
