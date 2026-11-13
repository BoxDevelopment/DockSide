//! Quick Look for Office documents and anything else with a shell preview
//! handler (the ones Explorer's preview pane uses).
//!
//! A preview handler draws into a real window of its own, which a layered
//! Finder window can't hold as a child, so it gets a borderless window owned
//! by the Finder window and kept over Quick Look's content area. Each one
//! runs on its own thread (with its own message loop), so a slow or stuck
//! handler never holds up Finder.

use std::cell::RefCell;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{w, GUID, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUNDSMALL, DWM_WINDOW_CORNER_PREFERENCE};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, STGM_READ, STGM_SHARE_DENY_NONE};
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithFile, IInitializeWithStream};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// The shell's preview handler extension point.
const PREVIEW_HANDLER: PCWSTR = w!("{8895b1c6-b41f-4c1c-a562-0d564250836f}");
const MSG_PLACE: u32 = WM_APP + 1;
const MSG_QUIT: u32 = WM_APP + 2;
const TIMER_FOLLOW: usize = 1;

/// The handler class registered for `ext` (without the dot), if any.
pub fn handler_for(ext: &str) -> Option<GUID> {
    if ext.is_empty() {
        return None;
    }
    let mut buf = [0u16; 64];
    let mut len = buf.len() as u32;
    let assoc = HSTRING::from(format!(".{ext}"));
    unsafe {
        AssocQueryStringW(ASSOCF_INIT_DEFAULTTOSTAR, ASSOCSTR_SHELLEXTENSION, &assoc, PREVIEW_HANDLER, PWSTR(buf.as_mut_ptr()), &mut len).ok().ok()?;
    }
    let s = crate::win::from_wide(&buf);
    GUID::try_from(s.trim_matches(|c| c == '{' || c == '}')).ok()
}

pub const LOADING: u8 = 0;
pub const READY: u8 = 1;
pub const FAILED: u8 = 2;

struct Shared {
    status: AtomicU8,
    /// Where to sit, in screen pixels relative to the owner's window rect (None: hidden).
    place: Mutex<Option<RECT>>,
    hwnd: Mutex<isize>,
}

pub struct PreviewHost {
    shared: Arc<Shared>,
    placed: Option<RECT>,
}

impl PreviewHost {
    /// Starts `clsid` previewing `path` in a hidden window owned by `owner`.
    /// `notify` gets `msg` once it's ready or has failed.
    pub fn open(path: &Path, clsid: GUID, owner: HWND, bg: COLORREF, fg: COLORREF, notify: isize, msg: u32) -> PreviewHost {
        let shared = Arc::new(Shared { status: AtomicU8::new(LOADING), place: Mutex::new(None), hwnd: Mutex::new(0) });
        let s = shared.clone();
        let file = path.to_path_buf();
        let owner = owner.0 as isize;
        let spawned = std::thread::Builder::new().name("quick look preview".into()).spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let ok = host(&file, clsid, HWND(owner as _), bg, fg, &s);
            if !ok {
                s.status.store(FAILED, Ordering::Release);
            }
            crate::win::post(notify, msg);
            if ok {
                pump();
            }
            HOST.with(|h| {
                if let Some(st) = h.borrow_mut().take() {
                    let _ = st.handler.Unload();
                    let _ = DestroyWindow(st.hwnd);
                }
            });
        });
        if let Err(e) = spawned {
            log!("quick look preview thread failed: {e}");
            shared.status.store(FAILED, Ordering::Release);
        }
        PreviewHost { shared, placed: None }
    }

    pub fn status(&self) -> u8 {
        self.shared.status.load(Ordering::Acquire)
    }

    /// Shows the preview over `rc` (screen pixels relative to the owner's
    /// window rect), or hides it. Cheap to call every frame.
    pub fn place(&mut self, rc: Option<RECT>) {
        if rc == self.placed {
            return;
        }
        self.placed = rc;
        if let Ok(mut p) = self.shared.place.lock() {
            *p = rc;
        }
        let hwnd = *self.shared.hwnd.lock().unwrap();
        if hwnd != 0 {
            crate::win::post(hwnd, MSG_PLACE);
        }
    }
}

impl Drop for PreviewHost {
    fn drop(&mut self) {
        // The thread unloads the handler and goes; if it's still starting up,
        // it sees the window gone (place None) and quits once it's made one.
        if let Ok(mut p) = self.shared.place.lock() {
            *p = None;
        }
        self.shared.status.store(FAILED, Ordering::Release);
        let hwnd = *self.shared.hwnd.lock().unwrap();
        if hwnd != 0 {
            crate::win::post(hwnd, MSG_QUIT);
        }
    }
}

struct HostState {
    hwnd: HWND,
    owner: HWND,
    handler: IPreviewHandler,
    shared: Arc<Shared>,
    /// Where it last went, in screen pixels.
    at: Option<RECT>,
}

