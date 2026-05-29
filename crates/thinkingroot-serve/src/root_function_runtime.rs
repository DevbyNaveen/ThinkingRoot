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
//! async (input, env) => {
//!   const r = await fetch(env.SOME_API + "/x");   // egress-gated
//!   return { ok: r.ok };
//! }
//! ```
//! The runtime invokes `(<body>)(input, env)`, awaits the result if it
//! is a promise, and returns it as JSON. `input` is the node/REST
//! argument; `env` is the resolved secret map (cloud env vars overlaid
//! on the local `secrets.toml`, see `thinkingroot_cloud_auth::secrets`).
//!
//! ## !Send isolation
//!
//! `deno_core::JsRuntime` is `!Send`, so it cannot live across `.await`
//! on the multi-thread tokio runtime the daemon uses. The enabled path
//! confines the entire runtime to one `spawn_blocking` thread with its
//! own current-thread executor; only the `serde_json::Value` result
//! (which is `Send`) crosses back.

use std::collections::BTreeMap;

use serde_json::Value;

/// Outcome of a single function execution: the JSON return value, or a
/// user-facing error message.
pub type RunResult = Result<Value, String>;

/// Run a Root Function body. `timeout_secs` bounds wall-clock; `env` is
/// injected as the second argument. See module docs for the contract.
#[cfg(feature = "root-functions")]
pub async fn run_js(
    body: &str,
    input: &Value,
    env: &BTreeMap<String, String>,
    timeout_secs: u64,
) -> RunResult {
    let body = body.to_string();
    let input_json = serde_json::to_string(input).map_err(|e| e.to_string())?;
    let env_json = serde_json::to_string(env).map_err(|e| e.to_string())?;

    // Confine the !Send JsRuntime to a dedicated blocking thread with
    // its own current-thread tokio runtime + LocalSet.
    let handle = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("build js executor runtime: {e}"))?;
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            execute_in_isolate(&body, &input_json, &env_json, timeout_secs).await
        })
    });

    match handle.await {
        Ok(r) => r,
        Err(e) => Err(format!("root function task panicked or was cancelled: {e}")),
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

        let method = req.method.unwrap_or_else(|| "GET".to_string());
        let m = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| JsErrorBox::type_error(format!("invalid HTTP method `{method}`")))?;
        let client = reqwest::Client::new();
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

#[cfg(feature = "root-functions")]
async fn execute_in_isolate(
    body: &str,
    input_json: &str,
    env_json: &str,
    timeout_secs: u64,
) -> RunResult {
    use deno_core::{JsRuntime, RuntimeOptions, v8};

    // Wrap the user body so it is invoked with (input, env) and any
    // returned promise is awaited. The IIFE returns the resolved value.
    let code = format!(
        "(async () => {{ const __f = ({body}); return await __f({input_json}, {env_json}); }})()"
    );

    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![fetch_ext::extension()],
        ..Default::default()
    });

    // Install the `fetch` shim before user code runs.
    if let Err(e) = runtime.execute_script("[tr_fetch_bootstrap]", FETCH_BOOTSTRAP) {
        return Err(format!("fetch bootstrap failed: {e}"));
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
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(format!("root function timed out after {timeout_secs}s")),
        }
    };

    // Decode the resolved v8 value into JSON. `deno_core::scope!` opens a
    // HandleScope + ContextScope on the runtime's isolate (see the
    // crate's `examples/eval_js_value.rs`).
    deno_core::scope!(scope, &mut runtime);
    let local = v8::Local::new(scope, resolved);
    deno_core::serde_v8::from_v8::<Value>(scope, local)
        .map_err(|e| format!("result decode error: {e}"))
}

/// Feature-off path: the engine was built without `root-functions`, so
/// no JS isolate is linked. Honest typed error (not a silent success).
#[cfg(not(feature = "root-functions"))]
pub async fn run_js(
    _body: &str,
    _input: &Value,
    _env: &BTreeMap<String, String>,
    _timeout_secs: u64,
) -> RunResult {
    Err(
        "root-functions feature is not enabled in this build — rebuild `thinkingroot-serve` \
         with `--features root-functions` (the cloud image does; the desktop build does not)"
            .to_string(),
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
            5,
        )
        .await
        .expect("js runs");
        assert_eq!(out["doubled"], serde_json::json!(42));
    }

    #[tokio::test]
    async fn reads_env_secret_and_awaits_async() {
        let mut env = BTreeMap::new();
        env.insert("WHO".to_string(), "ada".to_string());
        let out = run_js(
            "async (input, env) => env.WHO + '!' ",
            &serde_json::json!({}),
            &env,
            5,
        )
        .await
        .expect("async js runs");
        assert_eq!(out, serde_json::json!("ada!"));
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
        let err = run_js("() => { throw new Error('boom') }", &serde_json::json!({}), &env, 5)
            .await
            .unwrap_err();
        assert!(err.to_lowercase().contains("boom") || err.contains("evaluation error"));
    }
}

#[cfg(all(test, not(feature = "root-functions")))]
mod disabled_tests {
    use super::*;

    #[tokio::test]
    async fn disabled_build_returns_typed_error() {
        let env = BTreeMap::new();
        let err = run_js("() => 1", &serde_json::json!({}), &env, 5)
            .await
            .unwrap_err();
        assert!(err.contains("root-functions feature is not enabled"));
        assert!(!is_enabled());
    }
}
