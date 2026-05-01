//! Pinned hub revocation signing keys.
//!
//! Per `docs/2026-04-27-phase-f-trust-verify-design.md` §6 the OSS
//! client trusts a small set of compile-time pinned keys. The set holds
//! more than one entry during a 30-day rotation overlap window
//! (`revocation-protocol-spec.md` §7.1) so a snapshot signed by either
//! the old or new key continues to verify until the old key ages out
//! in a follow-up `root` release.
//!
//! ## Trust anchor: `thinkingroot-revocation-v1`
//!
//! Real Ed25519 public-key bytes are embedded below. The matching
//! private key lives in
//! `~/.config/thinkingroot-cloud/secrets/revocation_signing.key.b64`
//! on the cloud signing host (chmod 0600); it is **not** present in
//! this repository. The cloud-side
//! `services/registry/src/revocation/sign.rs` consumes that file at
//! request-handling time to sign each snapshot before serving it from
//! `/api/v1/revoked`.
//!
//! Rotation flow when the cloud team mints a successor key
//! (`thinkingroot-revocation-v2`): publish the new public bytes here
//! as a *second* entry in [`PINNED_RAW`] alongside `v1`, ship a `root`
//! release, wait the 30-day overlap window, then drop the `v1` row in
//! a follow-up release. Snapshots signed under either key verify
//! during the overlap.

/// Compile-time table of `(key_id, ed25519_public)` pairs the binary
/// trusts. Append-only during rotation overlaps; entries are removed
/// only after the prior key has aged out of all in-flight snapshots.
pub const PINNED_RAW: &[(&str, [u8; 32])] = &[
    // thinkingroot-revocation-v1 — generated 2026-05-01 via
    // ed25519-dalek 2.x with `OsRng`. Public bytes only; the matching
    // private key is held outside this repo (see module docs).
    (
        "thinkingroot-revocation-v1",
        [
            0x63, 0x16, 0x84, 0x65, 0xe4, 0xc3, 0xbc, 0x54, 0x96, 0x45, 0x96, 0xf6, 0x26, 0x17,
            0xd0, 0x61, 0x07, 0xac, 0x81, 0x7c, 0x1e, 0xe8, 0xb7, 0xcb, 0x29, 0xed, 0x0a, 0xe2,
            0xd9, 0x10, 0x4b, 0xd6,
        ],
    ),
];

/// Ed25519 public key the client pins to verify revocation snapshots.
///
/// Construct from [`PINNED_RAW`] via [`pinned_keys`] for production use,
/// or build directly in tests.
#[derive(Debug, Clone)]
pub struct PinnedKey {
    /// Identifier the snapshot's `signing_key_id` field is matched
    /// against.
    pub key_id: String,
    /// Raw 32-byte Ed25519 public key.
    pub ed25519_public: [u8; 32],
}

/// Materialise the compile-time pinned key set into a `Vec` callers
/// can pass into [`crate::CacheConfig::trusted_keys`].
pub fn pinned_keys() -> Vec<PinnedKey> {
    PINNED_RAW
        .iter()
        .map(|(id, bytes)| PinnedKey {
            key_id: (*id).to_string(),
            ed25519_public: *bytes,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_set_is_non_empty_and_well_formed() {
        // Production binaries must ship with at least one trust anchor;
        // an empty set silently disables revocation verification.
        let keys = pinned_keys();
        assert!(!keys.is_empty(), "PINNED_RAW must contain ≥1 trust anchor");
        for k in &keys {
            assert!(!k.key_id.is_empty(), "every pinned key needs a key_id");
            // Sanity: every byte zero is almost certainly a placeholder
            // that escaped review.
            assert!(
                k.ed25519_public.iter().any(|b| *b != 0),
                "pinned key {} has all-zero bytes",
                k.key_id
            );
        }
    }

    #[test]
    fn pinned_key_id_is_unique() {
        let keys = pinned_keys();
        let mut seen = std::collections::HashSet::new();
        for k in &keys {
            assert!(
                seen.insert(k.key_id.clone()),
                "duplicate key_id in PINNED_RAW: {}",
                k.key_id
            );
        }
    }
}
