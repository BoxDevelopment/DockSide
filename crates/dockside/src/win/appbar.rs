//! Registers surfaces as AppBars so maximized windows leave room for them.

use std::cell::RefCell;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Shell::*;

struct Bar {
    hwnd: HWND,
    edge: u32,
    monitor: RECT,
    thickness: i32,
    /// The rect last handed to ABM_SETPOS.
    rc: RECT,
}

thread_local! {
    static BARS: RefCell<Vec<Bar>> = const { RefCell::new(Vec::new()) };
}

fn data(hwnd: HWND) -> APPBARDATA {
    APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: hwnd, ..Default::default() }
}

/// Where `bar` should sit given the other appbars (the taskbar among them) right now.
fn query(bar: &Bar) -> RECT {
    let mut abd = data(bar.hwnd);
    abd.uEdge = bar.edge;
    abd.rc = bar.monitor;
    unsafe { SHAppBarMessage(ABM_QUERYPOS, &mut abd) };
    if bar.edge == ABE_TOP {
        abd.rc.bottom = abd.rc.top + bar.thickness;
    } else {
        abd.rc.top = abd.rc.bottom - bar.thickness;
    }
    abd.rc
}

fn set_pos(bar: &Bar) {
    let mut abd = data(bar.hwnd);
    abd.uEdge = bar.edge;
    abd.rc = bar.rc;
    unsafe { SHAppBarMessage(ABM_SETPOS, &mut abd) };
}

/// Reserves `thickness` physical pixels along `edge` (ABE_TOP / ABE_BOTTOM) of `monitor`.
pub fn register(hwnd: HWND, callback: u32, edge: u32, monitor: RECT, thickness: i32) {
    // Explorer sends the callback; it runs unelevated.
    super::elevation::allow_messages(hwnd, &[callback]);
    let mut bar = Bar { hwnd, edge, monitor, thickness, rc: RECT::default() };
    super::tray::explorer_first(|| unsafe {
        let mut abd = data(hwnd);
        abd.uCallbackMessage = callback;
        SHAppBarMessage(ABM_NEW, &mut abd);
        bar.rc = query(&bar);
        set_pos(&bar);
    });
    BARS.with(|b| b.borrow_mut().push(bar));
}

/// Re-negotiates every bar's position. Call when another appbar moved or changed state
/// (ABN_POSCHANGED / ABN_STATECHANGE), e.g. the taskbar switching edge or auto-hiding:
/// a reservation made while the taskbar still held that edge would otherwise leave a gap.
pub fn refresh() {
    BARS.with(|b| {
        let mut bars = b.borrow_mut();
        if bars.is_empty() {
            return;
        }
        super::tray::explorer_first(|| {
            for bar in bars.iter_mut() {
                let rc = query(bar);
                // Only on change: each SETPOS notifies the other bars, which would refresh again.
                if rc != bar.rc {
                    bar.rc = rc;
                    set_pos(bar);
                }
            }
        });
    });
}

pub fn remove(hwnd: HWND) {
    BARS.with(|b| b.borrow_mut().retain(|bar| bar.hwnd != hwnd));
    super::tray::explorer_first(|| unsafe {
        let mut abd = data(hwnd);
        SHAppBarMessage(ABM_REMOVE, &mut abd);
    });
}
