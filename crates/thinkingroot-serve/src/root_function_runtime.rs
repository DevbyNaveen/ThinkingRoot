//! Root Function execution runtime.
//!
//! The actual JS isolate is `deno_core` (V8), gated behind the
//! `root-functions` Cargo feature so the desktop build and default CI
//! never pay V8's build cost. When the feature is OFF, [`run_js`]
//! returns a typed "not enabled" error rather than a silent no-op —
//! the `RootFunction` flow node and the REST `invoke` path surface that
//! honestly.
//!
//! ## Author contract
//!
//! A function body must evaluate to a callable, e.g.
//! ```js
//! async (input, ctx) => {
//!   const r = await fetch(ctx.env.SOME_API + "/x");   // egress-gated
//!   return { ok: r.ok, runId: ctx.runId };
//! }
//! ```
//! The runtime invokes `(<body>)(input, ctx)`, awaits the result if it
//! is a promise, and returns it as JSON. `input` is the node/REST
//! argument; `ctx` carries the resolved secret map as `ctx.env` plus the
//! invocation identity (`ctx.runId`, `ctx.ws`, `ctx.fnName`,
//! `ctx.version`, `ctx.attempt`, `ctx.sessionId`). The secret values are
//! also spread onto `ctx` itself, so a legacy `(input, env) => env.SECRET`
//! body still resolves. Secrets are cloud env vars overlaid on the local
//! `secrets.toml` (see `thinkingroot_cloud_auth::secrets`). Later phases
//! extend `ctx` with `ctx.llm`, `ctx.step`, and `ctx.cognition`.
//!
//! ## !Send isolation
//!
//! `deno_core::JsRuntime` is `!Send`, so it cannot live across `.await`
//! on the multi-thread tokio runtime the daemon uses. The enabled path
//! confines the entire runtime to one `spawn_blocking` thread with its
//! own current-thread executor; only the `serde_json::Value` result
//! (which is `Send`) crosses back.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::Value;
use thinkingroot_llm::llm::LlmClient;

/// Outcome of a single function execution: the JSON return value, or a
/// user-facing error message.
pub type RunResult = Result<Value, String>;

/// Invocation identity + context threaded into the isolate and exposed to
/// JS as `ctx`. Owned + `Send` so it crosses the `spawn_blocking`
/// boundary. Phase 2+ extends the *host* side (LLM / graph handles) while
/// this stays the JS-visible metadata. Defaults are fine for tests and
/// non-cloud callers.
#[derive(Debug, Clone, Default)]
pub struct FnCtxMeta {
    /// Unique id of this run (matches the recorded `RootFunctionRun.id`).
    pub run_id: String,
    /// Workspace the function belongs to.
    pub ws: String,
    /// Function name being invoked.
    pub fn_name: String,
    /// Resolved version of the function body.
    pub version: i64,
    /// Attempt counter (1 for a first run; >1 on resume/retry later).
    pub attempt: u32,
    /// Originating MCP session, when the invocation came from one. `None`
    /// for the session-less REST/flow path.
    pub session_id: Option<String>,
}

/// What a (possibly durable) run produced.
#[derive(Debug)]
pub enum RunOutcome {
    /// Completed with a JSON value.
    Done(Value),
    /// Paused awaiting a cognition answer. The pending request(s) are
    /// returned alongside so the caller can persist them; the run resumes
    /// (replays) once an answer is journaled.
    Suspended,
    /// Failed with a user-facing error.
    Failed(String),
}

impl RunOutcome {
    /// Convenience for callers/tests: the completed value, if `Done`.
    pub fn done(self) -> Option<Value> {
        match self {
            RunOutcome::Done(v) => Some(v),
            _ => None,
        }
    }
    pub fn is_suspended(&self) -> bool {
        matches!(self, RunOutcome::Suspended)
    }
}

/// A cognition request a run is blocked on. Persisted so an answer can be
/// matched back to the right run + journal step.
#[derive(Debug, Clone)]
pub struct PendingReq {
    /// Unguessable id used to answer this request.
    pub token: String,
    /// Journal step key the answer is recorded under (so replay finds it).
    pub step_key: String,
    /// The question posed to the answerer.
    pub question: String,
}

/// Run a Root Function body. `timeout_secs` bounds wall-clock; `env` is
/// injected as the second argument. See module docs for the contract.
#[cfg(feature = "root-functions")]
pub async fn run_js(
    body: &str,
    input: &Value,
    env: &BTreeMap<String, String>,
    ctx: FnCtxMeta,
    llm: Option<Arc<LlmClient>>,
    timeout_secs: u64,
) -> RunResult {
    // Convenience: no journal preload, recorded steps + pending discarded.
    // The durable path (`invoke_function`) uses `run_js_journaled`.
    let (outcome, _steps, _pending, _cites) =
        run_js_journaled(body, input, env, ctx, llm, HashMap::new(), timeout_secs).await;
    match outcome {
        RunOutcome::Done(v) => Ok(v),
        RunOutcome::Suspended => {
            Err("function suspended awaiting cognition (use the durable invoke path)".to_string())
        }
        RunOutcome::Failed(e) => Err(e),
    }
}

