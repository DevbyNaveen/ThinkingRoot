//! DID method resolution.
//!
//! ThinkingRoot recognises two DID methods in v0.1:
//!
//! - `did:web:` — the standard web-based method ([`DidMethod::Web`]).
//!   The DID `did:web:alice.example` resolves to
//!   `https://alice.example/.well-known/did.json`.
//! - `did:tr:agent:owner/name` — an in-house method
//!   ([`DidMethod::Tr`]) used by federation peers and pack authors
//!   who do not own a domain. Resolution is delegated to the
//!   configured registry's `/.well-known/tr-did/{owner}/{name}`
//!   endpoint.
//!
//! This module ships the parser, the trait surface
//! ([`DidResolver`] + [`VcVerifier`]), and a default `did:web:`
//! resolver. The `did:tr:agent:` resolver lives behind the trait
//! so cloud callers can plug their own registry in.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::keypair::PublicKeyRef;

/// One of the two DID methods recognised in v0.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DidMethod {
    /// `did:web:domain[:path]` — fetched from `https://domain/.well-known/did.json`.
    Web,
    /// `did:tr:agent:owner/name` — resolved against the registry's
    /// `/.well-known/tr-did/<owner>/<name>` endpoint.
    Tr,
}

impl DidMethod {
    /// Method-string used in the DID URI (`web`, `tr`).
    pub fn scheme(&self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Tr => "tr",
        }
    }
}

/// A parsed DID URI. The wrapped string keeps the original
/// representation; [`Did::method`] lazily inspects it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Did(pub String);

impl Did {
    /// Parse a DID URI. Errors if the prefix is not `did:` or the
    /// method is not one of [`DidMethod`].
    pub fn parse(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        if !trimmed.starts_with("did:") {
            return Err(Error::InvalidDid(format!(
                "missing `did:` prefix in `{trimmed}`"
            )));
        }
        let rest = &trimmed["did:".len()..];
        let (method_str, _identifier) = rest
            .split_once(':')
            .ok_or_else(|| Error::InvalidDid(format!("missing identifier in `{trimmed}`")))?;
        match method_str {
            "web" | "tr" => Ok(Self(trimmed.to_string())),
            other => Err(Error::InvalidDid(format!(
                "unsupported method `{other}` in `{trimmed}`"
            ))),
        }
    }

    /// Determine which method this DID uses.
    pub fn method(&self) -> Result<DidMethod> {
        let rest = self
            .0
            .strip_prefix("did:")
            .ok_or_else(|| Error::InvalidDid(self.0.clone()))?;
        let (method_str, _) = rest
            .split_once(':')
            .ok_or_else(|| Error::InvalidDid(self.0.clone()))?;
        match method_str {
            "web" => Ok(DidMethod::Web),
            "tr" => Ok(DidMethod::Tr),
            other => Err(Error::InvalidDid(format!("unsupported method `{other}`"))),
        }
    }

