//! Windows virtual desktops: switching (Alt+1…9, Win+Ctrl+←/→) and watching
//! for switches made anywhere (Task View, touchpad gestures).
//!
//! There's no public API for this, so it goes through the shell's internal
//! virtual desktop interfaces (via `winvd`, Windows 11 24H2+). Windows still
//! pops up the desktop's name after every switch; `hide_switch_popup` hides it,
//! and the Dynamic Island shows the switch instead.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{CreateRectRgn, SetWindowRgn, HRGN};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

/// When the desktop last changed (ms since the Unix epoch), so the popup Windows shows
/// right after can be told apart from its other popups in the same window class.
static LAST_SWITCH: AtomicU64 = AtomicU64::new(0);
static HIDE_POPUP: AtomicBool = AtomicBool::new(false);
/// The popup window we clipped, to give it back its region when we stop.
static POPUP_HWND: AtomicIsize = AtomicIsize::new(0);
/// How long after a switch Windows' desktop name popup can still appear.
const POPUP_WINDOW_MS: u64 = 1500;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn mark_switch() {
    LAST_SWITCH.store(now_ms(), Ordering::Relaxed);
}

#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    /// 0-based.
    pub index: usize,
    pub count: usize,
    pub name: String,
}

/// The current desktop, its position and name ("Desktop 2" when unnamed).
pub fn current() -> Option<Info> {
    let desktop = winvd::get_current_desktop().ok()?;
    let index = desktop.get_index().ok()? as usize;
    let count = winvd::get_desktop_count().ok()? as usize;
    let name = desktop.get_name().ok().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("Desktop {}", index + 1));
    Some(Info { index, count, name })
}

pub fn count() -> usize {
    winvd::get_desktop_count().map(|n| n as usize).unwrap_or(1)
}

/// Switches to the desktop at `index` (0-based); false when there isn't one.
pub fn switch_to(index: usize) -> bool {
    if index >= count() {
        return false;
    }
    mark_switch();
    match winvd::switch_desktop(index as u32) {
        Ok(()) => true,
        Err(e) => {
            log!("desktop switch failed: {e:?}");
            false
        }
    }
}

/// Moves one desktop left (`-1`) or right (`1`), stopping at the ends like Windows.
pub fn step(dir: i32) {
    if let Some(info) = current() {
        let next = (info.index as i32 + dir).clamp(0, info.count as i32 - 1) as usize;
        if next != info.index {
            switch_to(next);
        }
    }
}

/// Posts `msg` to `hwnd` whenever the current desktop changes (from anywhere).
/// Dropping it stops listening.
pub struct Watcher {
    _thread: winvd::DesktopEventThread,
}

pub fn watch(hwnd: isize, msg: u32) -> Option<Watcher> {
    let (tx, rx) = std::sync::mpsc::channel::<winvd::DesktopEvent>();
    let thread = winvd::listen_desktop_events(tx).map_err(|e| log!("desktop events unavailable: {e:?}")).ok()?;
    // Ends when the listener (and with it the sender) goes away.
    let spawned = std::thread::Builder::new().name("desktop-events".into()).spawn(move || {
        for event in rx {
            if matches!(event, winvd::DesktopEvent::DesktopChanged { .. }) {
                // Straight away (not via the UI thread): the popup follows within milliseconds.
                mark_switch();
                super::post(hwnd, msg);
            }
        }
    });
    if let Err(e) = spawned {
        log!("desktop events thread failed: {e}");
    }
    Some(Watcher { _thread: thread })
}

/// Hides Windows' desktop name popup (shown on every switch, even through the
/// internal API) while installed. It's a small Explorer XAML island window at
/// the bottom of the screen, reused for every switch. Hiding it when it shows
/// loses the race (it flashes, especially when switching fast), so the first
/// time it appears it gets an empty window region: from then on it can show
/// all it likes without drawing a pixel. The same class hosts other popups, so
/// only one that shows up right after a switch is touched.
pub struct PopupHider(HWINEVENTHOOK);

impl Drop for PopupHider {
    fn drop(&mut self) {
        HIDE_POPUP.store(false, Ordering::Relaxed);
        unsafe {
            let _ = UnhookWinEvent(self.0);
            // Let Windows' popup draw again.
            let popup = HWND(POPUP_HWND.swap(0, Ordering::Relaxed) as *mut _);
            if IsWindow(popup).as_bool() {
                SetWindowRgn(popup, HRGN::default(), false);
            }
        }
    }
}

pub fn hide_switch_popup() -> Option<PopupHider> {
    unsafe {
        let shell = GetShellWindow();
        let mut pid = 0;
        GetWindowThreadProcessId(shell, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let hook = SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, None, Some(on_show), pid, 0, WINEVENT_OUTOFCONTEXT);
        if hook.is_invalid() {
            log!("desktop popup hook failed");
            return None;
        }
        HIDE_POPUP.store(true, Ordering::Relaxed);
        Some(PopupHider(hook))
    }
}

unsafe extern "system" fn on_show(_: HWINEVENTHOOK, _: u32, hwnd: HWND, id_object: i32, _: i32, _: u32, _: u32) {
    if id_object != OBJID_WINDOW.0 || !HIDE_POPUP.load(Ordering::Relaxed) {
        return;
    }
    if now_ms().saturating_sub(LAST_SWITCH.load(Ordering::Relaxed)) > POPUP_WINDOW_MS {
        return;
    }
    if super::class_name(hwnd) != "XamlExplorerHostIslandWindow" || GetWindowTextLengthW(hwnd) != 0 {
        return;
    }
    // The name popup is small; the class's full-screen hosts (Alt+Tab) are not.
    let mut r = windows::Win32::Foundation::RECT::default();
    let _ = GetWindowRect(hwnd, &mut r);
    if r.right - r.left < 600 && r.bottom - r.top < 200 {
        // The window owns the region from here on.
        SetWindowRgn(hwnd, CreateRectRgn(0, 0, 0, 0), false);
        POPUP_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = ShowWindowAsync(hwnd, SW_HIDE);
    }
}