/// Durable-execution variant: `steps` is the previously-journaled
/// `step_key -> result_json` map (empty on a first run). Returns the run
/// outcome, the steps NEWLY recorded this execution, and any cognition
/// requests the run suspended on — so the caller can persist both for
/// replay/resume.
#[cfg(feature = "root-functions")]
pub async fn run_js_journaled(
    body: &str,
    input: &Value,
    env: &BTreeMap<String, String>,
    ctx: FnCtxMeta,
    llm: Option<Arc<LlmClient>>,
    steps: HashMap<String, String>,
    timeout_secs: u64,
) -> (RunOutcome, Vec<(String, String)>, Vec<PendingReq>, Vec<String>) {
    let body = body.to_string();
    let input_json = match serde_json::to_string(input) {
        Ok(s) => s,
        Err(e) => return (RunOutcome::Failed(e.to_string()), Vec::new(), Vec::new(), Vec::new()),
    };
    let env_json = match serde_json::to_string(env) {
        Ok(s) => s,
        Err(e) => return (RunOutcome::Failed(e.to_string()), Vec::new(), Vec::new(), Vec::new()),
    };

    // Confine the !Send JsRuntime to a dedicated blocking thread with its
    // own current-thread tokio runtime + LocalSet. `ctx`, the `llm` handle
    // (`Arc<LlmClient>`, Send), and the preloaded `steps` move cleanly into
    // the isolate thread; `enable_all` gives the LLM client its time+io driver.
    let handle = tokio::task::spawn_blocking(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                return (
                    RunOutcome::Failed(format!("build js executor runtime: {e}")),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                );
            }
        };
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            execute_in_isolate(&body, &input_json, &env_json, ctx, llm, steps, timeout_secs).await
        })
    });

    match handle.await {
        Ok(r) => r,
        Err(e) => (
            RunOutcome::Failed(format!("root function task panicked or was cancelled: {e}")),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
    }
}

/// Validate that a Root Function body evaluates to a callable, WITHOUT
/// invoking it. Cheap, non-brittle deploy-time gate: catches syntax errors
/// and non-function bodies (the common authoring mistakes) without the
/// false negatives of running the function against fake context. Used by
/// the `root_function` MCP authoring tool before it deploys.
#[cfg(feature = "root-functions")]
pub async fn validate_body(body: &str) -> Result<(), String> {
    let body = body.to_string();
    let handle = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("build js executor runtime: {e}"))?;
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            use deno_core::{JsRuntime, RuntimeOptions};
            let mut runtime = JsRuntime::new(RuntimeOptions::default());
            let code = format!(
                "{{ const __f = ({body}); if (typeof __f !== 'function') {{ \
                 throw new Error('Root Function body must evaluate to a function, \
                 e.g. async (input, ctx) => {{ ... }}'); }} }}"
            );
            runtime
                .execute_script("[validate_root_function]", code)
                .map(|_| ())
                .map_err(|e| format!("invalid function body: {e}"))
        })
    });
    match handle.await {
        Ok(r) => r,
        Err(e) => Err(format!("validation task failed: {e}")),
    }
}

/// Feature-off: cannot validate without the JS isolate linked.
#[cfg(not(feature = "root-functions"))]
pub async fn validate_body(_body: &str) -> Result<(), String> {
    Err("root-functions feature is not enabled in this build — cannot validate a \
         Root Function body"
        .to_string())
}

/// Run a function body against test fixtures (`(input, expected_output)`)
/// daemon-side and return `(all_passed, detail)`. The fixtures come from a
/// SEPARATE authority than the body (the `function_test` tool), so this is
/// the tamper-proof half of verify-before-merge. Works in both feature
/// configs (`run_js` returns a typed error when the isolate isn't linked,
/// which surfaces as a failed fixture).
pub async fn run_fixture_check(
    body: &str,
    fixtures: &[(Value, Value)],
    timeout_secs: u64,
) -> (bool, String) {
    if fixtures.is_empty() {
        return (
            false,
            "no fixtures defined for this function — author tests via the `function_test` tool"
                .to_string(),
        );
    }
    let env = BTreeMap::new();
    let mut failures = Vec::new();
    for (i, (input, expected)) in fixtures.iter().enumerate() {
        match run_js(body, input, &env, FnCtxMeta::default(), None, timeout_secs).await {
            Ok(v) if &v == expected => {}
            Ok(v) => failures.push(format!("fixture #{i}: expected {expected}, got {v}")),
            Err(e) => failures.push(format!("fixture #{i}: error: {e}")),
        }
    }
    if failures.is_empty() {
        (true, format!("{} fixture(s) passed", fixtures.len()))
    } else {
        (false, failures.join("; "))
    }
}

/// The egress-gated `fetch` op + extension. Exposes one async op,
/// `op_tr_fetch`, which performs an outbound HTTP request through
/// `reqwest` ONLY after the host clears [`crate::egress`]. The JS side
/// (see [`FETCH_BOOTSTRAP`]) wraps it in a familiar `fetch(url, opts)`.
#[cfg(feature = "root-functions")]
mod fetch_ext {
    use std::borrow::Cow;
    use std::collections::BTreeMap;

    use deno_core::{Extension, OpDecl, op2};
    use deno_error::JsErrorBox;

    #[derive(serde::Deserialize)]
    pub struct FetchReq {
        pub url: String,
        pub method: Option<String>,
        #[serde(default)]
        pub headers: BTreeMap<String, String>,
        pub body: Option<String>,
    }

    #[derive(serde::Serialize)]
    pub struct FetchResp {
        pub status: u16,
        pub ok: bool,
        pub headers: BTreeMap<String, String>,
        pub body: String,
    }

