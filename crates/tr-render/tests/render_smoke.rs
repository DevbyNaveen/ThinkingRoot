//! Smoke tests covering the public surface of `tr-render`. The
//! markdown payload deliberately uses substring assertions rather
//! than exact-match snapshots — the goal is to catch *missing*
//! information, not to lock the precise wording in place.

use semver::Version;
use tr_format::{ClaimRecord, ManifestV3, V3PackBuilder, read_v3_pack};
use tr_render::render_preview;

fn make_pack() -> Vec<u8> {
    let mut manifest = ManifestV3::new("alice/demo", Version::parse("0.1.0").unwrap());
    manifest.description = Some("A small demo pack.".into());
    manifest.authors = vec!["alice".into(), "bob".into()];
    manifest.license = Some("Apache-2.0".into());
    manifest.extractor = Some("thinkingroot/extract@0.9.1".into());

    let mut b = V3PackBuilder::new(manifest);
    b.add_source_file("a.md", b"alpha\n").unwrap();
    b.add_source_file("b.md", b"beta\n").unwrap();
    b.add_claim(ClaimRecord::new(
        "c-1",
        "alpha is the first letter",
        vec!["alpha".into()],
        "a.md",
        0,
        5,
    ));
    b.build().unwrap()
}

#[test]
fn markdown_summary_includes_essentials() {
    let bytes = make_pack();
    let pack = read_v3_pack(&bytes).unwrap();
    let preview = render_preview(&pack).unwrap();
    let md = &preview.markdown;

    assert!(md.contains("alice/demo"), "missing name in markdown");
    assert!(md.contains("0.1.0"), "missing version");
    assert!(md.contains("Apache-2.0"), "missing license");
    assert!(md.contains("tr/3"), "missing format version");
    assert!(md.contains("alice"), "missing first author");
    assert!(md.contains("bob"), "missing second author");
    assert!(md.contains("unsigned"), "missing signature label");
    assert!(md.contains("Source files: 2"), "missing source file count");
    assert!(md.contains("Claims: 1"), "missing claim count");
}

#[test]
fn manifest_table_aligns_key_value_pairs() {
    let bytes = make_pack();
    let pack = read_v3_pack(&bytes).unwrap();
    let preview = render_preview(&pack).unwrap();
    let table = &preview.manifest_table;

    assert!(table.starts_with("key"));
    let mut lines = table.lines();
    let header = lines.next().unwrap();
    let ruler = lines.next().unwrap();
    assert_eq!(header.len(), ruler.len(), "ruler width must match header");

    assert!(table.contains("alice/demo"));
    assert!(table.contains("Apache-2.0"));
    assert!(table.contains("tr/3"));
    assert!(table.contains("blake3:"), "pack_hash should appear");
}

#[test]
fn archive_stats_are_populated() {
    let bytes = make_pack();
    let pack = read_v3_pack(&bytes).unwrap();
    let preview = render_preview(&pack).unwrap();

    assert_eq!(preview.source_count, 2);
    assert_eq!(preview.claim_count, 1);
    assert!(preview.source_archive_bytes > 0);
}

#[test]
fn unsigned_pack_renders_unsigned_label() {
    let bytes = make_pack();
    let pack = read_v3_pack(&bytes).unwrap();
    let preview = render_preview(&pack).unwrap();
    assert!(preview.markdown.contains("unsigned"));
    assert!(preview.manifest_table.contains("unsigned"));
}

#[test]
fn signed_pack_renders_self_signed_label() {
    use ed25519_dalek::SigningKey;
    let mut manifest = ManifestV3::new("alice/signed", Version::parse("0.1.0").unwrap());
    manifest.license = Some("MIT".into());

    let mut b = V3PackBuilder::new(manifest);
    b.add_source_file("a.md", b"hello\n").unwrap();

    let key_bytes = [42u8; 32];
    let key = SigningKey::from_bytes(&key_bytes);
    let bytes = b.build_signed(&key, "alice-signed-0.1.0.tr").unwrap();
    let pack = read_v3_pack(&bytes).unwrap();
    let preview = render_preview(&pack).unwrap();

    assert!(
        preview.markdown.contains("self-signed"),
        "expected self-signed label in markdown, got:\n{}",
        preview.markdown
    );
    assert!(preview.manifest_table.contains("ed25519"));
}
