//! Thin Win32 platform layer.

pub mod accent;
pub mod appbar;
pub mod apps;
pub mod system;
pub mod taskbar;

use windows::core::{w, Error, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const CLASS_NAME: PCWSTR = w!("MacFinderSurface");

pub const WM_APP_FRAME: u32 = WM_APP + 1;
pub const WM_APP_RELOAD: u32 = WM_APP + 2;
pub const WM_APP_CLOSE_MENU: u32 = WM_APP + 3;
pub const WM_APP_MENU_KEY: u32 = WM_APP + 4;
pub const WM_APP_FOREGROUND: u32 = WM_APP + 5;
pub const WM_APP_WINDOWS_CHANGED: u32 = WM_APP + 6;
pub const WM_APP_APPBAR: u32 = WM_APP + 7;

/// The monitor we draw on, in physical pixels, with its DPI scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Monitor {
    pub rect: RECT,
    pub scale: f32,
}

impl Monitor {
    pub fn primary() -> Monitor {
        use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
        use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
        unsafe {
            let hmon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(hmon, &mut mi);
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            Monitor { rect: mi.rcMonitor, scale: dx as f32 / 96.0 }
        }
    }

    pub fn width_pt(&self) -> f32 {
        (self.rect.right - self.rect.left) as f32 / self.scale
    }
}

pub fn hinstance() -> HINSTANCE {
    unsafe { GetModuleHandleW(None).map(HINSTANCE::from).unwrap_or_default() }
}

pub fn register_class(proc_: WNDPROC) -> Result<()> {
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: proc_,
            hInstance: hinstance(),
            lpszClassName: CLASS_NAME,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(Error::from_win32());
        }
    }
    Ok(())
}

pub fn create_window(
    ex: WINDOW_EX_STYLE,
    style: WINDOW_STYLE,
    rc: RECT,
    parent: HWND,
) -> Result<HWND> {
    unsafe {
        CreateWindowExW(
            ex,
            CLASS_NAME,
            w!(""),
            style,
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top,
            parent,
            None,
            hinstance(),
            None,
        )
    }
}

/// Posts a message to a window handle stored as an integer (for use from other threads).
pub fn post(hwnd: isize, msg: u32) {
    unsafe {
        let _ = PostMessageW(HWND(hwnd as _), msg, WPARAM(0), LPARAM(0));
    }
}

pub fn post_wp(hwnd: isize, msg: u32, wp: usize) {
    unsafe {
        let _ = PostMessageW(HWND(hwnd as _), msg, WPARAM(wp), LPARAM(0));
    }
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}