    #[op2(async(lazy))]
    #[serde]
    pub async fn op_tr_fetch(#[serde] req: FetchReq) -> Result<FetchResp, JsErrorBox> {
        // Egress allowlist: default-deny when TR_OUTBOUND_ALLOWLIST is set.
        let host = url::Url::parse(&req.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        if !crate::egress::host_allowed_from_env(&host) {
            return Err(JsErrorBox::generic(format!(
                "egress blocked: host '{host}' is not in this project's TR_OUTBOUND_ALLOWLIST"
            )));
        }
        // SSRF guard: reject hosts that are — or resolve to — an internal/private
        // IP (loopback, RFC-1918, link-local incl. 169.254.169.254 cloud
        // metadata, ULA). Stops an allowlisted-but-internal target.
        if let Err(why) = crate::egress::vet_outbound_host(&host).await {
            return Err(JsErrorBox::generic(format!("egress blocked: {why}")));
        }

        let method = req.method.unwrap_or_else(|| "GET".to_string());
        let m = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| JsErrorBox::type_error(format!("invalid HTTP method `{method}`")))?;
        // Follow NO redirects: a 3xx from an allowlisted host could otherwise
        // bounce the request onto an internal IP (the egress + SSRF checks only
        // vetted the ORIGINAL host). The function author handles redirects
        // explicitly, and each explicit fetch is re-vetted.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| JsErrorBox::generic(format!("build http client: {e}")))?;
        let mut rb = client.request(m, &req.url);
        for (k, v) in &req.headers {
            rb = rb.header(k, v);
        }
        if let Some(b) = req.body {
            rb = rb.body(b);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| JsErrorBox::generic(format!("fetch failed: {e}")))?;
        let status = resp.status().as_u16();
        let ok = resp.status().is_success();
        let mut headers = BTreeMap::new();
        for (k, v) in resp.headers().iter() {
            if let Ok(s) = v.to_str() {
                headers.insert(k.as_str().to_string(), s.to_string());
            }
        }
        let body = resp
            .text()
            .await
            .map_err(|e| JsErrorBox::generic(format!("reading response body: {e}")))?;
        Ok(FetchResp { status, ok, headers, body })
    }

    pub fn extension() -> Extension {
        const DECL: OpDecl = op_tr_fetch();
        Extension {
            name: "tr_fetch",
            ops: Cow::Borrowed(&[DECL]),
            ..Default::default()
        }
    }
}

/// Host state resident in the isolate's `OpState`, readable by ops. This
/// is the plumbing that lets JS reach back into the engine. Phase 1
/// carries the invocation identity (surfaced as `ctx`); later phases
/// extend `FnHostState` with the LLM client + a graph command channel for
/// `ctx.llm` / `ctx.step` / `ctx.cognition`.
#[cfg(feature = "root-functions")]
mod host_ext {
    use std::borrow::Cow;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::sync::Arc;

    use deno_core::{Extension, OpDecl, OpState, op2};
    use deno_error::JsErrorBox;

    use super::LlmClient;

    /// Inserted into `OpState` before any user/bootstrap code runs. Carries
    /// everything host ops need to reach back into the engine.
    pub struct FnHostState {
        pub meta: super::FnCtxMeta,
        /// The workspace LLM client, when one is configured. `None` means
        /// no bring-your-own-key oracle is available for this project.
        pub llm: Option<Arc<LlmClient>>,
        /// Previously-journaled steps (`step_key -> result_json`) for this
        /// run — non-empty only on a resumed/replayed run.
        pub steps: HashMap<String, String>,
        /// Steps recorded during THIS execution, flushed to the journal by
        /// the caller after the run finishes or suspends.
        pub new_steps: Vec<(String, String)>,
        /// Cognition requests this run suspended on (flushed by the caller).
        pub new_pending: Vec<super::PendingReq>,
        /// Claim/object ids this run declared it used via `ctx.cite`, for
        /// run→object touch edges (the moat's causal-invalidation basis).
        pub new_cites: Vec<String>,
    }

    /// JS-facing shape of the invocation identity (camelCase to match the
    /// `ctx` object the bootstrap builds).
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct CtxMeta {
        pub run_id: String,
        pub ws: String,
        pub fn_name: String,
        pub version: i64,
        pub attempt: u32,
        pub session_id: Option<String>,
    }

    /// Args for `op_tr_llm_ask`. A serde struct (not scalar `#[string]`
    /// args) so the op is unambiguously non-fast — mirrors `op_tr_fetch`.
    #[derive(serde::Deserialize)]
    pub struct AskReq {
        pub question: String,
        #[serde(default)]
        pub context_json: String,
    }

    /// Result of a coprocessor call: the model's answer plus which oracle
    /// backend served it (truthfully reported, never guessed).
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct AskResult {
        pub answer: String,
        pub oracle_source: String,
    }

    /// Sync op: hand the resident invocation identity to JS. Reads
    /// `FnHostState` out of `OpState` (panics only if the host forgot to
    /// `put` it — which the runtime always does).
    #[op2]
    #[serde]
    pub fn op_tr_ctx(state: &mut OpState) -> CtxMeta {
        let m = &state.borrow::<FnHostState>().meta;
        CtxMeta {
            run_id: m.run_id.clone(),
            ws: m.ws.clone(),
            fn_name: m.fn_name.clone(),
            version: m.version,
            attempt: m.attempt,
            session_id: m.session_id.clone(),
        }
    }

