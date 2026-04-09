use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::path::PathBuf;

/// Build a single-threaded tokio runtime for blocking PyO3 calls.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
}

/// Compile a directory through the full ThinkingRoot pipeline.
///
/// Runs parse → extract (requires LLM credentials) → link → compile → verify.
/// Returns a summary dict with counts for each stage.
#[pyfunction]
fn compile(path: &str) -> PyResult<PyObject> {
    let root = PathBuf::from(path);
    let rt = runtime();
    let result = rt.block_on(async {
        let config = thinkingroot_core::config::Config::load(&root)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        let documents = thinkingroot_parse::parse_directory(&root, &config.parsers)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        let data_dir = root.join(".thinkingroot");
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        let storage = thinkingroot_graph::StorageEngine::init(&data_dir)
            .await
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        let ws_id = thinkingroot_core::types::WorkspaceId::new();
        // Extractor::new is async — initialises the embedding model.
        let extractor = thinkingroot_extract::Extractor::new(&config)
            .await
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        let extraction = extractor
            .extract_all(&documents, ws_id)
            .await
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        let linker = thinkingroot_link::Linker::new(&storage.graph);
        let link_result = linker
            .link(extraction)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        let compiler = thinkingroot_compile::Compiler::new(&config)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        let artifacts = compiler
            .compile_all(&storage.graph, &data_dir)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        let verifier = thinkingroot_verify::Verifier::new(&config);
        let verification = verifier
            .verify(&storage.graph)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        Ok::<_, PyErr>(serde_json::json!({
            "files_parsed": documents.len(),
            "claims_count": link_result.claims_linked,
            "entities_count": link_result.entities_created + link_result.entities_merged,
            "relations_count": link_result.relations_linked,
            "contradictions_count": link_result.contradictions_detected,
            "artifacts_count": artifacts.len(),
            "health_score": verification.health_score.as_percentage(),
        }))
    })?;

    to_py_json(&result)
}

/// Parse all files in a directory without LLM extraction.
///
/// Returns a list of document summaries: uri, source_type, content_hash, chunk_count.
#[pyfunction]
fn parse_directory(path: &str) -> PyResult<PyObject> {
    let root = PathBuf::from(path);
    let config = thinkingroot_core::config::ParserConfig::default();
    let docs = thinkingroot_parse::parse_directory(&root, &config)
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

    let result: Vec<serde_json::Value> = docs
        .iter()
        .map(|d| {
            serde_json::json!({
                "uri": d.uri,
                "source_type": format!("{:?}", d.source_type),
                "content_hash": d.content_hash.0,
                "chunk_count": d.chunks.len(),
            })
        })
        .collect();

    to_py_json(&result)
}

/// Parse a single file and return its chunks.
#[pyfunction]
fn parse_file(path: &str) -> PyResult<PyObject> {
    let file_path = PathBuf::from(path);
    let doc = thinkingroot_parse::parse_file(&file_path)
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

    let result = serde_json::json!({
        "uri": doc.uri,
        "source_type": format!("{:?}", doc.source_type),
        "content_hash": doc.content_hash.0,
        "chunks": doc.chunks.iter().map(|c| {
            serde_json::json!({
                "content": c.content,
                "chunk_type": format!("{:?}", c.chunk_type),
                "start_line": c.start_line,
                "end_line": c.end_line,
            })
        }).collect::<Vec<_>>(),
    });

    to_py_json(&result)
}

// ─── Engine ──────────────────────────────────────────────────

/// A handle to a compiled ThinkingRoot workspace for querying.
///
/// Obtain via `thinkingroot.open(path)`.
#[pyclass]
struct Engine {
    inner: thinkingroot_serve::engine::QueryEngine,
    ws_name: String,
    rt: tokio::runtime::Runtime,
}

#[pymethods]
impl Engine {
    fn get_entities(&self) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.list_entities(&self.ws_name))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn get_entity(&self, name: &str) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.get_entity(&self.ws_name, name))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    #[pyo3(signature = (r#type=None, min_confidence=None))]
    fn get_claims(
        &self,
        r#type: Option<&str>,
        min_confidence: Option<f64>,
    ) -> PyResult<PyObject> {
        let filter = thinkingroot_serve::engine::ClaimFilter {
            claim_type: r#type.map(String::from),
            min_confidence,
            ..Default::default()
        };
        let result = self
            .rt
            .block_on(self.inner.list_claims(&self.ws_name, filter))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn get_relations(&self, entity: &str) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.get_relations(&self.ws_name, entity))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn get_all_relations(&self) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.get_all_relations(&self.ws_name))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        // Convert raw tuples to named-field objects matching the REST API shape.
        let data: Vec<serde_json::Value> = result
            .into_iter()
            .map(|(from, to, rtype, strength)| {
                serde_json::json!({
                    "from": from,
                    "to": to,
                    "relation_type": rtype,
                    "strength": strength,
                })
            })
            .collect();
        to_py_json(&data)
    }

    #[pyo3(signature = (query, top_k=None))]
    fn search(&self, query: &str, top_k: Option<usize>) -> PyResult<PyObject> {
        let k = top_k.unwrap_or(10);
        let result = self
            .rt
            .block_on(self.inner.search(&self.ws_name, query, k))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn health(&self) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.health(&self.ws_name))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn verify(&self) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.verify(&self.ws_name))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn get_sources(&self) -> PyResult<PyObject> {
        Err(PyRuntimeError::new_err(
            "get_sources not yet implemented — use health() for source count",
        ))
    }

    fn get_contradictions(&self) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.health(&self.ws_name))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        to_py_json(&serde_json::json!({
            "count": result.contradictions,
            "warnings": result.warnings,
        }))
    }
}

/// Open an existing compiled workspace for querying.
///
/// The path should be a directory that has been compiled with `root compile`
/// or `thinkingroot.compile()`. Returns an Engine instance.
#[pyfunction]
fn open(path: &str) -> PyResult<Engine> {
    let root = PathBuf::from(path);
    let abs_path = std::fs::canonicalize(&root)
        .map_err(|e| PyRuntimeError::new_err(format!("Invalid path: {}", e)))?;
    let name = abs_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "default".to_string());

    let rt = runtime();
    let mut engine = thinkingroot_serve::engine::QueryEngine::new();
    rt.block_on(engine.mount(name.clone(), abs_path))
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

    Ok(Engine {
        inner: engine,
        ws_name: name,
        rt,
    })
}

// ─── Helpers ─────────────────────────────────────────────────

/// Convert a Serialize value to a Python object via JSON round-trip.
fn to_py_json<T: serde::Serialize>(value: &T) -> PyResult<PyObject> {
    Python::with_gil(|py| {
        let json_str = serde_json::to_string(value)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        let json_module = py.import("json")?;
        json_module
            .call_method1("loads", (json_str,))
            .map(|v| v.into())
    })
}

// ─── Module ──────────────────────────────────────────────────

#[pymodule]
fn _thinkingroot(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(compile, m)?)?;
    m.add_function(wrap_pyfunction!(parse_directory, m)?)?;
    m.add_function(wrap_pyfunction!(parse_file, m)?)?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    m.add_class::<Engine>()?;
    Ok(())
}
