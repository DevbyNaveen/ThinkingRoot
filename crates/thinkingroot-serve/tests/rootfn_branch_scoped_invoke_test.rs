//! A2 — branch-scoped Root Function invoke.
//!
//! A function can be invoked so its `ctx.memory.remember` writes are
//! quarantined to a branch (a CoW clone of main at fork) instead of main:
//!   - `target_branch` → writes land on a named branch (keep-or-abandon later)
//!   - `dry_run`       → writes land on a fresh ephemeral branch that is
//!                       abandoned after the run (a true dry run)
//!
//! This is the substrate for verify-before-keep (forge) and quarantined
//! dreaming. All assertions are model-independent (deterministic claim ids +
//! graph/fs only — no ONNX embedder needed). Requires `--features
//! root-functions`.

use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_branch::snapshot::resolve_data_dir;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::{InvokeBranchOpts, QueryEngine};

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

// A function that remembers `i.fact` and returns the claim id.
const REMEMBER: &str = r#"
    async (i, ctx) => ({
      id: await ctx.memory.remember(i.fact, { type: "fact", confidence: 0.9 })
    })
"#;

fn main_graph(root: &PathBuf) -> GraphStore {
    GraphStore::init(&root.join(".thinkingroot").join("graph")).unwrap()
}

fn branch_graph(root: &PathBuf, branch: &str) -> GraphStore {
    GraphStore::init(&resolve_data_dir(root, Some(branch)).join("graph")).unwrap()
}

/// target_branch: the remembered claim lands on the branch, NOT on main.
#[tokio::test]
async fn target_branch_routes_writes_off_main() {
    let (engine, root, _dir) = engine_with_ws("acme").await;
    engine.put_function("acme", "saver", REMEMBER, "js").await.unwrap();

    let out = engine
        .invoke_function_with_opts(
            "acme",
            "saver",
            &serde_json::json!({ "fact": "the sky is teal on this branch" }),
            InvokeBranchOpts {
                target_branch: Some("exp/x".to_string()),
                dry_run: false,
            },
        )
        .await
        .unwrap();

    let claim_id = out["id"].as_str().expect("function returns the claim id").to_string();

    // Result markers describe where the write went.
    assert_eq!(out["_branch"], serde_json::json!("exp/x"));
    assert_eq!(out["_dry_run"], serde_json::json!(false));
    assert!(out["_claims_written"].as_u64().unwrap() >= 1, "branch carries the claim");

    // The claim is on the branch graph …
    assert!(
        branch_graph(&root, "exp/x").get_claim_by_id(&claim_id).unwrap().is_some(),
        "claim must exist on the target branch"
    );
    // … and ABSENT from main (quarantine proven).
    assert!(
        main_graph(&root).get_claim_by_id(&claim_id).unwrap().is_none(),
        "branch-scoped write must NOT touch main"
    );

    // The branch persists (caller decides merge-or-abandon).
    let branches = thinkingroot_branch::list_branches(&root).unwrap();
    assert!(branches.iter().any(|b| b.name == "exp/x"), "named branch persists");
}

/// dry_run: the write happens in isolation, then the ephemeral branch — and
/// thus the write — vanishes; main is never touched.
#[tokio::test]
async fn dry_run_discards_branch_and_leaves_main_clean() {
    let (engine, root, _dir) = engine_with_ws("acme").await;
    engine.put_function("acme", "saver", REMEMBER, "js").await.unwrap();

    let out = engine
        .invoke_function_with_opts(
            "acme",
            "saver",
            &serde_json::json!({ "fact": "a thought I am only trying out" }),
            InvokeBranchOpts { target_branch: None, dry_run: true },
        )
        .await
        .unwrap();

    let claim_id = out["id"].as_str().unwrap().to_string();
    assert_eq!(out["_dry_run"], serde_json::json!(true));
    assert!(
        out["_claims_written"].as_u64().unwrap() >= 1,
        "dry-run still reports what WOULD have been written"
    );

    // Main never saw the claim.
    assert!(
        main_graph(&root).get_claim_by_id(&claim_id).unwrap().is_none(),
        "dry-run must not touch main"
    );

    // The ephemeral branch was abandoned — no dryrun/* branch remains.
    let branches = thinkingroot_branch::list_branches(&root).unwrap();
    assert!(
        !branches.iter().any(|b| b.name.starts_with("dryrun/")),
        "ephemeral dry-run branch must be cleaned up"
    );
}

/// Backward compatibility: a plain invoke (no opts) still writes to main.
#[tokio::test]
async fn default_invoke_writes_to_main() {
    let (engine, root, _dir) = engine_with_ws("acme").await;
    engine.put_function("acme", "saver", REMEMBER, "js").await.unwrap();

    let out = engine
        .invoke_function("acme", "saver", &serde_json::json!({ "fact": "this is real" }))
        .await
        .unwrap();

    let claim_id = out["id"].as_str().unwrap().to_string();
    // No branch markers on the default path.
    assert!(out.get("_branch").is_none(), "plain invoke adds no branch marker");
    assert!(
        main_graph(&root).get_claim_by_id(&claim_id).unwrap().is_some(),
        "default invoke writes to main as before"
    );
}
