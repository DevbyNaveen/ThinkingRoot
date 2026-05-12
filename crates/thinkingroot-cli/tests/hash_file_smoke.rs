//! `root hash-file <path>` prints the BLAKE3 hex digest of the
//! given file and exits 0.  Hidden subcommand used by `install.sh`
//! to populate the install manifest's `checksum_blake3` field at
//! install time.  No stable contract for external callers.

use std::process::Command;

#[test]
fn hash_file_emits_blake3_hex() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = tmp.path().join("payload.bin");
    let payload = b"the quick brown fox jumps over the lazy dog";
    std::fs::write(&fixture, payload).unwrap();

    let expected = {
        let mut h = blake3::Hasher::new();
        h.update(payload);
        h.finalize().to_hex().to_string()
    };

    let bin = env!("CARGO_BIN_EXE_root");
    let out = Command::new(bin)
        .arg("hash-file")
        .arg(&fixture)
        .output()
        .expect("spawn root");
    assert!(
        out.status.success(),
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.trim(), expected);
}
