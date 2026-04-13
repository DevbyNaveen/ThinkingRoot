use std::fs;

use tempfile::TempDir;
use thinkingroot_bench::Scale;

// ---------------------------------------------------------------------------
// Workspace generator
// ---------------------------------------------------------------------------

const CONFIG_TOML: &str = r#"[workspace]
name = "bench-workspace"
data_dir = ".thinkingroot"

[llm]
default_provider = "bedrock"
extraction_model = "amazon.nova-micro-v1:0"

[extraction]
max_chunk_tokens = 4000
min_confidence = 0.5

[compilation]
enabled_artifacts = ["entity_page", "architecture_map"]
output_dir = "artifacts"

[verification]
staleness_days = 90

[parsers]
exclude_patterns = [".thinkingroot/**"]
respect_gitignore = false
max_file_size = 1048576
"#;

/// Returns the number of synthetic source files for each scale tier.
fn file_count(scale: Scale) -> usize {
    match scale {
        Scale::Small => 50,
        Scale::Medium => 500,
        Scale::Large => 5000,
    }
}

/// Generate a single synthetic Rust source file with ~20 simple function defs.
fn synthetic_rust_file(module: usize, file_idx: usize) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(&format!(
        "//! Auto-generated bench file — module {module}, file {file_idx}.\n\n"
    ));
    for fn_idx in 0..20_usize {
        out.push_str(&format!(
            "pub fn func_{module}_{file_idx}_{fn_idx}(x: usize) -> usize {{\n    x + {fn_idx}\n}}\n\n"
        ));
    }
    out
}

/// Create a temporary workspace with `config.toml` and synthetic Rust sources.
fn generate_workspace(scale: Scale) -> TempDir {
    let dir = TempDir::new().expect("failed to create tempdir");
    let root = dir.path();

    // Write .thinkingroot/config.toml
    let cfg_dir = root.join(".thinkingroot");
    fs::create_dir_all(&cfg_dir).expect("failed to create .thinkingroot dir");
    fs::write(cfg_dir.join("config.toml"), CONFIG_TOML).expect("failed to write config.toml");

    // Write synthetic Rust source files
    let n = file_count(scale);
    for i in 0..n {
        let module = i / 50;
        let subdir = root.join(format!("src/module_{module}"));
        fs::create_dir_all(&subdir).expect("failed to create module dir");
        let path = subdir.join(format!("file_{i}.rs"));
        fs::write(&path, synthetic_rust_file(module, i)).expect("failed to write source file");
    }

    dir
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn parse_stage(bencher: divan::Bencher, scale: &Scale) {
    let scale = *scale;
    let workspace = generate_workspace(scale);
    let config = thinkingroot_core::config::ParserConfig::default();
    bencher.bench_local(move || {
        thinkingroot_parse::parse_directory(workspace.path(), &config).unwrap()
    });
}

#[divan::bench(args = [Scale::Small, Scale::Medium])]
fn parse_and_index(bencher: divan::Bencher, scale: &Scale) {
    let scale = *scale;
    let workspace = generate_workspace(scale);
    let config = thinkingroot_core::config::ParserConfig::default();

    bencher.bench_local(move || {
        let docs = thinkingroot_parse::parse_directory(workspace.path(), &config).unwrap();

        // Insert parsed documents as Sources into a fresh GraphStore
        let graph_dir = TempDir::new().expect("failed to create graph tempdir");
        let graph = thinkingroot_graph::graph::GraphStore::init(graph_dir.path())
            .expect("failed to init GraphStore");

        for doc in &docs {
            let source = thinkingroot_core::Source::new(doc.uri.clone(), doc.source_type)
                .with_hash(doc.content_hash.clone());
            graph.insert_source(&source).expect("failed to insert source");
        }

        // Return the count to prevent the compiler from eliminating the work
        (docs.len(), graph_dir)
    });
}

fn main() {
    divan::main();
}
