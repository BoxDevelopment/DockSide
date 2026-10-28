//! Discovering, naming, launching and controlling top-level application windows.

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::path::Path;

use windows::core::{w, Interface, HSTRING, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWM_CLOAKED_APP};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::class_name;

#[derive(Clone, Debug)]
pub struct AppWindow {
    pub hwnd: HWND,
    /// Full exe path as reported by the OS.
    pub exe: String,
    pub title: String,
}

impl AppWindow {
    /// Lower-cased exe file name, used for matching (e.g. `notepad.exe`).
    pub fn exe_name(&self) -> String {
        exe_name(&self.exe)
    }
}

pub fn exe_name(path: &str) -> String {
    Path::new(path).file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Visible, alt-tab-eligible windows in z-order (topmost first).
pub fn list_windows() -> Vec<AppWindow> {
    let mut hwnds: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut hwnds as *mut _ as isize));
    }
    let own = unsafe { GetCurrentProcessId() };
    hwnds
        .into_iter()
        .filter(|&h| is_app_window(h))
        .filter_map(|hwnd| {
            let pid = effective_pid(hwnd);
            if pid == own {
                return None;
            }
            Some(AppWindow { hwnd, exe: process_path(pid)?, title: window_title(hwnd) })
        })
        .collect()
}

unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> BOOL {
    let v = &mut *(lp.0 as *mut Vec<HWND>);
    v.push(hwnd);
    TRUE
}

fn is_app_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        if GetWindow(hwnd, GW_OWNER).map(|h| !h.is_invalid()).unwrap_or(false) {
            return false;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
            return false;
        }
        if ex & WS_EX_NOACTIVATE.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
            return false;
        }
        // The genie DLL cloaks a window while its animation stands in for it: still there.
        if is_cloaked(hwnd) && !(crate::genie::stands_in_for(hwnd) && cloak_flags(hwnd) == DWM_CLOAKED_APP) {
            return false;
        }
        if GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }
    }
    !matches!(
        class_name(hwnd).as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

fn is_cloaked(hwnd: HWND) -> bool {
    cloak_flags(hwnd) != 0
}

/// DWM_CLOAKED_* (0: not cloaked).
fn cloak_flags(hwnd: HWND) -> u32 {
    let mut cloaked: u32 = 0;
    let ok = unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut c_void, std::mem::size_of::<u32>() as u32)
    };
    if ok.is_ok() {
        cloaked
    } else {
        0
    }
}

pub fn is_desktop(hwnd: HWND) -> bool {
    matches!(class_name(hwnd).as_str(), "Progman" | "WorkerW")
}

/// Whether `hwnd` is an app filling `monitor` (a game, a fullscreen video, F11).
/// Windows' own fullscreen report also fires for live wallpapers and desktop
/// overlays (Wallpaper Engine, desktop pets), which are the desktop or are
/// layered/click-through windows drawn over it.
pub fn is_fullscreen_app(hwnd: HWND, monitor: RECT) -> bool {
    if hwnd.is_invalid() || is_desktop(hwnd) || is_cloaked(hwnd) {
        return false;
    }
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0) != 0 {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() {
            return false;
        }
        let covers = |r: &RECT| r.left <= monitor.left && r.top <= monitor.top && r.right >= monitor.right && r.bottom >= monitor.bottom;
        if !covers(&r) {
            return false;
        }
        // A maximized window's frame overhangs the screen too, but its caption
        // keeps the client area off the top edge; a fullscreen app's doesn't.
        let mut c = RECT::default();
        if GetClientRect(hwnd, &mut c).is_err() {
            return false;
        }
        let mut corners = [POINT { x: c.left, y: c.top }, POINT { x: c.right, y: c.bottom }];
        let _ = windows::Win32::Graphics::Gdi::MapWindowPoints(hwnd, HWND::default(), &mut corners);
        covers(&RECT { left: corners[0].x, top: corners[0].y, right: corners[1].x, bottom: corners[1].y })
    }
}

pub fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

/// Store apps live in ApplicationFrameHost; report the hosted app's process instead.
pub fn effective_pid(hwnd: HWND) -> u32 {
    let pid = window_pid(hwnd);
    if class_name(hwnd) != "ApplicationFrameWindow" {
        return pid;
    }
    let mut found = (pid, pid);
    unsafe {
        let _ = EnumChildWindows(hwnd, Some(find_core_child), LPARAM(&mut found as *mut _ as isize));
    }
    found.1
}

unsafe extern "system" fn find_core_child(child: HWND, lp: LPARAM) -> BOOL {
    let (frame_pid, out) = &mut *(lp.0 as *mut (u32, u32));
    let pid = window_pid(child);
    if pid != *frame_pid {
        *out = pid;
        return FALSE;
    }
    TRUE
}

