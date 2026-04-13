use std::path::Path;

use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType, DocumentIR};
use thinkingroot_core::types::{ContentHash, SourceId, SourceMetadata, SourceType};
use thinkingroot_core::{Error, Result};

/// Parse a manifest file into ManifestDependency chunks.
pub fn parse(path: &Path) -> Result<DocumentIR> {
    let content = std::fs::read_to_string(path).map_err(|e| Error::io_path(path, e))?;
    let hash = ContentHash::from_bytes(content.as_bytes());
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

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

    let chunks = match filename {
        "Cargo.toml" => parse_cargo_toml(&content),
        "pyproject.toml" => parse_pyproject_toml(&content),
        "package.json" => parse_package_json(&content),
        "go.mod" => parse_go_mod(&content),
        "requirements.txt" => parse_requirements_txt(&content),
        _ => {
            return Err(Error::UnsupportedFileType {
                extension: "unknown-manifest".to_string(),
            });
        }
    };

    for chunk in chunks {
        doc.add_chunk(chunk);
    }
    Ok(doc)
}

fn make_dep_chunk(raw_line: &str, project_name: &str, dep_name: &str) -> Chunk {
    let mut chunk = Chunk::new(raw_line, ChunkType::ManifestDependency, 0, 0);
    chunk.metadata = ChunkMetadata {
        type_name: Some(project_name.to_string()),
        import_path: Some(dep_name.to_string()),
        ..Default::default()
    };
    chunk
}

fn parse_cargo_toml(content: &str) -> Vec<Chunk> {
    let value: toml::Value = match toml::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse Cargo.toml: {e}");
            return Vec::new();
        }
    };
    let project_name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("unknown")
        .to_string();

    let mut chunks = Vec::new();
    for section in &["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = value.get(*section).and_then(|v| v.as_table()) {
            for (dep_name, _) in deps {
                chunks.push(make_dep_chunk(
                    &format!("{section}.{dep_name}"),
                    &project_name,
                    dep_name,
                ));
            }
        }
    }
    chunks
}

fn parse_package_json(content: &str) -> Vec<Chunk> {
    let value: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse package.json: {e}");
            return Vec::new();
        }
    };
    let project_name = value["name"].as_str().unwrap_or("unknown").to_string();
    let mut chunks = Vec::new();
    for section in &["dependencies", "devDependencies"] {
        if let Some(deps) = value[section].as_object() {
            for (dep_name, _) in deps {
                chunks.push(make_dep_chunk(
                    &format!("{section}.{dep_name}"),
                    &project_name,
                    dep_name,
                ));
            }
        }
    }
    chunks
}

fn parse_go_mod(content: &str) -> Vec<Chunk> {
    let mut project_name = "unknown".to_string();
    let mut in_require = false;
    let mut chunks = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(stripped) = trimmed.strip_prefix("module ") {
            project_name = stripped.trim().to_string();
        } else if trimmed == "require (" {
            in_require = true;
        } else if trimmed == ")" && in_require {
            in_require = false;
        } else if in_require && !trimmed.is_empty() && !trimmed.starts_with("//") {
            if let Some(dep_name) = trimmed.split_whitespace().next() {
                chunks.push(make_dep_chunk(trimmed, &project_name, dep_name));
            }
        } else if let Some(rest_raw) = trimmed
            .strip_prefix("require ")
            .filter(|_| !trimmed.contains('('))
        {
            let rest = rest_raw.trim();
            if let Some(dep_name) = rest.split_whitespace().next() {
                chunks.push(make_dep_chunk(rest, &project_name, dep_name));
            }
        }
    }
    chunks
}

fn parse_requirements_txt(content: &str) -> Vec<Chunk> {
    let project_name = "python-project".to_string();
    let mut chunks = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        // Skip VCS URLs like git+https://github.com/...
        if trimmed.contains("://") {
            continue;
        }
        let dep_name = trimmed
            .split(['>', '<', '=', '!', '~', '[', ';', ' '])
            .next()
            .unwrap_or(trimmed)
            .to_string();
        if !dep_name.is_empty() {
            chunks.push(make_dep_chunk(trimmed, &project_name, &dep_name));
        }
    }
    chunks
}

