//! macOS-style animations for other apps' windows: minimize into the Dock
//! (Genie or Scale), restore out of it, and a zoom-and-fade on close.
//!
//! Windows' own animations can't be hooked, so they're switched off and drawn
//! again on top: "Animate windows when minimizing and maximizing" is turned off
//! for the session (never written to the profile, and put back on exit), and a
//! close-animated window gets DWM's per-window "no transitions" flag. Events come
//! from hooks DockSide already has (minimize start/end, foreground, the shell
//! hook), after the fact: a minimizing window has already gone, so its image
//! comes from a DWM thumbnail (see `gpu::Grab`), and a closing window's from a
//! throttled capture of the focused window kept for that moment, which also
//! lets the usual minimize (of the focused window) start without waiting.
//!
//! The overlay and its swap chain exist only while something animates.

mod gpu;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SHIFT};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::motion_enabled;
use crate::config::{AnimSpeed, CloseEffect, MinimizeEffect, WindowsCfg};
use crate::geom::Rect;
use crate::win::apps;
use gpu::{Capturer, Gpu, Grab, Mesh, Overlay, Shadow, Snapshot, Station, Vertex, ROWS};

const GENIE_SECS: f32 = 0.5;
const SCALE_SECS: f32 = 0.32;
const CLOSE_SECS: f32 = 0.2;
/// Shift held as a window minimizes plays it in slow motion, as on a Mac.
const SLOW_MOTION: f32 = 6.0;
/// Longest the stand-in thumbnail holds the motion's first frames, in seconds.
const MAX_STAND_IN: f64 = 0.034;
/// A restored window gets this long to settle under the last frame once it
/// has drawn itself, and at most `LAND_TIMEOUT_MS` in all.
const SETTLE_MS: u128 = 100;
const LAND_TIMEOUT_MS: u128 = 500;
/// The cross-fade from the last frame to the real window, in seconds.
const HANDOFF_SECS: f64 = 0.12;
/// Give up on a window image that hasn't arrived by then.
const GRAB_TIMEOUT_MS: u128 = 400;
/// At most this many windows animate at once (Win+M minimizes everything).
const MAX_ANIMS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Minimize,
    Restore,
    Close,
}

struct Anim {
    hwnd: HWND,
    kind: Kind,
    genie: bool,
    /// The window on screen, and the Dock icon (screen pixels).
    window: Rect,
    icon: Rect,
    radius: f32,
    secs: f32,
    grab: Option<Grab>,
    /// Its image is coming from the station.
    from_station: bool,
    /// Restore: a fresher image of the window is coming from the station, to
    /// replace the kept one while the window is still small.
    refresh: bool,
    image: Option<Snapshot>,
    /// A DWM thumbnail standing in on the overlay until `image` arrives.
    stand_in: Option<isize>,
    created: Instant,
    /// Frame time (see `frame_time`) of its first frame.
    started: Option<f64>,
    /// The window's drop shadow, drawn with it (Windows draws it outside the
    /// window, so captures don't have it).
    shadow: Shadow,
    /// Restore: when the real window was shown. The last frame stays up until
    /// the window has drawn itself and settled (apps redraw over a few frames
    /// after a restore), then cross-fades to it from `fading` (frame time).
    landed: Option<(Instant, i64)>,
    fading: Option<f64>,
    /// Minimize: where the window goes back to, kept with its image.
    rect: RECT,
    zoomed: bool,
}

impl Anim {
    fn new(hwnd: HWND, kind: Kind, window: Rect, icon: Rect, radius: f32, secs: f32) -> Self {
        Self {
            hwnd,
            kind,
            genie: false,
            window,
            icon,
            radius,
            secs,
            grab: None,
            from_station: false,
            refresh: false,
            image: None,
            stand_in: None,
            created: Instant::now(),
            started: None,
            shadow: Shadow::NONE,
            landed: None,
            fading: None,
            rect: RECT::default(),
            zoomed: false,
        }
    }

    /// The image is here: the clock starts. Shift held: slow motion.
    /// Starts the clock now. Shift held: slow motion.
    fn start(&mut self) {
        self.started = Some(frame_time());
        if self.kind != Kind::Close && unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0 {
            self.secs *= SLOW_MOTION;
        }
    }

    /// The image is here. A clock already running (a stand-in thumbnail held the
    /// first frames) carries on, unless the image was so late that the motion
    /// would jump to catch up.
    fn begin(&mut self, image: Snapshot) {
        self.image = Some(image);
        self.grab = None;
        self.from_station = false;
        match self.started {
            None => self.start(),
            Some(s) => self.started = Some(s.max(frame_time() - MAX_STAND_IN)),
        }
    }

    /// 0..1 through the animation at frame time `now`.
    fn progress(&self, now: f64) -> f32 {
        self.started.map_or(0.0, |s| (((now - s) / self.secs as f64) as f32).clamp(0.0, 1.0))
    }

    fn rows(&self, now: f64, out: &mut Vec<Vertex>) {
        let u = self.progress(now);
        let pad = self.shadow.margin();
        match self.kind {
            Kind::Close => close_rows(self.window, u, pad, out),
            Kind::Minimize | Kind::Restore => {
                // A restore plays the minimize backwards.
                let p = if self.kind == Kind::Minimize { u } else { 1.0 - u };
                if self.genie && self.icon.y > self.window.y + 1.0 {
                    genie_rows(self.window, self.icon, p, pad, out)
                } else {
                    scale_rows(self.window, self.icon, p, pad, out)
                }
            }
        }
    }

    /// Everything it can cover, shadow included.
    fn bounds(&self) -> Rect {
        let m = self.shadow.margin();
        Rect::new(self.window.x - m, self.window.y - m, self.window.w + 2.0 * m, self.window.h + 2.0 * m).union(self.icon)
    }
}

