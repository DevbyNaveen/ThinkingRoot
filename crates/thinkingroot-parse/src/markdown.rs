use std::path::Path;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType, DocumentIR};
use thinkingroot_core::types::{ContentHash, SourceId, SourceMetadata, SourceType};
use thinkingroot_core::{Error, Result};

/// Parse a markdown file into a DocumentIR.
pub fn parse(path: &Path) -> Result<DocumentIR> {
    let content = std::fs::read_to_string(path).map_err(|e| Error::io_path(path, e))?;
    parse_markdown_content(path, &content)
}

/// Parse a plain text file as if it were a single prose chunk.
pub fn parse_as_text(path: &Path) -> Result<DocumentIR> {
    let content = std::fs::read_to_string(path).map_err(|e| Error::io_path(path, e))?;
    let hash = ContentHash::from_bytes(content.as_bytes());
    let line_count = content.lines().count() as u32;

    let mut doc = DocumentIR::new(
        SourceId::new(),
        path.to_string_lossy().to_string(),
        SourceType::File,
    );
    doc.content_hash = hash;
    doc.metadata = SourceMetadata {
        file_extension: path.extension().and_then(|e| e.to_str()).map(String::from),
        relative_path: Some(path.to_string_lossy().to_string()),
        ..Default::default()
    };

    if !content.trim().is_empty() {
        let len = content.len() as u64;
        doc.add_chunk(
            Chunk::new(&content, ChunkType::Prose, 1, line_count).with_byte_range(0, len),
        );
    }

    Ok(doc)
}

