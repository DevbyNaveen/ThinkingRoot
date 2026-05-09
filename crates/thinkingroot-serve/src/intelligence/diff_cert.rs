// crates/thinkingroot-serve/src/intelligence/diff_cert.rs
//
// Grounded Diff Certificate (Task 21 / Week 3, plan 2026-05-09).
//
// One-line pitch: a cryptographically-signed attestation that "this
// diff produced by an external agent rests on these specific
// substrate claims, and the byte ranges that aren't grounded are
// honestly named."
//
// Pipeline:
//
//   trace_log (every query_claims/search_claims/hybrid_retrieve call
//     during the agent's run) → set of (claim_id, byte_range) pairs
//   git diff (output of `git diff --no-color -U0 <parent>..HEAD`)
//     → Vec<FileDiff { path, hunks: Vec<Hunk { new_byte_range, … }>}>
//   matching: every diff hunk is matched against claim byte_ranges
//     by file-path + byte-range overlap. Matched → claim contributes;
//     unmatched → byte_range goes into `unmatched_byte_ranges`.
//   predicate: GroundedDiffPredicate carries claims_used,
//     unmatched_byte_ranges, agent name, pack hash.
//   sign: DSSE envelope, ed25519 local key. Reuses tr-sigstore's
//     DsseEnvelope / DsseSignature wire types so cosign-style
//     consumers parse the result identically.
//
// What v1.0 deliberately does NOT include (v1.1 work):
//
//   * Tree-sitter AST node matching. v1.0 is byte-range overlap; v3
//     claim byte_ranges already cover whole functions, so byte
//     overlap captures ≈90% of grounding correctly. Tree-sitter
//     refinement would split when one function holds N claims.
//
//   * Sigstore Fulcio + Rekor publishing. v1.0 is local key only.
//     v1.1 adds a "publish" knob that calls
//     tr_sigstore::live::sign_canonical_bytes_keyless when the user
//     opts in (workspace-level toggle in [security] config).
//
//   * Multi-hunk grouping. v1.0 emits one match record per hunk.
//     v1.1 may collapse adjacent matches against the same claim.

use base64::engine::Engine as _;
use blake3;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Reuse the wire types from tr-sigstore so cosign / external
/// verifiers see the same envelope shape they expect for v3 packs.
pub use tr_sigstore::{DsseEnvelope, DsseSignature};

/// Predicate type the in-toto statement carries. Stable URL — change
/// only on a wire-format break.
pub const GROUNDED_DIFF_PREDICATE_TYPE: &str = "https://thinkingroot.dev/grounded-diff/v1";

/// DSSE payload type for the Grounded Diff Certificate envelope.
/// Mirrors tr-sigstore's `DSSE_PAYLOAD_TYPE` but specialised so a
/// downstream classifier can route on the payload type alone.
pub const GROUNDED_DIFF_PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";

// ─────────────────────────────────────────────────────────────────
// Inputs
// ─────────────────────────────────────────────────────────────────

/// One byte range in a source file. Inclusive `start`, exclusive
/// `end`, in the file's BLAKE3-hashed bytes (post-edit for the
/// hunk's "new" half; pre-edit for the diff's "old" half).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

impl ByteRange {
    pub fn overlaps(&self, other: &ByteRange) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// One claim the agent retrieved during its session run. Built from
/// the `intelligence/retrieval_capture.rs` collector's output — the
/// trace_log of every search_claims / hybrid_retrieve call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimGrounding {
    /// Claim id from substrate.
    pub claim_id: String,
    /// File the claim is anchored in. Workspace-relative POSIX path.
    pub file: String,
    /// Byte range of the claim's source-of-truth span.
    pub byte_range: ByteRange,
}

/// One diff hunk the external agent produced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffHunk {
    /// File the hunk modifies. Workspace-relative POSIX path.
    pub file: String,
    /// Byte range in the **new** (post-edit) file the hunk wrote.
    pub new_byte_range: ByteRange,
}

// ─────────────────────────────────────────────────────────────────
// Outputs (the predicate body of the in-toto statement)
// ─────────────────────────────────────────────────────────────────

/// One claim grounding decision. Each entry says "this diff hunk
/// rested on this claim". Surfaces in the verifier UI as a row of
/// "edit X grounded by claim Y".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MatchedHunk {
    pub file: String,
    pub hunk_byte_range: ByteRange,
    pub claim_id: String,
}