/// What the app should do after a frame.
#[derive(Default)]
pub struct Tick {
    /// Still animating: keep the frame clock running.
    pub busy: bool,
    /// Windows whose restore animation has landed: show them now.
    pub activate: Vec<HWND>,
}

pub struct WindowFx {
    cfg: WindowsCfg,
    gpu: Option<Gpu>,
    capturer: Option<Capturer>,
    /// Exact last frames of minimizing windows.
    station: Option<Station>,
    overlay: Option<Overlay>,
    anims: Vec<Anim>,
    /// The window whose frames `capturer` keeps.
    followed: Option<HWND>,
    /// Last known on-screen bounds per window, and whether it was maximized.
    bounds: HashMap<isize, (RECT, bool)>,
    /// Images of windows we minimized (newest last), so a restore starts at once.
    minimized: Vec<Parked>,
    /// We turned Windows' minimize animation off (and owe it back).
    min_animate_off: bool,
    /// Windows we set DWM's "no transitions" flag on.
    no_transitions: HashSet<isize>,
    own_pid: u32,
    rows: Vec<Vertex>,
}

/// A minimized window's image and where it goes back to.
struct Parked {
    hwnd: HWND,
    image: Snapshot,
    rect: RECT,
    zoomed: bool,
}

/// Minimized windows whose images are kept (each about the window's size in GPU memory).
const PARKED: usize = 4;

impl WindowFx {
    pub fn new(cfg: &WindowsCfg) -> Self {
        let mut fx = Self {
            cfg: native(),
            gpu: None,
            capturer: None,
            station: None,
            overlay: None,
            anims: Vec::new(),
            followed: None,
            bounds: HashMap::new(),
            minimized: Vec::new(),
            min_animate_off: false,
            no_transitions: HashSet::new(),
            own_pid: unsafe { GetCurrentProcessId() },
            rows: Vec::with_capacity(ROWS * 2),
        };
        fx.configure(cfg);
        fx
    }

    /// Duration multiplier for the chosen speed.
    fn speed(&self) -> f32 {
        match self.cfg.speed {
            AnimSpeed::Slow => 1.6,
            AnimSpeed::Normal => 1.0,
            AnimSpeed::Fast => 0.65,
        }
    }

    fn custom_minimize(&self) -> bool {
        self.cfg.minimize_effect != MinimizeEffect::Windows
    }

    fn custom_close(&self) -> bool {
        self.cfg.close_effect != CloseEffect::Windows
    }

    /// Applies (new) settings: hands animations to Windows or takes them over.
    pub fn configure(&mut self, cfg: &WindowsCfg) {
        self.cfg = cfg.clone();
        if (self.custom_minimize() || self.custom_close()) && self.gpu.is_none() {
            match Gpu::new().and_then(|g| Capturer::new(&g).map(|c| (g, c))) {
                Ok((g, c)) => {
                    // Made now and parked: bringing up a new window costs the first animation frames.
                    self.overlay = Overlay::new(&g, home_rect()).map_err(|e| log!("window animation overlay: {e}")).ok();
                    // Shown once (blank, click-through) and parked: the first minimize
                    // after launch otherwise pays for its first composition.
                    if let Some(o) = &mut self.overlay {
                        if o.draw(&g, &[]).is_ok() {
                            o.park(&g);
                        }
                    }
                    self.station = Station::new(&c).map_err(|e| log!("window capture station: {e}")).ok();
                    self.gpu = Some(g);
                    self.capturer = Some(c);
                }
                Err(e) => {
                    log!("window animations unavailable: {e}");
                    self.cfg = native();
                }
            }
        }
        if self.custom_minimize() != self.min_animate_off {
            if self.custom_minimize() {
                // Only if the user has it on; and never saved, so a crash can't keep it off past sign-out.
                if minimize_animation_preferred() {
                    set_minimize_animation(false);
                    self.min_animate_off = true;
                }
            } else {
                set_minimize_animation(true);
                self.min_animate_off = false;
            }
        }
        if self.custom_close() {
            for w in apps::list_windows() {
                self.on_shown(w.hwnd);
            }
        } else {
            for h in self.no_transitions.drain() {
                gpu::set_transitions_disabled(HWND(h as _), false);
            }
        }
        if !self.custom_minimize() {
            self.minimized.clear();
        }
        if self.custom_minimize() || self.custom_close() {
            self.followed = None;
            self.on_foreground(unsafe { GetForegroundWindow() });
        } else {
            self.stop_all();
            self.overlay = None;
            self.station = None;
            self.capturer = None;
            self.gpu = None;
            self.followed = None;
        }
    }

    fn own(&self, hwnd: HWND) -> bool {
        apps::window_pid(hwnd) == self.own_pid
    }

    /// The focused window changed: remember where it is, and keep its image
    /// for a close or minimize.
    pub fn on_foreground(&mut self, hwnd: HWND) {
        if hwnd.is_invalid() || self.own(hwnd) || !apps::is_app_window(hwnd) {
            return;
        }
        self.note_bounds(hwnd);
        if self.followed == Some(hwnd) {
            return;
        }
        let Some(capturer) = &self.capturer else { return };
        // A game or video filling the screen isn't captured: it could cost the app its direct flip.
        let fullscreen = self.bounds.get(&(hwnd.0 as isize)).is_some_and(|(r, _)| covers_monitor(*r));
        self.followed = (!fullscreen).then_some(hwnd);
        capturer.follow(self.followed);
    }

    /// A window finished moving or resizing.
    pub fn on_move_end(&mut self, hwnd: HWND) {
        if !hwnd.is_invalid() && !self.own(hwnd) {
            self.note_bounds(hwnd);
        }
    }

