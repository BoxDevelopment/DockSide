//! The island's file shelf: an OLE drop target on the island window. Files
//! dropped on it are held (by path, nothing is copied) until dragged back out.

use std::cell::RefCell;
use std::path::PathBuf;

use windows::core::{implement, Result};
use windows::Win32::Foundation::{HWND, POINT, POINTL};
use windows::Win32::System::Com::{CoCreateInstance, IDataObject, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, RegisterDragDrop, RevokeDragDrop, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK,
    DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{CLSID_DragDropHelper, IDropTargetHelper};

use crate::finder::dnd::{ensure_ole, hdrop_paths, Phase};

pub fn register(hwnd: HWND) {
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
