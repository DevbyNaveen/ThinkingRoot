//! File-header annotation parser — Compile Completeness Contract §4.11.
//!
//! Emits one `source_annotations` row per recognised file-level pragma:
//! SPDX license identifier, copyright line, encoding pragma, shebang,
//! mode line. Plus the I-3 exception class
//! `kind = "trailing_newline_norm"` when `DocumentIR.trailing_newline_normalised`
//! is set (covers the byte the parser stripped from the end of file).
//!
//! Patterns are checked against the **first ~32 lines** of the source —
//! file headers virtually never extend further. The regex layer is
//! line-oriented to keep byte-range stamping precise.

use std::sync::OnceLock;

use regex::Regex;
use thinkingroot_core::ir::DocumentIR;
use thinkingroot_graph::{Blake3Cache, rows::SourceAnnotation};

use super::stable_row_id;

pub(super) fn emit(
    doc: &DocumentIR,
    bytes: &[u8],
    source_id: &str,
    cache: &mut Blake3Cache,
    out: &mut Vec<SourceAnnotation>,
) {
    if let Ok(text) = std::str::from_utf8(bytes) {
        scan_text(text, source_id, cache, out);
    }
    if doc.trailing_newline_normalised && !bytes.is_empty() {
        // Cover the last byte of the file with a trailing-newline-norm
        // annotation so I-3 byte coverage holds even when the parser
        // stripped a final newline.
        let bs = (bytes.len() - 1) as u64;
        let be = bytes.len() as u64;
        let id = stable_row_id("source_annotations", source_id, bs, be, "trailing_newline_norm");
        out.push(SourceAnnotation {
            id,
            source_id: source_id.to_string(),
            kind: "trailing_newline_norm".to_string(),
            value: "parser stripped trailing newline".to_string(),
            byte_start: bs,
            byte_end: be,
            content_blake3: cache.get(bs, be).to_string(),
        });
    }
}

fn scan_text(text: &str, source_id: &str, cache: &mut Blake3Cache, out: &mut Vec<SourceAnnotation>) {
    // Walk lines, tracking absolute byte offset. Stop after the first
    // 32 lines OR 4 KB scanned (whichever comes first) — file headers
    // never extend that far in practice.
    const MAX_LINES: usize = 32;
    const MAX_BYTES: usize = 4096;

    let mut byte_cursor: u64 = 0;
    for (line_idx, line) in text.split_inclusive('\n').enumerate() {
        if line_idx >= MAX_LINES || (byte_cursor as usize) > MAX_BYTES {
            break;
        }
        let line_len = line.len() as u64;
        // Strip the trailing '\n' for matching — we match against the
        // line content but stamp the byte range to include the newline
        // so adjacent lines don't have a 1-byte gap that would trip
        // Phase 9 (the chunk-byte-range a heading or claim covers
        // typically also includes its trailing newline).
        let content = line.trim_end_matches(['\r', '\n']);
        let content_len = content.len() as u64;
        let byte_start = byte_cursor;
        let byte_end = byte_cursor + content_len;

        if let Some((kind, value)) = recognise(content) {
            let id =
                stable_row_id("source_annotations", source_id, byte_start, byte_end, &kind);
            out.push(SourceAnnotation {
                id,
                source_id: source_id.to_string(),
                kind,
                value,
                byte_start,
                byte_end,
                content_blake3: cache.get(byte_start, byte_end).to_string(),
            });
        }
        byte_cursor += line_len;
    }
}

fn recognise(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if let Some(spdx) = spdx_regex().captures(trimmed) {
        return Some(("license".into(), spdx.get(1)?.as_str().trim().into()));
    }
    if let Some(c) = copyright_regex().captures(trimmed) {
        return Some(("copyright".into(), c.get(0)?.as_str().trim().into()));
    }
    if let Some(s) = shebang_regex().captures(trimmed) {
        return Some(("shebang".into(), s.get(0)?.as_str().into()));
    }
    if let Some(e) = encoding_regex().captures(trimmed) {
        return Some(("encoding".into(), e.get(1)?.as_str().into()));
    }
    if let Some(m) = mode_regex().captures(trimmed) {
        return Some(("mode".into(), m.get(1)?.as_str().into()));
    }
    None
}

fn spdx_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"SPDX-License-Identifier:\s*(.+)$").expect("valid spdx regex"))
}

fn copyright_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)copyright\s+(?:\(c\)|©)\s*\d{4}").expect("valid copyright regex")
    })
}

fn shebang_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^#!.+").expect("valid shebang regex"))
}

fn encoding_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // Python: `# -*- coding: utf-8 -*-` or `# encoding: utf-8`.
        Regex::new(r"#\s*-\*-\s*coding:\s*([^\s]+)\s*-\*-|#\s*encoding:\s*([^\s]+)")
            .expect("valid encoding regex")
    })
}

fn mode_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // Emacs/Vim mode line: `# -*- mode: rust -*-` or `// vim: set ft=rust:`.
        Regex::new(r"-\*-\s*mode:\s*([^\s]+)|vim:\s*set\s+ft=([^\s:]+)")
            .expect("valid mode regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Vec<SourceAnnotation> {
        let bytes = text.as_bytes().to_vec();
        let mut cache = Blake3Cache::new(&bytes);
        let mut out = Vec::new();
        scan_text(text, "src1", &mut cache, &mut out);
        out
    }

    #[test]
    fn detects_spdx_header() {
        let rows = run("// SPDX-License-Identifier: MIT\nuse std::fs;\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "license");
        assert_eq!(rows[0].value, "MIT");
    }

    #[test]
    fn detects_copyright_line() {
        let rows = run("// Copyright (c) 2026 ThinkingRoot\nuse std::fs;\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "copyright");
    }

    #[test]
    fn detects_shebang() {
        let rows = run("#!/usr/bin/env python3\nprint('hi')\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "shebang");
    }

    #[test]
    fn detects_encoding() {
        let rows = run("# -*- coding: utf-8 -*-\nprint('hi')\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "encoding");
        assert_eq!(rows[0].value, "utf-8");
    }

    #[test]
    fn no_header_returns_empty() {
        let rows = run("use std::fs;\nfn main() {}\n");
        assert!(rows.is_empty());
    }
}