    fn note_bounds(&mut self, hwnd: HWND) {
        if unsafe { IsIconic(hwnd).as_bool() } {
            return;
        }
        if let Some(r) = gpu::window_bounds(hwnd) {
            self.bounds.insert(hwnd.0 as isize, (r, unsafe { IsZoomed(hwnd).as_bool() }));
        }
    }

    /// An app window appeared (shell hook).
    pub fn on_shown(&mut self, hwnd: HWND) {
        if self.custom_close() && !self.own(hwnd) && !self.no_transitions.contains(&(hwnd.0 as isize)) && gpu::set_transitions_disabled(hwnd, true) {
            self.no_transitions.insert(hwnd.0 as isize);
        }
    }

    /// An app window was hidden or destroyed (shell hook). Returns true when a
    /// close animation started.
    pub fn on_hidden(&mut self, hwnd: HWND) -> bool {
        let key = hwnd.0 as isize;
        self.no_transitions.remove(&key);
        let known = self.bounds.remove(&key);
        self.minimized.retain(|p| p.hwnd != hwnd);
        if self.followed == Some(hwnd) {
            self.followed = None;
        }
        // Still on screen: just taken off the taskbar.
        if unsafe { IsWindow(hwnd).as_bool() && IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() } {
            return false;
        }
        if !motion_enabled() || self.cfg.close_effect != CloseEffect::Fade || self.anims.len() >= MAX_ANIMS {
            return false;
        }
        let (Some(gpu), Some(capturer)) = (&self.gpu, &self.capturer) else { return false };
        let Some((image, rect, zoomed)) = capturer.take(gpu, hwnd) else { return false };
        let image = match image {
            Ok(i) => i,
            Err(e) => return self.fail(e),
        };
        let rect = moved(rect, known);
        let window = to_rect(rect);
        let mut anim = Anim::new(hwnd, Kind::Close, window, window, corner_radius(hwnd, zoomed, rect), CLOSE_SECS * self.speed());
        anim.shadow = shadow(hwnd, zoomed, rect);
        anim.begin(image);
        self.anims.push(anim);
        self.fit_overlay()
    }

    /// A window started minimizing (it's already gone from the screen). `icon`
    /// is its Dock icon, if it has one. Returns true when an animation started.
    pub fn on_minimize_start(&mut self, hwnd: HWND, icon: Option<RECT>) -> bool {
        if !self.custom_minimize() || !motion_enabled() || self.own(hwnd) {
            return false;
        }
        let (Some(gpu), Some(capturer)) = (&self.gpu, &self.capturer) else { return false };
        self.anims.retain(|a| a.hwnd != hwnd);
        if self.anims.len() >= MAX_ANIMS {
            return false;
        }
        // Its last focused frame: where it was, and an image if the station is busy.
        let focused = capturer.take(gpu, hwnd).and_then(|(image, rect, zoomed)| image.ok().map(|i| (i, rect, zoomed)));
        // Windows' last frame of it, exactly as it left the screen (it has already
        // redrawn unfocused by then): a live thumbnail shows at once, and the
        // station has the image to bend a frame or two later.
        let station_size = self.station.as_mut().filter(|s| !s.busy()).and_then(|s| s.request(hwnd));
        if let Some(size) = station_size {
            let known = focused.as_ref().map(|(_, r, z)| (moved(*r, self.bounds.get(&(hwnd.0 as isize)).copied()), *z));
            let Some((rect, zoomed)) = known.filter(|(r, _)| r.right - r.left == size.0 && r.bottom - r.top == size.1).or_else(|| self.window_rect(hwnd, size)) else {
                if let Some(s) = &mut self.station {
                    s.cancel();
                }
                return false;
            };
            let mut anim = self.minimize_anim(hwnd, Kind::Minimize, rect, zoomed, icon);
            anim.from_station = true;
            anim.start();
            self.anims.push(anim);
            return self.show_stand_in(hwnd, rect);
        }
        // The station is busy (several at once): the focused frame, if there is one.
        if let Some((image, rect, zoomed)) = focused {
            let rect = moved(rect, self.bounds.get(&(hwnd.0 as isize)).copied());
            let mut anim = self.minimize_anim(hwnd, Kind::Minimize, rect, zoomed, icon);
            anim.begin(image);
            self.anims.push(anim);
            return self.fit_overlay();
        }
        // Any other window: through a thumbnail, which shows in its place until the image arrives.
        let grab = match Grab::start(capturer, hwnd) {
            Ok(g) => g,
            Err(e) => {
                log!("window minimize capture failed: {e}");
                return false;
            }
        };
        let Some((rect, zoomed)) = self.window_rect(hwnd, grab.size) else { return false };
        let mut anim = self.minimize_anim(hwnd, Kind::Minimize, rect, zoomed, icon);
        anim.grab = Some(grab);
        self.anims.push(anim);
        self.show_stand_in(hwnd, rect)
    }

    /// Puts the newest animation's window back where it was, as a live DWM
    /// thumbnail on the overlay (with our shadow around it), until its image
    /// arrives. Returns false when the overlay can't go up.
    fn show_stand_in(&mut self, hwnd: HWND, rect: RECT) -> bool {
        // The thumbnail first: the overlay's first frame then shows it.
        let fits = self.fit_overlay_rect();
        if let (Some(o), Some(a), true) = (&self.overlay, self.anims.last_mut(), fits) {
            let dest = RECT { left: rect.left - o.rect.left, top: rect.top - o.rect.top, right: rect.right - o.rect.left, bottom: rect.bottom - o.rect.top };
            a.stand_in = gpu::thumbnail(o.hwnd, hwnd, dest);
        }
        fits && self.fit_overlay()
    }