fn parse_pyproject_toml(content: &str) -> Vec<Chunk> {
    let value: toml::Value = match toml::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse pyproject.toml: {e}");
            return Vec::new();
        }
    };
    let project_name = value
        .get("project")
        .and_then(|p| p.get("name"))
        .or_else(|| {
            value
                .get("tool")
                .and_then(|t| t.get("poetry"))
                .and_then(|p| p.get("name"))
        })
        .and_then(|n| n.as_str())
        .unwrap_or("unknown")
        .to_string();

    let mut chunks = Vec::new();

    if let Some(deps) = value
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for (dep_name, _) in deps {
            if dep_name == "python" {
                continue;
            }
            chunks.push(make_dep_chunk(
                &format!("tool.poetry.dependencies.{dep_name}"),
                &project_name,
                dep_name,
            ));
        }
    }

    if let Some(deps) = value
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for dep_val in deps {
            if let Some(dep_str) = dep_val.as_str() {
                let dep_name = dep_str
                    .split(['>', '<', '=', '!', '~', '[', ';'])
                    .next()
                    .unwrap_or(dep_str)
                    .trim()
                    .to_string();
                if !dep_name.is_empty() {
                    chunks.push(make_dep_chunk(dep_str, &project_name, &dep_name));
                }
            }
        }
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_toml_extracts_deps() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1"
tokio = { version = "1", features = ["full"] }

[dev-dependencies]
tempfile = "3"
"#;
        let chunks = parse_cargo_toml(content);
        assert!(chunks.len() >= 3, "expected serde, tokio, tempfile");
        let dep_names: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.metadata.import_path.as_deref())
            .collect();
        assert!(dep_names.contains(&"serde"));
        assert!(dep_names.contains(&"tokio"));
        assert!(dep_names.contains(&"tempfile"));
        assert!(
            chunks
                .iter()
                .all(|c| c.metadata.type_name.as_deref() == Some("my-crate"))
        );
        assert!(
            chunks
                .iter()
                .all(|c| c.chunk_type == ChunkType::ManifestDependency)
        );
    }

    #[test]
    fn package_json_extracts_deps() {
        let content =
            r#"{"name":"my-app","dependencies":{"react":"18"},"devDependencies":{"jest":"29"}}"#;
        let chunks = parse_package_json(content);
        assert_eq!(chunks.len(), 2);
        let names: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.metadata.import_path.as_deref())
            .collect();
        assert!(names.contains(&"react"));
        assert!(names.contains(&"jest"));
        assert!(
            chunks
                .iter()
                .all(|c| c.metadata.type_name.as_deref() == Some("my-app"))
        );
    }

    #[test]
    fn go_mod_extracts_deps() {
        let content = "module github.com/myorg/myapp\n\ngo 1.21\n\nrequire (\n\tgithub.com/foo/bar v1.2.3\n\tgolang.org/x/text v0.3.0\n)\n";
        let chunks = parse_go_mod(content);
        assert_eq!(chunks.len(), 2);
        let paths: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.metadata.import_path.as_deref())
            .collect();
        assert!(paths.contains(&"github.com/foo/bar"));
        assert!(paths.contains(&"golang.org/x/text"));
        assert!(
            chunks
                .iter()
                .all(|c| c.metadata.type_name.as_deref() == Some("github.com/myorg/myapp"))
        );
    }

    #[test]
    fn requirements_txt_extracts_deps() {
        let content = "# comment\nrequests>=2.28\nDjango==4.2\nnumpy~=1.24\n-r other.txt\n";
        let chunks = parse_requirements_txt(content);
        assert_eq!(chunks.len(), 3);
        let names: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.metadata.import_path.as_deref())
            .collect();
        assert!(names.contains(&"requests"));
        assert!(names.contains(&"Django"));
        assert!(names.contains(&"numpy"));
    }

    #[test]
    fn requirements_txt_skips_vcs_urls() {
        let content = "requests>=2.28\ngit+https://github.com/user/repo.git#egg=mypackage\n";
        let chunks = parse_requirements_txt(content);
        assert_eq!(chunks.len(), 1, "VCS URL lines must be skipped");
        assert_eq!(chunks[0].metadata.import_path.as_deref(), Some("requests"));
    }

    #[test]
    fn pyproject_toml_extracts_poetry_deps() {
        let content = r#"
[tool.poetry]
name = "my-python-app"

[tool.poetry.dependencies]
python = "^3.11"
httpx = ">=0.24"
pydantic = "^2"
"#;
        let chunks = parse_pyproject_toml(content);
        // python is filtered out
        assert_eq!(chunks.len(), 2);
        let names: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.metadata.import_path.as_deref())
            .collect();
        assert!(names.contains(&"httpx"));
        assert!(names.contains(&"pydantic"));
        assert!(
            chunks
                .iter()
                .all(|c| c.metadata.type_name.as_deref() == Some("my-python-app"))
        );
    }
}
