//! Window previews over the Dock: rest the pointer on a running app's icon and
//! a glass panel rises above it, pointing down at it, with a card for each of
//! the app's windows (its title, traffic lights and a live thumbnail), so one of
//! several (two terminals, three browser windows) can be picked.
//!
//! The thumbnails are DWM's own (`DwmRegisterThumbnail`): DWM composes them
//! straight from each window's surface, live, so keeping them up to date costs
//! DockSide nothing. Each sits in a small window of its own that Windows 11
//! rounds, since nothing can be drawn over a DWM thumbnail to round it.
//! Minimized windows show the last frame DWM kept of them. The panel itself only
//! redraws when the highlight or a title changes, and follows its icon as the
//! Dock magnifies by moving, not redrawing.

use std::rc::Rc;

use windows::core::Result;
use windows::Win32::Foundation::{BOOL, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_BEZIER_SEGMENT, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_FIGURE_BEGIN_HOLLOW};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::{approach, Spring};
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconPixels;
use crate::gfx::{pt, Align, Gfx, ScaledBitmap, Weight};
use crate::surface::Surface;
use crate::theme::Theme;
use crate::win::apps::{self, AppWindow};
use crate::win::Monitor;

/// How long the pointer rests on an icon before its previews rise, in ms.
pub const HOVER_MS: u32 = 450;
/// How long the panel waits for the pointer to cross from the icon to it (or
/// come back), in ms, before closing.
pub const GRACE_MS: u32 = 260;

/// Room around the panel for its shadow (and the connector under it), in points.
const SHADOW: f32 = 24.0;
const RADIUS: f32 = 20.0;
/// Padding inside the panel, the app header's height and the gap between cards.
const PAD: f32 = 10.0;
const HEADER_H: f32 = 32.0;
const APP_ICON: f32 = 20.0;
const GAP: f32 = 10.0;
/// A card: its padding, the title/lights bar, and the thumbnail box (16:10).
const CARD_R: f32 = 15.0;
const CARD_PAD: f32 = 6.0;
const BAR_H: f32 = 24.0;
const BAR_GAP: f32 = 6.0;
const BOX_W: f32 = 210.0;
const BOX_H: f32 = 131.0;
const TITLE_SIZE: f32 = 12.0;
/// Traffic lights: diameter, gap, and the pill's padding.
const LIGHT_D: f32 = 11.0;
const LIGHT_GAP: f32 = 7.0;
const LIGHTS_PAD: f32 = 9.0;
/// Cards shrink down to this much of their full size before wrapping into rows.
const MIN_SCALE: f32 = 0.6;
/// The connector under the panel, and the gap it leaves above the icon.
const NUB_W: f32 = 22.0;
const NUB_H: f32 = 9.0;
const NUB_CLEAR: f32 = 7.0;
/// Kept clear at the screen's edges.
const EDGE: f32 = 8.0;
/// The droplet the panel swells out of, just above the connector (diameter, points).
const DROP: f32 = 26.0;

/// What a panel shows: the app, its windows, and where its icon is.
pub struct PreviewSource {
    pub windows: Vec<AppWindow>,
    pub name: String,
    pub icon: Option<Rc<IconPixels>>,
    /// The icon's centre and top, in screen points.
    pub anchor: (f32, f32),
}

/// A traffic light.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Light {
    Close,
    Minimize,
    Zoom,
}

const LIGHTS: [Light; 3] = [Light::Close, Light::Minimize, Light::Zoom];

/// What a click on the panel does.
pub enum Click {
    Activate(HWND),
    Light(HWND, Light),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Card(usize),
    Light(usize, Light),
}

struct Card {
    hwnd: HWND,
    title: String,
    /// In panel-local points (relative to the body's top-left).
    rect: Rect,
    title_pill: Rect,
    lights: Rect,
    thumb: Rect,
    /// The rounded window the thumbnail is shown in, and DWM's thumbnail.
    host: Option<HWND>,
    dwm: Option<isize>,
    /// The highlight, faded in and out (0..1).
    lit: f32,
}

impl Card {
    /// Centre of traffic light `k`.
    fn light_center(&self, k: usize) -> (f32, f32) {
        (self.lights.x + LIGHTS_PAD + LIGHT_D / 2.0 + k as f32 * (LIGHT_D + LIGHT_GAP), self.lights.cy())
    }

    fn light_at(&self, x: f32, y: f32) -> Option<Light> {
        if !self.lights.contains(x, y) {
            return None;
        }
        LIGHTS.iter().enumerate().find_map(|(k, l)| {
            let (cx, cy) = self.light_center(k);
            ((x - cx).abs() <= (LIGHT_D + LIGHT_GAP) / 2.0 && (y - cy).abs() <= BAR_H / 2.0).then_some(*l)
        })
    }
}

