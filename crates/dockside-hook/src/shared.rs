//! Shared by DockSide (the host) and every injected `dockside_hook.dll`: object
//! names, the settings section the host publishes and the DLLs read, and the
//! animation request the DLLs send. Included by both crates (`#[path]`), so the
//! layouts can't drift apart.

#![allow(dead_code)]

use core::mem::{offset_of, size_of};

// --- names (common/names.h) ---------------------------------------------------
// Bump the _vN suffix whenever the layout of SharedSettings or AnimRequest changes,
// so mismatched builds never talk to each other.

pub const SETTINGS_SECTION: &str = "Local\\DockSide_Genie_Settings_v1";
/// Per host instance (the suffix is `SharedSettings::host_instance`), so a restarted
/// host never inherits a signaled event / abandoned mutex from the previous one.
pub fn shutdown_event_name(instance: u32) -> String {
    format!("Local\\DockSide_Genie_Shutdown_v1_{instance:08X}")
}
pub fn alive_mutex_name(instance: u32) -> String {
    format!("Local\\DockSide_Genie_Alive_v1_{instance:08X}")
}
/// Exists while the DLL animates the windows of process `pid` itself (host instance,
/// pid): other processes then hand a minimize of its windows over to it.
pub fn active_name(instance: u32, pid: u32) -> String {
    format!("Local\\DockSide_Genie_Active_v1_{instance:08X}_{pid}")
}

pub const GHOST_WNDCLASS: &str = "DockSideGenieGhost_v1";
pub const HOST_WNDCLASS: &str = "DockSideGenieHost_v1";

/// Lowercase image names the DLL must never activate in.
pub const HOST_EXE: &str = "dockside.exe";

pub const CBT_PROC: &[u8] = b"dockside_cbt\0";
pub const GETMSG_PROC: &[u8] = b"dockside_getmsg\0";

/// NUL-terminated UTF-16.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// --- settings (common/settings.h) ---------------------------------------------

pub const SETTINGS_MAGIC: u32 = 0x3153_574D; // 'MWS1'
pub const LIST_CHARS: usize = 2048;

// Window open / close effects (SharedSettings::open_effect / close_effect). The DLLs
// only tell EFFECT_SYSTEM from the rest, so an effect added here needs no new DLL.
pub const EFFECT_SYSTEM: i32 = 0; // Windows' own animation (we do nothing)
pub const EFFECT_GENIE: i32 = 1; // out of / into its Dock icon
pub const EFFECT_SHATTER: i32 = 2; // the window breaks into shards (opening: they fly together)
pub const EFFECT_FRAGMENTS: i32 = 3; // the same with a few big pieces, each its own way

/// Published by the host in a shared section (SETTINGS_SECTION) and read by every
/// DLL, so only fixed-width fields - no pointers, no usize. The host writes under a
/// seqlock (`seq` is odd while a write is in progress); plain int fields can be read
/// directly, the lists are copied with `settings_copy_list`.
#[repr(C)]
pub struct SharedSettings {
    pub magic: u32,
    pub size: u32,
    pub seq: i32,
    pub generation: u32, // bumped on every publish
    pub host_pid: u32,
    pub host_instance: u32, // suffix of the per-instance named objects
    pub host_ipc_hwnd: u64, // window that receives WM_COPYDATA AnimRequests

    pub enabled: i32,
    pub dev_mode: i32, // load only into processes listed in dev_allowlist

    // Genie minimize / restore
    pub genie_duration_ms: i32,        // 50..2000
    pub genie_restore: i32,            // animate restores
    pub genie_launch: i32,             // open_effect != 0: the DLL animates first shows
    pub genie_multi_monitor: i32,      // target the window's own monitor's taskbar
    pub genie_unhide_taskbar: i32,     // reveal an auto-hidden taskbar during minimize
    pub genie_unhide_duration_ms: i32, // 0..5000
    pub genie_mesh_quality: i32,       // tiles per side, 8..96

    pub corner_radius: i32,
    pub corner_smoothing: i32,
    pub window_corners: i32,

    // Opening (first show) and closing (hide / destroy) of windows: EFFECT_*.
    pub open_effect: i32,
    pub close_effect: i32,
    pub open_duration_ms: i32,  // 100..1500
    pub close_duration_ms: i32, // 100..1500
    pub app_icons: i32,
    pub reserved: [i32; 7],

