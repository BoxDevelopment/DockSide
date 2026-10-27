// Portions from the Windhawk mod "MacOS Minimize Animation" (MIT, Abdullah Masood and Potassiumuncher).

//! Genie minimize / restore, and the open and close effects: the decision, hide and
//! reveal logic of the Windhawk mod "MacOS Minimize Animation" v3.1.7 (MIT),
//! including Potassiumuncher's genie engine. Everything that decides
//! WHEN to animate and how the real window is hidden / revealed is kept from the mod;
//! the rendering is DockSide's (an AnimRequest instead of a draw thread).

use core::ffi::c_void;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasherDefault;
use std::collections::hash_map::DefaultHasher;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicI32, AtomicIsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Shell::{ABM_GETSTATE, ABS_AUTOHIDE, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::loader::{effects_active_here, is_host_process, local_stop_event, peer_animates_itself, settings, spawn_worker};
use crate::shared::*;
use crate::{capture_to_section, create_user32_hook, host_ipc_window, send_anim_request, HookScope, UNLOADING};

type DefWindowProcFn = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
type ShowWindowFn = unsafe extern "system" fn(HWND, i32) -> BOOL;
type SetWindowPlacementFn = unsafe extern "system" fn(HWND, *const WINDOWPLACEMENT) -> BOOL;
type CloseWindowFn = unsafe extern "system" fn(HWND) -> BOOL;
type SetWindowPosFn = unsafe extern "system" fn(HWND, HWND, i32, i32, i32, i32, u32) -> BOOL;
type DestroyWindowFn = unsafe extern "system" fn(HWND) -> BOOL;
type IsIconicFn = unsafe extern "system" fn(HWND) -> BOOL;
type GetWindowPlacementFn = unsafe extern "system" fn(HWND, *mut WINDOWPLACEMENT) -> BOOL;

static DEF_WINDOW_PROC_W: AtomicIsize = AtomicIsize::new(0);
static DEF_WINDOW_PROC_A: AtomicIsize = AtomicIsize::new(0);
static SHOW_WINDOW: AtomicIsize = AtomicIsize::new(0);
static SHOW_WINDOW_ASYNC: AtomicIsize = AtomicIsize::new(0);
static SET_WINDOW_PLACEMENT: AtomicIsize = AtomicIsize::new(0);
static CLOSE_WINDOW: AtomicIsize = AtomicIsize::new(0);
static SET_WINDOW_POS: AtomicIsize = AtomicIsize::new(0);
static DESTROY_WINDOW: AtomicIsize = AtomicIsize::new(0);
static IS_ICONIC: AtomicIsize = AtomicIsize::new(0);
static GET_WINDOW_PLACEMENT: AtomicIsize = AtomicIsize::new(0);

/// The original (trampoline) of a hooked function.
fn orig<T: Copy>(a: &AtomicIsize) -> T {
    let p = a.load(Ordering::Acquire);
    unsafe { core::mem::transmute_copy::<isize, T>(&p) }
}

fn show_window_original(h: HWND, cmd: i32) -> BOOL {
    unsafe { orig::<ShowWindowFn>(&SHOW_WINDOW)(h, cmd) }
}

fn show_window_async_original(h: HWND, cmd: i32) -> BOOL {
    unsafe { orig::<ShowWindowFn>(&SHOW_WINDOW_ASYNC)(h, cmd) }
}

// Our own code always sees the real state (the IsIconic hook may report a window as
// minimized a moment early).
fn real_is_iconic(h: HWND) -> bool {
    if IS_ICONIC.load(Ordering::Acquire) != 0 {
        unsafe { orig::<IsIconicFn>(&IS_ICONIC)(h).as_bool() }
    } else {
        unsafe { IsIconic(h).as_bool() }
    }
}

fn real_get_window_placement(h: HWND, wp: &mut WINDOWPLACEMENT) -> bool {
    if GET_WINDOW_PLACEMENT.load(Ordering::Acquire) != 0 {
        unsafe { orig::<GetWindowPlacementFn>(&GET_WINDOW_PLACEMENT)(h, wp).as_bool() }
    } else {
        unsafe { GetWindowPlacement(h, wp).is_ok() }
    }
}

// Set on a window while our own deferred minimize (auto-hide path) runs through
// DefWindowProc, so it is not animated a second time.
const BYPASS_PROP: PCWSTR = w!("DockSideGenieBypass");

type Fast<K, V> = HashMap<K, V, BuildHasherDefault<DefaultHasher>>;

// Latest genie started from this process per window. A request in the SAME direction
// while one is in flight is a duplicate (e.g. two hooked paths for one minimize); the
// OPPOSITE direction goes through - the host turns the running genie around.
#[derive(Clone, Copy)]
struct ActiveAnim {
    token: u64,
    rising: bool,
}

#[derive(Default)]
struct State {
    launch_seen: HashSet<isize, BuildHasherDefault<DefaultHasher>>, // windows already shown / animated once
    anim_active: Fast<isize, ActiveAnim>,
    next_token: u64,
    // Windows whose minimize this process handed over to their own thread
    // (show_window_slow), until they really are minimized. Callers check right after
    // ShowWindow: Explorer's "Show desktop" sends a fallback SC_MINIMIZE (and waits) to
    // a window that isn't minimized yet, and its restore list keeps only the windows
    // that are minimized a few ms after its loop. Meanwhile IsIconic / GetWindowPlacement
    // in this process report such a window as minimized already. Value: deadline
    // (GetTickCount64).
    pending_min: Fast<isize, u64>,
}

fn state() -> MutexGuard<'static, State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default())).lock().unwrap_or_else(|e| e.into_inner())
}

static PENDING_COUNT: AtomicI32 = AtomicI32::new(0); // pending_min.len(): the hooks' fast path
const PENDING_MIN_MS: u64 = 1000; // a hung window stops pretending after this

thread_local! {
    // Set while one of our own hooked calls (ShowWindow, SC_MINIMIZE, ...) changes a
    // window's state on this thread: the HCBT_MINMAX that comes out of it is handled.
    static OWN_STATE_CHANGE: Cell<i32> = const { Cell::new(0) };
    // Which path started a request (shows up in DockSide's log).
    static SOURCE: Cell<u32> = const { Cell::new(SRC_UNKNOWN) };
}

struct OwnStateChange;

impl OwnStateChange {
    fn new() -> Self {
        OWN_STATE_CHANGE.with(|c| c.set(c.get() + 1));
        OwnStateChange
    }
}

impl Drop for OwnStateChange {
    fn drop(&mut self) {
        OWN_STATE_CHANGE.with(|c| c.set(c.get() - 1));
    }
}

struct SourceScope(u32);

impl SourceScope {
    fn new(s: u32) -> Self {
        SourceScope(SOURCE.with(|c| c.replace(s)))
    }
}

impl Drop for SourceScope {
    fn drop(&mut self) {
        SOURCE.with(|c| c.set(self.0));
    }
}

