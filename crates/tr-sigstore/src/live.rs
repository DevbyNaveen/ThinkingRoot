//! Live keyless-signing flow against Sigstore-public-good.
//!
//! Gated behind the `live` feature. Pulls in `sigstore-rs` as the
//! transport layer for Fulcio cert requests and Rekor witness
//! submission, plus the OAuth + browser machinery for interactive
//! OIDC. Consumers that only verify bundles never compile this module.
//!
//! Three public entry points cover the common cases:
//!
//! - [`IdentityToken`] — re-exported from sigstore-rs. The Fulcio API
//!   accepts a JWT id_token wrapped in this type. CLI flows acquire
//!   the token via [`browser_oidc_flow`] (interactive); CI flows
//!   typically receive an ambient OIDC token from their environment
//!   (e.g. `ACTIONS_ID_TOKEN_REQUEST_URL` for GitHub Actions) and
//!   construct the [`IdentityToken`] with [`identity_token_from_jwt`].
//! - [`browser_oidc_flow`] — opens the user's default browser to the
//!   configured OIDC issuer (default: Sigstore-public-good), runs a
//!   PKCE-protected redirect listener on `127.0.0.1:8080`, and
//!   returns the resulting [`IdentityToken`].
//! - [`sign_canonical_bytes_keyless`] (commit-2 work) — drives the
//!   full keyless flow: ephemeral keypair → Fulcio cert → DSSE sign
//!   → Rekor witness → assembled bundle, returned as canonical JSON
//!   bytes ready to drop into a v3 pack as `signature.sig`.
//!
//! This commit lands the feature flag, the dep tree, and the
//! re-exports / token helpers. The actual sign function is the next
//! commit (it requires a corresponding verifier change to accept
//! sigstore-rs's SHA-256 subject digest, which is its own commit).

#![allow(missing_docs)] // re-exported sigstore types document themselves

use std::io::Cursor;

use sigstore::bundle::sign::SigningContext;
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

/// Sign v3 pack canonical bytes via the Sigstore-public-good keyless
/// flow. Drives the full chain: ephemeral ECDSA P-256 keypair →
/// Fulcio cert request (with the supplied OIDC token) → DSSE
/// signature → Rekor witness submission → assembled bundle. Returns
/// the canonical JSON bytes of the resulting Sigstore Bundle, ready
/// to drop into a v3 pack as the `signature.sig` outer-tar entry.
///
/// **Network access required.** This function makes live HTTPS calls
/// to `fulcio.sigstore.dev` (cert) and `rekor.sigstore.dev` (witness)
/// — both Sigstore-public-good instances. There is no offline /
/// mocked variant; consumers who want to test the verify side
/// without driving the live signing path should use one of the
/// synthetic-bundle test helpers in the tr-sigstore test module.
///
/// **Subject digest is SHA-256, not BLAKE3.** sigstore-rs's
/// high-level signer auto-builds an in-toto statement whose subject
/// digest is `sha256(canonical_bytes)`. Self-signed bundles via
/// `root pack --sign <key>` keep their `blake3:<hex>` subject; the
/// verifier dispatches on which key is present in the digest map.
/// Both hashes are derivable from the same canonical bytes, so no
/// extra storage cost — verifying just runs the appropriate
/// recompute.
///
/// The bundle's in-toto statement uses a content-addressed subject
/// name (sigstore-rs picks `sha256:<hex>`). Verifier identity policy
/// matches on the cert SAN (e.g. the OIDC subject email), not on the
/// bundle's subject name — so v3 doesn't need to override that field.
pub fn sign_canonical_bytes_keyless(
    token: IdentityToken,
    canonical_bytes: &[u8],
) -> Result<Vec<u8>, Error> {
    // sigstore-rs's default flow is async (tokio). The blocking_signer
    // variant runs the same flow but synchronously — sufficient for
    // the CLI's `root pack --sign-keyless` path.
    let ctx = SigningContext::production()
        .map_err(|e| Error::CertParse(format!("sigstore production context: {e}")))?;

    let session = ctx
        .blocking_signer(token)
        .map_err(|e| Error::CertParse(format!("sigstore blocking_signer: {e}")))?;

    let mut cursor = Cursor::new(canonical_bytes);
    let signing_artifact = session
        .sign(&mut cursor)
        .map_err(|e| Error::CertParse(format!("sigstore sign: {e}")))?;

    let bundle = signing_artifact.to_bundle();
    serde_json::to_vec(&bundle).map_err(Error::BundleParse)
}

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