    /// Async op behind `ctx.llm.ask(question, context)` — the tools-blind
    /// coprocessor. The model gets ONLY the question + caller-supplied
    /// context; it has no tools, DB, or state. Backend selection (truthful):
    ///   1. BYO-key  — call the workspace `LlmClient` directly (primary,
    ///      works headless).
    ///   2. MCP sampling — opportunistic, only when an MCP session is
    ///      attached; not reachable from the session-less REST/flow invoke
    ///      path, so wired in a later phase.
    ///   3. none     — honest typed error (no oracle configured).
    #[op2(async(lazy))]
    #[serde]
    pub async fn op_tr_llm_ask(
        state: Rc<RefCell<OpState>>,
        #[serde] req: AskReq,
    ) -> Result<AskResult, JsErrorBox> {
        let AskReq { question, context_json } = req;
        // Clone the Arc out before the await — never hold the OpState
        // borrow across an await point.
        let llm = {
            let s = state.borrow();
            s.borrow::<FnHostState>().llm.clone()
        };
        let Some(client) = llm else {
            return Err(JsErrorBox::generic(
                "no LLM oracle available: this project has no LLM provider key \
                 configured (set one in Secrets), and no sampling-capable MCP \
                 client is attached — ctx.llm needs a model to call",
            ));
        };

        let system = "You are a tools-blind coprocessor invoked by a deterministic \
                      function. You have NO access to tools, databases, files, or \
                      state. Answer ONLY the question using the provided context, as \
                      concisely as possible. Do not ask follow-up questions.";
        let user = if context_json.is_empty() || context_json == "null" {
            question
        } else {
            format!("{question}\n\nContext (JSON):\n{context_json}")
        };

        let answer = client
            .chat(system, &user)
            .await
            .map_err(|e| JsErrorBox::generic(format!("llm oracle call failed: {e}")))?;
        Ok(AskResult { answer, oracle_source: "byo_key".to_string() })
    }

    #[derive(serde::Deserialize)]
    pub struct StepKeyReq {
        pub key: String,
    }

    #[derive(serde::Deserialize)]
    pub struct StepRecordReq {
        pub key: String,
        pub result_json: String,
    }

    /// Lookup result. A struct (not `Option<String>`, which op2's `#[serde]`
    /// rejects as a return) — `found=false` means the step hasn't run yet.
    #[derive(serde::Serialize)]
    pub struct LookupResult {
        pub found: bool,
        pub result_json: String,
    }

    /// Sync op behind `ctx.step` lookup: return the journaled result for a
    /// step key (newly-recorded-this-run wins over the preloaded journal).
    #[op2]
    #[serde]
    pub fn op_tr_step_lookup(state: &mut OpState, #[serde] req: StepKeyReq) -> LookupResult {
        let h = state.borrow::<FnHostState>();
        if let Some((_, v)) = h.new_steps.iter().rev().find(|(k, _)| *k == req.key) {
            return LookupResult { found: true, result_json: v.clone() };
        }
        match h.steps.get(&req.key) {
            Some(v) => LookupResult { found: true, result_json: v.clone() },
            None => LookupResult { found: false, result_json: String::new() },
        }
    }

    /// Sync op behind `ctx.step` record: append a completed step's result
    /// to the run's journal accumulator.
    #[op2]
    pub fn op_tr_step_record(state: &mut OpState, #[serde] rec: StepRecordReq) {
        let h = state.borrow_mut::<FnHostState>();
        h.new_steps.push((rec.key, rec.result_json));
    }

    #[derive(serde::Deserialize)]
    pub struct CognitionReq {
        pub step_key: String,
        pub question: String,
    }

    /// Sync op behind `ctx.cognition.ask`: register a pending cognition
    /// request (with a fresh unguessable token) for the run to suspend on.
    /// JS throws the `__TR_SUSPEND__` sentinel immediately after.
    #[op2]
    pub fn op_tr_cognition_request(state: &mut OpState, #[serde] req: CognitionReq) {
        let token = ulid::Ulid::new().to_string();
        let h = state.borrow_mut::<FnHostState>();
        h.new_pending.push(super::PendingReq {
            token,
            step_key: req.step_key,
            question: req.question,
        });
    }

    #[derive(serde::Deserialize)]
    pub struct CiteReq {
        pub claim_id: String,
    }

    /// Sync op behind `ctx.cite(claimId)`: record that this run used a graph
    /// object, so the moat can causally invalidate learned experience if
    /// that object later changes.
    #[op2]
    pub fn op_tr_cite(state: &mut OpState, #[serde] req: CiteReq) {
        let h = state.borrow_mut::<FnHostState>();
        if !req.claim_id.is_empty() {
            h.new_cites.push(req.claim_id);
        }
    }

    pub fn extension() -> Extension {
        const OPS: &[OpDecl] = &[
            op_tr_ctx(),
            op_tr_llm_ask(),
            op_tr_step_lookup(),
            op_tr_step_record(),
            op_tr_cognition_request(),
            op_tr_cite(),
        ];
        Extension {
            name: "tr_host",
            ops: Cow::Borrowed(OPS),
            ..Default::default()
        }
    }
}

/// JS shim giving Root Functions a familiar `fetch(url, opts)` backed by
/// the egress-gated op. Returns `{ status, ok, headers, text(), json() }`.
#[cfg(feature = "root-functions")]
const FETCH_BOOTSTRAP: &str = r#"
globalThis.fetch = async (url, opts = {}) => {
  const r = await Deno.core.ops.op_tr_fetch({
    url: String(url),
    method: opts.method,
    headers: opts.headers || {},
    body: opts.body === undefined || opts.body === null
      ? undefined
      : (typeof opts.body === 'string' ? opts.body : JSON.stringify(opts.body)),
  });
  return {
    status: r.status,
    ok: r.ok,
    headers: r.headers,
    text: async () => r.body,
    json: async () => JSON.parse(r.body),
  };
};
"#;

