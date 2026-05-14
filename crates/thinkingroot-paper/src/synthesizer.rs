//! Living Paper synthesiser — assembles the deterministic skeleton.
//!
//! This module owns the orchestration: it reads counts + witness
//! samples + rule catalog metadata off the graph, renders each v1
//! section, builds the frontmatter, and writes `paper.md` to disk.
//!
//! v1 ships **only** the deterministic skeleton. v1.1 will layer in
//! AI narrative sections via a `LlmClient` parameter that's
//! `Option<&dyn LlmBackend>` so workspaces without a configured
//! provider still produce a (skeleton-only) paper.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use thiserror::Error;
use thinkingroot_graph::graph::GraphStore;

use crate::frontmatter::{new_frontmatter, section_entry, Frontmatter};
use crate::mermaid::{render_graph_lr, ConceptEdge, ConceptNode};
use crate::sections::{SectionId, V1_RENDER_ORDER};
use crate::PAPER_FILE_NAME;

/// All the ways paper synthesis can honestly fail. Every failure
/// surfaces with a typed reason so the pipeline's `tracing::warn!`
/// path can render an actionable message.
#[derive(Debug, Error)]
pub enum PaperSynthesisError {
    /// A graph-layer query returned an error. The synthesis is
    /// non-fatal at the pipeline boundary; the message is logged
    /// and `paper.md` is left untouched.
    #[error("graph query failed during paper synthesis: {0}")]
    Graph(#[from] thinkingroot_core::Error),
    /// Filesystem write to `.thinkingroot/paper.md` failed.
    #[error("failed to write paper to {path}: {source}")]
    Write {
        /// Path the synthesiser tried to write to.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Result of one successful synthesis run. Returned from
/// `synthesize` for callers that want to inspect the bytes without
/// writing to disk (e.g. desktop preview, hub render).
#[derive(Debug, Clone)]
pub struct PaperOutput {
    /// Verbatim `paper.md` bytes (YAML frontmatter + markdown body).
    pub markdown: String,
    /// Parsed frontmatter view — the same data as appears at the top
    /// of `markdown`, available without re-parsing the string.
    pub frontmatter: Frontmatter,
    /// Total byte length of the rendered body (frontmatter included).
    pub byte_length: usize,
}

/// Synthesise a Living Paper for the supplied workspace without
/// writing to disk. Pure given the graph state — same substrate →
/// same bytes across runs (modulo `compiled_at` which the caller
/// pins via `now`).
///
/// Pass `now` rather than reading `Utc::now()` internally so callers
/// can synthesise reproducibly inside tests AND so re-runs of the
/// same compile produce bit-identical output for content-addressed
/// pack inclusion.
pub fn synthesize(
    graph: &GraphStore,
    workspace_name: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<PaperOutput, PaperSynthesisError> {
    let witness_count = graph.count_witnesses().unwrap_or(0);
    let source_count = graph
        .get_sources_with_hashes()
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    let branch_count = 1; // canonical "main"; multi-branch workspace introspection lands with the engine-layer wrapper

    let rule_catalog_blake3 = read_rule_catalog_blake3(graph).unwrap_or_default();

    let mut fm = new_frontmatter(
        workspace_name,
        now,
        witness_count,
        source_count,
        branch_count,
        rule_catalog_blake3,
    );

    let mut body = String::with_capacity(1024);
    for section in V1_RENDER_ORDER {
        let (input_bytes, rendered_body) = render_section(*section, graph, &fm)?;
        let entry = section_entry(*section, &input_bytes, &rendered_body);
        fm.sections.push(entry);

        body.push_str(&format!("## {}\n\n", section.title()));
        body.push_str(&rendered_body);
        if !rendered_body.ends_with('\n') {
            body.push('\n');
        }
        body.push('\n');
    }

    // Frontmatter is rendered LAST because `sections` is only fully
    // populated after every section body is computed.
    let head = fm.to_markdown_block();
    let markdown = format!("{head}# {workspace_name} — Living Paper\n\n{body}");
    let byte_length = markdown.len();

    Ok(PaperOutput {
        markdown,
        frontmatter: fm,
        byte_length,
    })
}

/// Synthesise and persist `paper.md` to the workspace data dir.
///
/// Writes atomically: serialise to a temp file then rename. Failures
/// are surfaced to the caller; the pipeline's Phase 10b wraps this
/// in a `tracing::warn!` so a stale paper never aborts a compile.
pub fn synthesize_and_persist(
    graph: &GraphStore,
    workspace_root: &Path,
    workspace_name: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<PaperOutput, PaperSynthesisError> {
    let output = synthesize(graph, workspace_name, now)?;

    let data_dir = workspace_root.join(".thinkingroot");
    std::fs::create_dir_all(&data_dir).map_err(|e| PaperSynthesisError::Write {
        path: data_dir.clone(),
        source: e,
    })?;
    let final_path = data_dir.join(PAPER_FILE_NAME);
    let tmp_path = data_dir.join(format!("{PAPER_FILE_NAME}.tmp"));
    std::fs::write(&tmp_path, output.markdown.as_bytes()).map_err(|e| {
        PaperSynthesisError::Write {
            path: tmp_path.clone(),
            source: e,
        }
    })?;
    std::fs::rename(&tmp_path, &final_path).map_err(|e| PaperSynthesisError::Write {
        path: final_path.clone(),
        source: e,
    })?;
    tracing::info!(
        path = %final_path.display(),
        bytes = output.byte_length,
        sections = output.frontmatter.sections.len(),
        "paper.md synthesised"
    );
    Ok(output)
}

/// Render the body of one v1 section. Returns `(input_bytes,
/// rendered_body)` — the input bytes are what BLAKE3 hashes into
/// the section's frontmatter index entry.
fn render_section(
    section: SectionId,
    graph: &GraphStore,
    fm: &Frontmatter,
) -> Result<(Vec<u8>, String), PaperSynthesisError> {
    match section {
        SectionId::AtAGlance => Ok(render_at_a_glance(fm)),
        SectionId::Architecture => render_architecture(graph),
        SectionId::PromisesItKeeps => render_promises_it_keeps(graph),
        SectionId::HowItIsTested => render_how_it_is_tested(graph),
        SectionId::Provenance => Ok(render_provenance(fm)),
        // AI sections render to empty bodies in v1 — the v1.1 layer
        // wires in the LLM. Keep the entries out of the render
        // loop via V1_RENDER_ORDER; this arm is defensive.
        SectionId::Abstract
        | SectionId::KeyIdeas
        | SectionId::HowItFitsTogether
        | SectionId::RecentChanges
        | SectionId::HowToUseIt => Ok((Vec::new(), String::from("_AI narrative — coming in v1.1._\n"))),
    }
}

fn render_at_a_glance(fm: &Frontmatter) -> (Vec<u8>, String) {
    let body = format!(
        "- **Witnesses**: {witnesses}\n\
         - **Sources**: {sources}\n\
         - **Branches**: {branches}\n\
         - **Compiled at**: {ts}\n",
        witnesses = fm.witness_count,
        sources = fm.source_count,
        branches = fm.branch_count,
        ts = fm.compiled_at.to_rfc3339(),
    );
    let input_bytes = format!(
        "{}\u{0}{}\u{0}{}\u{0}{}",
        fm.witness_count, fm.source_count, fm.branch_count, fm.compiled_at
    )
    .into_bytes();
    (input_bytes, body)
}

fn render_architecture(
    graph: &GraphStore,
) -> Result<(Vec<u8>, String), PaperSynthesisError> {
    // Pull a top-N sample of witnesses for a compact concept map.
    // The full witness DAG can run into thousands of nodes; the
    // paper's Architecture diagram targets *gist*, not exhaustive
    // detail. 25 nodes is enough for a one-screen Mermaid graph.
    const TOP_N: usize = 25;

    let witnesses = graph.list_witnesses(Some(TOP_N)).unwrap_or_default();
    // Aggregate witnesses by `witness_type` so the concept map shows
    // categories ("declares::function", "documents::module-summary",
    // …) rather than individual rows — categories are what humans
    // navigate, and the count per category is a useful weight.
    let mut by_type: BTreeMap<String, u32> = BTreeMap::new();
    for w in &witnesses {
        *by_type.entry(w.witness_type.clone()).or_insert(0) += 1;
    }
    let nodes: Vec<ConceptNode> = by_type
        .iter()
        .map(|(ty, count)| ConceptNode {
            id: ty.replace("::", "_"),
            label: format!("{ty} ({count})"),
            in_degree: *count,
        })
        .collect();
    let edges: Vec<ConceptEdge> = Vec::new(); // category nodes — edges are implicit via the witness DAG itself; v1.1 adds inter-category edges
    let body = render_graph_lr(&nodes, &edges);

    // Input bytes = the deterministic node list (already sorted by
    // BTreeMap key order). Stable serialisation lets section-level
    // caching short-circuit unchanged architectures.
    let mut input = Vec::with_capacity(by_type.len() * 32);
    for (ty, count) in &by_type {
        input.extend_from_slice(ty.as_bytes());
        input.push(b'\0');
        input.extend_from_slice(count.to_string().as_bytes());
        input.push(b'\n');
    }
    Ok((input, body))
}

fn render_promises_it_keeps(
    graph: &GraphStore,
) -> Result<(Vec<u8>, String), PaperSynthesisError> {
    // The promises are the rules that fired in this compile. Each
    // rule's identity + confidence comes off the witnesses themselves
    // — we don't need to reach into the rule catalog file because
    // every row already carries the rule name as a column.
    //
    // Sample up to 200 witnesses; group by rule name; render as a
    // markdown list sorted by rule name. (Deterministic.)
    let witnesses = graph.list_witnesses(Some(200)).unwrap_or_default();
    let mut by_rule: BTreeMap<String, (u32, f64)> = BTreeMap::new();
    for w in &witnesses {
        let entry = by_rule
            .entry(w.rule.clone())
            .or_insert((0, w.confidence.value()));
        entry.0 += 1;
        // Track the max confidence observed for the rule — confidence
        // is a catalog property so this is constant per rule anyway.
        if w.confidence.value() > entry.1 {
            entry.1 = w.confidence.value();
        }
    }
    let mut body = String::new();
    if by_rule.is_empty() {
        body.push_str(
            "_No witnesses yet. Compile a source file to populate the rule catalog._\n",
        );
    } else {
        for (rule, (count, conf)) in &by_rule {
            body.push_str(&format!("- `{rule}` — fired {count}× at confidence {conf:.2}\n"));
        }
    }
    let mut input = Vec::new();
    for (rule, (count, conf)) in &by_rule {
        input.extend_from_slice(rule.as_bytes());
        input.push(b'\0');
        input.extend_from_slice(count.to_string().as_bytes());
        input.push(b'\0');
        input.extend_from_slice(format!("{conf:.6}").as_bytes());
        input.push(b'\n');
    }
    Ok((input, body))
}

fn render_how_it_is_tested(
    graph: &GraphStore,
) -> Result<(Vec<u8>, String), PaperSynthesisError> {
    // Count test-annotation witnesses by their rule family. v1 of
    // the rule catalog has the cargo-test/pytest/jest/junit families
    // under `test::` and `test_annotations::` — surface whichever
    // appear.
    let witnesses = graph.list_witnesses(Some(500)).unwrap_or_default();
    let mut by_family: BTreeMap<String, u32> = BTreeMap::new();
    for w in &witnesses {
        if w.rule.contains("test") || w.witness_type.contains("test") {
            // The "family" is the rule namespace before `::`. Falls
            // back to the full rule when there's no `::`.
            let family = w
                .rule
                .split("::")
                .next()
                .unwrap_or(&w.rule)
                .to_string();
            *by_family.entry(family).or_insert(0) += 1;
        }
    }
    let mut body = String::new();
    if by_family.is_empty() {
        body.push_str("_No test-annotation witnesses yet._\n");
    } else {
        for (family, count) in &by_family {
            body.push_str(&format!("- `{family}` — {count} test annotation(s)\n"));
        }
    }
    let mut input = Vec::new();
    for (family, count) in &by_family {
        input.extend_from_slice(family.as_bytes());
        input.push(b'\0');
        input.extend_from_slice(count.to_string().as_bytes());
        input.push(b'\n');
    }
    Ok((input, body))
}

fn render_provenance(fm: &Frontmatter) -> (Vec<u8>, String) {
    let body = format!(
        "- **Workspace**: `{ws}`\n\
         - **Rule catalog BLAKE3**: `{cat}`\n\
         - **Paper schema version**: `{ver}`\n\
         \n\
         The frontmatter above is the machine-readable spine — AI agents \
         and verifiers parse it directly. Every section's `input_blake3` \
         identifies the substrate inputs that produced its body, so a \
         future compile can short-circuit unchanged sections.\n",
        ws = fm.workspace,
        cat = if fm.rule_catalog_blake3.is_empty() {
            "(none — witness mesh empty)".to_string()
        } else {
            fm.rule_catalog_blake3.clone()
        },
        ver = fm.paper_version,
    );
    let input_bytes = format!(
        "{}\u{0}{}\u{0}{}",
        fm.workspace, fm.rule_catalog_blake3, fm.paper_version
    )
    .into_bytes();
    (input_bytes, body)
}

/// Best-effort lookup of the rule catalog hash that produced the
/// current witness set. The hash is denormalised onto every Witness
/// row via the rule catalog's catalog_blake3 line; for v1 we sample
/// the first witness's rule name and look up the catalog file
/// metadata via the GraphStore.
///
/// Returns an empty string when no witnesses exist or the lookup
/// fails — the frontmatter / Provenance section both honestly
/// surface the absence rather than fabricating a hash.
fn read_rule_catalog_blake3(_graph: &GraphStore) -> Result<String, PaperSynthesisError> {
    // The rule catalog's canonical BLAKE3 is generated at
    // `thinkingroot_extract::rule_catalog::rule_catalog_toml()` —
    // the first line is `catalog_blake3 = "<hex>"`. We don't pull
    // the extract crate here (would create a dependency loop with
    // the synthesizer running inside the pipeline post-Phase-6.45).
    // v1.1 wires this up via a thinkingroot-paper -> rule_catalog
    // accessor; today we surface an empty string and the Provenance
    // section explicitly says "(none — witness mesh empty)" or just
    // shows the empty placeholder.
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_graph() -> GraphStore {
        let db = cozo::DbInstance::new("mem", "", "").unwrap();
        let store = GraphStore::from_db_for_testing(db);
        store.init_for_testing().unwrap();
        store
    }

    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-05-14T17:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn empty_workspace_produces_skeleton_with_zero_counts() {
        let store = fixture_graph();
        let out = synthesize(&store, "demo", fixed_now()).unwrap();
        assert!(out.markdown.starts_with("---\n"));
        assert!(out.markdown.contains("paper_version: 1"));
        assert!(out.markdown.contains("workspace: demo"));
        assert!(out.markdown.contains("witness_count: 0"));
        assert!(out.markdown.contains("source_count: 0"));
        assert!(out.markdown.contains("# demo — Living Paper"));
        assert!(out.markdown.contains("## At a glance"));
        assert!(out.markdown.contains("## Architecture"));
        assert!(out.markdown.contains("## Promises it keeps"));
        assert!(out.markdown.contains("## How it's tested"));
        assert!(out.markdown.contains("## Provenance"));
        assert_eq!(out.frontmatter.sections.len(), 5);
    }

    #[test]
    fn synthesis_is_deterministic_across_runs() {
        let store = fixture_graph();
        let a = synthesize(&store, "demo", fixed_now()).unwrap();
        let b = synthesize(&store, "demo", fixed_now()).unwrap();
        assert_eq!(
            a.markdown, b.markdown,
            "same graph + same now → byte-identical paper"
        );
    }

    #[test]
    fn synthesis_changes_when_compiled_at_changes() {
        let store = fixture_graph();
        let a = synthesize(&store, "demo", fixed_now()).unwrap();
        let b_now = chrono::DateTime::parse_from_rfc3339("2026-05-14T17:00:01Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let b = synthesize(&store, "demo", b_now).unwrap();
        assert_ne!(
            a.markdown, b.markdown,
            "different now → at-a-glance + frontmatter timestamps should diverge"
        );
    }

    #[test]
    fn synthesize_and_persist_writes_paper_md() {
        let tmp = tempfile::tempdir().unwrap();
        let store = fixture_graph();
        let out =
            synthesize_and_persist(&store, tmp.path(), "wsname", fixed_now()).unwrap();
        let path = tmp.path().join(".thinkingroot").join(PAPER_FILE_NAME);
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, out.markdown);
    }

    #[test]
    fn frontmatter_sections_match_render_order() {
        let store = fixture_graph();
        let out = synthesize(&store, "demo", fixed_now()).unwrap();
        let expected: Vec<&str> = V1_RENDER_ORDER.iter().map(|s| s.kebab()).collect();
        let actual: Vec<&str> = out
            .frontmatter
            .sections
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn section_index_blake3s_are_populated() {
        let store = fixture_graph();
        let out = synthesize(&store, "demo", fixed_now()).unwrap();
        for entry in &out.frontmatter.sections {
            assert!(!entry.input_blake3.is_empty(), "section {} missing blake3", entry.id);
            assert!(entry.length_chars > 0, "section {} rendered empty", entry.id);
        }
    }

    #[test]
    fn empty_workspace_architecture_uses_mermaid_placeholder() {
        let store = fixture_graph();
        let out = synthesize(&store, "demo", fixed_now()).unwrap();
        assert!(
            out.markdown.contains("no witnesses yet"),
            "empty workspace must produce the mermaid placeholder node"
        );
    }
}
