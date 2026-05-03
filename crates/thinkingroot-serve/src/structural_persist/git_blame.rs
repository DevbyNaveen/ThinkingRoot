//! Git-blame emitter — Compile Completeness Contract §4.15.
//!
//! Per-line-range author attribution via `git blame --line-porcelain`.
//! One row per blame hunk. Default-enabled per contract §15 Q2;
//! opt-out via `[compile] git_blame = false` in `tr.toml` (the config
//! gate is read upstream — this module just emits when called).
//!
//! Shell-out matches the existing pattern in
//! `crates/thinkingroot-parse/src/git.rs:10` (no `git2` dep needed).
//! Errors are non-fatal — a non-tracked file or a git-less workspace
//! returns zero rows and the audit still passes via `chunks_residual`.

use std::path::Path;
use std::process::Command;

use thinkingroot_graph::{Blake3Cache, rows::GitBlameRow};

/// Blame-walk a single source whose URI maps to a tracked file. Returns
/// an empty vec for non-tracked files, non-git workspaces, or any
/// shell-out failure (logged at DEBUG).
pub(super) fn emit(
    repo_root: &Path,
    file_path: &Path,
    bytes: &[u8],
    source_id: &str,
    cache: &mut Blake3Cache,
    out: &mut Vec<GitBlameRow>,
) {
    // Compute byte offsets per line so each blame hunk's
    // (line_start, line_end) maps to a (byte_start, byte_end).
    // Phase 9 unions over git_blame so byte ranges must be authoritative.
    let line_byte_offsets = compute_line_byte_offsets(bytes);
    if line_byte_offsets.is_empty() {
        return;
    }

    let output = match Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["blame", "--line-porcelain"])
        .arg(file_path)
        .output()
    {
        Ok(out) if out.status.success() => out,
        Ok(_) => {
            tracing::debug!(
                file = %file_path.display(),
                "git blame: non-zero exit (file not tracked?)"
            );
            return;
        }
        Err(e) => {
            tracing::debug!(
                file = %file_path.display(),
                error = %e,
                "git blame: shell-out failed"
            );
            return;
        }
    };

    let blame_text = String::from_utf8_lossy(&output.stdout);
    let hunks = parse_porcelain(&blame_text);

    for hunk in hunks {
        let line_start = hunk.line_start;
        let line_end = line_start + hunk.lines - 1;
        // Map line range → byte range. line numbers are 1-indexed.
        let bs_idx = (line_start.saturating_sub(1) as usize).min(line_byte_offsets.len() - 1);
        let be_idx = (line_end as usize).min(line_byte_offsets.len() - 1);
        let byte_start = line_byte_offsets[bs_idx];
        let byte_end = line_byte_offsets[be_idx];
        if byte_end <= byte_start {
            continue;
        }
        let blake3_str = cache.get(byte_start, byte_end).to_string();
        out.push(GitBlameRow {
            source_id: source_id.to_string(),
            line_start,
            line_end,
            commit_sha: hunk.sha,
            author: hunk.author,
            author_email: hunk.author_email,
            blamed_at: hunk.blamed_at,
            byte_start,
            byte_end,
            content_blake3: blake3_str,
        });
    }
}

#[derive(Debug, Default)]
struct Hunk {
    sha: String,
    line_start: u32,
    lines: u32,
    author: String,
    author_email: String,
    blamed_at: f64,
}

