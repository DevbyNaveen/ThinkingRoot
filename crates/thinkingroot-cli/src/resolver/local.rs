//! [`LocalFsResolver`] — `.tr` bytes loaded from a local file path.

use std::path::PathBuf;

use async_trait::async_trait;
use thinkingroot_core::resolver::{PackResolver, ResolverDescriptor, ResolverError};

/// Read a `.tr` from a local filesystem path. Performs no integrity
/// check beyond what [`tr_format::read_v3_pack`] does on parse —
/// local installs implicitly trust the user's filesystem.
pub struct LocalFsResolver {
    path: PathBuf,
}

impl LocalFsResolver {
    /// Build a resolver for the given path. The file is not opened
    /// until [`PackResolver::resolve`] is called.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl PackResolver for LocalFsResolver {
    async fn resolve(&self) -> Result<Vec<u8>, ResolverError> {
        std::fs::read(&self.path).map_err(|e| {
            ResolverError::with_source(
                "local-fs",
                format!("read {}", self.path.display()),
                e,
            )
        })
    }

    fn descriptor(&self) -> ResolverDescriptor {
        ResolverDescriptor::new("local-fs", self.path.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn resolve_returns_file_bytes() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("pack.tr");
        std::fs::write(&path, b"hello world").unwrap();

        let resolver = LocalFsResolver::new(path);
        let bytes = resolver.resolve().await.unwrap();
        assert_eq!(bytes, b"hello world");
    }

    #[tokio::test]
    async fn resolve_errors_on_missing_file() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("missing.tr");
        let resolver = LocalFsResolver::new(path);
        let err = resolver.resolve().await.unwrap_err();
        assert_eq!(err.kind, "local-fs");
        assert!(err.detail.contains("read"));
    }

    #[tokio::test]
    async fn descriptor_reports_local_fs_kind() {
        let resolver = LocalFsResolver::new("/tmp/example.tr");
        let d = resolver.descriptor();
        assert_eq!(d.kind, "local-fs");
        assert_eq!(d.source, "/tmp/example.tr");
    }
}
