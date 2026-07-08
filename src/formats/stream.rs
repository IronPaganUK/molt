//! Stream backends: tar (optionally gzip/bzip2/xz/zstd-compressed) and bare
//! single-file compression. These formats are one continuous stream, so the
//! punching model is "free the bytes behind the read cursor": once the
//! decompressor has consumed a stretch of the archive it will never seek
//! back, and everything decoded from it has already been written out.
//!
//! Weaker guarantee than zip/7z/rar: tar has no per-file checksum and gzip
//! only validates its CRC at end-of-stream, so corruption may be detected
//! after some bytes are already freed. Files already written are unaffected.

use super::{invalid, Backend, EntryInfo, Event, ExtractOptions, Summary};
use crate::punch;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq)]
pub enum Compression {
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

impl Compression {
    fn label(self, tar: bool) -> &'static str {
        match (self, tar) {
            (Compression::None, _) => "tar",
            (Compression::Gzip, true) => "tar.gz",
            (Compression::Bzip2, true) => "tar.bz2",
            (Compression::Xz, true) => "tar.xz",
            (Compression::Zstd, true) => "tar.zst",
            (Compression::Gzip, false) => "gz",
            (Compression::Bzip2, false) => "bz2",
            (Compression::Xz, false) => "xz",
            (Compression::Zstd, false) => "zst",
        }
    }
}

/// Counts bytes consumed from the underlying file — everything before
/// `consumed` is safe to punch (the decompressor holds it in memory or is
/// already done with it; it never seeks backwards).
struct TrackingReader {
    inner: File,
    consumed: Arc<AtomicU64>,
}

impl Read for TrackingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.consumed.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

fn decoder(path: &Path, comp: Compression) -> io::Result<(Box<dyn Read>, Arc<AtomicU64>)> {
    let consumed = Arc::new(AtomicU64::new(0));
    let tr = TrackingReader { inner: File::open(path)?, consumed: Arc::clone(&consumed) };
    let r: Box<dyn Read> = match comp {
        Compression::None => Box::new(tr),
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(tr)),
        Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(tr)),
        Compression::Xz => Box::new(xz2::read::XzDecoder::new_multi_decoder(tr)),
        Compression::Zstd => Box::new(zstd::stream::read::Decoder::new(tr)?),
    };
    Ok((r, consumed))
}

/// Open a stream-format backend, deciding tar vs. single file by looking
/// for the tar magic in the decompressed stream (fallback: file extension).
pub fn open(path: &Path, comp: Compression) -> io::Result<Box<dyn Backend>> {
    let is_tar = if comp == Compression::None {
        true
    } else {
        sniff_tar(path, comp)? || {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            stem.to_ascii_lowercase().ends_with(".tar")
                || matches!(
                    path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
                    Some("tgz" | "tbz2" | "txz" | "tzst")
                )
        }
    };
    Ok(Box::new(StreamBackend::open(path, comp, is_tar)?))
}

fn sniff_tar(path: &Path, comp: Compression) -> io::Result<bool> {
    let (mut r, _) = decoder(path, comp)?;
    let mut head = [0u8; 512];
    let mut got = 0;
    while got < head.len() {
        match r.read(&mut head[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => return Ok(false), // corrupt stream: let extraction report it
        }
    }
    Ok(got >= 262 && &head[257..262] == b"ustar")
}

pub struct StreamBackend {
    path: PathBuf,
    comp: Compression,
    is_tar: bool,
    entries: Vec<EntryInfo>,
    archive_size: u64,
    largest: u64,
}

impl StreamBackend {
    fn open(path: &Path, comp: Compression, is_tar: bool) -> io::Result<Self> {
        let archive_size = fs::metadata(path)?.len();
        let mut entries = Vec::new();

        if is_tar {
            // Listing pass: stream through the whole archive once.
            let (r, _) = decoder(path, comp)?;
            let mut ar = tar::Archive::new(r);
            for entry in ar.entries()? {
                let e = entry.map_err(|e| invalid(format!("tar read failed: {e}")))?;
                let name = e.path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                entries.push(EntryInfo {
                    is_dir: e.header().entry_type().is_dir(),
                    size: e.header().size().unwrap_or(0),
                    name,
                });
            }
        } else {
            // Bare compressed file: one entry, sized by streaming through.
            let (mut r, _) = decoder(path, comp)?;
            let mut size = 0u64;
            let mut buf = vec![0u8; 1024 * 1024];
            loop {
                match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => size += n as u64,
                    Err(e) => return Err(invalid(format!("corrupt stream: {e}"))),
                }
            }
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "output".into());
            entries.push(EntryInfo { name, size, is_dir: false });
        }

        let largest = entries.iter().map(|e| e.size).max().unwrap_or(0);
        Ok(Self { path: path.to_path_buf(), comp, is_tar, entries, archive_size, largest })
    }
}

/// Punch everything consumed so far that hasn't been punched yet.
/// Returns the number of bytes freed by this call.
fn punch_behind(
    handle: &File,
    consumed: &AtomicU64,
    punched: &mut u64,
    min_step: u64,
) -> u64 {
    let consumed = consumed.load(Ordering::Relaxed);
    if consumed <= *punched || consumed - *punched < min_step {
        return 0;
    }
    let len = consumed - *punched;
    if punch::punch_hole(handle, *punched, len).is_ok() {
        *punched = consumed;
        len
    } else {
        0
    }
}

