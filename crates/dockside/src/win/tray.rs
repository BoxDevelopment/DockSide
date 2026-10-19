//! A notification-area ("system tray") host, so the menu bar can list every
//! app's tray icon.
//!
//! Windows has no API to read other apps' tray icons, so we do what RetroBar
//! and Cairo do: register a window of class `Shell_TrayWnd` above Explorer's.
//! `Shell_NotifyIcon` (and `SHAppBarMessage`) find the tray with
//! `FindWindow("Shell_TrayWnd")` and send it `WM_COPYDATA`, so those messages
//! reach us first; we record the icons and forward every message to
//! Explorer's tray unchanged, which keeps the real tray (and appbars) working.
//! Broadcasting "TaskbarCreated" makes running apps re-add their icons.

use std::cell::RefCell;

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::icons::IconPixels;

const CLASS: PCWSTR = w!("Shell_TrayWnd");
/// `SHELLTRAYDATA` header before the 32-bit `NOTIFYICONDATA`.
const TRAY_SIGNATURE: u32 = 0x3475_3423;

const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIM_SETVERSION: u32 = 4;
const NIF_MESSAGE: u32 = 0x1;
const NIF_ICON: u32 = 0x2;
const NIF_TIP: u32 = 0x4;
const NIF_STATE: u32 = 0x8;
const NIF_GUID: u32 = 0x20;
const NIS_HIDDEN: u32 = 0x1;
const NOTIFYICON_VERSION_4: u32 = 4;
const NIN_SELECT: u32 = WM_USER;

/// Byte offsets inside the 32-bit `NOTIFYICONDATAW` that follows the header.
mod off {
    pub const HWND: usize = 4;
    pub const UID: usize = 8;
    pub const FLAGS: usize = 12;
    pub const CALLBACK: usize = 16;
    pub const ICON: usize = 20;
    pub const TIP: usize = 24; // WCHAR[128]
    pub const STATE: usize = 280;
    pub const STATE_MASK: usize = 284;
    pub const VERSION: usize = 800;
    pub const GUID: usize = 936;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Left,
    Right,
}

/// Identifies one icon: by GUID when the app gave one, else by (window, id).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconId {
    Guid(u128),
    Window(isize, u32),
}

pub struct TrayIcon {
    pub id: IconId,
    hwnd: HWND,
    uid: u32,
    callback: u32,
    version: u32,
    icon: Option<HICON>,
    pub tip: String,
    hidden: bool,
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        if let Some(h) = self.icon.take() {
            unsafe {
                let _ = DestroyIcon(h);
            }
        }
    }
}

struct Host {
    hwnd: HWND,
    icons: Vec<TrayIcon>,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// Explorer's own tray window (never ours).
pub fn explorer_tray() -> Option<HWND> {
    let me = unsafe { GetCurrentProcessId() };
    let mut after = HWND::default();
    unsafe {
        while let Ok(h) = FindWindowExW(None, after, CLASS, None) {
            let mut pid = 0;
            GetWindowThreadProcessId(h, Some(&mut pid));
            if pid != me {
                return Some(h);
            }
            after = h;
        }
    }
    None
}

/// Starts hosting tray icons (idempotent).
pub fn start() -> Result<()> {
    if HOST.with(|h| h.borrow().is_some()) {
        return Ok(());
    }
    unsafe {
        let instance = super::hinstance();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(tray_proc),
            hInstance: instance,
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            CLASS,
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )?;
        HOST.with(|h| *h.borrow_mut() = Some(Host { hwnd, icons: Vec::new() }));
        keep_on_top();
        // Running apps re-add their icons, which now come to us first.
        let created = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = SendNotifyMessageW(HWND_BROADCAST, created, WPARAM(0), LPARAM(0));
    }
    Ok(())
}

/// Stops hosting; apps keep talking to Explorer's tray.
pub fn stop() {
    if let Some(host) = HOST.with(|h| h.borrow_mut().take()) {
        unsafe {
            let _ = DestroyWindow(host.hwnd);
        }
    }
}

