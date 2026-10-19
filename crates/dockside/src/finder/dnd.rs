//! OLE drag and drop for Finder windows: dragging items out to other apps (or
//! other Finder windows), and accepting file drops onto folders.
//!
//! Both directions run modal loops or are called back from OLE, so nothing here
//! touches Finder state directly: drop feedback goes through `app::finder_drag`
//! and the file operation itself is queued with `app::finder_transfer`.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use windows::core::{implement, Result, HSTRING};
use windows::Win32::Foundation::{HWND, POINT, POINTL};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IDataObject, CLSCTX_INPROC_SERVER, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL,
};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop, CF_HDROP,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DragQueryFileW, IDropTargetHelper, IShellItemArray, SHCreateShellItemArrayFromIDLists, SHDoDragDrop,
    SHParseDisplayName, BHID_DataObject, CLSID_DragDropHelper, HDROP,
};

use super::fs::same_volume;

/// Where a drag is, in screen pixels.
#[derive(Clone, Copy, Debug)]
pub enum Phase {
    Over(i32, i32),
    Leave,
    Drop(i32, i32),
}

thread_local! {
    /// Items of the drag this thread started, so drops can tell they came from Finder.
    static OUTGOING: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    static OLE_READY: Cell<bool> = const { Cell::new(false) };
}

/// Drop targets need OLE (not just COM) on the UI thread.
pub fn ensure_ole() {
    if !OLE_READY.get() {
        if let Err(e) = unsafe { OleInitialize(None) } {
            log!("OleInitialize failed: {e}");
        }
        OLE_READY.set(true);
    }
}

/// Makes a Finder window accept file drops.
pub fn register(hwnd: HWND, id: u32) {
    ensure_ole();
    let helper: Option<IDropTargetHelper> = unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).ok() };
    let target: IDropTarget = DropTarget { id, hwnd, helper, paths: RefCell::new(Vec::new()) }.into();
    if let Err(e) = unsafe { RegisterDragDrop(hwnd, &target) } {
        log!("RegisterDragDrop failed: {e}");
    }
}

