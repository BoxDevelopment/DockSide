//! Activation in a process: maps the host's settings, installs the inline hooks
//! and watches the host, switching everything off again when it shuts down (or dies).
//!
//! Unlike a C++ hook (whose pass-through fast paths are guaranteed tail calls,
//! leaving no frame of the DLL on any stack), a Rust detour's fast path can still be
//! on a stack - a modal move loop runs inside DefWindowProc - so the DLL never
//! unmaps itself: it pins its module and, on shutdown, only switches the hooks off.

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN};
use windows::Win32::System::Memory::*;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::*;

use crate::minhook::*;
use crate::shared::*;
use crate::{genie, ACTIVE_CALLS, UNLOADING};

const UNINIT: i32 = 0; // nothing installed yet
const INITING: i32 = 1; // one thread is activating
const ACTIVE: i32 = 2; // hooks installed, watcher running
const WAIT_HOST: i32 = 3; // shut down cleanly; re-activates for a new host instance
const DEAD: i32 = 4; // never activate in this process (failed shutdown)

static STATE: AtomicI32 = AtomicI32::new(UNINIT);
static IS_HOST_PROCESS: AtomicBool = AtomicBool::new(false);

static SETTINGS_MAP: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static SETTINGS: AtomicPtr<SharedSettings> = AtomicPtr::new(core::ptr::null_mut());
static WAIT_INSTANCE: AtomicU32 = AtomicU32::new(0); // host instance we shut down for (WAIT_HOST)
static NEXT_PEEK: AtomicU64 = AtomicU64::new(0); // WAIT_HOST: earliest GetTickCount64 of the next look

// Handles as integers (HANDLE isn't Send).
static H_SHUTDOWN: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut()); // host's per-instance manual-reset event
static H_ALIVE: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut()); // host's per-instance mutex, abandoned if it crashes
static H_STOP: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut()); // local: wakes our workers at shutdown
static H_ACTIVE_MARK: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut()); // active_name() for this process

static SELF_NAMES: Mutex<ImageNames> = Mutex::new(ImageNames::EMPTY);
static ALLOWED_GEN: AtomicU32 = AtomicU32::new(u32::MAX);
static ALLOWED: AtomicBool = AtomicBool::new(false);

static WORKERS: Mutex<Vec<isize>> = Mutex::new(Vec::new());

fn h(p: &AtomicPtr<c_void>) -> HANDLE {
    HANDLE(p.load(Ordering::Acquire))
}

fn close(p: &AtomicPtr<c_void>) {
    let v = p.swap(core::ptr::null_mut(), Ordering::AcqRel);
    if !v.is_null() {
        unsafe {
            let _ = CloseHandle(HANDLE(v));
        }
    }
}

fn map_settings() -> bool {
    if !SETTINGS.load(Ordering::Acquire).is_null() {
        return true;
    }
    let name = wide(SETTINGS_SECTION);
    unsafe {
        let Ok(map) = OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())) else { return false };
        let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, core::mem::size_of::<SharedSettings>());
        let s = view.Value as *mut SharedSettings;
        if s.is_null() || (*s).magic != SETTINGS_MAGIC || (*s).size as usize != core::mem::size_of::<SharedSettings>() {
            if !s.is_null() {
                let _ = UnmapViewOfFile(view);
            }
            let _ = CloseHandle(map);
            return false;
        }
        SETTINGS_MAP.store(map.0, Ordering::Release);
        SETTINGS.store(s, Ordering::Release);
    }
    true
}

/// The section is only held while active: a DLL that is shut down (but not unloaded)
/// must not keep an elevated host's section alive, or the next host - maybe not
/// elevated - could not create its own.
fn unmap_settings() {
    let s = SETTINGS.swap(core::ptr::null_mut(), Ordering::AcqRel);
    unsafe {
        if !s.is_null() {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: s as *mut c_void });
        }
    }
    close(&SETTINGS_MAP);
}

