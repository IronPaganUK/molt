//! Molt — extract archives without doubling your disk usage.
//!
//! The archive sheds its skin as files emerge: entries are extracted in
//! on-disk order, verified where the format allows it, and the compressed
//! bytes they occupied are hole-punched away, freeing that space
//! immediately. Peak overhead is roughly one entry, not the whole payload.

use molt::formats::{self, Event};
use molt::util::human;

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::exit;

const VERSION: &str = molt::VERSION;

struct Options {
    archive: PathBuf,
    out_dir: PathBuf,
    keep: bool,    // extract normally, never touch the archive
    dry_run: bool, // report the space profile, extract nothing
    yes: bool,     // skip the destructive-operation confirmation
    password: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "molt {VERSION} — extract archives without doubling your disk usage

USAGE:
    molt <archive> [OPTIONS]

Formats: zip, 7z, rar, tar, tar.gz/tgz, tar.bz2, tar.xz, tar.zst,
         and bare .gz/.bz2/.xz/.zst files (detected by content).

OPTIONS:
    -o, --output <DIR>   Destination directory (default: archive name without extension)
    -k, --keep           Normal extraction; leave the archive untouched
    -n, --dry-run        Show the space profile without extracting
    -y, --yes            Don't ask for confirmation before consuming the archive
    -p, --password <PW>  Password for encrypted zip/7z/rar (prompted if omitted)
    -h, --help           Show this help

By default Molt CONSUMES the archive as it extracts: each entry is
verified where the format carries a checksum, then its compressed bytes
are freed from disk. When extraction finishes the archive file is
deleted. There is no undo."
    );
    exit(2);
}

fn parse_args() -> Options {
    let mut archive: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut password: Option<String> = None;
    let (mut keep, mut dry_run, mut yes) = (false, false, false);

    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => usage(),
            "-k" | "--keep" => keep = true,
            "-n" | "--dry-run" => dry_run = true,
            "-y" | "--yes" => yes = true,
            "-o" | "--output" => {
                out_dir = Some(PathBuf::from(args.next().unwrap_or_else(|| usage())))
            }
            "-p" | "--password" => password = Some(args.next().unwrap_or_else(|| usage())),
            _ if a.starts_with('-') => usage(),
            _ => {
                if archive.is_some() {
                    usage();
                }
                archive = Some(PathBuf::from(a));
            }
        }
    }

    let archive = archive.unwrap_or_else(|| usage());
    let out_dir = out_dir.unwrap_or_else(|| {
        // "game.tar.gz" → "game", "game.zip" → "game"
        let mut stem = archive.file_stem().map(PathBuf::from).unwrap_or_default();
        if stem.extension().is_some_and(|e| e.eq_ignore_ascii_case("tar")) {
            stem = stem.with_extension("");
        }
        archive.with_file_name(stem)
    });

    Options { archive, out_dir, keep, dry_run, yes, password }
}

fn main() {
    let opts = parse_args();
    if let Err(e) = run(&opts) {
        eprintln!("molt: error: {e}");
        exit(1);
    }
}

/// Ask for a password on the terminal (no echo). Errors out rather than
/// looping forever when stdin isn't interactive (e.g. scripted runs).
fn prompt_password(msg: &str) -> io::Result<String> {
    rpassword::prompt_password(msg).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("password required — pass it with --password ({e})"),
        )
    })
}

