//! `tr-render` — pure-Rust renderer producing human-readable previews
//! of v3 `.tr` packs.
//!
//! Both `root install --dry-run` and the desktop install sheet
//! consume [`render_preview`] to show the user what they're about to
//! mount before any extraction happens.
//!
//! No HTML / no JS / no Markdown-to-HTML conversion is performed
//! here: we emit Markdown text and a monospace-friendly ASCII table;
//! the caller decides how to display them.
//!
//! The crate intentionally has no async, no I/O beyond the
//! already-parsed [`V3Pack`], and no dependency on `tokio` — the
//! preview must be cheap to compute even from inside a Tauri command
//! handler.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod manifest_table;
mod markdown;

use tr_format::V3Pack;

/// Output bundle returned by [`render_preview`].
#[derive(Debug, Clone)]
pub struct RenderedPreview {
    /// Markdown summary of the pack — name, version, license,
    /// signature status, archive stats, extractor identity.
    pub markdown: String,
    /// Monospace-friendly ASCII table of the load-bearing manifest
    /// fields, suitable for terminal display.
    pub manifest_table: String,
    /// Number of source files declared in the manifest, when present.
    pub source_count: u64,
    /// Number of claims declared in the manifest, when present.
    pub claim_count: u64,
    /// Total source-archive payload bytes (`source.tar.zst` size).
    pub source_archive_bytes: u64,
}

/// Errors surfaced by [`render_preview`]. Today none — the renderer
/// is infallible given a parsed [`V3Pack`] — but kept as a stable
/// surface for forward-compat with future fallible inspection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {}

/// Convenience alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Render a preview of a v3 `.tr` pack.
///
/// The caller is expected to have already parsed the pack via
/// [`tr_format::read_v3_pack`] — the renderer pulls everything it
/// needs out of the parsed structure and never re-walks the archive
/// bytes.
pub fn render_preview(pack: &V3Pack) -> Result<RenderedPreview> {
    let source_count = pack.manifest.source_files.unwrap_or(0);
    let claim_count = pack.manifest.claim_count.unwrap_or(0);
    let source_archive_bytes = pack.source_archive.len() as u64;

    let manifest_table = manifest_table::format(pack);
    let markdown = markdown::summary(
        pack,
        ArchiveStats {
            source_count,
            claim_count,
            source_archive_bytes,
        },
    );

    Ok(RenderedPreview {
        markdown,
        manifest_table,
        source_count,
        claim_count,
        source_archive_bytes,
    })
}

/// Stats threaded into [`markdown::summary`]. Internal-only to keep
/// the public surface minimal.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArchiveStats {
    pub source_count: u64,
    pub claim_count: u64,
    pub source_archive_bytes: u64,
}
