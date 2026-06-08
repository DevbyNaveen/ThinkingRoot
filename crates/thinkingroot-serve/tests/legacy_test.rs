//! P4 — death → verified legacy: bequeath a genome, inherit it into a FRESH
//! successor. Only verified capabilities + high-confidence knowledge transfer —
//! never the raw, error-carrying memory stream (the world-first guarantee).

use std::path::PathBuf;
use tempfile::tempdir;

use thinkingroot_core::{Claim, ClaimType, ContentHash, Source, SourceType, TrustLevel, WorkspaceId};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::QueryEngine;

#[tokio::test]
async fn bequeath_then_inherit_transfers_genome_to_fresh_successor() {
    // ── forebear: 2 capabilities + one high-conf and one low-conf claim ──
    let fdir = tempdir().unwrap();
    let froot: PathBuf = fdir.path().to_path_buf();
    let fgraph = froot.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&fgraph).unwrap();
    let ws = WorkspaceId::new();
    {
        let g = GraphStore::init(&fgraph).unwrap();
        let src = Source::new("file:///f.md".into(), SourceType::Document)
            .with_trust(TrustLevel::Trusted)
            .with_hash(ContentHash("h".into()));
        let sid = src.id;
        g.insert_source(&src).unwrap();

        let hi = Claim::new("water boils at 100C", ClaimType::Fact, sid, ws).with_confidence(0.95);
        let hid = hi.id.to_string();
        g.insert_claim(&hi).unwrap();
        g.link_claim_to_source(&hid, &sid.to_string()).unwrap();

        let lo =
            Claim::new("maybe it rains tomorrow", ClaimType::Fact, sid, ws).with_confidence(0.3);
        let lid = lo.id.to_string();
        g.insert_claim(&lo).unwrap();
        g.link_claim_to_source(&lid, &sid.to_string()).unwrap();

        g.put_function("greeter", "() => 'hi'", "javascript").unwrap();
        g.put_function("adder", "(a, b) => a + b", "javascript").unwrap();
    }

    // ── successor: empty ──
    let sdir = tempdir().unwrap();
    let sroot: PathBuf = sdir.path().to_path_buf();
    let sgraph = sroot.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&sgraph).unwrap();
    {
        let _g = GraphStore::init(&sgraph).unwrap();
    }

    let mut engine = QueryEngine::new();
    engine.mount("forebear".to_string(), froot).await.unwrap();
    engine.mount("successor".to_string(), sroot).await.unwrap();

    // Verified filter: no experience seeded → only_verified yields 0 capabilities.
    let verified_only = engine.bequeath("forebear", 0.7, true).await.unwrap();
    assert_eq!(
        verified_only.capabilities.len(),
        0,
        "unverified skills are NOT bequeathed"
    );

    // Full genome (only_verified=false): both caps + ONLY the high-conf claim.
    let bundle = engine.bequeath("forebear", 0.7, false).await.unwrap();
    assert_eq!(
        bundle.capabilities.len(),
        2,
        "both deployed capabilities in the genome"
    );
    assert_eq!(
        bundle.knowledge.len(),
        1,
        "only high-confidence knowledge — never the raw stream"
    );
    assert_eq!(bundle.knowledge[0].statement, "water boils at 100C");

    // Successor inherits → starts from the forebear's confirmed skills.
    let report = engine.inherit("successor", bundle).await.unwrap();
    assert_eq!(report.capabilities_inherited, 2);
    assert_eq!(report.knowledge_inherited, 1);

    // Proof: the successor now HAS the inherited capabilities.
    let succ_fns = engine.list_functions("successor").await.unwrap();
    let names: std::collections::BTreeSet<_> = succ_fns.iter().map(|f| f.name.clone()).collect();
    assert!(
        names.contains("greeter") && names.contains("adder"),
        "successor deploys the inherited genome, got {names:?}"
    );
}