fn prune_pending(s: &mut State, now: u64) {
    s.pending_min.retain(|_, deadline| now < *deadline);
    PENDING_COUNT.store(s.pending_min.len() as i32, Ordering::Relaxed);
}

fn add_pending_minimize(h: HWND) {
    let mut s = state();
    let now = unsafe { GetTickCount64() };
    s.pending_min.insert(h.0 as isize, now + PENDING_MIN_MS);
    prune_pending(&mut s, now);
}

fn forget_pending_minimize(h: HWND) {
    if PENDING_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    let mut s = state();
    s.pending_min.remove(&(h.0 as isize));
    prune_pending(&mut s, unsafe { GetTickCount64() });
}

fn pending_minimize(h: HWND) -> bool {
    let mut s = state();
    prune_pending(&mut s, unsafe { GetTickCount64() });
    s.pending_min.contains_key(&(h.0 as isize))
}

fn setting(f: impl Fn(&SharedSettings) -> i32) -> i32 {
    settings().map_or(0, |s| f(unsafe { &*s }))
}

fn duration_ms() -> i32 {
    setting(|s| s.genie_duration_ms)
}
fn restore_enabled() -> bool {
    setting(|s| s.genie_restore) != 0
}
fn launch_enabled() -> bool {
    setting(|s| s.genie_launch) != 0
}
fn close_enabled() -> bool {
    setting(|s| s.close_effect) != EFFECT_SYSTEM
}
fn unhide_enabled() -> bool {
    setting(|s| s.genie_unhide_taskbar) != 0
}
fn unhide_duration_ms() -> i32 {
    setting(|s| s.genie_unhide_duration_ms)
}

fn is_minimize_cmd(cmd: i32) -> bool {
    cmd == SW_MINIMIZE.0 || cmd == SW_SHOWMINIMIZED.0 || cmd == SW_SHOWMINNOACTIVE.0
}

/// SHAppBarMessage(ABM_GETSTATE), looked up on first use instead of imported: an
/// import would load shell32 (and everything it pulls in) into every process the DLL
/// goes into. Most programs that minimize a window have it loaded anyway. The
/// reference is kept: unloading a system DLL under a foreign program is never worth it.
fn taskbar_state(tray: HWND) -> u32 {
    type AppBarMessage = unsafe extern "system" fn(u32, *mut APPBARDATA) -> usize;
    static F: OnceLock<isize> = OnceLock::new();
    let f = *F.get_or_init(|| unsafe {
        LoadLibraryExW(w!("shell32.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .ok()
            .and_then(|m| GetProcAddress(m, windows::core::s!("SHAppBarMessage")))
            .map_or(0, |p| p as isize)
    });
    if f == 0 {
        return 0;
    }
    let mut abd = APPBARDATA { cbSize: core::mem::size_of::<APPBARDATA>() as u32, hWnd: tray, ..Default::default() };
    unsafe { core::mem::transmute::<isize, AppBarMessage>(f)(ABM_GETSTATE, &mut abd) as u32 }
}

// The mod's "Excluded programs": matched against the process that OWNS the window,
// because the hooks animate a window from whichever process manipulates it. The answer
// is cached per process and settings generation, so the image path is not looked up
// for every window. Another process's entry also expires after a few seconds: its pid
// may be reused by a new program.
#[derive(Clone, Copy, Default)]
struct ExcludedEntry {
    pid: u32,
    generation: u32,
    expires: u64,
    excluded: bool,
}
const EXCLUDED_TTL_MS: u64 = 5000;

fn process_image_names(pid: u32, out: &mut ImageNames) -> bool {
    unsafe {
        let Ok(proc) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut path = [0u16; 260];
        let mut len = path.len() as u32;
        let ok = QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(proc);
        ok && make_image_names(&path[..len as usize], out)
    }
}

fn is_excluded(hwnd: HWND) -> bool {
    struct Cache {
        list: [u16; LIST_CHARS],
        entries: [ExcludedEntry; 8],
        next: usize,
    }
    static CACHE: Mutex<Cache> = Mutex::new(Cache { list: [0; LIST_CHARS], entries: [ExcludedEntry { pid: 0, generation: 0, expires: 0, excluded: false }; 8], next: 0 });
    let Some(s) = settings() else { return false };
    unsafe {
        if (*s).genie_excluded[0] == 0 {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return false;
        }
        let gen = core::ptr::read_volatile(core::ptr::addr_of!((*s).generation));
        let now = GetTickCount64();
        let own = pid == GetCurrentProcessId(); // our own image never changes
        {
            let c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(e) = c.entries.iter().find(|e| e.pid == pid && e.generation == gen && (own || now < e.expires)) {
                return e.excluded;
            }
        }
        let mut names = ImageNames::EMPTY;
        if !process_image_names(pid, &mut names) {
            return false;
        }
        let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        settings_copy_list(s, core::ptr::addr_of!((*s).genie_excluded), &mut c.list);
        let excluded = list_matches(&c.list, &names);
        let next = c.next;
        c.entries[next] = ExcludedEntry { pid, generation: gen, expires: now + EXCLUDED_TTL_MS, excluded };
        c.next = (next + 1) % 8;
        excluded
    }
}

fn set_dwm_transitions(hwnd: HWND, enable: bool) {
    let disable = BOOL(!enable as i32);
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, &disable as *const _ as *const c_void, 4);
    }
}

/// DWM-level hide: the window stays "visible" to Win32 and keeps painting, it is just
/// not composed - so a restore happens invisibly under the genie, without a flash.
fn set_cloak(hwnd: HWND, cloak: bool) {
    let v = BOOL(cloak as i32);
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &v as *const _ as *const c_void, 4);
    }
}

fn undo_rising_hide(hwnd: HWND, original_ex_style: isize, cloak_hidden: bool) {
    unsafe {
        if cloak_hidden {
            set_cloak(hwnd, false);
        } else {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            if original_ex_style & WS_EX_LAYERED.0 as isize == 0 {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, original_ex_style);
            }
        }
    }
    set_dwm_transitions(hwnd, true);
}

/// Only real top-level windows of a reasonable size.
fn should_animate(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        if style & WS_CHILD.0 as isize != 0 {
            return false;
        }
        let mut r = RECT::default();
        if real_is_iconic(hwnd) {
            let mut wp = WINDOWPLACEMENT { length: core::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            if !real_get_window_placement(hwnd, &mut wp) {
                return false;
            }
            r = wp.rcNormalPosition;
        } else if GetWindowRect(hwnd, &mut r).is_err() {
            return false;
        }
        r.right - r.left >= 40 && r.bottom - r.top >= 40
    }
}

/// Inside a message another thread sent and is blocked on: answer it now (with 0) and
/// carry on. Only for messages whose result nobody reads (WM_SYSCOMMAND).
fn release_cross_thread_sender() {
    unsafe {
        if InSendMessageEx(None) & (ISMEX_SEND | ISMEX_REPLIED) == ISMEX_SEND {
            let _ = ReplyMessage(LRESULT(0));
        }
    }
}

