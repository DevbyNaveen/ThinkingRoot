//! Clean-room reimplementation. Inspired by openhuman/tokenjuice/reduce.rs
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.1 (2026-05-17) — apply a rule's transform pipeline to a
//! single text stream (stdout OR stderr).
//!
//! Transforms run in order. Each returns a new `String`; no
//! transform mutates in place. The pipeline operates on lines (split
//! on `\n`); the final `String` is rejoined with `\n` and trailing
//! newline is preserved iff the input had one.

use super::rules::{Rule, Transform};

/// Apply a rule's transform pipeline. `failed = exit_code != 0` —
/// triggers wider head/tail windows when the rule has
/// `preserve_on_failure = true`.
pub(super) fn apply(input: &str, rule: &Rule, failed: bool) -> String {
    if input.is_empty() {
        return String::new();
    }
    let had_trailing_newline = input.ends_with('\n');
    // Split on '\n' preserves mid-stream blank lines (they appear as
    // empty entries). If the input ends with '\n' the final element
    // is also an empty entry which represents "nothing after the
    // trailing newline" — pop it so we don't double-count when we
    // rejoin and re-append the trailing newline below.
    let mut lines: Vec<String> = input.split('\n').map(|s| s.to_string()).collect();
    if had_trailing_newline && lines.last().is_some_and(|s| s.is_empty()) {
        lines.pop();
    }

    let preserve = rule.preserve_on_failure && failed;

    for t in rule.transforms {
        lines = apply_one(&lines, *t, preserve);
    }

    let mut out = lines.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    out
}

fn apply_one(lines: &[String], t: Transform, preserve: bool) -> Vec<String> {
    match t {
        Transform::StripAnsi => lines.iter().map(|l| strip_ansi(l)).collect(),
        Transform::DedupeAdjacent => dedupe_adjacent(lines),
        Transform::TrimBlankEdges => trim_blank_edges(lines),
        Transform::SkipContaining(needles) => lines
            .iter()
            .filter(|l| !needles.iter().any(|n| l.contains(n)))
            .cloned()
            .collect(),
        Transform::KeepContaining(needles) => lines
            .iter()
            .filter(|l| {
                let trimmed = l.trim();
                trimmed.is_empty() || needles.iter().any(|n| l.contains(n))
            })
            .cloned()
            .collect(),
        Transform::HeadTail { total, head, tail } => {
            head_tail(lines, total, head, tail, preserve)
        }
    }
}