    // Lowercase entries separated by NUL and ended by an empty entry. An entry
    // matches a full image path, a file name or a file name without ".exe".
    pub genie_excluded: [u16; LIST_CHARS], // never animate windows of these
    pub no_inject: [u16; LIST_CHARS],      // the DLL refuses to load into these
    pub dev_allowlist: [u16; LIST_CHARS],
}

// The host and the DLLs must see one layout. A deliberate change updates these
// numbers AND bumps the _vN object names.
const _: () = assert!(size_of::<SharedSettings>() == 12416);
const _: () = assert!(offset_of!(SharedSettings, seq) == 8);
const _: () = assert!(offset_of!(SharedSettings, host_ipc_hwnd) == 24);
const _: () = assert!(offset_of!(SharedSettings, enabled) == 32);
const _: () = assert!(offset_of!(SharedSettings, genie_mesh_quality) == 64);
const _: () = assert!(offset_of!(SharedSettings, close_duration_ms) == 92);
const _: () = assert!(offset_of!(SharedSettings, genie_excluded) == 128);
const _: () = assert!(offset_of!(SharedSettings, dev_allowlist) == 128 + 4 * LIST_CHARS);

/// Consistent copy of one list field (seqlock reader). `field` points into `s`.
///
/// # Safety
/// `s` must point to a mapped `SharedSettings` and `field` to one of its lists.
pub unsafe fn settings_copy_list(s: *const SharedSettings, field: *const [u16; LIST_CHARS], dst: &mut [u16; LIST_CHARS]) {
    use core::sync::atomic::{fence, Ordering};
    let seq = core::ptr::addr_of!((*s).seq);
    for _ in 0..10000 {
        let before = core::ptr::read_volatile(seq);
        if before & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        fence(Ordering::SeqCst);
        core::ptr::copy_nonoverlapping(field as *const u16, dst.as_mut_ptr(), LIST_CHARS);
        fence(Ordering::SeqCst);
        if core::ptr::read_volatile(seq) == before {
            break;
        }
    }
    dst[LIST_CHARS - 2] = 0;
    dst[LIST_CHARS - 1] = 0;
}

// --- animation request (common/ipc.h) -----------------------------------------
// DLL -> host animation request, sent with WM_COPYDATA (COPYDATASTRUCT::dwData =
// IPC_ANIM) and SendMessageTimeout, so the reply is the LRESULT. Handles are values
// in the sending process; the host duplicates what it needs before replying.

pub const IPC_ANIM: usize = 0x4453_0001;
pub const IPC_VERSION: u32 = 2;

pub const ANIM_MINIMIZE: u32 = 0; // falling: window -> Dock icon
pub const ANIM_RESTORE: u32 = 1; // rising: Dock icon -> window
pub const ANIM_LAUNCH: u32 = 2; // rising, first show of a window (open_effect)
pub const ANIM_CLOSE: u32 = 3; // falling, the window is hidden / destroyed (close_effect)

pub const REQ_HAS_SNAPSHOT: u32 = 1;
pub const REQ_SENT: u32 = 2; // asked from a message another thread sent (log only)
pub const REQ_SOURCE_SHIFT: u32 = 8; // bits 8..11: SRC_*
pub const REQ_SOURCE_MASK: u32 = 0xF00;
pub const REQ_CAPTURE_MS_SHIFT: u32 = 16; // bits 16..31: snapshot capture time in ms (log only)

pub const SRC_UNKNOWN: u32 = 0;
pub const SRC_SHOWWINDOW: u32 = 1;
pub const SRC_DEFWINDOWPROC: u32 = 2; // SC_MINIMIZE / SC_RESTORE
pub const SRC_SETWINDOWPLACEMENT: u32 = 3;
pub const SRC_CLOSEWINDOW: u32 = 4;
pub const SRC_CBT: u32 = 5; // HCBT_MINMAX in the window's own thread
pub const SRC_LAUNCH: u32 = 6;
pub const SRC_CLOSE: u32 = 7; // DestroyWindow / SW_HIDE / SWP_HIDEWINDOW

