// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ThinkingRoot
//! Authentication module — fixture for the Compile Completeness Contract
//! canonical end-to-end test (Test 12.1).
//!
//! @see https://docs.thinkingroot.dev/contract

use std::sync::Arc;
use std::sync::Mutex;
use std::collections::HashMap;

/// Token bag. Field types include `Vec<Arc<Mutex<…>>>` so the
/// `code_signatures.field_types_json` round-trip is exercised.
pub struct AuthState {
    pub sessions: Vec<Arc<Mutex<Session>>>,
    pub revoked: HashMap<String, u64>,
}

pub struct Session {
    pub id: String,
    pub user: String,
}

/// Rotate a session token. p99=120 ms at 50000 rps under load.
///
/// @param session_id the session to rotate
/// @returns the new token
/// @throws InvalidSession when the session id is unknown
/// @deprecated valid_until 2026-12-31; use `rotate_v2` instead.
pub fn rotate_token(session_id: &str) -> Result<String, String> {
    // TODO: handle revoked-session case once Vault integration lands.
    // FIXME: race condition under concurrent rotation needs a CAS lock.
    if session_id.is_empty() {
        return Err("empty session id".to_string());
    }
    let new_token = format!("tok-{}", session_id);
    if new_token.contains(' ') {
        // SAFETY: spaces are forbidden by the wire format.
        return Err("invalid token shape".to_string());
    }
    audit_rotation(session_id);
    Ok(new_token)
}

/// Audit-log a token rotation event.
fn audit_rotation(session_id: &str) {
    let _ = session_id;
    // NOTE: audit sink is a stub for the fixture.
}

#[test]
fn rotate_token_rejects_empty_id() {
    assert!(rotate_token("").is_err());
}

#[test]
#[ignore]
fn rotate_token_handles_unicode_ids() {
    let _ = rotate_token("session-Ω");
}
