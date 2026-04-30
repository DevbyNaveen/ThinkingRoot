//! Markdown summary builder.
//!
//! Output is plain Markdown — no HTML, no embedded scripts. The
//! desktop install sheet renders it via `react-markdown`; the CLI
//! prints it as-is for `root install --dry-run`.

use std::fmt::Write as _;

use tr_format::V3Pack;

use crate::ArchiveStats;

pub(crate) fn summary(pack: &V3Pack, stats: ArchiveStats) -> String {
    let m = &pack.manifest;
    let mut out = String::new();

    let _ = writeln!(out, "# {} {}", m.name, m.version);
    out.push('\n');
    if let Some(desc) = m.description.as_deref().filter(|d| !d.is_empty()) {
        let _ = writeln!(out, "> {desc}");
        out.push('\n');
    }

    out.push_str("## Overview\n\n");
    if let Some(license) = m.license.as_deref().filter(|l| !l.is_empty()) {
        let _ = writeln!(out, "- **License** — `{license}`");
    }
    let _ = writeln!(out, "- **Format** — `{}`", m.format_version);
    let _ = writeln!(out, "- **Signature** — {}", signature_label(pack));
    if !m.authors.is_empty() {
        let _ = writeln!(out, "- **Authors** — {}", m.authors.join(", "));
    }
    if let Some(extractor) = m.extractor.as_deref().filter(|e| !e.is_empty()) {
        let _ = writeln!(out, "- **Extractor** — `{extractor}`");
    }
    if let Some(extracted_at) = m.extracted_at {
        let _ = writeln!(out, "- **Extracted** — {}", extracted_at.to_rfc3339());
    }

    out.push_str("\n## Archive\n\n");
    let _ = writeln!(out, "- Source files: {}", stats.source_count);
    let _ = writeln!(out, "- Claims: {}", stats.claim_count);
    let _ = writeln!(
        out,
        "- Source archive: {}",
        format_bytes(stats.source_archive_bytes)
    );

    out
}

/// Human-readable signature status. The full bundle (cert chain,
/// Rekor witness) is not inspected here — that's the verifier's job.
/// This is just a one-liner for the preview.
fn signature_label(pack: &V3Pack) -> &'static str {
    match &pack.signature {
        None => "unsigned",
        Some(b) => match (
            b.verification_material.public_key.as_ref(),
            b.verification_material.x509_certificate_chain.as_ref(),
        ) {
            (None, Some(_)) => "Sigstore-keyless (Fulcio cert chain)",
            (Some(_), None) => "self-signed (Ed25519 public key embedded)",
            (Some(_), Some(_)) => "self-signed + Sigstore (transition shape)",
            (None, None) => "signed (verification material empty)",
        },
    }
}

fn format_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.2} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.2} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}
