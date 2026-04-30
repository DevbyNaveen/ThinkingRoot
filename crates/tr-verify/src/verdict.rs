//! Verdict carriers shared by the v3 verification path.
//!
//! Today this module exposes a single type: [`RevokedDetails`].
//! Historically the file held a five-variant `Verdict` enum tied to
//! the v1 wire format — that surface was deleted alongside v1's
//! `Manifest`/`PackBuilder`/`reader::Pack`. The v3 trust path uses
//! [`crate::V3Verdict`] (in `v3.rs`) directly; `RevokedDetails` is
//! the one shared payload between v3's [`crate::V3Verdict::Revoked`]
//! variant and the CLI's `format_revoked` helper.

use serde::{Deserialize, Serialize};

/// Details accompanying a `V3Verdict::Revoked` result. Carries the
/// full advisory so consumers can render `pack`/`version`/`reason`/
/// `revoked_at`/`details_url` without re-fetching the snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokedDetails {
    /// The advisory describing why the pack was revoked.
    pub advisory: tr_revocation::Advisory,
}
