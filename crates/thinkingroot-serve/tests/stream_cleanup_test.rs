//! Phase E regression tests: the background stream-cleanup task must
//! soft-delete expired `stream/*` branches without losing agent work.

use std::path::Path;
use tempfile::tempdir;

use thinkingroot_core::{
    Claim, ClaimType, ContentHash, Source, SourceType, TrustLevel, WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::intelligence::session::{SessionContext, new_session_store};
use thinkingroot_serve::maintenance::cleanup_once;

fn seed_main(root: &Path) {
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    let _ = GraphStore::init(&graph_dir).unwrap();
}

async fn make_stream_branch(root: &Path, session_id: &str) {
    let name = format!("stream/{session_id}");
    thinkingroot_branch::create_branch(root, &name, "main", None)
        .await
        .unwrap();
}

fn add_agent_contribute(root: &Path, session_id: &str) {
    let slug = format!("stream-{session_id}");
    let branch_dir = root.join(".thinkingroot").join("branches").join(&slug);
    let graph = GraphStore::init(&branch_dir.join("graph")).unwrap();
    let workspace = WorkspaceId::new();
    let uri = format!("mcp://agent/{session_id}");
    let source = Source::new(uri.clone(), SourceType::ChatMessage)
        .with_trust(TrustLevel::Untrusted)
        .with_hash(ContentHash(format!("h-{session_id}")));
    let src_id = source.id.to_string();
    graph.insert_source(&source).unwrap();
    let claim = Claim::new("agent observation", ClaimType::Fact, source.id, workspace);
    let cid = claim.id.to_string();
    graph.insert_claim(&claim).unwrap();
    graph.link_claim_to_source(&cid, &src_id).unwrap();
}

#[tokio::test]
async fn cleanup_abandons_expired_stream_branch_and_keeps_active_one() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    seed_main(&root);

    make_stream_branch(&root, "expired-sess").await;
    make_stream_branch(&root, "active-sess").await;

    let sessions = new_session_store();
    {
        // Active session — should keep its branch.
        let mut store = sessions.lock().await;
        store.insert(
            "active-sess".to_string(),
            SessionContext::new("active-sess", "demo"),
        );
        // No entry for "expired-sess" — cleanup must abandon that branch.
    }

    let stats = cleanup_once(&sessions, &root, 0, "abandon", None)
        .await
        .unwrap();
    assert_eq!(stats.branches_scanned, 2);
    // The "active-sess" branch is kept because the session is present and not idle.
    // With idle_secs=0 though, even the active session might be considered idle.
    // Re-run with a larger idle window to ensure the kept-path is exercised:

    let dir2 = tempdir().unwrap();
    let root2 = dir2.path().to_path_buf();
    seed_main(&root2);
    make_stream_branch(&root2, "expired-sess").await;
    make_stream_branch(&root2, "active-sess").await;
    let sessions2 = new_session_store();
    sessions2.lock().await.insert(
        "active-sess".to_string(),
        SessionContext::new("active-sess", "demo"),
    );
    let stats = cleanup_once(&sessions2, &root2, 3600, "abandon", None)
        .await
        .unwrap();
    assert_eq!(stats.branches_scanned, 2);
    assert_eq!(
        stats.abandoned, 1,
        "only the expired-sess branch should be abandoned"
    );
    assert_eq!(stats.kept, 1, "active-sess branch should be kept");
    assert_eq!(stats.purged, 0);

    let remaining = thinkingroot_branch::list_branches(&root2).unwrap();
    let active_names: Vec<String> = remaining.iter().map(|b| b.name.clone()).collect();
    assert!(
        active_names.contains(&"stream/active-sess".to_string()),
        "active branch must remain. got: {:?}",
        active_names
    );
    assert!(
        !active_names.contains(&"stream/expired-sess".to_string()),
        "expired branch must be removed from active list. got: {:?}",
        active_names
    );
}

#[tokio::test]
async fn cleanup_never_purges_branches_with_agent_contributes() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    seed_main(&root);

    // Two expired branches — one with agent contributes, one without.
    make_stream_branch(&root, "with-work").await;
    make_stream_branch(&root, "empty").await;
    add_agent_contribute(&root, "with-work");

    let sessions = new_session_store(); // empty — both branches orphaned

    // Ask for purge; the with-work branch must be downgraded to abandon.
    let stats = cleanup_once(&sessions, &root, 3600, "purge", None)
        .await
        .unwrap();
    assert_eq!(stats.branches_scanned, 2);
    assert_eq!(
        stats.purged, 1,
        "only the empty branch should be hard-purged. stats: {:?}",
        stats
    );
    assert_eq!(
        stats.abandoned, 1,
        "with-work branch must be downgraded to abandon. stats: {:?}",
        stats
    );

    // The with-work data dir must still exist on disk (abandon keeps data).
    let with_work_dir = root
        .join(".thinkingroot")
        .join("branches")
        .join("stream-with-work");
    assert!(
        with_work_dir.exists(),
        "abandoned branch data dir must survive cleanup"
    );
}

#[tokio::test]
async fn cleanup_ignores_non_stream_branches() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    seed_main(&root);
    thinkingroot_branch::create_branch(&root, "feature/important", "main", None)
        .await
        .unwrap();

    let sessions = new_session_store();
    let stats = cleanup_once(&sessions, &root, 3600, "abandon", None)
        .await
        .unwrap();
    assert_eq!(
        stats.branches_scanned, 0,
        "feature branches are not stream/* and should not be scanned"
    );
    assert_eq!(stats.abandoned, 0);

    let remaining = thinkingroot_branch::list_branches(&root).unwrap();
    assert_eq!(remaining.len(), 1, "feature branch must remain untouched");
}
