//! The Witness Mesh rule catalog — v1.0.
//!
//! Every Witness is the deterministic output of a named rule. This
//! module is the canonical registry: it owns the rule descriptors,
//! the catalog version, and the deterministic TOML serializer used by
//! the pack writer.
//!
//! Identity of a rule is `(name, version)`. The `@vN` suffix is part
//! of the rule name — `tree-sitter::function-decl@v1` is a different
//! rule from `@v2` (different inputs, different output, or different
//! deterministic body).
//!
//! Build-time grammar pinning: `build.rs` reads Cargo.lock and emits
//! one `pub const TREE_SITTER_<LANG>_VERSION: &str` per grammar. A
//! grammar bump → lockfile change → catalog change → pack hash change.
//! Reproducibility is end-to-end.
//!
//! Catalog TOML format (`rule_catalog.toml` in every pack):
//!
//! ```text
//! catalog_version = "1.0.0"
//! catalog_blake3 = "<self-hash>"
//!
//! [rules."tree-sitter::function-decl@v1"]
//! family = "tree-sitter"
//! input_types = ["raw_bytes"]
//! output_type = "declares::function"
//! default_confidence = 0.99
//! default_sensitivity = "Public"
//! description = "..."
//! ```

use std::fmt::Write as _;

// Pulls in TREE_SITTER_<LANG>_VERSION constants from build.rs.
include!(concat!(env!("OUT_DIR"), "/grammar_versions.rs"));

/// Catalog SemVer. Bump:
/// - patch: bug fix in an existing rule's apply body (output bytes
///   unchanged for the same input — invisible to consumers).
/// - minor: new rules added (old packs still readable).
/// - major: breaking changes to existing rule outputs (old packs
///   need migration).
///
/// `1.1.0` — added image-family rules for mathematical (no-LLM)
/// extraction of image content: perceptual hash, color histogram,
/// edge summary, EXIF metadata, dominant colors. Pure-Rust
/// deterministic; no shell-outs.
pub const CATALOG_VERSION: &str = "1.1.0";

/// One rule in the catalog. The `'static` lifetime everywhere is
/// load-bearing: descriptors live in a `phf` static map and the
/// no-allocation property is what lets `rule_catalog_toml()` produce
/// byte-identical output across processes.
///
/// Descriptors are write-only via the static `RULE_CATALOG` map; the
/// pack-read path consumes the serialized TOML into a separate
/// owned-string `RuleCatalogTomlSchema` defined in
/// `crates/tr-format/src/rule_catalog.rs`. We deliberately keep this
/// in-engine type and the on-disk type as separate Rust types — they
/// have different lifetime obligations (this one is `'static`; the
/// disk one owns its strings).
#[derive(Debug, Clone, Copy)]
pub struct RuleDescriptor {
    /// Versioned rule name, e.g. `"tree-sitter::function-decl@v1"`.
    pub name: &'static str,
    /// Rule family, e.g. `"tree-sitter"`, `"lsp"`, `"rustdoc"`,
    /// `"markdown"`, `"comment"`, `"manifest"`, `"toml"`, `"json"`,
    /// `"yaml"`, `"csv"`, `"git"`, `"code"`, `"cargo-test"`,
    /// `"pytest"`, `"jest"`, `"junit"`.
    pub family: &'static str,
    /// What this rule consumes — either `"raw_bytes"` or the
    /// Witness types this rule joins against. A rule with
    /// `input_types = &["raw_bytes", "declares::function"]` is a
    /// "leaf" rule that decorates an existing function-decl
    /// Witness with derived information (e.g. its rustdoc summary).
    pub input_types: &'static [&'static str],
    /// What this rule produces — a single Witness type string.
    pub output_type: &'static str,
    /// Programming language scope. `None` for language-agnostic
    /// rules (markdown, csv, etc.) and for tree-sitter rules whose
    /// per-language dispatch happens at apply time from the chunk's
    /// `Chunk.language` field.
    pub language: Option<&'static str>,
    /// Pinned grammar version (from `build.rs`/Cargo.lock).
    /// `None` for non-tree-sitter rules.
    pub grammar_version: Option<&'static str>,
    /// Static confidence every Witness this rule emits inherits.
    /// Tree-sitter / LSP / rustdoc rules: 0.99 (mechanical, no
    /// hallucination surface). Opt-in `comment::@claim` rules:
    /// 0.95 (the author may be wrong, but the extraction itself
    /// is exact). No rule carries 1.0 — no rule is infallible.
    pub default_confidence: f64,
    /// Sensitivity label every Witness this rule emits inherits.
    /// PascalCase form to match `Sensitivity::as_str()`.
    pub default_sensitivity: &'static str,
    /// Human-readable one-liner.
    pub description: &'static str,
}