pub fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        ok.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

pub fn activate(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            syscommand(hwnd, SC_RESTORE, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd).as_bool() {
            return;
        }
        // Foreground lock fallback: borrow the current foreground thread's input state.
        let fg = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(fg, None);
        let me = GetCurrentThreadId();
        let _ = AttachThreadInput(me, fg_thread, true);
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
        let _ = AttachThreadInput(me, fg_thread, false);
    }
}

/// For one of our windows about to fade away: if it has the focus, hands it to
/// the next app window below (or the desktop), as Windows would on destroy, so
/// typing doesn't go into a window that's on its way out.
pub fn pass_focus_from(hwnd: HWND) {
    unsafe {
        if GetForegroundWindow() != hwnd {
            return;
        }
        let mut next = GetWindow(hwnd, GW_HWNDNEXT).unwrap_or_default();
        while !next.is_invalid() {
            if is_app_window(next) && !IsIconic(next).as_bool() {
                let _ = SetForegroundWindow(next);
                return;
            }
            next = GetWindow(next, GW_HWNDNEXT).unwrap_or_default();
        }
        let _ = SetForegroundWindow(GetShellWindow());
    }
}

pub fn minimize(hwnd: HWND) {
    syscommand(hwnd, SC_MINIMIZE, SW_MINIMIZE);
}

/// Minimizes / restores the way the window's own title bar buttons do: the app
/// handles the command itself, so the genie DLL inside it animates the window (a
/// `ShowWindow` from here never runs through the app). A hung app would never get
/// to the command, so it is shown the plain way.
fn syscommand(hwnd: HWND, command: u32, fallback: SHOW_WINDOW_CMD) {
    unsafe {
        if IsHungAppWindow(hwnd).as_bool() || PostMessageW(hwnd, WM_SYSCOMMAND, WPARAM(command as usize), LPARAM(0)).is_err() {
            let _ = ShowWindow(hwnd, fallback);
        }
    }
}

pub fn toggle_zoom(hwnd: HWND) {
    unsafe {
        let cmd = if IsZoomed(hwnd).as_bool() { SW_RESTORE } else { SW_MAXIMIZE };
        let _ = ShowWindow(hwnd, cmd);
    }
}

pub fn is_minimized(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd).as_bool() }
}

pub fn close(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

pub fn launch(path: &str, args: Option<&str>) -> bool {
    let ok = super::elevation::shell_open(path, args, super::system::start_dir(path).as_deref());
    if !ok {
        log!("launch failed for {path}");
    }
    ok
}

/// Target path of a .lnk shortcut.
pub fn resolve_lnk(path: &str) -> Option<String> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        file.Load(&HSTRING::from(path), STGM_READ).ok()?;
        let mut buf = [0u16; 1024];
        link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()?;
        let s = super::from_wide(&buf);
        (!s.is_empty()).then_some(s)
    }
}

/// The exe's FileDescription version resource, e.g. "Google Chrome".
pub fn file_description(path: &str) -> Option<String> {
    unsafe {
        let hpath = HSTRING::from(path);
        let size = GetFileVersionInfoSizeW(&hpath, None);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        GetFileVersionInfoW(&hpath, 0, size, buf.as_mut_ptr() as *mut c_void).ok()?;

        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(buf.as_ptr() as *const c_void, w!("\\VarFileInfo\\Translation"), &mut ptr, &mut len)
            .as_bool()
            || len < 4
        {
            return None;
        }
        let lang = *(ptr as *const u16);
        let cp = *(ptr as *const u16).add(1);
        let key = HSTRING::from(format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\FileDescription"));
        if !VerQueryValueW(buf.as_ptr() as *const c_void, &key, &mut ptr, &mut len).as_bool() || len == 0 {
            return None;
        }
        let chars = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        let s = super::from_wide(chars).trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// Caches display names for exes: config override > FileDescription > file stem.
#[derive(Default)]
pub struct AppNames {
    cache: HashMap<String, String>,
}

impl AppNames {
    pub fn name_for(&mut self, exe: &str, overrides: &BTreeMap<String, String>) -> String {
        let file = exe_name(exe);
        if let Some(n) = overrides.iter().find(|(k, _)| k.to_lowercase() == file).map(|(_, v)| v) {
            return n.clone();
        }
        self.cache
            .entry(exe.to_string())
            .or_insert_with(|| {
                file_description(exe).unwrap_or_else(|| {
                    Path::new(exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                })
            })
            .clone()
    }
}
