//! LSP-backed rules in the Witness Mesh catalog.
//!
//! The v1.0 catalog declares six `lsp::*` rules:
//! - `lsp::type-of@v1`
//! - `lsp::definition-of@v1`
//! - `lsp::callers-of@v1`
//! - `lsp::implementations-of@v1`
//! - `lsp::documentation-of@v1`
//! - `lsp::skipped@v1`
//!
//! This module owns:
//! 1. **Backend detection** — probes `$PATH` for `rust-analyzer`,
//!    `tsserver`, `pyright-langserver`. Pure, side-effect-free.
//! 2. **`lsp::skipped@v1` emission** — when a source's language has
//!    no detected backend, emits a deterministic skipped Witness so
//!    the mesh records the absence honestly (no silent gap).
//!
//! The real LSP subprocess protocol (initialize → hover → definition
//! → callHierarchy → shutdown) is wired in `extractor.rs` at the
//! pipeline-dispatch layer where per-chunk request batching is
//! efficient. This module is the catalog-side surface; the pipeline
//! is the protocol-side surface. They communicate through the public
//! types here.
//!
//! Determinism contract for `lsp::skipped@v1`:
//! - Same source bytes + same set of missing backends → same Witness id.
//! - The Witness's `inputs` is a single `ByteRef` covering the entire
//!   source file. Its `content_blake3` is the BLAKE3 of the file's
//!   raw bytes. This makes the skip-Witness behave like every other
//!   Witness under `tr-verify`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use thinkingroot_core::types::{
    Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
};

/// LSP backend identity. Each variant corresponds to one server
/// binary searched for in `$PATH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspBackend {
    RustAnalyzer,
    Tsserver,
    Pyright,
}

impl LspBackend {
    /// Canonical binary name probed in `$PATH`.
    pub fn binary_name(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::Tsserver => "tsserver",
            // The pyright server's binary is `pyright-langserver`,
            // not `pyright` (which is the CLI wrapper).
            Self::Pyright => "pyright-langserver",
        }
    }

    /// Languages this backend serves. Used by `for_language` to
    /// route a source to its applicable backend.
    pub fn languages(self) -> &'static [&'static str] {
        match self {
            Self::RustAnalyzer => &["rust"],
            Self::Tsserver => &["typescript", "javascript"],
            Self::Pyright => &["python"],
        }
    }

    /// Return the backend (if any) that serves the given language.
    /// `None` for languages outside the v1.0 LSP scope (Go, Java,
    /// C/C++, etc. — those use tree-sitter only in v1.0).
    pub fn for_language(language: &str) -> Option<Self> {
        for backend in Self::ALL {
            if backend.languages().iter().any(|l| *l == language) {
                return Some(*backend);
            }
        }
        None
    }

    pub const ALL: &'static [LspBackend] =
        &[LspBackend::RustAnalyzer, LspBackend::Tsserver, LspBackend::Pyright];
}

/// Result of scanning `$PATH` for LSP binaries. Empty when no
/// backends are installed (the common case in CI).
#[derive(Debug, Clone)]
pub struct DetectedBackends {
    /// Set of available `(backend, path_to_binary)` pairs.
    pub available: Vec<(LspBackend, PathBuf)>,
}

impl DetectedBackends {
    pub fn is_empty(&self) -> bool {
        self.available.is_empty()
    }

    pub fn has(&self, backend: LspBackend) -> bool {
        self.available.iter().any(|(b, _)| *b == backend)
    }

    pub fn path_for(&self, backend: LspBackend) -> Option<&PathBuf> {
        self.available.iter().find(|(b, _)| *b == backend).map(|(_, p)| p)
    }
}

/// Scan `$PATH` for the three LSP backends. Returns the full set
/// of available `(backend, absolute path)` pairs.
///
/// Implementation: splits `PATH` on the platform separator (`:` on
/// Unix, `;` on Windows), iterates each directory, and checks for
/// each backend's binary name (with platform-specific `.exe`
/// extension on Windows). The first match per backend wins; the
/// result is order-stable (sorted by `LspBackend::ALL` order).
pub fn detect_backends() -> DetectedBackends {
    detect_backends_in_path(std::env::var_os("PATH").as_deref())
}

