//! The island's file shelf: an OLE drop target on the island window and its
//! drop zone. Files dropped on it are held (by path, nothing is copied) until
//! dragged back out.
//!
//! An elevated DockSide can't be an OLE drop target for normal apps (UIPI
//! blocks it), so it takes old-style WM_DROPFILES instead, and `ole_drag`
//! lets it notice a drag coming by watching the pointer (as the Dock does).

use std::cell::RefCell;
use std::path::PathBuf;

use windows::core::{implement, Result};
use windows::Win32::Foundation::{HWND, POINT, POINTL};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{CoCreateInstance, IDataObject, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, RegisterDragDrop, RevokeDragDrop, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK,
    DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{CLSID_DragDropHelper, DragAcceptFiles, DragFinish, DragQueryPoint, IDropTargetHelper, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{GetGUIThreadInfo, GUITHREADINFO, WM_COPYDATA, WM_DROPFILES};

use crate::finder::dnd::{ensure_ole, hdrop_paths, query_hdrop, Phase};
use crate::win::elevation;

/// WM_COPYGLOBALDATA: Explorer sends it along with WM_DROPFILES.
const WM_COPYGLOBALDATA: u32 = 0x0049;

pub fn register(hwnd: HWND) {
    if elevation::is_elevated() {
        // A registered OLE target would win over WM_DROPFILES, then fail to marshal.
        elevation::allow_messages(hwnd, &[WM_DROPFILES, WM_COPYDATA, WM_COPYGLOBALDATA]);
        unsafe { DragAcceptFiles(hwnd, true) };
        return;
    }
    ensure_ole();
    let helper: Option<IDropTargetHelper> = unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).ok() };
    let target: IDropTarget = ShelfTarget { hwnd, helper, paths: RefCell::new(Vec::new()) }.into();
    if let Err(e) = unsafe { RegisterDragDrop(hwnd, &target) } {
        log!("island RegisterDragDrop failed: {e}");
    }
}

pub fn revoke(hwnd: HWND) {
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

/// A WM_DROPFILES's files and where they landed, in screen pixels.
pub fn take_dropfiles(hwnd: HWND, hdrop: HDROP) -> (Vec<PathBuf>, POINT) {
    let paths = query_hdrop(hdrop);
    let mut pt = POINT::default();
    unsafe {
        let _ = DragQueryPoint(hdrop, &mut pt);
        let _ = ClientToScreen(hwnd, &mut pt);
        DragFinish(hdrop);
    }
    (paths, pt)
}

/// Whether a drag-and-drop is under way (from any app): a mouse button is held
/// and OLE's drag loop has the capture. Its hidden window (class
/// CLIPBRDWNDCLASS) holds it for the length of a drag, which tells a real drag
/// from a text selection or a window being moved.
pub fn ole_drag() -> bool {
    if !crate::win::system::mouse_button_down() {
        return false;
    }
    let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    unsafe { GetGUIThreadInfo(0, &mut info) }.is_ok()
        && !info.hwndCapture.is_invalid()
        && crate::win::class_name(info.hwndCapture) == "CLIPBRDWNDCLASS"
}

#[implement(IDropTarget)]
struct ShelfTarget {
    hwnd: HWND,
    helper: Option<IDropTargetHelper>,
    paths: RefCell<Vec<PathBuf>>,
}

impl ShelfTarget {
    /// We only keep references, so offer a copy (the source never deletes anything).
    fn effect(&self, effect: *mut DROPEFFECT, phase: Phase) {
        let paths = self.paths.borrow().clone();
        let ok = !paths.is_empty() && crate::app::island_drag(phase, &paths);
        if !effect.is_null() {
            unsafe {
                let allowed = (*effect).0;
                *effect = if !ok {
                    DROPEFFECT_NONE
                } else if allowed & DROPEFFECT_COPY.0 != 0 {
                    DROPEFFECT_COPY
                } else if allowed & DROPEFFECT_LINK.0 != 0 {
                    DROPEFFECT_LINK
                } else {
                    DROPEFFECT_NONE
                };
            }
        }
    }
}

impl IDropTarget_Impl for ShelfTarget_Impl {
    fn DragEnter(&self, data: Option<&IDataObject>, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        *self.paths.borrow_mut() = data.map(hdrop_paths).unwrap_or_default();
        self.effect(effect, Phase::Over(pt.x, pt.y));
        if let (Some(h), Some(d), false) = (&self.helper, data, effect.is_null()) {
            unsafe {
                let _ = h.DragEnter(self.hwnd, d, &POINT { x: pt.x, y: pt.y }, *effect);
            }
        }
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        self.effect(effect, Phase::Over(pt.x, pt.y));
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
        crate::app::island_drag(Phase::Leave, &[]);
        self.paths.borrow_mut().clear();
        Ok(())
    }

    fn Drop(&self, data: Option<&IDataObject>, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        self.effect(effect, Phase::Drop(pt.x, pt.y));
        if let (Some(h), Some(d), false) = (&self.helper, data, effect.is_null()) {
            unsafe {
                let _ = h.Drop(d, &POINT { x: pt.x, y: pt.y }, *effect);
            }
        }
        self.paths.borrow_mut().clear();
        Ok(())
    }
}
