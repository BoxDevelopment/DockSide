//! The Dock's OLE drop target. A drag from any app reveals a hidden Dock;
//! files dropped on an app's icon open in that app, on the Trash they're recycled.

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
use windows::Win32::UI::Shell::{CLSID_DragDropHelper, IDropTargetHelper};

use crate::dock::FileDrop;
use crate::finder::dnd::{ensure_ole, hdrop_paths, Phase};

pub fn register(hwnd: HWND) {
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