fn parse_markdown_content(path: &Path, content: &str) -> Result<DocumentIR> {
    let hash = ContentHash::from_bytes(content.as_bytes());

    let mut doc = DocumentIR::new(
        SourceId::new(),
        path.to_string_lossy().to_string(),
        SourceType::File,
    );
    doc.content_hash = hash;
    doc.metadata = SourceMetadata {
        file_extension: Some("md".to_string()),
        relative_path: Some(path.to_string_lossy().to_string()),
        ..Default::default()
    };

    let mut opts = Options::empty();
    // Wedge 4: GFM tables — pulldown-cmark only emits Tag::Table events when
    // the ENABLE_TABLES extension is on.  Without this flag pipe tables fall
    // through as prose, which is exactly the behaviour the wedge replaces.
    opts.insert(Options::ENABLE_TABLES);
    // AUTHORITATIVE byte offsets: the offset iterator yields `(Event, Range)` so
    // every chunk's byte range is the SOURCE span — never reconstructed by a
    // substring search. This stops prose/table chunks (whose rendered text is not
    // a verbatim substring) from failing the search and being silently dropped,
    // which was losing the body of every structured doc (e.g. a big CLAUDE.md).
    let parser = Parser::new_ext(content, opts).into_offset_iter();

    let mut current_heading: Option<String> = None;
    let mut current_text = String::new();
    let mut current_start_line: u32 = 1;
    let mut line_counter: u32 = 1;
    let mut in_code_block = false;
    let mut code_lang: Option<String> = None;
    let mut code_content = String::new();
    let mut in_heading = false;
    let mut heading_text = String::new();
    let mut in_list = false;
    let mut list_content = String::new();
    let mut heading_stack: Vec<(u8, String)> = Vec::new(); // (level, text) for parent tracking
    let mut current_heading_level: u8 = 1;
    let mut current_links: Vec<String> = Vec::new();

    // Wedge 4: GFM-table state.
    let mut in_table = false;
    let mut in_table_head = false;
    let mut in_table_cell = false;
    let mut table_cell_buf = String::new();
    let mut table_current_row: Vec<String> = Vec::new();
    let mut table_headers: Vec<String> = Vec::new();
    let mut table_body_row_idx: u32 = 0;

    // Authoritative byte spans (from the offset iterator). A prose run spans from
    // its first paragraph's start to its last paragraph's end; block elements
    // carry their own range. The chunk content becomes the verbatim source slice.
    let mut prose_start: Option<usize> = None;
    let mut prose_end: usize = 0;
    let mut heading_start: usize = 0;
    let mut code_start: usize = 0;
    let mut list_start: usize = 0;

    for (event, range) in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                // Flush any accumulated prose.
                flush_prose(
                    &mut doc,
                    &mut current_text,
                    content,
                    &mut prose_start,
                    prose_end,
                    current_start_line,
                    line_counter,
                    &current_heading,
                    &mut current_links,
                );
                in_heading = true;
                heading_start = range.start;
                heading_text.clear();
                current_heading_level = match level {
                    pulldown_cmark::HeadingLevel::H1 => 1,
                    pulldown_cmark::HeadingLevel::H2 => 2,
                    pulldown_cmark::HeadingLevel::H3 => 3,
                    pulldown_cmark::HeadingLevel::H4 => 4,
                    pulldown_cmark::HeadingLevel::H5 => 5,
                    pulldown_cmark::HeadingLevel::H6 => 6,
                };
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                let heading = heading_text.trim().to_string();
                if !heading.is_empty() {
                    // Pop stack entries at same or deeper level than current heading.
                    while heading_stack
                        .last()
                        .is_some_and(|(l, _)| *l >= current_heading_level)
                    {
                        heading_stack.pop();
                    }
                    let parent = heading_stack.last().map(|(_, t)| t.clone());

                    let mut heading_chunk =
                        Chunk::new(&heading, ChunkType::Heading, line_counter, line_counter)
                            .with_heading(heading.clone())
                            .with_byte_range(heading_start as u64, range.end as u64);
                    heading_chunk.metadata.heading_level = Some(current_heading_level);
                    heading_chunk.metadata.parent = parent;
                    doc.add_chunk(heading_chunk);

                    heading_stack.push((current_heading_level, heading.clone()));
                    current_heading = Some(heading);
                }
                current_start_line = line_counter + 1;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush_prose(
                    &mut doc,
                    &mut current_text,
                    content,
                    &mut prose_start,
                    prose_end,
                    current_start_line,
                    line_counter,
                    &current_heading,
                    &mut current_links,
                );
                in_code_block = true;
                code_start = range.start;
                code_content.clear();
                code_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => {
                        let lang = lang.to_string();
                        if lang.is_empty() { None } else { Some(lang) }
                    }
                    _ => None,
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                if !code_content.trim().is_empty() {
                    let lines = code_content.lines().count() as u32;
                    let mut chunk = Chunk::new(
                        code_content.trim(),
                        ChunkType::Code,
                        line_counter.saturating_sub(lines),
                        line_counter,
                    )
                    .with_byte_range(code_start as u64, range.end as u64);
                    if let Some(lang) = &code_lang {
                        chunk = chunk.with_language(lang.clone());
                    }
                    if let Some(h) = &current_heading {
                        chunk = chunk.with_heading(h.clone());
                    }
                    doc.add_chunk(chunk);
                }
                in_code_block = false;
                code_content.clear();
                current_start_line = line_counter + 1;
            }
            Event::Start(Tag::List(_)) => {
                flush_prose(
                    &mut doc,
                    &mut current_text,
                    content,
                    &mut prose_start,
                    prose_end,
                    current_start_line,
                    line_counter,
                    &current_heading,
                    &mut current_links,
                );
                in_list = true;
                list_start = range.start;
                list_content.clear();
            }
            Event::End(TagEnd::List(_)) => {
                if !list_content.trim().is_empty() {
                    let lines = list_content.lines().count() as u32;
                    let mut chunk = Chunk::new(
                        list_content.trim(),
                        ChunkType::List,
                        line_counter.saturating_sub(lines),
                        line_counter,
                    )
                    .with_byte_range(list_start as u64, range.end as u64);
                    if let Some(h) = &current_heading {
                        chunk = chunk.with_heading(h.clone());
                    }
                    doc.add_chunk(chunk);
                }
                in_list = false;
                list_content.clear();
                current_start_line = line_counter + 1;
            }
            Event::Start(Tag::Link { dest_url, .. }) if !in_heading && !in_list => {
                let url = dest_url.to_string();
                if !url.is_empty() && !url.starts_with('#') {
                    current_links.push(url);
                }
            }
            // ── Wedge 4: GFM table events ────────────────────────────────
            Event::Start(Tag::Table(_)) => {
                flush_prose(
                    &mut doc,
                    &mut current_text,
                    content,
                    &mut prose_start,
                    prose_end,
                    current_start_line,
                    line_counter,
                    &current_heading,
                    &mut current_links,
                );
                in_table = true;
                table_headers.clear();
                table_current_row.clear();
                table_body_row_idx = 0;
            }
            Event::End(TagEnd::Table) => {
                in_table = false;
                current_start_line = line_counter + 1;
            }
            Event::Start(Tag::TableHead) => {
                in_table_head = true;
                table_current_row.clear();
            }
            Event::End(TagEnd::TableHead) => {
                in_table_head = false;
                table_headers = std::mem::take(&mut table_current_row);
            }
            Event::Start(Tag::TableRow) => {
                table_current_row.clear();
            }
            Event::End(TagEnd::TableRow) => {
                if !in_table_head && !table_headers.is_empty() {
                    let columns: Vec<(String, String)> = table_headers
                        .iter()
                        .enumerate()
                        .map(|(i, h)| {
                            let cell = table_current_row.get(i).cloned().unwrap_or_default();
                            (h.clone(), cell)
                        })
                        .collect();
                    let display = columns
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(" | ");
                    let mut chunk =
                        Chunk::new(display, ChunkType::DataRow, line_counter, line_counter)
                            .with_byte_range(range.start as u64, range.end as u64);
                    if let Some(h) = &current_heading {
                        chunk = chunk.with_heading(h.clone());
                    }
                    chunk.metadata = ChunkMetadata {
                        row_index: Some(table_body_row_idx),
                        row_columns: columns,
                        ..Default::default()
                    };
                    doc.add_chunk(chunk);
                    table_body_row_idx += 1;
                }
                table_current_row.clear();
            }
            Event::Start(Tag::TableCell) => {
                in_table_cell = true;
                table_cell_buf.clear();
            }
            Event::End(TagEnd::TableCell) => {
                in_table_cell = false;
                table_current_row.push(table_cell_buf.trim().to_string());
                table_cell_buf.clear();
            }
            // Paragraph boundary. pulldown-cmark wraps each prose block in
            // Start/End(Paragraph). Without re-inserting the blank-line
            // separator here, consecutive paragraphs concatenate into ONE chunk
            // with no gap ("…Tesla.User: I work…"), which (a) corrupts the prose
            // and (b) makes the chunk text no longer a substring of the source —
            // so `fill_byte_ranges`' substring search fails and the chunk keeps
            // its sentinel `[0,0]` range, which the sentence-witness extractor
            // treats as an empty span and drops. Mirror the source's blank line
            // so the chunk stays source-faithful and byte-anchorable.
            Event::Start(Tag::Paragraph) => {
                // Mark the start of a prose run (first paragraph since last flush).
                if !in_heading && !in_code_block && !in_table && !in_list && prose_start.is_none() {
                    prose_start = Some(range.start);
                }
            }
            Event::End(TagEnd::Paragraph) => {
                if !in_heading && !in_code_block && !in_table && !in_list {
                    current_text.push_str("\n\n");
                    prose_end = range.end; // extend the prose run to this paragraph's end
                }
            }
            Event::Text(text) => {
                let text_str = text.to_string();
                line_counter += text_str.matches('\n').count() as u32;

                if in_heading {
                    heading_text.push_str(&text_str);
                } else if in_code_block {
                    code_content.push_str(&text_str);
                } else if in_table_cell {
                    table_cell_buf.push_str(&text_str);
                } else if in_table {
                    // Text outside cells inside a table (whitespace, etc.) — ignore.
                } else if in_list {
                    list_content.push_str(&text_str);
                    list_content.push('\n');
                } else {
                    current_text.push_str(&text_str);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                line_counter += 1;
                if in_code_block {
                    code_content.push('\n');
                } else if in_table_cell {
                    table_cell_buf.push(' ');
                } else if !in_heading && !in_table {
                    current_text.push('\n');
                }
            }
            Event::Code(code) => {
                if in_heading {
                    heading_text.push_str(&code);
                } else if in_table_cell {
                    table_cell_buf.push_str(&code);
                } else if !in_table {
                    current_text.push('`');
                    current_text.push_str(&code);
                    current_text.push('`');
                }
            }
            _ => {}
        }
    }

    // Flush remaining text.
    flush_prose(
        &mut doc,
        &mut current_text,
        content,
        &mut prose_start,
        prose_end,
        current_start_line,
        line_counter,
        &current_heading,
        &mut current_links,
    );

    // Defensive backstop only: every chunk above already carries an
    // authoritative byte range from the offset iterator, so this is a no-op in
    // practice. It still backfills any chunk that somehow lacks a range (kept so
    // a future chunk type can't silently regress to the [0,0]-dropped path).
    doc.fill_byte_ranges(content);

    // Wedge 3: coalesce tiny adjacent prose chunks under the same heading.
    // Default threshold = 500 tokens (~2_000 chars in the chars/4 heuristic);
    // overridable from ExtractionConfig::chunk_coalesce_threshold_tokens
    // by callers that re-run the pass with a custom budget.
    coalesce_adjacent_prose(&mut doc.chunks, /* threshold_tokens */ 500, /* max_merge */ 8);

    Ok(doc)
}

