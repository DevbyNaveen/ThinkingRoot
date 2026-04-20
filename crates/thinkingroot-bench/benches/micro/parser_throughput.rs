use std::io::Write;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Source generators
// ---------------------------------------------------------------------------

/// Generate synthetic but structurally valid Rust source code.
fn generate_rust_source(lines: usize) -> String {
    let mut out = String::with_capacity(lines * 60);
    out.push_str("use std::collections::HashMap;\n");
    out.push_str("use std::sync::Arc;\n");
    out.push('\n');

    let mut line = 3;
    let mut fn_idx = 0;

    while line < lines {
        // Doc comment
        out.push_str(&format!("/// Performs operation number {}.\n", fn_idx));
        line += 1;
        if line >= lines {
            break;
        }

        // Function signature
        out.push_str(&format!(
            "pub fn operation_{}(input: &str, count: usize) -> Option<String> {{\n",
            fn_idx
        ));
        line += 1;

        // Function body
        while line < lines.saturating_sub(2) {
            let body_line = line % 5;
            match body_line {
                0 => out.push_str("    let mut result = String::new();\n"),
                1 => out.push_str("    let map: HashMap<&str, usize> = HashMap::new();\n"),
                2 => out.push_str(&format!(
                    "    if count > {} {{ return None; }}\n",
                    fn_idx * 10 + 1
                )),
                3 => out.push_str("    result.push_str(input);\n"),
                _ => out.push_str("    let _x = Arc::new(result.clone());\n"),
            }
            line += 1;

            // Keep functions a reasonable size (8-12 lines of body)
            if line % 12 == 0 {
                break;
            }
        }

        out.push_str("    Some(result)\n");
        line += 1;
        out.push_str("}\n\n");
        line += 2;
        fn_idx += 1;
    }

    out
}

/// Generate synthetic but structurally valid Python source code.
fn generate_python_source(lines: usize) -> String {
    let mut out = String::with_capacity(lines * 50);
    out.push_str("import os\n");
    out.push_str("import sys\n");
    out.push_str("from typing import Optional, List, Dict\n");
    out.push('\n');

    let mut line = 4;
    let mut fn_idx = 0;

    while line < lines {
        // Docstring-bearing function
        out.push_str(&format!(
            "def process_{}(data: List[str], limit: int = 100) -> Optional[Dict[str, int]]:\n",
            fn_idx
        ));
        line += 1;
        if line >= lines {
            break;
        }

        out.push_str(&format!(
            "    \"\"\"Process batch {} of the input data.\"\"\"\n",
            fn_idx
        ));
        line += 1;

        while line < lines.saturating_sub(2) {
            let body_line = line % 6;
            match body_line {
                0 => out.push_str("    result = {}\n"),
                1 => out.push_str("    for item in data:\n"),
                2 => out.push_str("        if len(item) > limit:\n"),
                3 => out.push_str("            continue\n"),
                4 => out.push_str("        result[item] = len(item)\n"),
                _ => out.push_str("    count = len(result)\n"),
            }
            line += 1;

            if line % 10 == 0 {
                break;
            }
        }

        out.push_str("    return result\n");
        line += 1;
        out.push('\n');
        line += 1;
        fn_idx += 1;
    }

    out
}

/// Generate synthetic but structurally valid TypeScript source code.
fn generate_typescript_source(lines: usize) -> String {
    let mut out = String::with_capacity(lines * 55);
    out.push_str("import { EventEmitter } from 'events';\n");
    out.push('\n');

    let mut line = 2;
    let mut idx = 0;

    while line < lines {
        // Alternate between interfaces and exported functions
        if idx % 3 == 0 {
            out.push_str(&format!("export interface Config{} {{\n", idx));
            line += 1;
            let field_count = 4.min(lines.saturating_sub(line + 1));
            for f in 0..field_count {
                out.push_str(&format!("  field{}: string;\n", f));
                line += 1;
            }
            out.push_str("}\n\n");
            line += 2;
        } else {
            out.push_str(&format!(
                "/** Handler for stage {} of the pipeline. */\n",
                idx
            ));
            line += 1;
            if line >= lines {
                break;
            }

            out.push_str(&format!(
                "export function handle{}(input: string, opts: Config{}): string {{\n",
                idx,
                idx / 3 * 3
            ));
            line += 1;

            while line < lines.saturating_sub(2) {
                let body_line = line % 5;
                match body_line {
                    0 => out.push_str("  const parts = input.split(',');\n"),
                    1 => out.push_str("  let result = '';\n"),
                    2 => out.push_str("  for (const p of parts) {\n"),
                    3 => out.push_str("    result += p.trim();\n"),
                    _ => out.push_str("  }\n"),
                }
                line += 1;

                if line % 10 == 0 {
                    break;
                }
            }

            out.push_str("  return result;\n");
            line += 1;
            out.push_str("}\n\n");
            line += 2;
        }
        idx += 1;
    }

    out
}

