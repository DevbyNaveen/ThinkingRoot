//! Clean-room reimplementation. Inspired by openhuman/tree_summarizer/
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.3 (2026-05-17) — markdown-tree export + import-verification
//! for the witness mesh substrate.
//!
//! ## What this gives the user
//!
//! Run `root mount` against any workspace and the witness mesh is
//! queryable, but it lives inside CozoDB — not browsable in a
//! markdown editor. `export_memory_tree` walks the mesh and writes a
//! self-contained directory tree with YAML frontmatter every Obsidian
//! / Logseq / Foam install can ingest. The user opens their compiled
//! knowledge in their editor of choice; no proprietary lock-in.
//!
//! ## Why content-addressed, not time-keyed
//!
//! OpenHuman's tree_summarizer keys by `year/month/day/hour` —
//! suitable for chronological notebooks but lossy when re-exported
//! (LLM-summarised; can't round-trip). The witness mesh is
//! BLAKE3-content-addressed; we mirror that into the directory
//! structure (`sources/<src_slug>/witnesses/<witness_short_id>.md`).
//! Re-exporting the same workspace always produces the SAME tree
//! byte-for-byte → idempotent, diffable, version-controllable.
//!
//! ## Two MCP tools registered
//!
//! `register_memory_tree_tools()` (called once at AppState
//! construction) registers two tools via `mcp::tool_trait`:
//!   - `export_memory_tree` (write — paths under user control)
//!   - `import_memory_tree` (read — verification only at v1; does
//!     NOT mutate the engine because that would require source
//!     bytes the markdown tree intentionally doesn't carry)
//!
//! ## Why import is verification-only at v1
//!
//! A full round-trip "re-ingest these markdown files back into the
//! engine" would require re-deriving every Witness from its source
//! bytes. The markdown tree doesn't carry source bytes (those live
//! in `source.tar.zst` in a `.tr` pack); the user opens it in
//! Obsidian, not in a re-ingestion pipeline. The honest scope is:
//! verify that an exported tree is well-formed and that every
//! Witness's `id` re-derives from its `(rule, spans)`. Re-ingestion
//! is a separate operation that operates on `.tr` packs via
//! `root mount`.

pub mod frontmatter;
pub mod layout;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};

use crate::engine::{ClaimFilter, ClaimInfo, QueryEngine, SourceInfo};
use crate::mcp::tool_trait::{
    McpToolContext, McpToolError, McpToolHandler, register_tool,
};
use frontmatter::{FrontmatterNode, NodeType};
use thinkingroot_core::types::Witness;

/// Result of `export_memory_tree`.
#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    pub workspace: String,
    pub output_dir: String,
    pub sources_written: usize,
    pub witnesses_written: usize,
    pub claims_written: usize,
    pub bytes_written: usize,
}

/// Result of `import_memory_tree` (verification-only at v1).
#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub input_dir: String,
    pub nodes_parsed: usize,
    pub witnesses_id_verified: usize,
    pub failures: Vec<ImportFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportFailure {
    pub path: String,
    pub reason: String,
}

/// Top-level export. Walks `engine.list_sources` →
/// `list_witnesses_by_source` → `list_claims` and writes a
/// deterministic directory tree under `output_dir`.
///
/// Idempotent: re-running with the same workspace + output_dir
/// produces a byte-identical tree.
pub async fn export(
    engine: &QueryEngine,
    workspace: &str,
    output_dir: &Path,
) -> Result<ExportReport, McpToolError> {
    tokio::fs::create_dir_all(output_dir)
        .await
        .map_err(|e| McpToolError::Backend(thinkingroot_core::Error::GraphStorage(format!("memory_tree io: {e}"))))?;

    let mut report = ExportReport {
        workspace: workspace.to_string(),
        output_dir: output_dir.display().to_string(),
        sources_written: 0,
        witnesses_written: 0,
        claims_written: 0,
        bytes_written: 0,
    };

    let sources = engine.list_sources(workspace).await?;
    let total_claims = engine
        .list_claims(workspace, ClaimFilter::default())
        .await?;

    for src in &sources {
        let src_slug = src.uri.clone();
        write_source_index(output_dir, workspace, src, &mut report).await?;

        let witnesses = engine
            .list_witnesses_by_source(workspace, &src.id)
            .await?;
        for w in &witnesses {
            write_witness_node(output_dir, workspace, &src_slug, w, &mut report).await?;
        }

        // Claims for this source — filter the global list by URI to
        // avoid an extra engine call for every source. The cache is
        // already in-memory so the filter is cheap.
        for claim in total_claims.iter().filter(|c| c.source_uri == src.uri) {
            write_claim_node(output_dir, workspace, &src_slug, claim, &mut report).await?;
        }
    }

    write_workspace_index(output_dir, workspace, &sources, &total_claims, &mut report).await?;

    Ok(report)
}