/// `FindWindow` returns the first match in z-order: stay above Explorer's tray.
pub fn keep_on_top() {
    let Some(hwnd) = HOST.with(|h| h.borrow().as_ref().map(|h| h.hwnd)) else { return };
    unsafe {
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// Visible icons, in the order they were added: (id, tooltip, pixels).
pub fn icons(size: i32) -> Vec<(IconId, String, Option<IconPixels>)> {
    HOST.with(|h| {
        let mut h = h.borrow_mut();
        let Some(host) = h.as_mut() else { return Vec::new() };
        // Apps that crashed never sent NIM_DELETE.
        host.icons.retain(|i| unsafe { IsWindow(i.hwnd).as_bool() });
        host.icons
            .iter()
            .filter(|i| !i.hidden && i.icon.is_some())
            .map(|i| {
                let mut tip = i.tip.lines().next().unwrap_or("").trim().to_string();
                if tip.is_empty() {
                    // No tooltip: name it after the app's exe.
                    let mut pid = 0;
                    unsafe { GetWindowThreadProcessId(i.hwnd, Some(&mut pid)) };
                    tip = super::apps::process_path(pid)
                        .and_then(|p| std::path::Path::new(&p).file_stem().map(|s| s.to_string_lossy().into_owned()))
                        .unwrap_or_else(|| "Untitled".into());
                }
                (i.id, tip, i.icon.and_then(|h| icon_pixels(h, size)))
            })
            .collect()
    })
}

/// Clicks an icon the way the taskbar would: the app gets the mouse messages
/// (and, for version-4 icons, NIN_SELECT / WM_CONTEXTMENU) at the cursor.
pub fn click(id: IconId, button: Button) {
    let target = HOST.with(|h| {
        let h = h.borrow();
        let icon = h.as_ref()?.icons.iter().find(|i| i.id == id)?;
        Some((icon.hwnd, icon.uid, icon.callback, icon.version))
    });
    let Some((hwnd, uid, callback, version)) = target else { return };
    if callback == 0 {
        return;
    }
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        // Let the app bring its window or menu to the front.
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let msgs: &[u32] = match (button, version >= NOTIFYICON_VERSION_4) {
            (Button::Left, true) => &[WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT],
            (Button::Left, false) => &[WM_LBUTTONDOWN, WM_LBUTTONUP],
            (Button::Right, true) => &[WM_RBUTTONDOWN, WM_RBUTTONUP, WM_CONTEXTMENU],
            (Button::Right, false) => &[WM_RBUTTONDOWN, WM_RBUTTONUP],
        };
        for &m in msgs {
            let (wp, lp) = if version >= NOTIFYICON_VERSION_4 {
                let anchor = ((pt.y as u16 as usize) << 16) | pt.x as u16 as usize;
                (WPARAM(anchor), LPARAM((((uid & 0xFFFF) as isize) << 16) | m as isize))
            } else {
                (WPARAM(uid as usize), LPARAM(m as isize))
            };
            let _ = PostMessageW(hwnd, callback, wp, lp);
        }
    }
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COPYDATA => {
            let cds = &*(lp.0 as *const COPYDATASTRUCT);
            if cds.dwData == 1 && !cds.lpData.is_null() {
                let data = std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize);
                record(data);
            }
            // Explorer's tray answers: icon adds succeed, appbar queries get real results.
            LRESULT(forward(msg, wp, lp).unwrap_or((cds.dwData == 1) as isize))
        }
        // Anything else apps send the tray (WM_COMMAND, private messages) goes to Explorer too.
        WM_COMMAND | WM_USER..=0x7FFF => forward(msg, wp, lp).map(LRESULT).unwrap_or(LRESULT(0)),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn forward(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<isize> {
    let explorer = explorer_tray()?;
    let mut result = 0usize;
    unsafe {
        let ok = SendMessageTimeoutW(explorer, msg, wp, lp, SMTO_ABORTIFHUNG, 3000, Some(&mut result));
        (ok.0 != 0).then_some(result as isize)
    }
}

fn read_u32(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

/// Applies one `Shell_NotifyIcon` call to our icon list.
fn record(data: &[u8]) {
    if read_u32(data, 0) != Some(TRAY_SIGNATURE) {
        return;
    }
    let Some(message) = read_u32(data, 4) else { return };
    let nid = &data[8..];
    let field = |at: usize| read_u32(nid, at);
    let (Some(hwnd), Some(uid), Some(flags)) = (field(off::HWND), field(off::UID), field(off::FLAGS)) else { return };
    let hwnd = HWND(hwnd as usize as *mut _);
    let guid = (flags & NIF_GUID != 0)
        .then(|| nid.get(off::GUID..off::GUID + 16))
        .flatten()
        .map(|b| u128::from_le_bytes(b.try_into().unwrap()))
        .filter(|g| *g != 0);
    let id = match guid {
        Some(g) => IconId::Guid(g),
        None => IconId::Window(hwnd.0 as isize, uid),
    };

    HOST.with(|h| {
        let mut h = h.borrow_mut();
        let Some(host) = h.as_mut() else { return };
        let pos = host.icons.iter().position(|i| i.id == id);
        match message {
            NIM_DELETE => {
                if let Some(p) = pos {
                    host.icons.remove(p);
                }
                return;
            }
            NIM_ADD if pos.is_none() => host.icons.push(TrayIcon {
                id,
                hwnd,
                uid,
                callback: 0,
                version: 0,
                icon: None,
                tip: String::new(),
                hidden: false,
            }),
            NIM_ADD | NIM_MODIFY | NIM_SETVERSION => {}
            _ => return,
        }
        let Some(icon) = host.icons.iter_mut().find(|i| i.id == id) else { return };
        icon.hwnd = hwnd;
        icon.uid = uid;
        if message == NIM_SETVERSION {
            icon.version = field(off::VERSION).unwrap_or(0);
            return;
        }
        if flags & NIF_MESSAGE != 0 {
            icon.callback = field(off::CALLBACK).unwrap_or(0);
        }
        if flags & NIF_ICON != 0 {
            if let Some(old) = icon.icon.take() {
                unsafe {
                    let _ = DestroyIcon(old);
                }
            }
            // The handle belongs to the app; keep our own copy.
            let raw = HICON(field(off::ICON).unwrap_or(0) as usize as *mut _);
            icon.icon = (!raw.is_invalid()).then(|| unsafe { CopyIcon(raw).ok() }).flatten();
        }
        if flags & NIF_TIP != 0 {
            if let Some(b) = nid.get(off::TIP..off::TIP + 256) {
                let wide: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
                icon.tip = String::from_utf16_lossy(&wide);
            }
        }
        if flags & NIF_STATE != 0 {
            let mask = field(off::STATE_MASK).unwrap_or(0);
            if mask & NIS_HIDDEN != 0 {
                icon.hidden = field(off::STATE).unwrap_or(0) & NIS_HIDDEN != 0;
            }
        }
    });
}

/// Renders an icon into premultiplied BGRA at `size` px.
fn icon_pixels(icon: HICON, size: i32) -> Option<IconPixels> {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = CreateCompatibleDC(None);
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old = SelectObject(dc, dib);
        let drawn = DrawIconEx(dc, 0, 0, icon, size, size, 0, None, DI_NORMAL).is_ok();
        let len = (size * size * 4) as usize;
        let mut data = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(dc);
        if !drawn {
            return None;
        }
        // Icons without an alpha channel draw opaque pixels with alpha 0.
        if data.chunks_exact(4).all(|p| p[3] == 0) {
            data.chunks_exact_mut(4).filter(|p| p[0] | p[1] | p[2] != 0).for_each(|p| p[3] = 255);
        }
        Some(IconPixels { width: size as u32, height: size as u32, data })
    }
}
