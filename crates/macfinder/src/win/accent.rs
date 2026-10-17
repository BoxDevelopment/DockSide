//! DWM backdrop effects (acrylic / blur) via the undocumented but stable
//! `SetWindowCompositionAttribute`, plus DWM attributes our surfaces want.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::core::{s, w};
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::config::Backdrop;
use crate::geom::Color;

#[repr(C)]
struct AccentPolicy {
    state: u32,
    flags: u32,
    gradient: u32,
    animation: u32,
}

#[repr(C)]
struct CompositionData {
    attribute: u32,
    data: *mut c_void,
    size: usize,
}

const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_DISABLED: u32 = 0;
const ACCENT_ENABLE_BLURBEHIND: u32 = 3;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

type SetWca = unsafe extern "system" fn(HWND, *mut CompositionData) -> BOOL;

fn set_wca() -> Option<SetWca> {
    static F: OnceLock<Option<usize>> = OnceLock::new();
    let addr = *F.get_or_init(|| unsafe {
        let user32 = GetModuleHandleW(w!("user32.dll")).ok()?;
        GetProcAddress(user32, s!("SetWindowCompositionAttribute")).map(|f| f as usize)
    });
    addr.map(|a| unsafe { std::mem::transmute::<usize, SetWca>(a) })
}

pub fn apply(hwnd: HWND, backdrop: Backdrop, tint: Color) {
    let state = match backdrop {
        Backdrop::Acrylic => ACCENT_ENABLE_ACRYLICBLURBEHIND,
        Backdrop::Blur => ACCENT_ENABLE_BLURBEHIND,
        Backdrop::None => ACCENT_DISABLED,
    };
    // Acrylic misbehaves with a fully transparent gradient.
    let tint = if tint.a < 0.01 { tint.alpha(0.01) } else { tint };
    let mut policy = AccentPolicy { state, flags: 2, gradient: tint.abgr(), animation: 0 };
    let mut data = CompositionData {
        attribute: WCA_ACCENT_POLICY,
        data: &mut policy as *mut _ as *mut c_void,
        size: std::mem::size_of::<AccentPolicy>(),
    };
    if let Some(f) = set_wca() {
        unsafe {
            let _ = f(hwnd, &mut data);
        }
    }
}

/// Native Windows 11 rounded corners for shaped backdrops.
pub fn round_corners(hwnd: HWND) {
    unsafe {
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            std::mem::size_of_val(&corner) as u32,
        );
    }
}

/// Square corners (we shape windows ourselves), no peek, no open/close animations.
pub fn prepare(hwnd: HWND) {
    unsafe {
        let corner = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            std::mem::size_of_val(&corner) as u32,
        );
        let yes = BOOL(1);
        for attr in [DWMWA_EXCLUDED_FROM_PEEK, DWMWA_TRANSITIONS_FORCEDISABLED] {
            let _ = DwmSetWindowAttribute(
                hwnd,
                attr,
                &yes as *const _ as *const c_void,
                std::mem::size_of::<BOOL>() as u32,
            );
        }
    }
}