thread_local! {
    static HOST: RefCell<Option<HostState>> = const { RefCell::new(None) };
}

unsafe fn host(path: &Path, clsid: GUID, owner: HWND, bg: COLORREF, fg: COLORREF, shared: &Arc<Shared>) -> bool {
    static CLASS: std::sync::Once = std::sync::Once::new();
    CLASS.call_once(|| {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc_),
            hInstance: crate::win::hinstance(),
            lpszClassName: w!("DockSideQuickLookHost"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    });
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        w!("DockSideQuickLookHost"),
        w!("Quick Look"),
        WS_POPUP | WS_CLIPCHILDREN,
        0,
        0,
        100,
        100,
        owner,
        None,
        crate::win::hinstance(),
        None,
    ) else {
        return false;
    };
    let pref: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUNDSMALL;
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &pref as *const _ as _, 4);
    *shared.hwnd.lock().unwrap() = hwnd.0 as isize;
    // Dropped while starting: nothing to show.
    if shared.status.load(Ordering::Acquire) == FAILED {
        let _ = DestroyWindow(hwnd);
        return false;
    }
    let handler: IPreviewHandler = match CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER | CLSCTX_INPROC_SERVER) {
        Ok(h) => h,
        Err(e) => {
            log!("quick look: preview handler {clsid:?} unavailable: {e}");
            let _ = DestroyWindow(hwnd);
            return false;
        }
    };
    let wide = HSTRING::from(path.as_os_str());
    let init = if let Ok(f) = windows::core::Interface::cast::<IInitializeWithFile>(&handler) {
        f.Initialize(&wide, STGM_READ.0).is_ok()
    } else {
        false
    } || match windows::core::Interface::cast::<IInitializeWithStream>(&handler) {
        Ok(s) => SHCreateStreamOnFileEx(&wide, (STGM_READ | STGM_SHARE_DENY_NONE).0, 0, false, None).is_ok_and(|stm| s.Initialize(&stm, STGM_READ.0).is_ok()),
        Err(_) => false,
    } || match windows::core::Interface::cast::<IInitializeWithItem>(&handler) {
        Ok(i) => SHCreateItemFromParsingName::<_, _, IShellItem>(&wide, None).is_ok_and(|item| i.Initialize(&item, STGM_READ.0).is_ok()),
        Err(_) => false,
    };
    if !init {
        log!("quick look: preview handler couldn't take {}", path.display());
        let _ = DestroyWindow(hwnd);
        return false;
    }
    if let Ok(v) = windows::core::Interface::cast::<IPreviewHandlerVisuals>(&handler) {
        let _ = v.SetBackgroundColor(bg);
        let _ = v.SetTextColor(fg);
    }
    let rc = RECT { left: 0, top: 0, right: 100, bottom: 100 };
    if handler.SetWindow(hwnd, &rc).is_err() || handler.DoPreview().is_err() {
        log!("quick look: preview handler failed on {}", path.display());
        let _ = handler.Unload();
        let _ = DestroyWindow(hwnd);
        return false;
    }
    HOST.with(|h| *h.borrow_mut() = Some(HostState { hwnd, owner, handler, shared: shared.clone(), at: None }));
    shared.status.store(READY, Ordering::Release);
    follow();
    true
}

unsafe fn pump() {
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// Moves the window to where Quick Look wants it, relative to the owner (which
/// may be moving), or hides it.
fn follow() {
    HOST.with(|h| {
        let mut h = h.borrow_mut();
        let Some(st) = h.as_mut() else { return };
        let want = *st.shared.place.lock().unwrap();
        unsafe {
            let mut owner = RECT::default();
            let shown = IsWindowVisible(st.owner).as_bool() && !IsIconic(st.owner).as_bool() && GetWindowRect(st.owner, &mut owner).is_ok();
            let target = want.filter(|_| shown).map(|r| RECT {
                left: owner.left + r.left,
                top: owner.top + r.top,
                right: owner.left + r.right,
                bottom: owner.top + r.bottom,
            });
            if target == st.at {
                return;
            }
            match target {
                Some(r) => {
                    let (w, ht) = (r.right - r.left, r.bottom - r.top);
                    let _ = SetWindowPos(st.hwnd, None, r.left, r.top, w, ht, SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW);
                    let _ = st.handler.SetRect(&RECT { left: 0, top: 0, right: w, bottom: ht });
                    // Keeps up with the Finder window while it's dragged.
                    SetTimer(st.hwnd, TIMER_FOLLOW, 30, None);
                }
                None => {
                    let _ = ShowWindow(st.hwnd, SW_HIDE);
                    let _ = KillTimer(st.hwnd, TIMER_FOLLOW);
                }
            }
            st.at = target;
        }
    });
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        MSG_PLACE => {
            follow();
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_FOLLOW => {
            follow();
            LRESULT(0)
        }
        MSG_QUIT => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        // Never takes focus from Finder by being clicked on its frame.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