/// JS shim that assembles the `ctx` object handed to a function as its
/// second argument: the secret map (`ctx.env`, also spread on `ctx` for
/// legacy `(input, env)` bodies) plus the invocation identity pulled from
/// the host via `op_tr_ctx`. Later phases attach `ctx.llm` / `ctx.step` /
/// `ctx.cognition` here.
#[cfg(feature = "root-functions")]
const CTX_BOOTSTRAP: &str = r#"
globalThis.__tr_buildCtx = (input, env) => {
  const meta = Deno.core.ops.op_tr_ctx();
  const ctx = Object.assign({}, env);   // legacy: secrets at top level
  ctx.env = env;
  ctx.input = input;
  ctx.runId = meta.runId;
  ctx.ws = meta.ws;
  ctx.fnName = meta.fnName;
  ctx.version = meta.version;
  ctx.attempt = meta.attempt;
  ctx.sessionId = meta.sessionId ?? null;
  ctx.llm = {
    // Tools-blind coprocessor: ask the connected/configured model one
    // question. `context` is any JSON-serialisable value (optional).
    ask: async (question, context) => {
      const r = await Deno.core.ops.op_tr_llm_ask({
        question: String(question),
        context_json: context === undefined || context === null ? "" : JSON.stringify(context),
      });
      return r.answer;
    },
  };
  // ctx.step(name, fn): durable memoization. On a resumed run the recorded
  // result is returned and `fn` is NOT re-executed; otherwise `fn` runs and
  // its result is journaled. `fn` may be async.
  ctx.step = async (name, fn) => {
    const key = String(name);
    const hit = Deno.core.ops.op_tr_step_lookup({ key });
    if (hit.found) return JSON.parse(hit.result_json);
    const r = await fn();
    Deno.core.ops.op_tr_step_record({
      key,
      result_json: JSON.stringify(r === undefined ? null : r),
    });
    return r;
  };
  // Determinism enforcement: route non-deterministic *sync* globals through
  // the journal so a replayed run reproduces the original values instead of
  // diverging. (fetch journaling + crypto come in the next sub-milestone.)
  let __seq = 0;
  const __syncStep = (key, fn) => {
    const hit = Deno.core.ops.op_tr_step_lookup({ key });
    if (hit.found) return JSON.parse(hit.result_json);
    const r = fn();
    Deno.core.ops.op_tr_step_record({ key, result_json: JSON.stringify(r) });
    return r;
  };
  const __realNow = Date.now.bind(Date);
  Date.now = () => __syncStep(`__now__${__seq++}`, () => __realNow());
  const __realRandom = Math.random.bind(Math);
  Math.random = () => __syncStep(`__rand__${__seq++}`, () => __realRandom());
  // Journal fetch: on a replayed run the recorded response is returned and
  // the network is NOT hit again (determinism + idempotency on resume). The
  // egress check still runs on the first, live call (inside __realFetch).
  const __realFetch = globalThis.fetch;
  globalThis.fetch = async (url, opts = {}) => {
    const j = await ctx.step(`__fetch__${__seq++}`, async () => {
      const r = await __realFetch(url, opts);
      return { status: r.status, ok: r.ok, headers: r.headers, body: await r.text() };
    });
    return {
      status: j.status,
      ok: j.ok,
      headers: j.headers,
      text: async () => j.body,
      json: async () => JSON.parse(j.body),
    };
  };
  // ctx.cognition.ask(question): tools-blind, durable human/agent-in-the-loop.
  // On replay the journaled answer is returned; otherwise the run registers a
  // pending request and suspends (throws the sentinel the host turns into a
  // Suspended outcome) until an answer is journaled.
  let __cogSeq = 0;
  ctx.cognition = {
    ask: async (question) => {
      const key = `__cog__${__cogSeq++}`;
      const hit = Deno.core.ops.op_tr_step_lookup({ key });
      if (hit.found) return JSON.parse(hit.result_json);
      Deno.core.ops.op_tr_cognition_request({ step_key: key, question: String(question) });
      throw new Error("__TR_SUSPEND__");
    },
  };
  // ctx.cite(claimId): declare that this run used a graph object, so the
  // backend can causally invalidate what it learned if that object changes.
  ctx.cite = (claimId) => {
    Deno.core.ops.op_tr_cite({ claim_id: String(claimId) });
  };
  return ctx;
};
"#;

