//! 7z backend. A 7z archive groups entries into *blocks* (solid blocks);
//! every entry in a block shares one compressed stream, so the punching
//! unit is the block: once all of a block's entries are extracted and
//! CRC-verified, the block's packed bytes are freed. Non-solid archives
//! use one block per file, which degrades to per-entry punching.

use super::{invalid, password_error, Backend, EntryInfo, Event, ExtractOptions, Summary};
use crate::punch;
use crate::util::safe_join;
use sevenz_rust2::{Archive, BlockDecoder, EncoderMethod, Password, SIGNATURE_HEADER_SIZE};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub struct SevenZBackend {
    path: PathBuf,
    archive: Archive,
    source: File,
    entries: Vec<EntryInfo>,
    password: Password,
    has_password: bool,
    encrypted: bool,
    packed_total: u64,
    largest_block: u64,
}

/// Outcome of streaming one entry out of a block decoder.
enum EntryOutcome {
    Ok,
    /// Destination write failed but the block stream was drained past the
    /// entry, so later entries in the block are still reachable.
    WriteFailed(String),
    /// The compressed stream itself failed — the rest of the block is lost.
    DecodeFailed(String),
}

impl SevenZBackend {
    pub fn open(path: &Path, password: Option<&str>) -> io::Result<Self> {
        let mut source = File::open(path)?;
        let pw = password.map(Password::from).unwrap_or_else(Password::empty);
        let archive = Archive::read(&mut source, &pw).map_err(|e| match e {
            sevenz_rust2::Error::PasswordRequired => {
                password_error("7z header is encrypted; password required")
            }
            sevenz_rust2::Error::MaybeBadPassword(_) => {
                password_error("could not read 7z header — wrong password?")
            }
            e => invalid(format!("not a readable 7z: {e}")),
        })?;
        // Encrypted content shows up as an AES coder in a block's chain.
        let encrypted = archive.blocks.iter().any(|b| {
            b.coders.iter().any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256)
        });

        let entries: Vec<EntryInfo> = archive
            .files
            .iter()
            .map(|f| EntryInfo {
                name: f.name().replace('\\', "/"),
                size: f.size(),
                is_dir: f.is_directory(),
            })
            .collect();

        let packed_total: u64 = archive.pack_sizes().iter().sum();
        let largest_block = archive
            .blocks
            .iter()
            .map(|b| b.get_unpack_size())
            .max()
            .unwrap_or(0);

        Ok(Self {
            path: path.to_path_buf(),
            archive,
            source,
            entries,
            password: pw,
            has_password: password.is_some(),
            encrypted,
            packed_total,
            largest_block,
        })
    }

    /// Absolute byte range of a block's packed streams inside the file.
    /// Pack streams of a block are laid out contiguously.
    fn block_pack_range(&self, block: usize) -> Option<(u64, u64)> {
        let sm = &self.archive.stream_map;
        let first_pack = sm.block_first_pack_stream_index();
        let offsets = sm.pack_stream_offsets();
        let sizes = self.archive.pack_sizes();
        let start = *first_pack.get(block)?;
        let end = first_pack.get(block + 1).copied().unwrap_or(sizes.len());
        if start >= end {
            return None;
        }
        let offset = SIGNATURE_HEADER_SIZE + self.archive.pack_pos() + offsets.get(start).copied()?;
        let len: u64 = sizes[start..end].iter().sum();
        Some((offset, len))
    }
}

fn stream_entry(reader: &mut dyn Read, dest: &Path, buf: &mut [u8]) -> EntryOutcome {
    let mut out = match dest.parent().map(fs::create_dir_all).transpose().and_then(|_| File::create(dest).map(Some)) {
        Ok(Some(f)) => f,
        _ => {
            // Could not create the destination — drain the entry so the
            // block stream stays aligned for the entries after it.
            let msg = format!("cannot create {}", dest.display());
            return match io::copy(reader, &mut io::sink()) {
                Ok(_) => EntryOutcome::WriteFailed(msg),
                Err(e) => EntryOutcome::DecodeFailed(format!("{msg}; stream error: {e}")),
            };
        }
    };
    loop {
        match reader.read(buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = out.write_all(&buf[..n]) {
                    let msg = format!("write failed: {e}");
                    return match io::copy(reader, &mut io::sink()) {
                        Ok(_) => EntryOutcome::WriteFailed(msg),
                        Err(e) => EntryOutcome::DecodeFailed(format!("{msg}; stream error: {e}")),
                    };
                }
            }
            // Includes the CRC32 check the reader performs on the final
            // read: a mismatch surfaces here, before any punching.
            Err(e) => return EntryOutcome::DecodeFailed(e.to_string()),
        }
    }
    if let Err(e) = out.flush() {
        return EntryOutcome::WriteFailed(format!("flush failed: {e}"));
    }
    EntryOutcome::Ok
}

