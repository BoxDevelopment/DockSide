//! The Dock's OLE drop target. A drag from any app reveals a hidden Dock;
//! files dropped on an app's icon open in that app, on the Trash they're recycled.
//!
//! An elevated DockSide can't be an OLE drop target for normal apps (UIPI
//! blocks it), so it takes old-style WM_DROPFILES instead, and `foreign_drag`
//! lets it notice drags by watching the pointer.

use std::cell::RefCell;
use std::path::PathBuf;

use windows::core::{implement, Result};
use windows::Win32::Foundation::{HWND, POINT, POINTL};
use windows::Win32::System::Com::{CoCreateInstance, IDataObject, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, RegisterDragDrop, RevokeDragDrop, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK,
    DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::Shell::{CLSID_DragDropHelper, DragAcceptFiles, DragFinish, DragQueryPoint, IDropTargetHelper, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{GetGUIThreadInfo, GUITHREADINFO, GUI_INMOVESIZE, WM_COPYDATA, WM_DROPFILES};

use crate::dock::FileDrop;
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
    let target: IDropTarget = DockTarget { hwnd, helper, paths: RefCell::new(Vec::new()) }.into();
    if let Err(e) = unsafe { RegisterDragDrop(hwnd, &target) } {
        log!("dock RegisterDragDrop failed: {e}");
    }
}

pub fn revoke(hwnd: HWND) {
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

/// A WM_DROPFILES's files and where they landed, in client pixels.
pub fn take_dropfiles(hdrop: HDROP) -> (Vec<PathBuf>, POINT) {
    let paths = query_hdrop(hdrop);
    let mut pt = POINT::default();
    unsafe {
        let _ = DragQueryPoint(hdrop, &mut pt);
        DragFinish(hdrop);
    }
    (paths, pt)
}

/// Whether something is being dragged from another window: a mouse button is
/// held and a window other than `own` has the capture (OLE's drag loop takes
/// it), not counting a window being moved or sized.
pub fn foreign_drag(own: HWND) -> bool {
    unsafe {
        if GetAsyncKeyState(VK_LBUTTON.0 as i32) >= 0 && GetAsyncKeyState(VK_RBUTTON.0 as i32) >= 0 {
            return false;
        }
        let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        GetGUIThreadInfo(0, &mut info).is_ok()
            && !info.hwndCapture.is_invalid()
            && info.hwndCapture != own
            && !info.flags.contains(GUI_INMOVESIZE)
    }
}

#[implement(IDropTarget)]
struct DockTarget {
    hwnd: HWND,
    helper: Option<IDropTargetHelper>,
    paths: RefCell<Vec<PathBuf>>,
}

impl DockTarget {
    fn effect(&self, effect: *mut DROPEFFECT, phase: Phase) {
        let paths = self.paths.borrow().clone();
        let drop = crate::app::dock_drag(phase, &paths);
        if effect.is_null() {
            return;
        }
        unsafe {
            let allowed = (*effect).0;
            let pick = |want: &[DROPEFFECT]| want.iter().copied().find(|e| allowed & e.0 != 0).unwrap_or(DROPEFFECT_NONE);
            *effect = match (drop, phase) {
                (FileDrop::None, _) => DROPEFFECT_NONE,
                // We recycle the files ourselves: reporting a move would have the source delete them too.
                (FileDrop::Trash, Phase::Drop(..)) => DROPEFFECT_NONE,
                (FileDrop::Trash, _) => pick(&[DROPEFFECT_MOVE, DROPEFFECT_COPY]),
                (FileDrop::Open, _) => pick(&[DROPEFFECT_COPY, DROPEFFECT_LINK]),
            };
        }
    }
}

impl IDropTarget_Impl for DockTarget_Impl {
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
        crate::app::dock_drag(Phase::Leave, &[]);
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
