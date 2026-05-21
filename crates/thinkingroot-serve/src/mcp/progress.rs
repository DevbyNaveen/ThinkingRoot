//! C4 (2026-05-22) — MCP `notifications/progress` emission helper.
//!
//! Per MCP spec, `tools/call` requests can include a `_meta.progressToken`
//! (string or integer). When present, the server SHOULD emit
//! `notifications/progress` notifications mid-call, keyed by that
//! token, so the client can render progress UI without waiting for
//! the final response.
//!
//! This module bridges the daemon's existing in-process
//! `tokio::sync::mpsc` progress channel (used by the unified compile
//! pipeline at `rest.rs::run_unified_compile`) onto the SSE
//! session's `SseMsg::Message` sender, framing each pipeline event
//! as a JSON-RPC notification.
//!
//! Wire shape per MCP 2025-03-26 spec:
//! ```json
//! {
//!   "jsonrpc": "2.0",
//!   "method": "notifications/progress",
//!   "params": {
//!     "progressToken": "<token>",
//!     "progress": <current>,
//!     "total": <total>,
//!     "message": "<optional human-readable phase label>"
//!   }
//! }
//! ```
//!
//! Notifications never carry a JSON-RPC `id` (notifications are
//! one-way per JSON-RPC 2.0 §4.1).

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use super::sse::SseMsg;

/// Extract the `_meta.progressToken` field from a `tools/call`
/// arguments object. Returns `None` when the field is absent or
/// not a string/integer (per MCP spec the token MAY be either).
/// String form is used directly; integer form is stringified for
/// uniform downstream handling — the wire shape that goes back to
/// the client preserves the original kind via `progress_token_value`.
pub fn extract_progress_token(arguments: &Value) -> Option<Value> {
    arguments
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .cloned()
        .filter(|v| v.is_string() || v.is_number())
}

/// Frame a JSON-RPC `notifications/progress` and push it onto the
/// SSE session's outbound channel. Best-effort — a send failure
/// means the SSE consumer dropped (transport closed); the caller
/// stops emitting on subsequent failures naturally because the
/// channel sink itself goes dead.
pub fn emit_progress_notification(
    session_sender: &UnboundedSender<SseMsg>,
    progress_token: &Value,
    progress: f64,
    total: Option<f64>,
    message: Option<&str>,
) -> Result<(), tokio::sync::mpsc::error::SendError<SseMsg>> {
    let mut params = serde_json::json!({
        "progressToken": progress_token,
        "progress": progress,
    });
    if let Some(t) = total {
        params["total"] = serde_json::json!(t);
    }
    if let Some(m) = message {
        params["message"] = serde_json::json!(m);
    }
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": params,
    });
    let json = serde_json::to_string(&frame).expect("frame serialize");
    session_sender.send(SseMsg::Message(json))
}