impl Backend for SevenZBackend {
    fn kind(&self) -> &'static str {
        "7z"
    }

    fn entries(&self) -> &[EntryInfo] {
        &self.entries
    }

    fn peak_estimate(&self) -> u64 {
        self.packed_total + self.largest_block
    }

    fn is_solid(&self) -> bool {
        self.archive.is_solid
    }

    fn needs_password(&self) -> bool {
        self.encrypted && !self.has_password
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

        let password = self.password.clone();
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        let mut freed = 0u64;
        let mut failed = 0usize;
        let mut resumed = 0usize;
        let mut seen = vec![false; self.entries.len()];

        let block_count = self.archive.blocks.len();
        for b in 0..block_count {
            let start = self.archive.stream_map.block_first_file_index[b];
            let count = BlockDecoder::new(1, b, &self.archive, &password, &mut self.source)
                .entry_count();
            let range: Vec<usize> = (start..start + count).collect();
            range.iter().for_each(|&i| seen[i] = true);

            let any_selected = range.iter().any(|&i| opts.is_selected(i));
            if !any_selected {
                continue; // block never touched, stays intact in the archive
            }

            // Encrypted block without a password: fail its entries up front
            // with a clear message instead of a cryptic decode error.
            let block_encrypted = self.archive.blocks[b]
                .coders
                .iter()
                .any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256);
            if block_encrypted && !self.has_password {
                for &i in &range {
                    if opts.is_selected(i) {
                        failed += 1;
                        on(Event::Error {
                            index: i,
                            message: "entry is encrypted; password required".into(),
                        });
                    }
                }
                continue;
            }

            // Blocks are punched atomically, so a hollowed pack range means
            // every entry in this block came out in an earlier run.
            if let (Some(h), Some((off, len))) = (&check_handle, self.block_pack_range(b)) {
                if punch::hollowed(h, off, len).unwrap_or(false) {
                    for &i in &range {
                        if opts.is_selected(i) {
                            resumed += 1;
                            on(Event::Resumed { index: i });
                        }
                    }
                    continue;
                }
            }

            // Split borrows: the decoder needs &archive and &mut source.
            let decoder =
                BlockDecoder::new(1, b, &self.archive, &password, &mut self.source);

            let mut cursor = 0usize;
            let mut block_failed = false;
            let mut extracted_here: Vec<usize> = Vec::new();
            let out_dir = opts.out_dir;

            let walk = decoder.for_each_entries(&mut |entry, reader| {
                let index = range[cursor];
                cursor += 1;
                let selected = opts.is_selected(index);
                let is_dir = entry.is_directory();

                if !selected {
                    // Must still drain: entries in a block share one stream.
                    io::copy(reader, &mut io::sink()).map_err(sevenz_rust2::Error::from)?;
                    return Ok(true);
                }
                on(Event::Started { index });
                let dest = match safe_join(out_dir, &self.entries[index].name) {
                    Some(d) => d,
                    None => {
                        failed += 1;
                        on(Event::Error {
                            index,
                            message: format!("unsafe path in archive: {}", entry.name()),
                        });
                        io::copy(reader, &mut io::sink()).map_err(sevenz_rust2::Error::from)?;
                        return Ok(true);
                    }
                };
                if is_dir {
                    if let Err(e) = fs::create_dir_all(&dest) {
                        failed += 1;
                        on(Event::Error { index, message: e.to_string() });
                    } else {
                        extracted_here.push(index);
                        on(Event::Done { index });
                    }
                    return Ok(true);
                }
                match stream_entry(reader, &dest, &mut buf) {
                    EntryOutcome::Ok => {
                        extracted_here.push(index);
                        on(Event::Done { index });
                        Ok(true)
                    }
                    EntryOutcome::WriteFailed(msg) => {
                        failed += 1;
                        on(Event::Error { index, message: msg });
                        Ok(true)
                    }
                    EntryOutcome::DecodeFailed(msg) => {
                        failed += 1;
                        block_failed = true;
                        on(Event::Error { index, message: msg });
                        Ok(false) // abort this block; its stream is broken
                    }
                }
            });

            if let Err(e) = walk {
                // Decode-stack or drain failure outside a selected entry;
                // the loop below reports every selected entry not reached.
                block_failed = true;
                on(Event::Note(format!("7z block {b} failed: {e}")));
            }
            if block_failed {
                // Entries we never reached in this block are lost for this
                // run; report the selected ones so the UI can show it.
                for &i in range.iter().skip(cursor) {
                    if opts.is_selected(i) {
                        failed += 1;
                        on(Event::Error {
                            index: i,
                            message: "unreachable: earlier data in this solid block failed".into(),
                        });
                    }
                }
                continue;
            }

            // Punch the whole block only if every entry that lives in it
            // came out in this pass — otherwise a deselected entry would
            // lose its backing bytes.
            let complete = range.iter().all(|&i| opts.is_selected(i));
            if complete && !extracted_here.is_empty() {
                if let (Some(h), Some((off, len))) = (&punch_handle, self.block_pack_range(b)) {
                    if punch::punch_hole(h, off, len).is_ok() && len > 0 {
                        freed += len;
                        on(Event::Freed { bytes: len, indices: extracted_here });
                    }
                }
            } else if punch_handle.is_some() && !complete {
                on(Event::Note(format!(
                    "solid block {b}: kept (not all of its entries were selected)"
                )));
            }
        }

        // Entries that live in no block: directories and empty files.
        for (i, seen) in seen.iter().enumerate() {
            if *seen || !opts.is_selected(i) {
                continue;
            }
            let info = &self.entries[i];
            on(Event::Started { index: i });
            let Some(dest) = safe_join(opts.out_dir, &info.name) else {
                failed += 1;
                on(Event::Error { index: i, message: format!("unsafe path in archive: {}", info.name) });
                continue;
            };
            let r = if info.is_dir {
                fs::create_dir_all(&dest)
            } else {
                dest.parent().map(fs::create_dir_all).transpose().map(|_| ()).and_then(|_| File::create(&dest).map(|_| ()))
            };
            match r {
                Ok(()) => on(Event::Done { index: i }),
                Err(e) => {
                    failed += 1;
                    on(Event::Error { index: i, message: e.to_string() });
                }
            }
        }

        let all_out = opts.molt && failed == 0 && opts.all_selected(self.entries.len());
        Ok(Summary { failed, resumed, freed, all_out })
    }
}