    /// Borrow the raw URI.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Did {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Result of resolving a DID document. Phase F.1 only consumes the
/// public-key half of the document; richer service-endpoint fields
/// land in the `tr-c2pa` integration (Step 16).
#[derive(Debug, Clone)]
pub struct ResolvedDid {
    /// The DID that was resolved.
    pub did: Did,
    /// Public keys advertised by the DID document. Phase F.1 expects
    /// exactly one Ed25519 public key per DID; the field is plural
    /// so multi-key documents can land later without an API change.
    pub keys: Vec<PublicKeyRef>,
}

/// Trait for resolving a DID URI to a [`ResolvedDid`].
///
/// Implementations:
/// - [`DidWebResolver`] — fetches the well-known document over HTTPS.
/// - Cloud callers swap in their own resolver to consult the
///   registry's `/.well-known/tr-did/...` endpoint.
#[async_trait::async_trait]
pub trait DidResolver: Send + Sync {
    /// Resolve `did` to a public-key set.
    async fn resolve(&self, did: &Did) -> Result<ResolvedDid>;
}

/// Trait stub for verifiable-credential verification. Implementations
/// land in `tr-c2pa` (Step 16) where C2PA + Sigstore together carry
/// VC payloads attached to packs. We declare the trait here so
/// downstream callers can program against the contract today.
#[async_trait::async_trait]
pub trait VcVerifier: Send + Sync {
    /// Verify a VC payload signed by the named DID. Returns `Ok(())`
    /// if the credential is valid and not revoked.
    async fn verify(&self, did: &Did, payload: &[u8], signature: &[u8]) -> Result<()>;
}

/// Default `did:web:` resolver that fetches the well-known DID
/// document over HTTPS. Refuses non-https schemes — the resolver is
/// the load-bearing trust step, so it must not be downgrade-able.
pub struct DidWebResolver {
    client: reqwest::Client,
}

impl DidWebResolver {
    /// Construct a resolver with the default reqwest client.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Construct a resolver from an existing reqwest client (useful
    /// for tests against a mock server).
    pub fn from_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for DidWebResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl DidResolver for DidWebResolver {
    async fn resolve(&self, did: &Did) -> Result<ResolvedDid> {
        if did.method()? != DidMethod::Web {
            return Err(Error::InvalidDid(format!(
                "DidWebResolver only resolves did:web:, got {}",
                did
            )));
        }
        let url = did_web_url(did)?;
        tracing::debug!(%did, url = %url, "resolving did:web");
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| Error::DidWebFetch(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(Error::DidWebFetch(format!(
                "non-success status {}",
                resp.status()
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| Error::DidWebFetch(e.to_string()))?;
        let doc: WebDidDocument = serde_json::from_str(&body)?;
        let mut keys = Vec::new();
        for vm in doc.verification_method.unwrap_or_default() {
            if let Some(b64) = vm.public_key_base64 {
                keys.push(PublicKeyRef::from_base64(&b64)?);
            }
        }
        Ok(ResolvedDid {
            did: did.clone(),
            keys,
        })
    }
}

#[derive(Debug, Deserialize)]
struct WebDidDocument {
    #[serde(rename = "verificationMethod", default)]
    verification_method: Option<Vec<WebVerificationMethod>>,
}

#[derive(Debug, Deserialize)]
struct WebVerificationMethod {
    #[serde(rename = "publicKeyBase64", alias = "publicKeyMultibase", default)]
    public_key_base64: Option<String>,
}

fn did_web_url(did: &Did) -> Result<url::Url> {
    // `did:web:domain[:path...]` → `https://domain/path/.../did.json`
    let rest = did
        .0
        .strip_prefix("did:web:")
        .ok_or_else(|| Error::InvalidDid(did.0.clone()))?;
    let mut parts: Vec<&str> = rest.split(':').collect();
    let host = parts.remove(0);
    let mut url_str = format!("https://{host}");
    if parts.is_empty() {
        url_str.push_str("/.well-known/did.json");
    } else {
        for p in parts {
            url_str.push('/');
            url_str.push_str(p);
        }
        url_str.push_str("/did.json");
    }
    url::Url::parse(&url_str).map_err(|e| Error::InvalidDid(format!("{}: {e}", did.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_did_web() {
        let d = Did::parse("did:web:alice.example").unwrap();
        assert_eq!(d.method().unwrap(), DidMethod::Web);
    }

    #[test]
    fn parses_did_tr_agent() {
        let d = Did::parse("did:tr:agent:alice/researcher").unwrap();
        assert_eq!(d.method().unwrap(), DidMethod::Tr);
    }

    #[test]
    fn rejects_unknown_method() {
        let err = Did::parse("did:foo:bar").unwrap_err();
        assert!(matches!(err, Error::InvalidDid(_)));
    }

    #[test]
    fn rejects_missing_prefix() {
        let err = Did::parse("web:alice").unwrap_err();
        assert!(matches!(err, Error::InvalidDid(_)));
    }

    #[test]
    fn web_url_uses_well_known_when_no_path() {
        let d = Did::parse("did:web:alice.example").unwrap();
        let u = did_web_url(&d).unwrap();
        assert_eq!(u.as_str(), "https://alice.example/.well-known/did.json");
    }

    #[test]
    fn web_url_includes_path_segments() {
        let d = Did::parse("did:web:alice.example:keys:author").unwrap();
        let u = did_web_url(&d).unwrap();
        assert_eq!(u.as_str(), "https://alice.example/keys/author/did.json");
    }
}