/// A top-level window with a title bar (no child / tool / popup / splash windows).
fn is_launch_window(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    unsafe {
        if GetAncestor(hwnd, GA_ROOT) != hwnd {
            return false;
        }
        if GetWindowLongPtrW(hwnd, GWL_STYLE) & WS_CAPTION.0 as isize == 0 && !is_host_app_window(hwnd) {
            return false;
        }
        if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize != 0 {
            return false;
        }
    }
    should_animate(hwnd)
}

/// In DockSide itself: one of its app windows (Finder, Settings, ...). They draw their
/// own title bar, so they have no WS_CAPTION - a minimize box marks them instead. The
/// Dock, menu bar, popups and the genie's own window have none.
fn is_host_app_window(hwnd: HWND) -> bool {
    if !is_host_process() {
        return false;
    }
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid == GetCurrentProcessId()
            && GetAncestor(hwnd, GA_ROOT) == hwnd
            && GetWindowLongPtrW(hwnd, GWL_STYLE) & WS_MINIMIZEBOX.0 as isize != 0
            && GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize == 0
    }
}

/// Effects on for `hwnd` here. In DockSide itself only its own app windows: the Dock
/// minimizing / restoring other programs' windows keeps going straight to Windows
/// (their own DLL animates them).
fn active_for(hwnd: HWND) -> bool {
    effects_active_here() && (!is_host_process() || is_host_app_window(hwnd))
}

/// Geometry and capture in physical pixels whatever the app's own DPI awareness, so
/// the crop fractions and the host's own (per-monitor aware) rects describe the same
/// pixels.
struct DpiScope(DPI_AWARENESS_CONTEXT);

