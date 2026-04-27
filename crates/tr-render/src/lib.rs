//! `tr-render` — pure-Rust renderer producing human-readable previews
//! of TR-1 `.tr` packs.
//!
//! Both `root install --dry-run` and the desktop install sheet
//! consume [`render_preview`] to show the user what they're about to
//! mount before any extraction happens.
//!
//! No HTML / no JS / no Markdown-to-HTML conversion is performed
//! here: we emit Markdown text and a monospace-friendly ASCII table;
//! the caller decides how to display them.
//!
//! The crate intentionally has no async, no I/O beyond reading the
//! supplied byte slice, and no dependency on `tokio` — the preview
//! must be cheap to compute even from inside a Tauri command handler.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod manifest_table;
mod markdown;

use tr_format::{reader, Manifest};

/// Output bundle returned by [`render_preview`].
#[derive(Debug, Clone)]
pub struct RenderedPreview {
    /// Markdown summary of the pack — name, version, license,
    /// trust tier, capabilities, archive stats, and the manifest's
    /// readme inlined.
    pub markdown: String,
    /// Monospace-friendly ASCII table of the load-bearing manifest
    /// fields, suitable for terminal display.
    pub manifest_table: String,
    /// Count of in-archive source-byte references (entries under
    /// `provenance/`).
    pub source_count: usize,
    /// Total payload entries (excludes `manifest.json`).
    pub entry_count: usize,
    /// Sum of payload-entry sizes in bytes.
    pub payload_bytes: u64,
}

/// Errors surfaced by [`render_preview`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Failure reading the underlying `.tr` archive.
    #[error(transparent)]
    Format(#[from] tr_format::Error),
}

/// Convenience alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Render a preview of a `.tr` pack.
///
/// `manifest` is taken explicitly so the caller can avoid re-parsing
/// the same blob twice — `tr-verify` and the install pipeline both
/// already own a parsed [`Manifest`] by the time the preview is
/// requested. The supplied bytes are still walked so the function
/// can populate archive-level stats (entry / source / payload sizes).
pub fn render_preview(manifest: &Manifest, archive_bytes: &[u8]) -> Result<RenderedPreview> {
    let pack = reader::read_bytes(archive_bytes)?;

    let source_count = pack
        .paths()
        .filter(|p| p.starts_with("provenance/") && !p.ends_with('/'))
        .count();
    let entry_count = pack.len().saturating_sub(1);
    let payload_bytes = pack.payload_bytes();

    let manifest_table = manifest_table::format(manifest);
    let markdown = markdown::summary(
        manifest,
        ArchiveStats {
            source_count,
            entry_count,
            payload_bytes,
        },
    );

    Ok(RenderedPreview {
        markdown,
        manifest_table,
        source_count,
        entry_count,
        payload_bytes,
    })
}

/// Stats threaded into [`markdown::summary`]. Internal-only to keep
/// the public surface minimal.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArchiveStats {
    pub source_count: usize,
    pub entry_count: usize,
    pub payload_bytes: u64,
}
