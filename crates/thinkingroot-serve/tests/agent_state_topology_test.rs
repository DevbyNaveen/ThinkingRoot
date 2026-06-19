//! Integration tests for `engine.agent_topology(ws, name)` and
//! `engine.fork_run_branch` / `engine.settle_run_branch`.
//!
//! Proves:
//!   - an agent with `config_json = {"write_target":"per_run","merge_policy":"verified"}`
//!     resolves to `WriteTarget::PerRun` via the inheritance-chain fallback;
//!   - an unknown agent falls back to `AgentTopology::default()`;
//!   - `settle_run_branch` with Auto policy and ok=true merges the run branch;
//!   - `settle_run_branch` with ok=false rolls back (abandons) the branch;
//!   - `settle_run_branch` with Verified policy runs the health_score check.
//!   - (Task 8) a claim written on a run branch reaches main after a successful
//!     auto-merge, and does NOT reach main after a failed (rolled-back) run.

use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_core::{AgentMergePolicy, AgentTopology, WriteTarget};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::{AgentClaim, ClaimFilter, Principal, QueryEngine};
use thinkingroot_serve::intelligence::session::SessionStore;

async fn setup() -> (tempfile::TempDir, PathBuf, QueryEngine) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    GraphStore::init(&graph_dir).unwrap();

    let mut engine = QueryEngine::new();
    engine.mount("brain".to_string(), root.clone()).await.unwrap();
    (dir, root, engine)
}

fn mem(stmt: &str) -> AgentClaim {
    AgentClaim {
        statement: stmt.to_string(),
        claim_type: "fact".into(),
        confidence: Some(0.9),
        entities: vec![],
    }
}

#[tokio::test]
async fn agent_topology_resolves_write_target_from_config_json() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";

    // Persist an agent whose config_json declares per_run isolation + verified merge.
    engine
        .put_agent(
            ws,
            "researcher",
            "You are a careful researcher.",
            "",
            r#"{"write_target":"per_run","merge_policy":"verified"}"#,
        )
        .await
        .unwrap();

    let topo = engine.agent_topology(ws, "researcher").await;
    assert_eq!(
        topo.write_target,
        WriteTarget::PerRun,
        "researcher topology must resolve PerRun from config_json"
    );
    assert_eq!(topo.merge_policy, thinkingroot_core::AgentMergePolicy::Verified);
}

#[tokio::test]
async fn agent_topology_defaults_for_unknown_agent() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";

    // No agent persisted — must return the default topology (legacy behavior).
    let topo = engine.agent_topology(ws, "ghost").await;
    assert_eq!(
        topo,
        AgentTopology::default(),
        "unknown agent must resolve to default topology"
    );
}

// ── fork_run_branch / settle_run_branch tests ──────────────────────────────

#[tokio::test]
async fn settle_auto_merges_on_success() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";
    let sessions = SessionStore::default();

    // fork an isolated run branch
    let branch = engine.fork_run_branch(ws, "run-1", None).await.unwrap();
    assert_eq!(branch, "run/run-1");

    // contribute one claim to that branch so the merge has real work
    engine
        .contribute_claims_as(
            ws,
            "sess-run-1",
            Some("run/run-1"),
            vec![mem("run-1 discovered an important fact")],
            &sessions,
            Principal::Agent("run-1".into()),
        )
        .await
        .unwrap();

    // settle with Auto policy and ok=true → must merge
    let report = engine
        .settle_run_branch(ws, &branch, AgentMergePolicy::Auto, true)
        .await
        .unwrap();
    assert!(
        report.merged,
        "Auto+ok=true must merge the run branch into main: {:?}",
        report
    );
    assert!(!report.rolled_back, "must not be rolled back on success");
}

#[tokio::test]
async fn settle_rolls_back_on_failure() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";

    // fork but do NOT contribute — the run failed
    let branch = engine.fork_run_branch(ws, "run-fail", None).await.unwrap();

    // settle with ok=false → must abandon the branch, not merge
    let report = engine
        .settle_run_branch(ws, &branch, AgentMergePolicy::Auto, false)
        .await
        .unwrap();
    assert!(
        report.rolled_back,
        "ok=false must roll back (abandon) the branch: {:?}",
        report
    );
    assert!(!report.merged, "must not be merged on failure");
}

#[tokio::test]
async fn settle_verified_runs_health_gate() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";
    let sessions = SessionStore::default();

    // fork + contribute so the health check has content to evaluate
    let branch = engine.fork_run_branch(ws, "run-v", None).await.unwrap();
    engine
        .contribute_claims_as(
            ws,
            "sess-run-v",
            Some("run/run-v"),
            vec![mem("run-v verified a critical insight")],
            &sessions,
            Principal::Agent("run-v".into()),
        )
        .await
        .unwrap();

    // settle with Verified policy — checks must run regardless of pass/fail
    let report = engine
        .settle_run_branch(ws, &branch, AgentMergePolicy::Verified, true)
        .await
        .unwrap();
    assert!(
        report.checks.iter().any(|(name, _passed, _detail)| name == "health_score"),
        "Verified policy must run the health_score check: {:?}",
        report.checks
    );
    assert!(
        report.merged || !report.note.is_empty(),
        "health gate must either merge or explain why not: {:?}",
        report
    );
}

// ── gc_run_branches tests ──────────────────────────────────────────────────

