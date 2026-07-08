//! Drag files OUT of the Molt window into Explorer (Windows).
//!
//! The dragged entries are first extracted (non-destructively) to a temp
//! folder; a classic CF_HDROP data object then hands those paths to OLE
//! drag-and-drop, so Explorer performs an ordinary file copy on drop. This
//! is the same approach 7-Zip uses — virtual-file streaming would avoid
//! the temp copy but is far more machinery for the same user experience.
#![cfg(windows)]

use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use windows::core::implement;
use windows::Win32::Foundation::{
    BOOL, DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP,
    DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, E_NOTIMPL, HGLOBAL, OLE_E_ADVISENOTSUPPORTED,
    POINT, S_OK,
};
use windows::Win32::System::Com::{
    IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, DATADIR_GET, DVASPECT_CONTENT,
    FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_NONE, OleInitialize,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{SHCreateStdEnumFmtEtc, DROPFILES};

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn is_hdrop(f: &FORMATETC) -> bool {
    f.cfFormat == CF_HDROP.0
        && f.dwAspect == DVASPECT_CONTENT.0
        && (f.tymed & TYMED_HGLOBAL.0 as u32) != 0
}

/// Build the CF_HDROP payload: DROPFILES header + double-null-terminated
/// wide path list. A fresh HGLOBAL per call — the receiver frees it.
fn build_hdrop(paths: &[PathBuf]) -> windows::core::Result<HGLOBAL> {
    let mut wide: Vec<u16> = Vec::new();
    for p in paths {
        wide.extend(p.as_os_str().encode_wide());
        wide.push(0);
    }
    wide.push(0);

    let header = std::mem::size_of::<DROPFILES>();
    let size = header + wide.len() * 2;
    unsafe {
        let hg = GlobalAlloc(GMEM_MOVEABLE, size)?;
        let ptr = GlobalLock(hg) as *mut u8;
        std::ptr::write(
            ptr as *mut DROPFILES,
            DROPFILES {
                pFiles: header as u32,
                pt: POINT { x: 0, y: 0 },
                fNC: BOOL(0),
                fWide: BOOL(1),
            },
        );
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr.add(header) as *mut u16, wide.len());
        let _ = GlobalUnlock(hg);
        Ok(hg)
    }
}

#[implement(IDataObject)]
struct DataObject {
    paths: Vec<PathBuf>,
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let f = unsafe { &*pformatetcin };
        if !is_hdrop(f) {
            return Err(DV_E_FORMATETC.into());
        }
        let hglobal = build_hdrop(&self.paths)?;
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: hglobal },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> windows::core::HRESULT {
        if is_hdrop(unsafe { &*pformatetc }) {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> windows::core::HRESULT {
        unsafe { (*pformatetcout).ptd = std::ptr::null_mut() };
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection == DATADIR_GET.0 as u32 {
            unsafe { SHCreateStdEnumFmtEtc(&[hdrop_format()]) }
        } else {
            Err(E_NOTIMPL.into())
        }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Option<&windows::Win32::System::Com::IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(
        &self,
        fescapepressed: BOOL,
        grfkeystate: MODIFIERKEYS_FLAGS,
    ) -> windows::core::HRESULT {
        if fescapepressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if (grfkeystate & MK_LBUTTON) == MODIFIERKEYS_FLAGS(0) {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> windows::core::HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Run an OLE copy-drag with the given (already extracted) file paths.
/// Blocks until the user drops or cancels; returns true on a completed drop.
pub fn start_drag(paths: &[PathBuf]) -> bool {
    unsafe {
        // winit initializes OLE for its drop-target support; S_FALSE
        // ("already initialized") is fine, so the result is ignored.
        let _ = OleInitialize(None);
        let data: IDataObject = DataObject { paths: paths.to_vec() }.into();
        let source: IDropSource = DropSource.into();
        let mut effect = DROPEFFECT_NONE;
        let hr = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
        hr == DRAGDROP_S_DROP && (effect & DROPEFFECT_COPY) != DROPEFFECT_NONE
    }
}
