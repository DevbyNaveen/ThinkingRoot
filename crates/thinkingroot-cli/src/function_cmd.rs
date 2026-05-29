//! `root function {deploy,list,invoke}` — manage Root Functions over the
//! engine REST API (a running `root serve`). The engine stores the JS
//! body and runs it in its `deno_core` isolate.

use std::path::Path;

use anyhow::{Context, Result};

/// Send a request and unwrap the engine's `{ok, data, error}` envelope.
pub(crate) async fn send_envelope(req: reqwest::RequestBuilder) -> Result<serde_json::Value> {
    let resp = req.send().await.context("send request to engine")?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .context("parse engine response as JSON")?;
    if body.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        let msg = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("engine returned {status}: {msg}");
    }
    Ok(body.get("data").cloned().unwrap_or(serde_json::Value::Null))
}

pub(crate) fn with_auth(
    req: reqwest::RequestBuilder,
    api_key: Option<&str>,
) -> reqwest::RequestBuilder {
    match api_key {
        Some(k) => req.bearer_auth(k),
        None => req,
    }
}

pub async fn deploy(
    name: &str,
    code: &Path,
    workspace: &str,
    url: &str,
    api_key: Option<&str>,
) -> Result<()> {
    let body = std::fs::read_to_string(code)
        .with_context(|| format!("read function body from {}", code.display()))?;
    let client = reqwest::Client::new();
    let req = with_auth(
        client
            .put(format!("{url}/api/v1/ws/{workspace}/functions"))
            .json(&serde_json::json!({ "name": name, "body": body, "language": "js" })),
        api_key,
    );
    let data = send_envelope(req).await?;
    let version = data.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
    println!("✓ deployed '{name}' as version {version}");
    Ok(())
}

pub async fn list(workspace: &str, url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let req = with_auth(
        client.get(format!("{url}/api/v1/ws/{workspace}/functions")),
        api_key,
    );
    let data = send_envelope(req).await?;
    let arr = data.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        println!("No functions deployed in workspace '{workspace}'.");
    } else {
        for f in arr {
            let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let version = f.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
            println!("{name}  (v{version})");
        }
    }
    Ok(())
}

pub async fn invoke(
    name: &str,
    input: &str,
    workspace: &str,
    url: &str,
    api_key: Option<&str>,
) -> Result<()> {
    let input_json: serde_json::Value =
        serde_json::from_str(input).context("--input must be valid JSON")?;
    let client = reqwest::Client::new();
    let req = with_auth(
        client
            .post(format!("{url}/api/v1/ws/{workspace}/functions/{name}/invoke"))
            .json(&serde_json::json!({ "input": input_json })),
        api_key,
    );
    let data = send_envelope(req).await?;
    let result = data.get("result").cloned().unwrap_or(serde_json::Value::Null);
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
