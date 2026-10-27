//! Handles AnimRequests from the DLLs (WM_COPYDATA on DockSide's UI thread). Only
//! cheap work happens here - handle duplication, the snapshot upload, geometry - so
//! the requesting app thread (blocked in SendMessageTimeout, 150 ms at most) is
//! released within a few ms.

use std::cell::RefCell;
use std::collections::HashMap;

use windows::Win32::Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUNDSMALL, DWM_WINDOW_CORNER_PREFERENCE};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, GetWindowRgnBox, MonitorFromPoint, MonitorFromRect, MonitorFromWindow, OffsetRect, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, RGN_ERROR};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::Memory::{MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcess, PROCESS_DUP_HANDLE};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GetWindowPlacement, GetWindowRect, IsWindow, IsZoomed, GWL_EXSTYLE, WINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED, WS_EX_TOOLWINDOW};

use super::effect::{self, DockTarget};
use super::render::{qpc, Job, Renderer};
use super::shared::*;
use super::Settings;

fn dup_from(process: HANDLE, value: u64) -> HANDLE {
    if value == 0 {
        return HANDLE::default();
    }
    let mut out = HANDLE::default();
    unsafe {
        if DuplicateHandle(process, HANDLE(value as usize as _), GetCurrentProcess(), &mut out, 0, false, DUPLICATE_SAME_ACCESS).is_err() {
            return HANDLE::default();
        }
    }
    out
}

/// Where the window is on screen (DWM's extended frame). A window already minimized
/// when its request is read sits at Windows' parking spot (-32000, -32000), and that
/// rect would stretch the genie's canvas over 30000 px: its normal - or maximized -
/// place is used then.
fn window_screen_rect(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe {
        if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as _, std::mem::size_of::<RECT>() as u32).is_err() && GetWindowRect(hwnd, &mut r).is_err() {
            return None;
        }
        if !MonitorFromRect(&r, MONITOR_DEFAULTTONULL).is_invalid() {
            return Some(r);
        }
        let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetWindowPlacement(hwnd, &mut wp).is_err() || !GetMonitorInfoW(MonitorFromRect(&wp.rcNormalPosition, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            return None;
        }
        if wp.flags.0 & WPF_RESTORETOMAXIMIZED.0 != 0 {
            return Some(mi.rcWork);
        }
        // The normal place is in work-area coordinates (screen ones for tool windows).
        r = wp.rcNormalPosition;
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 == 0 {
            let _ = OffsetRect(&mut r, mi.rcWork.left - mi.rcMonitor.left, mi.rcWork.top - mi.rcMonitor.top);
        }
        (!MonitorFromRect(&r, MONITOR_DEFAULTTONULL).is_invalid()).then_some(r)
    }
}

pub struct Server {
    pub renderer: Renderer,
    pub settings: Settings,
    /// The window's Dock icon on screen, if it has one. None too while DockSide is
    /// busy elsewhere (the request arrived in the middle of other work).
    pub dock_icon: fn(HWND) -> Option<RECT>,
    /// Each window's icon as last seen, for when `dock_icon` can't tell.
    pub last_icon: RefCell<HashMap<isize, RECT>>,
}

impl Server {
    pub fn handle(&self, cds: *const COPYDATASTRUCT) -> u32 {
        let Some(cds) = (unsafe { cds.as_ref() }) else {
            return REPLY_FAIL;
        };
        if cds.dwData != IPC_ANIM || cds.cbData as usize != std::mem::size_of::<AnimRequest>() || cds.lpData.is_null() {
            return REPLY_FAIL;
        }
        let req = unsafe { std::ptr::read_unaligned(cds.lpData as *const AnimRequest) };
        self.handle_anim(&req)
    }