#[tokio::test]
async fn orphan_run_branches_are_gc_after_ttl() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";

    // Fork an orphaned run branch (simulate crash before settle_run_branch).
    let _b = engine.fork_run_branch(ws, "orphan-1", None).await.unwrap();

    // idle_secs = 0 → every run/* branch is immediately eligible.
    let purged = engine.gc_run_branches(ws, 0).await.unwrap();
    assert!(purged >= 1, "expected at least one orphan purged, got {purged}");

    // Non-run/ branches must be untouched: fork a normal feature branch and
    // confirm it still exists as Active after GC.
    engine.fork_run_branch(ws, "run-safe", None).await.unwrap();

    // Use the branch module directly to create a non-run branch.
    let root = _root.clone();
    thinkingroot_branch::create_branch_full(
        &root,
        "feature/keep-me",
        "main",
        None,
        None,
        thinkingroot_core::BranchPermissions::default(),
        thinkingroot_core::BranchKind::Feature,
        thinkingroot_core::MergePolicy::Manual,
        None,
    )
    .await
    .unwrap();

    // GC with idle_secs = 0 again — should only purge run/ branches.
    let purged2 = engine.gc_run_branches(ws, 0).await.unwrap();
    assert!(purged2 >= 1, "run/run-safe must also be purged");

    // feature/keep-me must still be Active.
    let branches = thinkingroot_branch::list_branches(&root).unwrap();
    let keep_me = branches.iter().find(|b| b.name == "feature/keep-me");
    assert!(
        keep_me.is_some(),
        "feature/keep-me must survive gc_run_branches"
    );
    assert!(
        matches!(
            keep_me.unwrap().status,
            thinkingroot_core::BranchStatus::Active
        ),
        "feature/keep-me must still be Active after GC"
    );
}

// ── Task 8: end-to-end isolation proof ────────────────────────────────────────
//
// Verification method: `engine.list_claims_branched(ws, filter, None)` reads
// claims from main via the in-memory cache, which is atomically reloaded on a
// successful merge into main (engine.rs:9541 `*handle.cache.write().await = new_cache`).
// After a rollback (delete_branch) the cache is never touched, so main stays clean.
// We use `list_claims_branched` with an empty filter and look for the distinctive
// statement substring — no ONNX embedder / semantic search required; it is a pure
// graph/cache read.

/// A successful per_run agent run: the claim it wrote on its isolated run branch
/// must become visible on main after the auto-merge.
#[tokio::test]
async fn per_run_success_merges_claim_to_main() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";
    let sessions = SessionStore::default();

    // Distinctive marker — unique enough to rule out any false-positive from
    // other tests sharing the same process (each test uses its own tempdir+engine).
    let marker = "Zephyr protocol uses port 7777";

    // Fork an isolated run branch and write the marker claim onto it.
    let branch = engine.fork_run_branch(ws, "run-e2e-ok", None).await.unwrap();
    engine
        .contribute_claims_as(
            ws,
            "sess-e2e-ok",
            Some(&branch),
            vec![mem(marker)],
            &sessions,
            Principal::Agent("run-e2e-ok".into()),
        )
        .await
        .unwrap();

    // Settle with Auto+ok=true — must merge into main.
    let report = engine
        .settle_run_branch(ws, &branch, AgentMergePolicy::Auto, true)
        .await
        .unwrap();
    assert!(
        report.merged,
        "Auto+ok=true must have merged the run branch: {:?}",
        report
    );

    // The cache is reloaded by merge_into_branch_cancellable (engine.rs:9541).
    // list_claims_branched(…, None) → delegates to list_claims → in-memory cache.
    let claims = engine
        .list_claims_branched(ws, ClaimFilter::default(), None)
        .await
        .unwrap();

    let found = claims.iter().any(|c| c.statement.contains(marker));
    assert!(
        found,
        "marker claim '{marker}' must be recallable on main after a successful auto-merge; \
         main had {} claims: {:#?}",
        claims.len(),
        claims.iter().map(|c| &c.statement).collect::<Vec<_>>()
    );
}

/// A failed per_run agent run: the claim it wrote is discarded; main stays clean.
#[tokio::test]
async fn failed_per_run_leaves_main_clean() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";
    let sessions = SessionStore::default();

    // Distinctive marker for the "bad" claim.
    let bad_marker = "Omega backdoor listens on port 1337 — INVALID claim";

    // Fork and write a "bad" claim to the run branch.
    let branch = engine
        .fork_run_branch(ws, "run-e2e-fail", None)
        .await
        .unwrap();
    engine
        .contribute_claims_as(
            ws,
            "sess-e2e-fail",
            Some(&branch),
            vec![mem(bad_marker)],
            &sessions,
            Principal::Agent("run-e2e-fail".into()),
        )
        .await
        .unwrap();

    // Settle with ok=false — branch is abandoned; nothing reaches main.
    let report = engine
        .settle_run_branch(ws, &branch, AgentMergePolicy::Auto, false)
        .await
        .unwrap();
    assert!(
        report.rolled_back,
        "ok=false must have rolled back (abandoned) the branch: {:?}",
        report
    );
    assert!(!report.merged, "must not be merged on failure");

    // The cache is NOT updated on a rollback — main stays unchanged.
    let claims = engine
        .list_claims_branched(ws, ClaimFilter::default(), None)
        .await
        .unwrap();

    let found = claims.iter().any(|c| c.statement.contains(bad_marker));
    assert!(
        !found,
        "bad marker claim must NOT appear on main after a rolled-back run; \
         main had {} claims: {:#?}",
        claims.len(),
        claims.iter().map(|c| &c.statement).collect::<Vec<_>>()
    );
}
