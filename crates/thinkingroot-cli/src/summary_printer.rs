use thinkingroot_core::{IncrementalSummary, types::PHASE_NAMES};

/// Format a byte count as a human-readable string.
fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

const DIVIDER: &str = "─────────────────────────────────────────────";

/// Render `summary` as a multi-line ASCII table.
///
/// Phase timings are emitted in `PHASE_NAMES` order so callers see
/// a stable pipeline ordering rather than BTreeMap alphabetic (which
/// would put "audit" before "diff").
pub fn render(summary: &IncrementalSummary) -> String {
    let mut out = String::with_capacity(512);

    out.push_str(&format!(
        "Compile complete ({}ms)\n",
        summary.total_elapsed_ms
    ));
    out.push_str(DIVIDER);
    out.push('\n');

    out.push_str(&format!(
        "Sources    | {} total | {} changed | {} deleted | {} dirty (re-resolved)\n",
        summary.sources_total,
        summary.sources_truly_changed,
        summary.sources_deleted,
        summary.sources_resolution_dirty,
    ));
    out.push_str(&format!(
        "Claims     | +{} added | ~{} updated | -{} deleted\n",
        summary.claims_added,
        summary.claims_updated,
        summary.claims_deleted,
    ));
    out.push_str(&format!(
        "Structural | {} cascaded | {} emitted | {} re-extracted\n",
        summary.structural_rows_cascaded,
        summary.structural_rows_emitted,
        format_bytes(summary.bytes_re_extracted),
    ));
    out.push_str(&format!(
        "Extract    | {} LLM calls | {} cache hits | {} zero-LLM\n",
        summary.llm_calls,
        summary.cache_hits,
        summary.structural_extractions,
    ));

    out.push_str(DIVIDER);
    out.push('\n');
    out.push_str("Phase timings:\n");

    for name in PHASE_NAMES {
        if let Some(ms) = summary.phase_timings.get(*name) {
            out.push_str(&format!("  {:<20} {}ms\n", name, ms));
        }
    }

    out.push_str(DIVIDER);
    out.push('\n');

    out
}

/// Print `render(summary)` to stderr or stdout depending on `to_stderr`.
///
/// Pass `to_stderr = true` when a TTY progress bar wrote to stderr so
/// the summary sits on the same stream as the bars. This mirrors the
/// existing `out` closure in `run_compile`.
pub fn print(summary: &IncrementalSummary, to_stderr: bool) {
    let text = render(summary);
    if to_stderr {
        eprint!("{text}");
    } else {
        std::io::Write::write_all(&mut std::io::stdout(), text.as_bytes())
            .expect("write to stdout failed");
    }
}
