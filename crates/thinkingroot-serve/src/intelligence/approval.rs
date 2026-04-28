// crates/thinkingroot-serve/src/intelligence/approval.rs
//
// Approval gates for write tools.
//
// The agent loop (`agent.rs`) consults an `ApprovalGate` for every
// write tool the LLM proposes. Read tools never go through here — the
// LLM may freely call `search`, `ask`, `list_branches`, etc. Writes
// (create_branch, contribute_claim, merge_branch, abandon_branch,
// resolve_contradiction, supersede_claim) are gated so the host can
// surface a confirmation in the UI / require a CLI flag / deny outright.
//
// Three production implementations ship today:
//
//   * [`AutoApprove`]  — always approves. For tests, for the CLI's
//                        `--yolo` mode, and for any call site that has
//                        already collected upstream consent.
//   * [`DenyAll`]      — always rejects. For read-only deployments
//                        (public registry mirror, MCP wrapper that
//                        only proxies queries).
//   * [`ChannelApprovalGate`] — round-trips an approval request
//                        through an mpsc channel. The host (desktop UI
//                        in S5, future CLI prompt) consumes requests
//                        and sends back an [`ApprovalDecision`] via a
//                        oneshot reply channel. This is the production
//                        default for the desktop chat surface.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc, oneshot};

/// What the gate decided. `Approved` lets the agent dispatch the
/// tool. `Rejected` is fed back to the LLM as an `is_error: true`
/// ToolResult so the model can adapt (apologise, ask the user, try a
/// different approach) rather than crash.
#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    Approved,
    Rejected { reason: String },
}

impl ApprovalDecision {
    pub fn is_approved(&self) -> bool {
        matches!(self, ApprovalDecision::Approved)
    }
}

/// Async gate. Implementors decide whether a write tool may run, given
/// the tool name and the JSON input the LLM produced.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn check(&self, tool_name: &str, input: &serde_json::Value) -> ApprovalDecision;
}

/// Always approves. For tests and trusted CLI contexts.
#[derive(Debug, Default, Clone, Copy)]
pub struct AutoApprove;

#[async_trait]
impl ApprovalGate for AutoApprove {
    async fn check(&self, _tool: &str, _input: &serde_json::Value) -> ApprovalDecision {
        ApprovalDecision::Approved
    }
}

/// Always rejects. For read-only deployments. The reason string is
/// stable so callers can detect the deny-all case for observability.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAll;

const DENY_ALL_REASON: &str = "this deployment does not allow agent write tools";

#[async_trait]
impl ApprovalGate for DenyAll {
    async fn check(&self, _tool: &str, _input: &serde_json::Value) -> ApprovalDecision {
        ApprovalDecision::Rejected {
            reason: DENY_ALL_REASON.to_string(),
        }
    }
}

/// One pending approval request — sent from the gate to the host.
#[derive(Debug)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub input: serde_json::Value,
    /// Reply channel. The host consumes the request, decides, and
    /// sends back the [`ApprovalDecision`]. Dropping the sender
    /// counts as a rejection (the gate treats a closed channel as
    /// "the host went away — fail safe").
    pub reply: oneshot::Sender<ApprovalDecision>,
}

/// Production approval gate that routes each request through an mpsc
/// channel. The host (desktop UI, CLI prompt, etc.) holds the
/// receiver and replies via the oneshot inside each request.
///
/// This is the gate the desktop wires in once Sprint S5 lands the UI;
/// the CLI's interactive `root chat` mode wires the same gate to a
/// terminal prompt.
pub struct ChannelApprovalGate {
    sender: mpsc::Sender<ApprovalRequest>,
    /// If the channel send fails (host dropped the receiver), we treat
    /// every subsequent check as a hard reject. This matches the
    /// fail-safe direction: when the human's listening process is
    /// gone, refuse writes rather than dispatch them silently.
    closed: Arc<Mutex<bool>>,
}

impl ChannelApprovalGate {
    /// Build a gate + the receiver the host listens on.
    pub fn new(buffer: usize) -> (Self, mpsc::Receiver<ApprovalRequest>) {
        let (tx, rx) = mpsc::channel(buffer);
        (
            Self {
                sender: tx,
                closed: Arc::new(Mutex::new(false)),
            },
            rx,
        )
    }
}

const CHANNEL_GONE_REASON: &str = "approval channel closed (host receiver dropped)";

