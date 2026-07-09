//! RAR backend. Extraction and CRC verification are done by the unrar
//! library (which processes entries strictly front-to-back); the packed
//! byte range of each entry is recovered by our own lightweight RAR4/RAR5
//! header walk, since unrar does not expose file offsets.
//!
//! Non-solid archives: each entry's packed range is punched right after
//! unrar reports it extracted (unrar verifies the CRC before returning).
//! Solid archives: later entries need the earlier compressed bytes to
//! rebuild the dictionary, so punching is only enabled when *everything*
//! is being extracted in one front-to-back pass.

use super::{invalid, password_error, Backend, EntryInfo, Event, ExtractOptions, Summary};
use crate::punch;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Packed data range of one file header, from our own parse.
struct RarRange {
    data_start: u64,
    data_len: u64,
    solid: bool,
}

pub struct RarBackend {
    path: PathBuf,
    entries: Vec<EntryInfo>,
    /// Parallel to `entries` when header parsing succeeded; empty otherwise
    /// (extraction still works, punching is disabled).
    ranges: Vec<RarRange>,
    /// Why `ranges` ended up empty, for diagnostics — set only when it did.
    parse_diag: Option<String>,
    /// Parallel to `entries`: entry content is encrypted.
    encrypted_flags: Vec<bool>,
    password: Option<String>,
    any_encrypted: bool,
    solid: bool,
    packed_total: u64,
    largest: u64,
}

fn rar_archive<'a>(path: &'a Path, password: Option<&'a str>) -> unrar::Archive<'a> {
    match password {
        Some(pw) => unrar::Archive::with_password(path, pw),
        None => unrar::Archive::new(path),
    }
}

/// Map an unrar error, flagging the password-related codes so UIs prompt.
fn rar_err(context: &str, e: unrar::error::UnrarError) -> io::Error {
    use unrar::error::Code;
    match e.code {
        Code::MissingPassword => password_error(format!("{context}: password required")),
        Code::BadPassword => password_error(format!("{context}: wrong password")),
        _ => invalid(format!("{context}: {e}")),
    }
}

impl RarBackend {
    pub fn open(path: &Path, password: Option<&str>) -> io::Result<Self> {
        // Listing via unrar: names, sizes, dir/encrypted/split flags.
        // Header-encrypted archives need the password even for this.
        let mut entries = Vec::new();
        let mut encrypted_flags = Vec::new();
        let (mut any_split, mut any_encrypted) = (false, false);
        let archive = rar_archive(path, password)
            .open_for_listing()
            .map_err(|e| rar_err("cannot list rar", e))?;
        for header in archive {
            let h = header.map_err(|e| rar_err("rar listing failed", e))?;
            any_split |= h.is_split();
            any_encrypted |= h.is_encrypted();
            encrypted_flags.push(h.is_encrypted());
            entries.push(EntryInfo {
                name: h.filename.to_string_lossy().replace('\\', "/"),
                size: h.unpacked_size,
                is_dir: h.is_directory(),
            });
        }
        if any_split {
            return Err(invalid("multi-volume rar archives are not supported"));
        }

        // Byte ranges via our own header walk. If it disagrees with the
        // listing, extraction still works but nothing is punched.
        let mut parse_diag = None;
        let ranges = match parse_ranges(path) {
            Ok(r) if r.len() == entries.len() => r,
            Ok(r) => {
                parse_diag = Some(format!(
                    "header walk found {} entries, unrar listed {}",
                    r.len(),
                    entries.len()
                ));
                Vec::new()
            }
            Err(e) => {
                parse_diag = Some(e.to_string());
                Vec::new()
            }
        };

        let solid = ranges.iter().any(|r| r.solid);
        let packed_total: u64 = ranges.iter().map(|r| r.data_len).sum();
        let largest = entries.iter().map(|e| e.size).max().unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            entries,
            ranges,
            parse_diag,
            encrypted_flags,
            password: password.map(str::to_owned),
            any_encrypted,
            solid,
            packed_total,
            largest,
        })
    }
}

