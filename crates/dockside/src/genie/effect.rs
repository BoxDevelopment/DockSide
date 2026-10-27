// Portions from the Windhawk mod "MacOS Minimize Animation" (MIT, Abdullah Masood and Potassiumuncher).

//! Geometry of one genie animation, all in physical screen pixels (DockSide is
//! per-monitor DPI aware). Ported from the mod's two engines: the modern lamp curve
//! (Potassiumuncher) and the original strip-based "classic" warp; plus our own
//! scale effect and shards.

use windows::core::w;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW, GetSystemMetrics, GetWindowRect, SM_CXSCREEN, SM_CYSCREEN};

pub const STYLE_MODERN: i32 = 0; // Potassiumuncher's lamp curve
pub const STYLE_CLASSIC: i32 = 1; // the mod's original (v2.2.0) funnel
pub const STYLE_SCALE: i32 = 2; // macOS's other effect: shrinks into the icon and fades
pub const STYLE_SHATTER: i32 = 3; // opening / closing: the window breaks into shards that fly apart
pub const STYLE_FRAGMENTS: i32 = 4; // the same with a few big pieces, each flying its own way

pub fn is_shatter_style(style: i32) -> bool {
    style == STYLE_SHATTER || style == STYLE_FRAGMENTS
}

pub const SHATTER_CELLS: i32 = 9; // ~100 shards
pub const FRAGMENT_CELLS: i32 = 5; // ~30 big pieces