#[async_trait]
impl ApprovalGate for ChannelApprovalGate {
    async fn check(&self, tool_name: &str, input: &serde_json::Value) -> ApprovalDecision {
        if *self.closed.lock().await {
            return ApprovalDecision::Rejected {
                reason: CHANNEL_GONE_REASON.to_string(),
            };
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        let request = ApprovalRequest {
            tool_name: tool_name.to_string(),
            input: input.clone(),
            reply: reply_tx,
        };

        if self.sender.send(request).await.is_err() {
            *self.closed.lock().await = true;
            return ApprovalDecision::Rejected {
                reason: CHANNEL_GONE_REASON.to_string(),
            };
        }

        match reply_rx.await {
            Ok(decision) => decision,
            Err(_) => {
                // Host dropped the reply oneshot without sending —
                // most often because the user quit the prompt. Same
                // fail-safe: refuse the write.
                ApprovalDecision::Rejected {
                    reason: "approval pending dropped without decision".to_string(),
                }
            }
        }
    }
}

// ─── HTTP-bridge approval gate ──────────────────────────────────
//
// The streaming `/ask/stream` handler can't use `ChannelApprovalGate`
// directly because the approval reply arrives over a separate HTTP
// POST (`/ask/approval/{id}`) — there is no in-process consumer to
// pump the mpsc receiver. Instead the handler stores a
// `oneshot::Sender<ApprovalDecision>` keyed by tool_use_id in
// [`PendingApprovalMap`], emits an SSE `approval_requested` event,
// and the approval POST looks up the sender and fires it.

/// Map keyed by tool_use_id → reply oneshot. Lives on `AppState`.
pub type PendingApprovalMap =
    Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>;

pub fn new_pending_approval_map() -> PendingApprovalMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Approval gate that registers each pending request in a shared
/// [`PendingApprovalMap`] and waits for the corresponding entry to
/// fire. Used by the streaming agent path so the desktop UI / CLI
/// prompt / external client can post the decision back over HTTP.
///
/// Each tool call routes through one `ToolApprovalRouter::check`. The
/// router keys the oneshot by `tool_use_id` (passed via
/// `with_tool_use_id`) — without one, the gate auto-rejects with a
/// reason rather than risk a missing key.
pub struct ToolApprovalRouter {
    pending: PendingApprovalMap,
    /// Per-call tool_use_id, scoped to the agent invocation. Set by
    /// `with_tool_use_id` before each `check` so the gate can correlate
    /// the request with the SSE event the streaming handler emitted.
    tool_use_id: Mutex<Option<String>>,
}

impl ToolApprovalRouter {
    pub fn new(pending: PendingApprovalMap) -> Self {
        Self {
            pending,
            tool_use_id: Mutex::new(None),
        }
    }

    /// Set the tool_use_id the next `check` will use to register the
    /// pending oneshot. The streaming handler calls this immediately
    /// before the agent dispatches the corresponding write call.
    pub async fn set_pending_id(&self, id: String) {
        *self.tool_use_id.lock().await = Some(id);
    }

