//! Rekor inclusion-proof + SignedEntryTimestamp (SET) verification.
//!
//! Rekor (Sigstore's transparency log) witnesses every keyless signing
//! event. A `TransparencyLogEntry` carries:
//!
//! - **Inclusion proof.** Merkle audit path + tree size + claimed root
//!   hash. Verifying this proves "the leaf I'm verifying is in the
//!   log at index N when the log had M leaves" without contacting Rekor.
//! - **SignedEntryTimestamp (SET).** Rekor's signature over the
//!   canonical (logIndex, logId, integratedTime, body-hash) tuple.
//!   Verifying this proves "Rekor witnessed this entry at this time
//!   under this log identifier" — the load-bearing identity binding.
//!
//! Both halves matter: a forged inclusion proof is detectable only if
//! the SET-signed root we can recompute matches what Rekor signed; a
//! valid SET without an inclusion proof says "Rekor saw this once" but
//! doesn't bind the entry to a specific position in the log. Verifying
//! both is what Sigstore-rs does, and what this module does.
//!
//! This commit's scope:
//! - Inclusion-proof Merkle math (delegates to `tr-transparency` for the
//!   RFC 6962 hash recurrence).
//! - SET signature verification with caller-supplied Rekor public key
//!   (vendoring Sigstore-public-good's Rekor key is a follow-up).
//! - Leaf-hash input is supplied by the caller — recomputing it from
//!   `canonicalized_body` requires Sigstore's hashedrekord JCS
//!   canonicalization, which is its own commit.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::{Error, RekorInclusionProof, TlogEntry};

/// Rekor's RFC 6962 leaf prefix — every leaf hash is
/// `SHA-256(0x00 || canonical_body)`. Exposed as a constant so callers
/// recomputing the leaf hash from a Rekor body do it the same way the
/// log does.
pub const RFC6962_LEAF_PREFIX: u8 = 0x00;

/// Re-derive a Rekor entry leaf hash from the canonical body bytes.
/// Equivalent to `SHA-256(0x00 || body)`.
pub fn leaf_hash_from_canonical_body(canonical_body: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([RFC6962_LEAF_PREFIX]);
    h.update(canonical_body);
    h.finalize().into()
}

/// Verify a Rekor inclusion proof against the bundle's claimed root
/// hash. Walks the audit path leaf-up, deriving the per-level
/// direction (left vs right child) from the entry's `log_index` and
/// the proof's `tree_size`.
///
/// Returns Ok if and only if the recomputed root matches the claimed
/// `inclusion_proof.root_hash`. This says nothing about whether
/// Rekor actually signed that root; pair with [`verify_set_signature`]
/// to bind the proof to Rekor's identity.
pub fn verify_inclusion_proof_offline(
    leaf_hash: &[u8; 32],
    inclusion_proof: &RekorInclusionProof,
) -> Result<(), Error> {
    let b64 = base64::engine::general_purpose::STANDARD;

    if inclusion_proof.tree_size <= 0 {
        return Err(Error::CertParse(format!(
            "rekor inclusion_proof.tree_size must be positive (was {})",
            inclusion_proof.tree_size
        )));
    }
    if inclusion_proof.log_index < 0 || inclusion_proof.log_index >= inclusion_proof.tree_size {
        return Err(Error::CertParse(format!(
            "rekor inclusion_proof.log_index {} out of range for tree_size {}",
            inclusion_proof.log_index, inclusion_proof.tree_size
        )));
    }

    // Decode the claimed root hash.
    let root_bytes = b64
        .decode(&inclusion_proof.root_hash)
        .map_err(Error::Base64)?;
    if root_bytes.len() != 32 {
        return Err(Error::CertParse(format!(
            "rekor root_hash must be 32 bytes (was {})",
            root_bytes.len()
        )));
    }
    let mut expected_root = [0u8; 32];
    expected_root.copy_from_slice(&root_bytes);

    // Decode the audit-path siblings.
    let mut siblings: Vec<[u8; 32]> = Vec::with_capacity(inclusion_proof.hashes.len());
    for (i, h_b64) in inclusion_proof.hashes.iter().enumerate() {
        let raw = b64.decode(h_b64).map_err(Error::Base64)?;
        if raw.len() != 32 {
            return Err(Error::CertParse(format!(
                "rekor audit path hash[{i}] must be 32 bytes (was {})",
                raw.len()
            )));
        }
        let mut s = [0u8; 32];
        s.copy_from_slice(&raw);
        siblings.push(s);
    }

    // Walk the path. RFC 6962 §2.1.1: at each level, the sibling is
    // the other half of the leaf-pair this node belongs to. The
    // direction (left/right of running hash) is determined by the
    // parity of the current index.
    let mut idx = inclusion_proof.log_index as u64;
    let mut size = inclusion_proof.tree_size as u64;
    let mut running = *leaf_hash;
    let mut sibling_iter = siblings.iter();

    while size > 1 {
        let last_node_at_level = size - 1;
        if idx == last_node_at_level && idx.is_multiple_of(2) {
            // Lone right-most leaf at this level: it's promoted to
            // the next level without hashing (RFC 6962). No sibling
            // consumed.
        } else {
            let sibling = sibling_iter.next().ok_or_else(|| {
                Error::CertParse("rekor audit path is too short for the given tree shape".into())
            })?;
            running = if idx.is_multiple_of(2) {
                hash_node(&running, sibling)
            } else {
                hash_node(sibling, &running)
            };
        }
        idx /= 2;
        size = size.div_ceil(2);
    }

    if sibling_iter.next().is_some() {
        return Err(Error::CertParse(
            "rekor audit path has more siblings than the tree shape requires".into(),
        ));
    }

    if running != expected_root {
        return Err(Error::SignatureMismatch);
    }

    Ok(())
}