pub struct DockPreview {
    pub surface: Surface,
    /// Which Dock (index) and which of its items (launch target) it shows.
    pub dock: usize,
    pub key: String,
    name: String,
    icon: Option<Rc<IconPixels>>,
    icon_bmp: Option<Option<ScaledBitmap>>,
    cards: Vec<Card>,
    /// The panel body, in surface points; its size in points.
    body: Rect,
    /// Screen bounds (points) the panel stays inside.
    screen: Rect,
    /// The connector's centre, in body-local x.
    nub_x: f32,
    hit: Option<Hit>,
    pressed: Option<Hit>,
    /// The pointer is over the panel.
    pub hovering: bool,
    pub tracking: bool,
    pub dirty: bool,
    /// Thumbnail opacity last given to DWM (0..255).
    thumb_alpha: u8,
    /// How far the bubble has swelled, across and up (0 = a droplet at the
    /// connector, 1 = the whole panel; springs, so they overshoot a little).
    grow_w: Spring,
    grow_h: Spring,
    /// The surface's own fade (closing), which the thumbnails follow too.
    opacity: f32,
}

/// The panel's grid: columns, rows and thumbnail scale for `n` windows across `room` points.
fn grid(n: usize, room: f32) -> (usize, usize, f32) {
    let n = n.max(1);
    let width = |cols: usize, k: f32| cols as f32 * (BOX_W * k + 2.0 * CARD_PAD) + (cols - 1) as f32 * GAP + 2.0 * PAD;
    if width(n, 1.0) <= room {
        return (n, 1, 1.0);
    }
    // Shrink to fit one row, as far as MIN_SCALE.
    let k = (room - 2.0 * PAD - (n - 1) as f32 * GAP - n as f32 * 2.0 * CARD_PAD) / (n as f32 * BOX_W);
    if k >= MIN_SCALE {
        return (n, 1, k);
    }
    let card = BOX_W * MIN_SCALE + 2.0 * CARD_PAD;
    let cols = (((room - 2.0 * PAD + GAP) / (card + GAP)).floor() as usize).max(1);
    (cols, n.div_ceil(cols), MIN_SCALE)
}

/// The part of a `w` x `h` source that fills a `bw` x `bh` box: the full
/// width (or height), top-anchored, like a window seen from its title bar down.
fn crop(src: RECT, bw: f32, bh: f32) -> RECT {
    let (w, h) = ((src.right - src.left) as f32, (src.bottom - src.top) as f32);
    if w <= 0.0 || h <= 0.0 || bw <= 0.0 || bh <= 0.0 {
        return src;
    }
    let box_aspect = bw / bh;
    if w / h > box_aspect {
        // Wider than the box: the middle of it.
        let cw = (h * box_aspect).round() as i32;
        let left = src.left + ((w as i32 - cw) / 2);
        RECT { left, top: src.top, right: left + cw, bottom: src.bottom }
    } else {
        // Taller: from the top.
        let ch = (w / box_aspect).round() as i32;
        RECT { left: src.left, top: src.top, right: src.right, bottom: src.top + ch }
    }
}

/// The part of `hwnd`'s window rect that's really drawn (Windows pads windows
/// with invisible resize borders), relative to the window rect, in pixels.
/// None when minimized: DWM's kept frame already leaves them out.
fn visible_source(hwnd: HWND) -> Option<RECT> {
    if apps::is_minimized(hwnd) {
        return None;
    }
    let mut win = RECT::default();
    let mut frame = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut win).ok()?;
        DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut frame as *mut _ as _, std::mem::size_of::<RECT>() as u32).ok()?;
    }
    let r = RECT { left: frame.left - win.left, top: frame.top - win.top, right: frame.right - win.left, bottom: frame.bottom - win.top };
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

/// `c` with its alpha scaled by `k`.
fn fade(c: Color, k: f32) -> Color {
    c.alpha(c.a * k)
}

/// Mixes `a` toward `b` by `t`.
fn mix(a: Color, b: Color, t: f32) -> Color {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Color { r: l(a.r, b.r), g: l(a.g, b.g), b: l(a.b, b.b), a: l(a.a, b.a) }
}

