//! Thin Win32 platform layer.

pub mod accent;
pub mod appbar;
pub mod apps;
pub mod composition;
pub mod startup;
pub mod system;
pub mod taskbar;

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
        match FindWindowExW(HWND_MESSAGE, None, CLASS_NAME, None) {
            Ok(h) => PostMessageW(h, WM_APP_OPEN_SETTINGS, WPARAM(finder as usize), LPARAM(0)).is_ok(),
            Err(_) => false,
        }
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

/// Parses "alt+space", "ctrl+shift+k", "win+f1", ... into RegisterHotKey arguments.
pub fn parse_hotkey(spec: &str) -> Option<(windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS, u32)> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let mut mods = HOT_KEY_MODIFIERS(0);
    let mut key = None;
    for part in spec.to_lowercase().split('+').map(str::trim) {
        match part {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" | "cmd" => mods |= MOD_WIN,
            "space" => key = Some(0x20),
            "enter" | "return" => key = Some(0x0D),
            "tab" => key = Some(0x09),
            p if p.len() == 1 && p.chars().all(|c| c.is_ascii_alphanumeric()) => {
                key = Some(p.to_ascii_uppercase().as_bytes()[0] as u32)
            }
            p if p.starts_with('f') && p[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) => {
                key = Some(0x70 + p[1..].parse::<u32>().unwrap() - 1)
            }
            _ => return None,
        }
    }
    key.map(|k| (mods, k))
}

#[cfg(test)]
mod tests {
    #[test]
    fn hotkeys() {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        assert_eq!(super::parse_hotkey("alt+space"), Some((MOD_ALT, 0x20)));
        assert_eq!(super::parse_hotkey("Ctrl+Alt+Space"), Some((MOD_CONTROL | MOD_ALT, 0x20)));
        assert_eq!(super::parse_hotkey("win+f1"), Some((MOD_WIN, 0x70)));
        assert_eq!(super::parse_hotkey("ctrl+k"), Some((MOD_CONTROL, b'K' as u32)));
        assert_eq!(super::parse_hotkey("alt+banana"), None);
        assert_eq!(super::parse_hotkey("alt"), None);
    }
}
