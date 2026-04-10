use pyo3::prelude::*;
use std::path::PathBuf;

// Custom exception exported to Python as `thinkingroot.ThinkingRootError`.
pyo3::create_exception!(
    _thinkingroot,
    ThinkingRootError,
    pyo3::exceptions::PyException
);

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
    let result = rt
        .block_on(thinkingroot_serve::pipeline::run_pipeline(&root))
        .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;

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
        .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;

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
        .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;

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
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn get_entity(&self, name: &str) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.get_entity(&self.ws_name, name))
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    #[pyo3(signature = (r#type=None, min_confidence=None))]
    fn get_claims(&self, r#type: Option<&str>, min_confidence: Option<f64>) -> PyResult<PyObject> {
        let filter = thinkingroot_serve::engine::ClaimFilter {
            claim_type: r#type.map(String::from),
            min_confidence,
            ..Default::default()
        };
        let result = self
            .rt
            .block_on(self.inner.list_claims(&self.ws_name, filter))
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn get_relations(&self, entity: &str) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.get_relations(&self.ws_name, entity))
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn get_all_relations(&self) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.get_all_relations(&self.ws_name))
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
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
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn health(&self) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.health(&self.ws_name))
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn verify(&self) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.verify(&self.ws_name))
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn get_sources(&self) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.list_sources(&self.ws_name))
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    fn get_contradictions(&self) -> PyResult<PyObject> {
        let result = self
            .rt
            .block_on(self.inner.list_contradictions(&self.ws_name))
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
        to_py_json(&result)
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
        .map_err(|e| ThinkingRootError::new_err(format!("Invalid path: {}", e)))?;
    let name = abs_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "default".to_string());

    let rt = runtime();
    let mut engine = thinkingroot_serve::engine::QueryEngine::new();
    rt.block_on(engine.mount(name.clone(), abs_path))
        .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;

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
        let json_str =
            serde_json::to_string(value).map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
        let json_module = py.import("json")?;
        json_module
            .call_method1("loads", (json_str,))
            .map(|v| v.into())
    })
}

// ─── Module ──────────────────────────────────────────────────

#[pymodule]
fn _thinkingroot(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("ThinkingRootError", m.py().get_type::<ThinkingRootError>())?;
    m.add_function(wrap_pyfunction!(compile, m)?)?;
    m.add_function(wrap_pyfunction!(parse_directory, m)?)?;
    m.add_function(wrap_pyfunction!(parse_file, m)?)?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    m.add_class::<Engine>()?;
    Ok(())
}
