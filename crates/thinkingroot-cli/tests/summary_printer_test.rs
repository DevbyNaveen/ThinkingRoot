use std::collections::BTreeMap;
use thinkingroot_core::{IncrementalSummary, types::PHASE_NAMES};

fn make_summary() -> IncrementalSummary {
    let mut phase_timings = BTreeMap::new();
    for name in PHASE_NAMES {
        phase_timings.insert(name.to_string(), 10u64);
    }
    IncrementalSummary {
        sources_total: 1245,
        sources_unchanged: 1230,
        sources_truly_changed: 3,
        sources_deleted: 1,
        sources_resolution_dirty: 11,
        claims_added: 18,
        claims_updated: 9,
        claims_deleted: 7,
        structural_rows_emitted: 124,
        structural_rows_cascaded: 41,
        bytes_re_extracted: 12_700,
        llm_calls: 4,
        cache_hits: 20,
        structural_extractions: 0,
        chunks_without_extraction: 0,
        phase_timings,
        total_elapsed_ms: 843,
    }
}

#[test]
fn render_returns_non_empty_string() {
    use thinkingroot_cli::summary_printer;
    let summary = make_summary();
    let out = summary_printer::render(&summary);
    assert!(!out.is_empty());
    assert!(out.contains("Compile complete"), "expected 'Compile complete' in: {out}");
    assert!(out.contains("Sources"), "expected 'Sources' in: {out}");
    assert!(out.contains("Claims"), "expected 'Claims' in: {out}");
    assert!(out.contains("Phase timings"), "expected 'Phase timings' in: {out}");
}

#[test]
fn render_with_zero_summary_shows_empty_state() {
    use thinkingroot_cli::summary_printer;
    let summary = IncrementalSummary::default();
    let out = summary_printer::render(&summary);
    assert!(out.contains("0 total"), "expected '0 total' in: {out}");
}

#[test]
fn render_phase_timings_in_canonical_order() {
    use thinkingroot_cli::summary_printer;
    let mut phase_timings = BTreeMap::new();
    for name in PHASE_NAMES {
        phase_timings.insert(name.to_string(), 10u64);
    }
    let summary = IncrementalSummary {
        phase_timings,
        total_elapsed_ms: 100,
        ..Default::default()
    };
    let out = summary_printer::render(&summary);

    // Locate the "Phase timings:" header so we only search within
    // that section, not in the table rows above it (which contain
    // words like "Extract" that would otherwise beat "diff").
    let timings_start = out
        .find("Phase timings")
        .expect("'Phase timings' header not found in output");
    let timings_section = &out[timings_start..];

    // Full contract test: assert ALL 9 adjacent pairs from PHASE_NAMES appear
    // in the correct pipeline order. This catches any BTreeMap-alphabetic
    // ordering regression (e.g. "audit" < "diff" alphabetically).
    for window in PHASE_NAMES.windows(2) {
        let prev_name = window[0];
        let next_name = window[1];
        let prev_pos = timings_section
            .find(prev_name)
            .unwrap_or_else(|| panic!("phase '{}' missing from phase timings section", prev_name));
        let next_pos = timings_section
            .find(next_name)
            .unwrap_or_else(|| panic!("phase '{}' missing from phase timings section", next_name));
        assert!(
            prev_pos < next_pos,
            "'{}' (pos {}) must appear before '{}' (pos {}) in phase timings",
            prev_name, prev_pos, next_name, next_pos
        );
    }
}

#[test]
fn render_format_bytes_iec_labels() {
    use thinkingroot_cli::summary_printer;

    let case = |bytes: u64, expected: &str| {
        let summary = IncrementalSummary {
            bytes_re_extracted: bytes,
            ..Default::default()
        };
        let out = summary_printer::render(&summary);
        assert!(
            out.contains(expected),
            "bytes={bytes}: expected '{expected}' in output:\n{out}"
        );
    };

    case(0, "0 B");
    case(1023, "1023 B");
    case(1024, "1.00 KiB");
    case(1_500_000, "1.43 MiB");
    case(1_073_741_824, "1.00 GiB");
}

#[test]
fn render_output_contains_no_json_braces() {
    use thinkingroot_cli::summary_printer;
    let summary = make_summary();
    let out = summary_printer::render(&summary);
    assert!(
        !out.contains('{') && !out.contains('}'),
        "render() must produce plain ASCII table, not JSON; got:\n{out}"
    );
}