fn run(opts: &Options) -> io::Result<()> {
    let archive_size = fs::metadata(&opts.archive)?.len();
    let mut password = opts.password.clone();

    // With -y the run must never block on input: a missing password is a
    // hard error instead of a prompt.
    let may_prompt = !opts.yes;
    let mut backend = loop {
        match formats::open_with_password(&opts.archive, password.as_deref()) {
            Ok(b) => {
                // Listing worked, but the contents are encrypted.
                if b.needs_password() {
                    if !may_prompt {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "archive is encrypted — pass the password with --password",
                        ));
                    }
                    password = Some(prompt_password("Archive is encrypted. Password: ")?);
                    continue;
                }
                break b;
            }
            Err(e) if formats::is_password_error(&e) => {
                if !may_prompt {
                    return Err(e);
                }
                eprintln!("molt: {e}");
                password = Some(prompt_password("Password: ")?);
            }
            Err(e) => return Err(e),
        }
    };

    let entries = backend.entries();
    let count = entries.len();
    let file_count = entries.iter().filter(|e| !e.is_dir).count();
    let total_uncompressed = backend.total_uncompressed();

    println!("molt {VERSION}");
    println!(
        "archive     : {} ({}, {})",
        opts.archive.display(),
        backend.kind(),
        human(archive_size)
    );
    println!("entries     : {count}");
    println!("extracted   : {} total", human(total_uncompressed));
    println!(
        "peak needed : ~{}   (classic extraction: {})",
        human(backend.peak_estimate()),
        human(archive_size + total_uncompressed)
    );

    if opts.dry_run {
        println!("dry run — nothing extracted.");
        return Ok(());
    }

    if !opts.keep && !opts.yes {
        eprint!(
            "\nMolt will CONSUME this archive as it extracts (no undo).\n\
             Entries are verified before their bytes are freed, where the\n\
             format carries a checksum.\n\
             Proceed? [y/N] "
        );
        io::stderr().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            println!("aborted — archive untouched.");
            return Ok(());
        }
    }

    let started = std::time::Instant::now();
    let mut freed: u64 = 0;
    let mut done: usize = 0;
    let names: Vec<String> = backend.entries().iter().map(|e| e.name.clone()).collect();
    let dirs: Vec<bool> = backend.entries().iter().map(|e| e.is_dir).collect();

    let summary = backend.extract(
        &formats::ExtractOptions { out_dir: &opts.out_dir, selected: None, molt: !opts.keep },
        &mut |ev| match ev {
            Event::Started { .. } => {}
            Event::Done { index } => {
                if !dirs[index] {
                    done += 1;
                    println!(
                        "[{:>4}/{file_count}] {}  ({} freed)",
                        done.min(file_count),
                        names[index],
                        human(freed)
                    );
                }
            }
            Event::Resumed { index } => {
                if !dirs[index] {
                    done += 1;
                    println!(
                        "[{:>4}/{file_count}] {}  (already extracted in an earlier run)",
                        done.min(file_count),
                        names[index]
                    );
                }
            }
            Event::Freed { bytes, .. } => freed += bytes,
            Event::Error { index, message } => {
                eprintln!("molt: {}: {message}", names.get(index).map_or("?", |s| s));
            }
            Event::Note(msg) => eprintln!("molt: note: {msg}"),
        },
    )?;

    drop(backend);

    if summary.all_out {
        fs::remove_file(&opts.archive)?;
        println!(
            "\ndone in {:.1?} — {} extracted, {} freed, archive consumed{}.",
            started.elapsed(),
            human(total_uncompressed),
            human(summary.freed),
            if summary.resumed > 0 {
                format!(" ({} entries were already out from an earlier run)", summary.resumed)
            } else {
                String::new()
            }
        );
    } else if opts.keep {
        println!(
            "\ndone in {:.1?} — {} extracted, archive kept{}.",
            started.elapsed(),
            human(total_uncompressed),
            if summary.failed > 0 {
                format!(" ({} entries failed)", summary.failed)
            } else {
                String::new()
            }
        );
        if summary.failed > 0 {
            exit(1);
        }
    } else {
        println!(
            "\ndone in {:.1?} — {} freed so far{}.",
            started.elapsed(),
            human(summary.freed),
            if summary.failed > 0 {
                format!(", {} entr{} failed — archive kept",
                    summary.failed,
                    if summary.failed == 1 { "y" } else { "ies" })
            } else {
                String::from(", archive kept")
            }
        );
        if summary.failed > 0 {
            exit(1);
        }
    }

    Ok(())
}
