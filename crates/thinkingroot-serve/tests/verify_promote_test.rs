//! #12 — verify-before-merge for self-authored functions.
//!
//! Proves the explicit gate: a function authored on a quarantine branch is
//! promoted to trunk ONLY when its trunk-owned fixtures all pass; a failing
//! one stays quarantined. Requires `--features root-functions` (the isolate
//! must actually run the fixtures).

use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::QueryEngine;

#[tokio::test]
async fn verify_and_promote_gates_on_fixtures() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    GraphStore::init(&graph_dir).unwrap(); // trunk graph

    // A session quarantine branch.
    thinkingroot_branch::create_branch(&root, "stream/s1", "main", None)
        .await
        .unwrap();

    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root.clone()).await.unwrap();

    // ── A correct function authored on the branch + passing fixtures (trunk).
    engine
        .put_function_on_branch("demo", "stream/s1", "double", "(i, ctx) => i.n * 2", "js")
        .await
        .unwrap();
    engine
        .put_function_test("demo", "double", &serde_json::json!({ "n": 2 }), &serde_json::json!(4))
        .await
        .unwrap();
    engine
        .put_function_test("demo", "double", &serde_json::json!({ "n": 5 }), &serde_json::json!(10))
        .await
        .unwrap();

    let res = engine
        .verify_and_promote_function("demo", "double", "stream/s1")
        .await
        .unwrap();
    assert_eq!(res["promoted"], serde_json::json!(true), "passing fn must promote: {res}");
    assert!(
        engine.get_function("demo", "double").await.unwrap().is_some(),
        "promoted function must now exist on trunk"
    );

    // ── A wrong function authored on the branch fails its fixture → NOT promoted.
    engine
        .put_function_on_branch("demo", "stream/s1", "bad", "(i, ctx) => 0", "js")
        .await
        .unwrap();
    engine
        .put_function_test("demo", "bad", &serde_json::json!({ "n": 2 }), &serde_json::json!(4))
        .await
        .unwrap();

    let res2 = engine
        .verify_and_promote_function("demo", "bad", "stream/s1")
        .await
        .unwrap();
    assert_eq!(res2["promoted"], serde_json::json!(false), "failing fn must NOT promote: {res2}");
    assert!(
        engine.get_function("demo", "bad").await.unwrap().is_none(),
        "failing function must stay quarantined off trunk"
    );
}