    /// Resolve a pending approval — used by the
    /// `/ask/approval/{id}` HTTP handler. Returns `true` when an entry
    /// existed and was fired, `false` otherwise (e.g. the agent
    /// already timed out, or the id is wrong).
    pub async fn resolve(
        pending: &PendingApprovalMap,
        id: &str,
        decision: ApprovalDecision,
    ) -> bool {
        let mut guard = pending.lock().await;
        match guard.remove(id) {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }
}

const NO_PENDING_ID_REASON: &str =
    "internal: ToolApprovalRouter::check called without a tool_use_id";

#[async_trait]
impl ApprovalGate for ToolApprovalRouter {
    async fn check(&self, _tool_name: &str, _input: &serde_json::Value) -> ApprovalDecision {
        let id = {
            let mut guard = self.tool_use_id.lock().await;
            guard.take()
        };
        let Some(id) = id else {
            return ApprovalDecision::Rejected {
                reason: NO_PENDING_ID_REASON.to_string(),
            };
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().await;
            guard.insert(id.clone(), reply_tx);
        }

        match reply_rx.await {
            Ok(decision) => decision,
            Err(_) => {
                // Sender dropped without firing — most often because
                // the user closed the conversation before answering.
                // Clean the stale entry and report a rejection.
                let mut guard = self.pending.lock().await;
                guard.remove(&id);
                ApprovalDecision::Rejected {
                    reason: "approval channel closed before decision".to_string(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn auto_approve_always_approves() {
        let gate = AutoApprove;
        let d = gate.check("create_branch", &json!({"name": "x"})).await;
        assert!(d.is_approved());
    }

    #[tokio::test]
    async fn deny_all_always_rejects_with_stable_reason() {
        let gate = DenyAll;
        match gate.check("create_branch", &json!({"name": "x"})).await {
            ApprovalDecision::Rejected { reason } => {
                assert_eq!(reason, DENY_ALL_REASON);
            }
            ApprovalDecision::Approved => panic!("DenyAll must reject"),
        }
    }

    #[tokio::test]
    async fn channel_gate_round_trips_decision() {
        let (gate, mut rx) = ChannelApprovalGate::new(4);

        // Spawn the "host" — pull one request, approve it.
        tokio::spawn(async move {
            let req = rx.recv().await.expect("expected one request");
            assert_eq!(req.tool_name, "create_branch");
            assert_eq!(req.input["name"], "feat");
            let _ = req.reply.send(ApprovalDecision::Approved);
        });

        let d = gate.check("create_branch", &json!({"name": "feat"})).await;
        assert!(d.is_approved());
    }

    #[tokio::test]
    async fn channel_gate_round_trips_rejection_with_reason() {
        let (gate, mut rx) = ChannelApprovalGate::new(4);
        tokio::spawn(async move {
            let req = rx.recv().await.expect("expected one request");
            let _ = req.reply.send(ApprovalDecision::Rejected {
                reason: "user said no".to_string(),
            });
        });
        let d = gate.check("merge_branch", &json!({"branch": "feat"})).await;
        match d {
            ApprovalDecision::Rejected { reason } => {
                assert_eq!(reason, "user said no");
            }
            _ => panic!("expected rejection"),
        }
    }

    #[tokio::test]
    async fn channel_gate_treats_dropped_receiver_as_reject() {
        let (gate, rx) = ChannelApprovalGate::new(4);
        // Drop the host receiver without consuming.
        drop(rx);
        let d = gate.check("create_branch", &json!({"name": "x"})).await;
        match d {
            ApprovalDecision::Rejected { reason } => {
                assert_eq!(reason, CHANNEL_GONE_REASON);
            }
            _ => panic!("expected rejection when receiver gone"),
        }
        // And it stays rejected on subsequent calls — `closed` flag
        // short-circuits without trying to send through the dead
        // channel.
        let d2 = gate.check("create_branch", &json!({"name": "x"})).await;
        assert!(!d2.is_approved());
    }

    #[tokio::test]
    async fn channel_gate_treats_dropped_reply_as_reject() {
        let (gate, mut rx) = ChannelApprovalGate::new(4);
        // Host receives request but drops the oneshot without sending.
        tokio::spawn(async move {
            let req = rx.recv().await.expect("expected one request");
            drop(req.reply);
        });
        let d = gate.check("create_branch", &json!({"name": "x"})).await;
        assert!(!d.is_approved());
    }

    // ─── ToolApprovalRouter (HTTP bridge) ────────────────────────

    #[tokio::test]
    async fn router_resolve_unblocks_pending_check() {
        let pending = new_pending_approval_map();
        let router = ToolApprovalRouter::new(pending.clone());
        router.set_pending_id("call-1".into()).await;

        let pending_for_resolver = pending.clone();
        tokio::spawn(async move {
            // Wait until the entry is registered, then resolve it.
            for _ in 0..50 {
                if pending_for_resolver.lock().await.contains_key("call-1") {
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
            }
            let resolved = ToolApprovalRouter::resolve(
                &pending_for_resolver,
                "call-1",
                ApprovalDecision::Approved,
            )
            .await;
            assert!(resolved);
        });

        let d = router.check("create_branch", &json!({"name": "x"})).await;
        assert!(d.is_approved());
        // Sender removed from the map.
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn router_resolve_with_rejection_round_trips_reason() {
        let pending = new_pending_approval_map();
        let router = ToolApprovalRouter::new(pending.clone());
        router.set_pending_id("call-2".into()).await;

        let p2 = pending.clone();
        tokio::spawn(async move {
            for _ in 0..50 {
                if p2.lock().await.contains_key("call-2") {
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
            }
            ToolApprovalRouter::resolve(
                &p2,
                "call-2",
                ApprovalDecision::Rejected {
                    reason: "user clicked Reject".to_string(),
                },
            )
            .await;
        });

        match router.check("create_branch", &json!({})).await {
            ApprovalDecision::Rejected { reason } => assert_eq!(reason, "user clicked Reject"),
            _ => panic!("expected rejection"),
        }
    }

    #[tokio::test]
    async fn router_check_without_pending_id_rejects_safely() {
        let pending = new_pending_approval_map();
        let router = ToolApprovalRouter::new(pending);
        // No set_pending_id call → gate must reject with a recognisable
        // reason rather than panic or hang.
        let d = router.check("any", &json!({})).await;
        match d {
            ApprovalDecision::Rejected { reason } => {
                assert_eq!(reason, NO_PENDING_ID_REASON);
            }
            _ => panic!("expected rejection on missing id"),
        }
    }

    #[tokio::test]
    async fn resolve_unknown_id_is_a_noop() {
        let pending = new_pending_approval_map();
        let resolved = ToolApprovalRouter::resolve(
            &pending,
            "nonexistent",
            ApprovalDecision::Approved,
        )
        .await;
        assert!(!resolved);
    }
}