    fn minimize_anim(&self, hwnd: HWND, kind: Kind, rect: RECT, zoomed: bool, icon: Option<RECT>) -> Anim {
        let icon = icon.map(to_rect).unwrap_or_else(|| fallback_icon(rect));
        let mut a = Anim::new(hwnd, kind, to_rect(rect), icon, corner_radius(hwnd, zoomed, rect), duration(self.cfg.minimize_effect) * self.speed());
        a.shadow = shadow(hwnd, zoomed, rect);
        a.genie = self.cfg.minimize_effect == MinimizeEffect::Genie;
        a.rect = rect;
        a.zoomed = zoomed;
        a
    }

    /// Windows reports the end of a minimize both when it finishes and when the
    /// window comes back. Only the latter matters: a minimize still playing is
    /// cut short, and its kept image is stale.
    pub fn on_minimize_end(&mut self, hwnd: HWND) {
        if unsafe { IsIconic(hwnd).as_bool() } {
            return;
        }
        let station = &mut self.station;
        self.anims.retain(|a| {
            let cut = a.hwnd == hwnd && a.kind == Kind::Minimize;
            if cut {
                if let Some(id) = a.stand_in {
                    gpu::drop_thumbnail(id);
                }
                if let (true, Some(s)) = (a.from_station, station.as_mut()) {
                    s.cancel();
                }
            }
            !cut
        });
        self.minimized.retain(|p| p.hwnd != hwnd);
        if self.anims.is_empty() {
            self.park_overlay();
        }
    }

    /// Restores a minimized window out of its Dock icon. Returns false when it
    /// isn't animated (the caller restores it right away); otherwise the window
    /// is handed back in `Tick::activate` once the animation lands.
    pub fn restore(&mut self, hwnd: HWND, icon: Option<RECT>) -> bool {
        if !self.custom_minimize() || !self.cfg.animate_restore || !motion_enabled() || !unsafe { IsIconic(hwnd).as_bool() } {
            return false;
        }
        let Some(capturer) = &self.capturer else { return false };
        if self.anims.iter().any(|a| a.hwnd == hwnd && a.kind == Kind::Restore) {
            return true; // already on its way
        }
        self.anims.retain(|a| a.hwnd != hwnd);
        if self.anims.len() >= MAX_ANIMS {
            return false;
        }
        // The image we kept when it minimized, or else a fresh one through a thumbnail.
        let anim = match self.minimized.iter().position(|p| p.hwnd == hwnd) {
            Some(i) => {
                let p = self.minimized.remove(i);
                let mut anim = self.minimize_anim(hwnd, Kind::Restore, p.rect, p.zoomed, icon);
                anim.begin(p.image);
                // Windows' own last frame of it, in case it changed since.
                let size = (p.rect.right - p.rect.left, p.rect.bottom - p.rect.top);
                if let Some(station) = self.station.as_mut().filter(|s| !s.busy()) {
                    anim.refresh = station.request(hwnd) == Some(size);
                    if !anim.refresh {
                        station.cancel();
                    }
                }
                anim
            }
            None if self.station.as_ref().is_some_and(|s| !s.busy()) => {
                let Some(size) = self.station.as_mut().and_then(|s| s.request(hwnd)) else { return false };
                let Some((rect, zoomed)) = self.window_rect(hwnd, size) else { return false };
                let mut anim = self.minimize_anim(hwnd, Kind::Restore, rect, zoomed, icon);
                anim.from_station = true;
                anim
            }
            None => {
                let grab = match Grab::start(capturer, hwnd) {
                    Ok(g) => g,
                    Err(e) => {
                        log!("window restore capture failed: {e}");
                        return false;
                    }
                };
                let Some((rect, zoomed)) = self.window_rect(hwnd, grab.size) else { return false };
                let mut anim = self.minimize_anim(hwnd, Kind::Restore, rect, zoomed, icon);
                anim.grab = Some(grab);
                anim
            }
        };
        self.anims.push(anim);
        if self.fit_overlay() {
            true
        } else {
            self.anims.retain(|a| a.hwnd != hwnd);
            false
        }
    }

    /// Where a (possibly minimized) window of `size` (as DWM keeps it) is or
    /// will be restored to on screen, and whether it's maximized.
    fn window_rect(&self, hwnd: HWND, size: (i32, i32)) -> Option<(RECT, bool)> {
        let close = |r: &RECT| ((r.right - r.left) - size.0).abs() <= 2 && ((r.bottom - r.top) - size.1).abs() <= 2;
        // Where it last was (focused, moved), or else its restore placement.
        let known = self
            .capturer
            .as_ref()
            .and_then(|c| c.last_rect(hwnd))
            .map(|r| (r, unsafe { IsZoomed(hwnd).as_bool() }))
            .or_else(|| self.bounds.get(&(hwnd.0 as isize)).copied())
            .filter(|(r, _)| close(r));
        if let Some(known) = known {
            return Some(known);
        }
        let (r, zoomed) = placement_rect(hwnd)?;
        // Centered on the placement, at the size DWM reports (snapped windows differ from their placement).
        let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        let r = if zoomed { r } else { RECT { left: cx - size.0 / 2, top: cy - size.1 / 2, right: cx - size.0 / 2 + size.0, bottom: cy - size.1 / 2 + size.1 } };
        Some((r, zoomed))
    }