/// Parse `git blame --line-porcelain` output. Format: each line of the
/// blamed file produces a header `<sha> <orig_line> <final_line>
/// <num_lines>` followed by metadata lines (`author`, `author-mail`,
/// `author-time`, etc.) and a single content line prefixed with `\t`.
/// Successive lines from the same hunk share the same sha but only the
/// first emits the metadata block.
fn parse_porcelain(text: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    let mut last_sha_meta: std::collections::HashMap<String, (String, String, f64)> =
        std::collections::HashMap::new();

    for line in text.lines() {
        if line.starts_with('\t') {
            // Content line — close out the current hunk if it has data.
            continue;
        }
        if let Some(rest) = line.strip_prefix("author ") {
            if let Some(h) = current.as_mut() {
                h.author = rest.to_string();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("author-mail ") {
            if let Some(h) = current.as_mut() {
                h.author_email = rest.trim_matches(|c| c == '<' || c == '>').to_string();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("author-time ") {
            if let Ok(ts) = rest.parse::<f64>() {
                if let Some(h) = current.as_mut() {
                    h.blamed_at = ts;
                }
            }
            continue;
        }
        // Header line: `<sha> <orig> <final> [<lines>]`. Tighten the
        // recognition so prose metadata lines like `summary <text>`
        // (which split into 2+ whitespace tokens but aren't hunk
        // headers) don't accidentally start a new hunk.
        let parts: Vec<&str> = line.split_whitespace().collect();
        let looks_like_header = parts.len() >= 3
            && parts[0].len() >= 7
            && parts[0].chars().all(|c| c.is_ascii_hexdigit())
            && parts[1].parse::<u32>().is_ok()
            && parts[2].parse::<u32>().is_ok();
        if looks_like_header {
            // Push previous hunk if any.
            if let Some(prev) = current.take() {
                if !prev.sha.is_empty() {
                    last_sha_meta.insert(
                        prev.sha.clone(),
                        (prev.author.clone(), prev.author_email.clone(), prev.blamed_at),
                    );
                }
                hunks.push(prev);
            }
            let sha = parts[0].to_string();
            let final_line: u32 = parts[2].parse().unwrap_or(0);
            let lines: u32 = parts.get(3).and_then(|p| p.parse().ok()).unwrap_or(1);
            // If we've seen this sha before, reuse its metadata (porcelain
            // emits metadata only on the first hunk per sha).
            let meta = last_sha_meta.get(&sha).cloned();
            current = Some(Hunk {
                sha,
                line_start: final_line,
                lines,
                author: meta.as_ref().map(|m| m.0.clone()).unwrap_or_default(),
                author_email: meta.as_ref().map(|m| m.1.clone()).unwrap_or_default(),
                blamed_at: meta.as_ref().map(|m| m.2).unwrap_or(0.0),
            });
        }
    }

    if let Some(prev) = current {
        if !prev.sha.is_empty() {
            hunks.push(prev);
        }
    }

    hunks
}

/// Cumulative byte offset at the start of each line. Index 0 = byte 0;
/// the final entry is `bytes.len()` (so an exclusive end-byte
/// computation works).
fn compute_line_byte_offsets(bytes: &[u8]) -> Vec<u64> {
    let mut offsets = Vec::with_capacity(bytes.len() / 40 + 1);
    offsets.push(0u64);
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            offsets.push((i + 1) as u64);
        }
    }
    if !bytes.is_empty() && *bytes.last().unwrap() != b'\n' {
        offsets.push(bytes.len() as u64);
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_byte_offsets_three_lines() {
        let bytes = b"abc\ndef\nghi";
        let offs = compute_line_byte_offsets(bytes);
        assert_eq!(offs, vec![0, 4, 8, 11]);
    }

    #[test]
    fn parse_porcelain_single_hunk() {
        let text = "abc1234567890abc1234567890abc1234567890abc12345 1 1 2\n\
                    author Alice\n\
                    author-mail <alice@example.com>\n\
                    author-time 1700000000\n\
                    summary First commit\n\
                    \tline 1\n\
                    abc1234567890abc1234567890abc1234567890abc12345 2 2\n\
                    \tline 2\n";
        let hunks = parse_porcelain(text);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].author, "Alice");
        assert_eq!(hunks[0].author_email, "alice@example.com");
        assert_eq!(hunks[0].lines, 2);
    }

    #[test]
    fn empty_blame_output_returns_no_hunks() {
        assert!(parse_porcelain("").is_empty());
    }
}
