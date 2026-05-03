use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::path::PathBuf;
use std::sync::Arc;

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
        .block_on(thinkingroot_serve::pipeline::run_pipeline(
            &root, None, None,
        ))
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
/// Obtain via `thinkingroot.open(path)`. Each `Engine` owns its own
/// `EngramManager` and a stable `session_id` so RARP probes survive
/// across method calls without the caller threading a session token
/// through every signature.
#[pyclass]
struct Engine {
    inner: thinkingroot_serve::engine::QueryEngine,
    ws_name: String,
    rt: tokio::runtime::Runtime,
    engram_manager: Arc<thinkingroot_serve::intelligence::engram::EngramManager>,
    session_id: String,
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

    /// The session id this engine threads through every AEP call.
    /// Stable for the lifetime of the Engine; useful for cross-process
    /// observability (matches `X-TR-Session-Id` header on the REST
    /// path for the same workspace).
    #[getter]
    fn session_id(&self) -> &str {
        &self.session_id
    }

    // ─── Hybrid Retrieval ────────────────────────────────────────
    //
    // Wraps `QueryEngine::hybrid_retrieve`. Returns the full
    // `HybridResponse` shape — the Python SDK exposes this as
    // structured dataclasses on its side.

    #[pyo3(signature = (
        query,
        top_k=None,
        require_certificate=None,
        include_quarantined=None,
        require_provenance_verified=None,
    ))]
    fn hybrid_search(
        &self,
        query: &str,
        top_k: Option<usize>,
        require_certificate: Option<bool>,
        include_quarantined: Option<bool>,
        require_provenance_verified: Option<bool>,
    ) -> PyResult<PyObject> {
        use thinkingroot_serve::engine::{RetrievalRequest, ScoringProfile};
        let req = RetrievalRequest {
            query_text: query.to_string(),
            typed_predicates: vec![],
            session_id: self.session_id.clone(),
            clearance: vec![thinkingroot_core::types::Sensitivity::Public],
            top_k: top_k.unwrap_or(20),
            time_window: None,
            scoring_profile: ScoringProfile::default(),
            require_certificate: require_certificate.unwrap_or(false),
            include_test_origin: true,
            include_quarantined: include_quarantined.unwrap_or(false),
            require_provenance_verified: require_provenance_verified.unwrap_or(false),
            now: None,
            scoped_claim_ids: None,
        };
        let result = self
            .rt
            .block_on(self.inner.hybrid_retrieve(&self.ws_name, req, None))
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
        to_py_json(&result)
    }

    // ─── RARP / Active Engram Protocol ───────────────────────────

    /// Materialize an Engram for `topic` and return `{pointer, summary}`.
    /// `seed_entity_ids` falls back to a vector search when omitted.
    /// `scope` is a dict with the same shape as the MCP `scope` arg
    /// (depth_hops, event_window_days, clearance, seed_claim_ids,
    /// score_with_hybrid).
    #[pyo3(signature = (topic, seed_entity_ids=None, scope=None))]
    fn materialize_engram(
        &self,
        topic: &str,
        seed_entity_ids: Option<Vec<String>>,
        scope: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<PyObject> {
        let topic_owned = topic.to_string();
        let seeds: Vec<String> = match seed_entity_ids {
            Some(ids) => ids,
            None => {
                let result = self
                    .rt
                    .block_on(self.inner.search(&self.ws_name, topic, 10))
                    .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
                result.entities.into_iter().map(|e| e.id).collect()
            }
        };
        if seeds.is_empty() {
            return Err(ThinkingRootError::new_err(format!(
                "no semantic anchors for topic '{topic}'"
            )));
        }

        // Convert the optional Python scope dict to a serde_json::Value
        // we can hand to the existing parser (kept inside thinkingroot-
        // serve so the v1 Engine and the SDK share one source of truth).
        let scope_value: Option<serde_json::Value> = match scope {
            Some(d) => {
                let json_module = d.py().import("json")?;
                let json_str: String =
                    json_module.call_method1("dumps", (d,))?.extract()?;
                serde_json::from_str(&json_str)
                    .map_err(|e| ThinkingRootError::new_err(e.to_string()))?
            }
            None => None,
        };
        let scope_parsed = thinkingroot_serve::mcp::tools::parse_scope(scope_value.as_ref());

        let graph = match self.rt.block_on(self.inner.graph_store(&self.ws_name)) {
            Some(g) => g,
            None => {
                return Err(ThinkingRootError::new_err(format!(
                    "workspace '{}' not mounted",
                    self.ws_name
                )));
            }
        };

        let session_id = self.session_id.clone();
        let manager = self.engram_manager.clone();
        let ws = self.ws_name.clone();
        let result = self
            .rt
            .block_on(async move {
                manager
                    .materialize_engram(
                        &session_id,
                        &ws,
                        &topic_owned,
                        &graph,
                        seeds,
                        scope_parsed,
                        None,
                    )
                    .await
            })
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;

        let (pointer, summary) = result;
        Python::with_gil(|py| {
            let dict = PyDict::new(py);
            dict.set_item("pointer", pointer)?;
            // Round-trip the summary through JSON for shape parity with
            // the REST surface (the SDK uses the same dict layout).
            let summary_json = serde_json::to_string(&*summary)
                .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;
            let json_module = py.import("json")?;
            let summary_obj = json_module.call_method1("loads", (summary_json,))?;
            dict.set_item("summary", summary_obj)?;
            Ok(dict.into())
        })
    }

    /// Probe an Engram with a question. Returns the full ProbeAnswer
    /// shape as a dict (matches `engine::ProbeAnswer` serialization).
    #[pyo3(signature = (
        pointer,
        question,
        clearance=None,
        probe_kind=None,
        score_with_hybrid=None,
    ))]
    fn probe_engram(
        &self,
        pointer: &str,
        question: &str,
        clearance: Option<Vec<String>>,
        probe_kind: Option<&str>,
        score_with_hybrid: Option<bool>,
    ) -> PyResult<PyObject> {
        use thinkingroot_serve::engine::{RetrievalRequest, ScoringProfile};

        let graph = match self.rt.block_on(self.inner.graph_store(&self.ws_name)) {
            Some(g) => g,
            None => {
                return Err(ThinkingRootError::new_err(format!(
                    "workspace '{}' not mounted",
                    self.ws_name
                )));
            }
        };
        let byte_store = match self.inner.byte_store(&self.ws_name) {
            Some(b) => b,
            None => {
                return Err(ThinkingRootError::new_err(format!(
                    "workspace '{}' has no byte store",
                    self.ws_name
                )));
            }
        };

        let parsed_clearance: Option<Vec<thinkingroot_core::types::Sensitivity>> = clearance
            .as_ref()
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| {
                        thinkingroot_serve::mcp::tools::parse_sensitivity_str(s)
                    })
                    .collect()
            });
        let probe_kind_parsed = probe_kind
            .and_then(thinkingroot_serve::mcp::tools::parse_probe_kind_str);

        let session_id = self.session_id.clone();
        let manager = self.engram_manager.clone();
        let pointer_owned = pointer.to_string();
        let question_owned = question.to_string();
        let probe_clearance = parsed_clearance.clone();
        let mut answer = self
            .rt
            .block_on(async move {
                manager
                    .probe_engram(
                        &session_id,
                        &pointer_owned,
                        &question_owned,
                        parsed_clearance,
                        &graph,
                        byte_store.as_ref(),
                        probe_kind_parsed,
                    )
                    .await
            })
            .map_err(|e| ThinkingRootError::new_err(e.to_string()))?;

        if score_with_hybrid.unwrap_or(false) && !answer.claim_ids.is_empty() {
            let req = RetrievalRequest {
                query_text: question.to_string(),
                typed_predicates: vec![],
                session_id: self.session_id.clone(),
                clearance: probe_clearance
                    .unwrap_or_else(|| vec![thinkingroot_core::types::Sensitivity::Public]),
                top_k: answer.claim_ids.len(),
                time_window: None,
                scoring_profile: ScoringProfile::default(),
                require_certificate: false,
                include_test_origin: true,
                include_quarantined: false,
                require_provenance_verified: false,
                now: None,
                scoped_claim_ids: Some(answer.claim_ids.clone()),
            };
            if let Ok(resp) = self
                .rt
                .block_on(self.inner.hybrid_retrieve(&self.ws_name, req, None))
            {
                let new_order: Vec<String> =
                    resp.hits.iter().map(|h| h.claim_id.clone()).collect();
                thinkingroot_serve::mcp::tools::reorder_probe_answer_in_place(
                    &mut answer,
                    &new_order,
                );
            }
        }

        to_py_json(&answer)
    }

    /// List engrams active for this Engine's session.
    fn list_engrams(&self) -> PyResult<PyObject> {
        let session_id = self.session_id.clone();
        let manager = self.engram_manager.clone();
        let refs = self
            .rt
            .block_on(async move { manager.list_engrams(&session_id).await });
        to_py_json(&refs)
    }

    /// Explicitly evict an Engram. Returns True when an Engram was
    /// removed, False otherwise.
    fn expire_engram(&self, pointer: &str) -> PyResult<bool> {
        let session_id = self.session_id.clone();
        let manager = self.engram_manager.clone();
        let ptr = pointer.to_string();
        Ok(self
            .rt
            .block_on(async move { manager.expire_engram(&session_id, &ptr).await }))
    }

    /// Reset this Engine's session — drops every active Engram.
    /// Useful after a writing compile to pick up fresh claim ids.
    fn reset_session(&self) -> PyResult<()> {
        let session_id = self.session_id.clone();
        let manager = self.engram_manager.clone();
        self.rt
            .block_on(async move { manager.invalidate_session(&session_id).await });
        Ok(())
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

    // Each in-process Engine owns its own EngramManager + session id.
    // Mirrors the AppState pattern in thinkingroot-serve so AEP probes
    // get the same lifecycle semantics (TTL eviction, cache-dirty
    // invalidation, max engrams per session). The session id is
    // BLAKE3-derived from name+now so it's stable for the Engine's
    // lifetime but unique across distinct Engine instances.
    let engram_manager = thinkingroot_serve::intelligence::engram::EngramManager::new(
        thinkingroot_serve::intelligence::engram::EngramConfig::default(),
    );
    let session_id = generate_session_id(&name);

    Ok(Engine {
        inner: engine,
        ws_name: name,
        rt,
        engram_manager,
        session_id,
    })
}

/// Mint a per-Engine session id stable for the Engine's lifetime.
/// The id is derived from `(workspace_name, process pid, monotonic
/// nanos)` so concurrent Engines in the same process never collide.
fn generate_session_id(workspace: &str) -> String {
    let pid = std::process::id() as u64;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let input = format!("{workspace}|{pid}|{nanos}");
    let h = blake3::hash(input.as_bytes());
    let hex = h.to_hex();
    format!("py-{}", &hex[..16])
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