    /// Sizes the overlay to cover every animation (creating it if needed).
    /// Returns false, having given up on everything, when the GPU fails.
    fn fit_overlay(&mut self) -> bool {
        let Some(gpu) = &self.gpu else { return false };
        let Some(need) = self.anims.iter().map(Anim::bounds).reduce(Rect::union) else { return true };
        let need = clip_to_screen(RECT {
            left: need.x.floor() as i32 - 2,
            top: need.y.floor() as i32 - 2,
            right: need.right().ceil() as i32 + 2,
            bottom: need.bottom().ceil() as i32 + 2,
        });
        if self.overlay.is_none() {
            match Overlay::new(gpu, need) {
                Ok(o) => self.overlay = Some(o),
                Err(e) => return self.fail(e),
            }
        }
        let o = self.overlay.as_mut().unwrap();
        let r = o.rect;
        let covered = need.left >= r.left && need.top >= r.top && need.right <= r.right && need.bottom <= r.bottom;
        // It only ever grows (to the monitor at least), so the usual animation starts
        // without resizing anything.
        let result = if covered { Ok(()) } else { o.set_rect(union(union(r, need), monitor_info(need).rcMonitor)) };
        // The first frame goes up straight away (and shows the overlay).
        match result.and_then(|_| self.render(frame_time())) {
            Ok(()) => true,
            Err(e) => !self.fail(e),
        }
    }

    /// Makes sure the overlay exists and covers every animation, without drawing.
    fn fit_overlay_rect(&mut self) -> bool {
        let Some(gpu) = &self.gpu else { return false };
        let Some(need) = self.anims.iter().map(Anim::bounds).reduce(Rect::union) else { return true };
        let need = clip_to_screen(RECT {
            left: need.x.floor() as i32 - 2,
            top: need.y.floor() as i32 - 2,
            right: need.right().ceil() as i32 + 2,
            bottom: need.bottom().ceil() as i32 + 2,
        });
        if self.overlay.is_none() {
            match Overlay::new(gpu, need) {
                Ok(o) => self.overlay = Some(o),
                Err(e) => return self.fail(e),
            }
        }
        let o = self.overlay.as_mut().unwrap();
        let r = o.rect;
        if need.left >= r.left && need.top >= r.top && need.right <= r.right && need.bottom <= r.bottom {
            return true;
        }
        match o.set_rect(union(union(r, need), monitor_info(need).rcMonitor)) {
            Ok(()) => true,
            Err(e) => !self.fail(e),
        }
    }

    /// Advances and draws every animation.
    pub fn tick(&mut self) -> Tick {
        let mut tick = Tick::default();
        if self.anims.is_empty() {
            self.park_overlay();
            return tick;
        }
        let Some(gpu) = &self.gpu else {
            self.stop_all();
            return tick;
        };
        let now = frame_time();
        let mut failed = None;
        for a in &mut self.anims {
            if a.refresh {
                match self.station.as_mut().and_then(|s| s.take(gpu)) {
                    Some(Ok(image)) => {
                        a.image = Some(image);
                        a.refresh = false;
                    }
                    Some(Err(_)) => a.refresh = false,
                    None => {}
                }
                continue;
            }
            if a.image.is_some() {
                continue;
            }
            let taken = if a.from_station { self.station.as_mut().and_then(|s| s.take(gpu)) } else { a.grab.as_ref().and_then(|g| g.take(gpu)) };
            match taken {
                Some(Ok(image)) => a.begin(image),
                Some(Err(e)) => failed = Some(e),
                None => {}
            }
        }
        if let Some(e) = failed {
            self.fail(e);
            return tick;
        }
        // Images that never came: drop the animation (a restore still restores).
        self.anims.retain(|a| {
            let stale = a.image.is_none() && a.created.elapsed().as_millis() > GRAB_TIMEOUT_MS;
            if stale && a.kind == Kind::Restore {
                tick.activate.push(a.hwnd);
            }
            if stale {
                if let Some(id) = a.stand_in {
                    gpu::drop_thumbnail(id);
                }
                if a.from_station {
                    if let Some(s) = &mut self.station {
                        s.cancel();
                    }
                }
            }
            !stale
        });

        // Landed restores show the real window under the last frame.
        for a in &mut self.anims {
            if a.kind == Kind::Restore && a.progress(now) >= 1.0 && a.landed.is_none() {
                a.landed = Some((Instant::now(), gpu::qpc_now()));
                tick.activate.push(a.hwnd);
            }
        }

        if let Err(e) = self.render(now) {
            self.fail(e);
            return tick;
        }
        let mut landed = Vec::new();
        let capturer = self.capturer.as_ref();
        self.anims.retain_mut(|a| {
            let keep = match (a.kind, a.landed, a.fading) {
                (Kind::Restore, Some(_), Some(from)) => now - from < HANDOFF_SECS,
                // Until the window has drawn itself since it was shown (seen by the
                // capture that follows the focused window) and settled, or it's
                // taking too long: then the cross-fade starts.
                (Kind::Restore, Some((at, since)), None) => {
                    let ms = at.elapsed().as_millis();
                    let drawn = capturer.is_some_and(|c| c.shown_since(a.hwnd, since));
                    if (drawn && ms >= SETTLE_MS) || ms >= LAND_TIMEOUT_MS {
                        a.fading = Some(now);
                    }
                    true
                }
                (Kind::Restore, None, _) => true,
                _ => a.progress(now) < 1.0,
            };
            // A minimized window's image stays for its restore.
            if !keep && a.kind == Kind::Minimize {
                if let Some(image) = a.image.take() {
                    landed.push(Parked { hwnd: a.hwnd, image, rect: a.rect, zoomed: a.zoomed });
                }
            }
            keep
        });
        for p in landed {
            self.minimized.retain(|m| m.hwnd != p.hwnd);
            if self.minimized.len() >= PARKED {
                self.minimized.remove(0);
            }
            self.minimized.push(p);
        }
        if self.anims.is_empty() {
            self.park_overlay();
        }
        tick.busy = !self.anims.is_empty();
        tick
    }

