//! Live keyless-signing primitives — gated behind the `live` feature.
//!
//! Two layers, separable for headless vs interactive callers:
//!
//! 1. **OIDC token acquisition** — [`identity_token_from_jwt`] wraps a
//!    pre-fetched JWT (CI ambient federated identity, environment
//!    variable, header from a custom OAuth flow). [`browser_oidc_flow`]
//!    runs the interactive PKCE redirect against Sigstore-public-good's
//!    OIDC issuer.
//!
//! 2. **Keyless DSSE signing** — [`sign_canonical_bytes_keyless`] takes a
//!    JWT plus the v3 pack's canonical bytes, requests an ephemeral
//!    cert from Fulcio, signs the DSSE-PAE-encoded in-toto statement
//!    with the issued ephemeral key, submits the signed envelope to
//!    Rekor as an `intoto v0.0.2` entry, and assembles a
//!    [`SigstoreBundle`] ready to drop into the v3 outer tar's
//!    `signature.sig` slot.
//!
//! Why we drive Rekor directly (not via `sigstore::rekor`): sigstore-rs
//! 0.13's bundle layer hardcodes `kind="hashedrekord"` in its
//! `TryFrom<RekorLogEntry> for TransparencyLogEntry` impl and the
//! verify path explicitly errors `DsseUnsupported`. The high-level
//! `bundle::sign::SigningContext::sign(reader)` therefore emits
//! `MessageSignature` bundles incompatible with v3's DSSE-only wire
//! format. We use sigstore-rs for the Fulcio cert request + ephemeral
//! signer (the load-bearing keyless primitive) and bypass its bundle
//! layer entirely.
//!
//! Subject-digest convention: keyless bundles emit BOTH `blake3` (the
//! v3 manifest's pack_hash, our chain) and `sha256` (the cosign /
//! Sigstore ecosystem default) under the in-toto subject. Our verifier
//! ([`crate::verify_bundle_against_canonical_bytes`]) already accepts
//! either; cosign/SLSA tooling reads `sha256` and ignores algorithms
//! they don't recognize. In-toto v1 explicitly permits multi-algorithm
//! digest maps for exactly this case.

#![allow(missing_docs)] // re-exported sigstore types document themselves

use std::time::SystemTime;

use base64::Engine as _;
use serde::Deserialize;
use sigstore::oauth::IdentityToken as SigstoreIdentityToken;

use crate::{
    DSSE_PAYLOAD_TYPE, DSSE_STATEMENT_TYPE, DsseEnvelope, DsseSignature, Error,
    IN_TOTO_STATEMENT_V1, InTotoStatement, KindVersion, LogId, PackPredicate, RekorCheckpoint,
    RekorInclusionProof, SIGSTORE_BUNDLE_MEDIA_TYPE, SigstoreBundle, Subject, TlogEntry,
    VerificationMaterial, X509Certificate, X509CertificateChain, dsse_pae, format_rfc3339,
    sha256_hex,
};

// Re-exports — using these directly from `tr_sigstore::live` keeps
// callers from ever needing to depend on the `sigstore` crate
// themselves (which means callers also avoid the heavy transitive
// deps unless they enable our `live` feature).
pub use sigstore::bundle::Bundle as SigstoreSdkBundle;
pub use sigstore::oauth::IdentityToken;

/// Default Sigstore-public-good Fulcio root URL. Production callers
/// override via [`SignKeylessOptions::fulcio_url`] to point at a
/// private deployment.
pub const FULCIO_URL: &str = "https://fulcio.sigstore.dev";

/// Default Sigstore-public-good Rekor base URL.
pub const REKOR_URL: &str = "https://rekor.sigstore.dev";

/// Construct a [`IdentityToken`] from a raw JWT string (typically an
/// OIDC id_token obtained out-of-band — e.g. CI environment, an
/// ambient federated identity, a previously-cached token).
///
/// The JWT is parsed but *not* verified against an OIDC issuer's
/// keys: that verification happens at Fulcio's end of the keyless
/// flow. This function only structurally validates the JWT shape so
/// failures surface at the caller's boundary rather than deep inside
/// the Sigstore SDK.
///
/// `IdentityToken::try_from(&str)` enforces `aud == "sigstore"` —
/// non-Sigstore-aud tokens are rejected here.
pub fn identity_token_from_jwt(jwt: &str) -> Result<IdentityToken, Error> {
    SigstoreIdentityToken::try_from(jwt)
        .map_err(|e| Error::CertParse(format!("OIDC id_token parse: {e}")))
}

