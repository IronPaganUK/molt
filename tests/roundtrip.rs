//! Round-trip tests: build an archive, molt it, verify the payload came out
//! intact and the archive was consumed (or kept, when that's the contract).

use molt::formats::{self, Event, ExtractOptions};
use std::fs;
use std::io::Write;
use std::path::Path;

/// (name, payload) — mix of compressible, incompressible, and nested paths.
fn payload() -> Vec<(&'static str, Vec<u8>)> {
    let mut rand = Vec::with_capacity(300_000);
    let mut x: u64 = 0x2545F4914F6CDD1D;
    for _ in 0..300_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        rand.push(x as u8);
    }
    vec![
        ("big1.txt", b"molt test line of compressible text\n".repeat(9000).to_vec()),
        ("data/big2.txt", b"another compressible payload here\n".repeat(8000).to_vec()),
        ("data/rand.bin", rand),
        ("small.txt", b"hello\n".to_vec()),
    ]
}

fn verify_out(out_dir: &Path) {
    for (name, data) in payload() {
        let got = fs::read(out_dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(got, data, "{name} content mismatch");
    }
}

fn extract_all(archive: &Path, out_dir: &Path, molt_mode: bool) -> formats::Summary {
    let mut backend = formats::open(archive).expect("open backend");
    let summary = backend
        .extract(
            &ExtractOptions { out_dir, selected: None, molt: molt_mode },
            &mut |_| {},
        )
        .expect("extract");
    drop(backend);
    if summary.all_out {
        fs::remove_file(archive).expect("delete consumed archive");
    }
    summary
}

fn make_zip(path: &Path) {
    let mut w = zip::ZipWriter::new(fs::File::create(path).unwrap());
    let opts = zip::write::FileOptions::default();
    for (name, data) in payload() {
        w.start_file(name, opts).unwrap();
        w.write_all(&data).unwrap();
    }
    w.finish().unwrap();
}

fn make_tar_gz(path: &Path) {
    let gz = flate2::write::GzEncoder::new(
        fs::File::create(path).unwrap(),
        flate2::Compression::default(),
    );
    let mut tar = tar::Builder::new(gz);
    for (name, data) in payload() {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, name, data.as_slice()).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
}

#[test]
fn zip_molt_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.zip");
    let out = dir.path().join("out");
    make_zip(&archive);

    let summary = extract_all(&archive, &out, true);
    assert_eq!(summary.failed, 0);
    assert!(summary.all_out, "zip should be fully consumable");
    assert!(summary.freed > 0, "zip should free bytes as it goes");
    assert!(!archive.exists(), "archive should be gone");
    verify_out(&out);
}

#[test]
fn zip_keep_mode_leaves_archive() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.zip");
    let out = dir.path().join("out");
    make_zip(&archive);
    let before = fs::read(&archive).unwrap();

    let summary = extract_all(&archive, &out, false);
    assert_eq!(summary.failed, 0);
    assert!(!summary.all_out);
    assert_eq!(summary.freed, 0);
    assert_eq!(fs::read(&archive).unwrap(), before, "archive must be untouched");
    verify_out(&out);
}

#[test]
fn zip_partial_selection_keeps_unselected_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.zip");
    let out = dir.path().join("out");
    make_zip(&archive);

    let mut backend = formats::open(&archive).expect("open backend");
    let n = backend.entries().len();
    // Select only the first entry.
    let mut selected = vec![false; n];
    selected[0] = true;
    let summary = backend
        .extract(
            &ExtractOptions { out_dir: &out, selected: Some(&selected), molt: true },
            &mut |_| {},
        )
        .expect("extract");
    assert_eq!(summary.failed, 0);
    assert!(!summary.all_out, "partial run must not consume the archive");
    drop(backend);

    // Resume: re-running skips the hollowed first entry, extracts the rest,
    // and finishes the job — including deleting the now-empty shell.
    let out2 = dir.path().join("out2");
    let mut backend = formats::open(&archive).expect("reopen backend");
    let mut resumed_events = 0;
    let summary2 = backend
        .extract(&ExtractOptions { out_dir: &out2, selected: None, molt: true }, &mut |ev| {
            if let Event::Resumed { .. } = ev {
                resumed_events += 1;
            }
        })
        .expect("extract rest");
    assert_eq!(summary2.failed, 0, "hollowed entry must be skipped, not failed");
    assert_eq!(summary2.resumed, 1);
    assert_eq!(resumed_events, 1);
    assert!(summary2.all_out, "resumed run should finish the job");
    for (name, data) in payload().into_iter().skip(1) {
        assert_eq!(fs::read(out2.join(name)).unwrap(), data, "{name} mismatch");
    }
    // The first entry came out in run 1, not run 2.
    assert!(!out2.join(payload()[0].0).exists());
    drop(backend);
    fs::remove_file(&archive).unwrap();
}

