// Portions from the Windhawk mod "MacOS Minimize Animation" (MIT, Abdullah Masood and Potassiumuncher).

//! The hook DLL: loaded into other programs by DockSide's global `WH_CBT` hook. Its
//! inline hooks (`genie`) capture a window before it minimizes, closes or opens and
//! ask DockSide to animate it, holding the real change back until the first frame
//! is on screen.
//!
//! This runs inside other people's programs. A crash or hang here takes down
//! Explorer, a browser or a game, so: no panics out of any hook (slow paths are
//! wrapped), every detour has a cheap fast path and calls the original in the end,
//! and anything that fails falls back to Windows' own behaviour.

mod genie;
mod loader;
mod minhook;
pub mod shared;

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicI32, Ordering};

use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::{GetFileAttributesW, FILE_ATTRIBUTE_DIRECTORY, INVALID_FILE_ATTRIBUTES};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleFileNameW, GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::*;
use windows::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};
use windows::Win32::UI::WindowsAndMessaging::*;

use shared::*;

pub(crate) static MODULE: AtomicIsize = AtomicIsize::new(0);

/// Set while the DLL is shutting down: hooks pass straight through, workers wind down.
pub(crate) static UNLOADING: AtomicBool = AtomicBool::new(false);

/// Threads currently executing hook code (see `HookScope`).
pub(crate) static ACTIVE_CALLS: AtomicI32 = AtomicI32::new(0);

pub(crate) struct HookScope;

impl HookScope {
    pub fn new() -> Self {
        ACTIVE_CALLS.fetch_add(1, Ordering::AcqRel);
        HookScope
    }
}

impl Drop for HookScope {
    fn drop(&mut self) {
        ACTIVE_CALLS.fetch_sub(1, Ordering::AcqRel);
    }
}

// System processes the DLL never stays in, whatever the settings say: session and logon
// infrastructure, the compositor, the font driver host, the UAC prompt, the lock screen,
// the shell's own UI hosts and the crash reporter (names without ".exe").
const NEVER_LOAD: [&str; 14] = [
    "csrss", "winlogon", "logonui", "lsass", "dwm", "fontdrvhost", "consent", "lockapp", "searchhost",
    "startmenuexperiencehost", "shellexperiencehost", "textinputhost", "werfault", "werfaultsecure",
];

// Anti-cheat that ships in a folder next to the game. It may take any foreign module
// for a cheat, so the DLL stays out of every program installed there.
const ANTI_CHEAT_DIRS: [&str; 3] = ["EasyAntiCheat", "EasyAntiCheat_EOS", "BattlEye"];

/// True if folder `dir` (its first `len` characters, no trailing backslash) holds one
/// of ANTI_CHEAT_DIRS.
fn has_anti_cheat_dir(dir: &[u16], len: usize) -> bool {
    let mut probe = [0u16; 260 + 32];
    if len == 0 || len + 32 > probe.len() {
        return false;
    }
    for name in ANTI_CHEAT_DIRS {
        probe[..len].copy_from_slice(&dir[..len]);
        probe[len] = b'\\' as u16;
        let mut n = len + 1;
        for c in name.encode_utf16() {
            probe[n] = c;
            n += 1;
        }
        probe[n] = 0;
        let attr = unsafe { GetFileAttributesW(PCWSTR(probe.as_ptr())) };
        if attr != INVALID_FILE_ATTRIBUTES && attr & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            return true;
        }
    }
    false
}

/// `path`: the exe as GetModuleFileName gave it; `lower`: the same, lowercase (same
/// length). Looks next to the exe and, for Unreal games
/// (<game>\<Project>\Binaries\Win64\<exe>), also in <Project> and <game>, where
/// their anti-cheat usually sits.
fn next_to_anti_cheat(path: &[u16], lower: &[u16]) -> bool {
    let n = wlen(lower);
    let Some(slash) = (0..n).rev().find(|&i| lower[i] == b'\\' as u16) else { return false };
    let dir_len = slash;
    if has_anti_cheat_dir(path, dir_len) {
        return true;
    }
    const TAIL: usize = 15; // "\binaries\win64"
    if dir_len <= TAIL || !(ends_with(&lower[..dir_len], "\\binaries\\win64") || ends_with(&lower[..dir_len], "\\binaries\\win32")) {
        return false;
    }
    let project_len = dir_len - TAIL;
    if has_anti_cheat_dir(path, project_len) {
        return true;
    }
    let mut game_len = project_len;
    while game_len > 0 && lower[game_len - 1] != b'\\' as u16 {
        game_len -= 1;
    }
    game_len > 1 && has_anti_cheat_dir(path, game_len - 1)
}