#[cfg(feature = "root-functions")]
async fn execute_in_isolate(
    body: &str,
    input_json: &str,
    env_json: &str,
    ctx: FnCtxMeta,
    llm: Option<Arc<LlmClient>>,
    steps: HashMap<String, String>,
    timeout_secs: u64,
) -> (RunOutcome, Vec<(String, String)>, Vec<PendingReq>, Vec<String>) {
    use deno_core::{JsRuntime, RuntimeOptions, v8};

    // Wrap the user body so it is invoked with (input, ctx) — where ctx is
    // assembled by CTX_BOOTSTRAP from the secret map + host identity — and
    // any returned promise is awaited. The IIFE returns the resolved value.
    let code = format!(
        "(async () => {{ const __f = ({body}); const __ctx = globalThis.__tr_buildCtx({input_json}, {env_json}); return await __f({input_json}, __ctx); }})()"
    );

    // Per-invocation heap cap so a runaway function can't exhaust the engine
    // container's memory. Paired with the wall-clock timeout below, these are
    // the two resource bounds on untrusted user code.
    const MAX_HEAP_BYTES: usize = 128 * 1024 * 1024; // 128 MB

    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![fetch_ext::extension(), host_ext::extension()],
        create_params: Some(v8::CreateParams::default().heap_limits(0, MAX_HEAP_BYTES)),
        ..Default::default()
    });

    // Make the invocation identity + LLM oracle + journal reachable by host
    // ops before any bootstrap or user code runs.
    runtime.op_state().borrow_mut().put(host_ext::FnHostState {
        meta: ctx,
        llm,
        steps,
        new_steps: Vec::new(),
        new_pending: Vec::new(),
        new_cites: Vec::new(),
    });

    // If the function nears the heap cap, terminate it. We bump the limit a
    // little inside the callback so V8 unwinds cleanly (raises a catchable
    // termination) instead of hard-aborting the whole engine process.
    let handle = runtime.v8_isolate().thread_safe_handle();
    runtime.add_near_heap_limit_callback(move |current, _initial| {
        handle.terminate_execution();
        current + 8 * 1024 * 1024
    });

    // Compute the outcome in a labeled block so every exit path falls
    // through to journal extraction below (no early `return`s). A throw of
    // the `__TR_SUSPEND__` sentinel (from `ctx.cognition.ask`) becomes a
    // `Suspended` outcome rather than a failure.
    let outcome: RunOutcome = 'run: {
        // Install the `fetch` shim and the `ctx` builder before user code runs.
        if let Err(e) = runtime.execute_script("[tr_fetch_bootstrap]", FETCH_BOOTSTRAP) {
            break 'run RunOutcome::Failed(format!("fetch bootstrap failed: {e}"));
        }
        if let Err(e) = runtime.execute_script("[tr_ctx_bootstrap]", CTX_BOOTSTRAP) {
            break 'run RunOutcome::Failed(format!("ctx bootstrap failed: {e}"));
        }

        // Drive the event loop until the IIFE's promise resolves, bounded by
        // the wall-clock timeout. `resolve_value` borrows `&mut runtime`;
        // the borrow ends before we open a scope to decode the result.
        let resolved = {
            let work = async {
                let promise = runtime
                    .execute_script("[root_function]", code)
                    .map_err(|e| format!("script error: {e}"))?;
                runtime
                    .resolve_value(promise)
                    .await
                    .map_err(|e| format!("evaluation error: {e}"))
            };
            match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), work).await {
                Ok(Ok(g)) => g,
                Ok(Err(e)) => {
                    // A cognition suspension surfaces as a thrown sentinel.
                    if e.contains("__TR_SUSPEND__") {
                        break 'run RunOutcome::Suspended;
                    }
                    break 'run RunOutcome::Failed(e);
                }
                Err(_) => {
                    break 'run RunOutcome::Failed(format!(
                        "root function timed out after {timeout_secs}s"
                    ));
                }
            }
        };

        // Decode the resolved v8 value into JSON. `deno_core::scope!` opens a
        // HandleScope + ContextScope on the runtime's isolate (see the
        // crate's `examples/eval_js_value.rs`).
        deno_core::scope!(scope, &mut runtime);
        let local = v8::Local::new(scope, resolved);
        match deno_core::serde_v8::from_v8::<Value>(scope, local) {
            Ok(v) => RunOutcome::Done(v),
            Err(e) => RunOutcome::Failed(format!("result decode error: {e}")),
        }
    };

    // Pull the journal + pending requests + cites recorded this run out of
    // OpState so the caller can persist them (the scope! borrow has ended).
    let (new_steps, new_pending, new_cites) = runtime
        .op_state()
        .borrow_mut()
        .try_take::<host_ext::FnHostState>()
        .map(|h| (h.new_steps, h.new_pending, h.new_cites))
        .unwrap_or_default();

    (outcome, new_steps, new_pending, new_cites)
}

/// Feature-off path: the engine was built without `root-functions`, so
/// no JS isolate is linked. Honest typed error (not a silent success).
#[cfg(not(feature = "root-functions"))]
pub async fn run_js(
    _body: &str,
    _input: &Value,
    _env: &BTreeMap<String, String>,
    _ctx: FnCtxMeta,
    _llm: Option<Arc<LlmClient>>,
    _timeout_secs: u64,
) -> RunResult {
    Err(
        "root-functions feature is not enabled in this build — rebuild `thinkingroot-serve` \
         with `--features root-functions` (the cloud image does; the desktop build does not)"
            .to_string(),
    )
}

