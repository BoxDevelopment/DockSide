//! The drop zone at the top of the screen. While the mouse button is held and
//! moving (any drag), an invisible window covers the top edge, so a file drag
//! only has to reach the top of the screen, not land on the small pill. It's a
//! drop target like the island itself; once a drag is over it, it grows to
//! cover the opened shelf too, so the drag can move on to a Quick Action.
//!
//! The window is layered at alpha 1/255: invisible, but still hit-tested
//! (Windows lets the pointer through pixels that are fully transparent).

use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, SetLayeredWindowAttributes, SetWindowPos, ShowWindow, HWND_TOPMOST, LWA_ALPHA, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_HIDE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use super::shelf;
use crate::win::create_window;

pub struct Catch {
    hwnd: HWND,
    /// Where it's shown, or `None` while hidden.
    rect: Option<RECT>,
}

impl Catch {
    pub fn new() -> Option<Self> {
        let ex = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_LAYERED;
        let hwnd = match create_window(ex, WS_POPUP, RECT::default(), HWND::default()) {
            Ok(h) => h,
            Err(e) => {
                log!("island drop zone: {e}");
                return None;
            }
        };
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA);
        }
        shelf::register(hwnd);
        Some(Self { hwnd, rect: None })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Where it's shown, in screen pixels (`None` while hidden).
    pub fn rect(&self) -> Option<RECT> {
        self.rect
    }

    /// Shows the zone over `rect` (screen pixels), above the island.
    pub fn show(&mut self, rect: RECT) {
        if self.rect == Some(rect) {
            return;
        }
        self.rect = Some(rect);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }

    pub fn hide(&mut self) {
        if self.rect.take().is_some() {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }
}

impl Drop for Catch {
    fn drop(&mut self) {
        shelf::revoke(self.hwnd);
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