pub const REPLY_FAIL: u32 = 0; // not animating: undo the hide, let Windows animate
pub const REPLY_OK: u32 = 1; // animating; h_done will be signaled
pub const REPLY_NEED_SNAPSHOT: u32 = 2; // restore without a cached snapshot: capture and resend
// A request for a window whose genie is still in flight is never refused: the host
// merges it (same direction) or turns the running genie around (opposite direction).

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AnimRequest {
    pub version: u32,
    pub kind: u32,
    pub hwnd: u64,
    pub pid: u32,
    pub flags: u32,
    /// Pagefile section holding a raw_w x raw_h top-down BGRA (premultiplied) snapshot
    /// of the full window rect, straight from PrintWindow.
    pub h_section: u64,
    pub raw_w: i32,
    pub raw_h: i32,
    /// Part of the snapshot covered by the DWM extended frame, as fractions of raw size.
    pub crop_u0: f32,
    pub crop_v0: f32,
    pub crop_u1: f32,
    pub crop_v1: f32,
    pub h_first_frame: u64, // minimize / close: set once the first frame is on screen
    pub h_done: u64,        // set after the last frame; the DLL then tears down
    pub h_revealed: u64,    // rising: set by the DLL once the real window is visible
}

const _: () = assert!(size_of::<AnimRequest>() == 80);
const _: () = assert!(offset_of!(AnimRequest, h_section) == 24);
const _: () = assert!(offset_of!(AnimRequest, crop_u0) == 40);
const _: () = assert!(offset_of!(AnimRequest, h_revealed) == 72);

// --- image names (common/strutil.h) -------------------------------------------

/// Lowercase forms of an image path used for list matching: full path, file name,
/// or file name without ".exe".
pub struct ImageNames {
    pub full: [u16; 260],
    pub base: [u16; 260],
    pub base_no_ext: [u16; 260],
}

impl ImageNames {
    pub const EMPTY: ImageNames = ImageNames { full: [0; 260], base: [0; 260], base_no_ext: [0; 260] };
}

fn lower(c: u16) -> u16 {
    match c {
        0x41..=0x5A => c + 32,
        0..=0x7F => c,
        _ => char::from_u32(c as u32).and_then(|ch| ch.to_lowercase().next()).filter(|l| (*l as u32) < 0x10000).map_or(c, |l| l as u16),
    }
}

pub fn wlen(s: &[u16]) -> usize {
    s.iter().position(|&c| c == 0).unwrap_or(s.len())
}

/// `path` without its terminator.
pub fn make_image_names(path: &[u16], out: &mut ImageNames) -> bool {
    let n = wlen(path).min(259);
    if n == 0 {
        return false;
    }
    *out = ImageNames::EMPTY;
    for i in 0..n {
        out.full[i] = lower(path[i]);
    }
    let base_at = (0..n).rev().find(|&i| out.full[i] == b'\\' as u16 || out.full[i] == b'/' as u16).map_or(0, |i| i + 1);
    let bn = n - base_at;
    out.base[..bn].copy_from_slice(&out.full[base_at..n]);
    out.base_no_ext[..bn].copy_from_slice(&out.full[base_at..n]);
    if bn > 4 && ends_with(&out.base_no_ext[..bn], ".exe") {
        out.base_no_ext[bn - 4] = 0;
    }
    true
}

pub fn ends_with(s: &[u16], suffix: &str) -> bool {
    let s = &s[..wlen(s)];
    let suf: [u16; 32] = {
        let mut b = [0u16; 32];
        for (i, c) in suffix.encode_utf16().take(32).enumerate() {
            b[i] = c;
        }
        b
    };
    let sl = suffix.len().min(32);
    s.len() >= sl && s[s.len() - sl..] == suf[..sl]
}

pub fn eq_ascii(s: &[u16], other: &str) -> bool {
    let s = &s[..wlen(s)];
    s.len() == other.len() && s.iter().zip(other.bytes()).all(|(&a, b)| a == b as u16)
}

fn contains(hay: &[u16], needle: &[u16]) -> bool {
    !needle.is_empty() && hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// `list`: entries separated by NUL, ended by an empty entry. An entry matches the
/// full path, the file name or the file name without ".exe"; an entry with a
/// backslash that does not end in ".exe" is a folder pattern, found anywhere in the
/// full path (e.g. "\steamapps\common\" matches every program installed there).
pub fn list_matches(list: &[u16], n: &ImageNames) -> bool {
    let (full, base, bne) = (&n.full[..wlen(&n.full)], &n.base[..wlen(&n.base)], &n.base_no_ext[..wlen(&n.base_no_ext)]);
    let mut p = 0;
    while p < list.len() && list[p] != 0 {
        let len = wlen(&list[p..]);
        let e = &list[p..p + len];
        if e == full || e == base || e == bne {
            return true;
        }
        let exe = ends_with(e, ".exe");
        if !exe && e.contains(&(b'\\' as u16)) && contains(full, e) {
            return true;
        }
        p += len + 1;
    }
    false
}
