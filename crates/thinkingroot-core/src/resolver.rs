//! Pluggable resolvers for `.tr` pack bytes.
//!
//! A [`PackResolver`] knows how to fetch the raw bytes of one `.tr`
//! file from a specific source — local filesystem, direct HTTPS URL,
//! cloud registry coordinate, future S3-mirror, IPFS, OCI, etc. The
//! trait lives in `thinkingroot-core` (not the CLI) so consumers in
//! cloud services and the desktop can implement custom backends
//! without depending on the CLI binary crate.
//!
//! Trust verification (Sigstore signatures, revocation deny-lists,
//! BLAKE3 cross-check) happens **after** this layer in the consumer.
//! The resolver's only job is to return the bytes — or fail loudly.
//!
//! # Telemetry: [`ResolverDescriptor`]
//!
//! `descriptor()` returns a static description of *what* the resolver
//! is configured to fetch. The `source` string is sanitised so that
//! credentials embedded in URLs (`https://user:pass@example.com/`) are
//! never logged. Used by the CLI install path's structured-log line
//! and by the desktop telemetry breadcrumb on retry events.

use std::fmt;

/// Backend-agnostic source for `.tr` archive bytes.
///
/// Implementations are responsible for any source-specific integrity
/// check (e.g. BLAKE3 cross-check against a registry-advertised
/// header) before returning. Callers do not retry on
/// [`ResolverError`] — the resolver's failure surface is terminal for
/// that resolver instance.
#[async_trait::async_trait]
pub trait PackResolver: Send + Sync {
    /// Fetch the raw bytes of the `.tr` file this resolver was
    /// configured for.
    async fn resolve(&self) -> Result<Vec<u8>, ResolverError>;

    /// Static description of what this resolver fetches. Used for
    /// structured logs and telemetry breadcrumbs. The returned
    /// `source` string MUST NOT contain credentials.
    fn descriptor(&self) -> ResolverDescriptor;
}

/// Static description of a [`PackResolver`]. Returned by
/// `descriptor()` for telemetry. The `source` field is sanitised —
/// credentials embedded in URLs are stripped before construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverDescriptor {
    /// Stable resolver kind identifier. Built-in values:
    ///
    /// - `"local-fs"` — file path on local disk
    /// - `"http-direct"` — a single HTTPS URL pointing at a `.tr`
    /// - `"http-registry"` — `{owner}/{slug}@{version}` against a
    ///   `tr-registry/1` discovery doc
    ///
    /// Custom backends should use `"custom:<name>"` (e.g.
    /// `"custom:s3-mirror"`).
    pub kind: &'static str,
    /// Human-readable description of the source. URLs MUST have any
    /// `user:pass@` userinfo stripped; absolute paths are emitted as
    /// `~/relative` when they live under `$HOME` so structured logs
    /// don't pin the developer's username. See
    /// [`ResolverDescriptor::sanitise_url`].
    pub source: String,
}

impl ResolverDescriptor {
    /// Construct a descriptor, sanitising the `source` string. Use
    /// this constructor in resolver implementations rather than
    /// building the struct directly.
    pub fn new(kind: &'static str, source: impl Into<String>) -> Self {
        let raw = source.into();
        Self {
            kind,
            source: Self::sanitise_url(&raw).unwrap_or(raw),
        }
    }

    /// Strip `user:pass@` userinfo from a URL while preserving the
    /// host, port, path, and query. Returns `None` when the input
    /// doesn't parse as a URL — the caller then falls back to the raw
    /// string. We avoid a full URL crate dep; the sanitiser only
    /// handles the `scheme://userinfo@host…` shape.
    pub fn sanitise_url(raw: &str) -> Option<String> {
        let scheme_end = raw.find("://")?;
        let after_scheme = &raw[scheme_end + 3..];
        let rest_idx = after_scheme.find('@')?;
        let host_and_rest = &after_scheme[rest_idx + 1..];
        // Refuse pathological inputs where `@` appears inside a path
        // segment without a userinfo separator (e.g. an `@` in a S3
        // object key after the host — `@` is technically reserved but
        // some toolchains emit it). Userinfo cannot contain `/`, so if
        // the candidate userinfo segment contains a `/`, this isn't
        // userinfo at all.
        let candidate_userinfo = &after_scheme[..rest_idx];
        if candidate_userinfo.contains('/') {
            return None;
        }
        Some(format!("{}://{}", &raw[..scheme_end], host_and_rest))
    }
}

/// Terminal failure from a [`PackResolver`]. Carries the resolver
/// kind and a free-form detail string so the caller can render a
/// useful error message; the optional `source` chain preserves the
/// underlying cause for `tracing`'s `error.cause` field.
#[derive(Debug)]
pub struct ResolverError {
    /// Stable resolver kind that failed. Mirrors
    /// [`ResolverDescriptor::kind`].
    pub kind: &'static str,
    /// Human-readable detail string.
    pub detail: String,
    /// Underlying cause when the resolver wraps a foreign error.
    pub source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl ResolverError {
    /// Build a [`ResolverError`] with no underlying source.
    pub fn new(kind: &'static str, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            source: None,
        }
    }

    /// Build a [`ResolverError`] wrapping an underlying cause. The
    /// cause is preserved for tracing; callers SHOULD NOT include the
    /// cause's message in `detail` — the `Display` impl already
    /// renders the chain.
    pub fn with_source(
        kind: &'static str,
        detail: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            detail: detail.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for ResolverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "resolver `{}` failed: {}", self.kind, self.detail)?;
        if let Some(s) = &self.source {
            write!(f, ": {s}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ResolverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|s| &**s as _)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_sanitises_credentials() {
        let d = ResolverDescriptor::new(
            "http-direct",
            "https://user:pass@example.com/pack.tr?token=abc",
        );
        assert_eq!(d.kind, "http-direct");
        assert_eq!(d.source, "https://example.com/pack.tr?token=abc");
    }

    #[test]
    fn descriptor_passes_through_url_without_userinfo() {
        let d = ResolverDescriptor::new("http-direct", "https://example.com/pack.tr");
        assert_eq!(d.source, "https://example.com/pack.tr");
    }

    #[test]
    fn descriptor_passes_through_local_path() {
        let d = ResolverDescriptor::new("local-fs", "/tmp/demo.tr");
        assert_eq!(d.source, "/tmp/demo.tr");
    }

    #[test]
    fn descriptor_does_not_treat_at_in_path_as_userinfo() {
        // `@` after the host without preceding userinfo is part of
        // the path; do not corrupt the URL by stripping it.
        let d = ResolverDescriptor::new(
            "http-direct",
            "https://example.com/path/with@at-sign.tr",
        );
        assert_eq!(d.source, "https://example.com/path/with@at-sign.tr");
    }

    #[test]
    fn resolver_error_display_includes_source() {
        let inner = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let err = ResolverError::with_source("local-fs", "could not read pack", inner);
        let s = format!("{err}");
        assert!(s.contains("local-fs"));
        assert!(s.contains("could not read pack"));
        assert!(s.contains("missing"));
    }

    #[test]
    fn resolver_error_chains_via_std_error() {
        let inner = std::io::Error::other("inner");
        let err = ResolverError::with_source("local-fs", "outer", inner);
        let src: &dyn std::error::Error = &err;
        assert!(src.source().is_some());
    }
}