impl Backend for StreamBackend {
    fn kind(&self) -> &'static str {
        self.comp.label(self.is_tar)
    }

    fn entries(&self) -> &[EntryInfo] {
        &self.entries
    }

    fn peak_estimate(&self) -> u64 {
        self.archive_size + self.largest
    }

    fn is_solid(&self) -> bool {
        true // one continuous stream: all-or-nothing for space freeing
    }

    fn extract(
        &mut self,
        opts: &ExtractOptions,
        on: &mut dyn FnMut(Event),
    ) -> io::Result<Summary> {
        let full_run = opts.all_selected(self.entries.len());
        let mut punching = opts.molt;
        if punching && !full_run {
            punching = false;
            on(Event::Note(
                "stream archive: partial extraction frees no space \
                 (the archive is one continuous stream)"
                    .into(),
            ));
        }
        if punching && self.comp != Compression::None {
            on(Event::Note(
                "stream format: bytes are freed behind the read cursor; this \
                 format has no per-file checksum to verify first"
                    .into(),
            ));
        }

        let punch_handle = if punching {
            let h = OpenOptions::new().read(true).write(true).open(&self.path)?;
            punch::prepare(&h)
                .map_err(|e| invalid(format!("filesystem can't hole-punch here: {e}")))?;
            Some(h)
        } else {
            None
        };
        fs::create_dir_all(opts.out_dir)?;

        const STEP: u64 = 8 * 1024 * 1024; // punch in ≥8 MiB strides
        let mut punched = 0u64;
        let mut freed = 0u64;
        let mut failed = 0usize;
        let mut done_since_punch: Vec<usize> = Vec::new();

        let (r, consumed) = decoder(&self.path, self.comp)?;

        if self.is_tar {
            let mut ar = tar::Archive::new(r);
            let iter = match ar.entries() {
                Ok(it) => it,
                Err(e) => return Err(invalid(format!("tar read failed: {e}"))),
            };
            let mut index = 0usize;
            for entry in iter {
                let stop = index >= self.entries.len();
                let mut e = match entry {
                    Ok(e) => e,
                    Err(err) => {
                        for i in index..self.entries.len() {
                            if opts.is_selected(i) {
                                failed += 1;
                                on(Event::Error {
                                    index: i,
                                    message: format!("stream failed: {err}"),
                                });
                            }
                        }
                        index = self.entries.len();
                        break;
                    }
                };
                if stop {
                    break; // archive grew since listing? extract nothing extra
                }
                if opts.is_selected(index) {
                    on(Event::Started { index });
                    // unpack_in refuses paths that escape the destination.
                    match e.unpack_in(opts.out_dir) {
                        Ok(true) => {
                            on(Event::Done { index });
                            done_since_punch.push(index);
                        }
                        Ok(false) => {
                            failed += 1;
                            on(Event::Error {
                                index,
                                message: "skipped: unsafe path in archive".into(),
                            });
                        }
                        Err(err) => {
                            failed += 1;
                            on(Event::Error { index, message: err.to_string() });
                        }
                    }
                }
                if let Some(h) = &punch_handle {
                    let b = punch_behind(h, &consumed, &mut punched, STEP);
                    if b > 0 {
                        freed += b;
                        on(Event::Freed { bytes: b, indices: std::mem::take(&mut done_since_punch) });
                    }
                }
                index += 1;
            }
            // Anything the listing promised but the stream never produced.
            for i in index..self.entries.len() {
                if opts.is_selected(i) {
                    failed += 1;
                    on(Event::Error { index: i, message: "missing from stream".into() });
                }
            }
        } else {
            // Single decompressed file.
            let mut r = r;
            on(Event::Started { index: 0 });
            let dest = opts.out_dir.join(&self.entries[0].name);
            let result = (|| -> io::Result<()> {
                let mut out = File::create(&dest)?;
                let mut buf = vec![0u8; 4 * 1024 * 1024];
                loop {
                    let n = r.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    out.write_all(&buf[..n])?;
                    if let Some(h) = &punch_handle {
                        let b = punch_behind(h, &consumed, &mut punched, STEP);
                        if b > 0 {
                            freed += b;
                            on(Event::Freed { bytes: b, indices: vec![] });
                        }
                    }
                }
                out.flush()
            })();
            match result {
                Ok(()) => {
                    on(Event::Done { index: 0 });
                    done_since_punch.push(0);
                }
                Err(e) => {
                    failed += 1;
                    on(Event::Error { index: 0, message: e.to_string() });
                }
            }
        }

        // Final sweep: free the tail (trailers, padding) behind the cursor.
        if let Some(h) = &punch_handle {
            if failed == 0 {
                let b = punch_behind(h, &consumed, &mut punched, 1);
                if b > 0 {
                    freed += b;
                    on(Event::Freed { bytes: b, indices: std::mem::take(&mut done_since_punch) });
                }
            }
        }

        let all_out = opts.molt && failed == 0 && full_run;
        // Streams can't resume: there are no per-entry byte ranges to test.
        Ok(Summary { failed, resumed: 0, freed, all_out })
    }
}