/// Feature-off durable variant — same honest "not enabled" error, no steps.
#[cfg(not(feature = "root-functions"))]
pub async fn run_js_journaled(
    _body: &str,
    _input: &Value,
    _env: &BTreeMap<String, String>,
    _ctx: FnCtxMeta,
    _llm: Option<Arc<LlmClient>>,
    _steps: HashMap<String, String>,
    _timeout_secs: u64,
) -> (RunOutcome, Vec<(String, String)>, Vec<PendingReq>, Vec<String>) {
    (
        RunOutcome::Failed(
            "root-functions feature is not enabled in this build — rebuild `thinkingroot-serve` \
             with `--features root-functions` (the cloud image does; the desktop build does not)"
                .to_string(),
        ),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
}

/// Whether this build can actually execute Root Functions. Lets callers
/// (REST/flow) give a clear up-front signal instead of failing at run.
pub fn is_enabled() -> bool {
    cfg!(feature = "root-functions")
}

#[cfg(all(test, feature = "root-functions"))]
mod deno_tests {
    use super::*;

    #[tokio::test]
    async fn runs_js_and_returns_json() {
        let env = BTreeMap::new();
        let out = run_js(
            "(input, env) => ({ doubled: input.n * 2 })",
            &serde_json::json!({ "n": 21 }),
            &env,
            FnCtxMeta::default(),
            None,
            5,
        )
        .await
        .expect("js runs");
        assert_eq!(out["doubled"], serde_json::json!(42));
    }

    #[tokio::test]
    async fn reads_env_secret_and_awaits_async() {
        // Legacy `(input, env) => env.WHO` contract still resolves because
        // the secret values are spread onto `ctx` (the second arg).
        let mut env = BTreeMap::new();
        env.insert("WHO".to_string(), "ada".to_string());
        let out = run_js(
            "async (input, env) => env.WHO + '!' ",
            &serde_json::json!({}),
            &env,
            FnCtxMeta::default(),
            None,
            5,
        )
        .await
        .expect("async js runs");
        assert_eq!(out, serde_json::json!("ada!"));
    }

    #[tokio::test]
    async fn ctx_carries_run_identity_and_env() {
        let mut env = BTreeMap::new();
        env.insert("WHO".to_string(), "ada".to_string());
        let ctx = FnCtxMeta {
            run_id: "run_123".to_string(),
            ws: "wsA".to_string(),
            fn_name: "fnA".to_string(),
            version: 7,
            attempt: 1,
            session_id: None,
        };
        let out = run_js(
            "(input, ctx) => ({ rid: ctx.runId, ws: ctx.ws, fn: ctx.fnName, \
             v: ctx.version, who: ctx.env.WHO, sid: ctx.sessionId })",
            &serde_json::json!({}),
            &env,
            ctx,
            None,
            5,
        )
        .await
        .expect("js runs");
        assert_eq!(out["rid"], serde_json::json!("run_123"));
        assert_eq!(out["ws"], serde_json::json!("wsA"));
        assert_eq!(out["fn"], serde_json::json!("fnA"));
        assert_eq!(out["v"], serde_json::json!(7));
        assert_eq!(out["who"], serde_json::json!("ada"));
        assert_eq!(out["sid"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn fetch_is_egress_gated() {
        // Allowlist set + target NOT in it ⇒ the op denies before any
        // network call, so this is deterministic (no real request).
        // SAFETY: only this test reads/writes this env var.
        unsafe { std::env::set_var("TR_OUTBOUND_ALLOWLIST", "api.allowed.example") };
        let env = BTreeMap::new();
        let err = run_js(
            "async () => { const r = await fetch('https://blocked.example/x'); return await r.text(); }",
            &serde_json::json!({}),
            &env,
            FnCtxMeta::default(),
            None,
            10,
        )
        .await
        .unwrap_err();
        unsafe { std::env::remove_var("TR_OUTBOUND_ALLOWLIST") };
        assert!(
            err.to_lowercase().contains("egress blocked") || err.contains("blocked.example"),
            "expected egress denial, got: {err}"
        );
    }

    #[tokio::test]
    async fn js_error_is_surfaced() {
        let env = BTreeMap::new();
        let err = run_js(
            "() => { throw new Error('boom') }",
            &serde_json::json!({}),
            &env,
            FnCtxMeta::default(),
            None,
            5,
        )
        .await
        .unwrap_err();
        assert!(err.to_lowercase().contains("boom") || err.contains("evaluation error"));
    }

    #[tokio::test]
    async fn step_replay_returns_journaled_value_not_recomputed() {
        // A run whose journal already has the step result must return the
        // journaled value and NOT execute the step fn.
        let env = BTreeMap::new();
        let mut steps = HashMap::new();
        steps.insert("greeting".to_string(), "\"from-journal\"".to_string());
        let (outcome, _new, _pending, _cites) = run_js_journaled(
            "async (input, ctx) => await ctx.step('greeting', async () => 'FRESH')",
            &serde_json::json!({}),
            &env,
            FnCtxMeta::default(),
            None,
            steps,
            5,
        )
        .await;
        assert_eq!(outcome.done().expect("done"), serde_json::json!("from-journal"));
    }

    #[tokio::test]
    async fn step_records_new_result() {
        let env = BTreeMap::new();
        let (outcome, new_steps, _pending, _cites) = run_js_journaled(
            "async (input, ctx) => await ctx.step('k', async () => ({ x: 1 }))",
            &serde_json::json!({}),
            &env,
            FnCtxMeta::default(),
            None,
            HashMap::new(),
            5,
        )
        .await;
        assert_eq!(outcome.done().expect("done"), serde_json::json!({ "x": 1 }));
        assert!(
            new_steps.iter().any(|(k, v)| k == "k" && v.contains("\"x\":1")),
            "expected step 'k' journaled, got: {new_steps:?}"
        );
    }

    #[tokio::test]
    async fn math_random_is_journaled_and_deterministic_on_replay() {
        // First run records the random draw; replaying with that journal
        // must reproduce the identical value (determinism enforcement).
        let env = BTreeMap::new();
        let (o1, steps1, _p1, _c1) = run_js_journaled(
            "async (input, ctx) => Math.random()",
            &serde_json::json!({}),
            &env,
            FnCtxMeta::default(),
            None,
            HashMap::new(),
            5,
        )
        .await;
        let v1 = o1.done().expect("run1");
        assert!(!steps1.is_empty(), "Math.random should have journaled a step");
        let preload: HashMap<String, String> = steps1.into_iter().collect();
        let (o2, _s2, _p2, _c2) = run_js_journaled(
            "async (input, ctx) => Math.random()",
            &serde_json::json!({}),
            &env,
            FnCtxMeta::default(),
            None,
            preload,
            5,
        )
        .await;
        assert_eq!(v1, o2.done().expect("run2"), "replay must reproduce the journaled random");
    }

    #[tokio::test]
    async fn fetch_is_journaled_and_replayed_without_network() {
        // With the fetch response journaled, a replayed run returns it WITHOUT
        // hitting the network — even for a host no allowlist would permit
        // (proving replay short-circuits before egress/network).
        let env = BTreeMap::new();
        let mut steps = HashMap::new();
        steps.insert(
            "__fetch__0".to_string(),
            r#"{"status":200,"ok":true,"headers":{},"body":"{\"hi\":1}"}"#.to_string(),
        );
        let (outcome, _s, _p, _c) = run_js_journaled(
            "async (input, ctx) => { const r = await fetch('https://blocked.example/x'); return await r.json(); }",
            &serde_json::json!({}),
            &env,
            FnCtxMeta::default(),
            None,
            steps,
            5,
        )
        .await;
        assert_eq!(outcome.done().expect("done"), serde_json::json!({ "hi": 1 }));
    }

    #[tokio::test]
    async fn fixture_check_passes_matching_fails_mismatching() {
        let (pass, _) = run_fixture_check(
            "(i, ctx) => i.n * 2",
            &[(serde_json::json!({ "n": 2 }), serde_json::json!(4))],
            5,
        )
        .await;
        assert!(pass, "matching fixture should pass");
        let (fail, detail) = run_fixture_check(
            "(i, ctx) => i.n * 2",
            &[(serde_json::json!({ "n": 2 }), serde_json::json!(5))],
            5,
        )
        .await;
        assert!(!fail, "mismatched fixture should fail");
        assert!(detail.contains("expected"), "detail should explain: {detail}");
        // No fixtures → fail (you must author tests).
        let (none, _) = run_fixture_check("(i, ctx) => 1", &[], 5).await;
        assert!(!none);
    }

    #[tokio::test]
    async fn validate_body_accepts_callable_rejects_garbage() {
        assert!(validate_body("async (input, ctx) => ({ ok: true })").await.is_ok());
        assert!(validate_body("(input, ctx) => 1").await.is_ok());
        // Not a function:
        assert!(validate_body("42").await.is_err());
        // Syntax error:
        assert!(validate_body("this is not valid javascript {{{").await.is_err());
    }

    #[tokio::test]
    async fn ctx_cite_collects_touched_objects() {
        let env = BTreeMap::new();
        let (outcome, _steps, _pending, cites) = run_js_journaled(
            "async (input, ctx) => { ctx.cite('claim:a'); ctx.cite('claim:b'); return 1; }",
            &serde_json::json!({}),
            &env,
            FnCtxMeta::default(),
            None,
            HashMap::new(),
            5,
        )
        .await;
        assert_eq!(outcome.done().expect("done"), serde_json::json!(1));
        assert_eq!(cites, vec!["claim:a".to_string(), "claim:b".to_string()]);
    }

    #[tokio::test]
    async fn cognition_suspends_when_unanswered() {
        // No journaled answer ⇒ the run suspends and registers a pending
        // request carrying the question.
        let env = BTreeMap::new();
        let (outcome, _steps, pending, _cites) = run_js_journaled(
            "async (input, ctx) => { const v = await ctx.cognition.ask('churn risk?'); return { v }; }",
            &serde_json::json!({}),
            &env,
            FnCtxMeta::default(),
            None,
            HashMap::new(),
            5,
        )
        .await;
        assert!(outcome.is_suspended(), "expected Suspended, got {outcome:?}");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].question, "churn risk?");
        assert_eq!(pending[0].step_key, "__cog__0");
        assert!(!pending[0].token.is_empty());
    }

    #[tokio::test]
    async fn cognition_resumes_from_journaled_answer() {
        // With the answer journaled under the cognition's step key, the run
        // replays straight through to completion — no second suspend.
        let env = BTreeMap::new();
        let mut steps = HashMap::new();
        steps.insert("__cog__0".to_string(), "true".to_string());
        let (outcome, _steps, pending, _cites) = run_js_journaled(
            "async (input, ctx) => { const v = await ctx.cognition.ask('churn risk?'); return { escalate: v }; }",
            &serde_json::json!({}),
            &env,
            FnCtxMeta::default(),
            None,
            steps,
            5,
        )
        .await;
        assert!(pending.is_empty(), "should not re-suspend once answered");
        assert_eq!(outcome.done().expect("done"), serde_json::json!({ "escalate": true }));
    }

    #[tokio::test]
    async fn ctx_llm_without_oracle_errors_honestly() {
        // No LLM client configured ⇒ ctx.llm.ask must surface a clear,
        // honest error rather than hang or fabricate an answer.
        let env = BTreeMap::new();
        let err = run_js(
            "async (input, ctx) => await ctx.llm.ask('is this a refund?', input)",
            &serde_json::json!({ "msg": "I want my money back" }),
            &env,
            FnCtxMeta::default(),
            None,
            5,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_lowercase().contains("no llm oracle"),
            "expected honest no-oracle error, got: {err}"
        );
    }
}

#[cfg(all(test, not(feature = "root-functions")))]
mod disabled_tests {
    use super::*;

    #[tokio::test]
    async fn disabled_build_returns_typed_error() {
        let env = BTreeMap::new();
        let err = run_js("() => 1", &serde_json::json!({}), &env, FnCtxMeta::default(), None, 5)
            .await
            .unwrap_err();
        assert!(err.contains("root-functions feature is not enabled"));
        assert!(!is_enabled());
    }
}
