//! Pinned hub revocation signing keys.
//!
//! Per `docs/2026-04-27-phase-f-trust-verify-design.md` §6 the OSS
//! client trusts a small set of compile-time pinned keys. The set holds
//! more than one entry during a 30-day rotation overlap window
//! (`revocation-protocol-spec.md` §7.1) so a snapshot signed by either
//! the old or new key continues to verify until the old key ages out
//! in a follow-up `root` release.
//!
//! **v0.1 bootstrap:** the [`PINNED_RAW`] table is empty pending the
//! cloud team's coordinated key publication. Tests inject their own
//! [`PinnedKey`] via [`crate::CacheConfig::trusted_keys`] — the empty
//! default is intentional and forces an explicit configuration in any
//! production caller until the real key lands via a coordinated PR
//! pair.

/// Compile-time table of `(key_id, ed25519_public)` pairs the binary
/// trusts. Populated by a coordinated PR pair with the cloud team
/// before the v0.1 tag.
pub const PINNED_RAW: &[(&str, [u8; 32])] = &[
    // Intentionally empty until the cloud key is published. See
    // §6 of the Phase F design doc for the rotation flow.
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