impl DockPreview {
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &crate::config::Config, monitor: Monitor, dock: usize, key: String, src: PreviewSource) -> Result<Self> {
        let s = monitor.scale;
        let screen = Rect::new(monitor.rect.left as f32 / s, monitor.rect.top as f32 / s, monitor.width_pt(), monitor.height_pt());
        let n = src.windows.len();
        let (cols, rows, k) = grid(n, screen.w - 2.0 * EDGE);
        let (box_w, box_h) = ((BOX_W * k).round(), (BOX_H * k).round());
        let (card_w, card_h) = (box_w + 2.0 * CARD_PAD, CARD_PAD + BAR_H + BAR_GAP + box_h + CARD_PAD);
        let w = cols as f32 * card_w + (cols - 1) as f32 * GAP + 2.0 * PAD;
        let h = HEADER_H + rows as f32 * card_h + (rows - 1) as f32 * GAP + PAD;

        let lights_w = 2.0 * LIGHTS_PAD + 3.0 * LIGHT_D + 2.0 * LIGHT_GAP;
        // A last row that isn't full is centred, like the first.
        let cards: Vec<Card> = src
            .windows
            .iter()
            .enumerate()
            .map(|(i, win)| {
                let (row, col) = (i / cols, i % cols);
                let in_row = if row + 1 == rows { n - row * cols } else { cols };
                let row_w = in_row as f32 * card_w + (in_row - 1) as f32 * GAP;
                let rect = Rect::new((w - row_w) / 2.0 + col as f32 * (card_w + GAP), HEADER_H + row as f32 * (card_h + GAP), card_w, card_h);
                let bar_y = rect.y + CARD_PAD;
                let lights = Rect::new(rect.right() - CARD_PAD - lights_w, bar_y, lights_w, BAR_H);
                Card {
                    hwnd: win.hwnd,
                    title: win.title.clone(),
                    rect,
                    title_pill: Rect::new(rect.x + CARD_PAD, bar_y, (lights.x - 6.0 - rect.x - CARD_PAD).max(0.0), BAR_H),
                    lights,
                    thumb: Rect::new(rect.x + CARD_PAD, bar_y + BAR_H + BAR_GAP, box_w, box_h),
                    host: None,
                    dwm: None,
                    lit: 0.0,
                }
            })
            .collect();

        let (x, nub_x) = Self::place(screen, w, src.anchor.0);
        let y = (src.anchor.1 - NUB_CLEAR - NUB_H - h).max(screen.y + EDGE);
        let rect = RECT {
            left: ((x - SHADOW) * s).round() as i32,
            top: ((y - SHADOW) * s).round() as i32,
            right: ((x + w + SHADOW) * s).round() as i32,
            bottom: ((y + h + SHADOW) * s).round() as i32,
        };
        let surface = Surface::new(gfx, rect, s, cfg.menubar.backdrop.for_panels(), theme.menu_tint)?;
        let body = Rect::new(SHADOW, SHADOW, w, h);

        let mut p = Self {
            surface,
            dock,
            key,
            name: src.name,
            icon: src.icon,
            icon_bmp: None,
            cards,
            body,
            screen,
            nub_x,
            hit: None,
            pressed: None,
            hovering: false,
            tracking: false,
            dirty: true,
            // The thumbnails come in once the bubble is nearly full.
            thumb_alpha: 0,
            // Up leads and across follows, a touch bouncier: the droplet
            // stretches upward, then spreads and settles with a wobble.
            grow_w: Spring::new(0.0, 0.46, 0.64),
            grow_h: Spring::new(0.0, 0.38, 0.70),
            opacity: 1.0,
        };
        p.grow_w.set(1.0);
        p.grow_h.set(1.0);
        p.apply_shape();
        p.add_thumbnails();
        Ok(p)
    }

    /// The panel's left edge (screen points) centred over an icon at `cx`, kept
    /// on screen, and where the connector then sits along it.
    fn place(screen: Rect, w: f32, cx: f32) -> (f32, f32) {
        let x = (cx - w / 2.0).clamp(screen.x + EDGE, (screen.right() - EDGE - w).max(screen.x + EDGE));
        let reach = RADIUS + NUB_W / 2.0;
        (x, (cx - x).clamp(reach, (w - reach).max(reach)))
    }

    /// A card's thumbnail box, in physical screen pixels.
    fn thumb_px(&self, r: Rect) -> RECT {
        let s = self.surface.scale;
        let (ox, oy) = (self.surface.rect.left as f32 + self.body.x * s, self.surface.rect.top as f32 + self.body.y * s);
        RECT {
            left: (ox + r.x * s).round() as i32,
            top: (oy + r.y * s).round() as i32,
            right: (ox + r.right() * s).round() as i32,
            bottom: (oy + r.bottom() * s).round() as i32,
        }
    }

    /// The panel's shape as far as it has swelled (surface points), its corner
    /// radius, and how far its content has faded in.
    fn shape(&self) -> (Rect, f32, f32) {
        let (gw, gh) = (self.grow_w.value.max(0.0), self.grow_h.value.max(0.0));
        let (body, room) = (self.body, SHADOW - 3.0);
        let w = (DROP + (body.w - DROP) * gw).clamp(0.0, body.w + 2.0 * room);
        let h = (DROP + (body.h - DROP) * gh).clamp(0.0, body.h + room);
        // Out of the connector, drifting over to the panel's centre as it spreads.
        let from = body.x + self.nub_x;
        let cx = from + (body.cx() - from) * gw.min(1.0);
        let x = (cx - w / 2.0).clamp(body.x - room, body.right() + room - w);
        let r = Rect::new(x, body.bottom() - h, w, h);
        let radius = (DROP / 2.0 + (RADIUS - DROP / 2.0) * gw.min(gh).min(1.0)).min(w / 2.0).min(h / 2.0);
        let content = ((gw.min(gh) - 0.6) / 0.35).clamp(0.0, 1.0);
        (r, radius, content)
    }

    /// How far in the thumbnails are: they're windows of their own, outside
    /// the bubble's clip, so they wait until it has (all but) filled out.
    fn thumbs_in(&self) -> f32 {
        let g = self.grow_w.value.min(self.grow_h.value);
        ((g - 0.9) / 0.1).clamp(0.0, 1.0) * self.opacity
    }

    /// Fits the glass to the current shape, and the thumbnails to how far in the content is.
    fn apply_shape(&mut self) {
        let (r, radius, _) = self.shape();
        self.surface.set_backdrop_shape(r, radius);
        self.set_thumb_alpha(self.thumbs_in());
    }

    fn set_thumb_alpha(&mut self, k: f32) {
        let a = (k.clamp(0.0, 1.0) * 255.0).round() as u8;
        if a == self.thumb_alpha {
            return;
        }
        // Windows shades the rounded windows they sit in: hidden while there's nothing to show.
        if (a == 0) != (self.thumb_alpha == 0) {
            for host in self.cards.iter().filter_map(|c| c.host) {
                unsafe {
                    let _ = ShowWindow(host, if a == 0 { SW_HIDE } else { SW_SHOWNOACTIVATE });
                }
            }
        }
        self.thumb_alpha = a;
        for t in self.cards.iter().filter_map(|c| c.dwm) {
            let props = DWM_THUMBNAIL_PROPERTIES { dwFlags: DWM_TNP_OPACITY, opacity: a, ..Default::default() };
            unsafe {
                let _ = DwmUpdateThumbnailProperties(t, &props);
            }
        }
    }

    /// Starts the bubble `from` part grown (0: out of the droplet; 1: already full).
    pub fn grow_from(&mut self, from: f32) {
        self.grow_w.snap(from);
        self.grow_h.snap(from);
        self.grow_w.set(1.0);
        self.grow_h.set(1.0);
        self.apply_shape();
        self.dirty = true;
    }

    /// Starts closing: the bubble shrinks back down into the connector (see `set_opacity` for the fade).
    pub fn close(&mut self) {
        self.grow_w = self.grow_w.with(0.24, 1.0);
        self.grow_h = self.grow_h.with(0.30, 1.0);
        self.grow_w.set(0.0);
        self.grow_h.set(0.0);
        self.hit = None;
    }

    /// For each window, a rounded window over its card with DWM's live thumbnail in it.
    fn add_thumbnails(&mut self) {
        let owner = self.surface.content;
        for i in 0..self.cards.len() {
            let rc = self.thumb_px(self.cards[i].thumb);
            let c = &mut self.cards[i];
            let ex = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP;
            let Ok(host) = crate::win::create_window(ex, WS_POPUP, rc, owner) else { continue };
            unsafe {
                let round = DWMWCP_ROUND;
                let _ = DwmSetWindowAttribute(host, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as _, 4);
                let none: u32 = 0xFFFF_FFFE; // DWMWA_COLOR_NONE: no outline
                let _ = DwmSetWindowAttribute(host, DWMWA_BORDER_COLOR, &none as *const _ as _, 4);
            }
            c.host = Some(host);
            let Ok(t) = (unsafe { DwmRegisterThumbnail(host, c.hwnd) }) else {
                log!("dock preview: no thumbnail for {:?}", c.hwnd);
                continue;
            };
            let source = visible_source(c.hwnd).or_else(|| {
                let sz = unsafe { DwmQueryThumbnailSourceSize(t) }.ok()?;
                Some(RECT { left: 0, top: 0, right: sz.cx, bottom: sz.cy })
            });
            let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
            let mut props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: RECT { left: 0, top: 0, right: w, bottom: h },
                opacity: self.thumb_alpha,
                fVisible: BOOL::from(true),
                fSourceClientAreaOnly: BOOL::from(false),
                ..Default::default()
            };
            if let Some(src) = source {
                props.dwFlags |= DWM_TNP_RECTSOURCE;
                props.rcSource = crop(src, w as f32, h as f32);
            }
            if unsafe { DwmUpdateThumbnailProperties(t, &props) }.is_err() {
                unsafe {
                    let _ = DwmUnregisterThumbnail(t);
                }
                continue;
            }
            c.dwm = Some(t);
            // Shown once the bubble has filled out (see `set_thumb_alpha`).
            if self.thumb_alpha > 0 {
                unsafe {
                    let _ = ShowWindow(host, SW_SHOWNOACTIVATE);
                }
            }
        }
    }

    /// Whether `hwnd` is one of the windows the thumbnails are shown in (they let clicks through to the panel).
    pub fn is_host(&self, hwnd: HWND) -> bool {
        self.cards.iter().any(|c| c.host == Some(hwnd))
    }

    /// Whether it shows exactly these windows (in any order: focusing one
    /// reorders the list, and the cards stay where they are).
    pub fn shows(&self, windows: &[AppWindow]) -> bool {
        windows.len() == self.cards.len() && windows.iter().all(|w| self.cards.iter().any(|c| c.hwnd == w.hwnd))
    }

    /// Takes the windows' current titles (same windows); true when one changed.
    pub fn set_titles(&mut self, windows: &[AppWindow]) -> bool {
        let mut changed = false;
        for c in &mut self.cards {
            let Some(w) = windows.iter().find(|w| w.hwnd == c.hwnd) else { continue };
            if c.title != w.title {
                c.title = w.title.clone();
                changed = true;
            }
        }
        self.dirty |= changed;
        changed
    }

    /// Keeps the panel over its icon (centre `cx`, screen points) as the Dock
    /// magnifies: the windows move; only a connector that has to slide redraws.
    pub fn follow(&mut self, cx: f32) {
        let s = self.surface.scale;
        let (x, nub_x) = Self::place(self.screen, self.body.w, cx);
        let left = ((x - SHADOW) * s).round() as i32;
        let dx = left - self.surface.rect.left;
        if dx != 0 {
            self.surface.move_to(left, self.surface.rect.top);
            for host in self.cards.iter().filter_map(|c| c.host) {
                let mut r = RECT::default();
                unsafe {
                    if GetWindowRect(host, &mut r).is_ok() {
                        let _ = SetWindowPos(host, None, r.left + dx, r.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                    }
                }
            }
        }
        if (nub_x - self.nub_x).abs() > 0.25 {
            self.nub_x = nub_x;
            self.dirty = true;
        }
    }

    /// What's under (x, y), in surface points.
    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        let (x, y) = (x - self.body.x, y - self.body.y);
        let i = self.cards.iter().position(|c| c.rect.contains(x, y))?;
        Some(match self.cards[i].light_at(x, y) {
            Some(l) => Hit::Light(i, l),
            None => Hit::Card(i),
        })
    }

    fn card_of(hit: Option<Hit>) -> Option<usize> {
        match hit? {
            Hit::Card(i) | Hit::Light(i, _) => Some(i),
        }
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        self.hovering = self.body.contains(x, y);
        let at = self.hit_at(x, y);
        if at != self.hit {
            self.hit = at;
            self.dirty = true;
        }
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        self.hovering = false;
        self.pressed = None;
        if self.hit.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn on_down(&mut self, x: f32, y: f32) {
        self.pressed = self.hit_at(x, y);
        self.dirty = true;
    }

    /// What a press and release on the same spot does.
    pub fn on_up(&mut self, x: f32, y: f32) -> Option<Click> {
        let pressed = self.pressed.take();
        self.dirty = true;
        let at = self.hit_at(x, y).filter(|h| Some(*h) == pressed)?;
        Some(match at {
            Hit::Card(i) => Click::Activate(self.cards[i].hwnd),
            Hit::Light(i, l) => Click::Light(self.cards[i].hwnd, l),
        })
    }

    /// Advances the bubble and the highlight fades; true while anything still moves.
    pub fn tick(&mut self, dt: f32) -> bool {
        let mut moving = false;
        let (w, h) = (self.grow_w.step(dt), self.grow_h.step(dt));
        if w || h {
            self.apply_shape();
            self.dirty = true;
            moving = true;
        }
        let lit = Self::card_of(self.hit);
        for (i, c) in self.cards.iter_mut().enumerate() {
            let want = if lit == Some(i) { 1.0 } else { 0.0 };
            if c.lit != want {
                // In quickly, out a little slower, like AppKit's hover.
                c.lit = approach(c.lit, want, dt, if want > c.lit { 30.0 } else { 16.0 });
                moving = true;
                self.dirty = true;
            }
        }
        moving
    }

    /// Fades the panel and its thumbnails together (closing).
    pub fn set_opacity(&mut self, opacity: f32) {
        self.opacity = opacity.clamp(0.0, 1.0);
        self.surface.set_opacity(self.opacity);
        self.set_thumb_alpha(self.thumbs_in());
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        self.dirty = false;
        if self.icon_bmp.is_none() {
            let rt = self.surface.canvas.target().clone();
            self.icon_bmp = Some(self.icon.as_ref().and_then(|px| px.to_bitmap(&rt)).map(ScaledBitmap::new));
        }
        let title_font = gfx.format(&theme.font, TITLE_SIZE, Weight::Regular, Align::Leading);
        let name_font = gfx.format(&theme.font, 13.0, Weight::SemiBold, Align::Leading);
        let has_backdrop = self.surface.backdrop.is_some();
        let dark = theme.dark;
        let (body, cards, hit, pressed, nub_x) = (self.body, &self.cards, self.hit, self.pressed, self.nub_x);
        let (shape, radius, content) = self.shape();
        let (name, icon) = (&self.name, self.icon_bmp.as_ref().and_then(|b| b.as_ref()));

        // Layers of glass on glass: the cards a shade lighter than the panel,
        // their pills a shade lighter again.
        let card_fill = if dark { Color::gray(255, 0.06) } else { Color::gray(255, 0.38) };
        let card_edge = if dark { Color::gray(255, 0.10) } else { Color::gray(0, 0.06) };
        let pill = if dark { Color::gray(255, 0.09) } else { Color::gray(255, 0.55) };
        let pill_lit = if dark { Color::gray(255, 0.14) } else { Color::gray(255, 0.8) };
        let thumb_edge = if dark { Color::gray(255, 0.12) } else { Color::gray(0, 0.12) };
        let idle_light = if dark { Color::gray(255, 0.22) } else { Color::gray(0, 0.16) };
        let colors = [Color::rgba(255, 95, 87, 1.0), Color::rgba(254, 188, 46, 1.0), Color::rgba(40, 200, 64, 1.0)];
        let glyph = Color::rgba(0, 0, 0, 0.55);
        let panel_fill = if has_backdrop { theme.menu_tint.alpha(0.35) } else { theme.menu_tint.alpha(1.0) };
        // The connector has no glass behind it: the panel's tint, solid enough to read as the panel.
        let nub_fill = theme.menu_tint.alpha(theme.menu_tint.a.max(0.9));

        let r = self.surface.render(gfx, |p| {
            p.soft_shadow(shape, radius, 10, 1.8, 0.024, 5.0, theme.shadow);
            p.clear_round(shape.inset(2.0, 2.0), (radius - 2.0).max(0.0));
            p.fill_round(shape, radius, panel_fill);
            p.stroke_round(shape.inset(0.25, 0.25), radius, theme.menu_border, 0.5);

            // The connector: a soft point under the panel, aimed at the icon,
            // stopping short of it. It's there from the first frame: the
            // droplet the bubble swells from sits on it.
            let (nx, base) = (body.x + nub_x, body.bottom() - 0.75);
            let (hw, tip) = ((NUB_W / 2.0).min(shape.w / 2.0 - radius * 0.3).max(4.0), base + NUB_H);
            let nub = |filled: bool| {
                p.gfx.path(|sk| unsafe {
                    sk.BeginFigure(pt(nx - hw, base), if filled { D2D1_FIGURE_BEGIN_FILLED } else { D2D1_FIGURE_BEGIN_HOLLOW });
                    sk.AddBezier(&D2D1_BEZIER_SEGMENT { point1: pt(nx - hw * 0.45, base), point2: pt(nx - 2.6, tip - 1.2), point3: pt(nx - 1.2, tip - 0.4) });
                    sk.AddBezier(&D2D1_BEZIER_SEGMENT { point1: pt(nx - 0.5, tip), point2: pt(nx + 0.5, tip), point3: pt(nx + 1.2, tip - 0.4) });
                    sk.AddBezier(&D2D1_BEZIER_SEGMENT { point1: pt(nx + 2.6, tip - 1.2), point2: pt(nx + hw * 0.45, base), point3: pt(nx + hw, base) });
                    sk.EndFigure(if filled { D2D1_FIGURE_END_CLOSED } else { D2D1_FIGURE_END_OPEN });
                })
            };
            if let (Some(fill), Some(edge)) = (nub(true), nub(false)) {
                p.fill_geometry(&fill, nub_fill);
                p.stroke_geometry(&edge, theme.menu_border, 0.5);
            }

            if content <= 0.0 {
                return;
            }
            // The content shows inside the swelling shape, fading in as it fills.
            p.push_clip(shape.inset(radius * 0.3, radius * 0.3));
            p.faded(content, || {
            // Header: the app's icon and name.
            let hy = body.y + (HEADER_H - APP_ICON) / 2.0;
            let mut tx = body.x + PAD + 4.0;
            if let Some(b) = icon {
                p.scaled_bitmap(b, Rect::new(tx, hy, APP_ICON, APP_ICON), 1.0);
                tx += APP_ICON + 8.0;
            }
            p.text(name, &name_font, Rect::new(tx, body.y, body.right() - PAD - tx, HEADER_H), theme.text);

            for (i, c) in cards.iter().enumerate() {
                let r = c.rect.offset(body.x, body.y);
                let down = Self::card_of(pressed) == Some(i) && Self::card_of(hit) == Some(i);
                p.fill_round(r, CARD_R, if down { mix(card_fill, pill_lit, 0.5) } else { card_fill });
                p.stroke_round(r.inset(0.25, 0.25), CARD_R, card_edge, 0.5);
                if c.lit > 0.0 {
                    p.stroke_round(r.inset(1.0, 1.0), CARD_R - 1.0, fade(theme.accent, c.lit), 2.0);
                }

                // Title pill.
                let tp = c.title_pill.offset(body.x, body.y);
                p.fill_round(tp, BAR_H / 2.0, mix(pill, pill_lit, c.lit));
                p.text(&c.title, &title_font, tp.inset(10.0, 0.0), theme.text);

                // Traffic lights: muted until the card is under the pointer; their
                // marks show while the pointer is on them, as on macOS.
                let lp = c.lights.offset(body.x, body.y);
                p.fill_round(lp, BAR_H / 2.0, mix(pill, pill_lit, c.lit));
                let on_lights = matches!(hit, Some(Hit::Light(j, _)) if j == i);
                for (k, color) in colors.iter().enumerate() {
                    let (cx, cy) = c.light_center(k);
                    let (cx, cy) = (cx + body.x, cy + body.y);
                    p.fill_circle(cx, cy, LIGHT_D / 2.0, mix(idle_light, *color, c.lit));
                    if on_lights {
                        let g = 2.6;
                        match LIGHTS[k] {
                            Light::Close => {
                                p.line(cx - g, cy - g, cx + g, cy + g, glyph, 1.2);
                                p.line(cx - g, cy + g, cx + g, cy - g, glyph, 1.2);
                            }
                            Light::Minimize => p.line(cx - g - 0.4, cy, cx + g + 0.4, cy, glyph, 1.3),
                            Light::Zoom => {
                                p.line(cx - g - 0.4, cy, cx + g + 0.4, cy, glyph, 1.3);
                                p.line(cx, cy - g - 0.4, cx, cy + g + 0.4, glyph, 1.3);
                            }
                        }
                    }
                }

                // The thumbnail's own window covers this; a soft shadow and
                // hairline around it seat it on the card.
                let t = c.thumb.offset(body.x, body.y);
                if c.dwm.is_some() {
                    p.soft_shadow(t, 8.0, 5, 1.0, 0.03, 1.0, theme.shadow);
                    p.clear_rect(t.inset(1.0, 1.0));
                    p.fill_round(t, 8.0, Color::HIT);
                    p.stroke_round(t.inset(-0.5, -0.5), 8.5, thumb_edge, 0.5);
                } else {
                    p.fill_round(t, 8.0, pill);
                }
            }
            });
            p.pop_clip();
        });
        if let Err(e) = r {
            log!("dock preview render failed: {e}");
        }
    }
}