const COMPAT_TASKBAR_HEIGHT: f32 = 70.0;
const COMPAT_ICON_SIZE: f32 = 33.0;
const PI: f32 = std::f32::consts::PI;
const CLASSIC_SPREAD: f32 = 0.65; // rows nearest the dock lead the morph

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Geometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Where the genie goes: the center of the window's Dock icon (and its size) when
/// it is known, else just an x on the taskbar edge.
#[derive(Clone, Copy, Default, Debug)]
pub struct DockTarget {
    pub x: i32,
    pub y: i32,    // valid with has_rect
    pub size: i32, // icon size, valid with has_rect
    pub has_rect: bool,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct GenieParams {
    /// ANIM_CLOSE: the window is gone afterwards (nothing to cache).
    pub closing: bool,
    /// DPI / 96 of the window's shadow (`window_shadow`), 0 for a window without one
    /// (maximized). The canvas leaves room for it.
    pub shadow_scale: f32,
    pub style: i32,
    /// Window (DWM extended frame) rect.
    pub win: Geometry,
    /// Modern: the icon rect the genie funnels into.
    /// Classic: x = dock x, y = dock y, width = neck width, height = 1 if the dock is
    /// above the window (taskbar at the top), else 0.
    /// Scale: the rect the window ends in, its own shape fitted into the icon.
    /// Shatter / fragments: x = columns, y = rows of the crack grid, width = seed,
    /// height = 1 for fragments.
    pub icon: Geometry,
    pub canvas_left: i32,
    pub canvas_top: i32,
    pub canvas_width: i32,
    pub canvas_height: i32,
}

// --- shadow_look.h ---------------------------------------------------------------

/// A macOS-like window shadow: a broad soft layer a little below the window and a
/// tight contact layer along its edge. Drawn under the genie, which hands over to the
/// real window: a shadow that changed there would jump.
#[derive(Clone, Copy, Debug)]
pub struct ShadowLook {
    pub sigma1: f32,
    pub dy1: f32,
    pub op1: f32, // broad shadow
    pub sigma2: f32,
    pub dy2: f32,
    pub op2: f32, // contact shadow
    pub margin: i32, // px around the window the shadow reaches
}

/// `scale` = DPI / 96.
pub fn window_shadow(scale: f32) -> ShadowLook {
    let sigma1 = 15.0 * scale;
    let dy1 = 7.0 * scale;
    ShadowLook { sigma1, dy1, op1: 0.32, sigma2: 1.8 * scale, dy2: 1.0 * scale, op2: 0.22, margin: (3.0 * sigma1 + dy1).ceil() as i32 + 2 }
}

// --- corner_shape.h --------------------------------------------------------------

/// The window corner: a superellipse |x|^n + |y|^n = 1 over `extent` px from the
/// corner, fitted to Figma's corner smoothing - extent (1 + smoothing) * radius,
/// exponent that puts its 45 degree point on the plain arc. Smoothing 0 gives n = 2,
/// Windows 11's circular corner. Returns (extent, exponent).
pub fn corner_shape(radius: f32, smoothing: f32, w: f32, h: f32) -> (f32, f32) {
    let half = w.min(h) * 0.5;
    let r = radius.min(half);
    let p = ((1.0 + smoothing) * r).min(half);
    if r <= 0.0 || p <= 0.0 {
        return (0.0, 2.0);
    }
    let diag = r * (1.0 - std::f32::consts::FRAC_1_SQRT_2) / p; // 45 degree point, fraction of the extent
    (p, std::f32::consts::LN_2 / -(1.0 - diag).ln())
}

// --- genie_effect.cpp ------------------------------------------------------------

fn monitor_rect(mon: HMONITOR) -> Option<RECT> {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe { GetMonitorInfoW(mon, &mut mi).as_bool() }.then_some(mi.rcMonitor)
}

/// Union of the monitors involved (the window's own and the one the genie funnels
/// to), so a window on another monitor than the dock target is never clipped.
fn monitor_union(win: &RECT, dock_mon: HMONITOR) -> RECT {
    let wm = monitor_rect(unsafe { MonitorFromRect(win, MONITOR_DEFAULTTONEAREST) }).unwrap_or(*win);
    let dm = (!dock_mon.is_invalid()).then(|| monitor_rect(dock_mon)).flatten().unwrap_or(wm);
    RECT { left: wm.left.min(dm.left), top: wm.top.min(dm.top), right: wm.right.max(dm.right), bottom: wm.bottom.max(dm.bottom) }
}

/// Trim the canvas to the monitors in use, but never cut the window rect or the dock
/// target column away.
fn clamp_canvas(left: &mut i32, top: &mut i32, right: &mut i32, bottom: &mut i32, win: &RECT, dock_x: i32, mons: &RECT) {
    *left = (*left).max(mons.left);
    *right = (*right).min(mons.right);
    *top = (*top).max(mons.top);
    *bottom = (*bottom).min(mons.bottom);

    *left = (*left).min(win.left);
    *right = (*right).max(win.right);
    *top = (*top).min(win.top);
    *bottom = (*bottom).max(win.bottom);
    *left = (*left).min(dock_x - 16); // icon rect is dock_x +- 11
    *right = (*right).max(dock_x + 16);
}

/// Room for the window's shadow around the canvas.
fn pad_for_shadow(g: &GenieParams, left: &mut i32, top: &mut i32, right: &mut i32, bottom: &mut i32) {
    if g.shadow_scale <= 0.0 {
        return;
    }
    let m = window_shadow(g.shadow_scale).margin;
    *left -= m;
    *top -= m;
    *right += m;
    *bottom += m;
}

fn set_canvas(out: &mut GenieParams, left: i32, top: i32, right: i32, bottom: i32) {
    out.canvas_left = left;
    out.canvas_top = top;
    out.canvas_width = (right - left).max(1);
    // One px short of the full span, as in the mod (a ghost exactly covering a monitor
    // can be treated as a fullscreen window by the shell).
    out.canvas_height = (bottom - top - 1).max(1);
}

fn lamp_vertex_macos(tx: f32, ty: f32, p: f32, w: &Geometry, i: &Geometry) -> (f32, f32) {
    let split = 0.3;
    let k = if p <= split { p / split } else { 1.0 };
    let j = if p > split { (p - split) / (1.0 - split) } else { 0.0 };

    let expand_height = i.y - w.y - w.height;
    let full_height = (i.y - w.y) - expand_height * (1.0 - k);
    let height = full_height - j * full_height;
    let safe_full = if full_height.abs() < 0.001 { 0.001 } else { full_height };

    let y = ty * height;
    let x = tx * i.width + tx * (w.width - i.width) * (1.0 - j) * (1.0 - ty) + tx * (w.width - i.width) * (1.0 - k) * ty;

    let offset_x = (i.x - w.x) * (y / (full_height + 0.1)) * k + (i.x - w.x) * j;
    let offset_y = i.y - w.y - height - expand_height * (1.0 - k);

    let effect_x = (((height - y) / safe_full) * 2.0 * PI + PI).sin() * (w.x + w.width * tx - (i.x + i.width * tx)) / 7.0 * k;

    (w.x + x + offset_x + effect_x, w.y + y + offset_y)
}

fn classic_morph(v: f32, tt: f32) -> f32 {
    let m = (tt * (1.0 + CLASSIC_SPREAD) - (1.0 - v) * CLASSIC_SPREAD).clamp(0.0, 1.0);
    m * m * (3.0 - 2.0 * m)
}

/// The classic warp: every source row slides towards the dock edge and narrows
/// towards the neck, the rows nearest the dock first.
fn classic_vertex(tx: f32, ty: f32, tt: f32, w: &Geometry, d: &Geometry) -> (f32, f32) {
    let above = d.height > 0.5;
    let e = classic_morph(if above { 1.0 - ty } else { ty }, tt);
    let id_y = w.y + w.height * ty;
    let y = id_y + (d.y - id_y) * e;
    let width = w.width + (d.width - w.width) * e;
    let cx0 = w.x + w.width * 0.5;
    let cx = cx0 + (d.x - cx0) * e;
    (cx + (tx - 0.5) * width, y)
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let s = ((x - a) / (b - a)).clamp(0.0, 1.0);
    s * s * (3.0 - 2.0 * s)
}

/// The scale effect: the window shrinks to the end rect `e` (its shape fitted into
/// the icon). The size leads - the window first draws back, then glides - and both
/// settle together, so the motion feels like one gesture rather than a slide.
fn scale_vertex(tx: f32, ty: f32, t: f32, w: &Geometry, e: &Geometry) -> (f32, f32) {
    let size = 1.0 - (1.0 - t) * (1.0 - t);
    let pos = t * t * (3.0 - 2.0 * t);
    let cx = w.x + w.width * 0.5 + (e.x + e.width * 0.5 - w.x - w.width * 0.5) * pos;
    let cy = w.y + w.height * 0.5 + (e.y + e.height * 0.5 - w.y - w.height * 0.5) * pos;
    let width = w.width + (e.width - w.width) * size;
    let height = w.height + (e.height - w.height) * size;
    (cx + (tx - 0.5) * width, cy + (ty - 0.5) * height)
}

pub fn find_taskbar_for_monitor(mon: HMONITOR) -> HWND {
    unsafe {
        let main = FindWindowW(w!("Shell_TrayWnd"), None).unwrap_or_default();
        let main_mon = MonitorFromWindow(main, MONITOR_DEFAULTTOPRIMARY);
        if mon == main_mon || mon.is_invalid() {
            return main;
        }
        let mut sec = HWND::default();
        while let Ok(s) = FindWindowExW(None, sec, w!("Shell_SecondaryTrayWnd"), None) {
            sec = s;
            if MonitorFromWindow(s, MONITOR_DEFAULTTONULL) == mon {
                return s;
            }
        }
        main
    }
}

fn screen_rect() -> RECT {
    unsafe { RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) } }
}