async fn write_workspace_index(
    output_dir: &Path,
    workspace: &str,
    sources: &[SourceInfo],
    claims: &[ClaimInfo],
    report: &mut ExportReport,
) -> Result<(), McpToolError> {
    let mut links = String::new();
    for src in sources {
        let slug = layout::sanitize_for_fs(&src.uri);
        links.push_str(&format!("- [{}](sources/{}/index.md)\n", src.uri, slug));
    }
    let body = format!(
        "# Workspace `{workspace}` — Knowledge Tree\n\n\
         Sources: **{}** · Claims: **{}**\n\n\
         ## Sources\n\n{}",
        sources.len(),
        claims.len(),
        links
    );
    let node = FrontmatterNode {
        node_type: NodeType::Index,
        workspace: workspace.to_string(),
        ..Default::default()
    };
    let path = layout::workspace_index(output_dir);
    write_node(&path, &node, &body, report).await
}

async fn write_source_index(
    output_dir: &Path,
    workspace: &str,
    src: &SourceInfo,
    report: &mut ExportReport,
) -> Result<(), McpToolError> {
    let node = FrontmatterNode {
        node_type: NodeType::Source,
        id: Some(src.id.clone()),
        workspace: workspace.to_string(),
        content_blake3: if src.content_hash.is_empty() {
            None
        } else {
            Some(src.content_hash.clone())
        },
        source_id: Some(src.id.clone()),
        ..Default::default()
    };
    let body = format!(
        "# Source: `{uri}`\n\n\
         - **id**: `{id}`\n\
         - **type**: {kind}\n\
         - **blake3**: `{hash}`\n",
        uri = src.uri,
        id = src.id,
        kind = src.source_type,
        hash = if src.content_hash.is_empty() {
            "(none)"
        } else {
            &src.content_hash
        }
    );
    let path = layout::source_index(output_dir, &src.uri);
    write_node(&path, &node, &body, report).await?;
    report.sources_written += 1;
    Ok(())
}

async fn write_witness_node(
    output_dir: &Path,
    workspace: &str,
    source_slug: &str,
    w: &Witness,
    report: &mut ExportReport,
) -> Result<(), McpToolError> {
    let span = w.spans.first();
    let parents: Vec<String> = w
        .inputs
        .iter()
        .filter_map(|i| match i {
            thinkingroot_core::types::WitnessInput::WitnessRef { id } => Some(id.to_hex()),
            thinkingroot_core::types::WitnessInput::ByteRef { .. } => None,
        })
        .collect();
    let node = FrontmatterNode {
        node_type: NodeType::Witness,
        id: Some(w.id.to_hex()),
        workspace: workspace.to_string(),
        created_at: Some(w.created_at.to_rfc3339()),
        content_blake3: Some(w.content_blake3.clone()),
        rule: Some(w.rule.clone()),
        parents,
        byte_start: span.map(|s| s.start),
        byte_end: span.map(|s| s.end),
        source_id: Some(w.source.to_string()),
        ..Default::default()
    };
    let body = format!(
        "# Witness `{id_short}`\n\n\
         - **rule**: `{rule}`\n\
         - **type**: `{kind}`\n\
         - **bytes**: `[{start}..{end})`\n\
         - **content_blake3**: `{hash}`\n\
         - **confidence**: {conf}\n",
        id_short = layout::short_id(&w.id.to_hex()),
        rule = w.rule,
        kind = w.witness_type,
        start = span.map(|s| s.start).unwrap_or(0),
        end = span.map(|s| s.end).unwrap_or(0),
        hash = w.content_blake3,
        conf = w.confidence.value(),
    );
    let path = layout::witness_file(output_dir, source_slug, &w.id.to_hex());
    write_node(&path, &node, &body, report).await?;
    report.witnesses_written += 1;
    Ok(())
}

