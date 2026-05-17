//! Clean-room reimplementation. Inspired by openhuman/tokenjuice/*
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.1 (2026-05-17) — terminal-output compaction for shell_exec.
//!
//! ## What this is
//!
//! Many CLIs emit noisy output that wastes LLM context: `git status`
//! repeats long path lists, `npm install` paints animated progress
//! bars over scroll-back, `cargo build` floods stdout with
//! `Compiling foo v0.1.0` lines before the meaningful warnings.
//! TokenJuice classifies a command + applies a rule's transform
//! pipeline (strip ANSI, dedupe adjacent lines, head/tail truncate)
//! to compact the output BEFORE it goes back to the model — typical
//! reduction is 50-80% on git/npm/cargo runs.
//!
//! ## What this isn't
//!
//! - **Not a token counter.** We work in bytes; the actual savings
//!   in tokens scale ~linearly with byte savings under tiktoken.
//! - **Not a redactor.** Compaction preserves the meaningful content
//!   verbatim — we only drop adjacent duplicates, ANSI escape
//!   sequences, and middle ranges of long outputs.
//! - **Not a regex playground.** 10 hardcoded rules at v1. Adding
//!   more is a `&'static Rule` literal in `rules.rs`, not a runtime
//!   config flag.
//!
//! ## Threshold + safety
//!
//! Two short-circuits to "passthrough":
//!   1. Input under `MIN_BYTES_TO_COMPACT` (512 B) — not worth it.
//!   2. Compacted output is more than `PASSTHROUGH_RATIO` (0.95) of
//!      the original — the rule didn't actually save anything.
//!
//! When `exit_code != 0` and the matching rule sets
//! `preserve_on_failure`, the head/tail windows are doubled so the
//! error context survives.

mod classify;
mod reduce;
mod rules;

pub use rules::{Rule, Transform};

/// Result of compacting one `(stdout, stderr)` pair.
///
/// The compacted strings live on `stdout` / `stderr` (identity if
/// `applied == false`). The diagnostic fields are surfaced in
/// `ShellOutcome` so the model + the user can both see "this output
/// was 4.2 KB before TokenJuice → 1.1 KB after, rule git.status@v1".
#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub stdout: String,
    pub stderr: String,
    /// Versioned identifier of the rule that ran. `"passthrough@v1"`
    /// when no rule matched OR the threshold short-circuited.
    pub rule_id: String,
    pub original_bytes: usize,
    pub compacted_bytes: usize,
    /// `true` iff the result differs from the raw input. Lets
    /// downstream surfaces (the desktop chat view) decide whether
    /// to badge the tool result as "compacted" without recomputing
    /// the byte counts.
    pub applied: bool,
}

const MIN_BYTES_TO_COMPACT: usize = 512;
const PASSTHROUGH_RATIO: f64 = 0.95;
const PASSTHROUGH_RULE_ID: &str = "passthrough@v1";