/// Instance of the host that published the section right now (no lasting mapping).
fn peek_host_instance() -> Option<u32> {
    let name = wide(SETTINGS_SECTION);
    unsafe {
        let map = OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())).ok()?;
        let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, core::mem::size_of::<SharedSettings>());
        let s = view.Value as *const SharedSettings;
        let ok = !s.is_null() && (*s).magic == SETTINGS_MAGIC && (*s).size as usize == core::mem::size_of::<SharedSettings>();
        let instance = ok.then(|| (*s).host_instance);
        if !s.is_null() {
            let _ = UnmapViewOfFile(view);
        }
        let _ = CloseHandle(map);
        instance
    }
}

fn workers_finished() -> bool {
    let w = WORKERS.lock().unwrap_or_else(|e| e.into_inner());
    w.iter().all(|&t| unsafe { WaitForSingleObject(HANDLE(t as _), 0) } == WAIT_OBJECT_0)
}

fn close_workers() {
    let mut w = WORKERS.lock().unwrap_or_else(|e| e.into_inner());
    for &t in w.iter() {
        unsafe {
            let _ = CloseHandle(HANDLE(t as _));
        }
    }
    *w = Vec::new();
}

fn close_host_handles() {
    close(&H_ACTIVE_MARK);
    close(&H_SHUTDOWN);
    close(&H_ALIVE);
    close(&H_STOP);
}

/// Switches every hook off and waits (bounded) until no thread runs hook code or a
/// worker any more. False if either fails: the hooks then stay in, passing straight
/// through (UNLOADING).
fn disable_and_drain() -> bool {
    UNLOADING.store(true, Ordering::SeqCst);
    close(&H_ACTIVE_MARK); // other processes stop handing their minimizes to us
    let stop = h(&H_STOP);
    if !stop.is_invalid() {
        unsafe {
            let _ = SetEvent(stop);
        }
    }
    // MinHook fails only if it cannot write the code back (or freeze the threads for
    // it); a hook that stays on keeps passing through, so trying again is safe.
    let mut off = false;
    for _ in 0..5 {
        off = unsafe { MH_DisableHook(MH_ALL_HOOKS) } == MH_OK;
        if off {
            break;
        }
        unsafe { Sleep(20) };
    }
    if !off {
        return false;
    }
    // Wait for threads still inside hook code or a worker.
    for _ in 0..500 {
        // ~10 s
        if ACTIVE_CALLS.load(Ordering::SeqCst) == 0 && workers_finished() {
            // Grace period for a thread that jumped into a detour but has not yet
            // reached its HookScope.
            unsafe { Sleep(50) };
            if ACTIVE_CALLS.load(Ordering::SeqCst) == 0 && workers_finished() {
                return true;
            }
        }
        unsafe { Sleep(20) };
    }
    false
}