    /// Draws every animation that has its image (showing the overlay if it was
    /// parked), then retires the stand-in thumbnails it replaces.
    fn render(&mut self, now: f64) -> windows::core::Result<()> {
        let (Some(gpu), Some(overlay)) = (&self.gpu, &mut self.overlay) else { return Ok(()) };
        let mut meshes: Vec<(usize, usize)> = Vec::new();
        self.rows.clear();
        for (i, a) in self.anims.iter().enumerate() {
            // No image yet: only a stand-in's shadow, held at the start.
            if a.image.is_none() && a.stand_in.is_none() {
                continue;
            }
            let start = self.rows.len();
            meshes.push((i, start));
            a.rows(if a.image.is_some() { now } else { a.started.unwrap_or(now) }, &mut self.rows);
            if let Some(from) = a.fading {
                let k = (1.0 - (now - from) / HANDOFF_SECS).clamp(0.0, 1.0) as f32;
                self.rows[start..].iter_mut().for_each(|v| v.a *= k);
            }
        }
        let drawn: Vec<Mesh> = meshes
            .iter()
            .enumerate()
            .map(|(n, &(i, start))| {
                let end = meshes.get(n + 1).map_or(self.rows.len(), |m| m.1);
                let a = &self.anims[i];
                let size = a.image.as_ref().map_or((a.window.w, a.window.h), |i| i.size);
                Mesh { image: a.image.as_ref(), size, rows: &self.rows[start..end], radius: a.radius, shadow: a.shadow }
            })
            .collect();
        overlay.draw(gpu, &drawn)?;
        // The real frame is up: the stand-in thumbnails can go.
        for a in &mut self.anims {
            if a.image.is_some() {
                if let Some(id) = a.stand_in.take() {
                    gpu::drop_thumbnail(id);
                }
            }
        }
        Ok(())
    }

    /// Hides the overlay until the next animation.
    fn park_overlay(&mut self) {
        if let (Some(o), Some(gpu)) = (&mut self.overlay, &self.gpu) {
            o.park(gpu);
        }
    }

    /// Something on the GPU failed (often a driver reset): drop everything, to
    /// be rebuilt on the next animation. Pending restores are lost, so callers
    /// only fail while nothing is waiting on one. Returns false.
    fn fail(&mut self, e: windows::core::Error) -> bool {
        log!("window animation failed: {e}");
        let restores: Vec<HWND> = self.anims.iter().filter(|a| a.kind == Kind::Restore).map(|a| a.hwnd).collect();
        self.stop_all();
        for h in restores {
            apps::activate(h);
        }
        self.minimized.clear();
        self.followed = None;
        self.overlay = None;
        self.station = None;
        self.capturer = None;
        self.gpu = None;
        match Gpu::new().and_then(|g| Capturer::new(&g).map(|c| (g, c))) {
            Ok((g, c)) => {
                self.station = Station::new(&c).map_err(|e| log!("window capture station: {e}")).ok();
                self.gpu = Some(g);
                self.capturer = Some(c);
                self.on_foreground(unsafe { GetForegroundWindow() });
            }
            Err(e) => log!("window animations unavailable: {e}"),
        }
        false
    }

    fn stop_all(&mut self) {
        for a in self.anims.drain(..) {
            if let Some(id) = a.stand_in {
                gpu::drop_thumbnail(id);
            }
        }
        if let Some(s) = &mut self.station {
            s.cancel();
        }
        self.park_overlay();
    }

    pub fn busy(&self) -> bool {
        !self.anims.is_empty()
    }
}

impl Drop for WindowFx {
    fn drop(&mut self) {
        self.stop_all();
        if self.min_animate_off {
            set_minimize_animation(true);
        }
        for h in self.no_transitions.drain() {
            gpu::set_transitions_disabled(HWND(h as _), false);
        }
    }
}

/// Everything left to Windows.
fn native() -> WindowsCfg {
    WindowsCfg { minimize_effect: MinimizeEffect::Windows, animate_restore: false, close_effect: CloseEffect::Windows, speed: AnimSpeed::Normal }
}

fn duration(effect: MinimizeEffect) -> f32 {
    if effect == MinimizeEffect::Genie { GENIE_SECS } else { SCALE_SECS }
}

/// The user's own "Animate windows when minimizing and maximizing" choice, as
/// saved in their profile (the live value may be ours).
fn minimize_animation_preferred() -> bool {
    let mut buf = [0u16; 8];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::w!("Control Panel\\Desktop\\WindowMetrics"),
            windows::core::w!("MinAnimate"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as _),
            Some(&mut len),
        )
    };
    r.is_err() || buf[0] != u16::from(b'0')
}

/// Turns Windows' minimize/maximize animation on or off for this session only.
fn set_minimize_animation(on: bool) {
    let mut info = ANIMATIONINFO { cbSize: std::mem::size_of::<ANIMATIONINFO>() as u32, iMinAnimate: on as i32 };
    unsafe {
        if let Err(e) = SystemParametersInfoW(SPI_SETANIMATION, info.cbSize, Some(&mut info as *mut _ as _), SPIF_SENDCHANGE) {
            log!("minimize animation setting failed: {e}");
        }
    }
}

/// A window's restore position in screen pixels, and whether it restores maximized.
fn placement_rect(hwnd: HWND) -> Option<(RECT, bool)> {
    let mut p = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    unsafe { GetWindowPlacement(hwnd, &mut p).ok()? };
    let info = monitor_info(p.rcNormalPosition);
    if p.flags.0 & WPF_RESTORETOMAXIMIZED.0 != 0 || unsafe { IsZoomed(hwnd).as_bool() } {
        return Some((info.rcWork, true));
    }
    // Placement is in workspace coordinates (relative to the work area) unless it's a tool window.
    let ex = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    let (dx, dy) = if ex & WS_EX_TOOLWINDOW.0 != 0 { (0, 0) } else { (info.rcWork.left - info.rcMonitor.left, info.rcWork.top - info.rcMonitor.top) };
    let r = p.rcNormalPosition;
    Some((RECT { left: r.left + dx, top: r.top + dy, right: r.right + dx, bottom: r.bottom + dy }, false))
}