impl DpiScope {
    fn new() -> Self {
        DpiScope(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for DpiScope {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

/// Everything the worker needs to finish an animation once the host is done with it
/// (the mod's COMMON TEARDOWN, minus the rendering resources).
struct Teardown {
    real: HWND,
    rising: bool,
    original_ex_style: isize,
    hidden_by_cloak: bool,
    deferred_minimize: bool,
    requested_unhide: bool,
    next_app: HWND,
    duration_ms: i32,
    unhide_duration_ms: i32,
    done: HANDLE,
    revealed: HANDLE,
    token: u64,
    cancelled: AtomicI32, // request failed: exit without touching anything
}

unsafe extern "system" fn teardown_thread(param: *mut c_void) -> u32 {
    let d = Box::from_raw(param as *mut Teardown);
    let _ = catch_unwind(AssertUnwindSafe(|| teardown(&d)));
    0
}

fn teardown(d: &Teardown) {
    if d.cancelled.load(Ordering::Acquire) != 0 {
        return;
    }
    unsafe {
        // The host signals `done` right after the last frame. The bound only matters if
        // the host dies mid-animation: the real window must never stay hidden.
        let waits = [d.done, local_stop_event()];
        let n = if waits[1].is_invalid() { 1 } else { 2 };
        WaitForMultipleObjects(&waits[..n], false, d.duration_ms.max(0) as u32 + 3000);

        if d.rising && IsWindow(d.real).as_bool() {
            // Reveal the real window the same way it was hidden.
            if d.hidden_by_cloak {
                set_cloak(d.real, false);
            } else {
                let _ = SetLayeredWindowAttributes(d.real, COLORREF(0), 255, LWA_ALPHA);
                if d.original_ex_style & WS_EX_LAYERED.0 as isize == 0 {
                    SetWindowLongPtrW(d.real, GWL_EXSTYLE, d.original_ex_style);
                }
            }
        }
        // The host keeps its last frame up until it sees this, so there is no gap
        // between the genie and the real window.
        if !d.revealed.is_invalid() {
            let _ = SetEvent(d.revealed);
        }

        // The auto-hide deferred path keeps DWM transitions off through the real
        // (deferred) minimize below; every other path re-enables them now.
        if !d.deferred_minimize {
            set_dwm_transitions(d.real, true);
        }

        // Auto-hide "unhide" completion (Potassiumuncher's engine): the caller cloaked the
        // window after our first frame and deferred the real minimize to us, so the
        // revealed taskbar did not slide away mid-animation.
        if d.deferred_minimize {
            if d.requested_unhide && d.unhide_duration_ms > d.duration_ms {
                let mut remaining = d.unhide_duration_ms - d.duration_ms;
                while remaining > 0 && !UNLOADING.load(Ordering::Relaxed) {
                    let chunk = remaining.min(20);
                    Sleep(chunk as u32);
                    remaining -= chunk;
                }
            }
            if !UNLOADING.load(Ordering::Relaxed) && IsWindow(d.real).as_bool() {
                let _ = SetPropW(d.real, BYPASS_PROP, HANDLE(1 as _));
                SendMessageTimeoutW(d.real, WM_SYSCOMMAND, WPARAM(SC_MINIMIZE as usize), LPARAM(0), SMTO_NORMAL, 1000, None);
                let _ = RemovePropW(d.real, BYPASS_PROP);
            }
            // Never leave an invisible, non-minimized window behind.
            if IsWindow(d.real).as_bool() {
                set_cloak(d.real, false);
                set_dwm_transitions(d.real, true);
            }
            // Shift focus so the taskbar slides back down natively.
            if d.requested_unhide && !UNLOADING.load(Ordering::Relaxed) {
                if !d.next_app.is_invalid() && IsWindow(d.next_app).as_bool() && IsWindowVisible(d.next_app).as_bool() {
                    let _ = SetForegroundWindow(d.next_app);
                } else if let Ok(progman) = FindWindowW(w!("Progman"), PCWSTR::null()) {
                    let _ = SetForegroundWindow(progman);
                }
            }
        }

        {
            let mut s = state();
            let key = d.real.0 as isize;
            if s.anim_active.get(&key).is_some_and(|a| a.token == d.token) {
                s.anim_active.remove(&key);
            }
        }
        let _ = CloseHandle(d.done);
        if !d.revealed.is_invalid() {
            let _ = CloseHandle(d.revealed);
        }
    }
}

/// Returns true if the host took the animation. `original_ex_style` is the window's
/// TRUE extended style captured before any WS_EX_LAYERED was added for a hide;
/// `cloak_hidden` says how a rising caller hid the window. The deferred* args drive
/// the auto-hide path (falling only): the caller cloaks after our first frame and the
/// worker does the real minimize at the end.
fn start_anim(hwnd: HWND, rising: bool, original_ex_style: isize, cloak_hidden: bool, deferred_minimize: bool, requested_unhide: bool, next_app: HWND) -> bool {
    let bail = || -> bool {
        if rising {
            undo_rising_hide(hwnd, original_ex_style, cloak_hidden);
        } else {
            set_dwm_transitions(hwnd, true);
        }
        false
    };

    if is_excluded(hwnd) {
        return bail();
    }
    let Some(host) = host_ipc_window() else { return bail() };

    // DWM extended frame bounds: GetWindowRect includes the invisible resize borders,
    // the extended frame is where the pixels really are. The snapshot is the full
    // window rect; the host crops it to the extended frame. The crop is computed in
    // physical pixels and sent as fractions, so it also fits a snapshot PrintWindow
    // renders at a DPI-virtualized size. The capture itself runs in the thread's own
    // DPI context.
    let (mut phys, mut ext) = (RECT::default(), RECT::default());
    unsafe {
        let _dpi = DpiScope::new();
        if GetWindowRect(hwnd, &mut phys).is_err() {
            return bail();
        }
        if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut ext as *mut _ as *mut c_void, core::mem::size_of::<RECT>() as u32).is_err() {
            ext = phys;
        }
    }
    let mut win = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut win) }.is_err() {
        return bail();
    }
    let (raw_w, raw_h) = (win.right - win.left, win.bottom - win.top);
    let (phys_w, phys_h) = (phys.right - phys.left, phys.bottom - phys.top);
    let crop_l = (ext.left - phys.left).max(0);
    let crop_t = (ext.top - phys.top).max(0);
    let crop_r = (ext.right - phys.left).min(phys_w);
    let crop_b = (ext.bottom - phys.top).min(phys_h);
    if raw_w <= 0 || raw_h <= 0 || phys_w <= 0 || phys_h <= 0 || crop_r - crop_l <= 0 || crop_b - crop_t <= 0 {
        return bail();
    }

    if UNLOADING.load(Ordering::Relaxed) {
        return bail();
    }

    // One genie per window and direction. Any window we animate also counts as "seen",
    // so the launch path won't fire on it later.
    let key = hwnd.0 as isize;
    let token = {
        let mut s = state();
        if s.anim_active.get(&key).is_some_and(|a| a.rising == rising) {
            // Duplicate of the genie already in flight: it owns the window's hide and
            // transitions and finishes them; nothing to undo here.
            return false;
        }
        s.next_token += 1;
        let token = s.next_token;
        s.anim_active.insert(key, ActiveAnim { token, rising });
        s.launch_seen.insert(key);
        token
    };
    let release = || {
        let mut s = state();
        if s.anim_active.get(&key).is_some_and(|a| a.token == token) {
            s.anim_active.remove(&key);
        }
    };

    let event = || unsafe { CreateEventW(None, true, false, PCWSTR::null()).unwrap_or_default() };
    let done = event();
    let revealed = if rising { event() } else { HANDLE::default() };
    let first = if rising { HANDLE::default() } else { event() };
    let close_events = || unsafe {
        for h in [done, revealed, first] {
            if !h.is_invalid() {
                let _ = CloseHandle(h);
            }
        }
    };
    if done.is_invalid() || (rising && revealed.is_invalid()) || (!rising && first.is_invalid()) {
        close_events();
        release();
        return bail();
    }

    // Opening and closing are their own kinds of request, with their own effect and
    // duration (the paths that start them set the source).
    let source = SOURCE.with(|c| c.get());
    let kind = if source == SRC_LAUNCH {
        ANIM_LAUNCH
    } else if source == SRC_CLOSE {
        ANIM_CLOSE
    } else if rising {
        ANIM_RESTORE
    } else {
        ANIM_MINIMIZE
    };
    let duration = match kind {
        ANIM_LAUNCH => setting(|s| s.open_duration_ms),
        ANIM_CLOSE => setting(|s| s.close_duration_ms),
        _ => duration_ms(),
    };

    // The worker is created up front (suspended), so once the host has accepted the
    // request there is always someone to finish it.
    let td = Box::into_raw(Box::new(Teardown {
        real: hwnd,
        rising,
        original_ex_style,
        hidden_by_cloak: cloak_hidden,
        deferred_minimize,
        requested_unhide,
        next_app,
        duration_ms: duration,
        unhide_duration_ms: unhide_duration_ms(),
        done,
        revealed,
        token,
        cancelled: AtomicI32::new(0),
    }));
    let Some(worker) = spawn_worker(teardown_thread, td as *mut c_void, true) else {
        drop(unsafe { Box::from_raw(td) });
        close_events();
        release();
        return bail();
    };

    let mut req = AnimRequest {
        version: IPC_VERSION,
        kind,
        hwnd: key as u64,
        pid: unsafe { GetCurrentProcessId() },
        raw_w,
        raw_h,
        crop_u0: crop_l as f32 / phys_w as f32,
        crop_v0: crop_t as f32 / phys_h as f32,
        crop_u1: crop_r as f32 / phys_w as f32,
        crop_v1: crop_b as f32 / phys_h as f32,
        h_first_frame: first.0 as u64,
        h_done: done.0 as u64,
        h_revealed: revealed.0 as u64,
        flags: (source << REQ_SOURCE_SHIFT) & REQ_SOURCE_MASK,
        ..Default::default()
    };
    if unsafe { InSendMessageEx(None) } & ISMEX_SEND != 0 {
        req.flags |= REQ_SENT;
    }

    // Falling: capture now, while the window is still on screen. Rising: the host may
    // still hold the snapshot from the minimize; capture only if it asks.
    let (mut c0, mut c1, mut freq) = (0i64, 0i64, 1i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut c0);
    }
    // DockSide's own windows are layered: PrintWindow sees only black there, and the
    // host has their pixels anyway.
    let own_pixels = is_host_app_window(hwnd);
    let mut section = if rising || own_pixels { None } else { capture_to_section(hwnd, raw_w, raw_h) };
    if let Some(sec) = section {
        unsafe {
            let _ = QueryPerformanceCounter(&mut c1);
            let _ = QueryPerformanceFrequency(&mut freq);
        }
        let ms = ((c1 - c0).max(0) as u64 * 1000 / freq.max(1) as u64).min(0xFFFF) as u32;
        req.flags |= REQ_HAS_SNAPSHOT | (ms << REQ_CAPTURE_MS_SHIFT);
        req.h_section = sec.0 as u64;
    }
    let mut reply = send_anim_request(host, &req);
    if rising && reply == REPLY_NEED_SNAPSHOT && !own_pixels {
        // No cached minimize snapshot (e.g. minimized before DockSide started): capture
        // the freshly restored, still cloaked window.
        section = capture_to_section(hwnd, raw_w, raw_h);
        if let Some(sec) = section {
            req.flags |= REQ_HAS_SNAPSHOT;
            req.h_section = sec.0 as u64;
            reply = send_anim_request(host, &req);
        } else {
            reply = REPLY_FAIL;
        }
    }
    if let Some(sec) = section {
        unsafe {
            let _ = CloseHandle(sec); // the host has its own duplicate now
        }
    }

    if reply != REPLY_OK {
        unsafe {
            (*td).cancelled.store(1, Ordering::Release);
            ResumeThread(worker);
        }
        close_events();
        release();
        return bail();
    }