/// Where the dock target sits without a Dock icon: left-aligned taskbars start near
/// the monitor's left, centered ones at its middle.
pub fn default_dock_x(mon: HMONITOR) -> i32 {
    let m = monitor_rect(mon).unwrap_or_else(screen_rect);
    let mut align: u32 = 1;
    let mut size = 4u32;
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced"),
            w!("TaskbarAl"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut align as *mut u32 as *mut _),
            Some(&mut size),
        );
    }
    if align == 0 {
        m.left + 160
    } else {
        m.left + (m.right - m.left) / 2
    }
}

/// The crack grid for a w x h window: `cells` cells along its longer side, never
/// fewer than 3 along the other. Returns (cols, rows).
pub fn shatter_grid(w: i32, h: i32, cells: i32) -> (i32, i32) {
    let aspect = if h > 0 { w as f32 / h as f32 } else { 1.0 };
    let cols = if aspect >= 1.0 { cells } else { (cells as f32 * aspect).round() as i32 };
    let rows = if aspect >= 1.0 { (cells as f32 / aspect).round() as i32 } else { cells };
    (cols.max(3), rows.max(3))
}

pub fn compute_genie_params(r: &RECT, mon: HMONITOR, dock: &DockTarget, style: i32, out: &mut GenieParams) -> bool {
    let mut dock_x = dock.x;
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    if w <= 0 || h <= 0 {
        return false;
    }
    let w_geom = Geometry { x: r.left as f32, y: r.top as f32, width: w as f32, height: h as f32 };

    if is_shatter_style(style) {
        // No dock: the pieces fly up to ~0.6 of the window's diagonal away from it.
        let fragments = style == STYLE_FRAGMENTS;
        out.style = style;
        out.win = w_geom;
        let (cols, rows) = shatter_grid(w, h, if fragments { FRAGMENT_CELLS } else { SHATTER_CELLS });
        out.icon = Geometry { x: cols as f32, y: rows as f32, width: (unsafe { GetTickCount() } & 0xFFFF) as f32, height: if fragments { 1.0 } else { 0.0 } };
        let reach = (((w * w + h * h) as f32).sqrt() * 0.75) as i32;
        let (mut left, mut top, mut right, mut bottom) = (r.left - reach, r.top - reach, r.right + reach, r.bottom + reach);
        pad_for_shadow(out, &mut left, &mut top, &mut right, &mut bottom);
        clamp_canvas(&mut left, &mut top, &mut right, &mut bottom, r, r.left + w / 2, &monitor_union(r, mon));
        set_canvas(out, left, top, right, bottom);
        return true;
    }

    let m = monitor_rect(mon).unwrap_or_else(screen_rect);
    let tray = find_taskbar_for_monitor(mon);
    let mut tr = RECT::default();
    let have_tray = !tray.is_invalid() && unsafe { GetWindowRect(tray, &mut tr) }.is_ok() && tr.bottom - tr.top > 0;
    let mons = monitor_union(r, mon);
    out.style = style;
    out.win = w_geom;
    let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
    if unsafe { GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_err() {
        dpi_y = 96;
    }
    let dpi_scale = dpi_y as f32 / 96.0;

    if style == STYLE_SCALE {
        // Into the middle of the icon, or of the taskbar above the guessed x.
        let cx = dock_x as f32;
        let cy = if dock.has_rect {
            dock.y as f32
        } else if have_tray {
            (tr.top + tr.bottom) as f32 * 0.5
        } else {
            m.bottom as f32 - COMPAT_TASKBAR_HEIGHT * 0.5 * dpi_scale
        };
        // The window's own shape, as large as the icon.
        let icon = if dock.has_rect && dock.size > 0 { dock.size as f32 } else { 24.0 * dpi_scale };
        let ew = if w >= h { icon } else { icon * w as f32 / h as f32 };
        let eh = if w >= h { icon * h as f32 / w as f32 } else { icon };
        out.icon = Geometry { x: cx - ew * 0.5, y: cy - eh * 0.5, width: ew, height: eh };

        let mut left = w_geom.x.min(out.icon.x) as i32;
        let mut top = w_geom.y.min(out.icon.y) as i32;
        let mut right = (w_geom.x + w_geom.width).max(out.icon.x + ew).ceil() as i32;
        let mut bottom = (w_geom.y + w_geom.height).max(out.icon.y + eh).ceil() as i32;
        pad_for_shadow(out, &mut left, &mut top, &mut right, &mut bottom);
        clamp_canvas(&mut left, &mut top, &mut right, &mut bottom, r, cx as i32, &mons);
        set_canvas(out, left, top, right, bottom);
        return true;
    }

    if style == STYLE_CLASSIC {
        // Funnel to the taskbar edge of the monitor (top if the taskbar is in the upper
        // half), kept outside the window's own span so the row map stays monotonic.
        dock_x = dock_x.clamp(m.left, m.right.max(m.left));
        // Always the screen edge on the taskbar's side, as in the mod, below the icon
        // when it is known: the funnel pours away under the Dock.
        let mut dock_y = m.bottom as f32;
        if have_tray && (tr.top + tr.bottom) < (m.top + m.bottom) {
            dock_y = m.top as f32;
        }
        let bottom_f = (r.top + h) as f32;
        let above = dock_y < (r.top as f32 + bottom_f) * 0.5;
        if above {
            dock_y = dock_y.min(r.top as f32);
        } else if dock_y < bottom_f {
            dock_y = bottom_f;
        }
        let neck_w = (w as f32 * 0.03).clamp(12.0, 60.0);
        out.icon = Geometry { x: dock_x as f32, y: dock_y, width: neck_w, height: if above { 1.0 } else { 0.0 } };

        let mut left = r.left.min(dock_x) - w / 2;
        let mut right = (r.left + w).max(dock_x) + w / 2;
        let mut top = r.top.min(dock_y as i32);
        let mut bottom = (r.top + h).max(dock_y as i32);
        pad_for_shadow(out, &mut left, &mut top, &mut right, &mut bottom);
        clamp_canvas(&mut left, &mut top, &mut right, &mut bottom, r, dock_x, &mons);
        set_canvas(out, left, top, right, bottom);
        return true;
    }

    let taskbar_top = if have_tray { tr.top as f32 } else { m.bottom as f32 - COMPAT_TASKBAR_HEIGHT * dpi_scale };
    // The lamp curve pours the window into the line y = icon.y, icon.width wide: with
    // the icon known, that is the middle of the icon (the genie visibly goes into it);
    // otherwise the taskbar's top edge, as in the mod.
    let i_geom = if dock.has_rect {
        Geometry { x: dock.x as f32 - dock.size as f32 * 0.5, y: dock.y as f32, width: dock.size as f32, height: dock.size as f32 * 0.5 }
    } else {
        Geometry { x: dock_x as f32 - 11.0, y: taskbar_top, width: 22.0, height: COMPAT_ICON_SIZE * dpi_scale }
    };
    out.icon = i_geom;

    // Tight canvas between the window rect and the dock target. A ~W/2 pad alone can't
    // bound the curve's sway (its amplitude scales with the window-to-dock distance),
    // so the horizontal pad is the exact sway bound max(|w.x-i.x|, |w.right-i.right|)/7.
    let sway = (r.left as f32 - i_geom.x).abs().max((r.left as f32 + w as f32 - i_geom.x - i_geom.width).abs()) / 7.0;
    let pad_x = ((w / 2) as f32).max(sway) as i32 + 1;
    let mut left = r.left.min(dock_x) - pad_x;
    let mut right = (r.left + w).max(dock_x) + pad_x;
    let mut top = w_geom.y.min(i_geom.y) as i32;
    let mut bottom = (w_geom.y + w_geom.height).max(i_geom.y + i_geom.height) as i32;
    pad_for_shadow(out, &mut left, &mut top, &mut right, &mut bottom);
    clamp_canvas(&mut left, &mut top, &mut right, &mut bottom, r, dock_x, &mons);
    set_canvas(out, left, top, right, bottom);
    true
}

/// Time -> progress. Modern and scale ease in and out (smootherstep: no jolt where
/// the motion starts or stops), classic is linear (its funnel eases per row). These
/// three are symmetric, f(1 - x) = 1 - f(x), which is what lets a running genie be
/// turned around at any point without a jump (see the renderer's retarget). Shatter
/// and fragments are never turned around.
pub fn genie_ease(g: &GenieParams, raw: f32) -> f32 {
    let raw = raw.clamp(0.0, 1.0);
    if g.style == STYLE_CLASSIC {
        return raw;
    }
    if is_shatter_style(g.style) {
        // Closing eases every shard on its own (ShatterVS). Opening plays it backwards
        // (t from 1 to 0), eased out so the pieces slow down as they settle into place.
        return if g.closing { raw } else { 1.0 - (1.0 - raw) * (1.0 - raw) };
    }
    raw * raw * raw * (raw * (raw * 6.0 - 15.0) + 10.0)
}

/// Global opacity at progress t: every style melts into the icon instead of vanishing
/// at once - modern over its last 8 %, classic over its last 20 %, scale over its
/// last 40 %.
pub fn genie_fade(g: &GenieParams, t: f32) -> f32 {
    match g.style {
        STYLE_CLASSIC => 1.0 - smoothstep(0.8, 1.0, t),
        STYLE_SCALE => 1.0 - smoothstep(0.6, 1.0, t),
        STYLE_SHATTER | STYLE_FRAGMENTS => 1.0 - smoothstep(0.96, 1.0, t), // the pieces fade themselves
        _ => 1.0 - smoothstep(0.92, 1.0, t),
    }
}

/// CPU copy of the shader's vertex function (tx, ty in 0..1, progress t).
pub fn genie_vertex(g: &GenieParams, tx: f32, ty: f32, t: f32) -> (f32, f32) {
    match g.style {
        STYLE_CLASSIC => classic_vertex(tx, ty, t, &g.win, &g.icon),
        STYLE_SCALE => scale_vertex(tx, ty, t, &g.win, &g.icon),
        _ => lamp_vertex_macos(tx, ty, t, &g.win, &g.icon),
    }
}

/// True when the warped mesh has collapsed to (almost) a line: such frames are not
/// presented, otherwise they flash as a single horizontal line.
pub fn genie_frame_degenerate(g: &GenieParams, t: f32) -> bool {
    if genie_fade(g, t) <= 0.0 {
        return true;
    }
    if is_shatter_style(g.style) {
        return false; // pieces never collapse to a line
    }
    // The mesh outline is its border, so sampling the four edges bounds it.
    let n = 8;
    let (mut left, mut right, mut top, mut bottom) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for e in 0..4 {
        for s in 0..=n {
            let u = s as f32 / n as f32;
            let tx = if e == 0 || e == 1 { u } else if e == 2 { 0.0 } else { 1.0 };
            let ty = if e == 0 { 0.0 } else if e == 1 { 1.0 } else { u };
            let (px, py) = genie_vertex(g, tx, ty, t);
            left = left.min(px);
            right = right.max(px);
            top = top.min(py);
            bottom = bottom.max(py);
        }
    }
    right - left < 2.0 || bottom - top < 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(style: i32) -> GenieParams {
        GenieParams {
            style,
            win: Geometry { x: 100.0, y: 100.0, width: 800.0, height: 500.0 },
            icon: Geometry { x: 1200.0, y: 1000.0, width: 64.0, height: 32.0 },
            ..Default::default()
        }
    }

    #[test]
    fn modern_starts_as_the_window() {
        let g = params(STYLE_MODERN);
        let (x, y) = genie_vertex(&g, 0.0, 0.0, 0.0);
        assert!((x - 100.0).abs() < 1e-3 && (y - 100.0).abs() < 1e-3);
        let (x, y) = genie_vertex(&g, 1.0, 1.0, 0.0);
        assert!((x - 900.0).abs() < 1e-3 && (y - 600.0).abs() < 1e-3);
    }

    #[test]
    fn modern_ends_in_the_icon_line() {
        let g = params(STYLE_MODERN);
        for (tx, ty) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
            let (x, y) = genie_vertex(&g, tx, ty, 1.0);
            assert!((y - 1000.0).abs() < 1e-2, "y {y}");
            assert!(x >= 1200.0 - 1e-2 && x <= 1264.0 + 1e-2, "x {x}");
        }
        assert!(genie_frame_degenerate(&g, 1.0));
    }

    #[test]
    fn eases_are_symmetric() {
        for style in [STYLE_MODERN, STYLE_CLASSIC, STYLE_SCALE] {
            let g = params(style);
            for i in 0..=10 {
                let x = i as f32 / 10.0;
                assert!((genie_ease(&g, 1.0 - x) - (1.0 - genie_ease(&g, x))).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn scale_goes_from_window_to_end_rect() {
        let g = params(STYLE_SCALE);
        assert_eq!(genie_vertex(&g, 0.0, 0.0, 0.0), (100.0, 100.0));
        let (x, y) = genie_vertex(&g, 1.0, 1.0, 1.0);
        assert!((x - 1264.0).abs() < 1e-3 && (y - 1032.0).abs() < 1e-3);
    }

    #[test]
    fn plain_corner_is_circular() {
        let (extent, exponent) = corner_shape(8.0, 0.0, 800.0, 500.0);
        assert_eq!(extent, 8.0);
        assert!((exponent - 2.0).abs() < 1e-4);
    }

    #[test]
    fn shatter_grid_follows_the_aspect() {
        assert_eq!(shatter_grid(900, 300, 9), (9, 3));
        assert_eq!(shatter_grid(300, 900, 9), (3, 9));
    }
}