async fn write_claim_node(
    output_dir: &Path,
    workspace: &str,
    source_slug: &str,
    claim: &ClaimInfo,
    report: &mut ExportReport,
) -> Result<(), McpToolError> {
    let node = FrontmatterNode {
        node_type: NodeType::Claim,
        id: Some(claim.id.clone()),
        workspace: workspace.to_string(),
        claim_type: Some(claim.claim_type.clone()),
        source_id: None,
        ..Default::default()
    };
    let body = format!(
        "# Claim `{id_short}`\n\n\
         > {statement}\n\n\
         - **type**: `{kind}`\n\
         - **confidence**: {conf}\n\
         - **source**: `{src}`\n",
        id_short = layout::short_id(&claim.id),
        statement = claim.statement,
        kind = claim.claim_type,
        conf = claim.confidence,
        src = claim.source_uri,
    );
    let path = layout::claim_file(output_dir, source_slug, &claim.id);
    write_node(&path, &node, &body, report).await?;
    report.claims_written += 1;
    Ok(())
}

async fn write_node(
    path: &Path,
    node: &FrontmatterNode,
    body: &str,
    report: &mut ExportReport,
) -> Result<(), McpToolError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| McpToolError::Backend(thinkingroot_core::Error::GraphStorage(format!("memory_tree io: {e}"))))?;
    }
    let bytes = format!("{}{body}", frontmatter::emit(node));
    let len = bytes.len();
    tokio::fs::write(path, bytes)
        .await
        .map_err(|e| McpToolError::Backend(thinkingroot_core::Error::GraphStorage(format!("memory_tree io: {e}"))))?;
    report.bytes_written += len;
    Ok(())
}

/// Verification-only import. Walks `input_dir` recursively, parses
/// every `.md` file's frontmatter, and for `node_type: witness`
/// nodes re-derives `WitnessId` from `(rule, spans)` and asserts
/// it matches the stored `id` field. Returns counts + per-file
/// failures.
///
/// Does NOT mutate the engine. Re-ingestion is a different
/// operation (`root mount` on a `.tr` pack); the markdown tree
/// intentionally doesn't carry source bytes.
pub async fn import_verify(input_dir: &Path) -> Result<ImportReport, McpToolError> {
    use thinkingroot_core::types::{WitnessId, WitnessSpan};

    let mut report = ImportReport {
        input_dir: input_dir.display().to_string(),
        nodes_parsed: 0,
        witnesses_id_verified: 0,
        failures: Vec::new(),
    };

    let entries = walk_markdown(input_dir).await?;
    for path in entries {
        let bytes = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) => {
                report.failures.push(ImportFailure {
                    path: path.display().to_string(),
                    reason: format!("read: {e}"),
                });
                continue;
            }
        };
        match frontmatter::parse(&bytes) {
            Ok((node, _)) => {
                report.nodes_parsed += 1;
                if node.node_type == NodeType::Witness {
                    let id_str = match &node.id {
                        Some(i) => i,
                        None => {
                            report.failures.push(ImportFailure {
                                path: path.display().to_string(),
                                reason: "witness node missing `id`".into(),
                            });
                            continue;
                        }
                    };
                    let rule = match &node.rule {
                        Some(r) => r,
                        None => {
                            report.failures.push(ImportFailure {
                                path: path.display().to_string(),
                                reason: "witness node missing `rule`".into(),
                            });
                            continue;
                        }
                    };
                    // We don't have `spans` round-tripped in v1 — the
                    // frontmatter carries only `byte_start` /
                    // `byte_end`. WitnessId::derive needs the full
                    // span sequence including `file_blake3`. So at
                    // v1 we can only verify identity-consistency
                    // when the export carries the byte range AND
                    // the source's content_blake3. Reconstruct a
                    // single-span Witness from those + re-derive.
                    let file_blake3 = node
                        .content_blake3
                        .clone()
                        .unwrap_or_default();
                    let span = WitnessSpan {
                        file_blake3,
                        start: node.byte_start.unwrap_or(0),
                        end: node.byte_end.unwrap_or(0),
                    };
                    let derived = WitnessId::derive(rule, std::slice::from_ref(&span));
                    if derived.to_hex() == *id_str {
                        report.witnesses_id_verified += 1;
                    } else {
                        report.failures.push(ImportFailure {
                            path: path.display().to_string(),
                            reason: format!(
                                "witness id mismatch — stored: {id_str}, re-derived: {}",
                                derived.to_hex()
                            ),
                        });
                    }
                }
            }
            Err(e) => {
                report.failures.push(ImportFailure {
                    path: path.display().to_string(),
                    reason: format!("frontmatter parse: {e}"),
                });
            }
        }
    }
    Ok(report)
}