/// Where a window whose last frame was at `frame` is now: moving it sends no
/// new frame, but a move (or focus) since then was noted in `known`.
fn moved(frame: RECT, known: Option<(RECT, bool)>) -> RECT {
    match known {
        Some((r, _)) if r.right - r.left == frame.right - frame.left && r.bottom - r.top == frame.bottom - frame.top => r,
        _ => frame,
    }
}

fn monitor_info(r: RECT) -> MONITORINFO {
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let _ = GetMonitorInfoW(MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST), &mut info);
    }
    info
}

/// The primary monitor, where the overlay starts out.
fn home_rect() -> RECT {
    monitor_info(RECT { left: 0, top: 0, right: 1, bottom: 1 }).rcMonitor
}

fn covers_monitor(r: RECT) -> bool {
    let m = monitor_info(r).rcMonitor;
    r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
}

/// With no Dock: into the bottom edge of the window's monitor, under its middle.
fn fallback_icon(window: RECT) -> Rect {
    let m = monitor_info(window).rcMonitor;
    let s = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() } as f32 / 96.0;
    let cx = ((window.left + window.right) / 2).clamp(m.left, m.right) as f32;
    Rect::new(cx - 24.0 * s, m.bottom as f32 - 4.0 * s, 48.0 * s, 48.0 * s)
}

/// When the frame being drawn now reaches the screen, in seconds: the next
/// vblank DWM reports. Motion then follows the display's steady beat, not the
/// moment our frame message happened to run.
fn frame_time() -> f64 {
    let (mut now, mut freq) = (0i64, 1i64);
    let mut info = DWM_TIMING_INFO { cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32, ..Default::default() };
    unsafe {
        let _ = QueryPerformanceFrequency(&mut freq);
        if DwmGetCompositionTimingInfo(HWND::default(), &mut info).is_ok() && info.qpcRefreshPeriod > 0 {
            return (info.qpcVBlank + info.qpcRefreshPeriod) as f64 / freq.max(1) as f64;
        }
        let _ = QueryPerformanceCounter(&mut now);
    }
    now as f64 / freq.max(1) as f64
}

