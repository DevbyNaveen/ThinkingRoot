//! Engine telemetry export — optional OTLP/HTTP-JSON shipping of tracing
//! events to a collector (OpenObserve / Tempo / Honeycomb / Grafana Cloud).
//!
//! Mirrors the control-plane `tr-common::telemetry` shim so the engine speaks
//! the same wire format with the same env contract — NO heavy `opentelemetry`
//! / `tonic` dependency (that adds ~40 MB + ~30 s compile for marginal gain
//! over this small hand-rolled OTLP/HTTP-JSON layer; we POST batches with a
//! plain tokio TCP write, reusing deps the engine already compiles in).
//!
//! Contract (identical to the cloud services):
//! * unset `OTEL_EXPORTER_OTLP_ENDPOINT` → stderr only (today's behaviour, zero
//!   cost — no task, no socket).
//! * set it → events are ALSO POSTed to `<endpoint>/v1/logs` as OTLP/HTTP-JSON,
//!   with `OTEL_EXPORTER_OTLP_HEADERS` (`key=value,key2=value2`) for auth.
//! The provisioner sets both on every spawned engine, so per-project engine
//! logs (incl. the `retrieval_complete` timing event) land in OpenObserve next
//! to the gateway / provisioner / identity / billing streams.

use std::time::Duration;

use serde_json::{json, Value};
use tracing::field::{Field, Visit};
use tracing::span;
use tracing::subscriber::Subscriber;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Initialise the global subscriber: stderr `fmt` (engine's existing format) +
/// an OTLP layer when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. `filter` is the
/// already-resolved `EnvFilter` (the engine builds it from its own default
/// directive). Idempotent via `try_init`.
pub fn init_tracing(service_name: &'static str, filter: EnvFilter) {
    let fmt = tracing_subscriber::fmt::layer()
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr); // always stderr, never stdout

    let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty());
    let otlp_header_block = render_otlp_header_block(
        &std::env::var("OTEL_EXPORTER_OTLP_HEADERS").unwrap_or_default(),
    );

    let registry = tracing_subscriber::registry().with(filter).with(fmt);

    if let Some(endpoint) = otlp_endpoint {
        let otlp = OtlpHttpLayer::new(service_name.to_string(), endpoint, otlp_header_block);
        let _ = registry.with(otlp).try_init();
    } else {
        let _ = registry.try_init();
    }
}

/// Parse OTel-standard `OTEL_EXPORTER_OTLP_HEADERS` (`key=value,key2=value2`)
/// into a ready-to-splice HTTP/1.1 header block (`Key: Value\r\n` per pair,
/// empty when none). Values may contain `=` (split on the first only).
fn render_otlp_header_block(raw: &str) -> String {
    let mut out = String::new();
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            let (k, v) = (k.trim(), v.trim());
            if !k.is_empty() {
                out.push_str(k);
                out.push_str(": ");
                out.push_str(v);
                out.push_str("\r\n");
            }
        }
    }
    out
}

// ── OTLP/HTTP-JSON shim ──────────────────────────────────────────────

/// Layer that buffers tracing events and POSTs them to an OTLP/HTTP endpoint
/// as `logs` records. Logs-only (no span trees) — trace context can be added
/// later without changing call sites.
struct OtlpHttpLayer {
    sender: tokio::sync::mpsc::UnboundedSender<OtlpRecord>,
    service_name: String,
}

impl OtlpHttpLayer {
    fn new(service_name: String, endpoint: String, header_block: String) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<OtlpRecord>();
        let svc_for_task = service_name.clone();
        // Spawn the exporter task. If no Tokio runtime is running yet, the
        // spawn is skipped and the Layer falls back to stderr-only (the send
        // errors are ignored).
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(run_exporter(svc_for_task, endpoint, header_block, rx));
        }
        Self {
            sender: tx,
            service_name,
        }
    }
}

#[derive(Debug, Clone)]
struct OtlpRecord {
    name: String,
    target: String,
    level: String,
    fields: Value,
    time_unix_nano: u128,
}

impl<S> Layer<S> for OtlpHttpLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut visitor = JsonVisitor(serde_json::Map::new());
        event.record(&mut visitor);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let metadata = event.metadata();
        let record = OtlpRecord {
            name: metadata.name().to_string(),
            target: metadata.target().to_string(),
            level: metadata.level().to_string(),
            fields: Value::Object(visitor.0),
            time_unix_nano: now,
        };
        let _ = self.sender.send(record);
    }

    fn on_new_span(&self, attrs: &span::Attributes<'_>, _: &span::Id, _: Context<'_, S>) {
        let mut visitor = JsonVisitor(serde_json::Map::new());
        attrs.record(&mut visitor);
        let _ = visitor;
        let _ = &self.service_name;
    }
}

struct JsonVisitor(serde_json::Map<String, Value>);

impl Visit for JsonVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), json!(format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
}