    fn handle_anim(&self, r: &AnimRequest) -> u32 {
        let requested_at = qpc();
        let s = &self.settings;
        let hwnd = HWND(r.hwnd as usize as _);
        const SOURCES: [&str; 8] = ["?", "ShowWindow", "DefWindowProc", "SetWindowPlacement", "CloseWindow", "HCBT_MINMAX", "launch", "close"];
        const KINDS: [&str; 4] = ["minimize", "restore", "open", "close"];
        let source = ((r.flags & REQ_SOURCE_MASK) >> REQ_SOURCE_SHIFT) as usize;
        log!(
            "genie: {} request for {:#x} (pid {}, {} x {}{}, via {}{})",
            KINDS.get(r.kind as usize).unwrap_or(&"?"),
            r.hwnd,
            r.pid,
            r.raw_w,
            r.raw_h,
            if r.flags & REQ_HAS_SNAPSHOT != 0 { format!(", snapshot in {} ms", r.flags >> REQ_CAPTURE_MS_SHIFT) } else { String::new() },
            SOURCES.get(source).unwrap_or(&"?"),
            if r.flags & REQ_SENT != 0 { ", sent by another thread" } else { "" }
        );
        let is_window = unsafe { IsWindow(hwnd).as_bool() };
        if r.version != IPC_VERSION || !s.enabled || !is_window || r.kind > ANIM_CLOSE {
            log!("genie: rejected (version {}, enabled {}, window {is_window})", r.version, s.enabled);
            return REPLY_FAIL;
        }
        let opening = r.kind == ANIM_LAUNCH;
        let closing = r.kind == ANIM_CLOSE;
        // Minimizing and restoring with Windows' own animation: the DLL still asks (the
        // shared settings have no switch for it), and is told to let Windows animate.
        if !opening && !closing && !s.minimize {
            return REPLY_FAIL;
        }
        // Opening / closing: only with an effect of ours chosen for it (the DLL only asks then).
        if (opening && s.open_effect == EFFECT_SYSTEM) || (closing && s.close_effect == EFFECT_SYSTEM) {
            return REPLY_FAIL;
        }
        // Shatter and fragments: the window breaks apart (closing) or its pieces fly
        // together (opening), wherever its Dock icon is.
        let fx = if opening {
            s.open_effect
        } else if closing {
            s.close_effect
        } else {
            EFFECT_GENIE
        };
        let shatter = fx == EFFECT_SHATTER || fx == EFFECT_FRAGMENTS;
        let crop_ok = r.crop_u0 >= 0.0 && r.crop_v0 >= 0.0 && r.crop_u1 <= 1.0 && r.crop_v1 <= 1.0 && r.crop_u1 > r.crop_u0 && r.crop_v1 > r.crop_v0;
        if r.raw_w <= 0 || r.raw_h <= 0 || r.raw_w > 16384 || r.raw_h > 16384 || !crop_ok {
            log!("genie: rejected (bad size / crop {:.3} {:.3} {:.3} {:.3})", r.crop_u0, r.crop_v0, r.crop_u1, r.crop_v1);
            return REPLY_FAIL;
        }

        let rising = r.kind == ANIM_RESTORE || r.kind == ANIM_LAUNCH;
        let has_snapshot = r.flags & REQ_HAS_SNAPSHOT != 0 && r.h_section != 0;
        // One of our own windows: PrintWindow only sees black in a layered window, so
        // the DLL leaves the picture to us - the canvas it last presented.
        let own = !has_snapshot && r.pid == unsafe { GetCurrentProcessId() };
        let own_size = if own { crate::gfx::with_presented(hwnd, |_, w, h| (w, h)) } else { None };
        if own && own_size.is_none() && !rising {
            log!("genie: no canvas for our own window {:#x}, left to Windows", r.hwnd);
            return REPLY_FAIL;
        }
        let (raw_w, raw_h) = own_size.unwrap_or((r.raw_w, r.raw_h));
        // Crop of the extended frame in snapshot pixels.
        let crop_x = (r.crop_u0 * raw_w as f32).round() as i32;
        let crop_y = (r.crop_v0 * raw_h as f32).round() as i32;
        let crop_w = (r.crop_u1 * raw_w as f32).round() as i32 - crop_x;
        let crop_h = (r.crop_v1 * raw_h as f32).round() as i32 - crop_y;
        if crop_w < 1 || crop_h < 1 {
            return REPLY_FAIL;
        }

        // A window whose genie is still in flight needs no snapshot: the running genie
        // keeps its own and simply turns around (see the renderer's retarget). An opening
        // window is always captured fresh: a cached minimize snapshot of the same handle
        // would be an old picture.
        let active = self.renderer.is_active(hwnd);
        if rising && !has_snapshot && own_size.is_none() && !active && (opening || !self.renderer.has_snapshot(hwnd, crop_w, crop_h)) {
            return REPLY_NEED_SNAPSHOT;
        }

        let mut job = Job::new(hwnd);
        job.rising = rising;
        job.crop_x = crop_x;
        job.crop_y = crop_y;
        job.crop_w = crop_w;
        job.crop_h = crop_h;
        job.requested_at = requested_at;
        job.duration_ms = if opening {
            s.open_duration_ms
        } else if closing {
            s.close_duration_ms
        } else {
            s.duration_ms
        };
        job.tiles = s.mesh_quality;
        job.style = match fx {
            EFFECT_SHATTER => effect::STYLE_SHATTER,
            EFFECT_FRAGMENTS => effect::STYLE_FRAGMENTS,
            _ => s.style,
        };
        job.params.closing = closing;
        self.renderer.retain(hwnd); // from here on every exit goes through fail / the renderer
        super::stand_in(hwnd, job.duration_ms);

        let mut section = HANDLE::default();
        if let Ok(process) = unsafe { OpenProcess(PROCESS_DUP_HANDLE, false, r.pid) } {
            job.h_done = dup_from(process, r.h_done);
            job.h_first = dup_from(process, r.h_first_frame);
            job.h_revealed = dup_from(process, r.h_revealed);
            if has_snapshot {
                section = dup_from(process, r.h_section);
            }
            unsafe {
                let _ = CloseHandle(process);
            }
        }
        let mut ok = !job.h_done.is_invalid() && (!rising || !job.h_revealed.is_invalid()) && (rising || !job.h_first.is_invalid());
        // Uploaded even for an active window: its genie may end before this job is picked up.
        if ok && has_snapshot {
            ok = !section.is_invalid() && self.upload_snapshot(section, &mut job, r.raw_w, r.raw_h);
        } else if ok && own_size.is_some() {
            // Real per-pixel alpha (the window's own soft shadow): never forced opaque.
            ok = crate::gfx::with_presented(hwnd, |px, w, h| self.upload_pixels(px, &mut job, w, h)).unwrap_or(false);
        }
        if !section.is_invalid() {
            unsafe {
                let _ = CloseHandle(section);
            }
        }
        if !ok {
            log!("genie: request for {:#x} (pid {}) rejected: handles/snapshot unavailable", r.hwnd, r.pid);
            job.close_handles(); // nothing left to signal
            self.renderer.fail(job); // just releases the window
            return REPLY_FAIL;
        }

        // Geometry in physical pixels (DockSide is per-monitor DPI aware).
        let Some(rect) = window_screen_rect(hwnd) else {
            log!("genie: {:#x} has no place on any monitor, left to Windows", r.hwnd);
            self.renderer.fail(job);
            return REPLY_FAIL;
        };
        job.win_rect = rect;
        // Our own windows draw their shadow and rounded corners into the canvas.
        if own_size.is_none() && !unsafe { IsZoomed(hwnd).as_bool() } {
            let (mut dpi_x, mut dpi_y) = (96, 96);
            unsafe {
                let _ = GetDpiForMonitor(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            }
            let scale = dpi_y as f32 / 96.0;
            // A maximized window has no shadow; any other one takes its shadow along.
            job.params.shadow_scale = scale;
            // The corner the window really has, so the genie doesn't change it on the way.
            // A window the DLL cut (it has a region) has the one from the settings; every
            // other window has DWM's: Windows 11's circular 8 px, 4 px when asked for
            // small corners, none when asked for square ones.
            let mut rgn_box = RECT::default();
            if unsafe { GetWindowRgnBox(hwnd, &mut rgn_box) } != RGN_ERROR {
                job.corner_radius_px = s.corner_radius as f32 * scale;
                job.corner_smoothing = s.corner_smoothing as f32 / 100.0;
            } else {
                let mut pref = DWM_WINDOW_CORNER_PREFERENCE::default();
                unsafe {
                    let _ = DwmGetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &mut pref as *mut _ as _, 4);
                }
                let radius = if pref == DWMWCP_DONOTROUND {
                    0.0
                } else if pref == DWMWCP_ROUNDSMALL {
                    4.0
                } else {
                    8.0
                };
                job.corner_radius_px = radius * scale;
            }
        }

        // The pieces stay on the window's own monitor and go nowhere in particular.
        if shatter {
            job.mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
            self.dispatch(job, DockTarget::default());
            return REPLY_OK;
        }

        // Into the window's Dock icon, on the Dock's monitor. A window the Dock doesn't
        // show (or a Dock that is off) goes where a taskbar button would be.
        let icon = match (self.dock_icon)(hwnd) {
            Some(icon) => {
                let mut last = self.last_icon.borrow_mut();
                if last.len() > 256 {
                    last.retain(|&h, _| unsafe { IsWindow(HWND(h as _)).as_bool() });
                }
                last.insert(hwnd.0 as isize, icon);
                Some(icon)
            }
            None => self.last_icon.borrow().get(&(hwnd.0 as isize)).copied(),
        };
        let dock = match icon {
            Some(icon) => {
                job.mon = unsafe { MonitorFromRect(&icon, MONITOR_DEFAULTTONEAREST) };
                DockTarget { x: (icon.left + icon.right) / 2, y: (icon.top + icon.bottom) / 2, size: icon.right - icon.left, has_rect: true }
            }
            None => {
                job.mon = if s.multi_monitor {
                    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }
                } else {
                    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
                };
                DockTarget { x: effect::default_dock_x(job.mon), ..Default::default() }
            }
        };
        self.dispatch(job, dock);
        REPLY_OK
    }

    fn dispatch(&self, mut job: Box<Job>, dock: DockTarget) {
        if effect::is_shatter_style(job.style) {
            log!("genie: {:?} in pieces ({})", job.hwnd.0, if job.style == effect::STYLE_FRAGMENTS { "fragments" } else { "shatter" });
        } else if dock.has_rect {
            log!("genie: {:?} docks into its Dock icon at {},{} (icon {} px)", job.hwnd.0, dock.x, dock.y, dock.size);
        } else {
            log!("genie: {:?} docks at x {} on the screen edge (no Dock icon)", job.hwnd.0, dock.x);
        }
        let (rect, mon, style) = (job.win_rect, job.mon, job.style);
        if !effect::compute_genie_params(&rect, mon, &dock, style, &mut job.params) {
            self.renderer.fail(job);
            return;
        }
        self.renderer.submit(job);
    }

    /// Maps the DLL's capture, decides the alpha fallback and uploads it. PrintWindow
    /// wrote straight into this section, so this is the only copy on its way to the GPU.
    fn upload_snapshot(&self, section: HANDLE, job: &mut Job, raw_w: i32, raw_h: i32) -> bool {
        let bytes = raw_w as usize * raw_h as usize * 4;
        let view = unsafe { MapViewOfFile(section, FILE_MAP_READ, 0, 0, bytes) };
        if view.Value.is_null() {
            return false;
        }
        let px = unsafe { std::slice::from_raw_parts(view.Value as *const u32, raw_w as usize * raw_h as usize) };

        // The mod's fallback: if most of the extended frame came back fully transparent,
        // the alpha channel isn't describing real translucency - treat it as opaque.
        // Sampling every 4th pixel each way is plenty for a majority vote.
        let (mut samples, mut zero) = (0usize, 0usize);
        for y in (job.crop_y..(job.crop_y + job.crop_h).min(raw_h)).step_by(4) {
            let row = &px[y as usize * raw_w as usize..];
            for x in (job.crop_x..(job.crop_x + job.crop_w).min(raw_w)).step_by(4) {
                samples += 1;
                if row[x as usize] >> 24 == 0 {
                    zero += 1;
                }
            }
        }
        job.force_opaque = samples > 0 && zero * 2 > samples;

        let ok = self.upload_pixels(px, job, raw_w, raw_h);
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value });
        }
        ok
    }

    /// Uploads a `raw_w` x `raw_h` premultiplied BGRA picture as the job's snapshot.
    fn upload_pixels(&self, px: &[u32], job: &mut Job, raw_w: i32, raw_h: i32) -> bool {
        let mut ok = false;
        if let Some(device) = self.renderer.device() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: raw_w as u32,
                Height: raw_h as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: px.as_ptr() as _, SysMemPitch: raw_w as u32 * 4, SysMemSlicePitch: 0 };
            match unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut job.raw)) } {
                Ok(()) => ok = job.raw.is_some(),
                Err(e) => log!("genie: snapshot upload {raw_w} x {raw_h} failed: {e}"),
            }
        }
        ok
    }
}