/// Runs under the loader lock: kernel32/ntdll only. Decides whether this process may
/// keep the DLL at all - refusing here unmaps it again, so programs on the "do not
/// inject" list (and, in test mode, everything outside the allowlist) never have our
/// code in them.
fn should_load_into_this_process(is_host: &mut bool) -> bool {
    let mut path = [0u16; 260];
    let len = unsafe { GetModuleFileNameW(None, &mut path) } as usize;
    if len == 0 || len >= path.len() {
        return false;
    }
    static mut SELF_NAMES: ImageNames = ImageNames::EMPTY; // DllMain calls are serialized
    let names = unsafe { &mut *core::ptr::addr_of_mut!(SELF_NAMES) };
    if !make_image_names(&path[..len], names) {
        return false;
    }
    // The host loads the DLL itself (to hand its hook procedures to Windows); it keeps
    // it, but never activates anything in it.
    if eq_ascii(&names.base, HOST_EXE) {
        *is_host = true;
        return true;
    }
    if NEVER_LOAD.iter().any(|n| eq_ascii(&names.base_no_ext, n)) {
        return false;
    }

    let section = wide_static(SETTINGS_SECTION);
    let Ok(map) = (unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(section.as_ptr())) }) else { return false };
    let view = unsafe { MapViewOfFile(map, FILE_MAP_READ, 0, 0, core::mem::size_of::<SharedSettings>()) };
    let s = view.Value as *const SharedSettings;
    let mut allow = false;
    unsafe {
        if !s.is_null() && (*s).magic == SETTINGS_MAGIC && (*s).size as usize == core::mem::size_of::<SharedSettings>() {
            static mut LIST: [u16; LIST_CHARS] = [0; LIST_CHARS];
            let list = &mut *core::ptr::addr_of_mut!(LIST);
            settings_copy_list(s, core::ptr::addr_of!((*s).no_inject), list);
            if !list_matches(list, names) {
                if (*s).dev_mode != 0 {
                    settings_copy_list(s, core::ptr::addr_of!((*s).dev_allowlist), list);
                    allow = list_matches(list, names);
                } else {
                    allow = true;
                }
            }
        }
        if !s.is_null() {
            let _ = UnmapViewOfFile(view);
        }
        let _ = CloseHandle(map);
    }
    // Last: the only check that touches the disk.
    allow && !next_to_anti_cheat(&path, &names.full)
}

/// A NUL-terminated UTF-16 copy of an ASCII constant, without allocating (DllMain).
fn wide_static(s: &str) -> [u16; 96] {
    let mut b = [0u16; 96];
    for (i, c) in s.encode_utf16().take(95).enumerate() {
        b[i] = c;
    }
    b
}

#[no_mangle]
extern "system" fn DllMain(inst: HINSTANCE, reason: u32, reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(inst.0 as isize, Ordering::Relaxed);
        unsafe {
            let _ = DisableThreadLibraryCalls(HMODULE(inst.0));
        }
        let mut is_host = false;
        if !should_load_into_this_process(&mut is_host) {
            return BOOL(0);
        }
        loader::set_host_process(is_host);
    } else if reason == DLL_PROCESS_DETACH && reserved.is_null() {
        loader::on_process_detach();
    }
    BOOL(1)
}

