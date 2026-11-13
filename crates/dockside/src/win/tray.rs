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
//!
//! Everything hinges on staying first in z-order: whenever Explorer's tray is
//! ahead of ours, `Shell_NotifyIcon` calls skip us and those icons go missing.
//! So we never leave the topmost band (an elevated DockSide's hidden window
//! that does never gets back in), and raising is verified with `FindWindow`
//! and re-checked often.

use std::cell::RefCell;
use std::time::{Duration, Instant};

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
const NOTIFYICON_VERSION: u32 = 3;
const NOTIFYICON_VERSION_4: u32 = 4;
const NIN_SELECT: u32 = WM_USER;
/// `WM_COPYDATA` kinds the tray receives (`COPYDATASTRUCT::dwData`).
const COPYDATA_NOTIFY_ICON: usize = 1;
const COPYDATA_ICON_RECT: usize = 3;
/// How often the host checks it's still the tray apps find.
const RAISE_TIMER: usize = 1;
const RAISE_MS: u32 = 250;
/// Asks apps for their icons once more this long after starting: DockSide's
/// first second is busy building surfaces, and most apps' answers to the first
/// ask got lost then.
const ASK_AGAIN_TIMER: usize = 2;
const ASK_AGAIN_MS: u32 = 2000;
/// Behind Explorer's tray for longer than this, we may have missed icons being
/// added: ask apps to add them again.
const MISSED_ADDS_AFTER: Duration = Duration::from_secs(2);
/// An app asking where its icon is this soon after a click gets the click's spot.
const CLICK_RECT_FOR: Duration = Duration::from_secs(10);
/// Half the side of the icon rectangle reported around a click, in pixels.
const CLICK_RECT_HALF: i32 = 12;

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
    /// Nesting depth of `explorer_first`: while above 0 we're lowered on purpose.
    lowered: u32,
    /// Since when Explorer's tray has been ahead of ours (unintentionally).
    behind_since: Option<Instant>,
    last_click: Option<LastClick>,
}

/// The icon last clicked from the menu bar, and where, for `Shell_NotifyIconGetRect`.
struct LastClick {
    id: IconId,
    hwnd: HWND,
    uid: u32,
    pt: POINT,
    at: Instant,
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
        // Apps reach us with WM_COPYDATA, which UIPI drops while we're elevated.
        super::elevation::allow_messages(hwnd, &[WM_COPYDATA]);
        HOST.with(|h| {
            *h.borrow_mut() = Some(Host { hwnd, icons: Vec::new(), lowered: 0, behind_since: None, last_click: None })
        });
        keep_on_top();
        // Explorer's taskbar re-asserts itself topmost now and then; win back first place quickly.
        SetTimer(hwnd, RAISE_TIMER, RAISE_MS, None);
        SetTimer(hwnd, ASK_AGAIN_TIMER, ASK_AGAIN_MS, None);
        ask_for_icons();
    }
    Ok(())
}

/// Running apps re-add their icons ("TaskbarCreated"), which now come to us first.
/// Sent to each top-level window rather than broadcast: few apps answered an
/// elevated DockSide's startup broadcast (the same broadcast from an
/// unelevated process got answers).
fn ask_for_icons() {
    unsafe extern "system" fn each(hwnd: HWND, msg: LPARAM) -> BOOL {
        let _ = SendNotifyMessageW(hwnd, msg.0 as u32, WPARAM(0), LPARAM(0));
        TRUE
    }
    unsafe {
        let created = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = EnumWindows(Some(each), LPARAM(created as isize));
    }
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
    let host = HOST.with(|h| h.try_borrow().ok()?.as_ref().filter(|h| h.lowered == 0).map(|h| h.hwnd));
    let Some(hwnd) = host else { return };
    let (first, err) = raise(hwnd);
    let now = Instant::now();
    let rebroadcast = HOST.with(|h| {
        let mut h = h.try_borrow_mut().ok()?;
        let host = h.as_mut()?;
        match (first, host.behind_since) {
            (true, None) => None,
            (true, Some(since)) => {
                host.behind_since = None;
                let missed = now - since > MISSED_ADDS_AFTER;
                log!("tray: our tray host is first again after {} ms", (now - since).as_millis());
                Some(missed)
            }
            (false, None) => {
                host.behind_since = Some(now);
                log!("tray: Explorer's tray is ahead of ours, so new icons go to Explorer only ({err})");
                None
            }
            (false, Some(_)) => None,
        }
    });
    if rebroadcast == Some(true) {
        log!("tray: asking apps to re-add their icons");
        ask_for_icons();
    }
}