/// Real-data end-to-end exercise of the Self-Compounding Cognitive Backend.
///
/// This is NOT a toy-value unit check — it deploys a realistic churn-risk
/// Root Function, invokes it with real customer records, then walks the four
/// capabilities the whole arc was built for, printing the *actual* data at
/// each step (run with `--nocapture` to watch it):
///
///   1. Live deterministic execution in the real V8 isolate over real CozoDB.
///   2. Durable journaling + replay — same run_id replays the journaled step
///      (identical value), a fresh run_id recomputes.
///   3. The moat learns — repeated real runs rank a reliable function above a
///      buggy competitor by Wilson-scored confident success rate.
///   4. Causal un-learning — superseding a claim a run cited decays exactly
///      the experience grounded on it, through the real supersede cascade.
#[tokio::test]
async fn churn_backend_real_data_end_to_end() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    GraphStore::init(&graph_dir).unwrap();

    // A realistic, graph-grounded churn scorer. It cites the policy claim it
    // depends on, and runs an "expensive" base-risk lookup inside ctx.step so
    // a resume never recomputes it.
    const CHURN: &str = r#"
        async (input, ctx) => {
          ctx.cite("claim:churn-policy-v1");
          const base = await ctx.step("base_risk", async () =>
            input.daysSinceActive > 30 ? 0.6 : 0.15);
          const ticketPenalty = input.openTickets > 2 ? 0.25 : 0.0;
          const risk = Math.min(1, base + ticketPenalty);
          return { customer: input.customer, churnRisk: risk, tier: risk >= 0.6 ? "high" : "low" };
        }
    "#;

    let mut engine = QueryEngine::new();
    engine.mount("acme".to_string(), root.clone()).await.unwrap();
    engine.put_function("acme", "scoreChurn", CHURN, "js").await.unwrap();

    // ── 1. Live execution over real customer records ────────────────────────
    println!("\n=== 1. LIVE EXECUTION (real V8 isolate, real CozoDB) ===");
    let customers = [
        serde_json::json!({ "customer": "acme-corp", "daysSinceActive": 45, "openTickets": 4 }),
        serde_json::json!({ "customer": "globex",    "daysSinceActive": 10, "openTickets": 1 }),
        serde_json::json!({ "customer": "initech",   "daysSinceActive": 60, "openTickets": 0 }),
    ];
    for c in &customers {
        let out = engine.invoke_function("acme", "scoreChurn", c).await.unwrap();
        println!("  input {:<58} -> {}", c.to_string(), out);
        assert!(out["churnRisk"].is_number(), "real run must return a numeric risk");
    }
    // The hand-computable expectations, asserted against the real isolate.
    let acme = engine.invoke_function("acme", "scoreChurn", &customers[0]).await.unwrap();
    assert_eq!(acme["churnRisk"], serde_json::json!(0.85)); // 0.6 + 0.25
    assert_eq!(acme["tier"], serde_json::json!("high"));
    let globex = engine.invoke_function("acme", "scoreChurn", &customers[1]).await.unwrap();
    assert_eq!(globex["churnRisk"], serde_json::json!(0.15));
    assert_eq!(globex["tier"], serde_json::json!("low"));

    // ── 2. Durable journaling + replay ──────────────────────────────────────
    println!("\n=== 2. DURABLE JOURNALING + REPLAY ===");
    const RANDOM_STEP: &str = r#"
        async (input, ctx) => {
          const token = await ctx.step("token", async () => Math.random());
          return { token };
        }
    "#;
    engine.put_function("acme", "mintToken", RANDOM_STEP, "js").await.unwrap();
    let first = engine.run_function_with_id("acme", "mintToken", &serde_json::json!({}), "run-A").await.unwrap();
    let replay = engine.run_function_with_id("acme", "mintToken", &serde_json::json!({}), "run-A").await.unwrap();
    let fresh = engine.run_function_with_id("acme", "mintToken", &serde_json::json!({}), "run-B").await.unwrap();
    println!("  run-A first invoke : {first}");
    println!("  run-A replay       : {replay}   <- identical: journaled step NOT recomputed");
    println!("  run-B fresh run    : {fresh}   <- different: new run, new value");
    assert_eq!(first, replay, "same run_id must replay the journaled step value");
    assert_ne!(first, fresh, "a fresh run_id must recompute, not reuse the journal");

    // ── 3. The moat learns (real runs → Wilson-scored ranking) ──────────────
    println!("\n=== 3. THE MOAT LEARNS (ranks reliable fn over buggy competitor) ===");
    // A competing function for the same input shape that always fails.
    const BUGGY: &str = r#"async (input, ctx) => { throw new Error("scorer crashed"); }"#;
    engine.put_function("acme", "scoreChurnBuggy", BUGGY, "js").await.unwrap();
    // Drive real traffic: scoreChurn succeeds on every customer; the buggy one errors.
    for _ in 0..2 {
        for c in &customers {
            let _ = engine.invoke_function("acme", "scoreChurn", c).await;
            let _ = engine.invoke_function("acme", "scoreChurnBuggy", c).await; // errors → negative evidence
        }
    }
    let probe = serde_json::json!({ "customer": "newco", "daysSinceActive": 33, "openTickets": 3 });
    let ranked = engine.route_functions("acme", &probe).await.unwrap();
    for e in &ranked {
        println!("  {:<18} score={:.4}  (✓{} ✗{})", e.function_name, e.score(), e.n_success, e.n_fail);
    }
    let best = ranked.iter().max_by(|a, b| a.score().total_cmp(&b.score())).unwrap();
    assert_eq!(best.function_name, "scoreChurn", "the reliable function must rank first");
    assert!(best.n_success > 0, "the winner must have real recorded successes");

    // ── 4. Causal un-learning through the real supersede cascade ────────────
    println!("\n=== 4. CAUSAL UN-LEARNING (supersede the cited claim) ===");
    // input_class the engine learned under = "{fn}:{function-independent shape}".
    let mut keys: Vec<&str> = probe.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    let input_class = format!("scoreChurn:obj[{}]", keys.join(","));

    // Release the engine's lock on the CozoDB so we can reopen it and run the
    // real supersede cascade against the very rows the live runs wrote.
    drop(engine);
    let graph = GraphStore::init(&graph_dir).unwrap();
    let before = graph.get_experience(&input_class, "scoreChurn").unwrap().expect("learned experience");
    println!("  before supersede: score={:.4}  (✓{} ✗{})", before.score(), before.n_success, before.n_fail);

    // The policy claim every scoreChurn run cited is superseded → the real
    // cascade decays exactly the experience grounded on it.
    graph.supersede_claim("claim:churn-policy-v1", "claim:churn-policy-v2").unwrap();

    let after = graph.get_experience(&input_class, "scoreChurn").unwrap().expect("experience persists");
    println!("  after  supersede: score={:.4}  (✓{} ✗{})  <- decayed: basis changed", after.score(), after.n_success, after.n_fail);
    assert!(after.score() < before.score(), "score must decay after the cited claim changed");
    assert!(after.n_success < before.n_success, "success evidence must be halved by invalidation");
    println!("\n=== ALL FOUR CAPABILITIES VERIFIED ON REAL DATA ===\n");
}
