//! Explorer integration (Windows): per-user registry entries that add
//! "Open with Molt" and "Molt here" to the right-click menu of supported
//! archive types, and list Molt in the "Open with" chooser. Everything
//! lives under HKCU\Software\Classes — no admin rights, no installer.
#![cfg(windows)]

use std::io;
use winreg::enums::{HKEY_CURRENT_USER, KEY_ALL_ACCESS};
use winreg::RegKey;

/// Extensions we claim (same set the file dialog offers).
const EXTS: &[&str] = &[
    "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "tzst",
];

const PROGID: &str = "Molt.Archive";

fn classes() -> io::Result<RegKey> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\Classes", KEY_ALL_ACCESS)
}

/// Register the ProgID, "Open with" entries, and context-menu verbs.
pub fn register() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let exe_quoted = format!("\"{}\"", exe.display());
    let icon = format!("{},0", exe.display());
    let classes = classes()?;

    // ProgID used by the "Open with" chooser.
    let (progid, _) = classes.create_subkey(PROGID)?;
    progid.set_value("", &"Molt archive")?;
    progid.create_subkey("DefaultIcon")?.0.set_value("", &icon)?;
    let (open, _) = progid.create_subkey(r"shell\open")?;
    open.set_value("", &"Open with Molt")?;
    open.create_subkey("command")?
        .0
        .set_value("", &format!("{exe_quoted} \"%1\""))?;

    for ext in EXTS {
        // "Open with" chooser entry — leaves the default handler alone.
        classes
            .create_subkey(format!(r".{ext}\OpenWithProgids"))?
            .0
            .set_value(PROGID, &"")?;

        // Context-menu verbs via SystemFileAssociations (also non-invasive).
        let base = format!(r"SystemFileAssociations\.{ext}\shell");
        let (v, _) = classes.create_subkey(format!(r"{base}\Molt.Open"))?;
        v.set_value("", &"Open with Molt")?;
        v.set_value("Icon", &icon)?;
        v.create_subkey("command")?
            .0
            .set_value("", &format!("{exe_quoted} \"%1\""))?;

        let (v, _) = classes.create_subkey(format!(r"{base}\Molt.Here"))?;
        v.set_value("", &"Molt here (extract && consume)")?;
        v.set_value("Icon", &icon)?;
        v.create_subkey("command")?
            .0
            .set_value("", &format!("{exe_quoted} --molt-here \"%1\""))?;
    }

    notify_shell();
    Ok(())
}

/// Remove everything `register` created. Missing keys are fine.
pub fn unregister() -> io::Result<()> {
    let classes = classes()?;
    let _ = classes.delete_subkey_all(PROGID);
    for ext in EXTS {
        if let Ok(k) = classes.open_subkey_with_flags(
            format!(r".{ext}\OpenWithProgids"),
            KEY_ALL_ACCESS,
        ) {
            let _ = k.delete_value(PROGID);
        }
        let base = format!(r"SystemFileAssociations\.{ext}\shell");
        let _ = classes.delete_subkey_all(format!(r"{base}\Molt.Open"));
        let _ = classes.delete_subkey_all(format!(r"{base}\Molt.Here"));
    }
    notify_shell();
    Ok(())
}

/// Tell Explorer the associations changed so menus refresh immediately.
fn notify_shell() {
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
}