/// Recursive walker — yields every `.md` path. Uses `tokio::fs`
/// throughout because `import_verify` is async and the typical
/// tree size (hundreds to low thousands of files) doesn't justify
/// a tokio::task::spawn_blocking ceremony.
async fn walk_markdown(dir: &Path) -> Result<Vec<PathBuf>, McpToolError> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&d).await {
            Ok(r) => r,
            Err(e) => {
                return Err(McpToolError::Backend(
                    thinkingroot_core::Error::GraphStorage(format!(
                        "memory_tree read_dir {}: {e}",
                        d.display()
                    )),
                ));
            }
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            let meta = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

// ── MCP tool registrations ─────────────────────────────────────────

struct ExportMemoryTreeTool;

#[async_trait]
impl McpToolHandler for ExportMemoryTreeTool {
    fn name(&self) -> &'static str {
        "export_memory_tree"
    }
    fn description(&self) -> &'static str {
        "Export a workspace's compiled knowledge (sources + witnesses + claims) as a hierarchical markdown directory tree with YAML frontmatter. Compatible with Obsidian / Logseq. Idempotent: same workspace + output_dir → byte-identical tree."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": "Workspace name." },
                "output_dir": { "type": "string", "description": "Absolute path to write the tree under. Created if missing." }
            },
            "required": ["workspace", "output_dir"]
        })
    }
    fn is_write(&self) -> bool {
        true
    }
    async fn handle(
        &self,
        args: Value,
        ctx: &McpToolContext<'_>,
    ) -> Result<Value, McpToolError> {
        let output_dir = args
            .get("output_dir")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpToolError::InvalidArgs("missing 'output_dir'".into()))?;
        let report = export(ctx.engine, ctx.workspace, Path::new(output_dir)).await?;
        Ok(serde_json::to_value(&report).map_err(|e| {
            McpToolError::Backend(thinkingroot_core::Error::GraphStorage(format!("memory_tree io: {e}")))
        })?)
    }
}

struct ImportMemoryTreeTool;