async fn run_exporter(
    service_name: String,
    endpoint: String,
    header_block: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<OtlpRecord>,
) {
    // Batch up to 64 records or flush every 2s; drop on backpressure rather
    // than block the producer (a broken collector must never stall the engine).
    let mut buf: Vec<OtlpRecord> = Vec::with_capacity(64);
    let mut flush = tokio::time::interval(Duration::from_secs(2));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            r = rx.recv() => {
                match r {
                    Some(rec) => {
                        buf.push(rec);
                        if buf.len() >= 64 {
                            let drain: Vec<_> = buf.drain(..).collect();
                            ship(&service_name, &endpoint, &header_block, drain).await;
                        }
                    }
                    None => break,
                }
            }
            _ = flush.tick() => {
                if !buf.is_empty() {
                    let drain: Vec<_> = buf.drain(..).collect();
                    ship(&service_name, &endpoint, &header_block, drain).await;
                }
            }
        }
    }
    if !buf.is_empty() {
        ship(&service_name, &endpoint, &header_block, buf).await;
    }
}

async fn ship(service_name: &str, endpoint: &str, header_block: &str, batch: Vec<OtlpRecord>) {
    let resource_logs = json!({
        "resourceLogs": [{
            "resource": {
                "attributes": [
                    { "key": "service.name", "value": { "stringValue": service_name } }
                ]
            },
            "scopeLogs": [{
                "scope": { "name": "thinkingroot.engine" },
                "logRecords": batch.iter().map(|r| json!({
                    "timeUnixNano": r.time_unix_nano.to_string(),
                    "severityText": r.level,
                    "body": { "stringValue": r.name },
                    "attributes": [
                        { "key": "target", "value": { "stringValue": r.target } },
                        { "key": "fields", "value": { "stringValue": r.fields.to_string() } }
                    ]
                })).collect::<Vec<_>>()
            }]
        }]
    });
    let url = format!("{}/v1/logs", endpoint.trim_end_matches('/'));
    if let Err(e) = post_json(&url, &resource_logs, header_block).await {
        // Never panic, never recurse into tracing — stderr directly.
        eprintln!("[otlp] export to {url} failed: {e}");
    }
}

async fn post_json(url: &str, body: &Value, header_block: &str) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let parsed = url::parse(url).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("bad url {url}"))
    })?;
    if parsed.scheme != "http" {
        return Err(std::io::Error::other(format!(
            "telemetry endpoint must be http (got {})",
            parsed.scheme
        )));
    }
    let payload = serde_json::to_vec(body).unwrap_or_default();
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
        parsed.path,
        parsed.host_header(),
        header_block,
        payload.len()
    );
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(parsed.authority()),
    )
    .await
    .map_err(|_| std::io::Error::other("connect timeout"))??;
    stream.write_all(req.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    let mut sink = Vec::with_capacity(256);
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut sink)).await;
    Ok(())
}

mod url {
    pub struct Parsed<'a> {
        pub scheme: &'a str,
        host: &'a str,
        port: u16,
        pub path: &'a str,
    }

    impl<'a> Parsed<'a> {
        pub fn host_header(&self) -> String {
            if (self.scheme == "http" && self.port == 80)
                || (self.scheme == "https" && self.port == 443)
            {
                self.host.to_string()
            } else {
                format!("{}:{}", self.host, self.port)
            }
        }
        pub fn authority(&self) -> String {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn parse(url: &str) -> Option<Parsed<'_>> {
        let (scheme, rest) = url.split_once("://")?;
        let default_port = match scheme {
            "http" => 80,
            "https" => 443,
            _ => return None,
        };
        let (authority, path_part) = rest
            .split_once('/')
            .map(|(a, _p)| (a, &rest[a.len()..]))
            .unwrap_or((rest, "/"));
        let (host, port) = match authority.split_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (authority, default_port),
        };
        let path = if path_part.is_empty() { "/" } else { path_part };
        Some(Parsed {
            scheme,
            host,
            port,
            path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::render_otlp_header_block;
    use super::url::parse;

    #[test]
    fn otlp_header_block_renders_otel_standard_headers() {
        assert_eq!(
            render_otlp_header_block("Authorization=Basic abc123"),
            "Authorization: Basic abc123\r\n"
        );
        assert_eq!(
            render_otlp_header_block(" Authorization=Basic xyz , X-Env=prod "),
            "Authorization: Basic xyz\r\nX-Env: prod\r\n"
        );
        assert_eq!(render_otlp_header_block("k=a=b"), "k: a=b\r\n");
        assert_eq!(render_otlp_header_block(""), "");
        assert_eq!(render_otlp_header_block("=novalue"), "");
    }

    #[test]
    fn parses_http_url_and_rejects_other_schemes() {
        let p = parse("http://localhost:5080/v1/logs").unwrap();
        assert_eq!(p.scheme, "http");
        assert_eq!(p.path, "/v1/logs");
        assert_eq!(p.authority(), "localhost:5080");
        assert!(parse("ftp://x/y").is_none());
    }
}
