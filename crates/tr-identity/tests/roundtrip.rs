//! Public-surface round-trip tests for `tr-identity`.

use tempfile::tempdir;
use tr_identity::{Did, DidMethod, Keypair, Keystore, PublicKeyRef, TrustedKey};

#[test]
fn keypair_sign_verify_round_trip() {
    let kp = Keypair::generate();
    let pk = kp.public();
    let sig = kp.sign(b"trustpack");
    pk.verify(b"trustpack", &sig).unwrap();
    assert!(pk.verify(b"trustpack-tampered", &sig).is_err());
}

#[test]
fn public_key_serializes_through_serde_json() {
    let kp = Keypair::generate();
    let pk = kp.public();
    let json = serde_json::to_string(&pk).unwrap();
    let pk2: PublicKeyRef = serde_json::from_str(&json).unwrap();
    assert_eq!(pk, pk2);
}

#[test]
fn keystore_persists_to_disk_and_reopens() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("trusted.json");

    {
        let mut store = Keystore::open(&path).unwrap();
        let kp = Keypair::generate();
        store.import_trusted(TrustedKey {
            id: "alice@thinkingroot.dev".into(),
            public: kp.public(),
            note: Some("hand-pasted".into()),
        });
        store.save().unwrap();
    }

    let reopened = Keystore::open(&path).unwrap();
    assert_eq!(reopened.len(), 1);
    let key = reopened.get("alice@thinkingroot.dev").unwrap();
    assert_eq!(key.note.as_deref(), Some("hand-pasted"));
}

#[test]
fn did_parses_known_methods() {
    let web = Did::parse("did:web:alice.example").unwrap();
    assert_eq!(web.method().unwrap(), DidMethod::Web);

    let tr = Did::parse("did:tr:agent:alice/researcher").unwrap();
    assert_eq!(tr.method().unwrap(), DidMethod::Tr);
}

#[test]
fn did_method_scheme_strings_are_stable() {
    assert_eq!(DidMethod::Web.scheme(), "web");
    assert_eq!(DidMethod::Tr.scheme(), "tr");
}

#[test]
fn keystore_default_path_resolves_under_thinkingroot_keys() {
    if let Some(p) = Keystore::default_path() {
        let display = p.display().to_string();
        assert!(display.contains("thinkingroot"));
        assert!(display.contains("keys"));
        assert!(display.ends_with("trusted.json"));
    }
}