    unsafe {
        ResumeThread(worker); // owns done / revealed from here on
        if !first.is_invalid() {
            // Hold the real minimize back until the genie's first frame is on screen,
            // otherwise the window would vanish a few frames before the genie appears.
            WaitForSingleObject(first, 200); // capped: worst case = no sync
            let _ = CloseHandle(first);
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Close animation: a window of this thread is about to be destroyed or hidden while on
// screen. The snapshot is taken now; the host plays the close effect over the spot the
// window leaves, starting before it goes (start_anim waits for the first frame).
// ---------------------------------------------------------------------------

/// Returns true if the close is animated (Windows' own close transition is then off).
fn try_close_anim(hwnd: HWND) -> bool {
    if hwnd.is_invalid() || !close_enabled() || UNLOADING.load(Ordering::Relaxed) {
        return false;
    }
    unsafe {
        // Only the window's own thread: PrintWindow from another one sends it WM_PRINT
        // and could wait on a thread that is waiting on us.
        if GetWindowThreadProcessId(hwnd, None) != GetCurrentThreadId() {
            return false;
        }
        if !IsWindowVisible(hwnd).as_bool() || real_is_iconic(hwnd) || !is_launch_window(hwnd) {
            return false;
        }
        // Cloaked: not on screen (another virtual desktop, a suspended app, our own hide).
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut c_void, 4).is_ok() && cloaked != 0 {
            return false;
        }
    }
    if state().anim_active.contains_key(&(hwnd.0 as isize)) {
        return false; // already minimizing / opening / closing
    }
    let _src = SourceScope::new(SRC_CLOSE);
    set_dwm_transitions(hwnd, false);
    let animated = start_anim(hwnd, false, unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) }, false, false, false, HWND::default());
    if animated && is_host_app_window(hwnd) {
        // DockSide's layered windows flash black when destroyed at full opacity: the
        // genie's first frame covers the window now, so take it out of composition.
        set_cloak(hwnd, true);
    }
    animated
}

/// After an animated hide: the next show of the window opens with the open effect
/// again (programs that close to the tray and come back).
fn forget_shown(hwnd: HWND) {
    state().launch_seen.remove(&(hwnd.0 as isize));
}

// ---------------------------------------------------------------------------
// Launch animation: the first show of a window (or a show after an animated hide)
// ---------------------------------------------------------------------------

struct LaunchData {
    hwnd: HWND,
    original_ex_style: isize,
    hidden_by_cloak: bool,
}

struct GeckoSearch {
    this: HWND,
    pid: u32,
    found: bool,
}

fn class_is(hwnd: HWND, name: &str) -> bool {
    let mut cls = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut cls) } as usize;
    n > 0 && String::from_utf16_lossy(&cls[..n]).eq_ignore_ascii_case(name)
}

unsafe extern "system" fn find_other_gecko_window(w: HWND, lp: LPARAM) -> BOOL {
    let s = &mut *(lp.0 as *mut GeckoSearch);
    if w == s.this || (!IsWindowVisible(w).as_bool() && !real_is_iconic(w)) || GetAncestor(w, GA_ROOT) != w {
        return BOOL(1);
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(w, Some(&mut pid));
    if pid == s.pid && class_is(w, "MozillaWindowClass") {
        s.found = true;
        return BOOL(0);
    }
    BOOL(1)
}

/// Firefox-family browsers: is `hwnd` the process's first browser window?
fn is_first_gecko_window(hwnd: HWND) -> bool {
    if !class_is(hwnd, "MozillaWindowClass") {
        return false;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let mut search = GeckoSearch { this: hwnd, pid, found: false };
    unsafe {
        let _ = EnumWindows(Some(find_other_gecko_window), LPARAM(&mut search as *mut _ as isize));
    }
    !search.found
}

/// Arm the launch animation before the real show call: check eligibility, kill the
/// DWM transition and hide the window (DWM cloak; the layered hide only for a first
/// Firefox-family window, an app-cloaked window, or when cloaking is refused).
/// Returns (original ex style, cloaked).
fn launch_prepare(hwnd: HWND, cmd: i32) -> Option<(isize, bool)> {
    if UNLOADING.load(Ordering::Relaxed) || !launch_enabled() {
        return None;
    }
    if cmd != SW_SHOW.0 && cmd != SW_SHOWNORMAL.0 && cmd != SW_SHOWDEFAULT.0 && cmd != SW_SHOWMAXIMIZED.0 {
        return None;
    }
    unsafe {
        if IsWindowVisible(hwnd).as_bool() || real_is_iconic(hwnd) {
            return None;
        }
    }
    // DockSide's own windows zoom in by themselves.
    if !is_launch_window(hwnd) || is_host_app_window(hwnd) {
        return None;
    }
    if !state().launch_seen.insert(hwnd.0 as isize) {
        return None;
    }
    if is_excluded(hwnd) {
        return None;
    }
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    set_dwm_transitions(hwnd, false);

    unsafe {
        let mut cloaked = 0u32;
        let app_cloaked = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut c_void, 4).is_ok() && cloaked & DWM_CLOAKED_APP != 0;
        let cloak = BOOL(1);
        if !app_cloaked && !is_first_gecko_window(hwnd) && DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &cloak as *const _ as *const c_void, 4).is_ok() {
            return Some((ex, true));
        }
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
    }
    Some((ex, false))
}

fn launch_commit(hwnd: HWND, original_ex_style: isize, cloaked: bool) {
    let ld = Box::into_raw(Box::new(LaunchData { hwnd, original_ex_style, hidden_by_cloak: cloaked }));
    if spawn_worker(launch_thread, ld as *mut c_void, false).is_none() {
        drop(unsafe { Box::from_raw(ld) });
        undo_rising_hide(hwnd, original_ex_style, cloaked);
    }
}

unsafe extern "system" fn launch_thread(param: *mut c_void) -> u32 {
    let ld = Box::from_raw(param as *mut LaunchData);
    let _ = catch_unwind(AssertUnwindSafe(|| launch(&ld)));
    0
}

fn launch(ld: &LaunchData) {
    let hwnd = ld.hwnd;
    unsafe {
        // Let the freshly shown (still hidden) window paint; UWP apps stay cloaked by
        // the shell behind their splash for a while - wait (bounded) for that cloak to
        // lift. Our own cloak reports as DWM_CLOAKED_APP, so that bit is masked out.
        let own_cloak = if ld.hidden_by_cloak { DWM_CLOAKED_APP } else { 0 };
        Sleep(60);
        for _ in 0..30 {
            if !IsWindow(hwnd).as_bool() || UNLOADING.load(Ordering::Relaxed) {
                break;
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut c_void, 4).is_err() || cloaked & !own_cloak == 0 {
                break;
            }
            Sleep(50);
        }

        if UNLOADING.load(Ordering::Relaxed) || !IsWindow(hwnd).as_bool() || real_is_iconic(hwnd) || !IsWindowVisible(hwnd).as_bool() {
            if IsWindow(hwnd).as_bool() {
                undo_rising_hide(hwnd, ld.original_ex_style, ld.hidden_by_cloak);
            }
            return;
        }
    }
    let _src = SourceScope::new(SRC_LAUNCH);
    start_anim(hwnd, true, ld.original_ex_style, ld.hidden_by_cloak, false, false, HWND::default());
}

