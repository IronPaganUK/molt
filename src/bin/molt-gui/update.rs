//! In-app self-updater (Windows GUI only).
//!
//! Checks the GitHub "latest release", and — if it's newer than the running
//! build — downloads the Windows exe, verifies its SHA-256 against the hash
//! published in the release notes (both fetched over HTTPS from GitHub), and
//! replaces the running executable in place. Replacing in place at the same
//! path means the Explorer right-click registration (which points at that
//! path) keeps working with no re-register needed.
#![cfg(windows)]

use std::io::Read;
use std::path::PathBuf;

const REPO: &str = "IronPaganUK/molt";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const USER_AGENT: &str = concat!("molt-updater/", env!("CARGO_PKG_VERSION"));

/// A newer release worth offering to the user.
#[derive(Clone)]
pub struct Update {
    pub version: String,
    pub exe_url: String,
    pub exe_sha256: String,
    pub notes_url: String,
}

/// Outcome of a check.
pub enum Check {
    /// A newer version is available.
    Available(Update),
    /// Already on the newest release; carries the current version.
    UpToDate(String),
}

pub fn current_version() -> &'static str {
    CURRENT
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

/// Query the latest release and decide whether it's newer than this build.
pub fn check() -> Result<Check, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = agent()
        .get(&url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("could not reach GitHub: {e}"))?
        .into_string()
        .map_err(|e| format!("could not read GitHub response: {e}"))?;

    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("unexpected GitHub response: {e}"))?;

    let tag = json["tag_name"].as_str().ok_or("release has no tag")?;
    let version = tag.trim_start_matches('v').to_string();
    let notes_url = json["html_url"].as_str().unwrap_or("").to_string();

    // Test hook: pretend to be an older build so the whole download/verify/
    // replace path can be exercised against a real release. Unset in normal
    // use; harmless if set (it still only ever installs the verified latest).
    let current = std::env::var("MOLT_UPDATE_FROM_VERSION").unwrap_or_else(|_| CURRENT.to_string());

    if !is_newer(&version, &current) {
        return Ok(Check::UpToDate(CURRENT.to_string()));
    }

    // Find the Windows exe asset.
    let assets = json["assets"].as_array().ok_or("release has no assets")?;
    let exe = assets
        .iter()
        .filter_map(|a| a["name"].as_str().zip(a["browser_download_url"].as_str()))
        .find(|(name, _)| name.ends_with("-windows-x86_64.exe"))
        .map(|(name, url)| (name.to_string(), url.to_string()))
        .ok_or("release has no Windows executable")?;

    let notes = json["body"].as_str().unwrap_or("");
    let exe_sha256 = sha256_for(notes, &exe.0)
        .ok_or("release notes don't list a SHA-256 for the Windows build")?;

    Ok(Check::Available(Update {
        version,
        exe_url: exe.1,
        exe_sha256,
        notes_url,
    }))
}

/// Compare `a` and `b` as dotted numeric versions ("8.1.1"). Returns true
/// when `a` is strictly newer than `b`. Unparseable parts sort as 0, so a
/// malformed tag never spuriously triggers an update.
fn is_newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.').map(|p| p.trim().parse().unwrap_or(0)).collect()
    };
    let (va, vb) = (parse(a), parse(b));
    for i in 0..va.len().max(vb.len()) {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// Pull the SHA-256 for `asset_name` out of a release-notes body. The notes
/// contain a fenced block of `<64-hex>  <filename>` lines.
fn sha256_for(notes: &str, asset_name: &str) -> Option<String> {
    for line in notes.lines() {
        if !line.contains(asset_name) {
            continue;
        }
        for tok in line.split_whitespace() {
            let t = tok.trim();
            if t.len() == 64 && t.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Some(t.to_ascii_lowercase());
            }
        }
    }
    None
}

/// Download the update, verify its hash, and replace the running exe. On
/// success the process should relaunch and exit (see [`relaunch`]).
pub fn download_and_apply(
    upd: &Update,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    use sha2::{Digest, Sha256};

    let resp = agent()
        .get(&upd.exe_url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;

    let total: Option<u64> = resp
        .header("Content-Length")
        .and_then(|s| s.parse().ok());

    // Stage the download next to the current exe so the final swap is a
    // same-volume rename (no cross-device copy).
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let staged = staged_path(&exe);
    let mut out = std::fs::File::create(&staged)
        .map_err(|e| format!("cannot write update file: {e}"))?;

    let mut reader = resp.into_reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut got: u64 = 0;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&staged);
                return Err(format!("download interrupted: {e}"));
            }
        };
        if let Err(e) = std::io::Write::write_all(&mut out, &buf[..n]) {
            let _ = std::fs::remove_file(&staged);
            return Err(format!("cannot write update file: {e}"));
        }
        hasher.update(&buf[..n]);
        got += n as u64;
        progress(got, total);
    }
    drop(out);

    let digest = hasher.finalize();
    let hex = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
    if hex != upd.exe_sha256 {
        let _ = std::fs::remove_file(&staged);
        return Err(format!(
            "downloaded file failed verification (expected {}…, got {}…) — update aborted",
            &upd.exe_sha256[..12],
            &hex[..12]
        ));
    }

    // Swap the running exe for the verified download (renames the old aside
    // and moves the new one into place; works while running).
    self_replace::self_replace(&staged)
        .map_err(|e| format!("could not replace the running program: {e}"))?;
    let _ = std::fs::remove_file(&staged);
    Ok(())
}

/// Launch the freshly-updated exe and exit this (old) process.
pub fn relaunch() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}

fn staged_path(exe: &std::path::Path) -> PathBuf {
    let mut p = exe.to_path_buf();
    let name = exe
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "molt-gui.exe".into());
    p.set_file_name(format!(".{name}.update"));
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("8.1.1", "7.3.6"));
        assert!(is_newer("7.3.7", "7.3.6"));
        assert!(is_newer("7.4.1", "7.3.9"));
        assert!(!is_newer("7.3.6", "7.3.6"));
        assert!(!is_newer("7.3.5", "7.3.6"));
        assert!(!is_newer("7.3.6", "8.1.1"));
        // malformed never triggers an update
        assert!(!is_newer("garbage", "7.3.6"));
    }

    #[test]
    fn parses_sha_from_notes() {
        let notes = "**SHA-256**\n```\n\
            e7de52ae287223f71199b00aac64474e979504e5fc105a424568907e1747b193  molt-7.3.6-linux-x86_64.tar.gz\n\
            f115906de6ffe4ab6b0c47fc1ce0d7a24287adcc31ed4fef1623d5b44fa9fab9  molt-7.3.6-windows-x86_64.exe\n```";
        let h = sha256_for(notes, "molt-7.3.6-windows-x86_64.exe").unwrap();
        assert_eq!(h, "f115906de6ffe4ab6b0c47fc1ce0d7a24287adcc31ed4fef1623d5b44fa9fab9");
        // linux line must not be mistaken for the windows one
        let l = sha256_for(notes, "molt-7.3.6-linux-x86_64.tar.gz").unwrap();
        assert_eq!(l, "e7de52ae287223f71199b00aac64474e979504e5fc105a424568907e1747b193");
        assert!(sha256_for(notes, "nonexistent.exe").is_none());
    }
}
