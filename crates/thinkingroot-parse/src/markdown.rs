use std::path::Path;

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use thinkingroot_core::ir::{Chunk, ChunkType, DocumentIR};
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
        doc.add_chunk(Chunk::new(content, ChunkType::Prose, 1, line_count));
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

    let parser = Parser::new(content);

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

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                // Flush any accumulated prose.
                flush_prose(
                    &mut doc,
                    &mut current_text,
                    current_start_line,
                    line_counter,
                    &current_heading,
                    &mut current_links,
                );
                in_heading = true;
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
                            .with_heading(heading.clone());
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
                    current_start_line,
                    line_counter,
                    &current_heading,
                    &mut current_links,
                );
                in_code_block = true;
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
                    );
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
                    current_start_line,
                    line_counter,
                    &current_heading,
                    &mut current_links,
                );
                in_list = true;
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
                    );
                    if let Some(h) = &current_heading {
                        chunk = chunk.with_heading(h.clone());
                    }
                    doc.add_chunk(chunk);
                }
                in_list = false;
                list_content.clear();
                current_start_line = line_counter + 1;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                if !in_heading && !in_list {
                    let url = dest_url.to_string();
                    if !url.is_empty() && !url.starts_with('#') {
                        current_links.push(url);
                    }
                }
            }
            Event::Text(text) => {
                let text_str = text.to_string();
                line_counter += text_str.matches('\n').count() as u32;

                if in_heading {
                    heading_text.push_str(&text_str);
                } else if in_code_block {
                    code_content.push_str(&text_str);
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
                } else if !in_heading {
                    current_text.push('\n');
                }
            }
            Event::Code(code) => {
                if in_heading {
                    heading_text.push_str(&code);
                } else {
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
        current_start_line,
        line_counter,
        &current_heading,
        &mut current_links,
    );

    Ok(doc)
}

fn flush_prose(
    doc: &mut DocumentIR,
    text: &mut String,
    start_line: u32,
    end_line: u32,
    heading: &Option<String>,
    links: &mut Vec<String>,
) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        let mut chunk = Chunk::new(trimmed, ChunkType::Prose, start_line, end_line);
        if let Some(h) = heading {
            chunk = chunk.with_heading(h.clone());
        }
        chunk.metadata.links = std::mem::take(links);
        doc.add_chunk(chunk);
    }
    text.clear();
    links.clear(); // ensure cleared even if text was empty
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