/// The v1.0 static catalog. 55 rules across 11 families.
///
/// Order is alphabetical by name; the canonical TOML emitter
/// re-sorts on its own pass so the macro-call order does not affect
/// the pack hash.
pub static RULE_CATALOG: phf::Map<&'static str, RuleDescriptor> = phf::phf_map! {
    // ── Code rules (tree-sitter family) — language-agnostic dispatch ──
    "tree-sitter::function-decl@v1" => RuleDescriptor {
        name: "tree-sitter::function-decl@v1",
        family: "tree-sitter",
        input_types: &["raw_bytes"],
        output_type: "declares::function",
        language: None,
        grammar_version: None, // language-agnostic; specific grammar resolved at apply time
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Function declarations via tree-sitter AST",
    },
    "tree-sitter::type-decl@v1" => RuleDescriptor {
        name: "tree-sitter::type-decl@v1",
        family: "tree-sitter",
        input_types: &["raw_bytes"],
        output_type: "declares::type",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Type/class/trait/interface declarations via tree-sitter AST",
    },
    "tree-sitter::module-decl@v1" => RuleDescriptor {
        name: "tree-sitter::module-decl@v1",
        family: "tree-sitter",
        input_types: &["raw_bytes"],
        output_type: "declares::module",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Module/package declarations via tree-sitter AST",
    },
    "tree-sitter::call-expr@v1" => RuleDescriptor {
        name: "tree-sitter::call-expr@v1",
        family: "tree-sitter",
        input_types: &["raw_bytes"],
        output_type: "calls",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Function/method call sites via tree-sitter AST",
    },
    "tree-sitter::import-decl@v1" => RuleDescriptor {
        name: "tree-sitter::import-decl@v1",
        family: "tree-sitter",
        input_types: &["raw_bytes"],
        output_type: "imports",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Import/use/include declarations via tree-sitter AST",
    },
    "tree-sitter::field-access@v1" => RuleDescriptor {
        name: "tree-sitter::field-access@v1",
        family: "tree-sitter",
        input_types: &["raw_bytes"],
        output_type: "accesses::field",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Field/property access sites via tree-sitter AST",
    },
    "tree-sitter::method-call@v1" => RuleDescriptor {
        name: "tree-sitter::method-call@v1",
        family: "tree-sitter",
        input_types: &["raw_bytes"],
        output_type: "invokes::method",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Method invocations via tree-sitter AST",
    },
    "tree-sitter::macro-invocation@v1" => RuleDescriptor {
        name: "tree-sitter::macro-invocation@v1",
        family: "tree-sitter",
        input_types: &["raw_bytes"],
        output_type: "invokes::macro",
        language: None, // applies for rust, c, cpp via per-language dispatch
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Macro invocations via tree-sitter AST (Rust/C/C++)",
    },
    "tree-sitter::unsafe-block@v1" => RuleDescriptor {
        name: "tree-sitter::unsafe-block@v1",
        family: "tree-sitter",
        input_types: &["raw_bytes"],
        output_type: "code::unsafe-region",
        language: Some("rust"),
        grammar_version: Some(TREE_SITTER_RUST_VERSION),
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "`unsafe { }` regions via tree-sitter AST",
    },
    "tree-sitter::test-fn@v1" => RuleDescriptor {
        name: "tree-sitter::test-fn@v1",
        family: "tree-sitter",
        input_types: &["raw_bytes"],
        output_type: "declares::test-fn",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Test functions (`#[test]`, `it(...)`, `def test_*`) via tree-sitter AST",
    },

    // ── LSP rules ──
    "lsp::type-of@v1" => RuleDescriptor {
        name: "lsp::type-of@v1",
        family: "lsp",
        input_types: &["raw_bytes", "declares::function"],
        output_type: "lsp::type-of",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Resolved type of an expression via LSP textDocument/hover",
    },
    "lsp::definition-of@v1" => RuleDescriptor {
        name: "lsp::definition-of@v1",
        family: "lsp",
        input_types: &["raw_bytes", "calls"],
        output_type: "lsp::definition-of",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Definition site for a symbol reference via LSP textDocument/definition",
    },
    "lsp::callers-of@v1" => RuleDescriptor {
        name: "lsp::callers-of@v1",
        family: "lsp",
        input_types: &["declares::function"],
        output_type: "lsp::callers-of",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Incoming-call sites for a function via LSP callHierarchy/incomingCalls",
    },
    "lsp::implementations-of@v1" => RuleDescriptor {
        name: "lsp::implementations-of@v1",
        family: "lsp",
        input_types: &["declares::type"],
        output_type: "lsp::implementations-of",
        language: Some("rust"),
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Trait implementations via LSP textDocument/implementation (rust-analyzer)",
    },
    "lsp::documentation-of@v1" => RuleDescriptor {
        name: "lsp::documentation-of@v1",
        family: "lsp",
        input_types: &["declares::function", "declares::type"],
        output_type: "lsp::documentation-of",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Hover documentation for a symbol via LSP textDocument/hover",
    },
    "lsp::skipped@v1" => RuleDescriptor {
        name: "lsp::skipped@v1",
        family: "lsp",
        input_types: &["raw_bytes"],
        output_type: "lsp::skipped",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Marker Witness emitted per source when no LSP backend is available — preserves mesh honesty",
    },

    // ── Documentation rules ──
    "rustdoc::function-summary@v1" => RuleDescriptor {
        name: "rustdoc::function-summary@v1",
        family: "rustdoc",
        input_types: &["raw_bytes", "declares::function"],
        output_type: "documents::function-summary",
        language: Some("rust"),
        grammar_version: Some(TREE_SITTER_RUST_VERSION),
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Rustdoc `///` summary line attached to a function declaration",
    },
    "rustdoc::param-doc@v1" => RuleDescriptor {
        name: "rustdoc::param-doc@v1",
        family: "rustdoc",
        input_types: &["raw_bytes", "declares::function"],
        output_type: "documents::param-doc",
        language: Some("rust"),
        grammar_version: Some(TREE_SITTER_RUST_VERSION),
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Rustdoc `# Arguments` parameter documentation",
    },
    "rustdoc::example-block@v1" => RuleDescriptor {
        name: "rustdoc::example-block@v1",
        family: "rustdoc",
        input_types: &["raw_bytes", "declares::function"],
        output_type: "documents::example",
        language: Some("rust"),
        grammar_version: Some(TREE_SITTER_RUST_VERSION),
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Rustdoc `# Examples` code-block extraction",
    },
    "jsdoc::function-summary@v1" => RuleDescriptor {
        name: "jsdoc::function-summary@v1",
        family: "jsdoc",
        input_types: &["raw_bytes", "declares::function"],
        output_type: "documents::function-summary",
        language: None, // applies for js + ts
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "JSDoc `/** ... */` summary attached to a function declaration",
    },
    "jsdoc::param@v1" => RuleDescriptor {
        name: "jsdoc::param@v1",
        family: "jsdoc",
        input_types: &["raw_bytes", "declares::function"],
        output_type: "documents::param-doc",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "JSDoc `@param` parameter documentation",
    },
    "javadoc::summary@v1" => RuleDescriptor {
        name: "javadoc::summary@v1",
        family: "javadoc",
        input_types: &["raw_bytes", "declares::function"],
        output_type: "documents::function-summary",
        language: Some("java"),
        grammar_version: Some(TREE_SITTER_JAVA_VERSION),
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Javadoc `/** ... */` summary attached to a method declaration",
    },
    "javadoc::param@v1" => RuleDescriptor {
        name: "javadoc::param@v1",
        family: "javadoc",
        input_types: &["raw_bytes", "declares::function"],
        output_type: "documents::param-doc",
        language: Some("java"),
        grammar_version: Some(TREE_SITTER_JAVA_VERSION),
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Javadoc `@param` parameter documentation",
    },
    "markdown::heading@v1" => RuleDescriptor {
        name: "markdown::heading@v1",
        family: "markdown",
        input_types: &["raw_bytes"],
        output_type: "documents::heading",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Markdown H1–H6 headings via pulldown-cmark AST",
    },
    "markdown::paragraph@v1" => RuleDescriptor {
        name: "markdown::paragraph@v1",
        family: "markdown",
        input_types: &["raw_bytes"],
        output_type: "documents::paragraph",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Markdown paragraph blocks",
    },
    "markdown::list-item@v1" => RuleDescriptor {
        name: "markdown::list-item@v1",
        family: "markdown",
        input_types: &["raw_bytes"],
        output_type: "documents::list-item",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Markdown list items (ordered + unordered)",
    },
    "markdown::link@v1" => RuleDescriptor {
        name: "markdown::link@v1",
        family: "markdown",
        input_types: &["raw_bytes"],
        output_type: "documents::link",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Markdown outbound links `[text](url)`",
    },
    "markdown::code-block@v1" => RuleDescriptor {
        name: "markdown::code-block@v1",
        family: "markdown",
        input_types: &["raw_bytes"],
        output_type: "documents::code-block",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Markdown fenced code blocks (with language hint)",
    },

    // ── Test-assertion rules ──
    "cargo-test::assertion@v1" => RuleDescriptor {
        name: "cargo-test::assertion@v1",
        family: "cargo-test",
        input_types: &["raw_bytes", "declares::test-fn"],
        output_type: "asserts::test",
        language: Some("rust"),
        grammar_version: Some(TREE_SITTER_RUST_VERSION),
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "`assert!`, `assert_eq!`, `assert_matches!` invocations in `#[test]` bodies",
    },
    "pytest::assertion@v1" => RuleDescriptor {
        name: "pytest::assertion@v1",
        family: "pytest",
        input_types: &["raw_bytes", "declares::test-fn"],
        output_type: "asserts::test",
        language: Some("python"),
        grammar_version: Some(TREE_SITTER_PYTHON_VERSION),
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "`assert <expr>` statements in pytest test functions",
    },
    "jest::assertion@v1" => RuleDescriptor {
        name: "jest::assertion@v1",
        family: "jest",
        input_types: &["raw_bytes", "declares::test-fn"],
        output_type: "asserts::test",
        language: None, // applies for js + ts
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "`expect(...).to<Matcher>(...)` calls inside `it(...)` blocks",
    },
    "junit::assertion@v1" => RuleDescriptor {
        name: "junit::assertion@v1",
        family: "junit",
        input_types: &["raw_bytes", "declares::test-fn"],
        output_type: "asserts::test",
        language: Some("java"),
        grammar_version: Some(TREE_SITTER_JAVA_VERSION),
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "`Assert.assert*` and `Assertions.assert*` in `@Test`-annotated methods",
    },

    // ── Structural data rules ──
    "toml::table@v1" => RuleDescriptor {
        name: "toml::table@v1",
        family: "toml",
        input_types: &["raw_bytes"],
        output_type: "declares::config-table",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "TOML `[table]` headers",
    },
    "toml::value@v1" => RuleDescriptor {
        name: "toml::value@v1",
        family: "toml",
        input_types: &["raw_bytes", "declares::config-table"],
        output_type: "declares::config-value",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "TOML key-value assignments inside a table",
    },
    "json::object@v1" => RuleDescriptor {
        name: "json::object@v1",
        family: "json",
        input_types: &["raw_bytes"],
        output_type: "declares::data-object",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "JSON object literals",
    },
    "json::array@v1" => RuleDescriptor {
        name: "json::array@v1",
        family: "json",
        input_types: &["raw_bytes"],
        output_type: "declares::data-array",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "JSON array literals",
    },
    "json::value@v1" => RuleDescriptor {
        name: "json::value@v1",
        family: "json",
        input_types: &["raw_bytes"],
        output_type: "declares::data-value",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "JSON scalar values (string/number/bool/null) at leaf positions",
    },
    "yaml::map@v1" => RuleDescriptor {
        name: "yaml::map@v1",
        family: "yaml",
        input_types: &["raw_bytes"],
        output_type: "declares::data-object",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "YAML mapping nodes",
    },
    "yaml::seq@v1" => RuleDescriptor {
        name: "yaml::seq@v1",
        family: "yaml",
        input_types: &["raw_bytes"],
        output_type: "declares::data-array",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "YAML sequence nodes",
    },
    "csv::row@v1" => RuleDescriptor {
        name: "csv::row@v1",
        family: "csv",
        input_types: &["raw_bytes"],
        output_type: "declares::data-row",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "CSV row records (one Witness per non-header row)",
    },
    "csv::column@v1" => RuleDescriptor {
        name: "csv::column@v1",
        family: "csv",
        input_types: &["raw_bytes"],
        output_type: "declares::data-column",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "CSV column headers (one Witness per header field)",
    },
    "manifest::dependency@v1" => RuleDescriptor {
        name: "manifest::dependency@v1",
        family: "manifest",
        input_types: &["raw_bytes"],
        output_type: "depends_on",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Project dependency declarations from Cargo.toml/package.json/go.mod/requirements.txt",
    },

    // ── Opt-in claim rules (comment family) ──
    "comment::@claim@v1" => RuleDescriptor {
        name: "comment::@claim@v1",
        family: "comment",
        input_types: &["raw_bytes"],
        output_type: "claim::@claim",
        language: None,
        grammar_version: None,
        default_confidence: 0.95,
        default_sensitivity: "Public",
        description: "Opt-in `@claim <text>` assertion in a comment",
    },
    "comment::@invariant@v1" => RuleDescriptor {
        name: "comment::@invariant@v1",
        family: "comment",
        input_types: &["raw_bytes"],
        output_type: "claim::@invariant",
        language: None,
        grammar_version: None,
        default_confidence: 0.95,
        default_sensitivity: "Public",
        description: "Opt-in `@invariant <condition>` assertion in a comment",
    },
    "comment::@owns@v1" => RuleDescriptor {
        name: "comment::@owns@v1",
        family: "comment",
        input_types: &["raw_bytes"],
        output_type: "claim::@owns",
        language: None,
        grammar_version: None,
        default_confidence: 0.95,
        default_sensitivity: "Public",
        description: "Opt-in `@owns <subsystem>` ownership annotation in a comment",
    },
    "comment::SAFETY@v1" => RuleDescriptor {
        name: "comment::SAFETY@v1",
        family: "comment",
        input_types: &["raw_bytes", "code::unsafe-region"],
        output_type: "code::safety-justification",
        language: Some("rust"),
        grammar_version: Some(TREE_SITTER_RUST_VERSION),
        default_confidence: 0.95,
        default_sensitivity: "Public",
        description: "Rust `// SAFETY:` justification paired to an `unsafe { }` region",
    },

    // ── Git rules ──
    "git::commit@v1" => RuleDescriptor {
        name: "git::commit@v1",
        family: "git",
        input_types: &["raw_bytes"],
        output_type: "git::commit",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Internal",
        description: "Git commit record (sha, author, message, changed_files)",
    },
    "git::author@v1" => RuleDescriptor {
        name: "git::author@v1",
        family: "git",
        input_types: &["raw_bytes", "git::commit"],
        output_type: "git::author",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Internal",
        description: "Author attribution for a git commit",
    },
    "git::changed-files@v1" => RuleDescriptor {
        name: "git::changed-files@v1",
        family: "git",
        input_types: &["raw_bytes", "git::commit"],
        output_type: "git::changed-files",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Internal",
        description: "File list touched by a git commit",
    },

    // ── Legacy migration rule ──
    // Singular purpose: `root migrate --to-witness-mesh` synthesises
    // one Witness per pre-existing `claims` row using this rule.
    // The Witness's spans pin the original claim's byte range; the
    // claim's `statement` text is discarded (the bytes are the truth).
    // v1.1 may deprecate this rule once all production workspaces have
    // run the migration.
    "legacy::claim@v1" => RuleDescriptor {
        name: "legacy::claim@v1",
        family: "legacy",
        input_types: &["raw_bytes"],
        output_type: "legacy::claim",
        language: None,
        grammar_version: None,
        // 0.50 — pre-Witness-Mesh claims went through LLM extraction
        // + tribunal grading. Treating them at confidence 0.99 would
        // over-claim. 0.50 signals "ingested from legacy substrate;
        // trust gracefully degraded until re-compile re-derives them
        // from the rule catalog."
        default_confidence: 0.50,
        default_sensitivity: "Public",
        description: "Migration rule — wraps a legacy `claims` row into a Witness",
    },

    // ── Code marker rules ──
    "code::marker::todo@v1" => RuleDescriptor {
        name: "code::marker::todo@v1",
        family: "code",
        input_types: &["raw_bytes"],
        output_type: "code::marker",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "`TODO:` / `TODO!` markers in code comments",
    },
    "code::marker::fixme@v1" => RuleDescriptor {
        name: "code::marker::fixme@v1",
        family: "code",
        input_types: &["raw_bytes"],
        output_type: "code::marker",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "`FIXME:` / `XXX:` / `HACK:` markers in code comments",
    },

    // ── Image-family rules (mathematical extraction; no LLM) ──
    // Catalog v1.1 adds pure-Rust feature extractors for raster
    // images: every rule consumes the whole-file byte range as a
    // single span (`spans[0] = (file_blake3, 0, len)`) and emits a
    // Witness whose payload is the deterministic feature.
    "image::phash@v1" => RuleDescriptor {
        name: "image::phash@v1",
        family: "image",
        input_types: &["raw_bytes"],
        output_type: "image::phash",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "8x8 DCT perceptual hash — near-duplicate detection across visually similar images",
    },
    "image::color-histogram@v1" => RuleDescriptor {
        name: "image::color-histogram@v1",
        family: "image",
        input_types: &["raw_bytes"],
        output_type: "image::color-histogram",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "RGB color distribution at 16-bucket resolution per channel (4096 buckets total)",
    },
    "image::edge-summary@v1" => RuleDescriptor {
        name: "image::edge-summary@v1",
        family: "image",
        input_types: &["raw_bytes"],
        output_type: "image::edge-summary",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Sobel-edge density + mean intensity — coarse structural fingerprint",
    },
    "image::exif@v1" => RuleDescriptor {
        name: "image::exif@v1",
        family: "image",
        input_types: &["raw_bytes"],
        output_type: "image::exif",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "EXIF metadata key/value pairs — camera, lens, timestamps, GPS when present",
    },
    "image::dominant-colors@v1" => RuleDescriptor {
        name: "image::dominant-colors@v1",
        family: "image",
        input_types: &["raw_bytes"],
        output_type: "image::dominant-colors",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Top-K dominant RGB clusters via online quantisation — palette fingerprint",
    },
    "image::skipped@v1" => RuleDescriptor {
        name: "image::skipped@v1",
        family: "image",
        input_types: &["raw_bytes"],
        output_type: "image::skipped",
        language: None,
        grammar_version: None,
        default_confidence: 0.99,
        default_sensitivity: "Public",
        description: "Image format unsupported / decode failed — honest absence (mirrors lsp::skipped@v1)",
    },
};