/// Raises `hwnd` until `FindWindow` finds it ahead of Explorer's tray. Returns
/// whether it is, and the last `SetWindowPos` error otherwise.
fn raise(hwnd: HWND) -> (bool, String) {
    let first = || unsafe { FindWindowW(CLASS, None).ok() == Some(hwnd) };
    let mut err = String::from("no error");
    // The first HWND_TOPMOST of a window outside the topmost band lands at that band's bottom.
    for _ in 0..3 {
        if first() {
            return (true, err);
        }
        if let Err(e) = unsafe { SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) } {
            err = e.to_string();
        }
    }
    (first(), err)
}

/// Runs `f` with Explorer's tray in front of ours, so `SHAppBarMessage` calls
/// in it reach Explorer directly. Explorer accepts appbar messages we forward
/// but doesn't reserve their space, so DockSide's own appbars go around us.
/// Nested calls lower us once and raise us when the outermost one ends.
pub fn explorer_first<R>(f: impl FnOnce() -> R) -> R {
    let lowered = HOST.with(|h| {
        let mut h = h.try_borrow_mut().ok()?;
        let host = h.as_mut()?;
        host.lowered += 1;
        Some((host.hwnd, host.lowered == 1))
    });
    if let Some((hwnd, true)) = lowered {
        // Just behind Explorer's tray, staying in the topmost band: an elevated
        // DockSide's window that leaves the band (HWND_NOTOPMOST / HWND_BOTTOM)
        // never gets back in, though SetWindowPos(HWND_TOPMOST) reports success.
        if let Some(explorer) = explorer_tray() {
            unsafe {
                let _ = SetWindowPos(hwnd, explorer, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }
    let r = f();
    if lowered.is_some() {
        let outermost = HOST.with(|h| {
            let mut h = h.try_borrow_mut().ok()?;
            let host = h.as_mut()?;
            host.lowered = host.lowered.saturating_sub(1);
            Some(host.lowered == 0)
        });
        if outermost == Some(true) {
            keep_on_top();
        }
    }
    r
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

/// The callback messages Explorer sends an icon of `version` for a click with
/// `button`: the raw mouse messages, then (version 3 and up) the "selected" /
/// "context menu" notification most apps act on.
fn click_messages(button: Button, version: u32) -> &'static [u32] {
    match (button, version >= NOTIFYICON_VERSION) {
        (Button::Left, true) => &[WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT],
        (Button::Left, false) => &[WM_LBUTTONDOWN, WM_LBUTTONUP],
        (Button::Right, true) => &[WM_RBUTTONDOWN, WM_RBUTTONUP, WM_CONTEXTMENU],
        (Button::Right, false) => &[WM_RBUTTONDOWN, WM_RBUTTONUP],
    }
}

/// The callback's (wParam, lParam) for `msg`: version 4 packs the anchor point
/// and icon id, earlier versions pass the icon id and the message.
fn callback_params(version: u32, uid: u32, msg: u32, pt: POINT) -> (WPARAM, LPARAM) {
    if version >= NOTIFYICON_VERSION_4 {
        let anchor = ((pt.y as u16 as usize) << 16) | pt.x as u16 as usize;
        (WPARAM(anchor), LPARAM((((uid & 0xFFFF) as isize) << 16) | msg as isize))
    } else {
        (WPARAM(uid as usize), LPARAM(msg as isize))
    }
}

/// Clicks an icon the way the taskbar would, at the pointer (the menu item
/// that was clicked): apps place their menu at the anchor, or read the cursor.
pub fn click(id: IconId, button: Button) {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let target = HOST.with(|h| {
        let mut h = h.borrow_mut();
        let host = h.as_mut()?;
        let icon = host.icons.iter().find(|i| i.id == id)?;
        let t = (icon.hwnd, icon.uid, icon.callback, icon.version);
        host.last_click = Some(LastClick { id, hwnd: icon.hwnd, uid: icon.uid, pt, at: Instant::now() });
        Some(t)
    });
    let Some((hwnd, uid, callback, version)) = target else { return };
    let exe = exe_name(hwnd);
    if callback == 0 {
        log!("tray: {exe} gave its icon no callback message; nothing to click");
        return;
    }
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        // Apps call SetForegroundWindow before TrackPopupMenu; without that right
        // their menu doesn't take the keyboard or close on an outside click. We
        // have it to give: the click that got us here was ours.
        if let Err(e) = AllowSetForegroundWindow(pid) {
            log!("tray: couldn't let {exe} come to the front: {e}");
        }
        log!("tray: {button:?} click to {exe} (version {version}) at {},{}", pt.x, pt.y);
        for &m in click_messages(button, version) {
            let (wp, lp) = callback_params(version, uid, m, pt);
            if let Err(e) = PostMessageW(hwnd, callback, wp, lp) {
                log!("tray: click to {exe} failed: {e}");
                return;
            }
        }
    }
}

/// The file name of the exe that owns `hwnd`, for the log.
fn exe_name(hwnd: HWND) -> String {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    super::apps::process_path(pid)
        .and_then(|p| std::path::Path::new(&p).file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| format!("pid {pid}"))
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COPYDATA => {
            let cds = &*(lp.0 as *const COPYDATASTRUCT);
            let data = if cds.lpData.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize)
            };
            // Asked where an icon just clicked from the menu bar is: there, not
            // in the hidden taskbar, so the app's flyout opens by the pointer.
            if cds.dwData == COPYDATA_ICON_RECT {
                if let Some(r) = clicked_icon_rect(data) {
                    return LRESULT(r);
                }
            }
            // Explorer's tray answers first: appbar queries get real results,
            // and we learn whether it knows the icon (see `record`).
            let answer = forward(msg, wp, lp);
            if cds.dwData == COPYDATA_NOTIFY_ICON {
                record(data, answer.map(|r| r != 0));
            }
            LRESULT(answer.unwrap_or((cds.dwData == COPYDATA_NOTIFY_ICON) as isize))
        }
        WM_TIMER if wp.0 == ASK_AGAIN_TIMER => {
            let _ = KillTimer(hwnd, ASK_AGAIN_TIMER);
            ask_for_icons();
            LRESULT(0)
        }
        WM_TIMER if wp.0 == RAISE_TIMER => {
            keep_on_top();
            LRESULT(0)
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

/// `Shell_NotifyIconGetRect` for the icon last clicked from the menu bar: an
/// icon-sized square around the click, as `MAKELPARAM(x, y)` of its top-left
/// (request 1) or bottom-right (request 2) corner. `None` for anything else,
/// which Explorer answers.
fn clicked_icon_rect(data: &[u8]) -> Option<isize> {
    if read_u32(data, 0)? != TRAY_SIGNATURE {
        return None;
    }
    let corner = read_u32(data, 4)?;
    let (id, hwnd, uid, pt) = HOST.with(|h| {
        let h = h.try_borrow().ok()?;
        let c = h.as_ref()?.last_click.as_ref().filter(|c| c.at.elapsed() < CLICK_RECT_FOR)?;
        Some((c.id, c.hwnd, c.uid, c.pt))
    })?;
    if !identifies(data, id, hwnd, uid) {
        return None;
    }
    let (x, y) = match corner {
        1 => (pt.x - CLICK_RECT_HALF, pt.y - CLICK_RECT_HALF),
        2 => (pt.x + CLICK_RECT_HALF, pt.y + CLICK_RECT_HALF),
        _ => return None,
    };
    Some(((y as u16 as u32) << 16 | x as u16 as u32) as i32 as isize)
}

/// Whether a `Shell_NotifyIconGetRect` request (header, then `cbSize`, the
/// window, the id and the GUID) names this icon. The window field is pointer
/// sized, so accept either layout.
fn identifies(data: &[u8], id: IconId, hwnd: HWND, uid: u32) -> bool {
    // (window, uid, guid) offsets: 64-bit window after padding, or 32-bit.
    [(16, 24, 28), (12, 16, 20)].into_iter().any(|(at_hwnd, at_uid, at_guid)| match id {
        IconId::Guid(g) => data.get(at_guid..at_guid + 16).map(|b| u128::from_le_bytes(b.try_into().unwrap())) == Some(g),
        IconId::Window(..) => {
            read_u32(data, at_hwnd) == Some(hwnd.0 as usize as u32) && read_u32(data, at_uid) == Some(uid)
        }
    })
}

/// Applies one `Shell_NotifyIcon` call to our icon list. `explorer_ok`: how
/// Explorer's tray answered it (`None` when there's no Explorer to ask). An
/// update for an icon we never saw added (it was added while Explorer's tray
/// was ahead of ours) adds it, if Explorer knows the icon.
fn record(data: &[u8], explorer_ok: Option<bool>) {
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

    let change = HOST.with(|h| {
        let mut h = h.borrow_mut();
        let host = h.as_mut()?;
        let pos = host.icons.iter().position(|i| i.id == id);
        let change = match message {
            NIM_DELETE => {
                let gone = host.icons.remove(pos?);
                return Some(("removed", gone.tip.lines().next().unwrap_or("").to_string(), gone.version));
            }
            // Explorer refuses a second add of an icon it already has (one we
            // missed being added): it's still an icon.
            NIM_ADD if pos.is_none() => Some("added"),
            NIM_MODIFY | NIM_SETVERSION if pos.is_none() && explorer_ok == Some(true) => Some("found"),
            NIM_ADD | NIM_MODIFY | NIM_SETVERSION if pos.is_some() => None,
            _ => return None,
        };
        if change.is_some() {
            host.icons.push(TrayIcon {
                id,
                hwnd,
                uid,
                callback: 0,
                version: 0,
                icon: None,
                tip: String::new(),
                hidden: false,
            });
        }
        let icon = host.icons.iter_mut().find(|i| i.id == id)?;
        icon.hwnd = hwnd;
        icon.uid = uid;
        if message == NIM_SETVERSION {
            icon.version = field(off::VERSION).unwrap_or(0);
            return change.map(|c| (c, icon.tip.clone(), icon.version));
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
        if flags & NIF_ICON != 0 && icon.icon.is_none() && field(off::ICON).unwrap_or(0) != 0 {
            log!("tray: couldn't copy the icon {} gave", exe_name(hwnd));
        }
        change.map(|c| (c, icon.tip.lines().next().unwrap_or("").to_string(), icon.version))
    });
    if let Some((change, tip, version)) = change {
        log!("tray: {change} {} \"{tip}\" (version {version})", exe_name(hwnd));
    }
}

/// Renders an icon into premultiplied BGRA at `size` px.
pub(crate) fn icon_pixels(icon: HICON, size: i32) -> Option<IconPixels> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_3_and_4_get_the_notification_after_the_mouse_messages() {
        assert_eq!(click_messages(Button::Right, 0), &[WM_RBUTTONDOWN, WM_RBUTTONUP]);
        assert_eq!(click_messages(Button::Right, 3), &[WM_RBUTTONDOWN, WM_RBUTTONUP, WM_CONTEXTMENU]);
        assert_eq!(click_messages(Button::Left, 3), &[WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT]);
        assert_eq!(click_messages(Button::Left, 4), &[WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT]);
    }

    #[test]
    fn callback_params_match_each_version() {
        let pt = POINT { x: -5, y: 300 };
        let (wp, lp) = callback_params(3, 7, WM_CONTEXTMENU, pt);
        assert_eq!((wp.0, lp.0), (7, WM_CONTEXTMENU as isize));
        let (wp, lp) = callback_params(4, 7, WM_CONTEXTMENU, pt);
        assert_eq!(wp.0 & 0xFFFF, (-5i16) as u16 as usize);
        assert_eq!(wp.0 >> 16, 300);
        assert_eq!(lp.0, (7 << 16) | WM_CONTEXTMENU as isize);
    }

    fn rect_request(corner: u32, hwnd: u32, uid: u32, guid: u128) -> Vec<u8> {
        // dwMagic, dwMessage, cbSize, padding, hWnd (64-bit), uID, guidItem
        let mut d = Vec::new();
        for v in [TRAY_SIGNATURE, corner, 40, 0, hwnd, 0, uid] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(&guid.to_le_bytes());
        d
    }

    #[test]
    fn rect_requests_name_icons_by_window_or_guid() {
        let hwnd = HWND(0x1234 as *mut _);
        assert!(identifies(&rect_request(1, 0x1234, 9, 0), IconId::Window(0x1234, 9), hwnd, 9));
        assert!(!identifies(&rect_request(1, 0x1234, 8, 0), IconId::Window(0x1234, 9), hwnd, 9));
        let g = 0xDEAD_BEEF_u128 << 64 | 42;
        assert!(identifies(&rect_request(2, 0, 0, g), IconId::Guid(g), HWND::default(), 0));
        assert!(!identifies(&rect_request(2, 0, 0, g + 1), IconId::Guid(g), HWND::default(), 0));
    }
}
