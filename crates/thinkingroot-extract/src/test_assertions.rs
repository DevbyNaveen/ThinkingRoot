//! Test-assertion miner for the Witness Mesh rule catalog.
//!
//! Detects framework-specific assertion calls inside test-function
//! chunks and emits one Witness per assertion. The four supported
//! frameworks are cargo-test (Rust), pytest (Python), jest (JS/TS),
//! and junit (Java).
//!
//! Regex over tree-sitter walk: chosen to match the existing
//! `thinkingroot-serve::structural_persist::test_annotations`
//! convention. The mining is mechanical either way; regex is
//! ~95% accurate, has zero tree-sitter init cost per chunk, and the
//! 5% miss rate is for cases (assert! inside a macro, assert in a
//! comment) where a tree-sitter walk would also need scope filtering
//! to be honest. v1.1 may swap to tree-sitter queries for fan_in /
//! fan_out parity.
//!
//! "Is this a test function?" — the chunk must (a) be a
//! `ChunkType::FunctionDef` AND (b) carry a framework-specific
//! marker in its raw byte range (e.g. `#[test]` immediately before
//! the function header, `@Test` annotation for JUnit, `def test_*`
//! for pytest, `it(` / `test(` for jest). Without this gate, every
//! `assert_eq!` in non-test code would emit a Witness — including
//! production-side `debug_assert!` calls which we explicitly do not
//! want to claim are tests.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use thinkingroot_core::ir::{Chunk, ChunkType};
use thinkingroot_core::types::{
    Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
};

/// Framework dispatch + regex pair.
struct FrameworkRule {
    rule: &'static str,
    /// Marker that proves the enclosing function is a test under
    /// this framework. Searched in the chunk's raw bytes (file slice).
    marker: &'static Regex,
    /// Assertion call pattern. Each match becomes one Witness.
    assertion: &'static Regex,
    /// Language gate — match against `chunk.language` to skip
    /// frameworks that don't apply (e.g. don't try junit on Rust).
    languages: &'static [&'static str],
}

fn cargo_test_marker() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // `#[test]` or `#[tokio::test]` etc. immediately before the fn.
    R.get_or_init(|| Regex::new(r"#\[(?:[\w:]+::)?test(?:\([^)]*\))?\]").unwrap())
}
fn cargo_test_assertion() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\b(?:assert!|assert_eq!|assert_ne!|assert_matches!|panic!)\s*\(")
            .unwrap()
    })
}

fn pytest_marker() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // `def test_*` is the canonical pytest discovery convention.
    R.get_or_init(|| Regex::new(r"\bdef\s+test_\w+\s*\(").unwrap())
}
fn pytest_assertion() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // `assert <expr>` at indent-or-line-start, NOT followed by `_`
    // (avoid matching `assert_eq` which is non-Python).
    R.get_or_init(|| Regex::new(r"(?m)^[ \t]+assert(?:\s+|\()").unwrap())
}

fn jest_marker() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // `it("...", ...)` or `test("...", ...)` — the jest/mocha
    // discovery convention.
    R.get_or_init(|| Regex::new(r#"\b(?:it|test)\s*\(\s*['"`]"#).unwrap())
}
fn jest_assertion() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // `expect(...).<matcher>(...)`.
    R.get_or_init(|| Regex::new(r"\bexpect\s*\([^)]*\)\s*\.\w+").unwrap())
}

fn junit_marker() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // `@Test`, `@ParameterizedTest`, `@RepeatedTest` annotations.
    R.get_or_init(|| Regex::new(r"@(?:Test|ParameterizedTest|RepeatedTest)\b").unwrap())
}
fn junit_assertion() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // `Assert.assert*`, `Assertions.assert*`, `assertEquals`, etc.
    R.get_or_init(|| {
        Regex::new(
            r"\b(?:Assert(?:ions)?\.)?(?:assertEquals|assertTrue|assertFalse|assertNull|assertNotNull|assertThrows|assertArrayEquals|assertSame|assertNotSame|fail)\s*\(",
        )
        .unwrap()
    })
}