#[test]
fn zip_corrupt_entry_is_not_punched_and_archive_kept() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.zip");
    let out = dir.path().join("out");
    make_zip(&archive);

    // Flip bytes around 60% in — lands inside an entry's compressed data.
    let mut data = fs::read(&archive).unwrap();
    let off = data.len() * 6 / 10;
    data[off] ^= 0xFF;
    data[off + 1] ^= 0xFF;
    fs::write(&archive, &data).unwrap();

    let mut errors = 0;
    let mut backend = formats::open(&archive).expect("open backend");
    let summary = backend
        .extract(
            &ExtractOptions { out_dir: &out, selected: None, molt: true },
            &mut |ev| {
                if let Event::Error { .. } = ev {
                    errors += 1;
                }
            },
        )
        .expect("extract");
    assert!(summary.failed > 0, "corruption must be detected");
    assert_eq!(errors, summary.failed);
    assert!(!summary.all_out, "corrupt archive must be kept");
    assert!(archive.exists());
}

#[test]
fn tar_gz_molt_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.tar.gz");
    let out = dir.path().join("out");
    make_tar_gz(&archive);

    let backend = formats::open(&archive).expect("open backend");
    assert_eq!(backend.kind(), "tar.gz", "gz-wrapped tar must be sniffed as tar.gz");
    assert_eq!(backend.entries().len(), payload().len());
    drop(backend);

    let summary = extract_all(&archive, &out, true);
    assert_eq!(summary.failed, 0);
    assert!(summary.all_out);
    assert!(summary.freed > 0);
    assert!(!archive.exists());
    verify_out(&out);
}

#[test]
fn tar_gz_partial_selection_frees_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.tar.gz");
    let out = dir.path().join("out");
    make_tar_gz(&archive);

    let mut backend = formats::open(&archive).expect("open backend");
    let n = backend.entries().len();
    let mut selected = vec![true; n];
    selected[n - 1] = false;
    let mut noted = false;
    let summary = backend
        .extract(
            &ExtractOptions { out_dir: &out, selected: Some(&selected), molt: true },
            &mut |ev| {
                if let Event::Note(_) = ev {
                    noted = true;
                }
            },
        )
        .expect("extract");
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.freed, 0, "a partially-extracted stream must not be punched");
    assert!(!summary.all_out);
    assert!(noted, "user should be told why nothing was freed");
    assert!(archive.exists());
}

#[test]
fn bare_gz_single_file() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("big1.txt.gz");
    let out = dir.path().join("out");
    let data = b"molt test line of compressible text\n".repeat(9000);
    let mut gz = flate2::write::GzEncoder::new(
        fs::File::create(&archive).unwrap(),
        flate2::Compression::default(),
    );
    gz.write_all(&data).unwrap();
    gz.finish().unwrap();

    let backend = formats::open(&archive).expect("open backend");
    assert_eq!(backend.kind(), "gz");
    assert_eq!(backend.entries().len(), 1);
    assert_eq!(backend.entries()[0].name, "big1.txt");
    drop(backend);

    let summary = extract_all(&archive, &out, true);
    assert_eq!(summary.failed, 0);
    assert!(summary.all_out);
    assert_eq!(fs::read(out.join("big1.txt")).unwrap(), data);
    assert!(!archive.exists());
}