// Windows calls these in every GUI thread of the session. They are only the vehicle
// that gets the DLL loaded; the real work is done by the inline hooks
// `loader::ensure_init` installs. WH_CBT stays installed (it fires rarely: window
// create / activate / min-max); WH_GETMESSAGE is installed for a moment at host
// start-up so programs that were already running pick the DLL up right away.
#[no_mangle]
pub extern "system" fn dockside_cbt(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    loader::ensure_init();
    if code == HCBT_MINMAX as i32 {
        let _ = std::panic::catch_unwind(|| genie::on_min_max(HWND(wp.0 as _), (lp.0 & 0xFFFF) as i32));
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

#[no_mangle]
pub extern "system" fn dockside_getmsg(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    loader::ensure_init();
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

// --- hooks.cpp ----------------------------------------------------------------

/// Creates (but does not enable) an inline hook on a user32 export. GetProcAddress
/// follows forwarders, so e.g. DefWindowProcW lands on the ntdll implementation every
/// caller ends up in.
pub(crate) fn create_user32_hook(name: &[u8], detour: *mut c_void, original: &AtomicIsize) -> bool {
    unsafe {
        let Ok(user32) = GetModuleHandleW(windows::core::w!("user32.dll")) else { return false };
        let Some(target) = GetProcAddress(user32, PCSTR(name.as_ptr())) else { return false };
        let mut orig: *mut c_void = core::ptr::null_mut();
        if minhook::MH_CreateHook(target as *mut c_void, detour, &mut orig) != minhook::MH_OK {
            return false;
        }
        original.store(orig as isize, Ordering::Release);
        true
    }
}

// --- capture.cpp ----------------------------------------------------------------

/// Window-only capture: PrintWindow renders just this window, so the taskbar or
/// windows behind it can never bleed into the genie, and it also works on a cloaked
/// window. The DIB is backed by a pagefile section, so PrintWindow writes straight
/// into memory the host maps - no extra copy. Alpha fix-ups happen on the GPU.
pub(crate) fn capture_to_section(hwnd: HWND, raw_w: i32, raw_h: i32) -> Option<HANDLE> {
    if raw_w <= 0 || raw_h <= 0 || raw_w > 16384 || raw_h > 16384 {
        return None;
    }
    let bytes = raw_w as u64 * raw_h as u64 * 4;
    unsafe {
        let section = CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, (bytes >> 32) as u32, bytes as u32, PCWSTR::null()).ok()?;
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(screen);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: raw_w,
                biHeight: -raw_h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = core::ptr::null_mut();
        let dib = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, section, 0);
        let mut ok = false;
        if let Ok(dib) = dib {
            if !bits.is_null() {
                let old = SelectObject(mem, dib);
                ok = PrintWindow(hwnd, mem, PRINT_WINDOW_FLAGS(2) /* PW_RENDERFULLCONTENT */).as_bool();
                let _ = GdiFlush();
                SelectObject(mem, old);
            }
            let _ = DeleteObject(dib);
        }
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if !ok {
            let _ = CloseHandle(section);
            return None;
        }
        Some(section)
    }
}

// --- ipc_client.cpp -------------------------------------------------------------

pub(crate) fn host_ipc_window() -> Option<HWND> {
    let s = loader::settings()?;
    let h = HWND(unsafe { (*s).host_ipc_hwnd } as usize as _);
    (!h.is_invalid() && unsafe { IsWindow(h).as_bool() }).then_some(h)
}

/// Synchronous: the host only duplicates handles, uploads the snapshot and queues the
/// animation before replying, so this returns within a couple of milliseconds.
/// SMTO_BLOCK keeps this (usually UI) thread from dispatching unrelated sent messages
/// while it waits, so nothing re-enters the window procedure mid-request.
pub(crate) fn send_anim_request(host: HWND, req: &AnimRequest) -> u32 {
    let cds = COPYDATASTRUCT { dwData: IPC_ANIM, cbData: core::mem::size_of::<AnimRequest>() as u32, lpData: req as *const _ as *mut c_void };
    let mut result: usize = REPLY_FAIL as usize;
    let sent = unsafe { SendMessageTimeoutW(host, WM_COPYDATA, WPARAM(0), LPARAM(&cds as *const _ as isize), SMTO_BLOCK | SMTO_ABORTIFHUNG, 150, Some(&mut result)) };
    if sent.0 == 0 {
        return REPLY_FAIL;
    }
    result as u32
}
