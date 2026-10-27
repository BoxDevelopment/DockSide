//! MinHook 1.3.4 (minhook/, BSD-2, Tsuda Kageyu): the inline hooks.

use core::ffi::c_void;

pub type Status = i32;
pub const MH_OK: Status = 0;
pub const MH_ALL_HOOKS: *mut c_void = core::ptr::null_mut();

extern "system" {
    pub fn MH_Initialize() -> Status;
    pub fn MH_Uninitialize() -> Status;
    pub fn MH_CreateHook(target: *mut c_void, detour: *mut c_void, original: *mut *mut c_void) -> Status;
    pub fn MH_DisableHook(target: *mut c_void) -> Status;
    pub fn MH_QueueEnableHook(target: *mut c_void) -> Status;
    pub fn MH_ApplyQueued() -> Status;
}
