//! "Start DockSide at login": a per-user `Run` registry value.

use std::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: PCWSTR = w!("DockSide");
/// The value's name from before the rename to DockSide.
const LEGACY_VALUE: PCWSTR = w!("MacFinder");

pub fn is_enabled() -> bool {
    let mut size = 0u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE, RRF_RT_REG_SZ, None, None, Some(&mut size)).is_ok() }
}

pub fn set_enabled(enabled: bool) -> windows::core::Result<()> {
    unsafe {
        if !enabled {
            return RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE).ok();
        }
        let exe = std::env::current_exe().map_err(|_| windows::core::Error::from_win32())?;
        let cmd: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain(std::iter::once(0)).collect();
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE,
            REG_SZ.0,
            Some(cmd.as_ptr() as *const c_void),
            (cmd.len() * 2) as u32,
        )
        .ok()
    }
}

/// Keeps the registered path pointing at this exe if it was moved (or renamed from MacFinder).
pub fn refresh() {
    let legacy = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, LEGACY_VALUE).is_ok() };
    if legacy || is_enabled() {
        let _ = set_enabled(true);
    }
}
