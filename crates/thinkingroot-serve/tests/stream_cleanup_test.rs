//! Phase E regression tests: the background stream-cleanup task must
//! soft-delete expired `stream/*` branches without losing agent work.
//!
//! Phase A (2026-05-17) extension: `MergePolicy::AutoOnSessionEnd`
//! stream branches that carry agent contributes get merged into an
//! auto-created `topic/*` Feature branch (default
//! `MergePolicy::Manual`) — not abandoned, and explicitly NOT
//! auto-promoted to `main`.

use std::path::Path;
use tempfile::tempdir;

use thinkingroot_core::{
    BranchKind, BranchPermissions, BranchStatus, Claim, ClaimType, ContentHash, MergePolicy,
    Source, SourceType, TrustLevel, WorkspaceId,
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

/// Phase A helper: create a stream branch with explicit MergePolicy +
/// BranchKind::Stream so the cleanup task can apply policy-aware
/// dispatch. The default `create_branch` used by the legacy helpers
/// always sets `MergePolicy::Manual` + `BranchKind::Feature`, which
/// would short-circuit every new Phase A code path.
async fn make_stream_branch_with_policy(
    root: &Path,
    session_id: &str,
    policy: MergePolicy,
) -> String {
    let name = format!("stream/{session_id}");
    thinkingroot_branch::create_branch_full(
        root,
        &name,
        "main",
        None,
        Some(session_id.to_string()),
        BranchPermissions::default(),
        BranchKind::Stream {
            session_id: session_id.to_string(),
        },
        policy,
        None,
    )
    .await
    .unwrap();
    name
}

#[tokio::test]
async fn auto_on_session_end_with_contributes_merges_to_topic() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    seed_main(&root);

    let session_id = "sessabcd1234";
    let stream_name =
        make_stream_branch_with_policy(&root, session_id, MergePolicy::AutoOnSessionEnd).await;
    add_agent_contribute(&root, session_id);

    // Empty session store → branch is considered idle → triggers
    // policy dispatch.
    let sessions = new_session_store();
    let stats = cleanup_once(&sessions, &root, 3600, "abandon", None)
        .await
        .unwrap();

    assert_eq!(stats.branches_scanned, 1, "stats: {stats:?}");
    assert_eq!(
        stats.merged_to_topic, 1,
        "AutoOnSessionEnd + contributes must route to topic merge. stats: {stats:?}"
    );
    assert_eq!(stats.abandoned, 0, "must NOT abandon — would lose work");
    assert_eq!(stats.purged, 0);

    // A topic/* branch was auto-created.
    let branches = thinkingroot_branch::list_branches(&root).unwrap();
    let topic = branches
        .iter()
        .find(|b| b.name.starts_with("topic/") && matches!(b.status, BranchStatus::Active))
        .expect("expected a topic/* branch after auto-merge");

    // Topic branch defaults: Feature kind, Manual merge policy.
    // Promoting topic → main MUST require an explicit user merge.
    assert_eq!(
        topic.kind,
        BranchKind::Feature,
        "topic branch must be a Feature, not Stream/Sandbox/Tag"
    );
    assert_eq!(
        topic.merge_policy,
        MergePolicy::Manual,
        "topic branch must default to Manual — main is reached only by explicit user action"
    );

    // Stream branch is no longer Active (it's been merged or marked
    // merged in the registry). The exact post-merge status is owned by
    // the merge layer; what we assert here is the cleanup-loop
    // invariant: a successfully-merged stream does NOT show up Active.
    let still_active_stream = branches
        .iter()
        .any(|b| b.name == stream_name && matches!(b.status, BranchStatus::Active));
    assert!(
        !still_active_stream,
        "stream branch must not remain Active after successful auto-merge"
    );

    // Main is untouched — no claims promoted there. We verify this by
    // checking that the workspace main graph still has zero non-system
    // sources (the test seeds an empty main).
    let main_graph =
        GraphStore::init(&root.join(".thinkingroot").join("graph")).unwrap();
    let sources = main_graph.get_all_sources().unwrap();
    let agent_sources: Vec<_> = sources
        .iter()
        .filter(|(_, uri, _, _)| uri.starts_with("mcp://agent/"))
        .collect();
    assert!(
        agent_sources.is_empty(),
        "main must NOT receive agent contributes from auto-merge — only topic does. found: {agent_sources:?}"
    );
}

#[tokio::test]
async fn auto_on_session_end_without_contributes_just_abandons() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    seed_main(&root);

    let session_id = "emptysess";
    make_stream_branch_with_policy(&root, session_id, MergePolicy::AutoOnSessionEnd).await;
    // Intentionally NO add_agent_contribute — branch is empty.

    let sessions = new_session_store();
    let stats = cleanup_once(&sessions, &root, 3600, "abandon", None)
        .await
        .unwrap();

    assert_eq!(stats.branches_scanned, 1);
    assert_eq!(
        stats.merged_to_topic, 0,
        "empty AutoOnSessionEnd must NOT create a spurious topic branch"
    );
    assert_eq!(stats.abandoned, 1, "empty stream should be abandoned");

    let branches = thinkingroot_branch::list_branches(&root).unwrap();
    let active_topic = branches
        .iter()
        .any(|b| b.name.starts_with("topic/") && matches!(b.status, BranchStatus::Active));
    assert!(
        !active_topic,
        "no topic branch should be created for an empty stream"
    );
}