/// Generate synthetic Markdown with headings, lists, and fenced code blocks.
fn generate_markdown_source(lines: usize) -> String {
    let mut out = String::with_capacity(lines * 50);
    out.push_str("# Project Documentation\n\n");

    let mut line = 2;
    let mut section = 0;

    while line < lines {
        // Section heading
        out.push_str(&format!("## Section {}\n\n", section));
        line += 2;
        if line >= lines {
            break;
        }

        // Prose paragraph
        out.push_str(&format!(
            "This section describes component {} of the system. It handles request routing and \
             validation for incoming API calls.\n\n",
            section
        ));
        line += 2;
        if line >= lines {
            break;
        }

        // Bullet list
        let list_items = 4.min(lines.saturating_sub(line));
        for i in 0..list_items {
            out.push_str(&format!(
                "- Item {}.{}: configuration parameter\n",
                section, i
            ));
            line += 1;
        }
        out.push('\n');
        line += 1;
        if line >= lines {
            break;
        }

        // Fenced code block
        out.push_str("```rust\n");
        line += 1;
        let code_lines = 5.min(lines.saturating_sub(line + 1));
        for c in 0..code_lines {
            out.push_str(&format!("let val_{} = compute({});\n", c, c));
            line += 1;
        }
        out.push_str("```\n\n");
        line += 2;

        section += 1;
    }

    out
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

/// Write content to a temporary file with the given extension.
fn write_temp_file(content: &str, extension: &str) -> NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(extension)
        .tempfile()
        .expect("failed to create temp file");
    f.write_all(content.as_bytes())
        .expect("failed to write temp file");
    f.flush().expect("failed to flush temp file");
    f
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

const LINE_COUNTS: &[usize] = &[100, 500, 2000];

fn bench_parse_rust(c: &mut Criterion) {
    let mut group = c.benchmark_group("parser/rust");
    for &line_count in LINE_COUNTS {
        let src = generate_rust_source(line_count);
        let file = write_temp_file(&src, ".rs");
        group.bench_with_input(BenchmarkId::new("lines", line_count), &file, |b, f| {
            b.iter(|| thinkingroot_parse::parse_file(f.path()));
        });
    }
    group.finish();
}

fn bench_parse_python(c: &mut Criterion) {
    let mut group = c.benchmark_group("parser/python");
    for &line_count in LINE_COUNTS {
        let src = generate_python_source(line_count);
        let file = write_temp_file(&src, ".py");
        group.bench_with_input(BenchmarkId::new("lines", line_count), &file, |b, f| {
            b.iter(|| thinkingroot_parse::parse_file(f.path()));
        });
    }
    group.finish();
}

fn bench_parse_typescript(c: &mut Criterion) {
    let mut group = c.benchmark_group("parser/typescript");
    for &line_count in LINE_COUNTS {
        let src = generate_typescript_source(line_count);
        let file = write_temp_file(&src, ".ts");
        group.bench_with_input(BenchmarkId::new("lines", line_count), &file, |b, f| {
            b.iter(|| thinkingroot_parse::parse_file(f.path()));
        });
    }
    group.finish();
}

fn bench_parse_markdown(c: &mut Criterion) {
    let mut group = c.benchmark_group("parser/markdown");
    for &line_count in LINE_COUNTS {
        let src = generate_markdown_source(line_count);
        let file = write_temp_file(&src, ".md");
        group.bench_with_input(BenchmarkId::new("lines", line_count), &file, |b, f| {
            b.iter(|| thinkingroot_parse::parse_file(f.path()));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_parse_rust,
    bench_parse_python,
    bench_parse_typescript,
    bench_parse_markdown,
);
criterion_main!(benches);