/// The predicate body. Serialised as the in-toto statement's
/// `predicate` field; signed inside the DSSE envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroundedDiffPredicate {
    /// Adapter name from `AgentAdapter::name()` — `claude_code` or
    /// `cursor`.
    pub agent: String,
    /// BLAKE3 hex digest of the `.tr` pack the agent's MCP client
    /// was scoped to. Empty string means "no pack scope" (v1.0
    /// allows this with a warning).
    pub pack_hash: String,
    /// Distinct claim_ids this diff is grounded by. Sorted for
    /// stability. Empty when no claims matched.
    pub claims_used: Vec<String>,
    /// Per-hunk grounding decisions. Each entry attests one hunk
    /// rests on one claim. Multiple entries may share a claim_id if
    /// the agent edited multiple regions covered by the same claim.
    pub matched: Vec<MatchedHunk>,
    /// Byte ranges in the diff that no claim covered. Honest report
    /// of "ungrounded edits" — what tree-sitter language coverage
    /// will eventually shrink, but in v1.0 a non-empty list is the
    /// right user-facing signal that not everything is verified.
    pub unmatched_byte_ranges: Vec<DiffHunk>,
    /// RFC-3339 timestamp the certificate was minted.
    pub signed_at: String,
}

/// In-toto v1 statement: subject + predicate. Subject is the diff
/// itself — its BLAKE3 digest in `subject[0].digest.blake3`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroundedDiffStatement {
    #[serde(rename = "_type")]
    pub statement_type: String,
    pub subject: Vec<DiffSubject>,
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    pub predicate: GroundedDiffPredicate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffSubject {
    /// Stable name for the diff — `git-diff` is the recommended
    /// value when the diff is a unified-diff text.
    pub name: String,
    /// `{ "blake3": "<hex>" }`. Stable digest of the diff bytes.
    pub digest: serde_json::Map<String, serde_json::Value>,
}

// ─────────────────────────────────────────────────────────────────
// Building + matching
// ─────────────────────────────────────────────────────────────────

/// Build the predicate body (un-signed) from a trace_log + a list of
/// diff hunks. Pure function; deterministic given the same inputs.
///
/// `now` is taken as a parameter so callers can pin timestamps in
/// tests; production passes `chrono::Utc::now().to_rfc3339()`.
pub fn build_predicate(
    agent: &str,
    pack_hash: &str,
    claims: &[ClaimGrounding],
    hunks: &[DiffHunk],
    now: &str,
) -> GroundedDiffPredicate {
    let mut matched: Vec<MatchedHunk> = Vec::new();
    let mut unmatched: Vec<DiffHunk> = Vec::new();
    let mut claim_set: BTreeSet<String> = BTreeSet::new();

    for h in hunks {
        let mut found: Option<&ClaimGrounding> = None;
        for c in claims {
            if c.file == h.file && c.byte_range.overlaps(&h.new_byte_range) {
                // First match wins. The trace_log is best-effort
                // ordered by retrieval; first-match is good enough
                // and stable.
                found = Some(c);
                break;
            }
        }
        match found {
            Some(c) => {
                claim_set.insert(c.claim_id.clone());
                matched.push(MatchedHunk {
                    file: h.file.clone(),
                    hunk_byte_range: h.new_byte_range.clone(),
                    claim_id: c.claim_id.clone(),
                });
            }
            None => unmatched.push(h.clone()),
        }
    }

    GroundedDiffPredicate {
        agent: agent.to_string(),
        pack_hash: pack_hash.to_string(),
        claims_used: claim_set.into_iter().collect(),
        matched,
        unmatched_byte_ranges: unmatched,
        signed_at: now.to_string(),
    }
}

/// Wrap the predicate in an in-toto v1 statement targeting the
/// supplied diff bytes (BLAKE3-digested as the subject).
pub fn build_statement(
    diff_bytes: &[u8],
    predicate: GroundedDiffPredicate,
) -> GroundedDiffStatement {
    let digest = blake3::hash(diff_bytes);
    let mut digest_map = serde_json::Map::new();
    digest_map.insert(
        "blake3".to_string(),
        serde_json::Value::String(digest.to_hex().to_string()),
    );
    GroundedDiffStatement {
        statement_type: "https://in-toto.io/Statement/v1".to_string(),
        subject: vec![DiffSubject {
            name: "git-diff".to_string(),
            digest: digest_map,
        }],
        predicate_type: GROUNDED_DIFF_PREDICATE_TYPE.to_string(),
        predicate,
    }
}

