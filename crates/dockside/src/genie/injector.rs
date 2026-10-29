//! Gets `dockside_hook.dll` into GUI processes via SetWindowsHookEx. Windows calls
//! the hook procedures of processes it can't load the DLL into on the thread that
//! installed them, so that thread (DockSide's UI thread) must keep pumping messages.
//!
//! Only 64-bit apps at our integrity level or below get the DLL; the rest keep
//! Windows' own animations.

use std::path::PathBuf;

use windows::core::HSTRING;
use windows::Win32::Foundation::{FreeLibrary, BOOL, HMODULE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, HOOKPROC, WH_CBT, WH_GETMESSAGE, WM_NULL};

use super::shared::{CBT_PROC, GETMSG_PROC};

unsafe extern "system" fn post_null(hwnd: HWND, _: LPARAM) -> BOOL {
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    BOOL(1)
}

fn nudge_all_threads() {
    unsafe {
        let _ = EnumWindows(Some(post_null), LPARAM(0));
    }
}

pub struct Injector {
    dll: HMODULE,
    getmsg_proc: HOOKPROC,
    cbt: HHOOK,
    getmsg: Option<HHOOK>,
}

impl Injector {
    pub fn start() -> Option<Injector> {
        let path = write_dll().map_err(|e| log!("genie: writing the hook DLL failed: {e}")).ok()?;
        unsafe {
            let dll = LoadLibraryExW(&HSTRING::from(path.as_os_str()), None, LOAD_WITH_ALTERED_SEARCH_PATH)
                .map_err(|e| log!("genie: loading {} failed: {e}", path.display()))
                .ok()?;
            let procs = (GetProcAddress(dll, windows::core::PCSTR(CBT_PROC.as_ptr())), GetProcAddress(dll, windows::core::PCSTR(GETMSG_PROC.as_ptr())));
            let (Some(cbt_proc), Some(getmsg_proc)) = procs else {
                log!("genie: hook procedures not exported by {}", path.display());
                let _ = FreeLibrary(dll);
                return None;
            };
            let cbt_proc: HOOKPROC = Some(std::mem::transmute(cbt_proc));
            let getmsg_proc: HOOKPROC = Some(std::mem::transmute(getmsg_proc));
            match SetWindowsHookExW(WH_CBT, cbt_proc, dll, 0) {
                Ok(cbt) => {
                    log!("genie: WH_CBT installed ({})", path.display());
                    Some(Injector { dll, getmsg_proc, cbt, getmsg: None })
                }
                Err(e) => {
                    log!("genie: SetWindowsHookEx(WH_CBT) failed: {e}");
                    let _ = FreeLibrary(dll);
                    None
                }
            }
        }
    }

    /// One-off WH_GETMESSAGE + WM_NULL to every top-level window, so programs that are
    /// already running load the DLL now rather than at their next window event.
    pub fn begin_preload(&mut self) {
        if self.getmsg.is_some() {
            return;
        }
        match unsafe { SetWindowsHookExW(WH_GETMESSAGE, self.getmsg_proc, self.dll, 0) } {
            Ok(h) => {
                self.getmsg = Some(h);
                nudge_all_threads();
            }
            Err(e) => log!("genie: SetWindowsHookEx(WH_GETMESSAGE) failed: {e}"),
        }
    }

    pub fn end_preload(&mut self) {
        if let Some(h) = self.getmsg.take() {
            unsafe {
                let _ = UnhookWindowsHookEx(h);
            }
            log!("genie: preload hook removed");
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.end_preload();
        unsafe {
            let _ = UnhookWindowsHookEx(self.cbt);
        }
        // Windows drops its reference to the DLL in each process the next time one of
        // its threads handles a message; nudge them all so that happens now.
        nudge_all_threads();
        unsafe {
            let _ = FreeLibrary(self.dll);
        }
        log!("genie: hooks removed");
    }
}

/// `dockside_hook.dll`, built by build.rs, so DockSide ships as a single exe.
static HOOK_DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dockside_hook.dll"));

/// Writes the embedded DLL out under a name of its own for this build: every app
/// it's loaded into keeps the file open until it next handles a message after we
/// unhook, so a newer DockSide can't overwrite it in place.
fn write_dll() -> std::io::Result<PathBuf> {
    // FNV-1a, so each distinct DLL gets its own file name.
    let hash = HOOK_DLL.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
    let dir = crate::config::dir().join("hook");
    std::fs::create_dir_all(&dir)?;
    let name = format!("dockside_hook-{hash:016x}.dll");
    // Older copies go once no app has them open any more.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.file_name() != name.as_str() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let path = dir.join(name);
    if std::fs::metadata(&path).ok().map(|m| m.len()) != Some(HOOK_DLL.len() as u64) {
        // Written beside and renamed into place, so a half-written DLL is never loaded.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, HOOK_DLL)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}
