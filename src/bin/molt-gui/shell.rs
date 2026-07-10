//! Explorer integration (Windows): per-user registry entries that add
//! "Open with Molt" and "Molt here" to the right-click menu of supported
//! archive types, and list Molt in the "Open with" chooser. Everything
//! lives under HKCU\Software\Classes — no admin rights, no installer.
//!
//! Icon-safety note: we deliberately do NOT create `HKCU\Software\Classes\
//! .<ext>` keys. Doing so (as the old `OpenWithProgids` registration did,
//! ≤ 7.3.3) leaves an empty user-level extension key that shadows the
//! archive type's real icon, so Explorer shows a blank generic icon. The
//! context-menu verbs go under `SystemFileAssociations`, which is for
//! menus only and never affects an extension's icon or default program;
//! "Open with" discoverability goes under `Applications\molt-gui.exe`,
//! which also never touches the `.<ext>` keys. `unregister` (and a fresh
//! `register`) additionally repair the shadow keys left by older versions.
#![cfg(windows)]

use std::io;
use winreg::enums::{HKEY_CURRENT_USER, KEY_ALL_ACCESS};
use winreg::RegKey;

/// Extensions we claim (same set the file dialog offers).
const EXTS: &[&str] = &[
    "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "tzst",
];

/// ProgID created by ≤ 7.3.3 for the old OpenWithProgids scheme. No longer
/// created; still cleaned up so upgrades don't leave it behind.
const LEGACY_PROGID: &str = "Molt.Archive";

const APP_KEY: &str = r"Applications\molt-gui.exe";

fn classes() -> io::Result<RegKey> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\Classes", KEY_ALL_ACCESS)
}

/// Register the "Open with" app entry and the context-menu verbs.
pub fn register() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let exe_quoted = format!("\"{}\"", exe.display());
    let icon = format!("{},0", exe.display());
    let classes = classes()?;

    // Repair any icon-shadowing keys left by an older (≤ 7.3.3) install, so
    // re-registering in place fixes blank archive icons without needing a
    // separate unregister first.
    let _ = classes.delete_subkey_all(LEGACY_PROGID);
    for ext in EXTS {
        remove_legacy_ext_shadow(&classes, ext);
    }

    // "Open with" discoverability via an Applications entry — never touches
    // the .<ext> keys, so it cannot shadow an archive type's icon.
    let (app, _) = classes.create_subkey(APP_KEY)?;
    app.set_value("FriendlyAppName", &"Molt")?;
    app.create_subkey("DefaultIcon")?.0.set_value("", &icon)?;
    let (app_open, _) = app.create_subkey(r"shell\open")?;
    app_open.set_value("", &"Open with Molt")?;
    app_open
        .create_subkey("command")?
        .0
        .set_value("", &format!("{exe_quoted} \"%1\""))?;
    // Offer Molt prominently only for archive types.
    let (types, _) = app.create_subkey("SupportedTypes")?;
    for ext in EXTS {
        types.set_value(format!(".{ext}"), &"")?;
    }

    // Right-click verbs via SystemFileAssociations — the actual "Molt here"
    // feature. Menu-only namespace; does not affect icons or associations.
    for ext in EXTS {
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

/// Remove everything `register` created, plus any icon-shadowing keys left
/// by older versions. Missing keys are fine.
pub fn unregister() -> io::Result<()> {
    let classes = classes()?;

    let _ = classes.delete_subkey_all(APP_KEY);
    for ext in EXTS {
        let base = format!(r"SystemFileAssociations\.{ext}\shell");
        let _ = classes.delete_subkey_all(format!(r"{base}\Molt.Open"));
        let _ = classes.delete_subkey_all(format!(r"{base}\Molt.Here"));
    }

    // Legacy cleanup (≤ 7.3.3): restore archive icons broken by the old
    // OpenWithProgids scheme.
    let _ = classes.delete_subkey_all(LEGACY_PROGID);
    for ext in EXTS {
        remove_legacy_ext_shadow(&classes, ext);
    }

    notify_shell();
    Ok(())
}

/// Undo the old OpenWithProgids registration for one extension: drop our
/// ProgID from `.<ext>\OpenWithProgids`, then delete now-empty keys. Only
/// removes the `.<ext>` key if it holds nothing else — never clobbers a
/// real association or another app's entries.
fn remove_legacy_ext_shadow(classes: &RegKey, ext: &str) {
    let ext_key = format!(".{ext}");
    let owp = format!(r"{ext_key}\OpenWithProgids");

    if let Ok(k) = classes.open_subkey_with_flags(&owp, KEY_ALL_ACCESS) {
        let _ = k.delete_value(LEGACY_PROGID);
        let empty = k.enum_values().next().is_none() && k.enum_keys().next().is_none();
        drop(k);
        if empty {
            let _ = classes.delete_subkey(&owp);
        }
    }

    // If the extension key is now completely empty (no values incl. the
    // default, no subkeys), it's the shadow we created — remove it so the
    // real system icon resolves again.
    if let Ok(k) = classes.open_subkey_with_flags(&ext_key, KEY_ALL_ACCESS) {
        let empty = k.enum_values().next().is_none() && k.enum_keys().next().is_none();
        drop(k);
        if empty {
            let _ = classes.delete_subkey(&ext_key);
        }
    }
}

/// Tell Explorer the associations changed so menus and icons refresh.
fn notify_shell() {
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
}