// ─────────────────────────────────────────────────────────────────
// Sign + verify
// ─────────────────────────────────────────────────────────────────

/// DSSE Pre-Authentication Encoding. Per the DSSE spec
/// (https://github.com/secure-systems-lab/dsse): the bytes the
/// signer signs are `"DSSEv1 " + len(type) + " " + type + " " +
/// len(payload) + " " + payload` — prevents type confusion across
/// payload types. Local helper because tr-sigstore doesn't export
/// dsse_pae publicly.
fn dsse_pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"DSSEv1 ");
    out.extend_from_slice(payload_type.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload_type.as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload);
    out
}

/// Sign a Grounded Diff statement with a local ed25519 key. Returns
/// the DSSE envelope. v1.0 publication target is local disk; v1.1
/// can publish the same envelope to Rekor by recomputing the
/// inclusion proof + bundle.
pub fn sign_statement(
    statement: &GroundedDiffStatement,
    key: &SigningKey,
) -> Result<DsseEnvelope, DiffCertError> {
    let payload_bytes =
        serde_json::to_vec(statement).map_err(DiffCertError::Serialize)?;
    let pae = dsse_pae(GROUNDED_DIFF_PAYLOAD_TYPE, &payload_bytes);
    let signature = key.sign(&pae);
    let payload_b64 = base64::engine::general_purpose::STANDARD.encode(&payload_bytes);
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
    Ok(DsseEnvelope {
        payload: payload_b64,
        payload_type: GROUNDED_DIFF_PAYLOAD_TYPE.to_string(),
        signatures: vec![DsseSignature { sig: sig_b64 }],
    })
}

/// Verify a DSSE envelope against the public half of the local key
/// and decode the inner statement. Reverses `sign_statement`.
pub fn verify_envelope(
    envelope: &DsseEnvelope,
    public_key: &VerifyingKey,
) -> Result<GroundedDiffStatement, DiffCertError> {
    if envelope.payload_type != GROUNDED_DIFF_PAYLOAD_TYPE {
        return Err(DiffCertError::PayloadTypeMismatch {
            expected: GROUNDED_DIFF_PAYLOAD_TYPE.to_string(),
            actual: envelope.payload_type.clone(),
        });
    }
    let signature = envelope
        .signatures
        .first()
        .ok_or(DiffCertError::NoSignatures)?;
    let payload_bytes = base64::engine::general_purpose::STANDARD
        .decode(envelope.payload.as_bytes())
        .map_err(|e| DiffCertError::Decode(format!("payload base64: {e}")))?;
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature.sig.as_bytes())
        .map_err(|e| DiffCertError::Decode(format!("signature base64: {e}")))?;
    let sig: [u8; 64] = sig_bytes.try_into().map_err(|v: Vec<u8>| {
        DiffCertError::Decode(format!("expected 64-byte ed25519 sig, got {}", v.len()))
    })?;
    let signature = ed25519_dalek::Signature::from_bytes(&sig);
    let pae = dsse_pae(&envelope.payload_type, &payload_bytes);
    public_key
        .verify_strict(&pae, &signature)
        .map_err(|e| DiffCertError::SignatureInvalid(e.to_string()))?;
    let statement: GroundedDiffStatement =
        serde_json::from_slice(&payload_bytes).map_err(DiffCertError::Deserialize)?;
    Ok(statement)
}

#[derive(Debug, thiserror::Error)]
pub enum DiffCertError {
    #[error("serialise statement: {0}")]
    Serialize(serde_json::Error),
    #[error("deserialise statement: {0}")]
    Deserialize(serde_json::Error),
    #[error("envelope has no signatures")]
    NoSignatures,
    #[error("envelope payload type mismatch: expected {expected}, got {actual}")]
    PayloadTypeMismatch { expected: String, actual: String },
    #[error("decode failure: {0}")]
    Decode(String),
    #[error("signature did not verify: {0}")]
    SignatureInvalid(String),
    #[cfg(feature = "live")]
    #[error("sigstore keyless sign: {0}")]
    KeylessSign(String),
}

