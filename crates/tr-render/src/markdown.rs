//! Markdown summary builder.
//!
//! Output is plain Markdown — no HTML, no embedded scripts. The
//! desktop install sheet renders it via `react-markdown`; the CLI
//! prints it as-is for `root install --dry-run`.

use std::fmt::Write as _;

use tr_format::{Manifest, TrustTier};

use crate::ArchiveStats;

pub(crate) fn summary(manifest: &Manifest, stats: ArchiveStats) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "# {} {}", manifest.name, manifest.version);
    out.push('\n');
    if !manifest.description.is_empty() {
        let _ = writeln!(out, "> {}", manifest.description);
        out.push('\n');
    }

    out.push_str("## Overview\n\n");
    let _ = writeln!(out, "- **License** — `{}`", manifest.license);
    let _ = writeln!(
        out,
        "- **Trust tier** — {}",
        trust_tier_label(manifest.trust_tier)
    );
    if !manifest.authors.is_empty() {
        let _ = writeln!(out, "- **Authors** — {}", manifest.authors.join(", "));
    }
    if !manifest.tags.is_empty() {
        let tags = manifest
            .tags
            .iter()
            .map(|t| format!("`{t}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "- **Tags** — {tags}");
    }
    if let Some(claims) = manifest.claim_count {
        let _ = writeln!(out, "- **Claims** — {claims}");
    }
    if let Some(rooted) = manifest.rooted_pct {
        let _ = writeln!(out, "- **Rooted** — {rooted:.1}%");
    }

    out.push_str("\n## Capabilities\n\n");
    let caps = &manifest.capabilities;
    let any_declared = caps.is_privileged()
        || !caps.mcp_tools.is_empty()
        || !caps.mcp_resources.is_empty();
    if any_declared {
        if caps.network {
            out.push_str("- **Outbound network** required\n");
        }
        if caps.filesystem {
            out.push_str("- **Filesystem access** outside the pack sandbox required\n");
        }
        if caps.exec {
            out.push_str("- **Subprocess execution** required\n");
        }
        if !caps.mcp_tools.is_empty() {
            let tools = caps
                .mcp_tools
                .iter()
                .map(|t| format!("`{t}`"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "- MCP tools: {tools}");
        }
        if !caps.mcp_resources.is_empty() {
            let res = caps
                .mcp_resources
                .iter()
                .map(|r| format!("`{r}`"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "- MCP resources: {res}");
        }
    } else {
        out.push_str("- _none declared_\n");
    }

    out.push_str("\n## Archive\n\n");
    let _ = writeln!(out, "- Entries: {}", stats.entry_count);
    let _ = writeln!(out, "- Sources: {}", stats.source_count);
    let _ = writeln!(out, "- Payload: {}", format_bytes(stats.payload_bytes));

    if let Some(readme) = manifest.readme.as_deref().filter(|r| !r.is_empty()) {
        out.push_str("\n## README\n\n");
        out.push_str(readme);
        if !readme.ends_with('\n') {
            out.push('\n');
        }
    }

    out
}

fn trust_tier_label(tier: TrustTier) -> &'static str {
    match tier {
        TrustTier::T0 => "T0 — unsigned",
        TrustTier::T1 => "T1 — author-signed",
        TrustTier::T2 => "T2 — Sigstore-attested",
        TrustTier::T3 => "T3 — per-claim certificates",
        TrustTier::T4 => "T4 — re-rootable from embedded sources",
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
