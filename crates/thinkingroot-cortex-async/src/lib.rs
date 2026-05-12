//! Shared async probe for the cortex `/livez` endpoint.  Lives in
//! its own tiny crate so CLI and desktop can both depend on it
//! without depending on each other.
//!
//! `thinkingroot-core::cortex` stays sync (no tokio/reqwest); this
//! crate is the async wrapper.
//!
//! Spec: `docs/superpowers/specs/2026-05-11-install-runtime-smoothness-design.md` §3.

use std::time::Duration;

use thinkingroot_core::cortex::{LIVENESS_PATH, ProbeResult};

/// Probe `http://{host}:{port}/livez` and return the structured
/// `ProbeResult`.  Never panics.  Any I/O error, non-2xx response,
/// or timeout maps to `ProbeResult::Unhealthy`.
///
/// `timeout` is applied per-request via reqwest's builder.
pub async fn probe_livez(host: &str, port: u16, timeout: Duration) -> ProbeResult {
    let url = format!("http://{}:{}{}", host, port, LIVENESS_PATH);
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "reqwest client build failed; treating as Unhealthy");
            return ProbeResult::Unhealthy;
        }
    };
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(url, error = %e, "probe failed");
            return ProbeResult::Unhealthy;
        }
    };
    if !resp.status().is_success() {
        tracing::debug!(url, status = %resp.status(), "probe non-2xx");
        return ProbeResult::Unhealthy;
    }
    // Try to extract version + warnings from the JSON body. If the
    // body isn't JSON or doesn't carry the fields, fall back to a
    // synthetic Healthy with empty version — the caller can still
    // attach. Use `take` to consume the response into bytes once.
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return ProbeResult::Healthy { version: String::new() },
    };
    #[derive(serde::Deserialize)]
    struct LivezBody {
        #[serde(default)]
        status: String,
        #[serde(default)]
        version: String,
        #[serde(default)]
        warnings: Vec<String>,
    }
    let parsed: LivezBody = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(_) => return ProbeResult::Healthy { version: String::new() },
    };
    if parsed.status == "degraded" && !parsed.warnings.is_empty() {
        ProbeResult::Degraded {
            version: parsed.version,
            warnings: parsed.warnings,
        }
    } else {
        ProbeResult::Healthy { version: parsed.version }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::cortex::ProbeResult;

    #[tokio::test]
    async fn probe_livez_returns_unhealthy_for_unbound_port() {
        let result = probe_livez("127.0.0.1", 1, std::time::Duration::from_millis(200)).await;
        assert!(matches!(result, ProbeResult::Unhealthy), "got: {result:?}");
    }

    #[tokio::test]
    async fn probe_livez_returns_healthy_when_server_replies_2xx() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let body = "{\"status\":\"ok\",\"version\":\"0.9.1-test\",\"uptime_seconds\":0,\"warnings\":[]}";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let result = probe_livez("127.0.0.1", port, std::time::Duration::from_secs(2)).await;
        match result {
            ProbeResult::Healthy { version } => {
                assert_eq!(version, "0.9.1-test", "version should reflect server reply");
            }
            other => panic!("expected Healthy, got: {other:?}"),
        }
    }
}