/// Look up a rule descriptor by name. Returns `None` if the rule is
/// not in the catalog — callers should treat that as a hard error
/// (a Witness referencing an unknown rule is malformed).
pub fn get(rule_name: &str) -> Option<&'static RuleDescriptor> {
    RULE_CATALOG.get(rule_name)
}

/// All rule descriptors as a sorted-by-name iterator. Used by the
/// canonical TOML emitter and by `tr-verify` for catalog cross-check.
pub fn all_sorted() -> Vec<&'static RuleDescriptor> {
    let mut all: Vec<&'static RuleDescriptor> = RULE_CATALOG.values().collect();
    all.sort_by_key(|d| d.name);
    all
}

/// Serialize the entire catalog to a deterministic TOML string for
/// the `rule_catalog.toml` member of a `tr/3.2` pack.
///
/// Determinism contract:
/// - Entries sorted alphabetically by rule name.
/// - Inside each entry, fields emitted in the same fixed order.
/// - No comments in the output (comments don't affect TOML semantics
///   but they change the BLAKE3).
/// - `catalog_blake3` is appended at the top after the body has been
///   computed; it is the BLAKE3 of every byte AFTER its own line.
///
/// The output is byte-identical across processes for a given build
/// (grammar versions are constants pinned at compile time).
pub fn rule_catalog_toml() -> String {
    let mut body = String::new();
    let _ = writeln!(body, "catalog_version = \"{}\"", CATALOG_VERSION);
    body.push_str("[rules]\n");

    for d in all_sorted() {
        let _ = writeln!(body, "\n[rules.\"{}\"]", d.name);
        let _ = writeln!(body, "family = \"{}\"", d.family);
        let _ = writeln!(body, "input_types = [{}]", quote_list(d.input_types));
        let _ = writeln!(body, "output_type = \"{}\"", d.output_type);
        if let Some(lang) = d.language {
            let _ = writeln!(body, "language = \"{lang}\"");
        }
        if let Some(gv) = d.grammar_version {
            let _ = writeln!(body, "grammar_version = \"{gv}\"");
        }
        let _ = writeln!(body, "default_confidence = {:.2}", d.default_confidence);
        let _ = writeln!(body, "default_sensitivity = \"{}\"", d.default_sensitivity);
        let _ = writeln!(body, "description = \"{}\"", escape_toml_str(d.description));
    }

    let hash = blake3::hash(body.as_bytes()).to_hex().to_string();
    format!("catalog_blake3 = \"{hash}\"\n{body}")
}