/// Hash an interior node per RFC 6962: `SHA-256(0x01 || left || right)`.
#[inline]
fn hash_node(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([0x01]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// Verify Rekor's SignedEntryTimestamp (SET) signature over the
/// canonical `(integratedTime, logIndex, logID, body-hash)` tuple.
///
/// The signed bytes are computed exactly the way Rekor produces them:
/// JSON serialization with sorted keys, no whitespace, of an object
/// with keys `body` (base64 of canonical body), `integratedTime`,
/// `logID` (hex of the Rekor log's `key_id` bytes — the log_id field
/// in the bundle is base64 of the same bytes), and `logIndex`.
///
/// `canonical_body` is the entry's `canonicalized_body` field,
/// base64-decoded. `log_id_bytes` is the Rekor log's identifier (the
/// SHA-256 of Rekor's public key), base64-decoded from the bundle's
/// `log_id.key_id`.
pub fn verify_set_signature(
    entry: &TlogEntry,
    canonical_body: &[u8],
    log_id_bytes: &[u8],
    rekor_pubkey: &p256::ecdsa::VerifyingKey,
) -> Result<(), Error> {
    use signature::Verifier as _;

    let promise = entry
        .inclusion_promise
        .as_ref()
        .ok_or_else(|| Error::CertParse("rekor entry has no inclusionPromise".into()))?;

    let b64 = base64::engine::general_purpose::STANDARD;
    let set_bytes = b64
        .decode(&promise.signed_entry_timestamp)
        .map_err(Error::Base64)?;

    // Rekor signs DER-encoded ECDSA. Tolerate raw 64-byte form too.
    let signature = match p256::ecdsa::Signature::from_der(&set_bytes) {
        Ok(s) => s,
        Err(_) if set_bytes.len() == 64 => p256::ecdsa::Signature::from_slice(&set_bytes)
            .map_err(|_| Error::EcdsaSignatureFormat)?,
        Err(_) => return Err(Error::EcdsaSignatureFormat),
    };

    let payload = canonical_set_payload(
        entry.integrated_time,
        entry.log_index,
        log_id_bytes,
        canonical_body,
    );

    rekor_pubkey
        .verify(&payload, &signature)
        .map_err(|_| Error::SignatureMismatch)
}

/// Build the canonical bytes Rekor signs for the SET. Format matches
/// Rekor v1's implementation:
///
/// ```text
/// {"body":"<b64>","integratedTime":<int>,"logID":"<hex>","logIndex":<int>}
/// ```
///
/// Keys sorted alphabetically; no whitespace. `logID` is hex-encoded
/// while `body` is base64 — that's how Rekor's own canonicalizer does
/// it (see <https://github.com/sigstore/rekor> `pkg/types/rekord_v001`).
fn canonical_set_payload(
    integrated_time: i64,
    log_index: i64,
    log_id_bytes: &[u8],
    canonical_body: &[u8],
) -> Vec<u8> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let body_b64 = b64.encode(canonical_body);
    let log_id_hex = hex_lower(log_id_bytes);
    format!(
        "{{\"body\":\"{body_b64}\",\"integratedTime\":{integrated_time},\"logID\":\"{log_id_hex}\",\"logIndex\":{log_index}}}"
    )
    .into_bytes()
}

/// Lowercase hex encoding without dependencies. Used for the `logID`
/// field in the SET canonical payload.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Convert `entry.integrated_time` (Unix seconds) to a `SystemTime` —
/// useful when callers want to pass it to `verify_cert_chain` for
/// validity-window checks at the witness time, not at "now".
pub fn integrated_time_to_system(entry: &TlogEntry) -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_secs(entry.integrated_time.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InclusionPromise, RekorInclusionProof, TlogEntry};

    /// Build a leaf hash for testing. Uses the RFC 6962 leaf prefix on
    /// arbitrary deterministic body bytes.
    fn fake_leaf(seed: u8) -> [u8; 32] {
        let body = vec![seed; 16];
        leaf_hash_from_canonical_body(&body)
    }

    fn build_complete_tree_root(leaves: &[[u8; 32]]) -> [u8; 32] {
        // RFC 6962-shaped tree: lone right-most node promoted without
        // hashing. Same shape `tr-transparency::root_hash` produces.
        assert!(!leaves.is_empty());
        let mut layer: Vec<[u8; 32]> = leaves.to_vec();
        while layer.len() > 1 {
            let mut next: Vec<[u8; 32]> = Vec::with_capacity(layer.len().div_ceil(2));
            for chunk in layer.chunks(2) {
                match chunk {
                    [l, r] => next.push(hash_node(l, r)),
                    [only] => next.push(*only),
                    _ => unreachable!(),
                }
            }
            layer = next;
        }
        layer[0]
    }

    /// Build a Sigstore-style audit path for `leaf_index`. Re-uses the
    /// shape `tr-transparency::build_proof` produces — siblings only,
    /// directions derived implicitly from index parity at verify time.
    fn build_audit_path(leaves: &[[u8; 32]], leaf_index: usize) -> Vec<[u8; 32]> {
        let mut siblings: Vec<[u8; 32]> = Vec::new();
        let mut idx = leaf_index as u64;
        let mut layer: Vec<[u8; 32]> = leaves.to_vec();
        while layer.len() > 1 {
            let last_node = (layer.len() as u64) - 1;
            if !(idx == last_node && idx.is_multiple_of(2)) {
                let pair = (idx ^ 1) as usize;
                siblings.push(layer[pair]);
            }
            let mut next: Vec<[u8; 32]> = Vec::with_capacity(layer.len().div_ceil(2));
            for chunk in layer.chunks(2) {
                match chunk {
                    [l, r] => next.push(hash_node(l, r)),
                    [only] => next.push(*only),
                    _ => unreachable!(),
                }
            }
            layer = next;
            idx /= 2;
        }
        siblings
    }

    fn b64_encode(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn rfc6962_leaf_prefix_matches_spec() {
        // Spec example: leaf_hash(empty body) = SHA-256(0x00).
        let mut h = Sha256::new();
        h.update([0x00]);
        let expected: [u8; 32] = h.finalize().into();
        assert_eq!(leaf_hash_from_canonical_body(&[]), expected);
    }

    #[test]
    fn inclusion_proof_for_complete_4_leaf_tree_validates_each_leaf() {
        let leaves: Vec<[u8; 32]> = (0u8..4).map(fake_leaf).collect();
        let root = build_complete_tree_root(&leaves);

        for (i, leaf) in leaves.iter().enumerate() {
            let path = build_audit_path(&leaves, i);
            let proof = RekorInclusionProof {
                log_index: i as i64,
                tree_size: leaves.len() as i64,
                root_hash: b64_encode(&root),
                hashes: path.iter().map(|h| b64_encode(h)).collect(),
                checkpoint: None,
            };
            verify_inclusion_proof_offline(leaf, &proof)
                .unwrap_or_else(|e| panic!("leaf {i} failed: {e:?}"));
        }
    }

    #[test]
    fn inclusion_proof_for_unbalanced_5_leaf_tree() {
        // 5 leaves forces the lone-right-promotion case at level 0
        // (leaf 4) and at level 1 (the promoted leaf).
        let leaves: Vec<[u8; 32]> = (10u8..15).map(fake_leaf).collect();
        let root = build_complete_tree_root(&leaves);

        for (i, leaf) in leaves.iter().enumerate() {
            let path = build_audit_path(&leaves, i);
            let proof = RekorInclusionProof {
                log_index: i as i64,
                tree_size: leaves.len() as i64,
                root_hash: b64_encode(&root),
                hashes: path.iter().map(|h| b64_encode(h)).collect(),
                checkpoint: None,
            };
            verify_inclusion_proof_offline(leaf, &proof)
                .unwrap_or_else(|e| panic!("leaf {i} of 5 failed: {e:?}"));
        }
    }

    #[test]
    fn inclusion_proof_with_wrong_root_fails() {
        let leaves: Vec<[u8; 32]> = (20u8..24).map(fake_leaf).collect();
        let path = build_audit_path(&leaves, 1);
        // Use a fake root.
        let fake_root = fake_leaf(99);
        let proof = RekorInclusionProof {
            log_index: 1,
            tree_size: leaves.len() as i64,
            root_hash: b64_encode(&fake_root),
            hashes: path.iter().map(|h| b64_encode(h)).collect(),
            checkpoint: None,
        };
        let err = verify_inclusion_proof_offline(&leaves[1], &proof).unwrap_err();
        assert!(matches!(err, Error::SignatureMismatch));
    }

    #[test]
    fn inclusion_proof_with_log_index_out_of_range_errors_cleanly() {
        let leaves: Vec<[u8; 32]> = (30u8..34).map(fake_leaf).collect();
        let root = build_complete_tree_root(&leaves);
        let proof = RekorInclusionProof {
            log_index: 99, // way past tree_size
            tree_size: leaves.len() as i64,
            root_hash: b64_encode(&root),
            hashes: Vec::new(),
            checkpoint: None,
        };
        let err = verify_inclusion_proof_offline(&leaves[0], &proof).unwrap_err();
        assert!(matches!(err, Error::CertParse(_)));
    }

    #[test]
    fn set_signature_round_trips() {
        // Synthetic Rekor key + entry; sign the canonical SET payload
        // ourselves; verify with the matching public key.
        let mut bytes = [0u8; 32];
        bytes[31] = 0x42;
        let rekor_signer = p256::ecdsa::SigningKey::from_slice(&bytes).unwrap();
        let rekor_vk = *rekor_signer.verifying_key();

        let canonical_body = b"{\"apiVersion\":\"0.0.1\",\"kind\":\"hashedrekord\"}";
        let log_id_bytes = [0x55u8; 32];

        let entry_skeleton = TlogEntry {
            log_index: 1234,
            log_id: Some(crate::LogId {
                key_id: b64_encode(&log_id_bytes),
            }),
            kind_version: None,
            integrated_time: 1_700_000_000,
            inclusion_promise: None,
            inclusion_proof: None,
            canonicalized_body: Some(b64_encode(canonical_body)),
        };

        let payload = canonical_set_payload(
            entry_skeleton.integrated_time,
            entry_skeleton.log_index,
            &log_id_bytes,
            canonical_body,
        );
        use signature::Signer as _;
        let sig: p256::ecdsa::Signature = rekor_signer.sign(&payload);
        let sig_der = sig.to_der();

        let entry = TlogEntry {
            inclusion_promise: Some(InclusionPromise {
                signed_entry_timestamp: b64_encode(sig_der.as_bytes()),
            }),
            ..entry_skeleton
        };

        verify_set_signature(&entry, canonical_body, &log_id_bytes, &rekor_vk).unwrap();
    }

    #[test]
    fn set_signature_with_wrong_key_fails() {
        let mut bytes = [0u8; 32];
        bytes[31] = 0x10;
        let rekor_signer = p256::ecdsa::SigningKey::from_slice(&bytes).unwrap();

        bytes[31] = 0x20;
        let other_key = p256::ecdsa::SigningKey::from_slice(&bytes).unwrap();
        let other_vk = *other_key.verifying_key();

        let canonical_body = b"some-rekor-body";
        let log_id_bytes = [0x77u8; 32];

        let payload = canonical_set_payload(42, 7, &log_id_bytes, canonical_body);
        use signature::Signer as _;
        let sig: p256::ecdsa::Signature = rekor_signer.sign(&payload);
        let sig_der = sig.to_der();

        let entry = TlogEntry {
            log_index: 7,
            log_id: Some(crate::LogId {
                key_id: b64_encode(&log_id_bytes),
            }),
            kind_version: None,
            integrated_time: 42,
            inclusion_promise: Some(InclusionPromise {
                signed_entry_timestamp: b64_encode(sig_der.as_bytes()),
            }),
            inclusion_proof: None,
            canonicalized_body: Some(b64_encode(canonical_body)),
        };

        let err =
            verify_set_signature(&entry, canonical_body, &log_id_bytes, &other_vk).unwrap_err();
        assert!(matches!(err, Error::SignatureMismatch));
    }

    #[test]
    fn set_signature_missing_inclusion_promise_errors_cleanly() {
        let entry = TlogEntry {
            log_index: 0,
            log_id: None,
            kind_version: None,
            integrated_time: 0,
            inclusion_promise: None,
            inclusion_proof: None,
            canonicalized_body: None,
        };
        // Any P-256 key works; the function bails before signature
        // verification.
        let mut bytes = [0u8; 32];
        bytes[31] = 0x01;
        let signing = p256::ecdsa::SigningKey::from_slice(&bytes).unwrap();
        let vk = *signing.verifying_key();

        let err = verify_set_signature(&entry, b"", b"", &vk).unwrap_err();
        assert!(matches!(err, Error::CertParse(_)));
    }
}
