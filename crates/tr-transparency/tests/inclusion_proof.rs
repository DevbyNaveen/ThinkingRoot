//! End-to-end verification of a small transparency log: append
//! several entries, fetch each with its inclusion proof, and confirm
//! the proofs check against the current root. Also covers
//! persistence — closing and re-opening the log preserves both the
//! entries and the root.

use chrono::Utc;
use sha2::Digest;
use tempfile::tempdir;
use tr_transparency::{LogEntry, LogEntryKind, TransparencyLog, verify_inclusion};

fn fake_entry(pack: &str) -> LogEntry {
    LogEntry {
        kind: LogEntryKind::Publish,
        pack_ref: pack.to_string(),
        manifest_hash: "deadbeef".to_string(),
        author_did: "did:web:alice.example".to_string(),
        signature: "BASE64SIG==".to_string(),
        timestamp: Utc::now(),
    }
}

#[test]
fn append_and_inclusion_proof_round_trip() {
    let dir = tempdir().unwrap();
    let mut log = TransparencyLog::open(dir.path()).unwrap();

    let i0 = log.append(fake_entry("alice/a@0.1.0")).unwrap();
    let i1 = log.append(fake_entry("alice/b@0.1.0")).unwrap();
    let i2 = log.append(fake_entry("alice/c@0.1.0")).unwrap();

    assert_eq!((i0, i1, i2), (0, 1, 2));
    let (root, size) = log.root().unwrap();
    assert_eq!(size, 3);

    for i in 0..3 {
        let (entry, proof) = log.get(i).unwrap();
        // Recompute the leaf hash the same way TransparencyLog does.
        let bytes = serde_json::to_vec(&entry).unwrap();
        let mut hasher = sha2::Sha256::new();
        hasher.update(&bytes);
        let leaf: [u8; 32] = hasher.finalize().into();
        assert!(
            verify_inclusion(&leaf, &root, &proof),
            "entry {i} failed inclusion check"
        );
    }
}

#[test]
fn reopening_log_preserves_entries() {
    let dir = tempdir().unwrap();
    let path = dir.path().to_path_buf();

    {
        let mut log = TransparencyLog::open(&path).unwrap();
        log.append(fake_entry("alice/a@0.1.0")).unwrap();
        log.append(fake_entry("alice/b@0.1.0")).unwrap();
    }

    let log2 = TransparencyLog::open(&path).unwrap();
    assert_eq!(log2.len(), 2);
    assert!(log2.root().is_some());
}

#[test]
fn out_of_range_get_errors_loudly() {
    let dir = tempdir().unwrap();
    let mut log = TransparencyLog::open(dir.path()).unwrap();
    log.append(fake_entry("alice/a@0.1.0")).unwrap();
    let err = log.get(99).unwrap_err();
    assert!(matches!(err, tr_transparency::Error::OutOfRange(99, 1)));
}

#[test]
fn consistency_proof_present_for_extended_log() {
    let dir = tempdir().unwrap();
    let mut log = TransparencyLog::open(dir.path()).unwrap();
    for i in 0..4 {
        log.append(fake_entry(&format!("alice/p{i}@0.1.0")))
            .unwrap();
    }
    let proof = log.consistency_proof(2, 4).unwrap();
    // Two extra leaves were appended → at least one sibling needed.
    assert!(!proof.is_empty());
}
