//! Round-trip tests for the C2PA bridge. The real C2PA reader is
//! gated behind `--features c2pa-impl`; these tests cover the
//! always-available surface — embed/extract via the JSON layout
//! `tr-format` packs use, plus HTML emission.

use chrono::{TimeZone, Utc};
use semver::Version;
use tr_c2pa::{C2paAssertion, C2paManifest, emit_c2pa_for_html, extract_c2pa_claims};
use tr_format::manifest::Manifest;
use tr_format::writer::PackBuilder;

fn fixture_manifest() -> C2paManifest {
    C2paManifest {
        title: "demo-asset.png".into(),
        signed_at: Utc.with_ymd_and_hms(2026, 4, 27, 9, 30, 0).unwrap(),
        author: "did:web:alice.example".into(),
        assertions: vec![
            C2paAssertion {
                label: "c2pa.actions".into(),
                payload: serde_json::json!({"actions": [{"action": "c2pa.created"}]}),
            },
            C2paAssertion {
                label: "stds.schema-org.CreativeWork".into(),
                payload: serde_json::json!({"@type": "CreativeWork", "author": "alice"}),
            },
        ],
        raw: Vec::new(),
    }
}

#[test]
fn pack_extract_returns_persisted_assertions() {
    let mut pb = PackBuilder::new(Manifest::new(
        "alice/demo",
        Version::parse("0.1.0").unwrap(),
        "MIT",
    ));
    pb.put_text("artifacts/card.md", "# x").unwrap();
    let manifest_json = serde_json::to_vec(&fixture_manifest()).unwrap();
    pb.put_file("provenance/c2pa/manifest.json", &manifest_json)
        .unwrap();
    let bytes = pb.build().unwrap();
    let pack = tr_format::reader::read_bytes(&bytes).unwrap();

    let claims = extract_c2pa_claims(&pack).unwrap();
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0].label, "c2pa.actions");
    assert_eq!(claims[1].label, "stds.schema-org.CreativeWork");
}

#[test]
fn html_emission_includes_application_json_marker() {
    let html = emit_c2pa_for_html(&fixture_manifest()).unwrap();
    assert!(html.contains("application/c2pa+json"));
    assert!(html.contains("did:web:alice.example"));
}
