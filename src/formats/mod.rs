//! Format backends. Each archive type knows how to list its entries,
//! extract them in on-disk order, and free ("punch") the bytes an entry
//! occupied once it is safely out.
//!
//! Punching granularity differs per format:
//! - **zip**: per entry, after its CRC32 verifies (independent entries).
//! - **7z**: per solid block — a block's packed bytes are freed once every
//!   entry inside it has been extracted and CRC-verified.
//! - **rar**: per entry packed range. Solid archives are only punched when
//!   everything is being extracted (front-to-back), since later entries
//!   need the earlier compressed bytes to rebuild the dictionary.
//! - **tar / tar.gz / tar.bz2 / tar.xz / tar.zst / bare gz/bz2/xz/zst**:
//!   one continuous stream — bytes are freed behind the read cursor as the
//!   decompressor consumes them. These formats carry no per-file checksum,
//!   so the CRC-first guarantee is weaker here (integrity errors surface
//!   from the compression layer, for gzip only at end of stream).

pub mod rar;
pub mod sevenz;
pub mod stream;
pub mod zip;

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// One entry as shown to the user (listing order == event index space).
pub struct EntryInfo {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Progress events emitted during extraction.
pub enum Event {
    /// Extraction of entry `index` began.
    Started { index: usize },
    /// Entry `index` is fully extracted (and verified, where the format
    /// carries a checksum).
    Done { index: usize },
    /// Entry `index` was already hollowed out by an earlier run — its bytes
    /// are gone from the archive and it was extracted back then. Skipped.
    Resumed { index: usize },
    /// `bytes` of archive data were punched from disk. `indices` lists the
    /// entries whose payload that was (may be empty for stream formats).
    Freed { bytes: u64, indices: Vec<usize> },
    /// Entry `index` failed; extraction may continue with later entries.
    Error { index: usize, message: String },
    /// Informational message worth surfacing to the user.
    Note(String),
}

pub struct ExtractOptions<'a> {
    pub out_dir: &'a Path,
    /// Per-entry selection, parallel to `entries()`. `None` = everything.
    pub selected: Option<&'a [bool]>,
    /// Consume the archive as it extracts (hole punching).
    pub molt: bool,
}

impl ExtractOptions<'_> {
    pub fn is_selected(&self, index: usize) -> bool {
        self.selected.is_none_or(|s| s.get(index).copied().unwrap_or(false))
    }
    pub fn all_selected(&self, len: usize) -> bool {
        self.selected.is_none_or(|s| s.len() == len && s.iter().all(|&b| b))
    }
}

pub struct Summary {
    pub failed: usize,
    /// Entries skipped because an earlier run already extracted and
    /// hollowed them.
    pub resumed: usize,
    pub freed: u64,
    /// True when every entry in the archive is out and verified and the
    /// caller may delete the hollow shell (only ever true in molt mode).
    pub all_out: bool,
}

pub trait Backend {
    /// Short format name: "zip", "7z", "rar", "tar.gz", …
    fn kind(&self) -> &'static str;
    fn entries(&self) -> &[EntryInfo];
    /// Estimated peak disk usage in molt mode (remaining archive + working set).
    fn peak_estimate(&self) -> u64;
    /// Solid formats can only free space when everything is extracted together.
    fn is_solid(&self) -> bool;
    /// True when the archive contains encrypted entries and no (working)
    /// password was supplied — the UI should prompt and reopen.
    fn needs_password(&self) -> bool {
        false
    }
    fn extract(
        &mut self,
        opts: &ExtractOptions,
        on: &mut dyn FnMut(Event),
    ) -> io::Result<Summary>;

    fn total_uncompressed(&self) -> u64 {
        self.entries().iter().map(|e| e.size).sum()
    }
}

pub(crate) fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Marker error: the archive needs a (different) password. Detect it with
/// [`is_password_error`] so a UI can prompt instead of failing.
#[derive(Debug)]
pub struct PasswordError(pub String);

impl std::fmt::Display for PasswordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PasswordError {}

pub(crate) fn password_error(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, PasswordError(msg.into()))
}

pub fn is_password_error(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<PasswordError>())
}

/// Sniff the file's magic bytes and open the matching backend.
pub fn open(path: &Path) -> io::Result<Box<dyn Backend>> {
    open_with_password(path, None)
}

/// Like [`open`], with a password for encrypted zip/7z/rar archives.
pub fn open_with_password(
    path: &Path,
    password: Option<&str>,
) -> io::Result<Box<dyn Backend>> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 8];
    let n = f.read(&mut magic).unwrap_or(0);
    let mut ustar = [0u8; 5];
    let has_ustar = f.seek(SeekFrom::Start(257)).is_ok()
        && f.read_exact(&mut ustar).is_ok()
        && &ustar == b"ustar";
    drop(f);

    let m = &magic[..n];
    if m.starts_with(b"PK\x03\x04") || m.starts_with(b"PK\x05\x06") {
        return Ok(Box::new(zip::ZipBackend::open(path, password)?));
    }
    if m.starts_with(&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]) {
        return Ok(Box::new(sevenz::SevenZBackend::open(path, password)?));
    }
    if m.starts_with(b"Rar!\x1a\x07") {
        return Ok(Box::new(rar::RarBackend::open(path, password)?));
    }
    if m.starts_with(&[0x1F, 0x8B]) {
        return stream::open(path, stream::Compression::Gzip);
    }
    if m.starts_with(b"BZh") {
        return stream::open(path, stream::Compression::Bzip2);
    }
    if m.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        return stream::open(path, stream::Compression::Xz);
    }
    if m.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        return stream::open(path, stream::Compression::Zstd);
    }
    if has_ustar {
        return stream::open(path, stream::Compression::None);
    }
    Err(invalid(
        "unrecognized archive format \
         (supported: zip, 7z, rar, tar, tar.gz, tar.bz2, tar.xz, tar.zst, gz, bz2, xz, zst)",
    ))
}