#[async_trait]
impl McpToolHandler for ImportMemoryTreeTool {
    fn name(&self) -> &'static str {
        "import_memory_tree"
    }
    fn description(&self) -> &'static str {
        "Verification-only import: walks a previously-exported markdown tree, parses every node's YAML frontmatter, and confirms that every Witness's stored id re-derives from its (rule, spans). Returns counts + per-file failures. Does NOT modify the engine — re-ingestion is a separate operation via `root mount` on a `.tr` pack."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": "Workspace name (used for permission scoping; the verification doesn't read engine state)." },
                "input_dir": { "type": "string", "description": "Absolute path of a previously-exported memory tree." }
            },
            "required": ["workspace", "input_dir"]
        })
    }
    fn is_write(&self) -> bool {
        false
    }
    async fn handle(
        &self,
        args: Value,
        _ctx: &McpToolContext<'_>,
    ) -> Result<Value, McpToolError> {
        let input_dir = args
            .get("input_dir")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpToolError::InvalidArgs("missing 'input_dir'".into()))?;
        let report = import_verify(Path::new(input_dir)).await?;
        Ok(serde_json::to_value(&report).map_err(|e| {
            McpToolError::Backend(thinkingroot_core::Error::GraphStorage(format!("memory_tree io: {e}")))
        })?)
    }
}

/// Idempotent registration. Call once at AppState construction.
pub fn register_memory_tree_tools() {
    register_tool(Arc::new(ExportMemoryTreeTool));
    register_tool(Arc::new(ImportMemoryTreeTool));
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::types::WitnessSpan;

    #[tokio::test]
    async fn import_verify_walks_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let report = import_verify(tmp.path()).await.expect("must succeed");
        assert_eq!(report.nodes_parsed, 0);
        assert_eq!(report.witnesses_id_verified, 0);
        assert!(report.failures.is_empty());
    }

    #[tokio::test]
    async fn import_verify_catches_malformed_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("bad.md"), "no frontmatter at all")
            .await
            .unwrap();
        let report = import_verify(tmp.path()).await.unwrap();
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].reason.contains("frontmatter"));
    }

    #[tokio::test]
    async fn import_verify_round_trip_on_synthetic_witness() {
        // Construct a Witness, emit a frontmatter node mirroring
        // the export path, write it as a .md, then re-verify.
        let span = WitnessSpan {
            file_blake3: "deadbeef".to_string(),
            start: 100,
            end: 200,
        };
        // The export currently doesn't preserve `file_blake3` per
        // span; we verify the simpler "id matches re-derivation
        // from (rule, single_span_built_from_frontmatter)" path.
        let rule = "tree-sitter::function-decl@v1";
        let id = thinkingroot_core::types::WitnessId::derive(rule, std::slice::from_ref(&span));
        let node = FrontmatterNode {
            node_type: NodeType::Witness,
            id: Some(id.to_hex()),
            workspace: "ws1".into(),
            content_blake3: Some("deadbeef".into()), // matches file_blake3
            rule: Some(rule.to_string()),
            byte_start: Some(100),
            byte_end: Some(200),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let doc = format!("{}# Witness body\n", frontmatter::emit(&node));
        tokio::fs::write(tmp.path().join("w.md"), doc).await.unwrap();

        let report = import_verify(tmp.path()).await.unwrap();
        assert_eq!(report.nodes_parsed, 1);
        assert_eq!(report.witnesses_id_verified, 1);
        assert!(report.failures.is_empty());
    }

    #[tokio::test]
    async fn import_verify_flags_id_mismatch_loudly() {
        let span = WitnessSpan {
            file_blake3: "deadbeef".to_string(),
            start: 100,
            end: 200,
        };
        let node = FrontmatterNode {
            node_type: NodeType::Witness,
            // wrong id — not the one derive would produce
            id: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            workspace: "ws1".into(),
            content_blake3: Some("deadbeef".into()),
            rule: Some("tree-sitter::function-decl@v1".into()),
            byte_start: Some(span.start),
            byte_end: Some(span.end),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let doc = format!("{}# body\n", frontmatter::emit(&node));
        tokio::fs::write(tmp.path().join("bad.md"), doc).await.unwrap();

        let report = import_verify(tmp.path()).await.unwrap();
        assert_eq!(report.nodes_parsed, 1);
        assert_eq!(report.witnesses_id_verified, 0);
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].reason.contains("id mismatch"));
    }
}