fn activate() -> i32 {
    if !map_settings() {
        return WAIT_HOST;
    }
    let s = SETTINGS.load(Ordering::Acquire);
    let instance = unsafe { (*s).host_instance };
    // Any failure below waits for the next host instance instead of retrying on every
    // hook call.
    WAIT_INSTANCE.store(instance, Ordering::Relaxed);
    unsafe {
        let shutdown = OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide(&shutdown_event_name(instance)).as_ptr()));
        let alive = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE, false, PCWSTR(wide(&alive_mutex_name(instance)).as_ptr()));
        let stop = CreateEventW(None, true, false, PCWSTR::null());
        let (Ok(shutdown), Ok(alive), Ok(stop)) = (shutdown, alive, stop) else {
            return WAIT_HOST;
        };
        H_SHUTDOWN.store(shutdown.0, Ordering::Release);
        H_ALIVE.store(alive.0, Ordering::Release);
        H_STOP.store(stop.0, Ordering::Release);
        // Host already gone or going: don't install anything for it.
        let mut host_gone = WaitForSingleObject(shutdown, 0) == WAIT_OBJECT_0;
        // In the host the mutex is held by its UI thread: a wait from that same thread
        // would succeed (it is recursive) and read as "host died".
        if !host_gone && !is_host_process() {
            let r = WaitForSingleObject(alive, 0);
            if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
                let _ = ReleaseMutex(alive);
                host_gone = true;
            }
        }
        if host_gone {
            close_host_handles();
            return WAIT_HOST;
        }

        let mut path = [0u16; 260];
        let n = GetModuleFileNameW(None, &mut path) as usize;
        make_image_names(&path[..n.min(259)], &mut SELF_NAMES.lock().unwrap_or_else(|e| e.into_inner()));
        ALLOWED_GEN.store(u32::MAX, Ordering::Release);

        // Pinned: a detour's frame may be on a stack for as long as the program runs
        // (see the module comment), so this code must stay mapped.
        let mut module = HMODULE::default();
        if GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN, PCWSTR(activate as *const u16), &mut module).is_err() {
            close_host_handles();
            return WAIT_HOST;
        }

        UNLOADING.store(false, Ordering::SeqCst);
        if MH_Initialize() != MH_OK {
            close_host_handles();
            return WAIT_HOST;
        }
        let mut ok = genie::install_hooks() && MH_QueueEnableHook(MH_ALL_HOOKS) == MH_OK;
        let mut live = false; // hooks may be on - and running in other threads - from here on
        if ok {
            live = true;
            ok = MH_ApplyQueued() == MH_OK;
        }
        let watcher = if ok { CreateThread(None, 0, Some(watcher_thread), None, THREAD_CREATION_FLAGS(0), None).ok() } else { None };
        let Some(watcher) = watcher else {
            // Undone like a shutdown. If a hook cannot be taken out (or a thread does not
            // leave our code), the hooks stay in for good and pass straight through.
            if (live && !disable_and_drain()) || MH_Uninitialize() != MH_OK {
                UNLOADING.store(true, Ordering::SeqCst);
                return DEAD;
            }
            genie::on_shutdown();
            close_workers();
            close_host_handles();
            return WAIT_HOST;
        };
        let _ = CloseHandle(watcher);
        // The global WH_CBT hook reaches every thread here, so HCBT_MINMAX animates any
        // of our windows whoever minimizes it (see peer_animates_itself).
        if let Ok(mark) = CreateEventW(None, true, false, PCWSTR(wide(&active_name(instance, GetCurrentProcessId())).as_ptr())) {
            H_ACTIVE_MARK.store(mark.0, Ordering::Release);
        }
    }
    ACTIVE
}

/// True when everything was switched off cleanly. Otherwise the hooks stay in for
/// good (DEAD), passing straight through.
fn shutdown() -> bool {
    if !disable_and_drain() {
        STATE.store(DEAD, Ordering::Release);
        return false;
    }
    // Frees the trampolines: only once nothing can be in them (drained above).
    if unsafe { MH_Uninitialize() } != MH_OK {
        STATE.store(DEAD, Ordering::Release);
        return false;
    }
    genie::on_shutdown();
    close_workers();
    let s = SETTINGS.load(Ordering::Acquire);
    WAIT_INSTANCE.store(if s.is_null() { 0 } else { unsafe { (*s).host_instance } }, Ordering::Relaxed);
    close_host_handles();
    unmap_settings();
    STATE.store(WAIT_HOST, Ordering::Release);
    true
}

unsafe extern "system" fn watcher_thread(_: *mut c_void) -> u32 {
    let alive = h(&H_ALIVE);
    let handles = [h(&H_SHUTDOWN), alive];
    let r = WaitForMultipleObjects(&handles, false, INFINITE);
    if r.0 == WAIT_OBJECT_0.0 + 1 || r.0 == WAIT_ABANDONED.0 + 1 {
        let _ = ReleaseMutex(alive); // host died: let the other processes see it too
    }
    shutdown();
    0
}

pub fn set_host_process(is_host: bool) {
    IS_HOST_PROCESS.store(is_host, Ordering::Relaxed);
}

/// Running inside DockSide itself: only its own app windows are animated (see
/// `genie::active_for`).
pub fn is_host_process() -> bool {
    IS_HOST_PROCESS.load(Ordering::Relaxed)
}

pub fn on_process_detach() {
    // Process exit (the module is pinned, so never a dynamic unload): only the
    // settings view may still be mapped.
    unmap_settings();
}