// ─────────────────────────────────────────────────────────────────
// Keyless (Sigstore Fulcio + Rekor) signing — gated on `live`
// ─────────────────────────────────────────────────────────────────
//
// The default `sign_statement` produces a self-signed Ed25519 DSSE
// envelope — fast, offline, and adequate when the verifier already
// trusts the signer's public key (typical for in-org diffs).
//
// `sign_statement_keyless` takes the same statement and produces a
// full Sigstore Bundle v0.3 with:
//
// - An ephemeral ECDSA-P256 keypair issued by Fulcio against the
//   caller-supplied OIDC JWT (CI ambient token, gh-action federated
//   identity, or browser-flow user identity).
// - A Rekor `intoto v0.0.2` transparency-log entry (so the diff cert
//   is publicly attestable and revocable via Rekor's tlog).
// - The Fulcio cert chain inside `verification_material` so any
//   downstream `cosign verify-blob` invocation can validate the
//   chain against Sigstore's trust root.
//
// **Operational notes:**
//
// - Building requires `--features live` on `thinkingroot-serve`.
//   Default builds skip this entire surface and the heavy sigstore-rs
//   dep tree.
// - Running requires network reachability of `fulcio.sigstore.dev`
//   + `rekor.sigstore.dev` (or whatever URLs the caller passes via
//   `SignKeylessOptions`) AND a valid OIDC `id_token` whose `email`
//   or `sub` claim matches what Fulcio expects.
// - Rekor publication is always-on for the keyless path. Callers
//   that want local-only signing without transparency-log exposure
//   stay on `sign_statement` (Ed25519, self-signed).

#[cfg(feature = "live")]
pub use tr_sigstore::SigstoreBundle;
#[cfg(feature = "live")]
pub use tr_sigstore::live::SignKeylessOptions;

/// Sign a [`GroundedDiffStatement`] via Sigstore Fulcio + Rekor and
/// return a [`SigstoreBundle`] containing the cert chain, the DSSE
/// envelope, and the Rekor transparency-log inclusion proof.
///
/// `jwt` is an OIDC `id_token` Fulcio will exchange for an ephemeral
/// signing cert. The `aud` claim must equal `"sigstore"`. For local
/// testing, run `sigstore-cli get-token` (or the moral equivalent).
/// For CI, source the federated GitHub-Actions OIDC token.
///
/// Network: this function blocks on Fulcio (one POST) and Rekor
/// (one POST). Production callers should cap with a `tokio::timeout`
/// — Fulcio + Rekor are normally <5s but cold-paths can be slower.
///
/// Wire format: identical to `tr-sigstore::live::sign_canonical_bytes_keyless`'s
/// output for a v3 pack — the same offline verifiers
/// (`tr_sigstore::verify_bundle_offline`,
/// `verify_bundle_against_canonical_bytes`,
/// `verify_bundle_with_trust_root`) accept the diff-cert bundle
/// modulo the `predicate_type` / payload-shape difference.
#[cfg(feature = "live")]
pub async fn sign_statement_keyless(
    statement: &GroundedDiffStatement,
    jwt: &str,
    options: SignKeylessOptions,
) -> Result<SigstoreBundle, DiffCertError> {
    let payload_bytes = serde_json::to_vec(statement).map_err(DiffCertError::Serialize)?;
    tr_sigstore::live::sign_dsse_payload_keyless(
        GROUNDED_DIFF_PAYLOAD_TYPE,
        &payload_bytes,
        jwt,
        options,
    )
    .await
    .map_err(|e| DiffCertError::KeylessSign(e.to_string()))
}