pub fn revoke(hwnd: HWND) {
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

/// Drags `paths` with the shell's drag image until they are dropped or the drag
/// is cancelled. Modal: call outside the App borrow. Returns true if they were dropped somewhere.
pub fn drag_out(hwnd: HWND, paths: &[PathBuf]) -> bool {
    let Some(data) = data_object(paths) else { return false };
    OUTGOING.with(|o| *o.borrow_mut() = paths.to_vec());
    let r = unsafe { SHDoDragDrop(hwnd, &data, None, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK) };
    OUTGOING.with(|o| o.borrow_mut().clear());
    match r {
        Ok(effect) => effect != DROPEFFECT_NONE,
        Err(e) => {
            log!("drag failed: {e}");
            false
        }
    }
}

/// The shell's own data object for the items (CF_HDROP, shell ID lists, drag image support).
fn data_object(paths: &[PathBuf]) -> Option<IDataObject> {
    unsafe {
        let mut pidls: Vec<*const ITEMIDLIST> = Vec::new();
        for p in paths {
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(&HSTRING::from(p.as_os_str()), None, &mut pidl, 0, None).is_ok() && !pidl.is_null() {
                pidls.push(pidl);
            }
        }
        if pidls.is_empty() {
            return None;
        }
        let array: Result<IShellItemArray> = SHCreateShellItemArrayFromIDLists(&pidls);
        for &pidl in &pidls {
            CoTaskMemFree(Some(pidl as *const _));
        }
        array.and_then(|a| a.BindToHandler(None, &BHID_DataObject)).map_err(|e| log!("drag data failed: {e}")).ok()
    }
}

/// File paths in a drop (CF_HDROP); empty for anything else.
pub fn hdrop_paths(data: &IDataObject) -> Vec<PathBuf> {
    let fmt = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    unsafe {
        let Ok(mut medium) = data.GetData(&fmt) else { return Vec::new() };
        let hdrop = HDROP(medium.u.hGlobal.0);
        let n = DragQueryFileW(hdrop, u32::MAX, None);
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            out.push(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
        }
        ReleaseStgMedium(&mut medium);
        out
    }
}

const COPY: u32 = DROPEFFECT_COPY.0;
const MOVE: u32 = DROPEFFECT_MOVE.0;

/// Drop effect bits for dropping `paths` into `dest`. Like macOS, items dragged
/// within Finder move on the same drive and copy across drives; drops from
/// other apps copy. Shift forces a move and Ctrl (⌥ on macOS) a copy.
fn decide(paths: &[PathBuf], dest: &Path, allowed: u32, shift: bool, ctrl: bool, internal: bool) -> u32 {
    if paths.is_empty() || paths.iter().any(|p| dest.starts_with(p)) {
        return 0;
    }
    let want_move = shift || (!ctrl && internal && paths.iter().all(|p| same_volume(p, dest)));
    // Moving items into the folder they're already in does nothing; a Ctrl-drag duplicates them.
    if paths.iter().all(|p| p.parent() == Some(dest)) && (want_move || !ctrl) {
        return 0;
    }
    let want = if want_move { MOVE } else { COPY };
    if allowed & want != 0 {
        want
    } else if !shift && !ctrl && allowed & (COPY | MOVE) != 0 {
        if allowed & COPY != 0 { COPY } else { MOVE }
    } else {
        0
    }
}

#[implement(IDropTarget)]
struct DropTarget {
    id: u32,
    hwnd: HWND,
    helper: Option<IDropTargetHelper>,
    /// Files in the drag currently over the window.
    paths: RefCell<Vec<PathBuf>>,
}

impl DropTarget {
    /// Updates Finder's highlight and `*effect`; returns the destination folder if the drop is allowed.
    fn update(&self, keys: MODIFIERKEYS_FLAGS, effect: *mut DROPEFFECT, phase: Phase) -> Option<PathBuf> {
        let paths = self.paths.borrow().clone();
        let dest = if paths.is_empty() { None } else { crate::app::finder_drag(self.id, phase, &paths) };
        let internal = OUTGOING.with(|o| !o.borrow().is_empty());
        let e = match (&dest, effect.is_null()) {
            (Some(dest), false) => {
                let allowed = unsafe { (*effect).0 };
                decide(&paths, dest, allowed, keys.0 & MK_SHIFT.0 != 0, keys.0 & MK_CONTROL.0 != 0, internal)
            }
            _ => 0,
        };
        if !effect.is_null() {
            unsafe { *effect = DROPEFFECT(e) };
        }
        dest.filter(|_| e != 0)
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(&self, data: Option<&IDataObject>, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        *self.paths.borrow_mut() = data.map(hdrop_paths).unwrap_or_default();
        self.update(keys, effect, Phase::Over(pt.x, pt.y));
        if let (Some(h), Some(d), false) = (&self.helper, data, effect.is_null()) {
            unsafe {
                let _ = h.DragEnter(self.hwnd, d, &POINT { x: pt.x, y: pt.y }, *effect);
            }
        }
        Ok(())
    }

    fn DragOver(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        self.update(keys, effect, Phase::Over(pt.x, pt.y));
        if let (Some(h), false) = (&self.helper, effect.is_null()) {
            unsafe {
                let _ = h.DragOver(&POINT { x: pt.x, y: pt.y }, *effect);
            }
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        if let Some(h) = &self.helper {
            unsafe {
                let _ = h.DragLeave();
            }
        }
        crate::app::finder_drag(self.id, Phase::Leave, &[]);
        self.paths.borrow_mut().clear();
        Ok(())
    }

    fn Drop(&self, data: Option<&IDataObject>, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let dest = self.update(keys, effect, Phase::Drop(pt.x, pt.y));
        if let (Some(h), Some(d), false) = (&self.helper, data, effect.is_null()) {
            unsafe {
                let _ = h.Drop(d, &POINT { x: pt.x, y: pt.y }, *effect);
            }
        }
        let paths = std::mem::take(&mut *self.paths.borrow_mut());
        if let (Some(dest), false) = (dest, effect.is_null()) {
            let copy = unsafe { *effect } == DROPEFFECT_COPY;
            // The copy runs after the drop returns so the source app isn't blocked.
            crate::app::finder_transfer(self.id, paths, dest, copy);
            if !copy {
                // We move the files ourselves (an "optimized move"): the source must not delete them.
                unsafe { *effect = DROPEFFECT_NONE };
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn internal_drags_move_on_same_drive() {
        let items = [p("C:\\a\\x.txt")];
        assert_eq!(decide(&items, Path::new("C:\\b"), COPY | MOVE, false, false, true), MOVE);
        assert_eq!(decide(&items, Path::new("D:\\b"), COPY | MOVE, false, false, true), COPY);
        assert_eq!(decide(&items, Path::new("C:\\b"), COPY | MOVE, false, true, true), COPY);
    }

    #[test]
    fn external_drops_copy_unless_shift() {
        let items = [p("C:\\a\\x.txt")];
        assert_eq!(decide(&items, Path::new("C:\\b"), COPY | MOVE, false, false, false), COPY);
        assert_eq!(decide(&items, Path::new("C:\\b"), COPY | MOVE, true, false, false), MOVE);
        // A source that only allows copying still gets a copy.
        assert_eq!(decide(&items, Path::new("C:\\b"), COPY, false, false, false), COPY);
        assert_eq!(decide(&items, Path::new("C:\\b"), COPY, true, false, false), 0);
    }

    #[test]
    fn refuses_drops_into_self_or_same_folder() {
        let items = [p("C:\\a\\dir")];
        assert_eq!(decide(&items, Path::new("C:\\a\\dir"), COPY | MOVE, false, false, true), 0);
        assert_eq!(decide(&items, Path::new("C:\\a\\dir\\sub"), COPY | MOVE, false, false, true), 0);
        assert_eq!(decide(&items, Path::new("C:\\a"), COPY | MOVE, false, false, true), 0);
        // Ctrl-dragging into the same folder duplicates.
        assert_eq!(decide(&items, Path::new("C:\\a"), COPY | MOVE, false, true, true), COPY);
    }
}
