//! M1 — co-located `ctx.memory` for Root Functions.
//!
//! Proves a Root Function can `remember`/`recall` over its OWN workspace's
//! cognition graph, in-process, with durable-execution guarantees:
//!   1. `remember` persists a claim, and a replay/resume of the SAME run does
//!      NOT double-write (deterministic claim id + journaled step → exactly
//!      one claim).
//!   2. `remember` is confined to the run's own (per-user) workspace — no
//!      cross-workspace leakage.
//!   3. `ctx.memory` is actually wired into the isolate (the op is reached),
//!      independent of whether the ONNX embedder is staged.
//!
//! These assertions are **model-independent** (they use graph writes +
//! `list_claims`, never the ONNX embedder, which is staged only in the cloud
//! image). The full *semantic* recall round-trip is exercised in the Azure
//! e2e (M5), where the embed model is present. Requires
//! `--features root-functions` (the isolate must actually run).

use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::{ClaimFilter, QueryEngine};

fn all_claims_filter() -> ClaimFilter {
    ClaimFilter {
        claim_type: None,
        entity_name: None,
        min_confidence: None,
        limit: None,
        offset: None,
    }
}

async fn engine_with_ws(ws: &str) -> (QueryEngine, PathBuf, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    GraphStore::init(&graph_dir).unwrap();
    let mut engine = QueryEngine::new();
    engine.mount(ws.to_string(), root.clone()).await.unwrap();
    (engine, root, dir)
}

/// remember() persists exactly one claim, and replaying the SAME run_id does
/// not duplicate it — the durable-execution exactly-once guarantee for the
/// write effect (journaled step + deterministic id).
#[tokio::test]
async fn remember_persists_and_is_idempotent_on_replay() {
    let (engine, _root, _dir) = engine_with_ws("acme").await;

    const SAVE: &str = r#"
        async (i, ctx) => {
          const id = await ctx.memory.remember(i.fact, { type: "fact", confidence: 0.9 });
          return { id };
        }
    "#;
    engine.put_function("acme", "save", SAVE, "js").await.unwrap();

    let input = serde_json::json!({ "fact": "The capital of France is Paris" });

    // First execution journals the remember step + writes the claim.
    let out1 = engine
        .run_function_with_id("acme", "save", &input, "fixed_run_1")
        .await
        .unwrap();
    // Replay/resume of the SAME run_id: the journaled step is returned and the
    // write op is NOT re-executed.
    let out2 = engine
        .run_function_with_id("acme", "save", &input, "fixed_run_1")
        .await
        .unwrap();

    assert!(out1["id"].is_string(), "remember must return a claim id: {out1}");
    assert_eq!(
        out1["id"], out2["id"],
        "deterministic claim id must be stable across replay"
    );

    let claims = engine.list_claims("acme", all_claims_filter()).await.unwrap();
    let matching = claims
        .iter()
        .filter(|c| c.statement.contains("Paris"))
        .count();
    assert_eq!(
        matching, 1,
        "remember must be idempotent on replay — exactly one claim, not two (got {matching})"
    );

    // The persisted claim carries the run-scoped provenance source.
    let remembered = claims.iter().find(|c| c.statement.contains("Paris")).unwrap();
    assert!(
        remembered.source_uri.starts_with("rootfn://acme/"),
        "remembered claim must carry rootfn provenance, got '{}'",
        remembered.source_uri
    );
}

/// A function running in workspace A cannot write into workspace B — the
/// per-user isolation boundary is the workspace itself.
#[tokio::test]
async fn remember_is_isolated_per_workspace() {
    let (mut engine, _root_a, _dir_a) = engine_with_ws("u_alice").await;

    // Mount a second, independent workspace (u_bob) on the same engine.
    let dir_b = tempdir().unwrap();
    let root_b: PathBuf = dir_b.path().to_path_buf();
    std::fs::create_dir_all(root_b.join(".thinkingroot").join("graph")).unwrap();
    GraphStore::init(&root_b.join(".thinkingroot").join("graph")).unwrap();
    engine.mount("u_bob".to_string(), root_b).await.unwrap();

    const SAVE: &str = r#"async (i, ctx) => ({ id: await ctx.memory.remember(i.fact) })"#;
    engine.put_function("u_alice", "save", SAVE, "js").await.unwrap();

    engine
        .invoke_function(
            "u_alice",
            "save",
            &serde_json::json!({ "fact": "Alice's secret lead is Acme Corp" }),
        )
        .await
        .unwrap();

    // Alice's workspace has the claim; Bob's does not.
    let alice = engine.list_claims("u_alice", all_claims_filter()).await.unwrap();
    let bob = engine.list_claims("u_bob", all_claims_filter()).await.unwrap();
    assert!(
        alice.iter().any(|c| c.statement.contains("Acme Corp")),
        "Alice's own workspace must contain her remembered claim"
    );
    assert!(
        !bob.iter().any(|c| c.statement.contains("Acme Corp")),
        "cross-user leak: Bob's workspace must NOT contain Alice's claim"
    );
}

/// ctx.memory is wired into the isolate (the op is reached and bound to the
/// workspace) regardless of whether the ONNX embedder is staged. Without the
/// model, semantic recall's vector phase errors — but the failure must be the
/// engine recall path ("recall failed"), NOT "ctx.memory is unavailable",
/// which would mean caps were never threaded in.
#[tokio::test]
async fn ctx_memory_is_bound_to_the_workspace() {
    let (engine, _root, _dir) = engine_with_ws("acme").await;
    const RECALL: &str = r#"async (i, ctx) => ({ hits: await ctx.memory.recall(i.q, 5) })"#;
    engine.put_function("acme", "find", RECALL, "js").await.unwrap();

    let res = engine
        .invoke_function("acme", "find", &serde_json::json!({ "q": "anything" }))
        .await;

    match res {
        // Model staged (e.g. cloud/Azure): recall returns an array of hits.
        Ok(v) => assert!(v["hits"].is_array(), "recall must return an array of hits: {v}"),
        // Model absent (local CI): the op WAS reached and bound — it failed in
        // the engine recall path, not because caps were missing.
        Err(e) => {
            let msg = e.to_string();
            assert!(
                !msg.contains("ctx.memory is unavailable"),
                "caps were not threaded into the isolate: {msg}"
            );
            assert!(
                msg.contains("recall"),
                "expected an engine recall-path error, got: {msg}"
            );
        }
    }
}