// ─────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_now() -> &'static str {
        "2026-05-09T12:00:00Z"
    }

    fn fixture_key_pair() -> (SigningKey, VerifyingKey) {
        // Deterministic seed so tests are stable. Real callers MUST
        // generate fresh keys via OsRng.
        let seed: [u8; 32] = [
            0x42, 0x41, 0x40, 0x3F, 0x3E, 0x3D, 0x3C, 0x3B, 0x3A, 0x39, 0x38, 0x37, 0x36, 0x35,
            0x34, 0x33, 0x32, 0x31, 0x30, 0x2F, 0x2E, 0x2D, 0x2C, 0x2B, 0x2A, 0x29, 0x28, 0x27,
            0x26, 0x25, 0x24, 0x23,
        ];
        let key = SigningKey::from_bytes(&seed);
        let vk = key.verifying_key();
        (key, vk)
    }

    #[test]
    fn byte_range_overlap_basic() {
        let a = ByteRange { start: 0, end: 10 };
        let b = ByteRange { start: 5, end: 15 };
        let c = ByteRange { start: 11, end: 20 };
        assert!(a.overlaps(&b));
        assert!(!a.overlaps(&c));
        // Touching ranges (end == start) don't overlap — exclusive end.
        let d = ByteRange { start: 10, end: 20 };
        assert!(!a.overlaps(&d));
    }

    #[test]
    fn build_predicate_with_no_inputs_emits_empty_predicate() {
        let p = build_predicate("claude_code", "abc", &[], &[], fixture_now());
        assert_eq!(p.agent, "claude_code");
        assert_eq!(p.pack_hash, "abc");
        assert!(p.claims_used.is_empty());
        assert!(p.matched.is_empty());
        assert!(p.unmatched_byte_ranges.is_empty());
    }

    #[test]
    fn build_predicate_matches_hunk_to_overlapping_claim() {
        let claims = vec![ClaimGrounding {
            claim_id: "c1".into(),
            file: "src/auth.rs".into(),
            byte_range: ByteRange { start: 100, end: 500 },
        }];
        let hunks = vec![DiffHunk {
            file: "src/auth.rs".into(),
            new_byte_range: ByteRange { start: 200, end: 250 },
        }];
        let p = build_predicate("claude_code", "h", &claims, &hunks, fixture_now());
        assert_eq!(p.claims_used, vec!["c1"]);
        assert_eq!(p.matched.len(), 1);
        assert_eq!(p.matched[0].claim_id, "c1");
        assert!(p.unmatched_byte_ranges.is_empty());
    }

    #[test]
    fn build_predicate_lists_unmatched_when_no_claim_covers_the_hunk() {
        let claims = vec![ClaimGrounding {
            claim_id: "c1".into(),
            file: "src/other.rs".into(),
            byte_range: ByteRange { start: 0, end: 50 },
        }];
        let hunks = vec![DiffHunk {
            file: "src/auth.rs".into(),
            new_byte_range: ByteRange { start: 200, end: 250 },
        }];
        let p = build_predicate("claude_code", "h", &claims, &hunks, fixture_now());
        assert!(p.claims_used.is_empty());
        assert!(p.matched.is_empty());
        assert_eq!(p.unmatched_byte_ranges.len(), 1);
        assert_eq!(p.unmatched_byte_ranges[0].file, "src/auth.rs");
    }

    #[test]
    fn build_predicate_dedups_claims_used_when_two_hunks_match_same_claim() {
        let claims = vec![ClaimGrounding {
            claim_id: "c1".into(),
            file: "f.rs".into(),
            byte_range: ByteRange { start: 0, end: 1000 },
        }];
        let hunks = vec![
            DiffHunk {
                file: "f.rs".into(),
                new_byte_range: ByteRange { start: 10, end: 20 },
            },
            DiffHunk {
                file: "f.rs".into(),
                new_byte_range: ByteRange { start: 100, end: 200 },
            },
        ];
        let p = build_predicate("claude_code", "h", &claims, &hunks, fixture_now());
        assert_eq!(p.claims_used.len(), 1);
        assert_eq!(p.matched.len(), 2);
        assert!(p.matched.iter().all(|m| m.claim_id == "c1"));
    }

    #[test]
    fn build_predicate_handles_mixed_matched_and_unmatched() {
        let claims = vec![ClaimGrounding {
            claim_id: "c1".into(),
            file: "a.rs".into(),
            byte_range: ByteRange { start: 0, end: 100 },
        }];
        let hunks = vec![
            DiffHunk {
                file: "a.rs".into(),
                new_byte_range: ByteRange { start: 10, end: 20 },
            },
            DiffHunk {
                file: "b.rs".into(),
                new_byte_range: ByteRange { start: 0, end: 5 },
            },
        ];
        let p = build_predicate("c", "h", &claims, &hunks, fixture_now());
        assert_eq!(p.matched.len(), 1);
        assert_eq!(p.unmatched_byte_ranges.len(), 1);
        assert_eq!(p.unmatched_byte_ranges[0].file, "b.rs");
    }

    #[test]
    fn build_predicate_first_match_wins_when_two_claims_overlap() {
        let claims = vec![
            ClaimGrounding {
                claim_id: "first".into(),
                file: "f.rs".into(),
                byte_range: ByteRange { start: 0, end: 200 },
            },
            ClaimGrounding {
                claim_id: "second".into(),
                file: "f.rs".into(),
                byte_range: ByteRange { start: 50, end: 100 },
            },
        ];
        let hunks = vec![DiffHunk {
            file: "f.rs".into(),
            new_byte_range: ByteRange { start: 60, end: 80 },
        }];
        let p = build_predicate("c", "h", &claims, &hunks, fixture_now());
        assert_eq!(p.claims_used, vec!["first"]);
    }

    #[test]
    fn build_statement_includes_blake3_subject_digest() {
        let p = build_predicate("c", "h", &[], &[], fixture_now());
        let diff = b"diff --git ...";
        let s = build_statement(diff, p);
        assert_eq!(s.statement_type, "https://in-toto.io/Statement/v1");
        assert_eq!(s.predicate_type, GROUNDED_DIFF_PREDICATE_TYPE);
        assert_eq!(s.subject.len(), 1);
        assert_eq!(s.subject[0].name, "git-diff");
        let blake3_hex = s.subject[0]
            .digest
            .get("blake3")
            .and_then(|v| v.as_str())
            .unwrap();
        // BLAKE3 hex digest is 64 chars.
        assert_eq!(blake3_hex.len(), 64);
    }

    #[test]
    fn dsse_pae_canonical_form_is_stable() {
        let pae = dsse_pae("application/vnd.in-toto+json", b"{}");
        let s = std::str::from_utf8(&pae).unwrap();
        assert!(s.starts_with("DSSEv1 "));
        // type-len + " " + type + " " + payload-len + " " + payload
        assert!(s.contains(" application/vnd.in-toto+json "));
        assert!(s.ends_with("{}"));
    }

    #[test]
    fn sign_statement_round_trips_through_verify() {
        let (key, vk) = fixture_key_pair();
        let claims = vec![ClaimGrounding {
            claim_id: "c1".into(),
            file: "a.rs".into(),
            byte_range: ByteRange { start: 0, end: 100 },
        }];
        let hunks = vec![DiffHunk {
            file: "a.rs".into(),
            new_byte_range: ByteRange { start: 10, end: 20 },
        }];
        let p = build_predicate("claude_code", "h", &claims, &hunks, fixture_now());
        let stmt = build_statement(b"diff bytes", p);
        let envelope = sign_statement(&stmt, &key).unwrap();

        let recovered = verify_envelope(&envelope, &vk).unwrap();
        assert_eq!(recovered.predicate.claims_used, vec!["c1"]);
        assert_eq!(recovered.predicate.agent, "claude_code");
    }

    #[test]
    fn verify_envelope_rejects_wrong_payload_type() {
        let (key, vk) = fixture_key_pair();
        let p = build_predicate("c", "h", &[], &[], fixture_now());
        let stmt = build_statement(b"x", p);
        let mut envelope = sign_statement(&stmt, &key).unwrap();
        envelope.payload_type = "application/wrong".to_string();
        let err = verify_envelope(&envelope, &vk).unwrap_err();
        assert!(matches!(err, DiffCertError::PayloadTypeMismatch { .. }));
    }

    #[test]
    fn verify_envelope_rejects_tampered_payload() {
        let (key, vk) = fixture_key_pair();
        let p = build_predicate("c", "h", &[], &[], fixture_now());
        let stmt = build_statement(b"x", p);
        let mut envelope = sign_statement(&stmt, &key).unwrap();
        // Flip a bit in the base64 payload by overwriting it with a
        // different-but-valid statement.
        let p2 = build_predicate("evil", "h", &[], &[], fixture_now());
        let stmt2 = build_statement(b"x", p2);
        let payload2 = serde_json::to_vec(&stmt2).unwrap();
        envelope.payload =
            base64::engine::general_purpose::STANDARD.encode(&payload2);
        let err = verify_envelope(&envelope, &vk).unwrap_err();
        assert!(matches!(err, DiffCertError::SignatureInvalid(_)));
    }

    #[test]
    fn verify_envelope_rejects_envelope_without_signatures() {
        let (_, vk) = fixture_key_pair();
        let envelope = DsseEnvelope {
            payload: "anything".to_string(),
            payload_type: GROUNDED_DIFF_PAYLOAD_TYPE.to_string(),
            signatures: Vec::new(),
        };
        assert!(matches!(
            verify_envelope(&envelope, &vk),
            Err(DiffCertError::NoSignatures)
        ));
    }

    #[test]
    fn verify_envelope_rejects_malformed_base64() {
        let (_, vk) = fixture_key_pair();
        let envelope = DsseEnvelope {
            payload: "!!!not-base64!!!".to_string(),
            payload_type: GROUNDED_DIFF_PAYLOAD_TYPE.to_string(),
            signatures: vec![DsseSignature {
                sig: "also-not-base64".to_string(),
            }],
        };
        let err = verify_envelope(&envelope, &vk).unwrap_err();
        assert!(matches!(err, DiffCertError::Decode(_)));
    }

    #[test]
    fn signed_statement_serialises_to_stable_json() {
        // Determinism check: same predicate → same DSSE payload.
        // The signature itself is deterministic for ed25519 (RFC
        // 8032), so the whole envelope byte-equals across runs.
        let (key, _) = fixture_key_pair();
        let p = build_predicate("c", "h", &[], &[], fixture_now());
        let stmt = build_statement(b"x", p);
        let env1 = sign_statement(&stmt, &key).unwrap();
        let env2 = sign_statement(&stmt, &key).unwrap();
        assert_eq!(env1.payload, env2.payload);
        assert_eq!(env1.signatures[0].sig, env2.signatures[0].sig);
    }

    // ─── Keyless signing tests (live feature) ──────────────────────
    //
    // These tests are gated on `feature = "live"` AND `#[ignore]`'d so
    // the regular test loop (which links the offline default features
    // only) doesn't try to compile sigstore-rs. To run them:
    //
    //   SIGSTORE_OIDC_TOKEN=$(get-real-token) \
    //     cargo test --package thinkingroot-serve \
    //     --features live -- --ignored real_keyless
    //
    // The `SIGSTORE_OIDC_TOKEN` value must be a JWT whose `aud` claim
    // is `"sigstore"`. CI runs typically source it from a federated
    // GitHub-Actions OIDC token; local runs can use
    // `sigstore-cli get-token` or equivalent.
    //
    // The function-signature compile check is the load-bearing
    // verification at default `cargo test` time — if the public
    // interface drifts, callers fail to build. The runtime behaviour
    // against real Sigstore-public-good infrastructure is what these
    // ignored tests cover when explicitly invoked.

    #[cfg(feature = "live")]
    #[tokio::test]
    #[ignore = "live; requires SIGSTORE_OIDC_TOKEN env var + network reachability of fulcio.sigstore.dev + rekor.sigstore.dev"]
    async fn real_keyless_sign_emits_bundle_with_rekor_entry() {
        let jwt = match std::env::var("SIGSTORE_OIDC_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live keyless test: SIGSTORE_OIDC_TOKEN env var unset or empty"
                );
                return;
            }
        };

        let p = build_predicate(
            "claude_code",
            "test-pack-hash",
            &[],
            &[],
            fixture_now(),
        );
        let stmt = build_statement(b"diff --git ...", p);

        let bundle = sign_statement_keyless(
            &stmt,
            &jwt,
            SignKeylessOptions::default(),
        )
        .await
        .expect("keyless sign");

        // Structural assertions — the bundle MUST carry a Rekor
        // entry, a Fulcio cert chain, and a DSSE envelope whose
        // payload type is the diff-cert predicate's.
        assert_eq!(bundle.dsse_envelope.payload_type, GROUNDED_DIFF_PAYLOAD_TYPE);
        assert_eq!(bundle.dsse_envelope.signatures.len(), 1);
        let chain = bundle
            .verification_material
            .x509_certificate_chain
            .as_ref()
            .expect("Fulcio cert chain present");
        assert!(
            !chain.certificates.is_empty(),
            "Fulcio chain MUST contain at least the leaf cert"
        );
        assert_eq!(
            bundle.verification_material.tlog_entries.len(),
            1,
            "Rekor entry MUST be present after live keyless sign"
        );
    }
}
