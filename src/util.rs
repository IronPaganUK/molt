//! Small helpers shared by the CLI, the GUI, and the format backends.

use std::path::{Path, PathBuf};

/// Human-readable byte count ("123.4 MiB").
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

/// Prevent path traversal: resolve an entry name to a safe path inside `out_dir`.
/// Rejects absolute paths, drive prefixes, and `..` components.
pub fn safe_join(out_dir: &Path, name: &str) -> Option<PathBuf> {
    let mut p = out_dir.to_path_buf();
    for part in Path::new(name).components() {
        match part {
            std::path::Component::Normal(c) => p.push(c),
            std::path::Component::CurDir => {}
            _ => return None, // ParentDir, RootDir, Prefix => rejected
        }
    }
    Some(p)
}