/// Project a `ProgressEvent` from the unified compile pipeline onto
/// `(progress, total, message)` for MCP emission. Different
/// `ProgressEvent` variants carry different shapes; this helper is
/// the single mapping place so the compile-progress wire shape
/// stays consistent.
///
/// The mapping is honest: variants that carry `(done, total)` map to
/// `progress=done, total=Some(total)`; phase-boundary variants
/// (PhaseDone, ParseStart, etc.) emit `progress=1.0, total=None`
/// with the human-readable phase name as `message`. Variants with
/// no numeric progress (CompilationDone, etc.) emit `progress=1.0,
/// total=None` with a short status string. Returns `None` for
/// terminal events (IncrementalDone, PipelineFailed, CompileTick)
/// that the caller handles separately as the final response.
pub fn project_compile_progress(
    evt: &crate::pipeline::ProgressEvent,
) -> Option<(f64, Option<f64>, Option<String>)> {
    use crate::pipeline::ProgressEvent as PE;
    match evt {
        // Phase boundary events — emit 1.0 with phase name.
        PE::PhaseDone { name, elapsed_ms } => Some((
            1.0,
            None,
            Some(format!("phase done: {name} ({elapsed_ms}ms)")),
        )),
        PE::ParseStart => Some((0.0, None, Some("parsing sources".to_string()))),
        PE::ParseComplete { files } => Some((
            *files as f64,
            Some(*files as f64),
            Some(format!("parsed {files} files")),
        )),
        PE::DiffStart => Some((0.0, None, Some("diffing against graph".to_string()))),
        PE::DiffComplete {
            changed,
            unchanged,
            deleted,
        } => Some((
            1.0,
            None,
            Some(format!(
                "diff complete: {changed} changed, {unchanged} unchanged, {deleted} deleted"
            )),
        )),
        // Extraction — explicit done/total flow.
        PE::ExtractionStart {
            total_chunks,
            total_batches,
            ..
        } => Some((
            0.0,
            Some(*total_chunks as f64),
            Some(format!("extracting {total_chunks} chunks in {total_batches} batches")),
        )),
        PE::ExtractionBatchStart {
            batch_index,
            total_batches,
            ..
        } => Some((
            *batch_index as f64,
            Some(*total_batches as f64),
            Some(format!("batch {batch_index}/{total_batches}")),
        )),
        PE::ChunkDone {
            done,
            total,
            source_uri,
        } => Some((
            *done as f64,
            Some(*total as f64),
            Some(format!("chunk {done}/{total} ({source_uri})")),
        )),
        PE::ExtractionComplete {
            claims, entities, ..
        } => Some((
            1.0,
            None,
            Some(format!(
                "extraction complete: {claims} claims, {entities} entities"
            )),
        )),
        PE::ExtractionPartial { failed_batches, .. } => Some((
            1.0,
            None,
            Some(format!("extraction PARTIAL: {failed_batches} batches failed")),
        )),
        // Grounding + Witness Mesh + Linking + Vectors + Rooting —
        // explicit done/total wherever available.
        PE::GroundingStart { .. } => Some((0.0, None, Some("grounding".to_string()))),
        PE::GroundingProgress { done, total } => Some((
            *done as f64,
            Some(*total as f64),
            Some(format!("grounding {done}/{total}")),
        )),
        PE::GroundingDone { accepted, rejected } => Some((
            1.0,
            None,
            Some(format!("grounding done: {accepted} accepted, {rejected} rejected")),
        )),
        PE::WitnessMeshStart { raw } => Some((
            0.0,
            Some(*raw as f64),
            Some(format!("witness mesh: {raw} raw witnesses")),
        )),
        PE::WitnessMeshDone { .. } => {
            Some((1.0, None, Some("witness mesh done".to_string())))
        }
        PE::FingerprintDone { .. } => {
            Some((1.0, None, Some("fingerprints saved".to_string())))
        }
        PE::LinkingStart { total_entities } => Some((
            0.0,
            Some(*total_entities as f64),
            Some(format!("linking {total_entities} entities")),
        )),
        PE::EntityResolved { done, total } => Some((
            *done as f64,
            Some(*total as f64),
            Some(format!("entity {done}/{total}")),
        )),
        PE::LinkComplete { .. } => Some((1.0, None, Some("linking complete".to_string()))),
        PE::VectorUpdateDone { .. } => Some((1.0, None, Some("vectors updated".to_string()))),
        PE::VectorProgress { done, total } => Some((
            *done as f64,
            Some(*total as f64),
            Some(format!("vector {done}/{total}")),
        )),
        PE::CompilationDone { artifacts } => Some((
            1.0,
            None,
            Some(format!("compilation done: {artifacts} artifacts")),
        )),
        PE::CompilationProgress { done, total } => Some((
            *done as f64,
            Some(*total as f64),
            Some(format!("compiling {done}/{total}")),
        )),
        PE::VerificationDone { health } => Some((
            1.0,
            None,
            Some(format!("verified: health {health}/100")),
        )),
        PE::RootingStart { candidates } => Some((
            0.0,
            Some(*candidates as f64),
            Some(format!("rooting {candidates} candidates")),
        )),
        PE::RootingProgress { done, total } => Some((
            *done as f64,
            Some(*total as f64),
            Some(format!("rooting {done}/{total}")),
        )),
        PE::RootingDone { .. } => Some((1.0, None, Some("rooting done".to_string()))),
        // Terminal events — handled by the caller as the final
        // response, not as a progress notification.
        PE::IncrementalDone { .. } | PE::PipelineFailed { .. } | PE::CompileTick(_) => None,
        // Catch-all for variants we don't have a dedicated
        // projection for yet (e.g., GroundingModelReady). Emit
        // their Debug form as a status message with no numeric
        // progress — honest non-fabrication.
        other => Some((0.0, None, Some(format!("{other:?}")))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn extract_token_handles_string() {
        let args = serde_json::json!({
            "_meta": { "progressToken": "compile-1" },
            "workspace": "ws-a"
        });
        let tok = extract_progress_token(&args);
        assert_eq!(tok, Some(serde_json::json!("compile-1")));
    }

    #[test]
    fn extract_token_handles_integer() {
        let args = serde_json::json!({
            "_meta": { "progressToken": 42 },
            "workspace": "ws-a"
        });
        let tok = extract_progress_token(&args);
        assert_eq!(tok, Some(serde_json::json!(42)));
    }

    #[test]
    fn extract_token_returns_none_when_absent() {
        let args = serde_json::json!({ "workspace": "ws-a" });
        assert_eq!(extract_progress_token(&args), None);
    }

    #[test]
    fn extract_token_rejects_non_string_non_number() {
        let args = serde_json::json!({
            "_meta": { "progressToken": { "nested": "obj" } },
            "workspace": "ws-a"
        });
        assert_eq!(extract_progress_token(&args), None);
    }

    #[tokio::test]
    async fn emit_frames_well_formed_jsonrpc_notification() {
        let (tx, mut rx) = mpsc::unbounded_channel::<SseMsg>();
        emit_progress_notification(
            &tx,
            &serde_json::json!("compile-1"),
            3.0,
            Some(12.0),
            Some("batch 3/12"),
        )
        .expect("send ok");

        let msg = rx.recv().await.expect("received");
        let payload = match msg {
            SseMsg::Message(s) => s,
            _ => panic!("expected SseMsg::Message"),
        };
        let v: serde_json::Value = serde_json::from_str(&payload).expect("parse");
        assert_eq!(v["jsonrpc"], serde_json::json!("2.0"));
        assert_eq!(v["method"], serde_json::json!("notifications/progress"));
        assert_eq!(
            v["params"]["progressToken"],
            serde_json::json!("compile-1")
        );
        assert_eq!(v["params"]["progress"], serde_json::json!(3.0));
        assert_eq!(v["params"]["total"], serde_json::json!(12.0));
        assert_eq!(v["params"]["message"], serde_json::json!("batch 3/12"));
        // Notifications MUST NOT have an id per JSON-RPC 2.0 §4.1.
        assert!(v.get("id").is_none());
    }

    #[tokio::test]
    async fn emit_omits_total_and_message_when_absent() {
        let (tx, mut rx) = mpsc::unbounded_channel::<SseMsg>();
        emit_progress_notification(&tx, &serde_json::json!(7), 0.5, None, None)
            .expect("send ok");

        let msg = rx.recv().await.expect("received");
        let payload = match msg {
            SseMsg::Message(s) => s,
            _ => panic!("expected SseMsg::Message"),
        };
        let v: serde_json::Value = serde_json::from_str(&payload).expect("parse");
        assert_eq!(v["params"]["progressToken"], serde_json::json!(7));
        assert_eq!(v["params"]["progress"], serde_json::json!(0.5));
        assert!(v["params"].get("total").is_none());
        assert!(v["params"].get("message").is_none());
    }

    #[test]
    fn project_compile_progress_maps_phase_done() {
        let evt = crate::pipeline::ProgressEvent::PhaseDone {
            name: "extract".to_string(),
            elapsed_ms: 420,
        };
        let projected = project_compile_progress(&evt).expect("non-terminal");
        let (p, t, m) = projected;
        assert_eq!(p, 1.0);
        assert_eq!(t, None);
        let msg = m.unwrap();
        assert!(msg.contains("extract"));
        assert!(msg.contains("420ms"));
    }

    #[test]
    fn project_compile_progress_maps_extraction_batch_start() {
        let evt = crate::pipeline::ProgressEvent::ExtractionBatchStart {
            batch_index: 3,
            total_batches: 12,
            range_start: 100,
            range_end: 200,
            batch_chunks: 50,
        };
        let (p, t, m) = project_compile_progress(&evt).expect("non-terminal");
        assert_eq!(p, 3.0);
        assert_eq!(t, Some(12.0));
        assert!(m.unwrap().contains("3/12"));
    }

    #[test]
    fn project_compile_progress_maps_chunk_done_with_source() {
        let evt = crate::pipeline::ProgressEvent::ChunkDone {
            done: 42,
            total: 100,
            source_uri: "file:///src/main.rs".to_string(),
        };
        let (p, t, m) = project_compile_progress(&evt).expect("non-terminal");
        assert_eq!(p, 42.0);
        assert_eq!(t, Some(100.0));
        let msg = m.unwrap();
        assert!(msg.contains("42/100"));
        assert!(msg.contains("file:///src/main.rs"));
    }

    #[test]
    fn project_compile_progress_returns_none_for_terminal_events() {
        let evt = crate::pipeline::ProgressEvent::PipelineFailed {
            error: "doom".to_string(),
        };
        assert!(project_compile_progress(&evt).is_none());
    }
}