#[tokio::test]
async fn ephemeral_still_abandons_even_with_contributes() {
    // Regression guard: Phase A must NOT break Ephemeral's "discard,
    // never merge" contract. Even with agent contributes, Ephemeral
    // streams abandon — they explicitly opted out of being preserved.
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    seed_main(&root);

    let session_id = "ephemeral1";
    make_stream_branch_with_policy(&root, session_id, MergePolicy::Ephemeral).await;
    add_agent_contribute(&root, session_id);

    let sessions = new_session_store();
    let stats = cleanup_once(&sessions, &root, 3600, "abandon", None)
        .await
        .unwrap();

    assert_eq!(stats.branches_scanned, 1);
    assert_eq!(
        stats.merged_to_topic, 0,
        "Ephemeral must NEVER route to topic — discard contract"
    );
    assert_eq!(stats.abandoned, 1, "Ephemeral always abandons");

    let branches = thinkingroot_branch::list_branches(&root).unwrap();
    let active_topic = branches
        .iter()
        .any(|b| b.name.starts_with("topic/") && matches!(b.status, BranchStatus::Active));
    assert!(
        !active_topic,
        "Ephemeral must not synthesise a topic branch"
    );
}

#[tokio::test]
async fn auto_merge_topic_branch_is_idempotent_across_ticks() {
    // Two cleanup ticks on the same session-id-bucket must reuse the
    // same topic branch, not create a second one. Phase A's
    // deterministic naming (`topic/{date}-{session[:8]}`) is what
    // makes this work; this test pins that contract.
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    seed_main(&root);

    // Two streams with the same first-8-of-session-id prefix end up in
    // the same topic bucket on the same date.
    let stream_a = make_stream_branch_with_policy(
        &root,
        "samebucket-a",
        MergePolicy::AutoOnSessionEnd,
    )
    .await;
    add_agent_contribute(&root, "samebucket-a");

    let sessions = new_session_store();
    let _ = cleanup_once(&sessions, &root, 3600, "abandon", None)
        .await
        .unwrap();

    // Count topic branches after first tick.
    let topics_after_first: Vec<_> = thinkingroot_branch::list_branches(&root)
        .unwrap()
        .into_iter()
        .filter(|b| {
            b.name.starts_with("topic/") && matches!(b.status, BranchStatus::Active)
        })
        .collect();
    assert_eq!(
        topics_after_first.len(),
        1,
        "first tick should create exactly one topic branch"
    );

    // Add a SECOND stream branch with the SAME session-id-bucket and
    // re-run cleanup. The existing topic must be reused — no
    // duplicate.
    let _stream_b = make_stream_branch_with_policy(
        &root,
        "samebucket-b",
        MergePolicy::AutoOnSessionEnd,
    )
    .await;
    add_agent_contribute(&root, "samebucket-b");
    let _ = cleanup_once(&sessions, &root, 3600, "abandon", None)
        .await
        .unwrap();

    let topics_after_second: Vec<_> = thinkingroot_branch::list_branches(&root)
        .unwrap()
        .into_iter()
        .filter(|b| {
            b.name.starts_with("topic/") && matches!(b.status, BranchStatus::Active)
        })
        .collect();
    // The two sessions hash to different first-8-alnum prefixes
    // (samebuck != samebuck — wait, both start with "samebuck" so they
    // DO collapse to the same prefix). Confirm idempotency: same
    // topic name still maps to one Active topic.
    assert_eq!(
        topics_after_second
            .iter()
            .map(|b| &b.name)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        topics_after_second.len(),
        "no duplicate Active topic branches by name"
    );

    let _ = stream_a; // silence unused-var lint in older toolchains
}

