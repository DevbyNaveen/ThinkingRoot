//! `CloudError` — single error type for every cloud-touching code path.
//!
//! Spec: `docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md`
//! §8.4 Cross-cutting error catalog.

use std::io;

use thiserror::Error;

/// Single error type for every cloud-touching code path in the OSS
/// engine. UI code maps each variant to user-facing copy in one place
/// (`map_cloud_error` in `apps/thinkingroot-desktop/ui/src/lib/tauri.ts`
/// once the desktop side is wired up).
#[derive(Debug, Error)]
pub enum CloudError {
    #[error("not signed in — run `root login` first")]
    NotLoggedIn,

    #[error("session expired — token revoked or invalid; re-run `root login`")]
    AuthExpired,

    #[error("credits exhausted — needed {needed}, only {remaining} remaining")]
    CreditsExhausted { needed: u64, remaining: u64 },

    #[error("Pro tier required for feature `{feature}` — run `root upgrade` to switch tiers")]
    TierRequired { feature: String },

    #[error("rate limited — retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u32 },

    #[error("cloud unavailable — last upstream status {last_status}; local features unaffected")]
    CloudUnavailable { last_status: u16 },

    #[error(
        "auth.json schema version {found} is from a newer client \
         (this client supports up to v{max_supported}) — upgrade `root` to read it"
    )]
    IncompatibleSchema { found: u16, max_supported: u16 },

    #[error("login timed out after 60 seconds")]
    Timeout,

    #[error("login cancelled")]
    Cancelled,

    #[error("login callback failed CSRF check — state nonce mismatch")]
    StateMismatch,

    #[error("could not bind localhost callback listener: {0}")]
    BindFailed(#[source] io::Error),

    #[error("could not launch browser: {0}")]
    BrowserLaunch(String),

    #[error("another login is already in progress — run `root logout` to clear or wait")]
    AlreadyInFlight,

    #[error("hub rejected request: HTTP {status} — {body}")]
    HubReject { status: u16, body: String },

    #[error("I/O error while reading or writing auth.json: {0}")]
    Io(#[source] io::Error),

    #[error("auth.json JSON parse error: {0}")]
    JsonParse(#[source] serde_json::Error),

    #[error("HTTP transport error: {0}")]
    Http(#[source] reqwest::Error),
}

impl CloudError {
    /// Returns `true` if a retry is appropriate per the policy in
    /// `docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md` §8.5.
    pub fn is_retriable(&self) -> bool {
        matches!(
            self,
            CloudError::CloudUnavailable { .. } | CloudError::RateLimited { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_variants_render_user_facing_messages() {
        let cases: &[(CloudError, &str)] = &[
            (CloudError::NotLoggedIn, "not signed in"),
            (CloudError::AuthExpired, "session expired"),
            (CloudError::CreditsExhausted { needed: 100, remaining: 5 }, "credits exhausted"),
            (CloudError::TierRequired { feature: "long_context".into() }, "Pro tier required"),
            (CloudError::RateLimited { retry_after_secs: 30 }, "rate limited"),
            (CloudError::CloudUnavailable { last_status: 502 }, "cloud unavailable"),
            (CloudError::IncompatibleSchema { found: 99, max_supported: 2 }, "newer client"),
            (CloudError::Timeout, "timed out"),
            (CloudError::Cancelled, "cancelled"),
            (CloudError::StateMismatch, "CSRF"),
            (CloudError::AlreadyInFlight, "already in progress"),
        ];
        for (err, expected_substring) in cases {
            let rendered = err.to_string();
            assert!(
                rendered.to_lowercase().contains(&expected_substring.to_lowercase()),
                "{:?} → {rendered:?} missing {expected_substring:?}",
                err,
            );
        }
    }

    #[test]
    fn http_origin_errors_carry_status_and_body() {
        let err = CloudError::HubReject {
            status: 418,
            body: "I am a teapot".to_string(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("418"));
        assert!(rendered.contains("teapot"));
    }

    #[test]
    fn bind_failed_wraps_io_error() {
        let io = std::io::Error::new(std::io::ErrorKind::AddrInUse, "boom");
        let err = CloudError::BindFailed(io);
        assert!(err.to_string().to_lowercase().contains("bind"));
    }
}
