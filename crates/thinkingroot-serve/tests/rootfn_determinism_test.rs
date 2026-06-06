//! M4 — durable-execution determinism across the FULL co-located surface.
//!
//! A realistic multi-effect function (journaled nondeterminism + remember +
//! branch.fork in one body) must be exactly-once on a replay/resume of the
//! same run: identical output, no duplicate claim, no duplicate branch. This
//! is the crash-resume guarantee for the whole `ctx.*` surface combined —
//! the property that separates a SOTA durable runtime from a flashy demo.
//!
//! Model-independent (no semantic recall); requires `--features root-functions`.

use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::{ClaimFilter, QueryEngine};

#[tokio::test]
async fn full_surface_is_exactly_once_on_replay() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join(".thinkingroot").join("graph")).unwrap();
    GraphStore::init(&root.join(".thinkingroot").join("graph")).unwrap();
    let mut engine = QueryEngine::new();
    engine.mount("acme".to_string(), root.clone()).await.unwrap();

    // One body, several durable effects + a journaled nondeterministic draw:
    //   - Math.random() (journaled global)
    //   - ctx.memory.remember (idempotent write)
    //   - ctx.branch.fork (idempotent effect)
    const CAMPAIGN: &str = r#"
        async (i, ctx) => {
          const roll = Math.random();
          const claimId = await ctx.memory.remember("lead " + i.lead + " is qualified");
          const b = await ctx.branch.fork("exp/" + i.lead);
          return { roll, claimId, branch: b.name };
        }
    "#;
    engine.put_function("acme", "campaign", CAMPAIGN, "js").await.unwrap();

    let input = serde_json::json!({ "lead": "acme-corp" });

    // First execution: performs all effects + journals every step.
    let out1 = engine
        .run_function_with_id("acme", "campaign", &input, "run_X")
        .await
        .unwrap();
    // Replay of the SAME run: every step is served from the journal; no effect
    // re-executes; the nondeterministic draw reproduces from the journal.
    let out2 = engine
        .run_function_with_id("acme", "campaign", &input, "run_X")
        .await
        .unwrap();

    assert_eq!(out1, out2, "replay must reproduce the EXACT output (incl. Math.random)");

    // Exactly one claim, exactly one branch — no double effects.
    let claims = engine.list_claims("acme", ClaimFilter {
        claim_type: None,
        entity_name: None,
        min_confidence: None,
        limit: None,
        offset: None,
    }).await.unwrap();
    let claim_matches = claims.iter().filter(|c| c.statement.contains("acme-corp")).count();
    assert_eq!(claim_matches, 1, "remember must not double-write on replay (got {claim_matches})");

    let branches = thinkingroot_branch::list_branches(&root).unwrap();
    let branch_matches = branches.iter().filter(|b| b.name == "exp/acme-corp").count();
    assert_eq!(branch_matches, 1, "branch.fork must not duplicate on replay (got {branch_matches})");

    // A DIFFERENT run computes independently (the journal is per-run): a fresh
    // run_id re-draws Math.random and writes its own run-scoped claim id.
    let out3 = engine
        .run_function_with_id("acme", "campaign", &input, "run_Y")
        .await
        .unwrap();
    assert_ne!(
        out1["claimId"], out3["claimId"],
        "a distinct run must get its own run-scoped claim id"
    );
}