/// Open the user's default browser to the Sigstore-public-good OIDC
/// issuer, run a PKCE-protected redirect listener on
/// `127.0.0.1:8080`, and return the resulting [`IdentityToken`].
///
/// Blocking — the function returns once the user completes the
/// browser flow. Used by `root pack --sign-keyless` from the CLI.
/// For headless / CI flows, prefer [`identity_token_from_jwt`] with
/// an ambient OIDC token.
///
/// Defaults match Sigstore's public-good instance:
/// - issuer: `https://oauth2.sigstore.dev/auth`
/// - client id: `sigstore`
/// - redirect: `http://localhost:8080`
pub fn browser_oidc_flow(
    issuer_url: Option<&str>,
    client_id: Option<&str>,
    redirect_url: Option<&str>,
) -> Result<IdentityToken, Error> {
    use sigstore::oauth::openidflow::{OpenIDAuthorize, RedirectListener};

    let issuer = issuer_url.unwrap_or("https://oauth2.sigstore.dev/auth");
    let client = client_id.unwrap_or("sigstore");
    let redirect = redirect_url.unwrap_or("http://localhost:8080");

    let (auth_url, oauth_client, nonce, pkce_verifier) =
        OpenIDAuthorize::new(client, "", issuer, redirect)
            .auth_url()
            .map_err(|e| Error::CertParse(format!("OIDC authorize URL: {e}")))?;

    webbrowser::open(auth_url.as_ref())
        .map_err(|e| Error::CertParse(format!("open browser: {e}")))?;

    let listener_addr = redirect
        .strip_prefix("http://")
        .unwrap_or(redirect)
        .trim_end_matches('/');

    let (_claims, raw_token) =
        RedirectListener::new(listener_addr, oauth_client, nonce, pkce_verifier)
            .redirect_listener()
            .map_err(|e| Error::CertParse(format!("OIDC redirect listener: {e}")))?;

    Ok(IdentityToken::from(raw_token))
}

/// Optional overrides for [`sign_canonical_bytes_keyless`]. `Default`
/// targets Sigstore-public-good and is the right choice for any caller
/// signing for the public ecosystem.
#[derive(Debug, Clone)]
pub struct SignKeylessOptions {
    /// Fulcio root URL. Defaults to [`FULCIO_URL`].
    pub fulcio_url: String,
    /// Rekor base URL. Defaults to [`REKOR_URL`].
    pub rekor_url: String,
}

impl Default for SignKeylessOptions {
    fn default() -> Self {
        Self {
            fulcio_url: FULCIO_URL.to_string(),
            rekor_url: REKOR_URL.to_string(),
        }
    }
}