fn quote_list(items: &[&'static str]) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('"');
        out.push_str(item);
        out.push('"');
    }
    out
}

/// TOML basic-string escaping per the spec: backslash and double-quote
/// need escaping; control characters become `\uXXXX`. Our rule
/// descriptions are author-controlled ASCII so this stays simple, but
/// we still escape correctly so future-edits cannot silently corrupt
/// the catalog.
fn escape_toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(&mut out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_non_empty() {
        assert!(RULE_CATALOG.len() >= 50, "v1.0 catalog has at least 50 rules");
    }

    #[test]
    fn every_rule_name_matches_its_key() {
        for (key, descriptor) in RULE_CATALOG.entries() {
            assert_eq!(*key, descriptor.name, "key/name mismatch for {key}");
        }
    }

    #[test]
    fn rule_names_use_version_suffix() {
        for descriptor in RULE_CATALOG.values() {
            assert!(
                descriptor.name.contains("@v"),
                "rule `{}` missing `@vN` version suffix",
                descriptor.name
            );
        }
    }

    #[test]
    fn confidence_in_valid_range() {
        for descriptor in RULE_CATALOG.values() {
            assert!(
                (0.0..=1.0).contains(&descriptor.default_confidence),
                "rule `{}` has invalid confidence {}",
                descriptor.name,
                descriptor.default_confidence
            );
            // No rule claims perfection.
            assert!(
                descriptor.default_confidence < 1.0,
                "rule `{}` carries confidence 1.0 — no rule is infallible",
                descriptor.name
            );
        }
    }

    #[test]
    fn sensitivity_is_pascal_case() {
        for descriptor in RULE_CATALOG.values() {
            assert!(
                matches!(
                    descriptor.default_sensitivity,
                    "Public" | "Internal" | "Confidential" | "Restricted"
                ),
                "rule `{}` has invalid sensitivity `{}`",
                descriptor.name,
                descriptor.default_sensitivity
            );
        }
    }

    #[test]
    fn safety_rule_inputs_unsafe_region() {
        let safety = RULE_CATALOG
            .get("comment::SAFETY@v1")
            .expect("SAFETY rule exists");
        assert!(
            safety.input_types.contains(&"code::unsafe-region"),
            "SAFETY rule must require an unsafe-region input"
        );
    }

    #[test]
    fn toml_serialization_is_deterministic() {
        let a = rule_catalog_toml();
        let b = rule_catalog_toml();
        assert_eq!(a, b, "catalog TOML must be byte-identical across calls");
    }

    #[test]
    fn toml_starts_with_catalog_hash() {
        let toml = rule_catalog_toml();
        assert!(
            toml.starts_with("catalog_blake3 = \""),
            "catalog TOML must start with its own BLAKE3"
        );
        // Pin against the constant rather than a hardcoded version
        // string so the assertion survives minor bumps that add
        // rules without changing test intent.
        assert!(toml.contains(&format!("catalog_version = \"{}\"", CATALOG_VERSION)));
    }

    #[test]
    fn toml_contains_every_rule() {
        let toml = rule_catalog_toml();
        for descriptor in RULE_CATALOG.values() {
            let header = format!("[rules.\"{}\"]", descriptor.name);
            assert!(
                toml.contains(&header),
                "catalog TOML missing entry for {}",
                descriptor.name
            );
        }
    }

    #[test]
    fn get_returns_descriptor_by_name() {
        let d = get("tree-sitter::function-decl@v1").expect("known rule");
        assert_eq!(d.family, "tree-sitter");
        assert_eq!(d.output_type, "declares::function");
        assert!(get("nonexistent::rule@v99").is_none());
    }

    #[test]
    fn grammar_versions_are_non_empty() {
        // build.rs populated these; if any is empty, build.rs has a bug.
        assert!(!TREE_SITTER_RUST_VERSION.is_empty());
        assert!(!TREE_SITTER_PYTHON_VERSION.is_empty());
        assert!(!TREE_SITTER_JAVA_VERSION.is_empty());
        assert!(!TREE_SITTER_TYPESCRIPT_VERSION.is_empty());
    }

    #[test]
    fn escape_toml_str_escapes_specials() {
        assert_eq!(escape_toml_str("a \"b\" c"), r#"a \"b\" c"#);
        assert_eq!(escape_toml_str("a\\b"), r"a\\b");
        assert_eq!(escape_toml_str("a\nb"), r"a\nb");
    }
}
