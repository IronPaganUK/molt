//! ZIP backend — entries are independent, so each one's compressed range is
//! punched as soon as its CRC32 verifies. Entries are processed in on-disk
//! order so the freed space forms a contiguous wave behind the read cursor.

use super::{invalid, password_error, Backend, EntryInfo, Event, ExtractOptions, Summary};
use crate::punch;
use crate::util::safe_join;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

pub struct ZipBackend {
    path: PathBuf,
    zip: ZipArchive<File>,
    entries: Vec<EntryInfo>,
    /// Parallel to `entries`: (zip index, data_start, compressed, encrypted).
    meta: Vec<(usize, u64, u64, bool)>,
    password: Option<Vec<u8>>,
    any_encrypted: bool,
    total_compressed: u64,
    largest: u64,
}

impl ZipBackend {
    pub fn open(path: &Path, password: Option<&str>) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut zip = ZipArchive::new(file)
            .map_err(|e| invalid(format!("not a readable zip: {e}")))?;

        let count = zip.len();
        let mut order: Vec<(usize, u64, u64, String, u64, bool)> = Vec::with_capacity(count);
        let (mut total_compressed, mut largest) = (0u64, 0u64);
        let mut any_encrypted = false;
        for i in 0..count {
            let entry = zip.by_index_raw(i).map_err(|e| invalid(e.to_string()))?;
            order.push((
                i,
                entry.data_start(),
                entry.compressed_size(),
                entry.name().to_string(),
                entry.size(),
                false,
            ));
            total_compressed += entry.compressed_size();
            largest = largest.max(entry.size());
        }
        // The 0.6 zip crate has no public encrypted-flag accessor; probing
        // by_index errors with a distinct message for encrypted entries.
        for rec in order.iter_mut() {
            if let Err(zip::result::ZipError::UnsupportedArchive(msg)) = zip.by_index(rec.0) {
                if msg.contains("Password") {
                    rec.5 = true;
                    any_encrypted = true;
                }
            }
        }
        // On-disk order → freed space is a contiguous wave behind the cursor.
        order.sort_by_key(|&(_, start, _, _, _, _)| start);

        let mut entries = Vec::with_capacity(count);
        let mut meta = Vec::with_capacity(count);
        for (idx, start, compressed, name, size, encrypted) in order {
            entries.push(EntryInfo { is_dir: name.ends_with('/'), name, size });
            meta.push((idx, start, compressed, encrypted));
        }
        Ok(Self {
            path: path.to_path_buf(),
            zip,
            entries,
            meta,
            password: password.map(|p| p.as_bytes().to_vec()),
            any_encrypted,
            total_compressed,
            largest,
        })
    }
}

impl Backend for ZipBackend {
    fn kind(&self) -> &'static str {
        "zip"
    }

    fn entries(&self) -> &[EntryInfo] {
        &self.entries
    }

    fn peak_estimate(&self) -> u64 {
        self.total_compressed + self.largest
    }

    fn is_solid(&self) -> bool {
        false
    }

    fn needs_password(&self) -> bool {
        self.any_encrypted && self.password.is_none()
    }

    fn extract(
        &mut self,
        opts: &ExtractOptions,
        on: &mut dyn FnMut(Event),
    ) -> io::Result<Summary> {
        let punch_handle = if opts.molt {
            let h = OpenOptions::new().read(true).write(true).open(&self.path)?;
            punch::prepare(&h)
                .map_err(|e| invalid(format!("filesystem can't hole-punch here: {e}")))?;
            Some(h)
        } else {
            None
        };
        fs::create_dir_all(opts.out_dir)?;
        // Read-only handle for resume detection (works in --keep mode too).
        let check_handle = File::open(&self.path).ok();

        let mut buf = vec![0u8; 4 * 1024 * 1024];
        let mut freed = 0u64;
        let mut failed = 0usize;
        let mut resumed = 0usize;

        for i in 0..self.entries.len() {
            if !opts.is_selected(i) {
                continue;
            }
            let (zip_idx, data_start, compressed, encrypted) = self.meta[i];
            let info = &self.entries[i];

            // Hollowed by an earlier run → already extracted back then.
            if let Some(h) = &check_handle {
                if punch::hollowed(h, data_start, compressed).unwrap_or(false) {
                    resumed += 1;
                    on(Event::Resumed { index: i });
                    continue;
                }
            }
            on(Event::Started { index: i });

            let result = (|| -> io::Result<()> {
                let mut entry = match (encrypted, &self.password) {
                    (false, _) => {
                        self.zip.by_index(zip_idx).map_err(|e| invalid(e.to_string()))?
                    }
                    (true, Some(pw)) => self
                        .zip
                        .by_index_decrypt(zip_idx, pw)
                        .map_err(|e| invalid(e.to_string()))?
                        .map_err(|_| password_error("wrong password"))?,
                    (true, None) => {
                        return Err(password_error("entry is encrypted; password required"))
                    }
                };
                let dest = safe_join(opts.out_dir, entry.name()).ok_or_else(|| {
                    invalid(format!("unsafe path in archive: {}", entry.name()))
                })?;
                if info.is_dir {
                    fs::create_dir_all(&dest)?;
                } else {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let mut out = File::create(&dest)?;
                    // The zip reader validates the CRC32 as the stream is
                    // fully consumed; a mismatch errors HERE, before punching.
                    loop {
                        let n = entry.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        out.write_all(&buf[..n])?;
                    }
                    out.flush()?;
                }
                Ok(())
            })();

            match result {
                Ok(()) => {
                    if let Some(h) = &punch_handle {
                        if punch::punch_hole(h, data_start, compressed).is_ok() && compressed > 0
                        {
                            freed += compressed;
                            on(Event::Freed { bytes: compressed, indices: vec![i] });
                        }
                    }
                    on(Event::Done { index: i });
                }
                Err(e) => {
                    failed += 1;
                    on(Event::Error { index: i, message: e.to_string() });
                }
            }
        }

        let all_out =
            opts.molt && failed == 0 && opts.all_selected(self.entries.len());
        Ok(Summary { failed, resumed, freed, all_out })
    }
}