/// Strip ANSI CSI escape sequences (`ESC [ ... m`, `ESC [ ... K`,
/// etc.). Handles the common SGR / colour / cursor-control set. Does
/// NOT strip OSC sequences (`ESC ]`) — those are rare in CLI output
/// and stripping them safely needs a full state machine.
///
/// Implementation: linear scan; whenever we see `ESC [`, advance
/// past parameter bytes (`0x30..=0x3F`) and intermediate bytes
/// (`0x20..=0x2F`), then drop the final byte (`0x40..=0x7E`).
fn strip_ansi(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2; // skip ESC [
            while i < bytes.len() {
                let b = bytes[i];
                i += 1;
                // Final byte is in 0x40..=0x7E.
                if (0x40..=0x7E).contains(&b) {
                    break;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // `from_utf8_lossy` is safe — we only drop ANSI escape bytes
    // which are 7-bit ASCII; the surrounding UTF-8 multibyte
    // sequences are untouched.
    String::from_utf8_lossy(&out).into_owned()
}

fn dedupe_adjacent(lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut dup_count = 0usize;
    for line in lines {
        if out.last() == Some(line) {
            dup_count += 1;
        } else {
            if dup_count > 0 {
                out.push(format!("  [... {dup_count} more duplicate line(s) ...]"));
                dup_count = 0;
            }
            out.push(line.clone());
        }
    }
    if dup_count > 0 {
        out.push(format!("  [... {dup_count} more duplicate line(s) ...]"));
    }
    out
}

fn trim_blank_edges(lines: &[String]) -> Vec<String> {
    let start = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(0);
    let end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);
    if start >= end {
        return Vec::new();
    }
    lines[start..end].to_vec()
}

fn head_tail(
    lines: &[String],
    total: usize,
    head: usize,
    tail: usize,
    preserve: bool,
) -> Vec<String> {
    if lines.len() <= total {
        return lines.to_vec();
    }
    let (h, t) = if preserve { (head * 2, tail * 2) } else { (head, tail) };
    if h + t >= lines.len() {
        // After doubling for `preserve` we'd take more than we have
        // — just return as-is.
        return lines.to_vec();
    }
    let elided = lines.len() - h - t;
    let elided_bytes: usize = lines[h..(lines.len() - t)].iter().map(|l| l.len() + 1).sum();
    let mut out: Vec<String> = Vec::with_capacity(h + t + 1);
    out.extend_from_slice(&lines[..h]);
    out.push(format!(
        "  [... {elided} lines elided ({elided_bytes} bytes) ...]"
    ));
    out.extend_from_slice(&lines[(lines.len() - t)..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenjuice::rules::ALL_RULES;

    fn rule(id: &str) -> &'static Rule {
        ALL_RULES
            .iter()
            .copied()
            .find(|r| r.id == id)
            .expect("rule must exist")
    }

    #[test]
    fn strip_ansi_drops_sgr_colour_codes() {
        let input = "\u{1b}[31merror\u{1b}[0m: something broke";
        let stripped = strip_ansi(input);
        assert_eq!(stripped, "error: something broke");
    }

    #[test]
    fn strip_ansi_is_identity_on_clean_text() {
        let input = "no escapes here, just text";
        assert_eq!(strip_ansi(input), input);
    }

    #[test]
    fn dedupe_collapses_adjacent_identical_lines() {
        let lines: Vec<String> = vec!["a", "a", "a", "b", "a"].iter().map(|s| s.to_string()).collect();
        let out = dedupe_adjacent(&lines);
        assert_eq!(out.len(), 4); // a, dup-marker, b, a
        assert_eq!(out[0], "a");
        assert!(out[1].contains("duplicate"));
        assert_eq!(out[2], "b");
        assert_eq!(out[3], "a");
    }

    #[test]
    fn dedupe_passes_through_non_duplicates() {
        let lines: Vec<String> = vec!["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let out = dedupe_adjacent(&lines);
        assert_eq!(out, lines);
    }

    #[test]
    fn head_tail_short_circuits_when_under_threshold() {
        let lines: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        let out = head_tail(&lines, 20, 5, 2, false);
        assert_eq!(out, lines);
    }

    #[test]
    fn head_tail_inserts_elision_marker_when_over_threshold() {
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let out = head_tail(&lines, 30, 10, 5, false);
        // 10 head + 1 elision + 5 tail = 16
        assert_eq!(out.len(), 16);
        assert!(out[10].contains("elided"));
        assert_eq!(out[0], "line 0");
        assert_eq!(out[15], "line 99");
    }

    #[test]
    fn head_tail_doubles_windows_when_preserve_on_failure() {
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let out = head_tail(&lines, 30, 10, 5, true);
        // 20 head + 1 elision + 10 tail = 31
        assert_eq!(out.len(), 31);
    }

    #[test]
    fn apply_preserves_trailing_newline_invariant() {
        let r = rule("generic.ansi@v1");
        let input_with_nl = "abc\ndef\n";
        let input_no_nl = "abc\ndef";
        assert!(apply(input_with_nl, r, false).ends_with('\n'));
        assert!(!apply(input_no_nl, r, false).ends_with('\n'));
    }

    #[test]
    fn apply_git_status_collapses_duplicates() {
        let r = rule("git.status@v1");
        let input = (0..40)
            .map(|_| "        modified:   src/lib.rs")
            .collect::<Vec<_>>()
            .join("\n");
        let out = apply(&input, r, false);
        // Compaction MUST shrink the byte count.
        assert!(out.len() < input.len(), "expected dedupe to shrink: orig={} out={}", input.len(), out.len());
    }

    #[test]
    fn apply_cargo_test_keeps_only_meaningful_lines() {
        let r = rule("cargo.test@v1");
        let input = "\
warning: unused import\n\
foo bar baz\n\
random noise line\n\
test result: ok. 5 passed; 0 failed\n\
another irrelevant line\n";
        let out = apply(input, r, false);
        assert!(out.contains("warning:"));
        assert!(out.contains("test result:"));
        assert!(!out.contains("random noise"));
        assert!(!out.contains("another irrelevant"));
    }
}
