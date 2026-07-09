# Molt

**Extract archives without doubling your disk usage.**

Extracting a 100 GB game normally needs ~200 GB free: the archive *plus* the extracted files. Molt consumes the archive as it extracts — each file's compressed bytes are freed from disk the moment that file is safely out. Peak space needed drops from *archive + contents* to roughly *archive + one file*.

```
$ molt game.zip

molt 0.1.0
archive     : game.zip (492.8 MiB)
entries     : 5
extracted   : 648.4 MiB total
peak needed : ~654.9 MiB   (classic extraction: 1.1 GiB)

Molt will CONSUME this archive as it extracts (no undo).
Each entry is CRC-verified before its bytes are freed.
Proceed? [y/N] y

[   1/5] data/big1.txt  (123.2 MiB freed)
[   2/5] data/big3.txt  (246.4 MiB freed)
...
done in 5.2s — 648.4 MiB extracted, archive consumed.
```


![Molt](assets/molt_256.png)

## Download

From the [latest release](../../releases/latest):

- **Windows**: `molt-<version>-windows-x86_64.exe` — the Molt app as a single portable executable, no install needed. SmartScreen may warn on first run because the binary is unsigned; click **More info → Run anyway**.
- **Linux**: `molt-<version>-linux-x86_64.tar.gz` — the CLI.

