//! Registers surfaces as AppBars so maximized windows leave room for them.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Shell::*;

fn data(hwnd: HWND) -> APPBARDATA {
    APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: hwnd, ..Default::default() }
}

/// Reserves `thickness` physical pixels along `edge` (ABE_TOP / ABE_BOTTOM) of `monitor`.
pub fn register(hwnd: HWND, callback: u32, edge: u32, monitor: RECT, thickness: i32) {
    // Explorer sends the callback; it runs unelevated.
    super::elevation::allow_messages(hwnd, &[callback]);
    super::tray::explorer_first(|| unsafe {
        let mut abd = data(hwnd);
        abd.uCallbackMessage = callback;
        SHAppBarMessage(ABM_NEW, &mut abd);

        abd.uEdge = edge;
        abd.rc = monitor;
        SHAppBarMessage(ABM_QUERYPOS, &mut abd);
        if edge == ABE_TOP {
            abd.rc.bottom = abd.rc.top + thickness;
        } else {
            abd.rc.top = abd.rc.bottom - thickness;
        }
        SHAppBarMessage(ABM_SETPOS, &mut abd);
    });
}

pub fn remove(hwnd: HWND) {
    super::tray::explorer_first(|| unsafe {
        let mut abd = data(hwnd);
        SHAppBarMessage(ABM_REMOVE, &mut abd);
    });
}