// ---------------------------------------------------------------------------
// Hooks. Each has a pass-through fast path that calls the original; the slow paths
// hold a HookScope and never let a panic out (they fall back to the original).
// ---------------------------------------------------------------------------

// OwnStateChange only around the paths that animate: any other state change (e.g.
// SW_SHOWNOACTIVATE on a minimized window, which restores it) is left to HCBT_MINMAX.
fn show_window_slow(hwnd: HWND, cmd: i32) -> BOOL {
    let _scope = HookScope::new();
    let _src = SourceScope::new(SRC_SHOWWINDOW);
    if is_minimize_cmd(cmd) {
        // Another thread's window whose process animates its own (Explorer's "Show
        // desktop" minimizes every window like this, one call after another): hand the
        // minimize over to the window's thread, where HCBT_MINMAX animates it. Done here,
        // each call would wait for its genie's first frame and the genies would start one
        // by one, with Explorer deaf to a second press meanwhile.
        let mut pid = 0u32;
        if unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) != GetCurrentThreadId() } && peer_animates_itself(pid) {
            let was_visible = unsafe { IsWindowVisible(hwnd) };
            add_pending_minimize(hwnd);
            if !show_window_async_original(hwnd, cmd).as_bool() {
                forget_pending_minimize(hwnd);
            }
            return was_visible;
        }
        let _own = OwnStateChange::new();
        if should_animate(hwnd) {
            set_dwm_transitions(hwnd, false);
            start_anim(hwnd, false, unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) }, false, false, false, HWND::default());
        }
        return show_window_original(hwnd, cmd);
    }

    if (cmd == SW_RESTORE.0 || cmd == SW_SHOWNORMAL.0) && real_is_iconic(hwnd) {
        if restore_enabled() && should_animate(hwnd) {
            let _own = OwnStateChange::new();
            set_dwm_transitions(hwnd, false);
            set_cloak(hwnd, true);
            let res = show_window_original(hwnd, cmd);
            start_anim(hwnd, true, unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) }, true, false, false, HWND::default());
            return res;
        }
        return show_window_original(hwnd, cmd);
    }

    if cmd == SW_HIDE.0 {
        let animated = try_close_anim(hwnd);
        let res = show_window_original(hwnd, cmd);
        if animated {
            set_cloak(hwnd, false); // hidden now; a later show must be seen
            forget_shown(hwnd);
        }
        return res;
    }

    if let Some((ex, cloaked)) = launch_prepare(hwnd, cmd) {
        let _own = OwnStateChange::new();
        let res = show_window_original(hwnd, cmd);
        launch_commit(hwnd, ex, cloaked);
        return res;
    }
    show_window_original(hwnd, cmd)
}

unsafe extern "system" fn show_window_hook(hwnd: HWND, cmd: i32) -> BOOL {
    if !is_minimize_cmd(cmd) {
        forget_pending_minimize(hwnd);
    }
    if !active_for(hwnd) {
        return show_window_original(hwnd, cmd);
    }
    catch_unwind(|| show_window_slow(hwnd, cmd)).unwrap_or_else(|_| show_window_original(hwnd, cmd))
}

fn def_window_proc_slow(original: DefWindowProcFn, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let _scope = HookScope::new();
    let _own = OwnStateChange::new();
    let _src = SourceScope::new(SRC_DEFWINDOWPROC);
    unsafe {
        if msg == WM_DESTROY {
            state().launch_seen.remove(&(hwnd.0 as isize));
            return original(hwnd, msg, wp, lp);
        }
        if msg == WM_CLOSE {
            // The default close: DefWindowProc destroys the window right away.
            if active_for(hwnd) {
                try_close_anim(hwnd);
            }
            return original(hwnd, msg, wp, lp);
        }

        let cmd = (wp.0 & 0xFFF0) as u32;
        if cmd == SC_MINIMIZE {
            // Our own deferred minimize (auto-hide path): pass straight through.
            if !GetPropW(hwnd, BYPASS_PROP).is_invalid() {
                return original(hwnd, msg, wp, lp);
            }
            if active_for(hwnd) && should_animate(hwnd) {
                // Sent from another thread (Explorer's "Show desktop" sends it to one
                // window after another): WM_SYSCOMMAND's result means nothing, so let the
                // sender go on now instead of waiting out our capture and the first frame.
                release_cross_thread_sender();
                // Auto-hide taskbar? Reveal it and defer the real minimize until the
                // animation finishes, so it doesn't slide away mid-genie.
                let mut requested_unhide = false;
                let mut next = HWND::default();
                if unhide_enabled() {
                    let mut tray = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).unwrap_or_default();
                    let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                    if !tray.is_invalid() && MonitorFromWindow(tray, MONITOR_DEFAULTTOPRIMARY) != mon {
                        let mut sec = HWND::default();
                        while let Ok(s) = FindWindowExW(None, sec, w!("Shell_SecondaryTrayWnd"), PCWSTR::null()) {
                            sec = s;
                            if MonitorFromWindow(s, MONITOR_DEFAULTTONULL) == mon {
                                tray = s;
                                break;
                            }
                        }
                    }
                    if !tray.is_invalid() && taskbar_state(tray) & ABS_AUTOHIDE != 0 {
                        requested_unhide = true;
                        let mut it = GetWindow(hwnd, GW_HWNDNEXT).unwrap_or_default();
                        while !it.is_invalid() {
                            if IsWindowVisible(it).as_bool() && !real_is_iconic(it) && GetAncestor(it, GA_ROOT) == it && GetWindowTextLengthW(it) > 0 && it != hwnd && it != tray {
                                if GetWindowLongPtrW(it, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize == 0 {
                                    next = it;
                                    break;
                                }
                            }
                            it = GetWindow(it, GW_HWNDNEXT).unwrap_or_default();
                        }
                        let _ = SetForegroundWindow(tray);
                    }
                }

                set_dwm_transitions(hwnd, false);
                if requested_unhide {
                    if start_anim(hwnd, false, GetWindowLongPtrW(hwnd, GWL_EXSTYLE), false, true, true, next) {
                        // First frame is up: hide the real window flash-free; the worker
                        // minimizes it at the end.
                        set_cloak(hwnd, true);
                        return LRESULT(0);
                    }
                    return original(hwnd, msg, wp, lp);
                }
                start_anim(hwnd, false, GetWindowLongPtrW(hwnd, GWL_EXSTYLE), false, false, false, HWND::default());
            }
            return original(hwnd, msg, wp, lp);
        }

        if cmd == SC_RESTORE && real_is_iconic(hwnd) && active_for(hwnd) && restore_enabled() && should_animate(hwnd) {
            set_dwm_transitions(hwnd, false);
            set_cloak(hwnd, true);
            let res = original(hwnd, msg, wp, lp);
            release_cross_thread_sender(); // restored: the sender need not wait for the genie
            start_anim(hwnd, true, GetWindowLongPtrW(hwnd, GWL_EXSTYLE), true, false, false, HWND::default());
            return res;
        }
        original(hwnd, msg, wp, lp)
    }
}