/// Test-injectable variant taking a custom `PATH` string. Splits the
/// path on the platform separator and probes each entry.
pub fn detect_backends_in_path(path_env: Option<&std::ffi::OsStr>) -> DetectedBackends {
    let path_env = match path_env {
        Some(p) => p.to_owned(),
        None => return DetectedBackends { available: vec![] },
    };

    let directories: Vec<PathBuf> = std::env::split_paths(&path_env).collect();
    let mut available: Vec<(LspBackend, PathBuf)> = Vec::new();

    for backend in LspBackend::ALL {
        if let Some(path) = locate_binary(&directories, backend.binary_name()) {
            available.push((*backend, path));
        }
    }

    DetectedBackends { available }
}

fn locate_binary(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    let candidates: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    for dir in dirs {
        for cand in &candidates {
            let candidate = dir.join(cand);
            if candidate.is_file() && is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &std::path::Path) -> bool {
    // On Windows, the file-extension check inside `locate_binary`
    // is the executability signal.
    true
}

/// Emit an `lsp::skipped@v1` Witness recording that the source's
/// language has no detected LSP backend.
///
/// Returns `None` when the source's language is outside the v1.0
/// LSP scope (e.g. Go, Java, C/C++ — those don't have an `lsp::*`
/// rule in v1.0 and silently producing a skipped Witness would
/// over-claim).
pub fn emit_skipped_witness(
    language: &str,
    source_bytes: &[u8],
    file_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Option<Witness> {
    // Only emit a skip for languages the catalog claims to support.
    LspBackend::for_language(language)?;

    let span = WitnessSpan {
        file_blake3: file_blake3.to_string(),
        start: 0,
        end: source_bytes.len() as u64,
    };
    let content_blake3 = blake3::hash(source_bytes).to_hex().to_string();
    Some(Witness::new(
        "lsp::skipped@v1",
        "lsp::skipped",
        vec![WitnessInput::ByteRef {
            file_blake3: file_blake3.to_string(),
            start: 0,
            end: source_bytes.len() as u64,
        }],
        vec![span],
        source_id,
        workspace_id,
        Sensitivity::Public,
        Confidence::new(0.99),
        content_blake3,
        now,
    ))
}

/// Per-source LSP Witness extraction.
///
/// Routing:
/// - If `detected.has(LspBackend::for_language(language))`, the real
///   LSP protocol is invoked by the pipeline-level dispatcher (see
///   `extractor.rs` in Commit 2; the protocol wires hover / definition
///   / callHierarchy and returns a real Witness vec).
/// - Otherwise, emit `lsp::skipped@v1` so the absence is recorded
///   honestly in the mesh.
///
/// In v1.0.0 the protocol-side is gated on `LSP_PROTOCOL_ENABLED`
/// being true (default false until the Commit 2 protocol lands).
/// This lets the catalog ship the rules without falsely claiming
/// LSP-derived Witnesses today.
pub fn extract_witnesses_for_source(
    language: &str,
    source_bytes: &[u8],
    file_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    detected: &DetectedBackends,
    now: DateTime<Utc>,
) -> Vec<Witness> {
    let backend = match LspBackend::for_language(language) {
        Some(b) => b,
        None => return Vec::new(), // language outside v1.0 LSP scope
    };
    let backend_available = detected.has(backend);
    if !backend_available || !LSP_PROTOCOL_ENABLED {
        return emit_skipped_witness(
            language,
            source_bytes,
            file_blake3,
            source_id,
            workspace_id,
            now,
        )
        .map(|w| vec![w])
        .unwrap_or_default();
    }
    // Real protocol invocation lives in the pipeline-level
    // dispatcher (Commit 2 — `extractor.rs::run_lsp_pass`). When
    // the catalog ships standalone (this v1.0 scaffold), the
    // function returns the skipped witness above.
    Vec::new()
}

/// Compile-time gate for the LSP protocol invocation path.
///
/// `false` in this v1.0 scaffold: `extract_witnesses_for_source`
/// always emits `lsp::skipped@v1`. The flag flips to `true` in
/// Commit 2 when `extractor.rs::run_lsp_pass` wires the real
/// subprocess protocol. Until then we honestly emit "skipped"
/// rather than fabricating LSP-derived Witnesses.
pub const LSP_PROTOCOL_ENABLED: bool = false;

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn backend_for_language_routes_supported_languages() {
        assert_eq!(LspBackend::for_language("rust"), Some(LspBackend::RustAnalyzer));
        assert_eq!(
            LspBackend::for_language("typescript"),
            Some(LspBackend::Tsserver)
        );
        assert_eq!(
            LspBackend::for_language("javascript"),
            Some(LspBackend::Tsserver)
        );
        assert_eq!(LspBackend::for_language("python"), Some(LspBackend::Pyright));
    }

    #[test]
    fn backend_for_language_returns_none_outside_scope() {
        for lang in ["go", "java", "c", "cpp", "ruby", "kotlin", "swift", "haskell"] {
            assert_eq!(LspBackend::for_language(lang), None, "language {lang}");
        }
    }

    #[test]
    fn detect_backends_with_no_path_env_returns_empty() {
        let detected = detect_backends_in_path(None);
        assert!(detected.is_empty());
        assert!(!detected.has(LspBackend::RustAnalyzer));
        assert!(detected.path_for(LspBackend::RustAnalyzer).is_none());
    }

    #[test]
    fn detect_backends_with_empty_path_env_returns_empty() {
        let empty = OsString::from("");
        let detected = detect_backends_in_path(Some(&empty));
        assert!(detected.is_empty());
    }

    #[test]
    fn detect_backends_in_nonexistent_path_returns_empty() {
        let bogus = OsString::from("/nonexistent/path/12345");
        let detected = detect_backends_in_path(Some(&bogus));
        assert!(detected.is_empty());
    }

    #[test]
    fn detected_backends_helpers_match_population() {
        let det = DetectedBackends {
            available: vec![(LspBackend::RustAnalyzer, PathBuf::from("/usr/bin/rust-analyzer"))],
        };
        assert!(!det.is_empty());
        assert!(det.has(LspBackend::RustAnalyzer));
        assert!(!det.has(LspBackend::Tsserver));
        assert_eq!(
            det.path_for(LspBackend::RustAnalyzer).map(|p| p.as_path()),
            Some(std::path::Path::new("/usr/bin/rust-analyzer"))
        );
    }

    #[test]
    fn skipped_witness_emitted_for_supported_language() {
        let bytes = b"fn main() {}";
        let w = emit_skipped_witness(
            "rust",
            bytes,
            "fakeblake3",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        )
        .expect("rust is in v1.0 LSP scope");
        assert_eq!(w.rule, "lsp::skipped@v1");
        assert_eq!(w.witness_type, "lsp::skipped");
        assert_eq!(w.spans[0].start, 0);
        assert_eq!(w.spans[0].end, bytes.len() as u64);
        // BLAKE3 must match the actual source bytes.
        let expected = blake3::hash(bytes).to_hex().to_string();
        assert_eq!(w.content_blake3, expected);
    }

    #[test]
    fn skipped_witness_not_emitted_for_unscoped_language() {
        let w = emit_skipped_witness(
            "go",
            b"package main",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert!(w.is_none(), "Go is outside v1.0 LSP scope — never emit skipped");
    }

    #[test]
    fn extract_witnesses_routes_to_skipped_when_protocol_disabled() {
        // v1.0 scaffold: LSP_PROTOCOL_ENABLED == false. Even when
        // a backend is "detected," we honestly emit skipped because
        // the protocol invocation isn't wired yet.
        let det = DetectedBackends {
            available: vec![(LspBackend::RustAnalyzer, PathBuf::from("/usr/bin/rust-analyzer"))],
        };
        let witnesses = extract_witnesses_for_source(
            "rust",
            b"fn main() {}",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            &det,
            Utc::now(),
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].rule, "lsp::skipped@v1");
    }

    #[test]
    fn extract_witnesses_emits_skipped_for_supported_lang_without_backend() {
        let det = DetectedBackends { available: vec![] };
        let witnesses = extract_witnesses_for_source(
            "python",
            b"x = 1\n",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            &det,
            Utc::now(),
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].rule, "lsp::skipped@v1");
    }

    #[test]
    fn extract_witnesses_emits_nothing_for_unscoped_language() {
        let det = DetectedBackends { available: vec![] };
        let witnesses = extract_witnesses_for_source(
            "haskell",
            b"main = return ()\n",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            &det,
            Utc::now(),
        );
        assert!(witnesses.is_empty(), "Haskell is outside v1.0 LSP scope");
    }

    #[test]
    fn binary_names_match_canonical_lsp_install_names() {
        // Stability test: a regression in binary_name() would cause
        // backend detection to fail on every install.
        assert_eq!(LspBackend::RustAnalyzer.binary_name(), "rust-analyzer");
        assert_eq!(LspBackend::Tsserver.binary_name(), "tsserver");
        assert_eq!(LspBackend::Pyright.binary_name(), "pyright-langserver");
    }
}