(The Windows CLI isn't shipped as a release file — build it with `cargo build --release` if you want it.)

Releases are built automatically by GitHub Actions when a `v*` tag is pushed; the SHA-256 of each file is printed in the release notes.

### Checksums & VirusTotal

Verify a download: `Get-FileHash <file>` (Windows) or `sha256sum <file>` (Linux) and compare against the hash in the release notes. A file's VirusTotal report lives at `virustotal.com/gui/file/<sha-256>`.

<!-- Per release: copy each file's hash from the release notes into the
     SHA-256 column below, and put the same hash into the VirusTotal link.
     Then move the table you're replacing down into the <details> block,
     newest-first. -->

**v7.2.1**

| File | SHA-256 | VirusTotal |
|---|---|---|
| `molt-7.2.1-windows-x86_64.exe` | `PENDING` | [report](https://www.virustotal.com/gui/file/PENDING) |
| `molt-7.2.1-linux-x86_64.tar.gz` | `PENDING` | [report](https://www.virustotal.com/gui/file/PENDING) |

<details>
<summary><b>Older versions</b> (all clean — click to expand)</summary>

**v7.1.1**

| File | SHA-256 | VirusTotal |
|---|---|---|
| `molt-7.1.1-windows-x86_64.exe` | `86fd0cda37c0b820e9263278b8199a4f044003f69ba192b55273ace831c301e4` | [report](https://www.virustotal.com/gui/file/86fd0cda37c0b820e9263278b8199a4f044003f69ba192b55273ace831c301e4) |
| `molt-7.1.1-linux-x86_64.tar.gz` | `9c72008f16e5fae282c8c519c20a63090d0e3921c15f22361d4d07d2b4ac7012` | [report](https://www.virustotal.com/gui/file/9c72008f16e5fae282c8c519c20a63090d0e3921c15f22361d4d07d2b4ac7012) |

</details>

## Supported formats

| Format | Freed | Verified before freeing |
|---|---|---|
| **zip** | per entry | CRC32 per entry |
| **7z** (incl. solid) | per solid block, once all its entries are out | CRC32 per entry |
| **rar** (4 & 5, incl. solid) | per entry's packed range | CRC per entry (via unrar) |
| **tar**, **tar.gz/tgz**, **tar.bz2**, **tar.xz**, **tar.zst** | behind the read cursor as the stream is consumed | compression-layer checks only — tar has no per-file CRC |
| bare **.gz / .bz2 / .xz / .zst** | behind the read cursor | compression-layer checks only |

Formats are detected by content, not extension. Notes:

- **Solid archives** (solid rar, multi-entry 7z blocks, all tar streams) decompress front-to-back, which is exactly the access pattern hole punching wants — but partial extraction can't free space, because later entries need the earlier compressed bytes. Molt tells you when that's the case and extracts without freeing.
- **Encrypted archives are supported**: zip (ZipCrypto and AES), 7z (including encrypted headers), and rar. The CLI prompts for the password (or takes `--password`); the GUI shows a password dialog. Nothing is ever freed on a wrong password — entries fail verification before any punching. One limit: header-encrypted **rar** extracts fine but can't free space as it goes (the entry offsets can't be mapped), so the emptied archive is left for you to delete.
- **Multi-volume rar** archives are not supported.
- Listing a tar stream requires one decompression pass, so opening a huge `.tar.gz` takes a moment before extraction starts.

## How it works

Molt does **not** rewrite the archive (that would need extra space). Instead it uses filesystem *hole punching*: after an entry is extracted and its CRC32 verified, Molt tells the OS to deallocate the blocks backing that entry's compressed bytes inside the archive file.

- **Linux** (ext4, XFS, Btrfs, tmpfs): `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)`
- **Windows** (NTFS): `FSCTL_SET_SPARSE` + `FSCTL_SET_ZERO_DATA`

The archive's logical size never changes (so the zip reader keeps working), but its physical footprint shrinks entry by entry. When everything is out, the hollow shell is deleted.

Entries are extracted in **on-disk order** (not directory order), so freed space forms a contiguous wave behind the read cursor.

## Safety model

Molt is deliberately destructive — that's the point — but it never frees bytes it hasn't verified:

1. An entry is streamed out to disk in full.
2. The format's checksum must pass (a mismatch aborts *before* any punching of that entry) — CRC32 per entry for zip/7z/rar.
3. Only then is the entry's compressed range punched.

If extraction fails midway, everything already extracted is intact and verified; the remaining entries are still intact inside the (partially hollowed) archive, and Molt can be re-run on it. **Resume is automatic**: entries whose bytes were already punched out are recognized (their byte range in the archive is nothing but holes and zeros — no valid compressed data looks like that) and skipped, the remaining entries are extracted, and if that completes the job the hollow shell is deleted. Works for zip and rar (per entry) and 7z (per solid block); tar streams can't resume. What you *cannot* do is get back the archive as a shareable file. Use `--keep` for a normal non-destructive extraction.

Two format-specific caveats:

- **tar streams** (and bare .gz/.xz/…) carry no per-file checksum, so Molt frees bytes behind the read cursor as files are written out. Files already extracted are always intact, but a corrupt stream is only detected when the decompressor trips over it.
- **Solid archives** interrupted mid-run: entries after the failure point may be undecodable, because decoding them needs earlier (already punched) bytes. Molt only punches solid data when everything is being extracted in one pass.

**Not supported:** FAT32/exFAT (no sparse file support — common on USB sticks and SD cards). Molt's `prepare` step fails cleanly there.

## The app (molt-gui)

Molt ships as **one portable executable** — no installer, no runtime, nothing for the user to build. Double-click it (or right-click a zip → Open with Molt) and you get a 7-Zip-style window:

- Open an archive via the button, drag-and-drop onto the window, or "Open with…"
- Contents listed with sizes; tick what you want, or Select all
- **Molt mode** toggle: on = the archive is consumed as files come out; off = classic extraction
- **Extract & Free** with a clear "no undo" confirmation
- Live status per file (`extracting… / ✔ freed / ✔ extracted / ✖ failed`) and a running "X reclaimed" counter
- When the whole archive has been extracted and verified, the hollow shell is deleted
- **Drag a row into Explorer** to copy that file out without consuming anything
- **⚙ → Add to Explorer right-click menu** installs "Open with Molt" / "Molt here" for archives (equivalent to `molt-gui --register`; per-user, no admin needed)

Extraction runs on a background thread; each file is CRC-verified before its bytes are punched, same as the CLI.

Build it with `cargo build --release --features gui` → `molt-gui` (~6 MB). The plain `cargo build --release` gives the 460 KB CLI.

## Usage

```
molt <archive> [OPTIONS]

  -o, --output <DIR>   Destination directory (default: archive name)
  -k, --keep           Normal extraction; archive untouched
  -n, --dry-run        Show the space profile without extracting
  -y, --yes            Skip the confirmation prompt (never blocks on input)
  -p, --password <PW>  Password for encrypted zip/7z/rar (prompted if omitted)
```

## Building

```
cargo build --release                  # CLI (Linux/macOS/Windows)
cargo build --release --features gui   # + the GUI (Windows-focused)
cargo test --release                   # round-trip tests for the backends
```

Needs a C/C++ compiler for the bundled unrar, liblzma, and zstd (MSVC on Windows, gcc/g++ on Linux). CI builds and tests every push on Linux and Windows; pushing a `v*` tag builds and publishes release binaries automatically.

## Roadmap

- [x] **RAR / 7z support** — plus tar.gz/bz2/xz/zst and bare compressed files; solid archives decompress front-to-back, which is exactly the access pattern hole punching wants
- [x] **Intra-entry punching** — stream formats (tar.*, bare .gz/…) punch behind the read cursor as the stream is consumed
- [x] **Drag files OUT of the window** into Explorer — drag any row; the file is extracted to a temp copy and handed to OLE as a normal file drag (archive untouched)
- [x] **Resume support** — a partially hollowed archive is detected on re-run: already-freed entries are skipped, the rest are extracted, and the shell is deleted once everything is out (zip/rar per entry, 7z per block)
- [x] **Shell integration** — `molt-gui --register` (or the ⚙ menu) adds "Open with Molt" and "Molt here" to the Explorer right-click menu for all supported archive types; per-user registry only, `--unregister` removes it

## Versioning

`major.minor.hotfix` — e.g. **7.1.1**:

- **major**: big feature drops, redesigns
- **minor**: small features and tweaks
- **hotfix**: bug fixes only (starts at 1)

Release tags match `Cargo.toml`'s version exactly (`v7.1.1`).

## Name

An animal molts by shedding its shell as the new form emerges. Same thing here: the files emerge, the archive shell disappears behind them.

## License

MIT — see [LICENSE](LICENSE).