/// Wedge 3: post-process the chunk list, merging adjacent `Prose` chunks
/// that share a heading scope when their combined token estimate fits the
/// `threshold_tokens` budget (`chars / 4`).
///
/// Rules (all enforced):
/// - Only `Prose` chunks coalesce.  Heading / Code / List / Table /
///   FunctionDef / TypeDef / Import / Comment / ModuleDoc / DataRow /
///   ConfigEntry / ManifestDependency are barriers.
/// - Same `heading` value (None==None counts as "same scope").
/// - Combined `byte_start` = first.byte_start; `byte_end` = last.byte_end.
/// - `metadata.links` = sorted dedupe union.
/// - `start_line` = first; `end_line` = last.
/// - At most `max_merge` chunks per coalesced output (defensive ceiling).
pub fn coalesce_adjacent_prose(
    chunks: &mut Vec<Chunk>,
    threshold_tokens: usize,
    max_merge: usize,
) {
    if chunks.len() < 2 {
        return;
    }
    let max_chars = threshold_tokens.saturating_mul(4).max(1);
    let max_merge = max_merge.max(2);

    let mut out: Vec<Chunk> = Vec::with_capacity(chunks.len());
    for chunk in chunks.drain(..) {
        let Some(last) = out.last_mut() else {
            out.push(chunk);
            continue;
        };
        let both_prose =
            last.chunk_type == ChunkType::Prose && chunk.chunk_type == ChunkType::Prose;
        let same_heading = last.heading == chunk.heading;
        let combined_chars = last.content.len() + 2 + chunk.content.len();
        // Count merged so far on `last`: line span / metadata reveals nothing
        // direct, so use a bounded byte-range proxy plus a strict len check.
        let merged_count = last
            .content
            .matches("\n\n")
            .count()
            .saturating_add(1);
        let under_merge_cap = merged_count < max_merge;

        if both_prose && same_heading && combined_chars <= max_chars && under_merge_cap {
            // Merge `chunk` into `last`.
            let chunk_links = chunk.metadata.links.clone();
            last.content.push_str("\n\n");
            last.content.push_str(&chunk.content);
            last.end_line = chunk.end_line;
            if chunk.byte_end > last.byte_end {
                last.byte_end = chunk.byte_end;
            }
            // Sorted-dedupe link union.
            let mut links: Vec<String> = std::mem::take(&mut last.metadata.links);
            links.extend(chunk_links);
            links.sort();
            links.dedup();
            last.metadata.links = links;
        } else {
            out.push(chunk);
        }
    }
    *chunks = out;
}