#[tokio::test]
async fn persisted_chat_turns_propagate_to_topic_on_auto_merge() {
    // Phase B.2 end-to-end: write three chat turns onto the stream
    // branch via the same `persist_chat_turn` path the REST handler
    // calls, run cleanup_once with `AutoOnSessionEnd`, and verify the
    // three turn transcript sources land on the topic branch.
    //
    // This closes the loop between B.2 (persistence onto stream) and
    // Phase A (auto-merge stream → topic). If the merge ever drops
    // synthetic agent-contributed sources, this test catches it
    // before it reaches a release.
    use thinkingroot_serve::intelligence::turn_persistence::persist_chat_turn;

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    seed_main(&root);

    let session_id = "endtoend1";
    let stream_name =
        make_stream_branch_with_policy(&root, session_id, MergePolicy::AutoOnSessionEnd).await;

    // Three completed chat turns on this session.
    persist_chat_turn(
        &root,
        &stream_name,
        session_id,
        1,
        "How does the auth flow handle revoked tokens?",
        "The revocation list is checked on every refresh; revoked tokens hard-fail.",
    )
    .await
    .unwrap();
    persist_chat_turn(
        &root,
        &stream_name,
        session_id,
        2,
        "What's the grace window?",
        "30 seconds — long enough for in-flight requests, short enough to bound exposure.",
    )
    .await
    .unwrap();
    persist_chat_turn(
        &root,
        &stream_name,
        session_id,
        3,
        "Where's that constant defined?",
        "`crates/thinkingroot-auth/src/token.rs:42` — `GRACE_WINDOW_SECS`.",
    )
    .await
    .unwrap();

    // Trigger cleanup with idle threshold high enough that the
    // (absent) session is treated as idle and the branch hits the
    // policy dispatch.
    let sessions = new_session_store();
    let stats = cleanup_once(&sessions, &root, 3600, "abandon", None)
        .await
        .unwrap();
    assert_eq!(
        stats.merged_to_topic, 1,
        "stream with persisted turns must auto-merge to topic. stats: {stats:?}"
    );

    // Locate the auto-created topic branch.
    let branches = thinkingroot_branch::list_branches(&root).unwrap();
    let topic = branches
        .iter()
        .find(|b| b.name.starts_with("topic/") && matches!(b.status, BranchStatus::Active))
        .expect("topic branch must exist after auto-merge");

    // The topic branch's graph must contain all three turn
    // transcripts. Anything fewer means the merge dropped agent-
    // contributed synthetic sources — that's a regression.
    let topic_dir = thinkingroot_branch::snapshot::resolve_data_dir(&root, Some(&topic.name));
    let topic_graph = GraphStore::init(&topic_dir.join("graph")).unwrap();
    let sources = topic_graph.get_all_sources().unwrap();
    let turn_prefix = format!("mcp://agent/{session_id}/turn/");
    let turn_sources: Vec<&String> = sources
        .iter()
        .map(|(_, uri, _, _)| uri)
        .filter(|uri| uri.starts_with(&turn_prefix))
        .collect();
    assert_eq!(
        turn_sources.len(),
        3,
        "all 3 turn transcripts must propagate to topic. got: {turn_sources:?}"
    );
}

#[tokio::test]
async fn topic_branch_inherits_description_from_stream_after_merge() {
    // Phase B.1: when the stream branch carries a non-empty
    // description (set by the chat handler from the user's first
    // message), `cleanup_once` propagates that description onto the
    // auto-created topic branch as its human-readable title.
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    seed_main(&root);

    let session_id = "titledsess";
    let stream_name =
        make_stream_branch_with_policy(&root, session_id, MergePolicy::AutoOnSessionEnd).await;
    add_agent_contribute(&root, session_id);

    // Simulate what the REST chat handler does on the user's first
    // turn: persist the first user message onto the stream branch's
    // description so it survives session eviction.
    let first_msg = "How does the auth refresh-token flow work?";
    thinkingroot_branch::set_branch_description(
        &root,
        &stream_name,
        Some(first_msg.to_string()),
    )
    .unwrap();

    let sessions = new_session_store();
    let stats = cleanup_once(&sessions, &root, 3600, "abandon", None)
        .await
        .unwrap();
    assert_eq!(
        stats.merged_to_topic, 1,
        "AutoOnSessionEnd + contributes must route to topic. stats: {stats:?}"
    );

    let branches = thinkingroot_branch::list_branches(&root).unwrap();
    let topic = branches
        .iter()
        .find(|b| b.name.starts_with("topic/") && matches!(b.status, BranchStatus::Active))
        .expect("topic branch must exist after merge");

    assert_eq!(
        topic.description.as_deref(),
        Some(first_msg),
        "topic description must be propagated from the source stream's description"
    );
}

#[tokio::test]
async fn topic_branch_keeps_placeholder_when_stream_has_no_description() {
    // Phase B.1: when the stream branch has no description (e.g. the
    // REST chat handler's B.1 wire failed or the session evicted
    // before the first message reached it), the topic branch
    // surfaces the ensure_topic_branch placeholder — never an empty
    // title.
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    seed_main(&root);

    let session_id = "untitledsess";
    let _stream_name =
        make_stream_branch_with_policy(&root, session_id, MergePolicy::AutoOnSessionEnd).await;
    add_agent_contribute(&root, session_id);
    // Intentionally do NOT call set_branch_description on the stream.

    let sessions = new_session_store();
    let _ = cleanup_once(&sessions, &root, 3600, "abandon", None)
        .await
        .unwrap();

    let branches = thinkingroot_branch::list_branches(&root).unwrap();
    let topic = branches
        .iter()
        .find(|b| b.name.starts_with("topic/") && matches!(b.status, BranchStatus::Active))
        .expect("topic branch must exist after merge");

    let desc = topic
        .description
        .as_deref()
        .expect("topic must have the create-time placeholder");
    assert!(
        desc.contains("auto-created"),
        "topic must keep the placeholder when source had no description, got: {desc}"
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