impl Backend for RarBackend {
    fn kind(&self) -> &'static str {
        "rar"
    }

    fn entries(&self) -> &[EntryInfo] {
        &self.entries
    }

    fn peak_estimate(&self) -> u64 {
        self.packed_total + self.largest
    }

    fn is_solid(&self) -> bool {
        self.solid
    }

    fn needs_password(&self) -> bool {
        self.any_encrypted && self.password.is_none()
    }

    fn extract(
        &mut self,
        opts: &ExtractOptions,
        on: &mut dyn FnMut(Event),
    ) -> io::Result<Summary> {
        let full_run = opts.all_selected(self.entries.len());
        let mut punching = opts.molt && !self.ranges.is_empty();
        if punching && self.solid && !full_run {
            punching = false;
            on(Event::Note(
                "solid rar: partial extraction frees no space (later entries \
                 need the earlier compressed bytes)"
                    .into(),
            ));
        }
        if opts.molt && self.ranges.is_empty() {
            on(Event::Note(match &self.parse_diag {
                Some(reason) => format!(
                    "could not map this rar's entry offsets ({reason}); extracting without freeing"
                ),
                None => "could not map this rar's entry offsets; extracting without freeing".into(),
            }));
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
        // Read-only handle for resume detection (works in --keep mode too).
        let check_handle =
            if self.ranges.is_empty() { None } else { fs::File::open(&self.path).ok() };

        let mut freed = 0u64;
        let mut failed = 0usize;
        let mut resumed = 0usize;

        let mut archive = rar_archive(&self.path, self.password.as_deref())
            .open_for_processing()
            .map_err(|e| rar_err("cannot open rar", e))?;
        let mut index = 0usize;

        loop {
            let before_file = match archive.read_header() {
                Ok(Some(h)) => h,
                Ok(None) => break,
                Err(e) => {
                    // Header walk broke: everything not yet reached failed.
                    for i in index..self.entries.len() {
                        if opts.is_selected(i) {
                            failed += 1;
                            on(Event::Error { index: i, message: format!("rar error: {e}") });
                        }
                    }
                    break;
                }
            };
            if index >= self.entries.len() {
                // More headers than the listing showed; extract nothing else.
                break;
            }
            let mut selected = opts.is_selected(index);

            // No password for an encrypted entry: fail it cleanly and move
            // on, instead of letting unrar abort the whole run.
            if selected && self.encrypted_flags[index] && self.password.is_none() {
                failed += 1;
                on(Event::Error {
                    index,
                    message: "entry is encrypted; password required".into(),
                });
                selected = false; // fall through to the skip() path below
            }

            // Hollowed by an earlier run → already extracted back then.
            // (For solid archives this can only be a leading prefix, which
            // is exactly what unrar can't re-decode anyway.)
            if selected {
                if let Some(h) = &check_handle {
                    let r = &self.ranges[index];
                    if punch::hollowed(h, r.data_start, r.data_len).unwrap_or(false) {
                        resumed += 1;
                        on(Event::Resumed { index });
                        match before_file.skip() {
                            Ok(next) => {
                                archive = next;
                                index += 1;
                                continue;
                            }
                            Err(e) => {
                                for i in index + 1..self.entries.len() {
                                    if opts.is_selected(i) {
                                        failed += 1;
                                        on(Event::Error {
                                            index: i,
                                            message: format!("rar error: {e}"),
                                        });
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
            }
            // unrar sanitizes paths itself, but reject traversal outright.
            let unsafe_path = Path::new(&self.entries[index].name)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));

            if selected && !unsafe_path {
                on(Event::Started { index });
                match before_file.extract_with_base(opts.out_dir) {
                    Ok(next) => {
                        archive = next;
                        if let Some(h) = &punch_handle {
                            let r = &self.ranges[index];
                            if r.data_len > 0
                                && punch::punch_hole(h, r.data_start, r.data_len).is_ok()
                            {
                                freed += r.data_len;
                                on(Event::Freed { bytes: r.data_len, indices: vec![index] });
                            }
                        }
                        on(Event::Done { index });
                    }
                    Err(e) => {
                        // The typestate handle is consumed on error; the
                        // rest of the archive is unreachable in this run.
                        failed += 1;
                        on(Event::Error { index, message: format!("rar error: {e}") });
                        for i in index + 1..self.entries.len() {
                            if opts.is_selected(i) {
                                failed += 1;
                                on(Event::Error {
                                    index: i,
                                    message: "unreachable after previous rar error".into(),
                                });
                            }
                        }
                        break;
                    }
                }
            } else {
                if selected && unsafe_path {
                    failed += 1;
                    on(Event::Error {
                        index,
                        message: format!("unsafe path in archive: {}", self.entries[index].name),
                    });
                }
                match before_file.skip() {
                    Ok(next) => archive = next,
                    Err(e) => {
                        for i in index + 1..self.entries.len() {
                            if opts.is_selected(i) {
                                failed += 1;
                                on(Event::Error { index: i, message: format!("rar error: {e}") });
                            }
                        }
                        break;
                    }
                }
            }
            index += 1;
        }

        let all_out = opts.molt && failed == 0 && full_run && !self.ranges.is_empty();
        Ok(Summary { failed, resumed, freed, all_out })
    }
}

// ---------------------------------------------------------------- header walk
//
// Minimal RAR4 / RAR5 block walk collecting, for every *file* header, the
// absolute byte range of its packed data. No decompression, no CRC math —
// just enough structure to know which bytes belong to which entry.

fn parse_ranges(path: &Path) -> io::Result<Vec<RarRange>> {
    let mut f = std::io::BufReader::new(fs::File::open(path)?);
    let mut sig = [0u8; 8];
    f.read_exact(&mut sig[..7])?;
    if &sig[..7] == b"Rar!\x1a\x07\x00" {
        return parse_rar4(&mut f, 7);
    }
    f.read_exact(&mut sig[7..])?;
    if &sig == b"Rar!\x1a\x07\x01\x00" {
        return parse_rar5(&mut f, 8);
    }
    Err(invalid("unknown rar signature"))
}

fn rd<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        let n = r.read(&mut buf[got..])?;
        if n == 0 {
            return Ok(got == 0); // clean EOF only at a block boundary
        }
        got += n;
    }
    Ok(false)
}

/// RAR5 variable-length integer: 7 bits per byte, high bit = continuation.
fn vint(buf: &[u8], pos: &mut usize) -> io::Result<u64> {
    let mut v = 0u64;
    for shift in (0..70).step_by(7) {
        let b = *buf.get(*pos).ok_or_else(|| invalid("rar5: truncated vint"))?;
        *pos += 1;
        v |= u64::from(b & 0x7F) << shift.min(63);
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(invalid("rar5: vint too long"))
}

fn parse_rar5<R: Read>(f: &mut R, start: u64) -> io::Result<Vec<RarRange>> {
    let mut pos = start; // absolute offset of the next header
    let mut out = Vec::new();
    loop {
        let mut fixed = [0u8; 4]; // header CRC32
        if rd(f, &mut fixed)? {
            break; // EOF
        }
        // header size is a vint read byte-by-byte
        let mut size_bytes = Vec::with_capacity(3);
        let header_size = loop {
            let mut b = [0u8; 1];
            if rd(f, &mut b)? {
                return Err(invalid("rar5: truncated header size"));
            }
            size_bytes.push(b[0]);
            if b[0] & 0x80 == 0 {
                let mut p = 0;
                break vint(&size_bytes, &mut p)?;
            }
            if size_bytes.len() > 3 {
                return Err(invalid("rar5: header size vint too long"));
            }
        };
        let mut hdr = vec![0u8; header_size as usize];
        if rd(f, &mut hdr)? {
            return Err(invalid("rar5: truncated header"));
        }
        let header_end = pos + 4 + size_bytes.len() as u64 + header_size;

        let mut p = 0usize;
        let htype = vint(&hdr, &mut p)?;
        let hflags = vint(&hdr, &mut p)?;
        let _extra_size = if hflags & 0x01 != 0 { vint(&hdr, &mut p)? } else { 0 };
        let data_size = if hflags & 0x02 != 0 { vint(&hdr, &mut p)? } else { 0 };

        match htype {
            2 => {
                // file header: file_flags, unpacked_size, attributes,
                // [mtime u32], [data crc u32], compression_info, ...
                let file_flags = vint(&hdr, &mut p)?;
                let _unpacked = vint(&hdr, &mut p)?;
                let _attrs = vint(&hdr, &mut p)?;
                if file_flags & 0x02 != 0 {
                    p += 4;
                }
                if file_flags & 0x04 != 0 {
                    p += 4;
                }
                let comp_info = vint(&hdr, &mut p)?;
                out.push(RarRange {
                    data_start: header_end,
                    data_len: data_size,
                    solid: comp_info & 0x40 != 0,
                });
            }
            4 => return Err(invalid("rar5: encrypted headers")),
            5 => break, // end of archive
            _ => {}     // main/service headers: skip
        }

        // seek past the data area by draining (reader is buffered)
        io::copy(&mut f.take(data_size), &mut io::sink())?;
        pos = header_end + data_size;
    }
    Ok(out)
}

fn parse_rar4<R: Read>(f: &mut R, start: u64) -> io::Result<Vec<RarRange>> {
    let mut pos = start;
    let mut out = Vec::new();
    loop {
        let mut head = [0u8; 7];
        if rd(f, &mut head)? {
            break; // EOF
        }
        let htype = head[2];
        let flags = u16::from_le_bytes([head[3], head[4]]);
        let head_size = u16::from_le_bytes([head[5], head[6]]) as u64;
        if head_size < 7 {
            return Err(invalid("rar4: bad header size"));
        }
        let mut rest = vec![0u8; head_size as usize - 7];
        if rd(f, &mut rest)? {
            return Err(invalid("rar4: truncated header"));
        }

        // How much trailing data (beyond the header itself) follows before
        // the next header, if any. RAR4 has no generic size field in the
        // common 7-byte header — each block type that carries data puts
        // its own size field at a fixed spot in its own layout, so this
        // has to be worked out per header type below.
        let mut add_size = 0u64;

        match htype {
            0x73 => {
                // main header: encrypted headers can't be walked
                if flags & 0x0080 != 0 {
                    return Err(invalid("rar4: encrypted headers"));
                }
            }
            0x74 | 0x7A => {
                // File (0x74) and service/sub-block (0x7A) headers share
                // the FileHeader layout: PackSize is its first field,
                // unconditionally — no flag gates its presence.
                if rest.len() < 4 {
                    return Err(invalid("rar4: truncated file header"));
                }
                let mut pack = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as u64;
                // HIGH_PACK_SIZE (LHD_LARGE) extends PackSize past 4 GiB;
                // it sits right after the fixed 25-byte field block.
                if flags & 0x0100 != 0 && rest.len() >= 29 {
                    let hp = &rest[25..29];
                    pack |= (u32::from_le_bytes([hp[0], hp[1], hp[2], hp[3]]) as u64) << 32;
                }
                add_size = pack;
                if htype == 0x74 {
                    out.push(RarRange {
                        data_start: pos + head_size,
                        data_len: pack,
                        solid: flags & 0x0010 != 0,
                    });
                }
            }
            0x77 | 0x78 => {
                // Old-style service block (0x77) and recovery record /
                // PROTECT (0x78) both lead with a 4-byte DataSize.
                if rest.len() < 4 {
                    return Err(invalid("rar4: truncated protect/service header"));
                }
                add_size = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as u64;
            }
            0x7B => break, // end of archive
            // Comment (0x75), AV signature (0x76), sign (0x79): legacy
            // block types with no trailing data block of their own —
            // anything they carry lives inside head_size itself.
            _ => {}
        }

        io::copy(&mut f.take(add_size), &mut io::sink())?;
        pos += head_size + add_size;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a RAR4 common 7-byte block header.
    fn common(htype: u8, flags: u16, head_size: u16, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&[0, 0]); // HeadCRC (unchecked by our parser)
        buf.push(htype);
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&head_size.to_le_bytes());
    }

    /// A minimal RAR4 FILE header: fixed 25-byte field block (PackSize,
    /// UnpSize, HostOS, CRC32, FileTime, UnpVer, Method, NameSize,
    /// FileAttr) plus a 1-byte name, followed by `pack_size` bytes of
    /// dummy compressed data.
    fn file_header(name: u8, pack_size: u32, buf: &mut Vec<u8>) {
        let rest_len = 25 + 1; // fixed fields + 1-byte name
        common(0x74, 0x0000, 7 + rest_len as u16, buf);
        buf.extend_from_slice(&pack_size.to_le_bytes()); // PackSize
        buf.extend_from_slice(&pack_size.to_le_bytes()); // UnpSize (low)
        buf.push(0); // HostOS
        buf.extend_from_slice(&[0; 4]); // CRC32
        buf.extend_from_slice(&[0; 4]); // FileTime
        buf.push(20); // UnpVer
        buf.push(0x30); // Method
        buf.extend_from_slice(&1u16.to_le_bytes()); // NameSize
        buf.extend_from_slice(&[0; 4]); // FileAttr
        buf.push(name);
        buf.extend(std::iter::repeat(0xEE).take(pack_size as usize));
    }

    /// A recovery-record (PROTECT) header: DataSize, Version, RecSectors,
    /// TotalBlocks, an 8-byte mark, then `data_size` bytes of recovery
    /// data — the exact block type that used to desync the RAR4 walk.
    fn protect_header(data_size: u32, buf: &mut Vec<u8>) {
        let rest_len = 4 + 1 + 2 + 4 + 8;
        common(0x78, 0x0000, 7 + rest_len as u16, buf);
        buf.extend_from_slice(&data_size.to_le_bytes());
        buf.push(0); // Version
        buf.extend_from_slice(&[0; 2]); // RecSectors
        buf.extend_from_slice(&[0; 4]); // TotalBlocks
        buf.extend_from_slice(&[0; 8]); // Mark
        buf.extend(std::iter::repeat(0xBB).take(data_size as usize));
    }

    fn main_header(buf: &mut Vec<u8>) {
        common(0x73, 0x0000, 7 + 6, buf);
        buf.extend_from_slice(&[0; 6]); // HighPosAV + PosAV
    }

    fn end_header(buf: &mut Vec<u8>) {
        common(0x7B, 0x0000, 7, buf);
    }

    #[test]
    fn rar4_skips_recovery_record_between_files() {
        let mut body = Vec::new();
        main_header(&mut body);
        file_header(b'a', 5, &mut body);
        protect_header(50, &mut body); // must not desync the walk
        file_header(b'b', 3, &mut body);
        end_header(&mut body);

        let mut archive = b"Rar!\x1a\x07\x00".to_vec();
        archive.extend_from_slice(&body);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.rar");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&archive).unwrap();
        drop(f);

        let ranges = parse_ranges(&path).expect("parse_ranges must not error");
        assert_eq!(ranges.len(), 2, "PROTECT header must not appear as a fake entry");
        assert_eq!(ranges[0].data_len, 5, "a.txt PackSize");
        assert_eq!(ranges[1].data_len, 3, "b.txt PackSize");
    }

    #[test]
    fn rar4_single_file_no_recovery_record() {
        let mut body = Vec::new();
        main_header(&mut body);
        file_header(b'x', 42, &mut body);
        end_header(&mut body);

        let mut archive = b"Rar!\x1a\x07\x00".to_vec();
        archive.extend_from_slice(&body);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.rar");
        std::fs::write(&path, &archive).unwrap();

        let ranges = parse_ranges(&path).expect("parse_ranges must not error");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].data_len, 42);
        // signature(7) + main header(7+6=13) + file header(7+26=33)
        assert_eq!(ranges[0].data_start, 7 + 13 + 33);
    }
}
