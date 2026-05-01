//! HTTP-based resolvers — direct URL and discovery-doc-driven
//! cloud registry.
//!
//! Both share the `pack_cmd::http_client` configuration and the
//! `refuse_insecure_http` policy (HTTPS-only for non-loopback hosts).
//! [`HttpRegistryResolver`] adds the discovery-doc → download-URL →
//! BLAKE3 cross-check flow against `{owner}/{slug}@{version}`.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tr_format::digest::blake3_hex;

use super::PackResolver;
use crate::pack_cmd::{http_client, refuse_insecure_http};

// -----------------------------------------------------------------------------
// HttpDirectUrlResolver
// -----------------------------------------------------------------------------

/// Fetch a `.tr` directly from an `https://` (or `http://localhost`)
/// URL. No registry resolution, no advertised-hash cross-check.
pub struct HttpDirectUrlResolver {
    url: String,
}

impl HttpDirectUrlResolver {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

#[async_trait]
impl PackResolver for HttpDirectUrlResolver {
    async fn resolve(&self) -> Result<Vec<u8>> {
        refuse_insecure_http(&self.url)?;
        let client = http_client()?;
        let resp = client
            .get(&self.url)
            .send()
            .await
            .with_context(|| format!("GET {}", self.url))?;
        let resp = resp
            .error_for_status()
            .with_context(|| format!("GET {}", self.url))?;
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("read body from {}", self.url))?;
        Ok(bytes.to_vec())
    }
}

// -----------------------------------------------------------------------------
// HttpRegistryResolver
// -----------------------------------------------------------------------------

/// Resolve `{owner}/{slug}@{version}` against a TR-1 cloud registry.
///
/// Sequence:
///
/// 1. `GET <registry>/.well-known/tr-registry.json` for the discovery
///    document; reject if `format_version` ≠ `tr-registry/1` or
///    `tr_format` ≠ this client's supported version.
/// 2. Substitute `{owner}/{slug}/{version}` into the advertised
///    `endpoints.download` URL template.
/// 3. `GET` the download URL; abort if `Content-Length` exceeds the
///    discovery-doc-advertised `max_pack_bytes`.
/// 4. BLAKE3 cross-check the body against the `x-tr-content-hash`
///    response header. Mismatch → hard error before any parsing.
pub struct HttpRegistryResolver {
    registry_url: String,
    owner: String,
    slug: String,
    version: String,
}

impl HttpRegistryResolver {
    pub fn new(
        registry_url: impl Into<String>,
        owner: impl Into<String>,
        slug: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            registry_url: registry_url.into(),
            owner: owner.into(),
            slug: slug.into(),
            version: version.into(),
        }
    }
}

#[async_trait]
impl PackResolver for HttpRegistryResolver {
    async fn resolve(&self) -> Result<Vec<u8>> {
        refuse_insecure_http(&self.registry_url)?;
        let registry_url = self.registry_url.trim_end_matches('/');
        let client = http_client()?;

        // 1. Discovery doc.
        let discovery_url = format!("{}/.well-known/tr-registry.json", registry_url);
        let disco: serde_json::Value = client
            .get(&discovery_url)
            .send()
            .await
            .with_context(|| format!("GET {}", discovery_url))?
            .error_for_status()
            .with_context(|| format!("GET {}", discovery_url))?
            .json()
            .await
            .with_context(|| format!("parse JSON from {}", discovery_url))?;

        let registry_fmt = disco["format_version"].as_str().unwrap_or("");
        if registry_fmt != "tr-registry/1" {
            return Err(anyhow!(
                "registry at {} advertises unsupported format_version `{}`",
                registry_url,
                registry_fmt
            ));
        }
        let advertised_tr_fmt = disco["tr_format"].as_str().unwrap_or("");
        if advertised_tr_fmt != tr_format::FORMAT_VERSION_V3 {
            return Err(anyhow!(
                "registry advertises tr_format `{}` but this client only handles `{}`",
                advertised_tr_fmt,
                tr_format::FORMAT_VERSION_V3
            ));
        }
        let pattern = disco["endpoints"]["download"]
            .as_str()
            .ok_or_else(|| anyhow!("registry doc missing endpoints.download"))?;
        // Default max-pack-bytes when the discovery doc doesn't override.
        // 100 MiB matches the v3 spec §6.4 single-pack ceiling.
        let max_bytes = disco["max_pack_bytes"]
            .as_u64()
            .unwrap_or(100 * 1024 * 1024);

        // 2. Build the download URL by template substitution.
        let download_path = pattern
            .replace("{owner}", &self.owner)
            .replace("{slug}", &self.slug)
            .replace("{version}", &self.version);
        let download_url = format!("{}{}", registry_url, download_path);

        // 3. Fetch the bytes.
        let resp = client
            .get(&download_url)
            .send()
            .await
            .with_context(|| format!("GET {}", download_url))?
            .error_for_status()
            .with_context(|| format!("GET {}", download_url))?;

        if let Some(cl) = resp.content_length() {
            if cl > max_bytes {
                return Err(anyhow!(
                    "registry advertised content-length {} exceeds max_pack_bytes {} for {}/{}",
                    cl,
                    max_bytes,
                    self.owner,
                    self.slug
                ));
            }
        }

        // Capture the registry-advertised hash before consuming the
        // body — this is independent verification on top of the
        // pack-hash check `tr_format::read_v3_pack` performs against
        // the manifest's declared `pack_hash`.
        let advertised_hash: Option<String> = resp
            .headers()
            .get("x-tr-content-hash")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("read body from {}", download_url))?;
        if bytes.len() as u64 > max_bytes {
            return Err(anyhow!(
                "registry returned {} bytes, exceeds max_pack_bytes {}",
                bytes.len(),
                max_bytes
            ));
        }

        // 4. Defense-in-depth hash check. If the registry put a hash
        // in the response header, verify the body matches before we
        // even hand it to `tr_format::read_v3_pack`.
        if let Some(expected) = &advertised_hash {
            let actual = blake3_hex(&bytes);
            if &actual != expected {
                return Err(anyhow!(
                    "content hash mismatch for {}/{}@{}: registry advertised `{}`, computed `{}`",
                    self.owner,
                    self.slug,
                    self.version,
                    expected,
                    actual
                ));
            }
        }
        Ok(bytes.to_vec())
    }
}
