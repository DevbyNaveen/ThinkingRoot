//! Smoke tests covering the public surface of `tr-render`. The
//! markdown payload deliberately uses substring assertions rather
//! than exact-match snapshots — the goal is to catch *missing*
//! information, not to lock the precise wording in place.

use semver::Version;
use tr_format::capabilities::Capabilities;
use tr_format::manifest::Manifest;
use tr_format::writer::PackBuilder;
use tr_format::TrustTier;
use tr_render::render_preview;

fn make_pack() -> (Manifest, Vec<u8>) {
    let mut manifest = Manifest::new(
        "alice/demo",
        Version::parse("0.1.0").unwrap(),
        "Apache-2.0",
    );
    manifest.description = "A small demo pack.".into();
    manifest.authors = vec!["alice".into(), "bob".into()];
    manifest.tags = vec!["demo".into(), "test".into()];
    manifest.trust_tier = TrustTier::T1;
    manifest.claim_count = Some(7);
    manifest.rooted_pct = Some(80.0);
    manifest.capabilities = Capabilities {
        network: true,
        mcp_tools: vec!["query_claims".into()],
        ..Capabilities::default()
    };
    manifest.readme = Some("# Demo\n\nThis is a demo readme.".into());

    let mut pb = PackBuilder::new(manifest.clone());
    pb.put_text("artifacts/card.md", "# Hello").unwrap();
    pb.put_text("provenance/sources/a.src", "src 1").unwrap();
    pb.put_text("provenance/sources/b.src", "src 2").unwrap();
    let bytes = pb.build().unwrap();

    let final_manifest = tr_format::reader::read_bytes(&bytes).unwrap().manifest;
    (final_manifest, bytes)
}

#[test]
fn markdown_summary_includes_essentials() {
    let (m, bytes) = make_pack();
    let preview = render_preview(&m, &bytes).unwrap();
    let md = &preview.markdown;

    assert!(md.contains("alice/demo"), "missing name in markdown");
    assert!(md.contains("0.1.0"), "missing version");
    assert!(md.contains("Apache-2.0"), "missing license");
    assert!(md.contains("T1"), "missing trust tier");
    assert!(md.contains("80.0%"), "missing rooted_pct");
    assert!(md.contains("Outbound network"), "missing network capability");
    assert!(md.contains("query_claims"), "missing mcp_tools entry");
    assert!(md.contains("README"), "readme heading missing");
    assert!(md.contains("This is a demo readme"), "readme body missing");
}

#[test]
fn manifest_table_aligns_key_value_pairs() {
    let (m, bytes) = make_pack();
    let preview = render_preview(&m, &bytes).unwrap();
    let table = &preview.manifest_table;

    assert!(table.starts_with("key"));
    let mut lines = table.lines();
    let header = lines.next().unwrap();
    let ruler = lines.next().unwrap();
    assert_eq!(header.len(), ruler.len(), "ruler width must match header");

    assert!(table.contains("alice/demo"));
    assert!(table.contains("Apache-2.0"));
    assert!(table.contains("T1"));
}

#[test]
fn archive_stats_are_populated() {
    let (m, bytes) = make_pack();
    let preview = render_preview(&m, &bytes).unwrap();

    assert_eq!(preview.source_count, 2, "two provenance/ entries expected");
    assert!(
        preview.entry_count >= 3,
        "expected at least the three payload files (got {})",
        preview.entry_count
    );
    assert!(preview.payload_bytes > 0);
}

#[test]
fn empty_capabilities_renders_none_marker() {
    let manifest = Manifest::new(
        "alice/empty",
        Version::parse("0.1.0").unwrap(),
        "MIT",
    );
    let mut pb = PackBuilder::new(manifest.clone());
    pb.put_text("artifacts/x.md", "x").unwrap();
    let bytes = pb.build().unwrap();
    let final_manifest = tr_format::reader::read_bytes(&bytes).unwrap().manifest;

    let preview = render_preview(&final_manifest, &bytes).unwrap();
    assert!(preview.markdown.contains("_none declared_"));
}