fn def_window_proc_interesting(msg: u32, wp: WPARAM) -> bool {
    if msg == WM_DESTROY || msg == WM_CLOSE {
        return true;
    }
    if msg != WM_SYSCOMMAND {
        return false;
    }
    let cmd = (wp.0 & 0xFFF0) as u32;
    cmd == SC_MINIMIZE || cmd == SC_RESTORE
}

// Called for every message of every window: the fast path is a few compares and a call.
unsafe extern "system" fn def_window_proc_w_hook(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let original = orig::<DefWindowProcFn>(&DEF_WINDOW_PROC_W);
    if !def_window_proc_interesting(msg, wp) {
        return original(hwnd, msg, wp, lp);
    }
    catch_unwind(|| def_window_proc_slow(original, hwnd, msg, wp, lp)).unwrap_or_else(|_| original(hwnd, msg, wp, lp))
}

unsafe extern "system" fn def_window_proc_a_hook(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let original = orig::<DefWindowProcFn>(&DEF_WINDOW_PROC_A);
    if !def_window_proc_interesting(msg, wp) {
        return original(hwnd, msg, wp, lp);
    }
    catch_unwind(|| def_window_proc_slow(original, hwnd, msg, wp, lp)).unwrap_or_else(|_| original(hwnd, msg, wp, lp))
}

/// Custom-title-bar apps (Zed/GPUI, some Electron) minimize via ShowWindowAsync,
/// SetWindowPlacement or CloseWindow (no auto-hide handling on these paths).
fn try_minimize_anim(hwnd: HWND) {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || real_is_iconic(hwnd) {
            return;
        }
    }
    if !should_animate(hwnd) {
        return;
    }
    set_dwm_transitions(hwnd, false);
    start_anim(hwnd, false, unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) }, false, false, false, HWND::default());
}

// Only the first-show (launch) case. An async minimize / restore runs later, in the
// window's own thread: the HCBT_MINMAX there animates it (on_min_max). Doing it here
// instead held the caller up per window - Explorer's "Show desktop" started its genies
// one after another and restored windows without any.
fn show_window_async_slow(hwnd: HWND, cmd: i32) -> BOOL {
    let _scope = HookScope::new();
    let _src = SourceScope::new(SRC_LAUNCH);
    if let Some((ex, cloaked)) = launch_prepare(hwnd, cmd) {
        let res = show_window_async_original(hwnd, cmd);
        launch_commit(hwnd, ex, cloaked);
        return res;
    }
    show_window_async_original(hwnd, cmd)
}

unsafe extern "system" fn show_window_async_hook(hwnd: HWND, cmd: i32) -> BOOL {
    if !is_minimize_cmd(cmd) {
        forget_pending_minimize(hwnd);
    }
    if is_minimize_cmd(cmd) || !active_for(hwnd) || !launch_enabled() {
        return show_window_async_original(hwnd, cmd);
    }
    catch_unwind(|| show_window_async_slow(hwnd, cmd)).unwrap_or_else(|_| show_window_async_original(hwnd, cmd))
}

unsafe extern "system" fn set_window_placement_hook(hwnd: HWND, wp: *const WINDOWPLACEMENT) -> BOOL {
    let original = orig::<SetWindowPlacementFn>(&SET_WINDOW_PLACEMENT);
    let cmd = if wp.is_null() { -1 } else { (*wp).showCmd as i32 };
    if !wp.is_null() && !is_minimize_cmd(cmd) {
        forget_pending_minimize(hwnd);
    }
    if wp.is_null() || !is_minimize_cmd(cmd) || !active_for(hwnd) {
        return original(hwnd, wp);
    }
    let _ = catch_unwind(|| {
        let _scope = HookScope::new();
        let _own = OwnStateChange::new();
        let _src = SourceScope::new(SRC_SETWINDOWPLACEMENT);
        try_minimize_anim(hwnd);
    });
    let _own = OwnStateChange::new();
    original(hwnd, wp)
}

unsafe extern "system" fn close_window_hook(hwnd: HWND) -> BOOL {
    let original = orig::<CloseWindowFn>(&CLOSE_WINDOW);
    if !active_for(hwnd) {
        return original(hwnd);
    }
    let _ = catch_unwind(|| {
        let _scope = HookScope::new();
        let _own = OwnStateChange::new();
        let _src = SourceScope::new(SRC_CLOSEWINDOW);
        try_minimize_anim(hwnd);
    });
    let _own = OwnStateChange::new();
    original(hwnd)
}