/// Sign a v3 pack's canonical bytes via Sigstore-keyless and return a
/// [`SigstoreBundle`] ready for the outer tar's `signature.sig` slot.
///
/// End-to-end:
///
/// 1. Build the in-toto v1 statement: `subject[0]` carries `pack_filename`
///    + a digest map containing both `blake3` (the v3 chain) and
///    `sha256` (Sigstore ecosystem). `predicateType` is locked to
///    [`DSSE_STATEMENT_TYPE`]; `predicate.format_version="tr/3"` and
///    `signed_at` is RFC 3339.
/// 2. Wrap `jwt` as `TokenProvider::Static((CoreIdToken, challenge))`,
///    where the challenge is the JWT's `email` claim (Sigstore-public-
///    good's policy) or `sub` if `email` is absent (CI / SPIFFE).
/// 3. `FulcioClient::request_cert(ECDSA_P256_SHA256_ASN1)` issues an
///    ephemeral ECDSA P-256 keypair + leaf cert. Fulcio verifies the
///    JWT, asserts the challenge matches the configured claim, signs
///    the leaf cert.
/// 4. DSSE-PAE encode `(payloadType, payload_bytes)`, sign the PAE with
///    `SigStoreSigner::sign(...)`. Output is ASN.1 DER ECDSA-P256-SHA256.
/// 5. Build the Rekor `intoto v0.0.2` proposed-entry body — DSSE
///    envelope inline, leaf cert PEM in `publicKey`. POST to
///    `${rekor_url}/api/v1/log/entries`.
/// 6. Parse the Rekor response (a UUID→LogEntry map). Convert hex
///    fields (`logID`, `rootHash`, `hashes`) to base64 for our
///    Sigstore-Bundle-v0.3 wire shape; populate the [`TlogEntry`].
/// 7. Assemble the [`SigstoreBundle`]: `verification_material` carries
///    the cert chain (parsed leaf-first from Fulcio's PEM blob) plus
///    the single Rekor entry; `dsse_envelope` carries the signed
///    payload.
///
/// Every offline verification path in this crate
/// ([`crate::verify_bundle_offline`], [`crate::verify_bundle_against_canonical_bytes`],
/// [`crate::verify_bundle_with_trust_root`]) already accepts the
/// resulting bundle shape — chain validation against the Sigstore
/// trust root, ECDSA-P256 sig over the DSSE PAE, and Rekor inclusion
/// proof replay all work out of the box.
pub async fn sign_canonical_bytes_keyless(
    canonical_bytes: &[u8],
    pack_filename: &str,
    jwt: &str,
    signed_at: SystemTime,
    options: SignKeylessOptions,
) -> Result<SigstoreBundle, Error> {
    use openidconnect::core::CoreIdToken;
    use sigstore::crypto::SigningScheme;
    use sigstore::fulcio::{FulcioClient, TokenProvider};
    use std::str::FromStr as _;

    // ─── Step 1: Build in-toto statement with dual-algorithm subject. ──
    let blake3_hex = blake3::hash(canonical_bytes).to_hex().to_string();
    let sha256_subject = sha256_hex(canonical_bytes);

    let statement = InTotoStatement {
        statement_type: IN_TOTO_STATEMENT_V1.to_string(),
        subject: vec![Subject {
            name: pack_filename.to_string(),
            digest: {
                let mut m = serde_json::Map::new();
                m.insert(
                    "blake3".to_string(),
                    serde_json::Value::String(blake3_hex.clone()),
                );
                m.insert(
                    "sha256".to_string(),
                    serde_json::Value::String(sha256_subject.clone()),
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

    // ─── Step 2: JWT → TokenProvider::Static. ──────────────────────────
    let challenge = challenge_from_jwt(jwt)?;
    let core_token = CoreIdToken::from_str(jwt)
        .map_err(|e| Error::CertParse(format!("CoreIdToken parse: {e}")))?;
    let provider = TokenProvider::Static((core_token, challenge));

    // ─── Step 3: Fulcio cert request. ──────────────────────────────────
    let fulcio_url = reqwest::Url::parse(&options.fulcio_url)
        .map_err(|e| Error::CertParse(format!("Fulcio URL: {e}")))?;
    let client = FulcioClient::new(fulcio_url, provider);
    let (signer, fulcio_cert) = client
        .request_cert(SigningScheme::ECDSA_P256_SHA256_ASN1)
        .await
        .map_err(|e| Error::CertParse(format!("Fulcio request_cert: {e}")))?;

    // ─── Step 4: DSSE-PAE encode + sign with ephemeral key. ────────────
    let pae = dsse_pae(DSSE_PAYLOAD_TYPE, &payload_bytes);
    let sig_bytes = signer
        .sign(&pae)
        .map_err(|e| Error::CertParse(format!("ephemeral DSSE sign: {e}")))?;

    // ─── Step 5: Parse Fulcio cert chain (PEM blob → DER blocks). ──────
    let pem_text = std::str::from_utf8(fulcio_cert.as_ref())
        .map_err(|e| Error::CertParse(format!("FulcioCert UTF-8: {e}")))?;
    let cert_chain_der = parse_pem_cert_chain(pem_text)?;
    let leaf_cert_pem = first_pem_block(pem_text)?;

    // ─── Step 6: Build + POST Rekor proposed entry. ───────────────────
    let b64 = base64::engine::general_purpose::STANDARD;
    let payload_b64 = b64.encode(&payload_bytes);
    let sig_b64 = b64.encode(&sig_bytes);
    let leaf_pem_b64 = b64.encode(leaf_cert_pem.as_bytes());

    let proposed_entry = serde_json::json!({
        "kind": "intoto",
        "apiVersion": "0.0.2",
        "spec": {
            "content": {
                "envelope": {
                    "payloadType": DSSE_PAYLOAD_TYPE,
                    "payload": payload_b64,
                    "signatures": [{
                        "sig": sig_b64,
                        "publicKey": leaf_pem_b64,
                    }]
                }
            }
        }
    });

    let rekor_entry = submit_to_rekor(&options.rekor_url, &proposed_entry).await?;

    // ─── Step 7: Convert Rekor REST response into a TlogEntry. ────────
    let tlog_entry = rekor_entry_to_tlog(&rekor_entry)?;

    // ─── Step 8: Assemble SigstoreBundle. ─────────────────────────────
    let cert_objects: Vec<X509Certificate> = cert_chain_der
        .into_iter()
        .map(|der| X509Certificate {
            raw_bytes: b64.encode(&der),
        })
        .collect();

    Ok(SigstoreBundle {
        media_type: SIGSTORE_BUNDLE_MEDIA_TYPE.to_string(),
        verification_material: VerificationMaterial {
            public_key: None,
            x509_certificate_chain: Some(X509CertificateChain {
                certificates: cert_objects,
            }),
            tlog_entries: vec![tlog_entry],
        },
        dsse_envelope: DsseEnvelope {
            payload: payload_b64,
            payload_type: DSSE_PAYLOAD_TYPE.to_string(),
            signatures: vec![DsseSignature { sig: sig_b64 }],
        },
    })
}

// ─────────────────────────────────────────────────────────────────────
// JWT helpers
// ─────────────────────────────────────────────────────────────────────

/// Extract the value Fulcio's challenge claim expects from a JWT. For
/// Sigstore-public-good's policy this is `email` for OAuth issuers,
/// `sub` for SPIFFE / GitHub Actions / CI tokens. We try `email`
/// first, fall back to `sub`. Mismatches surface as a clean Fulcio
/// 403 (the server rejects the cert request) rather than a confusing
/// silent failure.
fn challenge_from_jwt(jwt: &str) -> Result<String, Error> {
    let segments: Vec<&str> = jwt.split('.').collect();
    if segments.len() != 3 {
        return Err(Error::CertParse(
            "JWT does not have three segments".to_string(),
        ));
    }
    let claims_b64 = segments[1];
    let claims_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(claims_b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(claims_b64))
        .map_err(|e| Error::CertParse(format!("JWT claims base64: {e}")))?;
    let claims: serde_json::Value = serde_json::from_slice(&claims_bytes)
        .map_err(|e| Error::CertParse(format!("JWT claims JSON: {e}")))?;

    if let Some(email) = claims.get("email").and_then(|v| v.as_str()) {
        return Ok(email.to_string());
    }
    if let Some(sub) = claims.get("sub").and_then(|v| v.as_str()) {
        return Ok(sub.to_string());
    }
    Err(Error::CertParse(
        "JWT claims missing both `email` and `sub` — no Fulcio challenge value available"
            .to_string(),
    ))
}

// ─────────────────────────────────────────────────────────────────────
// PEM cert chain parsing
// ─────────────────────────────────────────────────────────────────────

/// Split a multi-cert PEM blob (the shape Fulcio's v1 endpoint returns)
/// into individual DER-encoded certs, in the same order as the blob.
/// The leaf cert is first; intermediates and the root follow.
///
/// Hand-rolled instead of pulling in `pem` directly: the PEM format is
/// trivial (BEGIN/END markers + base64 body), and the existing crypto
/// dep tree already pulls in the `pem` crate transitively — we don't
/// want to compile a second copy at a different version.
fn parse_pem_cert_chain(pem: &str) -> Result<Vec<Vec<u8>>, Error> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";

    let mut out = Vec::new();
    let mut rest = pem;
    while let Some(begin_idx) = rest.find(BEGIN) {
        let body_start = begin_idx + BEGIN.len();
        let end_idx = rest[body_start..]
            .find(END)
            .ok_or_else(|| Error::CertParse("PEM block missing END marker".to_string()))?
            + body_start;
        let body = &rest[body_start..end_idx];
        let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(&cleaned)
            .map_err(|e| Error::CertParse(format!("PEM body base64: {e}")))?;
        out.push(der);
        rest = &rest[end_idx + END.len()..];
    }
    if out.is_empty() {
        return Err(Error::CertParse(
            "PEM blob has no CERTIFICATE blocks".to_string(),
        ));
    }
    Ok(out)
}

/// Slice the first complete PEM cert block (BEGIN…END inclusive) out
/// of a multi-cert PEM blob. Returned as a string slice for direct use
/// in the Rekor `publicKey` field — Rekor wants a full PEM document
/// there, not just the DER bytes.
fn first_pem_block(pem: &str) -> Result<&str, Error> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";

    let begin_idx = pem
        .find(BEGIN)
        .ok_or_else(|| Error::CertParse("PEM blob has no BEGIN marker".to_string()))?;
    let end_idx = pem[begin_idx..]
        .find(END)
        .ok_or_else(|| Error::CertParse("PEM blob has no END marker".to_string()))?
        + begin_idx
        + END.len();
    Ok(&pem[begin_idx..end_idx])
}

// ─────────────────────────────────────────────────────────────────────
// Rekor REST submission
// ─────────────────────────────────────────────────────────────────────

/// Wire shape Rekor's REST API returns from `POST /api/v1/log/entries`.
/// The response is a single-element map keyed by the new entry's UUID.
type RekorCreateResponse = std::collections::HashMap<String, RekorRestLogEntry>;

#[derive(Debug, Deserialize)]
struct RekorRestLogEntry {
    /// Base64 of the canonicalized entry body. Rekor performs
    /// canonicalization server-side (RFC 8785 / JCS), so this is the
    /// authoritative byte sequence for the leaf hash.
    #[serde(default)]
    #[allow(dead_code)] // populated for completeness; we re-encode for canonicalized_body
    body: Option<String>,
    #[serde(rename = "integratedTime")]
    integrated_time: i64,
    /// Hex-encoded SHA-256 of the Rekor pubkey. Bundle wire wants
    /// base64 of those bytes.
    #[serde(rename = "logID")]
    log_id: String,
    #[serde(rename = "logIndex")]
    log_index: i64,
    verification: RekorRestVerification,
}

#[derive(Debug, Deserialize)]
struct RekorRestVerification {
    #[serde(rename = "signedEntryTimestamp", default)]
    signed_entry_timestamp: Option<String>,
    #[serde(rename = "inclusionProof", default)]
    inclusion_proof: Option<RekorRestInclusionProof>,
}

#[derive(Debug, Deserialize)]
struct RekorRestInclusionProof {
    #[serde(rename = "logIndex")]
    log_index: i64,
    #[serde(rename = "treeSize")]
    tree_size: i64,
    /// Hex-encoded 32-byte hash. Bundle wire wants base64.
    #[serde(rename = "rootHash")]
    root_hash: String,
    #[serde(default)]
    hashes: Vec<String>,
    #[serde(default)]
    checkpoint: Option<String>,
}

/// POST a proposed entry to Rekor's `/api/v1/log/entries` endpoint and
/// return the (single) created [`RekorRestLogEntry`].
async fn submit_to_rekor(
    rekor_url: &str,
    proposed: &serde_json::Value,
) -> Result<RekorRestLogEntry, Error> {
    let endpoint = format!("{}/api/v1/log/entries", rekor_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let resp = client
        .post(&endpoint)
        .json(proposed)
        .send()
        .await
        .map_err(|e| Error::CertParse(format!("Rekor POST: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::CertParse(format!("Rekor returned {status}: {body}")));
    }

    let parsed: RekorCreateResponse = resp
        .json()
        .await
        .map_err(|e| Error::CertParse(format!("Rekor response decode: {e}")))?;
    parsed
        .into_values()
        .next()
        .ok_or_else(|| Error::CertParse("Rekor response had no log entry".to_string()))
}

/// Convert a Rekor REST [`RekorRestLogEntry`] (hex-encoded fields, REST
/// shape) into a Sigstore-Bundle-v0.3 [`TlogEntry`] (base64-encoded
/// fields, proto-derived shape).
fn rekor_entry_to_tlog(entry: &RekorRestLogEntry) -> Result<TlogEntry, Error> {
    let b64 = base64::engine::general_purpose::STANDARD;

    // logID: hex → bytes → base64.
    let log_id_bytes =
        hex_decode(&entry.log_id).map_err(|e| Error::CertParse(format!("Rekor logID hex: {e}")))?;
    let log_id_b64 = b64.encode(&log_id_bytes);

    let inclusion_proof = entry.verification.inclusion_proof.as_ref().map(|p| {
        let root_bytes = hex_decode(&p.root_hash).unwrap_or_default();
        let root_b64 = b64.encode(&root_bytes);
        let hashes_b64: Vec<String> = p
            .hashes
            .iter()
            .map(|h| {
                let bytes = hex_decode(h).unwrap_or_default();
                b64.encode(&bytes)
            })
            .collect();
        RekorInclusionProof {
            log_index: p.log_index,
            tree_size: p.tree_size,
            root_hash: root_b64,
            hashes: hashes_b64,
            checkpoint: p.checkpoint.as_ref().map(|c| RekorCheckpoint {
                envelope: c.clone(),
            }),
        }
    });

    let inclusion_promise = entry
        .verification
        .signed_entry_timestamp
        .as_ref()
        .map(|set| crate::InclusionPromise {
            signed_entry_timestamp: set.clone(),
        });

    Ok(TlogEntry {
        log_index: entry.log_index,
        log_id: Some(LogId { key_id: log_id_b64 }),
        kind_version: Some(KindVersion {
            kind: "intoto".to_string(),
            version: "0.0.2".to_string(),
        }),
        integrated_time: entry.integrated_time,
        inclusion_promise,
        inclusion_proof,
        canonicalized_body: entry.body.clone(),
    })
}

/// Decode a lowercase hex string into bytes. Pure stdlib + iterator —
/// avoids pulling in `hex` directly (it's already in the transitive
/// tree but we don't take a direct dep without need).
fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("hex string length {} not even", s.len()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("invalid hex byte: 0x{b:02x}")),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Building a minimal valid JWT (header.payload.signature, all
    /// base64url-no-padding) lets us round-trip [`identity_token_from_jwt`]
    /// without needing a live OIDC issuer. The signature is dummy bytes
    /// — sigstore-rs's parser validates JWT structure, not the
    /// cryptographic signature, at this layer (Fulcio does the latter).
    fn fake_jwt() -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = b64.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = b64.encode(
            br#"{"iss":"https://accounts.google.com","sub":"naveen@thinkingroot.dev","aud":"sigstore","email":"naveen@thinkingroot.dev","exp":9999999999,"iat":1000000000,"nbf":1000000000}"#,
        );
        let sig = b64.encode(b"dummy-signature");
        format!("{header}.{payload}.{sig}")
    }

    /// `IdentityToken` doesn't implement `Debug`, so the standard
    /// `Result::unwrap_err()` (which prints the Ok side on failure)
    /// won't compile. This helper extracts the Err branch with a
    /// custom panic message, keeping test diagnostics clean.
    fn expect_err<T>(result: Result<T, Error>) -> Error {
        match result {
            Ok(_) => panic!("expected an error, got Ok"),
            Err(e) => e,
        }
    }

    #[test]
    fn identity_token_from_jwt_parses_well_formed_token() {
        let jwt = fake_jwt();
        let _token = identity_token_from_jwt(&jwt).expect("parse JWT");
    }

    #[test]
    fn identity_token_from_jwt_rejects_garbage() {
        let err = expect_err(identity_token_from_jwt("not-a-jwt"));
        assert!(matches!(err, Error::CertParse(_)));
    }

    #[test]
    fn identity_token_from_jwt_rejects_two_segment_token() {
        let err = expect_err(identity_token_from_jwt("only.two-segments"));
        assert!(matches!(err, Error::CertParse(_)));
    }

    #[test]
    fn challenge_from_jwt_prefers_email() {
        let jwt = fake_jwt();
        let challenge = challenge_from_jwt(&jwt).unwrap();
        assert_eq!(challenge, "naveen@thinkingroot.dev");
    }

    #[test]
    fn challenge_from_jwt_falls_back_to_sub() {
        // JWT with only `sub`, no `email` — CI / SPIFFE shape.
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = b64.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = b64.encode(
            br#"{"iss":"https://token.actions.githubusercontent.com","sub":"repo:owner/x:ref:refs/heads/main","aud":"sigstore","exp":9999999999}"#,
        );
        let sig = b64.encode(b"dummy");
        let jwt = format!("{header}.{payload}.{sig}");
        let challenge = challenge_from_jwt(&jwt).unwrap();
        assert_eq!(challenge, "repo:owner/x:ref:refs/heads/main");
    }

    #[test]
    fn challenge_from_jwt_rejects_two_segment_jwt() {
        let err = challenge_from_jwt("a.b").unwrap_err();
        assert!(matches!(err, Error::CertParse(_)));
    }

    #[test]
    fn challenge_from_jwt_rejects_token_without_email_or_sub() {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = b64.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = b64.encode(br#"{"iss":"x","aud":"sigstore","exp":9999999999}"#);
        let sig = b64.encode(b"dummy");
        let jwt = format!("{header}.{payload}.{sig}");
        let err = challenge_from_jwt(&jwt).unwrap_err();
        assert!(matches!(err, Error::CertParse(_)));
    }

    /// Synthetic 3-cert PEM blob covering leaf + intermediate + root.
    /// The body bytes are arbitrary (the parser doesn't decode the DER
    /// — that happens later). Tests that the splitter walks all three
    /// blocks in order.
    fn fake_pem_chain() -> String {
        // Three distinct base64 bodies — different content per cert
        // so a test catches accidental sharing or mis-ordering.
        let b64 = base64::engine::general_purpose::STANDARD;
        let cert1 = b64.encode(b"leaf-cert-bytes-here-pretend-this-is-DER");
        let cert2 = b64.encode(b"intermediate-cert-bytes-here-also-fake-DER");
        let cert3 = b64.encode(b"root-cert-bytes-here-the-trust-anchor-fake");
        format!(
            "-----BEGIN CERTIFICATE-----\n{cert1}\n-----END CERTIFICATE-----\n\
             -----BEGIN CERTIFICATE-----\n{cert2}\n-----END CERTIFICATE-----\n\
             -----BEGIN CERTIFICATE-----\n{cert3}\n-----END CERTIFICATE-----\n"
        )
    }

    #[test]
    fn parse_pem_cert_chain_walks_three_blocks_in_order() {
        let pem = fake_pem_chain();
        let chain = parse_pem_cert_chain(&pem).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(&chain[0], b"leaf-cert-bytes-here-pretend-this-is-DER");
        assert_eq!(&chain[1], b"intermediate-cert-bytes-here-also-fake-DER");
        assert_eq!(&chain[2], b"root-cert-bytes-here-the-trust-anchor-fake");
    }

    #[test]
    fn parse_pem_cert_chain_handles_extra_whitespace() {
        // Real Fulcio responses have line breaks every 64 base64 chars
        // inside the body, plus a trailing newline. The cleaner strips
        // all whitespace before base64-decoding — this guards against a
        // regression where we matched only `\n` and broke on `\r\n`.
        let b64 = base64::engine::general_purpose::STANDARD;
        let body = b64.encode(b"x".repeat(150).as_slice());
        let mut wrapped = String::new();
        for (i, c) in body.chars().enumerate() {
            wrapped.push(c);
            if i % 64 == 63 {
                wrapped.push_str("\r\n");
            }
        }
        let pem =
            format!("-----BEGIN CERTIFICATE-----\r\n{wrapped}\r\n-----END CERTIFICATE-----\r\n");
        let chain = parse_pem_cert_chain(&pem).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].len(), 150);
        assert!(chain[0].iter().all(|&b| b == b'x'));
    }

    #[test]
    fn parse_pem_cert_chain_rejects_empty() {
        let err = parse_pem_cert_chain("garbage no markers").unwrap_err();
        assert!(matches!(err, Error::CertParse(_)));
    }

    #[test]
    fn parse_pem_cert_chain_rejects_missing_end() {
        let pem = "-----BEGIN CERTIFICATE-----\nabc\n";
        let err = parse_pem_cert_chain(pem).unwrap_err();
        assert!(matches!(err, Error::CertParse(_)));
    }

    #[test]
    fn first_pem_block_returns_only_the_leaf() {
        let pem = fake_pem_chain();
        let leaf = first_pem_block(&pem).unwrap();
        assert!(leaf.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(leaf.ends_with("-----END CERTIFICATE-----"));
        // The leaf body alone — body of cert 2 should not be present.
        assert!(!leaf.contains("intermediate-cert"));
    }

    #[test]
    fn hex_decode_round_trips_canonical_lowercase() {
        let bytes = vec![0x00, 0xab, 0xcd, 0xef, 0x12, 0x34];
        let s: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let decoded = hex_decode(&s).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn hex_decode_accepts_uppercase() {
        let decoded = hex_decode("ABCDEF").unwrap();
        assert_eq!(decoded, vec![0xab, 0xcd, 0xef]);
    }

    #[test]
    fn hex_decode_rejects_odd_length() {
        let err = hex_decode("abc").unwrap_err();
        assert!(err.contains("not even"));
    }

    #[test]
    fn hex_decode_rejects_non_hex() {
        let err = hex_decode("abxy").unwrap_err();
        assert!(err.contains("invalid hex byte"));
    }

    #[test]
    fn rekor_entry_to_tlog_converts_hex_to_base64() {
        // Synthetic Rekor REST response with all hex fields populated.
        let entry = RekorRestLogEntry {
            body: Some("Y2Fub25pY2FsLWJvZHk=".to_string()),
            integrated_time: 1700000000,
            log_id: "0102030405060708090a0b0c0d0e0f1011121314151617181920212223242526".to_string(),
            log_index: 42,
            verification: RekorRestVerification {
                signed_entry_timestamp: Some("c2V0LWJhc2U2NA==".to_string()),
                inclusion_proof: Some(RekorRestInclusionProof {
                    log_index: 42,
                    tree_size: 100,
                    root_hash: "ab".repeat(32),
                    hashes: vec!["cd".repeat(32), "ef".repeat(32)],
                    checkpoint: Some("rekor.sigstore.dev - 1234\n100\nQUJDRA==\n".to_string()),
                }),
            },
        };
        let tlog = rekor_entry_to_tlog(&entry).unwrap();
        assert_eq!(tlog.log_index, 42);
        assert_eq!(tlog.integrated_time, 1700000000);
        assert_eq!(tlog.kind_version.as_ref().unwrap().kind, "intoto");
        assert_eq!(tlog.kind_version.as_ref().unwrap().version, "0.0.2");
        // logID hex → 32 bytes → base64 of 32 bytes = 44 chars (with padding).
        assert_eq!(tlog.log_id.as_ref().unwrap().key_id.len(), 44);
        let proof = tlog.inclusion_proof.as_ref().unwrap();
        assert_eq!(proof.tree_size, 100);
        assert_eq!(proof.hashes.len(), 2);
        // Each hash is 32 bytes → base64 = 44 chars.
        for h in &proof.hashes {
            assert_eq!(h.len(), 44);
        }
        assert!(proof.checkpoint.is_some());
        assert_eq!(
            tlog.inclusion_promise
                .as_ref()
                .unwrap()
                .signed_entry_timestamp,
            "c2V0LWJhc2U2NA=="
        );
    }

    #[test]
    fn rekor_entry_to_tlog_handles_missing_inclusion_proof() {
        // Some Rekor versions return only the SET, no inclusion proof.
        let entry = RekorRestLogEntry {
            body: None,
            integrated_time: 1700000000,
            log_id: "00".repeat(32),
            log_index: 0,
            verification: RekorRestVerification {
                signed_entry_timestamp: Some("c2V0".to_string()),
                inclusion_proof: None,
            },
        };
        let tlog = rekor_entry_to_tlog(&entry).unwrap();
        assert!(tlog.inclusion_proof.is_none());
        assert!(tlog.inclusion_promise.is_some());
    }
}
