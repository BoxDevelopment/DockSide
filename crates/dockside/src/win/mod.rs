//! Thin Win32 platform layer.

pub mod accent;
pub mod appbar;
pub mod apps;
pub mod composition;
pub mod controls;
pub mod desktops;
pub mod elevation;
pub mod glass;
pub mod startup;
pub mod system;
pub mod taskbar;
pub mod tray;
pub mod wallpaper;

use windows::core::{w, Error, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const CLASS_NAME: PCWSTR = w!("DockSideSurface");

pub const WM_APP_FRAME: u32 = WM_APP + 1;
pub const WM_APP_RELOAD: u32 = WM_APP + 2;
pub const WM_APP_CLOSE_MENU: u32 = WM_APP + 3;
pub const WM_APP_MENU_KEY: u32 = WM_APP + 4;
pub const WM_APP_FOREGROUND: u32 = WM_APP + 5;
pub const WM_APP_WINDOWS_CHANGED: u32 = WM_APP + 6;
pub const WM_APP_APPBAR: u32 = WM_APP + 7;
/// Sent by a second launch to the running instance (wParam 1 = open Finder instead of Settings).
pub const WM_APP_OPEN_SETTINGS: u32 = WM_APP + 8;
/// Finder icon loader has results.
pub const WM_APP_ICONS: u32 = WM_APP + 9;
/// A Finder window's folder changed on disk (wParam = window id).
pub const WM_APP_FINDER_CHANGED: u32 = WM_APP + 10;

/// Asks an already-running instance to show its settings window.
pub fn signal_running_instance(finder: bool) -> bool {
    unsafe {
        let Ok(h) = FindWindowExW(HWND_MESSAGE, None, CLASS_NAME, None) else { return false };
        // An instance stuck exiting keeps its window, and Windows still answers
        // generic messages for it, so ask for a reply only DockSide gives.
        let mut result = 0usize;
        let sent = SendMessageTimeoutW(h, WM_APP_PING, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 1000, Some(&mut result));
        if sent.0 == 0 || result != PING_REPLY {
            return false;
        }
        PostMessageW(h, WM_APP_OPEN_SETTINGS, WPARAM(finder as usize), LPARAM(0)).is_ok()
    }
}

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
            // Resource 1 is the app icon embedded by build.rs (Alt-Tab, Settings, Finder).
            hIcon: LoadIconW(hinstance(), PCWSTR(1 as *const u16)).unwrap_or_default(),
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
    let hwnd = unsafe {
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
    }?;
    // Broadcasts from unelevated processes (Explorer restarting, settings
    // changes) would otherwise be dropped while we run elevated.
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    elevation::allow_messages(hwnd, &[taskbar_created, WM_SETTINGCHANGE]);
    Ok(hwnd)
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

/// Spotlight panel lost focus: close it.
pub const WM_APP_CLOSE_SPOTLIGHT: u32 = WM_APP + 11;
/// Spotlight's background index finished rebuilding.
pub const WM_APP_INDEX_READY: u32 = WM_APP + 12;
/// The media worker has a new Now Playing snapshot for the Dynamic Island.
pub const WM_APP_MEDIA: u32 = WM_APP + 13;
/// Dynamic Island background sources have signals queued.
pub const WM_APP_ISLAND: u32 = WM_APP + 14;
/// A volume key or Caps Lock was pressed (wParam = virtual key).
pub const WM_APP_ISLAND_KEY: u32 = WM_APP + 15;
/// A desktop shortcut was pressed (wParam = desktop number 1-9, or DESKTOP_LEFT / DESKTOP_RIGHT).
pub const WM_APP_DESKTOP_KEY: u32 = WM_APP + 16;
/// The current virtual desktop changed.
pub const WM_APP_DESKTOP: u32 = WM_APP + 17;
/// A relaunch checking the running instance is alive; answered with `PING_REPLY`.
pub const WM_APP_PING: u32 = WM_APP + 18;
pub const PING_REPLY: usize = 0x444F_434B; // "DOCK"
pub const WM_APP_UPDATE: u32 = WM_APP + 19;
/// A desktop widget source (weather, calendar, device batteries) has news.
pub const WM_APP_WIDGETS: u32 = WM_APP + 20;
/// The foreground window moved or resized (F11 fullscreen doesn't change focus).
pub const WM_APP_FG_MOVED: u32 = WM_APP + 21;
/// The icon picker's background workers finished something.
pub const WM_APP_ICON_PICKER: u32 = WM_APP + 22;
/// The icon picker lost focus: close it.
pub const WM_APP_CLOSE_PICKER: u32 = WM_APP + 23;
/// The elevated `--admin-task` helper finished (wParam bit 0: turned on, bit 1: succeeded).
pub const WM_APP_ADMIN_TASK: u32 = WM_APP + 24;
/// A Finder window's background listing or search has results (`wParam`: window id).
pub const WM_APP_FINDER_ASYNC: u32 = WM_APP + 25;
/// A key the Settings shortcut recorder captured (wParam: virtual key, plus 1 << 16 for a press).
pub const WM_APP_RECORD_KEY: u32 = WM_APP + 26;
/// The Spotlight modifier was tapped on its own (wParam 2: the second tap of a double tap).
pub const WM_APP_SPOTLIGHT_TAP: u32 = WM_APP + 27;
pub const DESKTOP_LEFT: usize = 100;
pub const DESKTOP_RIGHT: usize = 101;
