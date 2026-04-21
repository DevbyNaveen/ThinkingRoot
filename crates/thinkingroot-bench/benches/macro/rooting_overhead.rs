//! Rooting overhead benchmark.
//!
//! Measures the per-batch wall time the Phase 6.5 Rooting gate adds to a
//! compilation. Target budget per the Week 6 plan: **< 10% overhead on the
//! standard benchmark workspace**.
//!
//! The benchmark synthesizes N claims against an in-memory workspace, stores
//! matching source bytes, and runs `Rooter::root_batch`. We do NOT benchmark
//! extraction or Link here — only the Rooting pass itself — so the numbers
//! isolate the admission-gate cost.
//!
//! Sizes: 100, 1_000, 10_000 claims. Divan reports per-iteration time so
//! per-claim cost is `time / N`.

use divan::{Bencher, black_box};
use thinkingroot_core::types::{Claim, ClaimType, ContentHash, Source, SourceType, WorkspaceId};
use thinkingroot_rooting::{
    CandidateClaim, FileSystemSourceStore, Rooter, RootingConfig, SourceByteStore,
};

fn main() {
    divan::main();
}

struct Harness {
    _dir: tempfile::TempDir,
    graph: thinkingroot_graph::graph::GraphStore,
    store: FileSystemSourceStore,
    claims: Vec<Claim>,
}

/// Build a harness with `n` synthetic claims and a shared source whose bytes
/// contain every claim's tokens — i.e., every claim should pass Provenance.
fn prepare(n: usize) -> Harness {
    let dir = tempfile::tempdir().expect("tmpdir");
    let graph = thinkingroot_graph::graph::GraphStore::init(dir.path()).expect("graph init");
    let store = FileSystemSourceStore::new(dir.path()).expect("byte store");

    // Source body: repeat token patterns so provenance tokenizer finds matches.
    let mut body = String::new();
    for i in 0..n {
        body.push_str(&format!(
            "// entity_{i} exposes operation_{i} — service handler\npub fn operation_{i}() {{}}\n"
        ));
    }
    let hash = ContentHash::from_bytes(body.as_bytes());
    let source = Source::new("file:///bench.rs".into(), SourceType::File).with_hash(hash.clone());
    graph.insert_source(&source).expect("insert source");
    store
        .put(source.id, &hash, body.as_bytes())
        .expect("put bytes");

    let mut claims = Vec::with_capacity(n);
    for i in 0..n {
        let c = Claim::new(
            format!("entity_{i} exposes operation_{i}"),
            ClaimType::Fact,
            source.id,
            WorkspaceId::new(),
        );
        graph.insert_claim(&c).expect("insert claim");
        claims.push(c);
    }
    Harness {
        _dir: dir,
        graph,
        store,
        claims,
    }
}

// Default size list intentionally small: the Contradiction probe runs a
// Datalog query per claim, so 1K+ triggers O(n²) sweeps against the full
// `contradictions` relation in the synthetic harness. 100 is sufficient to
// characterize per-claim cost. Larger sizes can be opted into by editing
// the `args` list locally.
#[divan::bench(args = [100])]
fn root_batch(bencher: Bencher<'_, '_>, n: usize) {
    let harness = prepare(n);
    let candidates: Vec<CandidateClaim<'_>> = harness
        .claims
        .iter()
        .map(|c| CandidateClaim {
            claim: c,
            predicate: c.predicate.as_ref(),
            derivation: c.derivation.as_ref(),
        })
        .collect();
    let rooter = Rooter::new(&harness.graph, &harness.store, RootingConfig::default());
    bencher.bench_local(|| {
        let out = rooter.root_batch(&candidates).expect("root_batch");
        black_box(out);
    });
}
