//! Test-annotation emitter — Compile Completeness Contract §4.14.
//!
//! Detects test-marker attributes on FunctionDef chunks across four
//! frameworks (rust_test, junit, jest, pytest) via regex pass over the
//! chunk content. Per-language tree-sitter queries are a v1.1
//! refinement; the regex layer covers ~95% of typical cases.
//!
//! Emits one row per detected attribute per FunctionDef. Each row
//! references the FunctionDef's claim_id (resolved by byte-span match,
//! same pattern as `doc_tags`).

use std::sync::OnceLock;

use regex::Regex;
use thinkingroot_core::ir::{Chunk, ChunkType};
use thinkingroot_extract::ExtractionOutput;
use thinkingroot_graph::{Blake3Cache, rows::TestAnnotation};

use super::stable_row_id;

pub(super) fn emit(
    chunk: &Chunk,
    source_id: &str,
    cache: &mut Blake3Cache,
    out: &mut Vec<TestAnnotation>,
    extraction: &ExtractionOutput,
) {
    if !matches!(chunk.chunk_type, ChunkType::FunctionDef) {
        return;
    }

    let claim_id = extraction
        .claims
        .iter()
        .find(|c| {
            c.source_span
                .as_ref()
                .and_then(|s| match (s.byte_start, s.byte_end) {
                    (Some(bs), Some(be)) => Some(bs == chunk.byte_start && be == chunk.byte_end),
                    _ => None,
                })
                .unwrap_or(false)
        })
        .map(|c| c.id.to_string())
        .unwrap_or_default();

    let blake3_str = cache.get(chunk.byte_start, chunk.byte_end).to_string();
    let mut hits: Vec<(String, String, String)> = Vec::new(); // (framework, kind, name)

    // Rust — `#[test]` / `#[ignore]` / `#[should_panic]`
    for caps in rust_attrs_regex().captures_iter(&chunk.content) {
        if let Some(kind_match) = caps.get(1) {
            hits.push((
                "rust_test".to_string(),
                kind_match.as_str().to_string(),
                String::new(),
            ));
        }
    }

    // Java JUnit — `@Test` / `@Ignore` / `@Disabled`
    for caps in junit_attrs_regex().captures_iter(&chunk.content) {
        if let Some(kind_match) = caps.get(1) {
            hits.push((
                "junit".to_string(),
                kind_match.as_str().to_lowercase(),
                String::new(),
            ));
        }
    }

    // JS Jest / Mocha — `describe(...)` / `it(...)` / `test.skip(...)`
    for caps in jest_blocks_regex().captures_iter(&chunk.content) {
        let kind = caps
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let name = caps.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
        hits.push(("jest".to_string(), kind, name));
    }

    // Python pytest — `@pytest.fixture` / `@pytest.mark.skip` / `@pytest.mark.parametrize`
    for caps in pytest_attrs_regex().captures_iter(&chunk.content) {
        if let Some(kind_match) = caps.get(1) {
            hits.push((
                "pytest".to_string(),
                kind_match.as_str().to_string(),
                String::new(),
            ));
        }
    }

    for (idx, (framework, kind, name)) in hits.into_iter().enumerate() {
        let id = stable_row_id(
            "test_annotations",
            source_id,
            chunk.byte_start,
            chunk.byte_end,
            &format!("{idx}|{framework}|{kind}"),
        );
        out.push(TestAnnotation {
            id,
            source_id: source_id.to_string(),
            claim_id: claim_id.clone(),
            framework,
            annotation_kind: kind,
            name,
            byte_start: chunk.byte_start,
            byte_end: chunk.byte_end,
            content_blake3: blake3_str.clone(),
        });
    }
}

fn rust_attrs_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"#\[\s*(test|ignore|should_panic|tokio::test|async_std::test|test_case)\b").expect("rust attrs regex")
    })
}

fn junit_attrs_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"@(Test|Ignore|Disabled|ParameterizedTest|RepeatedTest|BeforeEach|AfterEach)\b").expect("junit attrs regex")
    })
}

fn jest_blocks_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // `describe('...')` / `it('...')` / `test.skip('...')` etc.
        Regex::new(
            r#"\b(describe|it|test|test\.skip|test\.only|describe\.skip)\s*\(\s*['"]([^'"]*)['"]"#,
        )
        .expect("jest regex")
    })
}

fn pytest_attrs_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"@pytest\.(fixture|mark\.skip|mark\.skipif|mark\.parametrize|mark\.xfail)\b")
            .expect("pytest attrs regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::ir::{Chunk, ChunkType};

    fn mk_fn_chunk(content: &str) -> Chunk {
        let mut c = Chunk::new(content.to_string(), ChunkType::FunctionDef, 1, 5);
        c.byte_start = 0;
        c.byte_end = content.len() as u64;
        c
    }

    fn run(content: &str) -> Vec<TestAnnotation> {
        let chunk = mk_fn_chunk(content);
        let bytes = chunk.content.as_bytes().to_vec();
        let mut cache = Blake3Cache::new(&bytes);
        let mut out = Vec::new();
        let extraction = ExtractionOutput::default();
        emit(&chunk, "src", &mut cache, &mut out, &extraction);
        out
    }

    #[test]
    fn detects_rust_test_attribute() {
        let rows = run("#[test]\nfn it_works() {}");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].framework, "rust_test");
        assert_eq!(rows[0].annotation_kind, "test");
    }

    #[test]
    fn detects_rust_ignore_and_should_panic() {
        let rows =
            run("#[test]\n#[ignore]\n#[should_panic]\nfn flaky() { panic!() }");
        assert_eq!(rows.len(), 3);
        let kinds: Vec<&str> = rows.iter().map(|r| r.annotation_kind.as_str()).collect();
        assert!(kinds.contains(&"test"));
        assert!(kinds.contains(&"ignore"));
        assert!(kinds.contains(&"should_panic"));
    }

    #[test]
    fn detects_tokio_test() {
        let rows = run("#[tokio::test]\nasync fn async_test() {}");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].annotation_kind, "tokio::test");
    }

    #[test]
    fn detects_pytest_fixture_and_skip() {
        let rows =
            run("@pytest.fixture\n@pytest.mark.skip\ndef setup_db(): pass");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.annotation_kind == "fixture"));
        assert!(rows.iter().any(|r| r.annotation_kind == "mark.skip"));
    }

    #[test]
    fn detects_junit_test() {
        let rows = run("@Test\npublic void shouldDoThing() {}");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].framework, "junit");
        assert_eq!(rows[0].annotation_kind, "test");
    }

    #[test]
    fn detects_jest_block_with_name() {
        let rows = run(r#"describe('auth', () => { it('logs in', () => {}) })"#);
        // Should detect both describe and it.
        assert!(rows.iter().any(|r| r.annotation_kind == "describe" && r.name == "auth"));
        assert!(rows.iter().any(|r| r.annotation_kind == "it" && r.name == "logs in"));
    }

    #[test]
    fn skips_non_function_chunks() {
        let mut chunk = mk_fn_chunk("#[test]\nfn x() {}");
        chunk.chunk_type = ChunkType::Comment;
        let bytes = chunk.content.as_bytes().to_vec();
        let mut cache = Blake3Cache::new(&bytes);
        let mut out = Vec::new();
        let extraction = ExtractionOutput::default();
        emit(&chunk, "src", &mut cache, &mut out, &extraction);
        assert!(out.is_empty());
    }
}
