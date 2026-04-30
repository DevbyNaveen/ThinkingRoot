//! Live round-trip test against Sigstore-public-good — `#[ignore]`-
//! gated per the project convention for tests that hit live network
//! services. Run explicitly with:
//!
//! ```bash
//! SIGSTORE_LIVE_JWT="$(echo $YOUR_OIDC_TOKEN)" \
//!   cargo test -p tr-sigstore --features live live_keyless -- --ignored
//! ```
//!
//! The test signs a tiny synthetic pack via [`sign_canonical_bytes_keyless`],
//! exercising the full live flow: Fulcio cert request → DSSE PAE
//! signing with the ephemeral key → Rekor witness submission. It then
//! re-verifies the resulting bundle via
//! [`verify_bundle_against_canonical_bytes`] (DSSE crypto + subject
//! digest match). This proves the bundle is structurally a v3
//! Sigstore Bundle that the offline verifier accepts.
//!
//! What this test does NOT do:
//! - Validate the cert chain against a vendored Fulcio trust root —
//!   that requires shipping the actual public-good Fulcio root certs,
//!   which is a Phase F concern (`docs/2026-04-29-phase-f-trust-verify-spec.md`
//!   §3) handled separately.
//! - Verify the Rekor inclusion proof — would similarly need the
//!   public-good Rekor public key vendored. The bundle contains the
//!   inclusion-proof bytes; the offline replay logic in `rekor.rs` is
//!   already covered by `set_signature_round_trips` etc. with
//!   synthetic keys.
//!
//! How to obtain a token in CI: GitHub Actions exposes one through
//! `ACTIONS_ID_TOKEN_REQUEST_URL` + `ACTIONS_ID_TOKEN_REQUEST_TOKEN`
//! when the workflow declares `id-token: write` permissions. The
//! token must be requested with `audience=sigstore` for
//! Sigstore-public-good Fulcio to accept it.

#![cfg(feature = "live")]

use std::time::SystemTime;

use tr_sigstore::{
    live::{SignKeylessOptions, sign_canonical_bytes_keyless},
    verify_bundle_against_canonical_bytes,
};

/// Synthetic canonical-pack bytes — a deterministic placeholder
/// standing in for the real `(canonical_manifest || NUL ||
/// source.tar.zst || NUL || claims.jsonl)` that `V3PackBuilder`
/// emits. The keyless signer doesn't care what's inside, only that
/// it's deterministic so the round-trip subject digest matches.
fn synthetic_canonical_bytes() -> Vec<u8> {
    let mut b = Vec::with_capacity(256);
    b.extend_from_slice(b"format_version = \"tr/3\"\nname = \"alice/live-test\"\n");
    b.push(0);
    b.extend_from_slice(b"<source.tar.zst placeholder bytes>");
    b.push(0);
    b.extend_from_slice(br#"{"id":"c1","stmt":"hello world"}"#);
    b.push(b'\n');
    b
}

/// Live end-to-end Sigstore-public-good round-trip. Reads the OIDC
/// token from `$SIGSTORE_LIVE_JWT`; skips with a clear panic message
/// if that env var is absent (the test is `#[ignore]`-gated, so it
/// only runs under explicit `cargo test -- --ignored` and the user
/// has explicitly opted in to live network).
#[test]
#[ignore = "live Sigstore-public-good round-trip — set SIGSTORE_LIVE_JWT and run with --ignored"]
fn round_trips_against_sigstore_public_good() {
    let jwt = std::env::var("SIGSTORE_LIVE_JWT").unwrap_or_else(|_| {
        panic!(
            "SIGSTORE_LIVE_JWT not set. To run this test:\n\
             1. Obtain an OIDC id_token with aud=sigstore (browser flow,\n\
                ambient CI federated identity, or `gh auth token` with\n\
                an audience exchange).\n\
             2. SIGSTORE_LIVE_JWT=$TOKEN cargo test --features live \\\n\
                live_keyless -- --ignored"
        )
    });
    if jwt.is_empty() {
        panic!("SIGSTORE_LIVE_JWT is empty — provide a non-empty Sigstore OIDC JWT");
    }

    let canonical_bytes = synthetic_canonical_bytes();
    let pack_filename = "alice-live-test-1.0.0.tr";

    // The async runtime: `tokio::test` would also work, but a manual
    // runtime keeps the test self-contained without a #[tokio::test]
    // attribute (which would require pulling tokio's `macros` feature
    // into our `live` deps just for this one test).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let bundle = runtime
        .block_on(sign_canonical_bytes_keyless(
            &canonical_bytes,
            pack_filename,
            &jwt,
            SystemTime::now(),
            SignKeylessOptions::default(),
        ))
        .expect("keyless sign should succeed against live Sigstore-public-good");

    // Bundle structural assertions — a Fulcio-issued bundle must
    // carry a cert chain (no self-signed `publicKey`) and at least
    // one Rekor witness entry.
    assert!(
        bundle.verification_material.public_key.is_none(),
        "Sigstore-keyless bundle must not carry a self-signed publicKey"
    );
    let chain = bundle
        .verification_material
        .x509_certificate_chain
        .as_ref()
        .expect("Fulcio-issued bundle must carry x509CertificateChain");
    assert!(
        !chain.certificates.is_empty(),
        "cert chain must be non-empty (leaf at minimum)"
    );

    let tlog = bundle
        .verification_material
        .tlog_entries
        .first()
        .expect("Sigstore-keyless bundle must carry at least one Rekor entry");
    assert!(
        tlog.log_index > 0,
        "Rekor log_index must be positive (live Rekor never returns 0)"
    );
    assert_eq!(
        tlog.kind_version.as_ref().map(|kv| kv.kind.as_str()),
        Some("intoto"),
        "Sigstore-public-good must accept our intoto v0.0.2 entry"
    );
    assert_eq!(
        tlog.kind_version.as_ref().map(|kv| kv.version.as_str()),
        Some("0.0.2"),
        "Rekor must record the entry as intoto v0.0.2"
    );
    assert!(
        tlog.integrated_time > 1_700_000_000,
        "integrated_time must be after 2023-11-15 (sanity check; live Rekor only ever moves forward)"
    );

    // DSSE round-trip: the bundle's signature must verify and the
    // in-toto statement's subject digest must match the canonical
    // bytes we signed (matched via the dual `blake3` / `sha256`
    // dispatcher).
    let statement = verify_bundle_against_canonical_bytes(&bundle, &canonical_bytes)
        .expect("DSSE crypto + subject digest must round-trip");

    assert_eq!(
        statement.predicate_type,
        tr_sigstore::DSSE_STATEMENT_TYPE,
        "predicateType must be locked to v3 statement type"
    );
    assert_eq!(
        statement.predicate.format_version, "tr/3",
        "predicate.format_version must be tr/3"
    );
    assert_eq!(
        statement.subject.first().map(|s| s.name.as_str()),
        Some(pack_filename),
        "subject[0].name must round-trip the supplied pack_filename"
    );
    let digest = &statement.subject[0].digest;
    assert!(
        digest.contains_key("blake3"),
        "subject digest must include blake3 (our chain)"
    );
    assert!(
        digest.contains_key("sha256"),
        "subject digest must include sha256 (Sigstore ecosystem interop)"
    );

    // Print the bundle so a human running the test can eyeball it
    // against `cosign verify-blob` if they want a third-party check.
    eprintln!(
        "live Sigstore-public-good keyless bundle:\n{}",
        serde_json::to_string_pretty(&bundle).unwrap()
    );
}
