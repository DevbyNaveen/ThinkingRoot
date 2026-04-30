//! Live OIDC primitives for the future keyless-signing flow against
//! Sigstore-public-good.
//!
//! Gated behind the `live` feature. Pulls in `sigstore-rs` for the
//! `IdentityToken` wrapper + the PKCE-redirect browser flow.
//! Consumers that only verify bundles never compile this module.
//!
//! Two public entry points are stable today:
//!
//! - [`identity_token_from_jwt`] — wrap a pre-fetched JWT (e.g. a CI
//!   ambient OIDC token from `ACTIONS_ID_TOKEN_REQUEST_URL`) into the
//!   sigstore-rs [`IdentityToken`] type used by Fulcio's API.
//! - [`browser_oidc_flow`] — interactive PKCE flow: opens the user's
//!   default browser to the OIDC issuer (default: Sigstore-public-
//!   good), runs a redirect listener on `127.0.0.1:8080`, and returns
//!   the resulting [`IdentityToken`].
//!
//! The actual end-to-end keyless signing function — Fulcio cert
//! request + DSSE-signed in-toto statement + Rekor witness submission
//! → a v3-compatible Sigstore Bundle JSON — is **not yet
//! implemented**. See the comment on the first commented-out block
//! below for the design constraint (sigstore-rs 0.13's high-level
//! signer emits `MessageSignature` bundles; v3 needs DSSE) and the
//! follow-up plan.

#![allow(missing_docs)] // re-exported sigstore types document themselves

use sigstore::oauth::IdentityToken as SigstoreIdentityToken;

use crate::Error;

// Re-exports — using these directly from `tr_sigstore::live` keeps
// callers from ever needing to depend on the `sigstore` crate
// themselves (which means callers also avoid the heavy transitive
// deps unless they enable our `live` feature).
pub use sigstore::bundle::Bundle as SigstoreSdkBundle;
pub use sigstore::oauth::IdentityToken;

/// Construct a [`IdentityToken`] from a raw JWT string (typically an
/// OIDC id_token obtained out-of-band — e.g. CI environment, an
/// ambient federated identity, a previously-cached token).
///
/// The JWT is parsed but *not* verified against an OIDC issuer's
/// keys: that verification happens at Fulcio's end of the keyless
/// flow. This function only structurally validates the JWT shape so
/// failures surface at the caller's boundary rather than deep inside
/// the Sigstore SDK.
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
///
/// Override any of these by passing non-`None` values; `None` falls
/// back to the public-good default.
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

    let (_claims, raw_token) = RedirectListener::new(
        listener_addr,
        oauth_client,
        nonce,
        pkce_verifier,
    )
    .redirect_listener()
    .map_err(|e| Error::CertParse(format!("OIDC redirect listener: {e}")))?;

    Ok(IdentityToken::from(raw_token))
}

// The actual keyless-signing function — Fulcio cert request +
// DSSE-signed in-toto statement + Rekor witness submission, returning
// a canonical Sigstore Bundle JSON ready for the v3 pack
// `signature.sig` slot — is **not** implemented here.
//
// Why: sigstore-rs 0.13's high-level signer
// (`bundle::sign::SigningContext::sign(reader)`) emits **MessageSignature**
// bundles (raw signature over a SHA-256 input digest), not DSSE
// attestation bundles. Phase F doc §3.3 mandates DSSE for v3 packs so
// the in-toto `predicateType` binds the bundle to the v3 wire format,
// and the in-toto subject digest pins the bundle to the canonical
// pack hash. A MessageSignature bundle has neither, and our
// `SigstoreBundle` struct (which has `dsse_envelope` as a required
// field) wouldn't even round-trip-deserialize one.
//
// The proper fix is to drive the flow at a lower level: use
// `sigstore::fulcio::FulcioClient::request_cert` to obtain the
// ephemeral cert + signer, build the DSSE statement ourselves with
// the v3 predicate type and subject digest, sign the DSSE PAE with
// the ephemeral signer, and submit the DSSE entry to Rekor via
// `sigstore::rekor`. That's its own commit; the OIDC token + browser
// flow primitives above are reusable when it lands.
//
// Tracking: `~/.claude/plans/zippy-wiggling-pelican.md` Task #55-B
// (DSSE Sigstore signing).

#[cfg(test)]
mod tests {
    use super::*;

    /// Building a minimal valid JWT (header.payload.signature, all
    /// base64url-no-padding) lets us round-trip
    /// [`identity_token_from_jwt`] without needing a live OIDC
    /// issuer. The signature is dummy bytes — sigstore-rs's parser
    /// validates JWT structure, not the cryptographic signature, at
    /// this layer (Fulcio does the latter).
    fn fake_jwt() -> String {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = b64.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        // Claims need at minimum `iss`, `sub`, `aud`, `exp`, `iat`
        // for sigstore-rs's IdentityToken parser to accept them.
        // Use a far-future `exp` so the token isn't considered
        // expired during the test.
        let payload = b64.encode(
            br#"{"iss":"https://accounts.google.com","sub":"naveen@thinkingroot.dev","aud":"sigstore","email":"naveen@thinkingroot.dev","exp":9999999999,"iat":1000000000,"nbf":1000000000}"#,
        );
        let sig = b64.encode(b"dummy-signature");
        format!("{header}.{payload}.{sig}")
    }

    /// `IdentityToken` doesn't implement `Debug`, so the standard
    /// `Result::unwrap_err()` that prints the Ok side on failure
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
        // Successful Ok variant proves the JWT was structurally valid
        // and survived sigstore-rs's parser. Rejecting unstructured
        // input is verified separately below; the exact claim
        // accessors on `IdentityToken` vary across sigstore-rs minor
        // versions, so we don't rely on them here.
        let _token = identity_token_from_jwt(&jwt).expect("parse JWT");
    }

    #[test]
    fn identity_token_from_jwt_rejects_garbage() {
        let err = expect_err(identity_token_from_jwt("not-a-jwt"));
        assert!(matches!(err, Error::CertParse(_)));
    }

    #[test]
    fn identity_token_from_jwt_rejects_two_segment_token() {
        // JWTs must have three dot-separated segments.
        let err = expect_err(identity_token_from_jwt("only.two-segments"));
        assert!(matches!(err, Error::CertParse(_)));
    }
}