// Store / UWP apps show their first window via SetWindowPos + SWP_SHOWWINDOW.
fn set_window_pos_slow(original: SetWindowPosFn, hwnd: HWND, after: HWND, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> BOOL {
    let _scope = HookScope::new();
    unsafe {
        if flags & SWP_HIDEWINDOW.0 != 0 {
            let animated = try_close_anim(hwnd);
            let res = original(hwnd, after, x, y, cx, cy, flags);
            if animated {
                set_cloak(hwnd, false); // hidden now; a later show must be seen
                forget_shown(hwnd);
            }
            return res;
        }
        let _src = SourceScope::new(SRC_LAUNCH);
        if let Some((ex, cloaked)) = launch_prepare(hwnd, SW_SHOW.0) {
            let res = original(hwnd, after, x, y, cx, cy, flags);
            launch_commit(hwnd, ex, cloaked);
            return res;
        }
        original(hwnd, after, x, y, cx, cy, flags)
    }
}

// Hot: most SetWindowPos calls are moves / resizes / z-order changes.
unsafe extern "system" fn set_window_pos_hook(hwnd: HWND, after: HWND, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> BOOL {
    let original = orig::<SetWindowPosFn>(&SET_WINDOW_POS);
    let show_hide = flags & (SWP_SHOWWINDOW.0 | SWP_HIDEWINDOW.0);
    if show_hide == 0 || !active_for(hwnd) || !(if flags & SWP_SHOWWINDOW.0 != 0 { launch_enabled() } else { close_enabled() }) {
        return original(hwnd, after, x, y, cx, cy, flags);
    }
    catch_unwind(|| set_window_pos_slow(original, hwnd, after, x, y, cx, cy, flags)).unwrap_or_else(|_| original(hwnd, after, x, y, cx, cy, flags))
}

// The window is going away: close effect first (DestroyWindow is also what a default
// WM_CLOSE ends in; that one is caught in DefWindowProc already).
unsafe extern "system" fn destroy_window_hook(hwnd: HWND) -> BOOL {
    let original = orig::<DestroyWindowFn>(&DESTROY_WINDOW);
    if !active_for(hwnd) || !close_enabled() {
        return original(hwnd);
    }
    let _ = catch_unwind(|| {
        let _scope = HookScope::new();
        try_close_anim(hwnd);
    });
    original(hwnd)
}

// See State::pending_min. Hot (Explorer's taskbar asks all the time): one load and a
// call unless a handed-over minimize is in flight.
unsafe extern "system" fn is_iconic_hook(hwnd: HWND) -> BOOL {
    let original = orig::<IsIconicFn>(&IS_ICONIC);
    if PENDING_COUNT.load(Ordering::Relaxed) == 0 {
        return original(hwnd);
    }
    let _scope = HookScope::new();
    if original(hwnd).as_bool() {
        forget_pending_minimize(hwnd);
        return BOOL(1);
    }
    BOOL(catch_unwind(|| pending_minimize(hwnd)).unwrap_or(false) as i32)
}

// Explorer saves every window's placement after minimizing them and restores from it:
// a handed-over window must already look minimized (and remember being maximized).
unsafe extern "system" fn get_window_placement_hook(hwnd: HWND, wp: *mut WINDOWPLACEMENT) -> BOOL {
    let original = orig::<GetWindowPlacementFn>(&GET_WINDOW_PLACEMENT);
    if PENDING_COUNT.load(Ordering::Relaxed) == 0 || wp.is_null() {
        return original(hwnd, wp);
    }
    let _scope = HookScope::new();
    let ok = original(hwnd, wp);
    if ok.as_bool() && (*wp).showCmd != SW_SHOWMINIMIZED.0 as u32 && catch_unwind(|| pending_minimize(hwnd)).unwrap_or(false) {
        if (*wp).showCmd == SW_SHOWMAXIMIZED.0 as u32 {
            (*wp).flags |= WPF_RESTORETOMAXIMIZED;
        }
        (*wp).showCmd = SW_SHOWMINIMIZED.0 as u32;
    }
    ok
}

// A restore seen in HCBT_MINMAX happens right after the notification returns: wait for
// it (the window is cloaked meanwhile), then start the rising genie.
struct PendingRestore {
    hwnd: HWND,
    ex_style: isize,
}

/// WS_MINIMIZE goes a moment before the window leaves the minimized spot (-32000) for
/// its real rect; a genie started in between would use the minimized caption's size.
fn restore_settled(hwnd: HWND) -> bool {
    if real_is_iconic(hwnd) {
        return false;
    }
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut r).is_ok() && r.left > -32000 && r.top > -32000 }
}

unsafe extern "system" fn pending_restore_thread(param: *mut c_void) -> u32 {
    let p = Box::from_raw(param as *mut PendingRestore);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let hwnd = p.hwnd;
        for _ in 0..60 {
            if !IsWindow(hwnd).as_bool() || restore_settled(hwnd) || UNLOADING.load(Ordering::SeqCst) {
                break;
            }
            Sleep(5);
        }
        if !IsWindow(hwnd).as_bool() {
            return;
        }
        if !restore_settled(hwnd) || UNLOADING.load(Ordering::SeqCst) {
            undo_rising_hide(hwnd, p.ex_style, true); // the restore never came
            return;
        }
        let _src = SourceScope::new(SRC_CBT);
        start_anim(hwnd, true, p.ex_style, true, false, false, HWND::default());
    }));
    0
}

/// HCBT_MINMAX: called in the window's own thread right before it is minimized /
/// restored, whoever asked for it (also ShowWindowAsync from Explorer's "Show desktop").
pub fn on_min_max(hwnd: HWND, cmd: i32) {
    if OWN_STATE_CHANGE.with(|c| c.get()) > 0 {
        return; // one of our hooked calls on this thread
    }
    if !active_for(hwnd) {
        return;
    }
    let _scope = HookScope::new();
    let _src = SourceScope::new(SRC_CBT);
    unsafe {
        if is_minimize_cmd(cmd) || cmd == SW_FORCEMINIMIZE.0 {
            if real_is_iconic(hwnd) || !IsWindowVisible(hwnd).as_bool() || !should_animate(hwnd) {
                return;
            }
            // Waits for the genie's first frame, then the minimize goes ahead.
            set_dwm_transitions(hwnd, false);
            start_anim(hwnd, false, GetWindowLongPtrW(hwnd, GWL_EXSTYLE), false, false, false, HWND::default());
            return;
        }
        // Any other command on a minimized window restores it (SW_RESTORE,
        // SW_SHOWNOACTIVATE, SW_SHOWNORMAL, ...), maximized windows included.
        if !real_is_iconic(hwnd) || !restore_enabled() || !should_animate(hwnd) {
            return;
        }
        if state().anim_active.get(&(hwnd.0 as isize)).is_some_and(|a| a.rising) {
            return; // already coming back
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        set_dwm_transitions(hwnd, false);
        set_cloak(hwnd, true);
        let p = Box::into_raw(Box::new(PendingRestore { hwnd, ex_style: ex }));
        if spawn_worker(pending_restore_thread, p as *mut c_void, false).is_none() {
            drop(Box::from_raw(p));
            undo_rising_hide(hwnd, ex, true);
        }
    }
}

pub fn install_hooks() -> bool {
    create_user32_hook(b"DefWindowProcW\0", def_window_proc_w_hook as *mut c_void, &DEF_WINDOW_PROC_W)
        && create_user32_hook(b"DefWindowProcA\0", def_window_proc_a_hook as *mut c_void, &DEF_WINDOW_PROC_A)
        && create_user32_hook(b"ShowWindow\0", show_window_hook as *mut c_void, &SHOW_WINDOW)
        && create_user32_hook(b"ShowWindowAsync\0", show_window_async_hook as *mut c_void, &SHOW_WINDOW_ASYNC)
        && create_user32_hook(b"SetWindowPlacement\0", set_window_placement_hook as *mut c_void, &SET_WINDOW_PLACEMENT)
        && create_user32_hook(b"CloseWindow\0", close_window_hook as *mut c_void, &CLOSE_WINDOW)
        && create_user32_hook(b"SetWindowPos\0", set_window_pos_hook as *mut c_void, &SET_WINDOW_POS)
        && create_user32_hook(b"DestroyWindow\0", destroy_window_hook as *mut c_void, &DESTROY_WINDOW)
        && create_user32_hook(b"IsIconic\0", is_iconic_hook as *mut c_void, &IS_ICONIC)
        && create_user32_hook(b"GetWindowPlacement\0", get_window_placement_hook as *mut c_void, &GET_WINDOW_PLACEMENT)
}

/// Frees the tables' memory too: a DLL that is shut down may stay mapped in the
/// program for a long time.
pub fn on_shutdown() {
    *state() = State::default();
    PENDING_COUNT.store(0, Ordering::Relaxed);
}