fn frameworks() -> [FrameworkRule; 4] {
    [
        FrameworkRule {
            rule: "cargo-test::assertion@v1",
            marker: cargo_test_marker(),
            assertion: cargo_test_assertion(),
            languages: &["rust"],
        },
        FrameworkRule {
            rule: "pytest::assertion@v1",
            marker: pytest_marker(),
            assertion: pytest_assertion(),
            languages: &["python"],
        },
        FrameworkRule {
            rule: "jest::assertion@v1",
            marker: jest_marker(),
            assertion: jest_assertion(),
            languages: &["javascript", "typescript"],
        },
        FrameworkRule {
            rule: "junit::assertion@v1",
            marker: junit_marker(),
            assertion: junit_assertion(),
            languages: &["java"],
        },
    ]
}

/// Extract assertion Witnesses from a single chunk.
///
/// Pre-conditions for emission:
/// - `chunk.chunk_type == FunctionDef`.
/// - `chunk.byte_start != chunk.byte_end` (authoritative range).
/// - `chunk.language` matches one of the framework's languages.
/// - The framework's `marker` regex matches the chunk's source bytes.
///
/// Each assertion match emits one Witness with:
/// - `rule = "<framework>::assertion@v1"`.
/// - `witness_type = "asserts::test"`.
/// - `span` covering just the assertion call (`assert_eq!(...)`).
/// - `content_blake3` of the matched span.
pub fn extract_witnesses_from_chunk(
    chunk: &Chunk,
    source_bytes: &[u8],
    file_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Vec<Witness> {
    if !matches!(chunk.chunk_type, ChunkType::FunctionDef) {
        return Vec::new();
    }
    if chunk.byte_start == 0 && chunk.byte_end == 0 {
        return Vec::new();
    }
    if chunk.byte_end <= chunk.byte_start {
        return Vec::new();
    }
    let Some(language) = chunk.language.as_deref() else {
        return Vec::new();
    };
    let start = chunk.byte_start as usize;
    let end = (chunk.byte_end as usize).min(source_bytes.len());
    if start >= source_bytes.len() {
        return Vec::new();
    }
    let window_bytes = &source_bytes[start..end];
    let window_text = std::str::from_utf8(window_bytes)
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or_else(|_| String::from_utf8_lossy(window_bytes));

    let mut out: Vec<Witness> = Vec::new();
    for fw in frameworks() {
        if !fw.languages.iter().any(|l| *l == language) {
            continue;
        }
        if !fw.marker.is_match(&window_text) {
            continue;
        }
        for m in fw.assertion.find_iter(&window_text) {
            let span_start = start + m.start();
            let span_end = start + m.end();
            if span_end > source_bytes.len() {
                continue;
            }
            let span = WitnessSpan {
                file_blake3: file_blake3.to_string(),
                start: span_start as u64,
                end: span_end as u64,
            };
            let content_blake3 = blake3::hash(&source_bytes[span_start..span_end])
                .to_hex()
                .to_string();
            let mut witness = Witness::new(
                fw.rule,
                "asserts::test",
                vec![WitnessInput::ByteRef {
                    file_blake3: file_blake3.to_string(),
                    start: span.start,
                    end: span.end,
                }],
                vec![span],
                source_id,
                workspace_id,
                Sensitivity::Public,
                Confidence::new(0.99),
                content_blake3,
                now,
            );
            if let Some(fn_name) = chunk.metadata.function_name.as_ref() {
                witness = witness.with_symbol(fn_name);
            }
            out.push(witness);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function_chunk(content: &str, language: &str, byte_start: u64, byte_end: u64) -> Chunk {
        let mut c = Chunk::new(content, ChunkType::FunctionDef, 1, 5);
        c.byte_start = byte_start;
        c.byte_end = byte_end;
        c.language = Some(language.to_string());
        c
    }

    #[test]
    fn cargo_test_assertions_are_extracted() {
        let source = "#[test]\nfn t() {\n    assert_eq!(1, 1);\n    assert!(true);\n}\n";
        let chunk = function_chunk(source, "rust", 0, source.len() as u64);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.len(), 2);
        for w in &out {
            assert_eq!(w.rule, "cargo-test::assertion@v1");
            assert_eq!(w.witness_type, "asserts::test");
        }
    }

    #[test]
    fn rust_function_without_test_marker_emits_nothing() {
        let source = "fn helper() {\n    assert_eq!(1, 1);\n}\n";
        let chunk = function_chunk(source, "rust", 0, source.len() as u64);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        // No `#[test]` → marker missing → no Witness. This is
        // intentional: production `assert_eq!` is not a test.
        assert!(out.is_empty());
    }

    #[test]
    fn pytest_assertions_are_extracted() {
        let source = "def test_addition():\n    assert 1 + 1 == 2\n    assert (3 - 1) == 2\n";
        let chunk = function_chunk(source, "python", 0, source.len() as u64);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].rule, "pytest::assertion@v1");
    }

    #[test]
    fn python_non_test_function_emits_nothing() {
        let source = "def add(a, b):\n    assert a > 0\n    return a + b\n";
        let chunk = function_chunk(source, "python", 0, source.len() as u64);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn jest_assertions_are_extracted() {
        let source = "it('adds', () => {\n  expect(1+1).toBe(2);\n  expect(true).toBeTruthy();\n});\n";
        let chunk = function_chunk(source, "javascript", 0, source.len() as u64);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].rule, "jest::assertion@v1");
    }

    #[test]
    fn typescript_jest_assertions_are_extracted() {
        let source = "test('eq', () => { expect(2).toBe(2); });\n";
        let chunk = function_chunk(source, "typescript", 0, source.len() as u64);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn junit_assertions_are_extracted() {
        let source = "@Test\nvoid foo() {\n  assertEquals(1, 1);\n  assertTrue(true);\n}\n";
        let chunk = function_chunk(source, "java", 0, source.len() as u64);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].rule, "junit::assertion@v1");
    }

    #[test]
    fn java_non_test_method_emits_nothing() {
        let source = "void helper() {\n  assertEquals(1, 1);\n}\n";
        let chunk = function_chunk(source, "java", 0, source.len() as u64);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn non_function_chunks_emit_nothing() {
        let source = "assert_eq!(1, 1);";
        let mut chunk = Chunk::new(source, ChunkType::Code, 1, 1);
        chunk.byte_start = 0;
        chunk.byte_end = source.len() as u64;
        chunk.language = Some("rust".into());
        let out = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        // Not a FunctionDef → step back even though the marker
        // would in theory match (rust assert_eq in raw code).
        assert!(out.is_empty());
    }

    #[test]
    fn cross_framework_chunk_routes_to_matching_language() {
        // A Rust chunk should never match jest patterns even if its
        // text accidentally contains "expect(".
        let source = "#[test]\nfn t() {\n    let x = expect(\"foo\");\n}\n";
        let chunk = function_chunk(source, "rust", 0, source.len() as u64);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        // No `assert!` in this function body → 0 Witnesses
        // (we deliberately don't emit a jest::assertion Witness
        // even though `expect(` is in the bytes — the language
        // gate keeps frameworks honest).
        assert!(out.is_empty());
    }

    #[test]
    fn symbol_attached_when_chunk_has_function_name() {
        let source = "#[test]\nfn test_thing() {\n    assert!(true);\n}\n";
        let mut chunk = function_chunk(source, "rust", 0, source.len() as u64);
        chunk.metadata.function_name = Some("test_thing".into());
        let out = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].symbol.as_deref(), Some("test_thing"));
    }
}