#[allow(clippy::too_many_arguments)]
fn flush_prose(
    doc: &mut DocumentIR,
    text: &mut String,
    source: &str,
    prose_start: &mut Option<usize>,
    prose_end: usize,
    start_line: u32,
    end_line: u32,
    heading: &Option<String>,
    links: &mut Vec<String>,
) {
    // Prefer the AUTHORITATIVE byte span (first paragraph start → last paragraph
    // end). The chunk content is then the VERBATIM source slice, so it is
    // anchorable by construction (slice == content) and never dropped for a
    // failed substring search. Falls back to the trimmed accumulated text only if
    // no span was captured (defensive — shouldn't happen for real prose).
    let span = prose_start.take();
    let (content_str, range): (String, Option<(u64, u64)>) = match span {
        Some(s) if prose_end > s && prose_end <= source.len() => (
            source[s..prose_end].to_string(),
            Some((s as u64, prose_end as u64)),
        ),
        _ => (text.trim().to_string(), None),
    };
    text.clear();
    if content_str.trim().is_empty() {
        links.clear();
        return;
    }
    let mut chunk = Chunk::new(&content_str, ChunkType::Prose, start_line, end_line);
    if let Some((s, e)) = range {
        chunk = chunk.with_byte_range(s, e);
    }
    if let Some(h) = heading {
        chunk = chunk.with_heading(h.clone());
    }
    chunk.metadata.links = std::mem::take(links);
    doc.add_chunk(chunk);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_paragraph_prose_is_source_faithful_and_anchored() {
        // Regression (2026-06-01): paragraph boundaries were dropped, so
        // consecutive paragraphs concatenated with no separator, the chunk text
        // stopped being a source substring, fill_byte_ranges failed, and the
        // sentence-witness extractor dropped everything (zero claims on every
        // multi-paragraph / conversational doc — the core OpenClaw use case).
        let content = "User: I drive a Tesla.\n\nUser: I work at Acme.\n\nUser: I like Rust.\n";
        let doc = parse_markdown_content(Path::new("c.md"), content).unwrap();
        let prose: Vec<_> = doc
            .chunks
            .iter()
            .filter(|c| c.chunk_type == ChunkType::Prose)
            .collect();
        assert!(!prose.is_empty(), "expected at least one prose chunk");
        for c in &prose {
            // Real byte range, not the [0,0] sentinel.
            assert!(
                c.byte_end > c.byte_start,
                "prose chunk has empty byte range: [{}..{}] {:?}",
                c.byte_start,
                c.byte_end,
                c.content
            );
            // The slice the extractor will read must equal the chunk content.
            let slice = &content[c.byte_start as usize..c.byte_end as usize];
            assert_eq!(slice, c.content, "byte range does not map back to content");
            // Paragraphs must not be jammed together.
            assert!(
                !c.content.contains("Tesla.User"),
                "paragraphs concatenated without separator: {:?}",
                c.content
            );
        }
    }

    #[test]
    fn large_structured_doc_keeps_every_body_not_just_headings() {
        // Regression (2026-06-27): a big CLAUDE.md kept its headings (verbatim →
        // matched the substring search) but DROPPED every body paragraph (the
        // parser re-rendered prose so it was no longer a source substring →
        // fill_byte_ranges failed → [0,0] → structural_persist dropped it). With
        // authoritative offsets, EVERY chunk has a real range and no body is lost.
        let content = "\
# Section One

The first body paragraph mentions RocksDB as the storage backend.

# Section Two

The second body paragraph mentions the gte-modernbert embedder.

# Section Three

The third body paragraph mentions per-project engine isolation.
";
        let doc = parse_markdown_content(Path::new("CLAUDE.md"), content).unwrap();
        // No chunk may carry the [0,0] sentinel — nothing is dropped downstream.
        for c in &doc.chunks {
            assert!(
                c.byte_end > c.byte_start,
                "chunk has empty byte range (would be dropped): {:?}",
                c.content
            );
        }
        // Every body must survive (not just the headings).
        let bodies: String = doc
            .chunks
            .iter()
            .filter(|c| c.chunk_type == ChunkType::Prose)
            .map(|c| c.content.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(bodies.contains("RocksDB"), "body 1 lost: {bodies}");
        assert!(bodies.contains("gte-modernbert"), "body 2 lost: {bodies}");
        assert!(bodies.contains("per-project engine isolation"), "body 3 lost: {bodies}");
        // Prose content is the verbatim source slice (anchorable by construction).
        for c in doc.chunks.iter().filter(|c| c.chunk_type == ChunkType::Prose) {
            let slice = &content[c.byte_start as usize..c.byte_end as usize];
            assert_eq!(slice, c.content, "prose chunk range does not map to content");
        }
    }

    #[test]
    fn parse_simple_markdown() {
        let content = "# Hello\n\nThis is a paragraph.\n\n## World\n\nAnother paragraph.\n";
        let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();

        assert!(doc.chunk_count() >= 4); // 2 headings + 2 prose
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.chunk_type == ChunkType::Heading)
        );
        assert!(doc.chunks.iter().any(|c| c.chunk_type == ChunkType::Prose));
    }

    #[test]
    fn parse_code_blocks() {
        let content = "# Code Example\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n";
        let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();

        let code_chunks: Vec<_> = doc
            .chunks
            .iter()
            .filter(|c| c.chunk_type == ChunkType::Code)
            .collect();
        assert!(!code_chunks.is_empty());
        assert_eq!(code_chunks[0].language.as_deref(), Some("rust"));
    }

    #[test]
    fn heading_level_is_captured() {
        let content = "# H1 Title\n\n## H2 Section\n\n### H3 Sub\n";
        let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
        let headings: Vec<_> = doc
            .chunks
            .iter()
            .filter(|c| c.chunk_type == ChunkType::Heading)
            .collect();
        assert_eq!(headings.len(), 3);
        assert_eq!(headings[0].metadata.heading_level, Some(1));
        assert_eq!(headings[1].metadata.heading_level, Some(2));
        assert_eq!(headings[2].metadata.heading_level, Some(3));
    }

    #[test]
    fn heading_parent_is_set_from_stack() {
        let content = "# Top\n\n## Child\n\n### Grandchild\n\n## Sibling\n";
        let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
        let headings: Vec<_> = doc
            .chunks
            .iter()
            .filter(|c| c.chunk_type == ChunkType::Heading)
            .collect();
        assert_eq!(headings.len(), 4);
        assert!(headings[0].metadata.parent.is_none(), "H1 has no parent");
        assert_eq!(headings[1].metadata.parent.as_deref(), Some("Top"));
        assert_eq!(headings[2].metadata.parent.as_deref(), Some("Child"));
        assert_eq!(
            headings[3].metadata.parent.as_deref(),
            Some("Top"),
            "Sibling H2 parent is Top"
        );
    }

    #[test]
    fn prose_links_are_collected() {
        let content =
            "# Sec\n\nSee [OAuth docs](./oauth.md) and [external](https://example.com/docs).\n";
        let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
        let prose = doc
            .chunks
            .iter()
            .find(|c| c.chunk_type == ChunkType::Prose)
            .unwrap();
        assert!(prose.metadata.links.contains(&"./oauth.md".to_string()));
        assert!(
            prose
                .metadata
                .links
                .contains(&"https://example.com/docs".to_string())
        );
    }

    #[test]
    fn fragment_only_links_are_skipped() {
        let content = "See [section](#intro) for details.\n";
        let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
        let prose = doc
            .chunks
            .iter()
            .find(|c| c.chunk_type == ChunkType::Prose)
            .unwrap();
        assert!(
            prose.metadata.links.iter().all(|l| !l.starts_with('#')),
            "fragment-only links must not be collected"
        );
    }

    #[test]
    fn links_in_list_do_not_leak_to_next_prose() {
        let content = "- Item with [link](https://list-link.com)\n\nProse after list.\n";
        let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
        let prose = doc
            .chunks
            .iter()
            .find(|c| c.chunk_type == ChunkType::Prose && c.content.contains("Prose after"))
            .expect("prose chunk must exist");
        assert!(
            prose.metadata.links.is_empty(),
            "list links must not leak into next prose chunk: {:?}",
            prose.metadata.links
        );
    }

    #[test]
    fn links_in_heading_do_not_leak_to_next_prose() {
        let content = "# [Title](https://heading-link.com)\n\nProse after heading.\n";
        let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
        let prose = doc
            .chunks
            .iter()
            .find(|c| c.chunk_type == ChunkType::Prose && c.content.contains("Prose after"))
            .expect("prose chunk must exist");
        assert!(
            prose.metadata.links.is_empty(),
            "heading links must not leak into next prose chunk: {:?}",
            prose.metadata.links
        );
    }

    // ── Wedge 4: GFM table → DataRow per body row ─────────────────────────

    #[test]
    fn gfm_table_emits_data_row_per_body_row() {
        let content = "\
# Users

| Name  | Age |
|-------|-----|
| Alice | 30  |
| Bob   | 25  |
";
        let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
        let rows: Vec<_> = doc
            .chunks
            .iter()
            .filter(|c| c.chunk_type == ChunkType::DataRow)
            .collect();
        assert_eq!(rows.len(), 2, "two body rows → two DataRow chunks");
        assert_eq!(rows[0].metadata.row_index, Some(0));
        assert_eq!(rows[1].metadata.row_index, Some(1));

        let cells_0: &Vec<(String, String)> = &rows[0].metadata.row_columns;
        assert!(cells_0.iter().any(|(k, v)| k == "Name" && v == "Alice"));
        assert!(cells_0.iter().any(|(k, v)| k == "Age" && v == "30"));
    }

    #[test]
    fn gfm_table_carries_current_heading() {
        let content = "\
## Inventory

| Item | Qty |
|------|-----|
| Pen  | 5   |
";
        let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
        let row = doc
            .chunks
            .iter()
            .find(|c| c.chunk_type == ChunkType::DataRow)
            .expect("expected one DataRow");
        assert_eq!(row.heading.as_deref(), Some("Inventory"));
    }

    // ── Wedge 3: prose coalescer ─────────────────────────────────────────

    fn prose_chunk(text: &str, heading: Option<&str>, links: Vec<String>) -> Chunk {
        let mut c = Chunk::new(text, ChunkType::Prose, 1, 1);
        if let Some(h) = heading {
            c = c.with_heading(h);
        }
        c.metadata.links = links;
        c
    }

    #[test]
    fn coalesce_merges_tiny_prose_chunks_under_threshold() {
        let mut chunks = vec![
            prose_chunk("First short paragraph.", Some("Sec"), vec![]),
            prose_chunk("Second short paragraph.", Some("Sec"), vec![]),
            prose_chunk("Third short paragraph.", Some("Sec"), vec![]),
        ];
        coalesce_adjacent_prose(&mut chunks, /* threshold */ 500, /* max */ 8);
        assert_eq!(chunks.len(), 1);
        let merged = &chunks[0];
        assert!(merged.content.contains("First"));
        assert!(merged.content.contains("Second"));
        assert!(merged.content.contains("Third"));
    }

    #[test]
    fn coalesce_does_not_merge_across_heading() {
        let mut chunks = vec![
            prose_chunk("intro", Some("A"), vec![]),
            prose_chunk("body", Some("B"), vec![]),
        ];
        coalesce_adjacent_prose(&mut chunks, 500, 8);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn coalesce_does_not_merge_across_non_prose() {
        let mut chunks = vec![
            prose_chunk("a", Some("S"), vec![]),
            Chunk::new("```\ncode\n```", ChunkType::Code, 1, 1),
            prose_chunk("b", Some("S"), vec![]),
        ];
        coalesce_adjacent_prose(&mut chunks, 500, 8);
        assert_eq!(chunks.len(), 3, "code chunk is a barrier");
    }

    #[test]
    fn coalesce_skipped_when_combined_exceeds_threshold() {
        let big = "x".repeat(2_500);
        let mut chunks = vec![
            prose_chunk(&big, Some("S"), vec![]),
            prose_chunk(&big, Some("S"), vec![]),
        ];
        coalesce_adjacent_prose(&mut chunks, /* threshold tokens */ 500, 8);
        // 5K + 2 join chars > 2_000 char budget → no merge.
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn coalesce_unions_links_metadata_dedupe_sorted() {
        let mut chunks = vec![
            prose_chunk("a", Some("S"), vec!["./z.md".to_string(), "./b.md".to_string()]),
            prose_chunk(
                "b",
                Some("S"),
                vec!["./z.md".to_string(), "./a.md".to_string()],
            ),
        ];
        coalesce_adjacent_prose(&mut chunks, 500, 8);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].metadata.links, vec!["./a.md", "./b.md", "./z.md"]);
    }

    #[test]
    fn coalesce_preserves_authoritative_byte_ranges() {
        let mut a = prose_chunk("first", Some("S"), vec![]);
        a.byte_start = 100;
        a.byte_end = 110;
        let mut b = prose_chunk("second", Some("S"), vec![]);
        b.byte_start = 200;
        b.byte_end = 220;
        let mut chunks = vec![a, b];
        coalesce_adjacent_prose(&mut chunks, 500, 8);
        assert_eq!(chunks.len(), 1);
        let c = &chunks[0];
        assert_eq!(c.byte_start, 100);
        assert_eq!(c.byte_end, 220);
    }

    #[test]
    fn coalesce_caps_at_max_merge_paragraphs() {
        // Build 12 tiny prose chunks; max_merge=4 forces 3 outputs.
        let mut chunks: Vec<_> = (0..12)
            .map(|i| prose_chunk(&format!("p{i}"), Some("S"), vec![]))
            .collect();
        coalesce_adjacent_prose(&mut chunks, 500, 4);
        assert!(
            chunks.len() >= 3,
            "max_merge=4 should produce ≥ 3 chunks for 12 inputs, got {}",
            chunks.len()
        );
    }
}
