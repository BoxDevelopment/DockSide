//! Menus the menu bar's status items open: Battery and the calendar under the
//! clock. Each is one piece of Control Center's glass in a window of its own,
//! with its content drawn into a single GPU layer that's drawn again only when
//! what it shows changes (see [`Content::key`]).

pub mod battery;
pub mod calendar;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::Instant;

use windows::core::{Interface, Result};
use windows::Foundation::Numerics::Vector3;
use windows::Win32::Foundation::{BOOL, HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_HOSTBACKDROPBRUSH};
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::ContainerVisual;

use crate::anim::lerp;
use crate::config::{Backdrop, Config};
use crate::controlcenter::{brushes, ease_out_back, pal, v3, Brushes, Glass, Pal};
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Gfx, Painter, Weight};
use crate::launchpad::comp::{self, Gpu, Layer};
use crate::menu::Action;
use crate::theme::Theme;
use crate::win::Monitor;

/// Which status item a dropdown belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Battery,
    Calendar,
}

/// Something clickable in a dropdown.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Hit {
    /// A menu row at the bottom ("Battery Settings…").
    Link(u8),
    /// Power mode segment.
    Mode(u8),
    Saver,
    Prev,
    Next,
    Today,
    /// A day, as days since 1970-01-01 (local).
    Day(i32),
    Event(u16),
}

/// What the App does after an input.
pub enum DropEvent {
    None,
    Close,
    /// Close, then run this.
    Run(Action),
}

/// What content gets to draw with.
pub struct Ctx<'a> {
    pub pal: &'a Pal,
    pub font: &'a str,
    pub accent: Color,
    pub hover: Option<Hit>,
    pub pressed: Option<Hit>,
}

impl Ctx<'_> {
    /// A row's hover highlight, under its content.
    pub fn highlight(&self, p: &Painter, r: Rect, hit: Hit) {
        if self.hover == Some(hit) {
            let a = if self.pressed == Some(hit) { 1.6 } else { 1.0 };
            p.fill_round(r, 8.0, self.pal.chip.alpha((self.pal.chip.a * a).min(1.0)));
        }
    }

    pub fn text(&self, p: &Painter, s: &str, size: f32, weight: Weight, align: Align, r: Rect, c: Color) {
        let f = p.gfx.format(self.font, size, weight, align);
        p.text(s, &f, r, c);
    }

    pub fn measure(&self, p: &Painter, s: &str, size: f32, weight: Weight) -> f32 {
        p.gfx.measure(s, &p.gfx.format(self.font, size, weight, Align::Leading))
    }

    pub fn separator(&self, p: &Painter, y: f32, w: f32) {
        p.line(PAD, y, w - PAD, y, self.pal.sep, 1.0);
    }

    /// A menu row with an optional trailing note (the "Battery Settings…" kind).
    pub fn link_row(&self, p: &Painter, hits: &mut Vec<(Rect, Hit)>, r: Rect, label: &str, hit: Hit) {
        let hr = Rect::new(6.0, r.y, r.w + 2.0 * (PAD - 6.0), r.h);
        self.highlight(p, hr, hit);
        self.text(p, label, 13.0, Weight::Regular, Align::Leading, r, self.pal.text);
        hits.push((hr, hit));
    }
}

/// Side padding inside a dropdown.
pub const PAD: f32 = 14.0;

pub trait Content {
    fn width(&self) -> f32;
    /// Hashes everything drawn (hover state is added by the host).
    fn key(&self, h: &mut DefaultHasher);
    /// Draws at the panel's width and returns the height used.
    fn draw(&self, p: &Painter, ctx: &Ctx, hits: &mut Vec<(Rect, Hit)>) -> f32;
    /// The height `draw` will use (so the window can be sized first).
    fn height(&self, gfx: &Gfx, font: &str) -> f32;
    fn click(&mut self, hit: Hit) -> DropEvent;
    fn wheel(&mut self, _delta: i32) -> bool {
        false
    }
    fn key_down(&mut self, _vk: u16) -> bool {
        false
    }
    /// A worker has news.
    fn update(&mut self) {}
}

/// How long opening and closing take.
const OPEN_S: f32 = 0.3;
const CLOSE_S: f32 = 0.14;
const RADIUS: f32 = 18.0;