/// Windows 11's drop shadow around a window, unless it's maximized.
fn shadow(hwnd: HWND, zoomed: bool, rect: RECT) -> Shadow {
    if zoomed || covers_monitor(rect) {
        return Shadow::NONE;
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let s = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    Shadow { strength: 0.36, blur: 26.0 * s, offset: 6.0 * s }
}

/// Windows 11 rounds window corners by 8 px (at 96 dpi), except when maximized.
fn corner_radius(hwnd: HWND, zoomed: bool, rect: RECT) -> f32 {
    if zoomed || covers_monitor(rect) {
        return 0.0;
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    8.0 * if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

fn clip_to_screen(r: RECT) -> RECT {
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let c = RECT { left: r.left.max(x), top: r.top.max(y), right: r.right.min(x + w), bottom: r.bottom.min(y + h) };
    if c.right > c.left && c.bottom > c.top { c } else { r }
}

fn union(a: RECT, b: RECT) -> RECT {
    RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) }
}

fn to_rect(r: RECT) -> Rect {
    Rect::new(r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32)
}

// --- the shapes -------------------------------------------------------------

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

/// The Genie runs in two overlapping stages, as on the Mac: the sides curve
/// into a funnel towards the icon, then the window slides down through it.
const CURVE_END: f32 = 0.4;
const SLIDE_START: f32 = 0.3;

/// Appends a row: the image's left and right edges at height `y`, widened by
/// `pad` (a share of the row's width) on each side for the shadow.
fn push_row(out: &mut Vec<Vertex>, y: f32, left: f32, right: f32, v: f32, a: f32, pad: f32) {
    let extra = (right - left) * pad;
    out.push(Vertex { x: left - extra, y, u: -pad, v, a });
    out.push(Vertex { x: right + extra, y, u: 1.0 + pad, v, a });
}

/// The Genie at `p` (0 = the window, 1 = gone into `icon`, which is below it).
/// The funnel's sides are S-curves from the window's top corners to the icon's;
/// the window keeps its height as it pours down the funnel, and everything past
/// the icon's bottom edge has gone into the Dock (which is drawn over it).
/// `pad` is the shadow's margin around the window, in window pixels.
fn genie_rows(window: Rect, icon: Rect, p: f32, pad: f32, out: &mut Vec<Vertex>) {
    let p = p.clamp(0.0, 1.0);
    let bend = ease_in_out(p / CURVE_END);
    let slide = ease_in_out((p - SLIDE_START) / (1.0 - SLIDE_START));
    let (top, neck) = (window.y, icon.y);
    let funnel = |y: f32| {
        let s = smoothstep(0.0, 1.0, (y - top) / (neck - top));
        (lerp(window.x, icon.x, s), lerp(window.right(), icon.right(), s))
    };
    // By the end, the window's top edge has reached the icon's bottom.
    let travel = icon.bottom() - window.y;
    let (pu, pv) = (pad / window.w.max(1.0), pad / window.h.max(1.0));
    for i in 0..ROWS {
        let v = -pv + (1.0 + 2.0 * pv) * i as f32 / (ROWS - 1) as f32;
        let y = (window.y + v * window.h + slide * travel).min(icon.bottom());
        let (l, r) = funnel(y);
        push_row(out, y, lerp(window.x, l, bend), lerp(window.right(), r, bend), v, 1.0, pu);
    }
}

/// A plain rect of the image (and its shadow margin) at `r`.
fn rect_rows(window: Rect, r: Rect, a: f32, pad: f32, out: &mut Vec<Vertex>) {
    let (pu, pv) = (pad / window.w.max(1.0), pad / window.h.max(1.0));
    push_row(out, r.y - r.h * pv, r.x, r.right(), -pv, a, pu);
    push_row(out, r.bottom() + r.h * pv, r.x, r.right(), 1.0 + pv, a, pu);
}

/// Scale at `p`: the window shrinks straight into the icon, and the Dock
/// covers it as it arrives.
fn scale_rows(window: Rect, icon: Rect, p: f32, pad: f32, out: &mut Vec<Vertex>) {
    let e = ease_in_out(p);
    let r = Rect::new(lerp(window.x, icon.x, e), lerp(window.y, icon.y, e), lerp(window.w, icon.w, e), lerp(window.h, icon.h, e));
    rect_rows(window, r, 1.0, pad, out);
}

/// Close at `p`: the window recedes a little as it fades.
fn close_rows(window: Rect, p: f32, pad: f32, out: &mut Vec<Vertex>) {
    let e = 1.0 - (1.0 - p.clamp(0.0, 1.0)).powi(3);
    let s = 1.0 - 0.1 * e;
    let (w, h) = (window.w * s, window.h * s);
    rect_rows(window, Rect::new(window.cx() - w / 2.0, window.cy() - h / 2.0, w, h), 1.0 - e, pad, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Rect = Rect::new(100.0, 100.0, 800.0, 500.0);
    const ICON: Rect = Rect::new(1200.0, 1000.0, 64.0, 64.0);

    fn rows(f: impl Fn(&mut Vec<Vertex>)) -> Vec<Vertex> {
        let mut v = Vec::new();
        f(&mut v);
        v
    }

    #[test]
    fn genie_starts_as_the_window() {
        let v = rows(|o| genie_rows(WINDOW, ICON, 0.0, 0.0, o));
        assert_eq!(v.len(), ROWS * 2);
        let (first, last) = (v[0], v[v.len() - 1]);
        assert_eq!((first.x, first.y), (WINDOW.x, WINDOW.y));
        assert!((last.x - WINDOW.right()).abs() < 1e-3 && (last.y - WINDOW.bottom()).abs() < 1e-3);
        assert!(v.iter().all(|r| r.a == 1.0));
    }

    #[test]
    fn genie_ends_inside_the_icon() {
        let v = rows(|o| genie_rows(WINDOW, ICON, 1.0, 0.0, o));
        for r in &v {
            assert!(r.x >= ICON.x - 0.5 && r.x <= ICON.right() + 0.5, "x {} outside icon", r.x);
            // Gone past the icon's bottom edge: every row has collapsed there.
            assert!((r.y - ICON.bottom()).abs() < 1e-3, "y {} not gone", r.y);
        }
    }

    #[test]
    fn genie_keeps_the_window_height_while_it_slides() {
        // Midway through the slide, rows still above the icon are spaced as in the window.
        let v = rows(|o| genie_rows(WINDOW, ICON, 0.6, 0.0, o));
        let (a, b) = (v[0].y, v[2].y);
        assert!((b - a - WINDOW.h / (ROWS - 1) as f32).abs() < 1e-2);
    }

    #[test]
    fn genie_rows_stay_ordered() {
        for step in 0..=20 {
            let p = step as f32 / 20.0;
            let v = rows(|o| genie_rows(WINDOW, ICON, p, 30.0, o));
            for pair in v.chunks(2) {
                assert!(pair[0].x <= pair[1].x + 1e-3, "edges crossed at p={p}");
            }
            for w in v.chunks(2).collect::<Vec<_>>().windows(2) {
                assert!(w[1][0].y >= w[0][0].y - 1e-3, "rows folded at p={p}");
            }
        }
    }

    #[test]
    fn scale_goes_from_window_to_icon() {
        let start = rows(|o| scale_rows(WINDOW, ICON, 0.0, 0.0, o));
        assert_eq!((start[0].x, start[0].y, start[3].x, start[3].y), (WINDOW.x, WINDOW.y, WINDOW.right(), WINDOW.bottom()));
        let end = rows(|o| scale_rows(WINDOW, ICON, 1.0, 0.0, o));
        assert_eq!((end[0].x, end[0].y, end[3].x, end[3].y), (ICON.x, ICON.y, ICON.right(), ICON.bottom()));
    }

    #[test]
    fn shadow_margin_surrounds_the_window() {
        let v = rows(|o| close_rows(WINDOW, 0.0, 40.0, o));
        assert_eq!((v[0].x, v[0].y), (WINDOW.x - 40.0, WINDOW.y - 40.0));
        assert_eq!((v[3].x, v[3].y), (WINDOW.right() + 40.0, WINDOW.bottom() + 40.0));
        // The image itself still spans u, v = 0..1 within the margin.
        assert!((v[0].u + 40.0 / WINDOW.w).abs() < 1e-6 && (v[0].v + 40.0 / WINDOW.h).abs() < 1e-6);
        let g = rows(|o| genie_rows(WINDOW, ICON, 0.0, 40.0, o));
        assert!((g[0].x - (WINDOW.x - 40.0)).abs() < 1e-3 && (g[0].y - (WINDOW.y - 40.0)).abs() < 1e-3);
    }

    #[test]
    fn close_fades_out_around_the_center() {
        let end = rows(|o| close_rows(WINDOW, 1.0, 0.0, o));
        assert_eq!(end[0].a, 0.0);
        let cx = (end[0].x + end[1].x) / 2.0;
        assert!((cx - WINDOW.cx()).abs() < 1e-3);
    }
}