impl Drop for DockPreview {
    fn drop(&mut self) {
        for c in &self.cards {
            unsafe {
                if let Some(t) = c.dwm {
                    let _ = DwmUnregisterThumbnail(t);
                }
                if let Some(h) = c.host {
                    let _ = DestroyWindow(h);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_few_windows_sit_in_one_row_at_full_size() {
        assert_eq!(grid(3, 1900.0), (3, 1, 1.0));
    }

    #[test]
    fn many_windows_shrink_then_wrap() {
        let (cols, rows, k) = grid(10, 1900.0);
        assert_eq!((cols, rows), (10, 1));
        assert!(k < 1.0 && k >= MIN_SCALE);
        let (cols, rows, k) = grid(30, 1900.0);
        assert!(rows > 1 && cols * rows >= 30);
        assert_eq!(k, MIN_SCALE);
    }

    #[test]
    fn thumbnails_fill_their_box_from_the_top() {
        // A tall window: full width, cut at the box's shape from its top.
        let r = crop(RECT { left: 0, top: 0, right: 1000, bottom: 1000 }, 160.0, 100.0);
        assert_eq!((r.left, r.top, r.right, r.bottom), (0, 0, 1000, 625));
        // A wide one: full height, its middle.
        let r = crop(RECT { left: 10, top: 0, right: 2010, bottom: 500 }, 160.0, 100.0);
        assert_eq!((r.left, r.right, r.bottom), (610, 1410, 500));
    }

    #[test]
    fn the_connector_points_at_the_icon_even_at_the_screen_edge() {
        let screen = Rect::new(0.0, 0.0, 1000.0, 800.0);
        let (x, nub) = DockPreview::place(screen, 400.0, 500.0);
        assert_eq!((x, nub), (300.0, 200.0));
        let (x, nub) = DockPreview::place(screen, 400.0, 30.0);
        assert_eq!(x, EDGE);
        assert_eq!(nub, RADIUS + NUB_W / 2.0);
    }

    /// Puts the panel on screen for a moment over the app with the most windows
    /// open, light then dark, and saves what's on screen there:
    /// `DOCKSIDE_PREVIEW_SHOTS=<dir> cargo test --profile fast -p dockside dock_preview::tests::shots -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn shots() {
        use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
        use windows::Win32::Graphics::Gdi::*;
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        let Ok(dir) = std::env::var("DOCKSIDE_PREVIEW_SHOTS") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let _ = crate::win::register_class(Some(proc_));
        let pump = |ms: u64| {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            let mut msg = MSG::default();
            while std::time::Instant::now() < until {
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        // What's on screen at (x, y), w x h pixels, as opaque BGRA.
        let grab = |x: i32, y: i32, w: i32, h: i32| -> Vec<u8> {
            let mut px = vec![0u8; (w * h * 4) as usize];
            unsafe {
                let screen = GetDC(None);
                let mem = CreateCompatibleDC(screen);
                let bmp = CreateCompatibleBitmap(screen, w, h);
                let old = SelectObject(mem, bmp);
                let _ = BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY);
                let mut bi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER { biSize: 40, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..Default::default() },
                    ..Default::default()
                };
                GetDIBits(mem, bmp, 0, h as u32, Some(px.as_mut_ptr() as _), &mut bi, DIB_RGB_COLORS);
                SelectObject(mem, old);
                let _ = DeleteObject(bmp);
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
            }
            for q in px.chunks_mut(4) {
                q[3] = 255;
            }
            px
        };
        let gfx = Gfx::new().unwrap();
        let all = apps::list_windows();
        let mut by_exe: std::collections::BTreeMap<String, Vec<AppWindow>> = Default::default();
        for w in all {
            by_exe.entry(w.exe_name()).or_default().push(w);
        }
        let mut groups: Vec<Vec<AppWindow>> = by_exe.into_values().collect();
        groups.sort_by_key(|g| std::cmp::Reverse(g.len()));
        // Nothing open twice: any few windows show the layout as well.
        let windows: Vec<AppWindow> =
            if groups[0].len() > 1 { groups[0].iter().take(5).cloned().collect() } else { groups.iter().take(3).map(|g| g[0].clone()).collect() };
        let exe = windows[0].exe.clone();
        let icon = crate::gfx::icons::load_shell_icon(&exe).map(Rc::new);
        println!("previewing {} window(s) of {exe}", windows.len());
        let monitor = Monitor::primary();
        let s = monitor.scale;
        let (cx, top) = (monitor.width_pt() / 2.0 + 40.0, monitor.height_pt() - 110.0);
        for dark in [false, true] {
            let mut cfg = crate::config::load(&crate::config::path()).unwrap_or_else(|_| crate::config::defaults());
            cfg.general.appearance = if dark { crate::config::Appearance::Dark } else { crate::config::Appearance::Light };
            let theme = Theme::new(&cfg, &gfx);
            let src = PreviewSource { windows: windows.clone(), name: "File Explorer".into(), icon: icon.clone(), anchor: (cx, top) };
            let mut p = DockPreview::new(&gfx, &theme, &cfg, monitor, 0, "x".into(), src).unwrap();
            p.render(&gfx, &theme);
            pump(250);
            // The bubble swelling, as a strip of frames (60 fps, every 4th).
            if !dark {
                let r = p.surface.rect;
                let (w, h) = (r.right - r.left, r.bottom - r.top + 30);
                let frames = [0usize, 2, 4, 6, 8, 11, 15, 22, 40];
                let mut strip = vec![0u8; (w as usize) * (h as usize) * 4 * frames.len()];
                let mut f = 0;
                p.grow_from(0.0);
                for (k, &at) in frames.iter().enumerate() {
                    while f < at {
                        p.tick(1.0 / 60.0);
                        f += 1;
                    }
                    p.render(&gfx, &theme);
                    pump(120);
                    let px = grab(r.left, r.top, w, h);
                    // Frames stacked top to bottom.
                    let off = k * (w as usize) * (h as usize) * 4;
                    strip[off..off + px.len()].copy_from_slice(&px);
                }
                crate::gfx::write_png(&gfx, w as u32, (h as usize * frames.len()) as u32, &strip, &format!("{dir}/preview-bubble.png"));
                while p.tick(1.0 / 60.0) {}
            }
            // The second card lit and its lights under the pointer.
            let j = 1.min(p.cards.len() - 1);
            p.hit = Some(Hit::Light(j, Light::Close));
            for _ in 0..30 {
                p.tick(1.0 / 60.0);
            }
            // The icon slid a little (magnification): the connector follows.
            p.follow(cx - 30.0);
            p.render(&gfx, &theme);
            pump(500);
            let r = p.surface.rect;
            let (w, h) = (r.right - r.left, r.bottom - r.top + 30);
            let px = grab(r.left, r.top, w, h);
            let name = if dark { "dark" } else { "light" };
            crate::gfx::write_png(&gfx, w as u32, h as u32, &px, &format!("{dir}/preview-{name}.png"));
            println!("{name}: {}x{} px at scale {s}", w, h);
            drop(p);
            pump(150);
        }
    }
}