pub struct Dropdown {
    pub hwnd: HWND,
    pub scale: f32,
    _gpu: Rc<Gpu>,
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    glass: Glass,
    layer: Layer,
    _brushes: Brushes,
    pal: Pal,
    font: String,
    accent: Color,
    content: Box<dyn Content>,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    key: u64,
    w: f32,
    h: f32,
    /// Top-left on screen and the most it may grow to, in points.
    at: (f32, f32),
    max_h: f32,
    /// Where it grows from (under the menu bar item), in panel points.
    origin: f32,
    opened: Instant,
    closing: Option<Instant>,
    pub tracking: bool,
    pub dirty: bool,
}

impl Dropdown {
    /// Opens under the menu bar item at `anchor` (its screen rect, in points).
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: &Monitor, anchor: Rect, kind: Kind, msg_hwnd: isize) -> Result<Self> {
        let content: Box<dyn Content> = match kind {
            Kind::Battery => Box::new(battery::BatteryMenu::new(msg_hwnd)),
            Kind::Calendar => Box::new(calendar::CalendarMenu::new(cfg)),
        };
        let gpu = comp::gpu(gfx)?;
        let c = gpu.compositor.clone();
        let s = monitor.scale;
        let (mon_l, mon_r, mon_b) = (monitor.rect.left as f32 / s, monitor.rect.right as f32 / s, monitor.rect.bottom as f32 / s);
        let w = content.width();
        let top = anchor.bottom() + 6.0;
        let max_h = (mon_b - top - 10.0).max(200.0);
        let h = content.height(gfx, &theme.font).min(max_h);
        // Left-aligned under its item like a menu, kept on screen.
        let x = anchor.x.min(mon_r - 10.0 - w).max(mon_l + 10.0);
        let rect = RECT {
            left: (x * s).round() as i32,
            top: (top * s).round() as i32,
            right: ((x + w) * s).round() as i32,
            bottom: ((top + h) * s).round() as i32,
        };
        let ex = WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP;
        let hwnd = crate::win::create_window(ex, WS_POPUP, rect, HWND::default())?;
        crate::win::accent::prepare(hwnd);
        let backdrop = cfg.menubar.backdrop.for_panels();
        let solid = backdrop == Backdrop::None;
        if !solid {
            unsafe {
                let on = BOOL(1);
                let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_HOSTBACKDROPBRUSH, &on as *const _ as *const _, std::mem::size_of::<BOOL>() as u32);
            }
        }
        let interop: ICompositorDesktopInterop = c.cast()?;
        let target = unsafe { interop.CreateDesktopWindowTarget(hwnd, true)? };
        let root = c.CreateContainerVisual()?;
        target.SetRoot(&root)?;
        let pal = pal(theme, solid);
        let b = brushes(&c, backdrop, &pal, theme.dark)?;
        let glass = Glass::new(&c, &b, w * s, h * s, RADIUS * s)?;
        root.Children()?.InsertAtTop(&glass.vis)?;
        let layer = Layer::new(&gpu, w, h, s)?;
        glass.vis.Children()?.InsertAtTop(&layer.visual)?;
        root.SetOpacity(0.0)?;
        let dd = Self {
            hwnd,
            scale: s,
            _gpu: gpu,
            _target: target,
            root,
            glass,
            layer,
            _brushes: b,
            pal,
            font: theme.font.clone(),
            accent: theme.accent,
            content,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            key: 0,
            w,
            h,
            at: (x, top),
            max_h,
            origin: (anchor.cx() - x).clamp(0.0, w),
            opened: Instant::now(),
            closing: None,
            tracking: false,
            dirty: true,
        };
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        Ok(dd)
    }

    pub fn raise(&self) {
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// Starts closing: it fades away, click-through, and is dropped once `closed`.
    pub fn close(&mut self) {
        if self.closing.is_some() {
            return;
        }
        self.closing = Some(Instant::now());
        unsafe {
            let ex = GetWindowLongW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_TRANSPARENT.0 as i32 | WS_EX_LAYERED.0 as i32);
        }
    }

    pub fn closed(&self) -> bool {
        self.closing.is_some_and(|t| t.elapsed().as_secs_f32() >= CLOSE_S || !crate::anim::motion_enabled())
    }

    pub fn update(&mut self) {
        self.content.update();
        self.dirty = true;
    }

    pub fn animating(&self) -> bool {
        let opening = self.opened.elapsed().as_secs_f32() < OPEN_S && crate::anim::motion_enabled();
        opening || (self.closing.is_some() && !self.closed())
    }

    /// Pushes the open / close animation to the compositor.
    pub fn tick(&mut self) -> bool {
        let s = self.scale;
        let _ = self.root.SetCenterPoint(v3(self.origin * s, 0.0));
        let motion = crate::anim::motion_enabled();
        match self.closing {
            Some(t0) => {
                let t = (t0.elapsed().as_secs_f32() / CLOSE_S).clamp(0.0, 1.0);
                let e = t * t;
                let _ = self.root.SetOpacity(1.0 - e);
                let k = lerp(1.0, 0.97, e);
                let _ = self.root.SetScale(Vector3 { X: k, Y: k, Z: 1.0 });
            }
            None => {
                let since = self.opened.elapsed().as_secs_f32();
                let t = if motion { (since / 0.14).clamp(0.0, 1.0) } else { 1.0 };
                let _ = self.root.SetOpacity(t * (2.0 - t));
                let g = if motion { ease_out_back((since / OPEN_S).clamp(0.0, 1.0), 1.0) } else { 1.0 };
                let k = lerp(0.94, 1.0, g);
                let _ = self.root.SetScale(Vector3 { X: k, Y: lerp(0.9, 1.0, g), Z: 1.0 });
            }
        }
        self.animating()
    }

    pub fn render(&mut self, gfx: &Gfx) {
        self.dirty = false;
        let mut h = DefaultHasher::new();
        self.content.key(&mut h);
        (self.hover, self.pressed, crate::screenshot_mode::on()).hash(&mut h);
        let key = h.finish();
        let height = self.content.height(gfx, &self.font).min(self.max_h);
        if key == self.key && (height - self.h).abs() < 0.5 {
            self.tick();
            return;
        }
        self.key = key;
        if (height - self.h).abs() >= 0.5 {
            self.resize(height);
        }
        let ctx = Ctx { pal: &self.pal, font: &self.font, accent: self.accent, hover: self.hover, pressed: self.pressed };
        let mut hits = Vec::new();
        let content = &self.content;
        self.layer.draw(gfx, |p| {
            content.draw(p, &ctx, &mut hits);
        });
        self.hits = hits;
        self.tick();
    }

    fn resize(&mut self, h: f32) {
        self.h = h;
        let s = self.scale;
        self.glass.set_size(self.w * s, h * s, RADIUS * s);
        self.layer.resize(self.w, h);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                HWND::default(),
                (self.at.0 * s).round() as i32,
                (self.at.1 * s).round() as i32,
                (self.w * s).round() as i32,
                (h * s).round() as i32,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    pub fn in_body(&self, x: f32, y: f32) -> bool {
        x >= 0.0 && x < self.w && y >= 0.0 && y < self.h
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        let h = self.hit_at(x, y);
        if h != self.hover {
            self.hover = h;
            self.dirty = true;
        }
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        if self.hover.is_some() || self.pressed.is_some() {
            self.hover = None;
            self.pressed = None;
            self.dirty = true;
        }
    }

    pub fn on_down(&mut self, x: f32, y: f32) {
        self.pressed = self.hit_at(x, y);
        self.dirty = true;
    }

    pub fn on_up(&mut self, x: f32, y: f32) -> DropEvent {
        let pressed = self.pressed.take();
        self.dirty = true;
        match (pressed, self.hit_at(x, y)) {
            (Some(a), Some(b)) if a == b => self.content.click(a),
            _ => DropEvent::None,
        }
    }

    pub fn on_wheel(&mut self, delta: i32) {
        if self.content.wheel(delta) {
            self.dirty = true;
        }
    }

    pub fn on_key(&mut self, vk: u16) -> DropEvent {
        const ESC: u16 = 0x1B;
        if vk == ESC {
            return DropEvent::Close;
        }
        if self.content.key_down(vk) {
            self.dirty = true;
        }
        DropEvent::None
    }
}

impl Drop for Dropdown {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
