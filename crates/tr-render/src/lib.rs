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
    /// Number of witness rows packed in `witnesses.cbor`. Zero when
    /// the pack is v3 / v3.1, or v3.2 without witnesses. Derived by
    /// decoding the CBOR array length — cheap (header-only walk).
    pub witness_count: u64,
    /// First ~500 chars of the Living Paper body (with YAML
    /// frontmatter stripped) when the pack carries `paper.md`. The
    /// install sheet shows this preview before mounting; the full
    /// body lands in the workspace on install. `None` when the pack
    /// has no paper attached.
    pub paper_preview_md: Option<String>,
    /// `true` iff the pack is `tr/3.2` and carries the Witness Mesh
    /// pair (`witnesses.cbor` + `rule_catalog.toml`). Used by the
    /// install UI to badge a pack as "witness-grounded".
    pub has_witness_mesh: bool,
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

    // Decode the CBOR array length (cheap header walk) — falls back
    // to zero on any decode error so a malformed v3.2 pack still
    // renders a useful preview instead of an opaque renderer crash.
    let witness_count = pack
        .witnesses_cbor
        .as_ref()
        .and_then(|bytes| {
            ciborium::de::from_reader::<Vec<ciborium::Value>, _>(bytes.as_slice()).ok()
        })
        .map(|v| v.len() as u64)
        .unwrap_or(0);

    let has_witness_mesh =
        pack.witnesses_cbor.is_some() && pack.rule_catalog_toml.is_some();

    let paper_preview_md = pack
        .paper_md
        .as_ref()
        .map(|bytes| extract_paper_preview(bytes));

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
        witness_count,
        paper_preview_md,
        has_witness_mesh,
    })
}

/// Strip the YAML frontmatter delimited by leading `---` lines and
/// return up to 500 chars of the visible markdown body. Used by the
/// install preview so the user sees the human-readable opening of
/// the Living Paper, not the machine-readable spine.
///
/// Made deliberately tolerant: a malformed paper (no frontmatter, or
/// no closing fence) falls back to the first 500 chars of the raw
/// body — the preview never errors.
fn extract_paper_preview(bytes: &[u8]) -> String {
    const MAX_PREVIEW_CHARS: usize = 500;
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return String::new(), // paper.md is UTF-8 by contract; opaque bytes get an empty preview
    };

    let body = if let Some(rest) = text.strip_prefix("---\n") {
        // Find the next "\n---" terminator; the body is everything
        // after the line following that terminator.
        if let Some(end) = rest.find("\n---") {
            // Skip past "\n---" and the rest of that line.
            let after_fence = &rest[end + 4..];
            after_fence
                .find('\n')
                .map(|i| &after_fence[i + 1..])
                .unwrap_or("")
        } else {
            // Unterminated frontmatter — best-effort: show the raw text.
            text
        }
    } else {
        text
    };

    body.trim_start().chars().take(MAX_PREVIEW_CHARS).collect()
}

/// Stats threaded into [`markdown::summary`]. Internal-only to keep
/// the public surface minimal.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArchiveStats {
    pub source_count: u64,
    pub claim_count: u64,
    pub source_archive_bytes: u64,
}
