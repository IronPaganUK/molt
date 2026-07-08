//! Password-support tests over small committed fixtures (tests/data/*),
//! all created with 7-Zip using the password "molt123". Fixtures contain
//! f1.txt (200×"secret payload alpha") and f2.txt (150×"secret payload beta").

use molt::formats::{self, ExtractOptions};
use std::fs;
use std::path::{Path, PathBuf};

const PASSWORD: &str = "molt123";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

fn expected_f1() -> Vec<u8> {
    b"secret payload alpha\n".repeat(200)
}

/// Copy a fixture into a temp dir so molt mode can consume it.
fn temp_copy(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    let dst = dir.path().join(name);
    fs::copy(fixture(name), &dst).unwrap();
    dst
}

fn roundtrip_encrypted(name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let archive = temp_copy(&dir, name);
    let out = dir.path().join("out");

    // Without a password: opening either fails with a password error
    // (encrypted headers) or succeeds but reports needs_password.
    match formats::open(&archive) {
        Ok(b) => assert!(b.needs_password(), "{name}: should want a password"),
        Err(e) => assert!(formats::is_password_error(&e), "{name}: wrong error kind: {e}"),
    }

    // Wrong password: no successful extraction, archive must survive.
    // (Header-encrypted archives already fail at open, which is fine.)
    if let Ok(mut b) = formats::open_with_password(&archive, Some("wrong-password")) {
        let summary = b
            .extract(&ExtractOptions { out_dir: &out, selected: None, molt: true }, &mut |_| {})
            .expect("extract call itself should not abort");
        assert!(summary.failed > 0, "{name}: wrong password must fail entries");
        assert!(!summary.all_out);
    }
    assert!(archive.exists(), "{name}: archive must survive a wrong password");

    // Right password: full molt round-trip, shell consumed.
    let mut b = formats::open_with_password(&archive, Some(PASSWORD)).expect("open with pw");
    assert!(!b.needs_password());
    let out2 = dir.path().join("out2");
    let summary = b
        .extract(&ExtractOptions { out_dir: &out2, selected: None, molt: true }, &mut |_| {})
        .expect("extract");
    assert_eq!(summary.failed, 0, "{name}: extraction with password failed");
    assert!(summary.all_out);
    drop(b);
    fs::remove_file(&archive).unwrap();
    assert_eq!(fs::read(out2.join("f1.txt")).unwrap(), expected_f1(), "{name}: content");
}

#[test]
fn zipcrypto_roundtrip() {
    roundtrip_encrypted("enc.zip");
}

#[test]
fn zip_aes_roundtrip() {
    roundtrip_encrypted("enc-aes.zip");
}

#[test]
fn sevenz_encrypted_roundtrip() {
    roundtrip_encrypted("enc.7z");
}

#[test]
fn sevenz_encrypted_headers_roundtrip() {
    roundtrip_encrypted("enc-hdr.7z");
}

#[test]
fn plain_archives_do_not_ask_for_password() {
    // Regression guard: an unencrypted zip must not trip the password path.
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("plain.zip");
    let mut w = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
    use std::io::Write;
    w.start_file("a.txt", zip::write::FileOptions::default()).unwrap();
    w.write_all(b"hello").unwrap();
    w.finish().unwrap();

    let b = formats::open(&archive).unwrap();
    assert!(!b.needs_password());
}