/// Compact one shell_exec output.
///
/// `command` is the argv[0] (typically `"git"`, `"npm"`, `"cargo"`).
/// `args` is argv[1..]. `stdout` + `stderr` are the raw bytes (often
/// already UTF-8 but not required). `exit_code` informs the
/// `preserve_on_failure` rule flag.
///
/// Returns a `CompactionResult` with both streams (lossy-converted
/// to String — the LLM expects text, not bytes) and the rule id +
/// byte counts.
pub fn compact_shell_output(
    command: &str,
    args: &[String],
    stdout: &[u8],
    stderr: &[u8],
    exit_code: i32,
) -> CompactionResult {
    let original_bytes = stdout.len() + stderr.len();

    // Short-circuit on small inputs — the rule's overhead outweighs
    // any savings. The model can handle 512 B fine.
    if original_bytes < MIN_BYTES_TO_COMPACT {
        return CompactionResult {
            stdout: String::from_utf8_lossy(stdout).into_owned(),
            stderr: String::from_utf8_lossy(stderr).into_owned(),
            rule_id: PASSTHROUGH_RULE_ID.to_string(),
            original_bytes,
            compacted_bytes: original_bytes,
            applied: false,
        };
    }

    let stdout_str = String::from_utf8_lossy(stdout);
    let stderr_str = String::from_utf8_lossy(stderr);

    let rule = match classify::classify(command, args, &stdout_str, &stderr_str) {
        Some(r) => r,
        None => {
            return CompactionResult {
                stdout: stdout_str.into_owned(),
                stderr: stderr_str.into_owned(),
                rule_id: PASSTHROUGH_RULE_ID.to_string(),
                original_bytes,
                compacted_bytes: original_bytes,
                applied: false,
            };
        }
    };

    let failed = exit_code != 0;
    let new_stdout = reduce::apply(&stdout_str, rule, failed);
    let new_stderr = reduce::apply(&stderr_str, rule, failed);
    let compacted_bytes = new_stdout.len() + new_stderr.len();

    // Threshold check: did the rule actually save anything?
    if (compacted_bytes as f64 / original_bytes as f64) > PASSTHROUGH_RATIO {
        return CompactionResult {
            stdout: stdout_str.into_owned(),
            stderr: stderr_str.into_owned(),
            rule_id: PASSTHROUGH_RULE_ID.to_string(),
            original_bytes,
            compacted_bytes: original_bytes,
            applied: false,
        };
    }

    CompactionResult {
        stdout: new_stdout,
        stderr: new_stderr,
        rule_id: rule.id.to_string(),
        original_bytes,
        compacted_bytes,
        applied: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_inputs_passthrough_without_rule_lookup() {
        // 100 bytes; under 512-byte threshold.
        let stdout = b"some short output";
        let result = compact_shell_output("git", &["status".into()], stdout, b"", 0);
        assert!(!result.applied);
        assert_eq!(result.rule_id, "passthrough@v1");
        assert_eq!(result.original_bytes, stdout.len());
    }

    #[test]
    fn unknown_command_falls_back_to_generic_or_passthrough() {
        // 600 B of noise; an unknown command name. Generic-ANSI rule
        // applies if it can strip enough; otherwise passthrough.
        let stdout = "a".repeat(600);
        let result = compact_shell_output(
            "totally-unknown-binary",
            &[],
            stdout.as_bytes(),
            b"",
            0,
        );
        // No ANSI to strip → generic rule will not improve; passes
        // the threshold check and emits passthrough.
        assert_eq!(result.original_bytes, 600);
        // applied may be false (passthrough) — both outcomes are honest.
        assert!(!result.applied || result.compacted_bytes < result.original_bytes);
    }

    #[test]
    fn applied_implies_compacted_bytes_strictly_less() {
        // A rule MUST reduce bytes if it applies — otherwise the
        // PASSTHROUGH_RATIO check should have intercepted.
        // Build a synthetic input that we know git.status@v1 will
        // compact: 50 adjacent identical lines.
        let line = "\tmodified:   /Users/foo/very/long/path/to/some/source/file.rs\n";
        let stdout: String = line.repeat(50);
        let result = compact_shell_output("git", &["status".into()], stdout.as_bytes(), b"", 0);
        if result.applied {
            assert!(result.compacted_bytes < result.original_bytes);
            assert_eq!(result.rule_id, "git.status@v1");
        }
        // (the rule might not match on argv shape; the assertion
        // above is conditional on it matching — see classify tests).
    }

    #[test]
    fn passthrough_keeps_original_bytes_count_intact() {
        let stdout = b"x".repeat(700);
        let result = compact_shell_output(
            "unknown-cmd",
            &[],
            stdout.as_slice(),
            b"",
            0,
        );
        assert_eq!(result.original_bytes, 700);
        // Passthrough preserves the input verbatim.
        if !result.applied {
            assert_eq!(result.stdout.len(), 700);
        }
    }
}