pub fn ensure_init() {
    let mut s = STATE.load(Ordering::Acquire);
    if s == ACTIVE || s == INITING || s == DEAD {
        return;
    }
    if s == WAIT_HOST {
        // Re-activate only once a new host instance has published its settings. Hook
        // calls come all the time (activations, clicks, ...): one look a second will do.
        let now = unsafe { GetTickCount64() };
        let next = NEXT_PEEK.load(Ordering::Relaxed);
        if now < next || NEXT_PEEK.compare_exchange(next, now + 1000, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return;
        }
        match peek_host_instance() {
            Some(i) if i != WAIT_INSTANCE.load(Ordering::Relaxed) => {}
            _ => return,
        }
    }
    if let Err(now) = STATE.compare_exchange(s, INITING, Ordering::AcqRel, Ordering::Acquire) {
        s = now;
        let _ = s;
        return;
    }
    let result = activate();
    if result == WAIT_HOST {
        unmap_settings(); // DEAD keeps everything: hooks may still run
    }
    STATE.store(result, Ordering::Release);
}

/// Valid while hooks are installed.
pub fn settings() -> Option<*const SharedSettings> {
    let s = SETTINGS.load(Ordering::Acquire);
    (!s.is_null()).then_some(s as *const _)
}

/// Enabled, not unloading, process still allowed.
pub fn effects_active_here() -> bool {
    let Some(s) = settings() else { return false };
    unsafe {
        if (*s).enabled == 0 || UNLOADING.load(Ordering::Relaxed) {
            return false;
        }
        // The lists can change while we are loaded: re-evaluate once per publish.
        let gen = core::ptr::read_volatile(core::ptr::addr_of!((*s).generation));
        if ALLOWED_GEN.load(Ordering::Acquire) != gen {
            static LIST: Mutex<[u16; LIST_CHARS]> = Mutex::new([0; LIST_CHARS]);
            let mut list = LIST.lock().unwrap_or_else(|e| e.into_inner());
            let names = SELF_NAMES.lock().unwrap_or_else(|e| e.into_inner());
            settings_copy_list(s, core::ptr::addr_of!((*s).no_inject), &mut list);
            let mut allowed = !list_matches(&*list, &names);
            if allowed && (*s).dev_mode != 0 {
                settings_copy_list(s, core::ptr::addr_of!((*s).dev_allowlist), &mut list);
                allowed = list_matches(&*list, &names);
            }
            ALLOWED.store(allowed, Ordering::Relaxed);
            ALLOWED_GEN.store(gen, Ordering::Release);
        }
    }
    ALLOWED.load(Ordering::Relaxed)
}

/// Manual-reset, set when the DLL starts shutting down.
pub fn local_stop_event() -> HANDLE {
    h(&H_STOP)
}

/// True if process `pid` runs the DLL for the same host: its own HCBT_MINMAX
/// animates its windows, whichever thread minimizes them.
pub fn peer_animates_itself(pid: u32) -> bool {
    let Some(s) = settings() else { return false };
    if pid == 0 {
        return false;
    }
    let name = wide(&active_name(unsafe { (*s).host_instance }, pid));
    match unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) } {
        Ok(e) => {
            unsafe {
                let _ = CloseHandle(e);
            }
            true
        }
        Err(_) => false, // also an elevated process we may not open: keep doing it here
    }
}

/// Starts a tracked worker thread (waited for at shutdown). Refused while unloading.
pub fn spawn_worker(proc: unsafe extern "system" fn(*mut c_void) -> u32, param: *mut c_void, suspended: bool) -> Option<HANDLE> {
    let mut w = WORKERS.lock().unwrap_or_else(|e| e.into_inner());
    if UNLOADING.load(Ordering::SeqCst) {
        return None;
    }
    w.retain(|&t| unsafe {
        let done = WaitForSingleObject(HANDLE(t as _), 0) == WAIT_OBJECT_0;
        if done {
            let _ = CloseHandle(HANDLE(t as _));
        }
        !done
    });
    let flags = THREAD_CREATION_FLAGS(if suspended { CREATE_SUSPENDED.0 } else { 0 });
    let t = unsafe { CreateThread(None, 0, Some(proc), Some(param), flags, None) }.ok()?;
    w.push(t.0 as isize);
    Some(t)
}
